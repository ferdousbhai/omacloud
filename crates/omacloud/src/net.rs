//! What the daemon does on the Omacloud network beside syncing: keep the
//! space this computer gives others (a node), say it's up, look after this
//! account's copies, and keep the tokens that pay for them.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use log::{info, warn};
use omacloud_core::Engine;
use omacloud_net::{
    credit::Token,
    node::{FIRST_LEASE, MONTH, Node, now},
    session::Session,
    upkeep::Renew,
};
use serde::{Deserialize, Serialize};

/// How often a node says it's up; the directory drops one silent for 72 hours.
const HEARTBEAT: Duration = Duration::from_secs(15 * 60);
/// How often copies are looked after.
const UPKEEP: Duration = Duration::from_secs(6 * 3600);
/// Copies read back each round.
const AUDITS: usize = 32;
/// Renewals keep copies this far ahead; holders are paid for the time they
/// served.
const LEASE: u64 = 2 * MONTH;
/// Tokens asked for when the wallet runs short and nothing else pays.
const STARTER: usize = 10;
/// How long a holder that failed this computer is passed over.
const SUSPECT_FOR: u64 = 30 * 24 * 3600;
/// How often copies are renewed: well inside a new copy's free first week,
/// so one placed (or repaired) between renewals is never let go. Holders
/// charge whole months, so renewing daily costs no more than monthly.
const RENEW_EVERY: u64 = 24 * 3600;

pub struct Contribution {
    key: omacloud_net::session::SecretKey,
    node: Option<Node>,
    give: u64,
    data: PathBuf,
    beat_at: Option<Instant>,
    upkeep_at: Option<Instant>,
}

impl Contribution {
    /// Start giving `give` bytes (none at 0) from `data`.
    ///
    /// # Errors
    ///
    /// If the node can't start.
    pub fn start(session: &Session, data: &Path, give: u64) -> Result<Self> {
        let key = omacloud_net::session::node_key(&data.join("node.key"))?;
        let node = if give > 0 {
            let node = session.serve(key.clone(), data.join("node"), give)?;
            info!(
                "giving {} to the network as node {}",
                human(give),
                hex::encode(&node.id()[..4])
            );
            Some(node)
        } else {
            None
        };
        Ok(Self {
            key,
            node,
            give,
            data: data.to_path_buf(),
            beat_at: Some(Instant::now()),
            upkeep_at: None,
        })
    }

    /// The node this computer serves as, if it gives space.
    #[must_use]
    pub fn node_id(&self) -> Option<omacloud_net::blob::NodeId> {
        self.node.as_ref().map(Node::id)
    }

    /// Do what's due; `engine` is the account's, none on a node that only
    /// gives. Failures only log: the network may be out of reach.
    pub fn tick(&mut self, session: &Session, engine: Option<&Engine>) {
        if let Some(node) = &self.node
            && self.beat_at.is_none_or(|t| t.elapsed() >= HEARTBEAT)
        {
            self.beat_at = Some(Instant::now());
            if let Err(e) = session.network().heartbeat(&self.key, node, self.give) {
                warn!("telling the network this node is up: {e:#}");
            }
            if let Err(e) = self.deposit(session, node) {
                warn!("trading in tokens: {e:#}");
            }
            if let Err(e) = session.block_on(node.sweep()) {
                warn!("dropping expired blobs: {e:#}");
            }
        }
        if let Some(engine) = engine
            && self.upkeep_at.is_none_or(|t| t.elapsed() >= UPKEEP)
        {
            self.upkeep_at = Some(Instant::now());
            if let Err(e) = self.upkeep(session, engine) {
                warn!("looking after this account's copies: {e:#}");
            }
            self.renew_shares(session);
        }
    }

    fn upkeep(&self, session: &Session, engine: &Engine) -> Result<()> {
        let (spec, _, _) = engine.repository()?;
        let key = spec
            .options
            .get("access_key_id")
            .context("the repository isn't on the network")?;
        // daily; each holder is paid at each month end, counted from when
        // that holder was last renewed
        let renewed = read_number(&self.data.join("renewed"));
        // per bucket: a key rotation's new one starts afresh, as its copies
        // weren't in the old one's renewals
        let mut last = read_holders(&self.data.join("holders.json"), key);
        // stamped with when the round began: a holder renewed before a
        // month end the round runs past was paid up to before it
        let started = now();
        let due = started.saturating_sub(renewed) >= RENEW_EVERY;
        let renew = due.then(|| Renew {
            until: now() + LEASE,
            last: last.clone(),
        });
        let wallet = self.data.join("wallet.json");
        let network = session.network();
        let mut pay = |n: u64| take(&wallet, n, || network.starter(STARTER));
        let mut suspects = read_suspects(&self.data);
        session.set_avoid(suspects.keys().copied().collect());
        let report = session.upkeep(key, AUDITS, renew, &mut pay)?;
        if !report.failed_nodes.is_empty() {
            for n in &report.failed_nodes {
                suspects.insert(*n, now() + SUSPECT_FOR);
            }
            session.set_avoid(suspects.keys().copied().collect());
            write_private(
                &self.data.join("suspects.json"),
                &serde_json::to_vec(&hexed(&suspects))?,
            )?;
        }
        if due {
            write_private(&self.data.join("renewed"), started.to_string().as_bytes())?;
            last.extend(report.renewed.iter().map(|n| (*n, started)));
            let kept = serde_json::json!({ "bucket": bucket_of(key), "last": hexed(&last) });
            write_private(&self.data.join("holders.json"), kept.to_string().as_bytes())?;
        }
        let space = serde_json::json!({ "stored": report.stored, "monthly": report.monthly, "at": now() });
        write_private(&self.data.join("space.json"), space.to_string().as_bytes())?;
        if report.failed > 0 || report.repaired > 0 || !report.lost.is_empty() {
            info!(
                "copies: {} audited, {} failed, {} objects given new copies",
                report.audited, report.failed, report.repaired
            );
        }
        for key in &report.lost {
            warn!("{key} has no copy left on the network");
        }
        Ok(())
    }

    /// Keep this computer's shares until they're due to end, paying from
    /// the wallet; forget those that ended.
    fn renew_shares(&self, session: &Session) {
        let original = read_shares(&self.data);
        let mut shares = original.clone();
        let before = shares.len();
        // an ended share is taken down, or its holders' grace would keep it
        // opening for months; kept to try again if that fails
        shares.retain(|s| {
            if s.until > now() {
                return true;
            }
            match session.take_down(&s.secret) {
                Ok(()) => false,
                Err(e) => {
                    warn!("taking down the ended share {}: {e:#}", s.name);
                    true
                }
            }
        });
        let wallet = self.data.join("wallet.json");
        let network = session.network();
        let mut changed = shares.len() != before;
        for s in &mut shares {
            let started = now();
            let Some(until) = s.renewal(started) else { continue };
            let key = omacloud_net::share::bucket_key(&s.secret);
            let mut pay = |n: u64| take(&wallet, n, || network.starter(STARTER));
            let last: BTreeMap<omacloud_net::blob::NodeId, u64> = s
                .holders
                .iter()
                .filter_map(|(id, t)| Some((hex::decode(id).ok()?.try_into().ok()?, *t)))
                .collect();
            match session.upkeep(&key, 0, Some(Renew { until, last }), &mut pay) {
                Ok(r) => {
                    s.holders.extend(r.renewed.iter().map(|n| (hex::encode(n), started)));
                    // a holder that didn't renew is replaced this round; the
                    // next tick, six hours on, renews the new copies
                    if r.failed == 0 {
                        s.renewed_at = started;
                    }
                    changed = true;
                }
                Err(e) => warn!("keeping the shared {}: {e:#}", s.name),
            }
        }
        if !changed {
            return;
        }
        // into the list as it is now: a share made or taken back while these
        // renewals ran (`omacloud share`, `unshare`) isn't lost
        let taken_down: BTreeSet<String> = original
            .iter()
            .filter(|s| !shares.iter().any(|t| t.secret == s.secret))
            .map(|s| s.secret.clone())
            .collect();
        let mut now_listed = read_shares(&self.data);
        now_listed.retain(|s| !taken_down.contains(&s.secret));
        for s in &mut now_listed {
            if let Some(ours) = shares.iter().find(|t| t.secret == s.secret) {
                s.renewed_at = s.renewed_at.max(ours.renewed_at);
                s.holders.extend(ours.holders.iter().map(|(k, v)| (k.clone(), *v)));
            }
        }
        if let Err(e) = write_shares(&self.data, &now_listed) {
            warn!("saving shares: {e:#}");
        }
    }

    /// Trade the tokens this node took for fresh ones, into the wallet.
    fn deposit(&self, session: &Session, node: &Node) -> Result<()> {
        let wallet = self.data.join("wallet.json");
        // a batch the Worker takes at a time, each banked before the next:
        // tokens it took are spent, so what it gave back must not be lost
        for batch in node.tokens()?.chunks(100) {
            let fresh = session.network().exchange(batch)?;
            let mut tokens = read_wallet(&wallet);
            tokens.extend(fresh);
            write_private(&wallet, &serde_json::to_vec(&tokens)?)?;
            node.mark_deposited(batch)?;
        }
        Ok(())
    }
}

/// A file this computer shared by link, kept until `until`. The secret is
/// the link's, so the file is private (0600) as the wallet is.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Shared {
    pub secret: String,
    pub name: String,
    pub made: u64,
    pub until: u64,
    /// When it was last renewed.
    #[serde(default)]
    pub renewed_at: u64,
    /// When each holder (hex node id) was last renewed: what it may charge
    /// is counted from then.
    #[serde(default)]
    pub holders: BTreeMap<String, u64>,
}

impl Shared {
    /// Until when to renew it now, if it's due: a share that outlives its
    /// first lease is renewed daily, as an account's copies are, two months
    /// ahead and never past `until`.
    #[must_use]
    pub fn renewal(&self, now: u64) -> Option<u64> {
        (now < self.until && self.until > self.made + FIRST_LEASE && now.saturating_sub(self.renewed_at) >= RENEW_EVERY)
            .then(|| self.until.min(now + LEASE))
    }
}

fn shares_path(data: &Path) -> PathBuf {
    data.join("shares.json")
}

/// This computer's shares, ended ones included until the daemon forgets them.
#[must_use]
pub fn read_shares(data: &Path) -> Vec<Shared> {
    read_json(&shares_path(data))
}

/// Save this computer's shares.
///
/// # Errors
///
/// If the file can't be written.
pub fn write_shares(data: &Path, shares: &[Shared]) -> Result<()> {
    fs::create_dir_all(data)?;
    write_private(&shares_path(data), &serde_json::to_vec(shares)?)
}

/// The account's space as the last upkeep round found it: bytes stored
/// (one copy of each), tokens a month that costs, tokens held, and how
/// far that pays. Nulls before the first round.
#[must_use]
pub fn space(data: &Path) -> serde_json::Value {
    let last: serde_json::Value = read_json(&data.join("space.json"));
    let tokens = read_wallet(&data.join("wallet.json")).len() as u64;
    let monthly = last["monthly"].as_u64();
    serde_json::json!({
        "stored": last["stored"],
        "monthly": monthly,
        "tokens": tokens,
        // whole months the wallet pays at this size, after what's paid
        "months": monthly.filter(|m| *m > 0).map(|m| tokens / m),
        "checked_at": last["at"],
    })
}

/// Take `n` tokens from the wallet, asking for the starter grant once when
/// it runs short.
fn take(wallet: &Path, n: u64, starter: impl FnOnce() -> Result<Vec<Token>>) -> Result<Vec<Token>> {
    let n = usize::try_from(n)?;
    if n == 0 {
        return Ok(Vec::new());
    }
    let mut tokens = read_wallet(wallet);
    if tokens.len() < n {
        tokens.extend(starter()?);
        // banked at once: the grant comes once a month, short or not
        write_private(wallet, &serde_json::to_vec(&tokens)?)?;
    }
    anyhow::ensure!(
        tokens.len() >= n,
        "out of storage credit: {n} tokens needed, {} held; give more space to the network",
        tokens.len()
    );
    let rest = tokens.split_off(n);
    write_private(wallet, &serde_json::to_vec(&rest)?)?;
    Ok(tokens)
}

fn read_wallet(path: &Path) -> Vec<Token> {
    read_json(path)
}

/// A JSON file this computer keeps, or its default when there's none yet
/// or it can't be read.
fn read_json<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// When each holder was last renewed for the bucket `key` reaches, as
/// `holders.json` keeps it: empty for another bucket (an older epoch's).
fn read_holders(path: &Path, key: &str) -> BTreeMap<omacloud_net::blob::NodeId, u64> {
    let kept: serde_json::Value = read_json(path);
    if kept["bucket"].as_str() != Some(bucket_of(key).as_str()) {
        return BTreeMap::new();
    }
    serde_json::from_value::<BTreeMap<String, u64>>(kept["last"].clone())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(id, t)| Some((hex::decode(id).ok()?.try_into().ok()?, t)))
        .collect()
}

/// Which bucket an access key reaches, without keeping the key itself.
fn bucket_of(key: &str) -> String {
    omacloud_net::seal::Keys::from_secret(key).scope
}

/// A map of node to time kept in `path`, empty when there's none.
fn read_node_times(path: &Path) -> BTreeMap<omacloud_net::blob::NodeId, u64> {
    read_json::<BTreeMap<String, u64>>(path)
        .into_iter()
        .filter_map(|(id, t)| Some((hex::decode(id).ok()?.try_into().ok()?, t)))
        .collect()
}

/// Holders that failed this computer, and until when they're passed over;
/// those whose time is up are left out.
fn read_suspects(data: &Path) -> BTreeMap<omacloud_net::blob::NodeId, u64> {
    let mut suspects = read_node_times(&data.join("suspects.json"));
    suspects.retain(|_, until| *until > now());
    suspects
}

fn hexed(times: &BTreeMap<omacloud_net::blob::NodeId, u64>) -> BTreeMap<String, u64> {
    times.iter().map(|(id, t)| (hex::encode(id), *t)).collect()
}

fn read_number(path: &Path) -> u64 {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Write a file only its owner reads, whole or not at all, and on disk
/// before it replaces the old one.
pub use omacloud_core::head::write_private;

/// A size as given on the command line: bytes, or with K, M, G or T
/// (binary units).
///
/// # Errors
///
/// If it isn't one.
pub fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = n.parse().with_context(|| format!("not a size: {s}"))?;
    let shift = match unit
        .trim()
        .to_ascii_uppercase()
        .trim_end_matches("IB")
        .trim_end_matches('B')
    {
        "" => 0,
        "K" => 10,
        "M" => 20,
        "G" => 30,
        "T" => 40,
        _ => anyhow::bail!("not a size: {s} (use K, M, G or T)"),
    };
    n.checked_shl(shift).filter(|v| v >> shift == n).context("too large")
}

/// Bytes as GiB or MiB, for the log.
#[must_use]
pub fn human(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let b = bytes as f64;
    if bytes >= 1 << 30 {
        format!("{:.1} GiB", b / f64::from(1u32 << 30))
    } else {
        format!("{:.1} MiB", b / f64::from(1u32 << 20))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_share_is_renewed_daily_when_it_outlives_its_first_week() {
        let day = 86_400;
        let week = Shared {
            secret: String::new(),
            name: "a".into(),
            made: 0,
            until: 7 * day,
            renewed_at: 0,
            holders: BTreeMap::new(),
        };
        assert_eq!(week.renewal(day), None, "its first lease covers it");
        let year = Shared {
            until: 365 * day,
            ..week.clone()
        };
        assert_eq!(year.renewal(day), Some(day + LEASE));
        let renewed = Shared {
            renewed_at: day,
            ..year.clone()
        };
        assert_eq!(renewed.renewal(day + 3600), None, "once a day");
        assert_eq!(renewed.renewal(2 * day), Some(2 * day + LEASE));
        // never past its end, and not once it ended (its take-down retried)
        let month = Shared {
            until: 30 * day,
            ..week
        };
        assert_eq!(month.renewal(day), Some(30 * day));
        assert_eq!(month.renewal(31 * day), None);
    }

    #[test]
    fn holders_renewal_dates_belong_to_one_bucket() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("holders.json");
        let last: BTreeMap<omacloud_net::blob::NodeId, u64> = [([7u8; 32], 100)].into();
        let kept = serde_json::json!({ "bucket": bucket_of("epoch 1 key"), "last": hexed(&last) });
        fs::write(&path, kept.to_string())?;
        assert_eq!(read_holders(&path, "epoch 1 key"), last);
        // after a rotation, the new bucket starts afresh
        assert!(read_holders(&path, "epoch 2 key").is_empty());
        Ok(())
    }

    #[test]
    fn space_says_how_far_the_wallet_pays() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let before = space(dir.path());
        assert!(before["stored"].is_null() && before["months"].is_null());
        assert_eq!(before["tokens"], 0);

        let token = serde_json::json!({ "msg": "00".repeat(32), "randomizer": "00".repeat(32), "sig": "00" });
        fs::write(
            dir.path().join("wallet.json"),
            serde_json::json!(vec![token; 7]).to_string(),
        )?;
        fs::write(
            dir.path().join("space.json"),
            r#"{"stored": 5000, "monthly": 3, "at": 1}"#,
        )?;
        fs::write(dir.path().join("renewed"), "100")?;
        let s = space(dir.path());
        assert_eq!((s["stored"].as_u64(), s["tokens"].as_u64()), (Some(5000), Some(7)));
        assert_eq!(s["months"], 2);
        Ok(())
    }

    #[test]
    fn sizes_read_as_people_write_them() -> Result<()> {
        assert_eq!(parse_size("0")?, 0);
        assert_eq!(parse_size("200G")?, 200 << 30);
        assert!(parse_size("1.5G").is_err());
        assert_eq!(parse_size("512MiB")?, 512 << 20);
        assert_eq!(parse_size("2t")?, 2 << 40);
        assert!(parse_size("lots").is_err());
        Ok(())
    }

    #[test]
    fn the_wallet_pays_and_asks_for_the_starter_grant_only_when_short() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let wallet = tmp.path().join("wallet.json");
        assert!(take(&wallet, 0, || panic!("not needed"))?.is_empty());
        // nothing held, and no grant: refused, with the remedy
        let e = take(&wallet, 1, || Ok(Vec::new())).unwrap_err();
        assert!(format!("{e:#}").contains("give more space"), "{e:#}");
        Ok(())
    }
}
