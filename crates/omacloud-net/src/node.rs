//! A storage node: keeps other accounts' blobs on this computer's disk.
//!
//! It trusts nobody and needs no trust: a blob is kept only if its bytes
//! hash to the id it was stored under, anyone may read any blob (they're
//! ciphertext, scrambled per holder), and a blob stays only as long as
//! someone keeps paying for it. Nothing here knows which account a blob
//! belongs to.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use iroh::{
    Endpoint, EndpointAddr,
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler, Router},
};
use iroh_blobs::{
    BlobsProtocol, Hash,
    store::{
        GcConfig,
        fs::{FsStore, options::Options},
    },
};
use rusqlite::{OptionalExtension, params};

use crate::{
    blob::NodeId,
    credit::{IssuerKey, Token, tokens_for},
    proto::{self, BlobId, MAX_OBJECT, Reply, Request},
};

/// A month, as leases count it.
pub const MONTH: u64 = 30 * 24 * 3600;

/// How long a new blob is kept before a renewal pays for it.
pub const FIRST_LEASE: u64 = 7 * 24 * 3600;

/// How far ahead a renewal may keep a blob. It's kept on credit, paid at
/// the next renewal, so this is what a holder risks on an owner.
pub const LONGEST_LEASE: u64 = 3 * MONTH;

/// How long a paid blob is kept after its lease ends, unpaid: an account
/// whose every computer was lost has this long to come back for it.
pub const GRACE: u64 = 180 * 24 * 3600;

/// What a node is set up with.
pub struct Config {
    /// Where its blobs and records live.
    pub dir: PathBuf,
    /// Bytes it gives.
    pub capacity: u64,
    /// The Worker's token key; without one (development, tests), storage
    /// is free.
    pub issuer: Option<IssuerKey>,
}

/// A running node.
pub struct Node {
    router: Router,
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    store: FsStore,
    db: Mutex<rusqlite::Connection>,
    capacity: u64,
    issuer: Option<IssuerKey>,
}

#[derive(Debug, Clone)]
struct Control(Arc<Inner>);

impl Node {
    /// Start serving on `endpoint`.
    ///
    /// # Errors
    ///
    /// If the store or its records can't be opened.
    pub async fn spawn(endpoint: Endpoint, config: Config) -> Result<Self> {
        std::fs::create_dir_all(&config.dir)?;
        let mut options = Options::new(&config.dir.join("blobs"));
        // an untagged blob (a lease ended, a store that failed) goes at
        // the next collection
        options.gc = Some(GcConfig {
            interval: Duration::from_secs(3600),
            add_protected: None,
        });
        let store = FsStore::load_with_opts(config.dir.join("blobs/blobs.db"), options).await?;
        let db = open_db(&config.dir.join("node.db"))?;
        let inner = Arc::new(Inner {
            store,
            db: Mutex::new(db),
            capacity: config.capacity,
            issuer: config.issuer,
        });
        let router = Router::builder(endpoint)
            .accept(iroh_blobs::ALPN, BlobsProtocol::new(&inner.store, None))
            .accept(proto::ALPN, Control(inner.clone()))
            .spawn();
        Ok(Self { router, inner })
    }

    #[must_use]
    pub fn id(&self) -> NodeId {
        *self.router.endpoint().id().as_bytes()
    }

    /// Where others reach it.
    #[must_use]
    pub fn addr(&self) -> EndpointAddr {
        self.router.endpoint().addr()
    }

    /// Bytes kept now.
    ///
    /// # Errors
    ///
    /// If the records can't be read.
    pub fn used(&self) -> Result<u64> {
        self.inner.used()
    }

    /// Tokens taken and not yet traded in at the Worker.
    ///
    /// # Errors
    ///
    /// If the records can't be read.
    pub fn tokens(&self) -> Result<Vec<Token>> {
        let db = self.inner.db.lock().expect("node db");
        let mut q = db.prepare("SELECT token FROM tokens WHERE deposited = 0")?;
        let rows = q.query_map([], |r| r.get::<_, String>(0))?;
        rows.map(|t| Ok(serde_json::from_str(&t?)?)).collect()
    }

    /// Note tokens as traded in at the Worker.
    ///
    /// # Errors
    ///
    /// If the records can't be written.
    pub fn mark_deposited(&self, tokens: &[Token]) -> Result<()> {
        let db = self.inner.db.lock().expect("node db");
        for t in tokens {
            db.execute("UPDATE tokens SET deposited = 1 WHERE id = ?1", [hex::encode(t.id())])?;
        }
        Ok(())
    }

    /// Drop the blobs whose lease (and, once paid, grace) ran out; their
    /// space is freed at the next collection. Returns how many.
    ///
    /// # Errors
    ///
    /// If the records or the store fail.
    pub async fn sweep(&self) -> Result<usize> {
        self.sweep_as_of(now()).await
    }

    /// As if `secs` had passed since each blob was stored and paid for, for
    /// tests.
    ///
    /// # Errors
    ///
    /// If the node's database can't be written.
    #[doc(hidden)]
    pub fn backdate(&self, secs: u64) -> Result<()> {
        let secs = i64::try_from(secs)?;
        self.inner.db.lock().expect("node db").execute(
            "UPDATE blobs SET paid_to = paid_to - ?1, expires = expires - ?1",
            [secs],
        )?;
        Ok(())
    }

    /// [`Self::sweep`] as if it were `now`, for tests.
    #[doc(hidden)]
    pub async fn sweep_as_of(&self, now: u64) -> Result<usize> {
        let gone: Vec<String> = {
            let db = self.inner.db.lock().expect("node db");
            let mut q =
                db.prepare("SELECT blob FROM blobs WHERE expires + (CASE WHEN paid THEN ?1 ELSE 0 END) < ?2")?;
            let rows = q.query_map(params![GRACE, now], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for blob in &gone {
            self.inner.store.tags().delete(tag(blob)).await?;
            self.inner
                .db
                .lock()
                .expect("node db")
                .execute("DELETE FROM blobs WHERE blob = ?1", [blob])?;
        }
        Ok(gone.len())
    }

    /// Stop serving and close the store.
    pub async fn shutdown(self) {
        if let Err(e) = self.router.shutdown().await {
            log::warn!("stopping the node: {e:#}");
        }
        if let Err(e) = self.inner.store.shutdown().await {
            log::warn!("closing the blob store: {e:#}");
        }
    }
}

/// Whole months a blob was kept since it was paid up to, until now or its
/// lease's end if that came first: the grace after a lease is unpaid. Only
/// whole months count, so an owner may renew as often as it likes, and
/// pays once a month has gone by.
fn months_served(paid_to: u64, expires: u64, now: u64) -> u64 {
    now.min(expires).saturating_sub(paid_to) / MONTH
}

fn open_db(path: &Path) -> Result<rusqlite::Connection> {
    let db = rusqlite::Connection::open(path)?;
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS blobs (
             blob TEXT PRIMARY KEY,
             size INTEGER NOT NULL,
             expires INTEGER NOT NULL,
             paid INTEGER NOT NULL DEFAULT 0
         );
         CREATE TABLE IF NOT EXISTS schema (version INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS tokens (
             id TEXT PRIMARY KEY,
             token TEXT NOT NULL,
             deposited INTEGER NOT NULL DEFAULT 0
         );",
    )?;
    // version 1: when each blob is paid up to (until then renewals paid
    // ahead, so a blob from then is paid to its expiry)
    let version: i64 = db
        .query_row("SELECT version FROM schema", [], |r| r.get(0))
        .optional()?
        .unwrap_or(0);
    if version < 1 {
        db.execute_batch(
            "ALTER TABLE blobs ADD COLUMN paid_to INTEGER NOT NULL DEFAULT 0;
             UPDATE blobs SET paid_to = expires;
             DELETE FROM schema; INSERT INTO schema VALUES (1);",
        )?;
    }
    Ok(db)
}

fn tag(blob: &str) -> String {
    format!("b/{blob}")
}

/// Unix seconds.
#[must_use]
pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl Inner {
    fn used(&self) -> Result<u64> {
        let db = self.db.lock().expect("node db");
        let used: i64 = db.query_row("SELECT COALESCE(SUM(size), 0) FROM blobs", [], |r| r.get(0))?;
        Ok(u64::try_from(used).unwrap_or(0))
    }

    async fn store(&self, blob: BlobId, size: u64, recv: &mut iroh::endpoint::RecvStream) -> Result<Reply> {
        let name = hex::encode(blob.0);
        let have = self
            .db
            .lock()
            .expect("node db")
            .query_row("SELECT 1 FROM blobs WHERE blob = ?1", [&name], |_| Ok(()))
            .optional()?
            .is_some();
        if have {
            return Ok(Reply::Stored);
        }
        if size > MAX_OBJECT {
            return Ok(Reply::Refused {
                why: format!("objects are at most {MAX_OBJECT} bytes"),
            });
        }
        if self.used()? + size > self.capacity {
            return Ok(Reply::Refused { why: "full".into() });
        }
        let mut data = vec![0u8; usize::try_from(size)?];
        recv.read_exact(&mut data).await?;
        if crate::blob::blob_id(&data) != blob.0 {
            return Ok(Reply::Refused {
                why: "the bytes don't hash to that blob".into(),
            });
        }
        let added = self.store.add_bytes(data).with_named_tag(tag(&name)).await?;
        anyhow::ensure!(added.hash == hash_of(&blob), "the store hashed it differently");
        self.db.lock().expect("node db").execute(
            "INSERT OR IGNORE INTO blobs (blob, size, expires, paid_to) VALUES (?1, ?2, ?3, ?3)",
            params![name, size, now() + FIRST_LEASE],
        )?;
        Ok(Reply::Stored)
    }

    fn renew(&self, blobs: &[BlobId], until: u64, tokens: Vec<Token>) -> Result<Reply> {
        let now = now();
        if until > now + LONGEST_LEASE {
            return Ok(Reply::Refused {
                why: "that lease is too long".into(),
            });
        }
        let db = self.db.lock().expect("node db");
        let mut kept = Vec::with_capacity(blobs.len());
        let mut served = Vec::with_capacity(blobs.len());
        // what was served unpaid: bytes kept, times the whole months since
        // each was last paid for (the first week is free)
        let mut byte_months = 0u64;
        for b in blobs {
            let row: Option<(i64, i64, i64)> = db
                .query_row(
                    "SELECT size, paid_to, expires FROM blobs WHERE blob = ?1",
                    [hex::encode(b.0)],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            kept.push(row.is_some());
            let months = row.map_or(0, |(size, paid_to, expires)| {
                let months = months_served(
                    u64::try_from(paid_to).unwrap_or(0),
                    u64::try_from(expires).unwrap_or(0),
                    now,
                );
                byte_months += u64::try_from(size).unwrap_or(0) * months;
                months
            });
            served.push(months);
        }
        // the grace after a lease is for blobs that were paid for; storage
        // is free without a token key
        // a renewal accepted is an owner standing by its blobs: they get the
        // grace too, so an account whose computers were all lost keeps its
        // newest data as long as its oldest
        if let Some(issuer) = &self.issuer {
            let owed = tokens_for(byte_months);
            // each good token once, however often it's given
            let mut fresh = std::collections::BTreeSet::new();
            for t in &tokens {
                if t.verify(issuer).is_err() {
                    continue;
                }
                let id = hex::encode(t.id());
                let taken = db
                    .query_row("SELECT 1 FROM tokens WHERE id = ?1", [&id], |_| Ok(()))
                    .optional()?
                    .is_some();
                if !taken {
                    fresh.insert(id);
                }
            }
            let fresh = fresh.len() as u64;
            if fresh < owed {
                return Ok(Reply::Owed { tokens: owed });
            }
            for t in &tokens {
                if t.verify(issuer).is_ok() {
                    db.execute(
                        "INSERT OR IGNORE INTO tokens (id, token) VALUES (?1, ?2)",
                        params![hex::encode(t.id()), serde_json::to_string(t)?],
                    )?;
                }
            }
        }
        for ((b, k), months) in blobs.iter().zip(&kept).zip(&served) {
            if *k {
                // paid up to the end of the months paid for: the part of a
                // month begun waits for the next renewal. A lease that ran
                // out was paid to its end, and the grace since is forgiven:
                // from now on it counts afresh
                db.execute(
                    "UPDATE blobs SET expires = MAX(expires, ?1), paid = 1, \
                     paid_to = CASE WHEN expires < ?2 THEN ?2 ELSE paid_to + ?3 END WHERE blob = ?4",
                    params![
                        until,
                        i64::try_from(now)?,
                        i64::try_from(months * MONTH)?,
                        hex::encode(b.0)
                    ],
                )?;
            }
        }
        Ok(Reply::Renewed { kept })
    }

    async fn serve(&self, conn: Connection) -> Result<()> {
        loop {
            let (mut send, mut recv) = match conn.accept_bi().await {
                Ok(pair) => pair,
                // the other side is done
                Err(_) => return Ok(()),
            };
            let request: Request = proto::recv(&mut recv).await?;
            let reply = match request {
                Request::Store { blob, size } => self.store(blob, size, &mut recv).await,
                Request::Renew { blobs, until, tokens } => self.renew(&blobs, until, tokens),
            }
            .unwrap_or_else(|e| Reply::Refused { why: format!("{e:#}") });
            proto::send(&mut send, &reply).await?;
            send.finish()?;
        }
    }
}

impl ProtocolHandler for Control {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let remote = conn.remote_id();
        if let Err(e) = self.0.serve(conn).await {
            log::debug!("node protocol with {remote}: {e:#}");
        }
        Ok(())
    }
}

/// The blob's id as iroh-blobs knows it.
#[must_use]
pub(crate) fn hash_of(blob: &BlobId) -> Hash {
    Hash::from_bytes(blob.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_holder_charges_for_the_lease_it_served_not_the_grace_after() {
        let t = 100 * MONTH;
        assert_eq!(months_served(t, t + 2 * MONTH, t), 0);
        // a month begun isn't owed yet: renewing often costs nothing more
        assert_eq!(months_served(t, t + 2 * MONTH, t + MONTH - 1), 0);
        assert_eq!(months_served(t, t + 2 * MONTH, t + MONTH + 1), 1);
        // a year on, deep in its grace: still the two months of the lease
        assert_eq!(months_served(t, t + 2 * MONTH, t + 12 * MONTH), 2);
    }
}
