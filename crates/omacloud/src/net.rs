//! What the daemon does on the Omacloud network beside syncing: keep the
//! space this computer gives others (a node), say it's up, look after this
//! account's copies, and keep the tokens that pay for them.

use std::{
    fs,
    io::Write,
    os::unix::fs::OpenOptionsExt,
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
/// Renewals keep copies this far ahead, and come again when less than a
/// month is left; holders are paid for the time they served.
const LEASE: u64 = 2 * MONTH;
/// Tokens asked for when the wallet runs short and nothing else pays.
const STARTER: usize = 10;
/// How long a holder that failed this computer is passed over.
const SUSPECT_FOR: u64 = 30 * 24 * 3600;

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
        // monthly, paying each holder for the month it kept the copies
        let renewed = read_number(&self.data.join("renewed"));
        let renew = (now().saturating_sub(renewed) >= LEASE - MONTH).then_some(Renew {
            until: now() + LEASE,
            last: renewed,
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
        if renew.is_some() {
            write_private(&self.data.join("renewed"), now().to_string().as_bytes())?;
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
        let mut shares = read_shares(&self.data);
        let before = shares.len();
        shares.retain(|s| s.until > now());
        let wallet = self.data.join("wallet.json");
        let network = session.network();
        let mut changed = shares.len() != before;
        for s in &mut shares {
            let Some(until) = s.renewal(now()) else { continue };
            let key = omacloud_net::share::bucket_key(&s.secret);
            let mut pay = |n: u64| take(&wallet, n, || network.starter(STARTER));
            let renew = Renew {
                until,
                last: s.renewed_at,
            };
            match session.upkeep(&key, 0, Some(renew), &mut pay) {
                Ok(_) => {
                    s.paid_until = until;
                    s.renewed_at = now();
                    changed = true;
                }
                Err(e) => warn!("keeping the shared {}: {e:#}", s.name),
            }
        }
        if changed && let Err(e) = write_shares(&self.data, &shares) {
            warn!("saving shares: {e:#}");
        }
    }

    /// Trade the tokens this node took for fresh ones, into the wallet.
    fn deposit(&self, session: &Session, node: &Node) -> Result<()> {
        let taken = node.tokens()?;
        if taken.is_empty() {
            return Ok(());
        }
        let fresh = session.network().exchange(&taken)?;
        let wallet = self.data.join("wallet.json");
        let mut tokens = read_wallet(&wallet);
        tokens.extend(fresh);
        write_private(&wallet, &serde_json::to_vec(&tokens)?)?;
        node.mark_deposited(&taken)?;
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
    /// Renewed this far, beyond the first lease.
    #[serde(default)]
    pub paid_until: u64,
    /// When it was last renewed (and its holders paid up to).
    #[serde(default)]
    pub renewed_at: u64,
}

impl Shared {
    /// Until when to renew it now, if it's due: the first lease covers a
    /// week, then renewals keep it up to two months ahead, coming again
    /// when less than one is left, never past `until`.
    #[must_use]
    pub fn renewal(&self, now: u64) -> Option<u64> {
        let covered = self.paid_until.max(self.made + FIRST_LEASE);
        (covered < self.until && covered <= now + MONTH).then(|| self.until.min(now + LEASE))
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
    let renewed = read_number(&data.join("renewed"));
    serde_json::json!({
        "stored": last["stored"],
        "monthly": monthly,
        "tokens": tokens,
        // whole months the wallet pays at this size, after what's paid
        "months": monthly.filter(|m| *m > 0).map(|m| tokens / m),
        "paid_until": (renewed > 0).then_some(renewed + LEASE),
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

/// Holders that failed this computer, and until when they're passed over;
/// those whose time is up are left out.
fn read_suspects(data: &Path) -> std::collections::BTreeMap<omacloud_net::blob::NodeId, u64> {
    let kept: std::collections::BTreeMap<String, u64> = read_json(&data.join("suspects.json"));
    kept.into_iter()
        .filter(|(_, until)| *until > now())
        .filter_map(|(id, until)| Some((hex::decode(id).ok()?.try_into().ok()?, until)))
        .collect()
}

fn hexed(
    suspects: &std::collections::BTreeMap<omacloud_net::blob::NodeId, u64>,
) -> std::collections::BTreeMap<String, u64> {
    suspects.iter().map(|(id, until)| (hex::encode(id), *until)).collect()
}

fn read_number(path: &Path) -> u64 {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Write a file only its owner reads, whole or not at all: a crash midway
/// leaves the old one.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?
        .write_all(bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

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
    fn a_share_is_renewed_only_past_its_first_week_and_until_it_ends() {
        let day = 86_400;
        let week = Shared {
            secret: String::new(),
            name: "a".into(),
            made: 0,
            until: 7 * day,
            paid_until: 0,
            renewed_at: 0,
        };
        assert_eq!(week.renewal(0), None);
        assert_eq!(week.renewal(6 * day), None);
        let year = Shared {
            until: 365 * day,
            ..week.clone()
        };
        // the first lease is within a month of ending from the start
        assert_eq!(year.renewal(0), Some(LEASE));
        let paid = Shared {
            paid_until: LEASE,
            ..year.clone()
        };
        assert_eq!(paid.renewal(day), None);
        assert_eq!(paid.renewal(LEASE - MONTH), Some(LEASE - MONTH + LEASE));
        // never past the end
        let month = Shared {
            until: 30 * day,
            ..week
        };
        assert_eq!(month.renewal(0), Some(30 * day));
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
        assert_eq!(s["paid_until"], 100 + LEASE);
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
