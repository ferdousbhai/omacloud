//! Coordination in the account's bucket, beside the files.
//!
//! The same layout as [`crate::DirCoordinator`] (`heads/`, `devices/`,
//! `epochs/`, `grants/`, `requests/`, `synced/`, `account.json`), as objects,
//! and `keys/` and `key-acks/` for changes of the bucket's key.
//! An append is a create-only write (`If-None-Match: *`), which the bucket
//! refuses if another device took that position first: a compare and swap
//! from the bucket itself. S3, R2, MinIO and Hetzner support it on unversioned
//! buckets (Hetzner not on versioned ones, so a bucket with object lock keeps
//! its coordination in a second, plain bucket). [`BucketCoordinator::check`]
//! finds out before an account depends on it. Dropbox has its own create-only
//! write, as OpenDAL's overwrites there: see [`crate::dropbox`].
//!
//! Devices learn of changes by polling: every write others need also rewrites
//! `changed`, so an idle device checks that one object instead of syncing.
//! Cutting off a lost device's storage takes changing the bucket key, since
//! every device holds it.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Mutex, OnceLock},
};

use anyhow::{Context, Result, bail};
use opendal::{ErrorKind, blocking::Operator, layers::RetryLayer, options};
use serde::{Serialize, de::DeserializeOwned};

use crate::{
    Head,
    devices::{Anchor, DeviceEntry, JoinRequest},
    dropbox::{DropboxCreate, RefusedToken},
    epoch::{EpochRecord, Grant},
    head::Coordinator,
    repo::RepoSpec,
};

fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("a tokio runtime")
    })
}

/// Rewritten on every write others need; see the module docs.
const CHANGED: &str = "changed";

/// Written twice by [`BucketCoordinator::check`], then deleted.
const CHECK: &str = "create-only-check";

/// Something different on every write.
fn nonce() -> Vec<u8> {
    hex::encode(rand::random::<[u8; 16]>()).into_bytes()
}

/// Parallel reads when catching up on a long history.
const READERS: usize = 16;

/// Whether `err` is the bucket refusing this computer's key: deleted at the
/// provider, most likely after a key change this computer missed.
#[must_use]
pub fn key_refused(err: &anyhow::Error) -> bool {
    err.chain().any(|e| {
        e.is::<RefusedToken>()
            || e.downcast_ref::<opendal::Error>().is_some_and(|e| {
                // R2 answers a deleted key with a 401, which OpenDAL leaves
                // unclassified; a 403 comes here as PermissionDenied
                e.kind() == ErrorKind::PermissionDenied
                    || (e.kind() == ErrorKind::Unexpected && e.to_string().contains("status: 401"))
            })
    })
}

/// Whether `err` is the storage out of reach or busy: no connection, a
/// timeout, or an answer that says to try again later.
#[must_use]
pub fn unreachable(err: &anyhow::Error) -> bool {
    err.chain().any(|e| {
        e.downcast_ref::<opendal::Error>()
            .is_some_and(|e| e.is_temporary() || e.kind() == ErrorKind::RateLimited)
    })
}

/// Why a bucket can't hold an account.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error(
    "this storage doesn't refuse a second create-only write, so two computers could overwrite each other's history"
)]
pub struct NoCreateOnly;

pub struct BucketCoordinator {
    /// Retries what the storage says to try again: reads, and writes that
    /// mean the same however often they land.
    op: Operator,
    /// Tries once, for create-only writes: one that landed with its answer
    /// lost would come back as taken, a race it didn't lose.
    once: Operator,
    /// Dropbox's create-only write and the folders it needs first.
    dropbox: Option<DropboxCreate>,
    /// The last seq seen per log; appends only add at the end, so finding
    /// the latest is probing forward from here.
    last: Mutex<HashMap<&'static str, u64>>,
}

impl BucketCoordinator {
    /// Coordinate in an OpenDAL location: `scheme` (`s3`, `fs`, ...) with
    /// its options (`bucket`, `endpoint`, keys, `root`).
    ///
    /// # Errors
    ///
    /// If the options don't make an operator.
    pub fn new(scheme: &str, options: &BTreeMap<String, String>) -> Result<Self> {
        let _guard = runtime().enter();
        let dropbox = (scheme == "dropbox").then(|| DropboxCreate::new(options)).transpose()?;
        if let Some(dropbox) = &dropbox {
            dropbox.prepare_root()?;
        }
        let once = opendal::Operator::via_iter(scheme, options.clone())
            .with_context(|| format!("opening the {scheme} location for coordination"))?;
        let op = once.clone().layer(RetryLayer::new().with_jitter());
        Ok(Self {
            op: Operator::new(op)?,
            once: Operator::new(once)?,
            dropbox,
            last: Mutex::new(HashMap::new()),
        })
    }

    /// Coordinate beside the repository `repo`: in its bucket (or `bucket`,
    /// with the same endpoint and key), under `omacloud-coordination`.
    ///
    /// # Errors
    ///
    /// If `repo` isn't in a bucket, or its options don't make an operator.
    pub fn beside(repo: &RepoSpec, bucket: Option<&str>) -> Result<Self> {
        let scheme = repo
            .repository
            .strip_prefix("opendal:")
            .context("coordinating in the bucket needs the repository to be one")?;
        let mut options = repo.options.clone();
        let root = match bucket {
            Some(b) => {
                _ = options.insert("bucket".into(), b.into());
                "/omacloud-coordination".to_string()
            }
            None => repo.under_root("omacloud-coordination"),
        };
        _ = options.insert("root".into(), root);
        Self::new(scheme, &options)
    }

    /// Write only if nothing is there: `Ok(false)` if something is.
    fn write_new(&self, key: &str, bytes: Vec<u8>) -> Result<bool> {
        if let Some(dropbox) = &self.dropbox {
            return dropbox.create(key, &bytes);
        }
        let opts = options::WriteOptions {
            if_not_exists: true,
            ..Default::default()
        };
        match self.once.write_options(key, bytes, opts) {
            Ok(_) => Ok(true),
            Err(e) if matches!(e.kind(), ErrorKind::ConditionNotMatch | ErrorKind::AlreadyExists) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    fn key(kind: &str, seq: u64) -> String {
        format!("{kind}/{seq:020}.json")
    }

    fn get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        match self.op.read(key) {
            Ok(buf) => Ok(Some(serde_json::from_slice(&buf.to_vec())?)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {key}")),
        }
    }

    /// Write only if nothing is there, and tell idle devices: `Ok(false)` if
    /// something is.
    fn create<T: Serialize>(&self, key: &str, value: &T) -> Result<bool> {
        let written = self
            .write_new(key, serde_json::to_vec(value)?)
            .with_context(|| format!("writing {key}"))?;
        if written {
            self.touch();
        }
        Ok(written)
    }

    /// Write without telling idle devices: what only says where a device
    /// stands is no news to the others, and moving the marker would have each
    /// sync wake the rest in turn.
    fn put_quietly<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        if let Some(dropbox) = &self.dropbox {
            dropbox.prepare_key(key)?;
        }
        self.op
            .write(key, serde_json::to_vec(value)?)
            .with_context(|| format!("writing {key}"))?;
        Ok(())
    }

    fn put<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        self.put_quietly(key, value)?;
        self.touch();
        Ok(())
    }

    /// Something was written: rewrite `changed` so idle devices look. A
    /// failure only delays them until their next full sync.
    fn touch(&self) {
        if let Err(e) = self.op.write(CHANGED, nonce()) {
            log::warn!("marking a change: {e}");
        }
    }

    fn names(&self, dir: &str) -> Result<Vec<String>> {
        let entries = match self.op.list(&format!("{dir}/")) {
            Ok(e) => e,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("listing {dir}")),
        };
        Ok(entries
            .into_iter()
            .filter(|e| e.metadata().is_file())
            .map(|e| e.name().to_string())
            .collect())
    }

    /// The last seq in a log: listed once, then probed forward. A log is
    /// numbered from 1 without gaps (appends only add at the end), so an
    /// entry listed past the count is stray: anyone with the bucket key can
    /// write one, and it mustn't send a device reading up to it.
    fn last_seq(&self, kind: &'static str) -> Result<u64> {
        let known = self.last.lock().unwrap().get(kind).copied();
        let mut seq = match known {
            Some(s) => s,
            None => {
                let seqs: Vec<u64> = self
                    .names(kind)?
                    .iter()
                    .filter_map(|n| n.strip_suffix(".json")?.parse::<u64>().ok())
                    .collect();
                let last = seqs.iter().copied().max().unwrap_or(0);
                if last > seqs.len() as u64 {
                    bail!(
                        "the bucket's {kind} log has gaps: entry {last} among {} entries",
                        seqs.len()
                    );
                }
                last
            }
        };
        loop {
            match self.op.stat(&Self::key(kind, seq + 1)) {
                Ok(_) => seq += 1,
                Err(e) if e.kind() == ErrorKind::NotFound => break,
                Err(e) => return Err(e).context("looking for new entries"),
            }
        }
        _ = self.last.lock().unwrap().insert(kind, seq);
        Ok(seq)
    }

    /// Entries after `after`, in order, read in parallel.
    fn after<T: DeserializeOwned + Send>(&self, kind: &'static str, after: u64) -> Result<Vec<T>> {
        let seqs: Vec<u64> = (after + 1..=self.last_seq(kind)?).collect();
        let read = |seq: u64| {
            self.get::<T>(&Self::key(kind, seq))?
                .with_context(|| format!("{kind} {seq} is missing"))
        };
        std::thread::scope(|scope| {
            let readers: Vec<_> = seqs
                .chunks(seqs.len().div_ceil(READERS).max(1))
                .map(|part| scope.spawn(|| part.iter().map(|&seq| read(seq)).collect::<Result<Vec<T>>>()))
                .collect();
            let mut out = Vec::with_capacity(seqs.len());
            for reader in readers {
                out.extend(reader.join().expect("a reader")?);
            }
            Ok(out)
        })
    }

    /// The last entry of a log.
    fn latest<T: DeserializeOwned>(&self, kind: &'static str) -> Result<Option<T>> {
        match self.last_seq(kind)? {
            0 => Ok(None),
            s => self.get(&Self::key(kind, s)),
        }
    }

    /// Every entry in `dir` whose name `wanted` picks.
    fn all_in<T: DeserializeOwned>(&self, dir: &str, wanted: impl Fn(&str) -> bool) -> Result<Vec<T>> {
        self.names(dir)?
            .iter()
            .filter(|n| n.ends_with(".json") && wanted(n))
            .filter_map(|n| self.get(&format!("{dir}/{n}")).transpose())
            .collect()
    }

    fn append_seq<T: Serialize>(&self, kind: &'static str, seq: u64, value: &T) -> Result<bool> {
        // the next after the last seen needs no look: a create-only write
        // refuses a position that's taken
        let known = self.last.lock().unwrap().get(kind).copied();
        if known.map(|k| k + 1) != Some(seq) && self.last_seq(kind)? + 1 != seq {
            return Ok(false);
        }
        let ok = self.create(&Self::key(kind, seq), value)?;
        if ok {
            _ = self.last.lock().unwrap().insert(kind, seq);
        }
        Ok(ok)
    }
}

impl Coordinator for BucketCoordinator {
    /// Whether the key reaches the bucket and the bucket refuses to create
    /// an object twice, which everything here relies on: [`NoCreateOnly`] if
    /// it overwrites, otherwise what the storage said.
    fn check(&self) -> Result<()> {
        // storage that can't do it says so as an error
        let write = || match self.write_new(CHECK, nonce()) {
            Err(e)
                if e.downcast_ref::<opendal::Error>()
                    .is_some_and(|e| e.kind() == ErrorKind::Unsupported) =>
            {
                Err(anyhow::Error::new(NoCreateOnly))
            }
            other => other.context("writing to the bucket"),
        };
        // one left by a try that stopped halfway would look like a refusal
        self.op.delete(CHECK).context("reaching the bucket")?;
        anyhow::ensure!(write()?, NoCreateOnly);
        let second = write();
        let deleted = self.op.delete(CHECK);
        anyhow::ensure!(!second?, NoCreateOnly);
        deleted.context("deleting from the bucket")?;
        Ok(())
    }
    fn head(&self) -> Result<Option<Head>> {
        self.latest("heads")
    }
    fn since(&self, after: u64) -> Result<Vec<Head>> {
        self.after("heads", after)
    }
    fn append(&self, head: &Head) -> Result<bool> {
        self.append_seq("heads", head.seq, head)
    }
    fn device_head(&self) -> Result<Option<DeviceEntry>> {
        self.latest("devices")
    }
    fn devices_since(&self, after: u64) -> Result<Vec<DeviceEntry>> {
        self.after("devices", after)
    }
    fn append_device(&self, entry: &DeviceEntry) -> Result<bool> {
        self.append_seq("devices", entry.seq, entry)
    }
    fn anchor(&self) -> Result<Option<Anchor>> {
        self.get("account.json")
    }
    fn set_anchor(&self, anchor: &Anchor) -> Result<bool> {
        self.create("account.json", anchor)
    }
    fn epochs(&self) -> Result<Vec<EpochRecord>> {
        self.after("epochs", 0)
    }
    fn append_epoch(&self, record: &EpochRecord) -> Result<bool> {
        // stored at epoch + 1, as the logs count from 1
        self.append_seq("epochs", record.epoch + 1, record)
    }
    fn grants(&self, device: &str) -> Result<Vec<Grant>> {
        let prefix = format!("{device}-");
        self.all_in("grants", |n| n.starts_with(&prefix))
    }
    fn put_grant(&self, grant: &Grant) -> Result<()> {
        self.put(&format!("grants/{}-{}.json", grant.device, grant.epoch), grant)
    }
    fn requests(&self) -> Result<Vec<JoinRequest>> {
        self.all_in("requests", |_| true)
    }
    fn request(&self, req: &JoinRequest) -> Result<()> {
        self.put(&format!("requests/{}.json", req.device), req)
    }
    fn latest_key_record(&self) -> Result<Option<crate::bucket_key::KeyRecord>> {
        self.latest("keys")
    }
    fn append_key_record(&self, record: &crate::bucket_key::KeyRecord) -> Result<bool> {
        self.append_seq("keys", record.seq, record)
    }
    fn ack_key(&self, seq: u64, device: &str) -> Result<()> {
        self.put_quietly(&format!("key-acks/{seq}/{device}"), &true)
    }
    fn key_acks(&self, seq: u64) -> Result<Vec<String>> {
        self.names(&format!("key-acks/{seq}"))
    }
    fn remove_request(&self, device: &str) -> Result<()> {
        self.op
            .delete(&format!("requests/{device}.json"))
            .context("removing a join request")?;
        self.touch();
        Ok(())
    }
    fn set_synced(&self, device: &str, seq: u64) -> Result<()> {
        self.put_quietly(&format!("synced/{device}.json"), &seq)
    }
    fn synced(&self) -> Result<BTreeMap<String, u64>> {
        let mut out = BTreeMap::new();
        for name in self.names("synced")? {
            if let Some(device) = name.strip_suffix(".json")
                && let Some(seq) = self.get(&format!("synced/{name}"))?
            {
                _ = out.insert(device.to_string(), seq);
            }
        }
        Ok(out)
    }
    fn marker(&self) -> Result<Option<String>> {
        // its contents, random on every write: an ETag or a modification
        // time isn't there on every storage, or is too coarse
        match self.op.read(CHANGED) {
            Ok(buf) => Ok(Some(String::from_utf8_lossy(&buf.to_vec()).into_owned())),
            // nothing written since devices started marking changes
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Some(String::new())),
            Err(e) => Err(e).context("checking for changes"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refused_and_unreachable_by_kind() {
        let err = |e: opendal::Error| anyhow::Error::new(e).context("reading the anchor");
        let denied = err(opendal::Error::new(ErrorKind::PermissionDenied, "403"));
        assert!(key_refused(&denied) && !unreachable(&denied));
        // as R2 answers a key that was deleted
        let unauthorized =
            err(opendal::Error::new(ErrorKind::Unexpected, "").with_context("response", "Parts { status: 401, .. }"));
        assert!(key_refused(&unauthorized) && !unreachable(&unauthorized));
        assert!(!key_refused(&err(opendal::Error::new(
            ErrorKind::Unexpected,
            "status: 500"
        ))));
        let busy = err(opendal::Error::new(ErrorKind::RateLimited, "429"));
        assert!(unreachable(&busy) && !key_refused(&busy));
        let down = err(opendal::Error::new(ErrorKind::Unexpected, "503").set_temporary());
        assert!(unreachable(&down));
        assert!(!unreachable(&err(opendal::Error::new(ErrorKind::NotFound, "404"))));
    }
}
