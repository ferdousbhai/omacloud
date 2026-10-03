//! Signed, hash chained heads, and the coordinator that relays them.
//!
//! The coordinator orders heads and relays them; it does not decide. Each
//! head names a snapshot, is signed by the device that pushed it, links to the
//! previous head by hash, and names the device chain position its signer saw.
//! A device remembers the last head it verified, so a coordinator that rolls
//! back, withholds, forks or forges history is caught; and a head counts only
//! if its signer was a valid device at that position (see [`crate::devices`]).

use std::{
    collections::BTreeSet,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use ed25519_dalek::{Signer, SigningKey};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};

use crate::{
    devices::{Anchor, DeviceChain, DeviceEntry, JoinRequest, public_hex, verify_sig},
    epoch::{EpochRecord, Grant},
};

/// One entry in a folder's history.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Head {
    /// Position in the chain, starting at 1.
    pub seq: u64,
    /// The restic snapshot id this head points at.
    pub snapshot: String,
    /// `hash()` of the previous head; empty for the first.
    pub prev: String,
    /// Device chain length the signer had verified when signing.
    pub devices: u64,
    /// Repository epoch the snapshot lives in (see [`crate::epoch`]).
    #[serde(default)]
    pub epoch: u64,
    /// The signing device's public key, hex.
    pub device: String,
    /// Signature over `signed_bytes()`, hex.
    pub sig: String,
}

impl Head {
    fn signed_bytes(
        seq: u64,
        snapshot: &str,
        prev: &str,
        devices: u64,
        epoch: u64,
        device: &str,
    ) -> Vec<u8> {
        format!("omacloud-head-v3\n{seq}\n{snapshot}\n{prev}\n{devices}\n{epoch}\n{device}\n")
            .into_bytes()
    }

    /// Build and sign the head that follows `prev`, in epoch 0.
    #[must_use]
    pub fn next(key: &SigningKey, prev: Option<&Self>, snapshot: &str, devices: u64) -> Self {
        Self::next_in(key, prev, snapshot, devices, 0)
    }

    /// Build and sign the head that follows `prev`, in `epoch`.
    #[must_use]
    pub fn next_in(
        key: &SigningKey,
        prev: Option<&Self>,
        snapshot: &str,
        devices: u64,
        epoch: u64,
    ) -> Self {
        let seq = prev.map_or(1, |p| p.seq + 1);
        let prev = prev.map(Self::hash).unwrap_or_default();
        let device = public_hex(key);
        let sig = key.sign(&Self::signed_bytes(
            seq, snapshot, &prev, devices, epoch, &device,
        ));
        Self {
            seq,
            snapshot: snapshot.to_string(),
            prev,
            devices,
            epoch,
            device,
            sig: hex::encode(sig.to_bytes()),
        }
    }

    /// Hash of the whole signed entry; the next head's `prev`.
    #[must_use]
    pub fn hash(&self) -> String {
        let mut h = Sha256::new();
        h.update(self.bytes());
        h.update(self.sig.as_bytes());
        hex::encode(h.finalize())
    }

    fn bytes(&self) -> Vec<u8> {
        Self::signed_bytes(
            self.seq,
            &self.snapshot,
            &self.prev,
            self.devices,
            self.epoch,
            &self.device,
        )
    }
}

/// What a device refuses to accept from a coordinator.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HeadError {
    #[error("rollback: coordinator serves head {served:?}, but head {known} was already seen")]
    Rollback { known: u64, served: Option<u64> },
    #[error("fork at head {seq}: coordinator serves a different history than was seen")]
    Fork { seq: u64 },
    #[error("head {seq} is not signed by the device it names")]
    BadSignature { seq: u64 },
    #[error("head {seq} is signed by {device}, not a valid device at device entry {devices}")]
    UntrustedDevice {
        seq: u64,
        device: String,
        devices: u64,
    },
    #[error("head {seq} names device entry {devices}, which this device hasn't been shown")]
    UnknownDevices { seq: u64, devices: u64 },
    #[error("head {seq} names an older device list than the head before it")]
    StaleDevices { seq: u64 },
    #[error("head {seq} goes back to an older repository epoch")]
    StaleEpoch { seq: u64 },
    #[error("head {seq} does not follow head {after}")]
    Broken { seq: u64, after: u64 },
}

/// Orders and relays heads, device entries and join requests. Implementations:
/// a directory (tests, LAN, development) and, later, the per account Durable
/// Object. It is trusted for availability only.
pub trait Coordinator: Send + Sync {
    /// The latest head, if any.
    fn head(&self) -> Result<Option<Head>>;
    /// All heads with `seq > after`, in order.
    fn since(&self, after: u64) -> Result<Vec<Head>>;
    /// Append `head` if it is exactly the next one. `Ok(false)` if another
    /// head already took that position.
    fn append(&self, head: &Head) -> Result<bool>;

    /// The latest device entry, if any.
    fn device_head(&self) -> Result<Option<DeviceEntry>>;
    /// All device entries with `seq > after`, in order.
    fn devices_since(&self, after: u64) -> Result<Vec<DeviceEntry>>;
    /// Append a device entry if it is exactly the next one.
    fn append_device(&self, entry: &DeviceEntry) -> Result<bool>;

    /// The account anchor, set once when the account is created.
    fn anchor(&self) -> Result<Option<Anchor>>;
    /// Set the anchor; `Ok(false)` if one exists.
    fn set_anchor(&self, anchor: &Anchor) -> Result<bool>;

    /// Append a rotation's head and its epoch record together, so no device
    /// sees the new epoch without its keys. `Ok(false)` if the head's
    /// position was taken. The default appends one after the other.
    fn append_rotation(&self, head: &Head, record: &EpochRecord) -> Result<bool> {
        if !self.append(head)? {
            return Ok(false);
        }
        anyhow::ensure!(
            self.append_epoch(record)?,
            "an epoch record was published at the same time"
        );
        Ok(true)
    }

    /// Block until a head with `seq > after` exists or the coordinator's
    /// wait runs out; returns the latest seq. The default doesn't wait.
    fn wait(&self, after: u64) -> Result<u64> {
        Ok(self.head()?.map_or(after, |h| h.seq))
    }

    /// All epoch records, from epoch 0.
    fn epochs(&self) -> Result<Vec<EpochRecord>>;
    /// Append an epoch record if it is exactly the next one.
    fn append_epoch(&self, record: &EpochRecord) -> Result<bool>;
    /// Grants sealed to `device`.
    fn grants(&self, device: &str) -> Result<Vec<Grant>>;
    /// Store a grant.
    fn put_grant(&self, grant: &Grant) -> Result<()>;

    /// Pending join requests.
    fn requests(&self) -> Result<Vec<JoinRequest>>;
    /// Leave a join request.
    fn request(&self, req: &JoinRequest) -> Result<()>;
    /// Drop a join request (after approving or declining it).
    fn remove_request(&self, device: &str) -> Result<()>;

    /// Bucket key changes of a self-hosted account, in order (see
    /// [`crate::bucket_key`]). Coordinators without own buckets have none.
    fn key_records(&self) -> Result<Vec<crate::bucket_key::KeyRecord>> {
        Ok(Vec::new())
    }
    /// Append a bucket key change if it is exactly the next one.
    fn append_key_record(&self, _record: &crate::bucket_key::KeyRecord) -> Result<bool> {
        anyhow::bail!("this account doesn't keep its own bucket key")
    }
    /// Note that `device` switched to key change `seq`.
    fn ack_key(&self, _seq: u64, _device: &str) -> Result<()> {
        Ok(())
    }
    /// The devices that switched to key change `seq`.
    fn key_acks(&self, _seq: u64) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
}

/// Everything as files in a directory: `heads/`, `devices/`, `epochs/`,
/// `grants/`, `requests/` and `account.json`. Appends are an atomic compare and swap across
/// processes: the entry is written to a temp file and hard linked into place,
/// which fails if the name exists.
#[derive(Debug, Clone)]
pub struct DirCoordinator {
    dir: PathBuf,
}

impl DirCoordinator {
    /// Use (and create) `dir`.
    ///
    /// # Errors
    ///
    /// If the directories can't be created.
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        for sub in [
            "heads", "devices", "epochs", "grants", "requests", "keys", "key-acks",
        ] {
            fs::create_dir_all(dir.join(sub))
                .with_context(|| format!("creating {}", dir.join(sub).display()))?;
        }
        Ok(Self { dir })
    }

    fn seqs(&self, kind: &str) -> Result<BTreeSet<u64>> {
        let mut out = BTreeSet::new();
        for e in fs::read_dir(self.dir.join(kind))? {
            let name = e?.file_name();
            let name = name.to_string_lossy();
            if let Some(seq) = name.strip_suffix(".json").and_then(|s| s.parse().ok()) {
                _ = out.insert(seq);
            }
        }
        Ok(out)
    }

    fn path(&self, kind: &str, seq: u64) -> PathBuf {
        self.dir.join(kind).join(format!("{seq:020}.json"))
    }

    fn read<T: DeserializeOwned>(path: &Path) -> Result<T> {
        let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    fn last<T: DeserializeOwned>(&self, kind: &str) -> Result<Option<T>> {
        self.seqs(kind)?
            .last()
            .map(|&s| Self::read(&self.path(kind, s)))
            .transpose()
    }

    fn after<T: DeserializeOwned>(&self, kind: &str, after: u64) -> Result<Vec<T>> {
        self.seqs(kind)?
            .range(after + 1..)
            .map(|&s| Self::read(&self.path(kind, s)))
            .collect()
    }

    /// Write `value` at `path` only if nothing is there yet.
    fn create<T: Serialize>(&self, path: &Path, value: &T) -> Result<bool> {
        let bytes = serde_json::to_vec_pretty(value)?;
        let tmp = self.dir.join(format!(
            ".tmp-{}-{}",
            std::process::id(),
            hex::encode(Sha256::digest(&bytes))
        ));
        fs::write(&tmp, bytes)?;
        let res = fs::hard_link(&tmp, path);
        _ = fs::remove_file(&tmp);
        match res {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    fn append_seq<T: Serialize>(&self, kind: &str, seq: u64, value: &T) -> Result<bool> {
        let current = self.seqs(kind)?.last().copied().unwrap_or(0);
        if seq != current + 1 {
            return Ok(false);
        }
        self.create(&self.path(kind, seq), value)
    }
}

impl Coordinator for DirCoordinator {
    fn head(&self) -> Result<Option<Head>> {
        self.last("heads")
    }
    fn since(&self, after: u64) -> Result<Vec<Head>> {
        self.after("heads", after)
    }
    fn append(&self, head: &Head) -> Result<bool> {
        self.append_seq("heads", head.seq, head)
    }
    fn device_head(&self) -> Result<Option<DeviceEntry>> {
        self.last("devices")
    }
    fn devices_since(&self, after: u64) -> Result<Vec<DeviceEntry>> {
        self.after("devices", after)
    }
    fn append_device(&self, entry: &DeviceEntry) -> Result<bool> {
        self.append_seq("devices", entry.seq, entry)
    }
    fn anchor(&self) -> Result<Option<Anchor>> {
        let path = self.dir.join("account.json");
        match fs::metadata(&path) {
            Ok(_) => Ok(Some(Self::read(&path)?)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    fn set_anchor(&self, anchor: &Anchor) -> Result<bool> {
        self.create(&self.dir.join("account.json"), anchor)
    }
    fn epochs(&self) -> Result<Vec<EpochRecord>> {
        self.after("epochs", 0)
    }
    fn append_epoch(&self, record: &EpochRecord) -> Result<bool> {
        // stored at epoch + 1, as the files count from 1
        self.append_seq("epochs", record.epoch + 1, record)
    }
    fn grants(&self, device: &str) -> Result<Vec<Grant>> {
        let mut out = Vec::new();
        for e in fs::read_dir(self.dir.join("grants"))? {
            let path = e?.path();
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            if name.starts_with(&format!("{device}-")) && name.ends_with(".json") {
                out.push(Self::read(&path)?);
            }
        }
        Ok(out)
    }
    fn put_grant(&self, grant: &Grant) -> Result<()> {
        let path = self
            .dir
            .join("grants")
            .join(format!("{}-{}.json", grant.device, grant.epoch));
        fs::write(path, serde_json::to_vec_pretty(grant)?)?;
        Ok(())
    }
    fn requests(&self) -> Result<Vec<JoinRequest>> {
        let mut out = Vec::new();
        for e in fs::read_dir(self.dir.join("requests"))? {
            let path = e?.path();
            if path.extension().is_some_and(|x| x == "json") {
                out.push(Self::read(&path)?);
            }
        }
        Ok(out)
    }
    fn request(&self, req: &JoinRequest) -> Result<()> {
        let path = self
            .dir
            .join("requests")
            .join(format!("{}.json", req.device));
        fs::write(path, serde_json::to_vec_pretty(req)?)?;
        Ok(())
    }
    fn remove_request(&self, device: &str) -> Result<()> {
        match fs::remove_file(self.dir.join("requests").join(format!("{device}.json"))) {
            Err(e) if e.kind() != ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }
    fn key_records(&self) -> Result<Vec<crate::bucket_key::KeyRecord>> {
        self.after("keys", 0)
    }
    fn append_key_record(&self, record: &crate::bucket_key::KeyRecord) -> Result<bool> {
        self.append_seq("keys", record.seq, record)
    }
    fn ack_key(&self, seq: u64, device: &str) -> Result<()> {
        let dir = self.dir.join("key-acks").join(seq.to_string());
        fs::create_dir_all(&dir)?;
        fs::write(dir.join(device), b"")?;
        Ok(())
    }
    fn key_acks(&self, seq: u64) -> Result<Vec<String>> {
        match fs::read_dir(self.dir.join("key-acks").join(seq.to_string())) {
            Ok(it) => Ok(it
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }
}

/// Verifies what a coordinator serves against what this device has seen.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HeadTracker {
    /// The last head this device verified.
    pub known: Option<Head>,
}

impl HeadTracker {
    /// Fetch and verify the coordinator's latest head, and remember it.
    /// `devices` must be advanced first.
    ///
    /// # Errors
    ///
    /// A [`HeadError`] if the coordinator misbehaves, or an I/O error.
    pub fn advance(
        &mut self,
        coord: &dyn Coordinator,
        devices: &DeviceChain,
    ) -> Result<Option<Head>> {
        let known_seq = self.known.as_ref().map_or(0, |k| k.seq);
        let served = coord.head()?;
        match (&self.known, &served) {
            (Some(k), None) => {
                return Err(HeadError::Rollback {
                    known: k.seq,
                    served: None,
                }
                .into());
            }
            (Some(k), Some(h)) if h.seq < k.seq => {
                return Err(HeadError::Rollback {
                    known: k.seq,
                    served: Some(h.seq),
                }
                .into());
            }
            (Some(k), Some(h)) if h.seq == k.seq => {
                if h != k {
                    return Err(HeadError::Fork { seq: h.seq }.into());
                }
                return Ok(Some(h.clone()));
            }
            (None, None) => return Ok(None),
            _ => {}
        }
        let mut last = self.known.clone();
        for head in coord.since(known_seq)? {
            check(last.as_ref(), &head, devices)?;
            last = Some(head);
        }
        // `since` may run ahead of `head` (a push landed in between), never behind
        if last.as_ref().map_or(0, |l| l.seq) < served.as_ref().map_or(0, |s| s.seq) {
            return Err(HeadError::Fork {
                seq: served.map_or(0, |s| s.seq),
            }
            .into());
        }
        self.known.clone_from(&last);
        Ok(last)
    }

    /// Verify a head this device appended, then remember it.
    ///
    /// # Errors
    ///
    /// If `head` doesn't follow the known head.
    pub fn accept_own(&mut self, head: &Head, devices: &DeviceChain) -> Result<()> {
        check(self.known.as_ref(), head, devices)?;
        self.known = Some(head.clone());
        Ok(())
    }
}

/// Verify that `head` follows `prev`, is signed by the device it names, and
/// that device was valid at the device chain position the head names.
///
/// # Errors
///
/// The [`HeadError`] that describes what's wrong.
pub fn check(prev: Option<&Head>, head: &Head, devices: &DeviceChain) -> Result<(), HeadError> {
    let seq = head.seq;
    let after = prev.map_or(0, |p| p.seq);
    let prev_hash = prev.map(Head::hash).unwrap_or_default();
    if seq != after + 1 || head.prev != prev_hash {
        return Err(HeadError::Broken { seq, after });
    }
    if !verify_sig(&head.device, &head.sig, &head.bytes()) {
        return Err(HeadError::BadSignature { seq });
    }
    if head.devices > devices.len() {
        return Err(HeadError::UnknownDevices {
            seq,
            devices: head.devices,
        });
    }
    // the device list a head names never goes back, so once any device has
    // pushed after a revocation, the revoked key can't sign its way back in
    if prev.is_some_and(|p| head.devices < p.devices) {
        return Err(HeadError::StaleDevices { seq });
    }
    // likewise for repository epochs: once rotated, no head may point back
    // into a repository a removed device can still read
    if prev.is_some_and(|p| head.epoch < p.epoch) {
        return Err(HeadError::StaleEpoch { seq });
    }
    if !devices.is_valid_at(&head.device, head.devices) {
        return Err(HeadError::UntrustedDevice {
            seq,
            device: head.device.clone(),
            devices: head.devices,
        });
    }
    Ok(())
}

/// Read a device signing key from `path`, creating it (mode 0600) if absent.
///
/// # Errors
///
/// If the file can't be read or written, or holds a malformed key.
pub fn load_or_create_key(path: &Path) -> Result<SigningKey> {
    match fs::read_to_string(path) {
        Ok(s) => {
            let bytes: [u8; 32] = hex::decode(s.trim())?
                .try_into()
                .map_err(|_| anyhow::anyhow!("{}: not a 32 byte key", path.display()))?;
            Ok(SigningKey::from_bytes(&bytes))
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {
            let key = SigningKey::generate(&mut rand::rngs::OsRng);
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir)?;
            }
            write_private(path, hex::encode(key.to_bytes()).as_bytes())?;
            Ok(key)
        }
        Err(e) => Err(e.into()),
    }
}

/// Write a file readable only by its owner.
///
/// # Errors
///
/// If the file can't be written.
pub fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let tmp = path.with_extension("tmp");
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::{Action, genesis};

    fn key() -> SigningKey {
        SigningKey::generate(&mut rand::rngs::OsRng)
    }

    /// A root and two devices, a and b.
    fn chain(a: &SigningKey, b: &SigningKey) -> DeviceChain {
        let root = key();
        let mut c = DeviceChain::new(&public_hex(&root));
        c.push(genesis(&root, a, "a")).unwrap();
        c.push(DeviceEntry::next(
            a,
            c.last(),
            Action::Add {
                device: public_hex(b),
                name: "b".into(),
            },
        ))
        .unwrap();
        c
    }

    /// A coordinator that serves whatever heads it's told to.
    #[derive(Default)]
    struct Evil(std::sync::Mutex<Vec<Head>>);

    impl Coordinator for Evil {
        fn head(&self) -> Result<Option<Head>> {
            Ok(self.0.lock().unwrap().last().cloned())
        }
        fn since(&self, after: u64) -> Result<Vec<Head>> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .iter()
                .filter(|h| h.seq > after)
                .cloned()
                .collect())
        }
        fn append(&self, head: &Head) -> Result<bool> {
            self.0.lock().unwrap().push(head.clone());
            Ok(true)
        }
        fn device_head(&self) -> Result<Option<DeviceEntry>> {
            Ok(None)
        }
        fn devices_since(&self, _: u64) -> Result<Vec<DeviceEntry>> {
            Ok(vec![])
        }
        fn append_device(&self, _: &DeviceEntry) -> Result<bool> {
            Ok(false)
        }
        fn anchor(&self) -> Result<Option<Anchor>> {
            Ok(None)
        }
        fn set_anchor(&self, _: &Anchor) -> Result<bool> {
            Ok(false)
        }
        fn epochs(&self) -> Result<Vec<EpochRecord>> {
            Ok(vec![])
        }
        fn append_epoch(&self, _: &EpochRecord) -> Result<bool> {
            Ok(false)
        }
        fn grants(&self, _: &str) -> Result<Vec<Grant>> {
            Ok(vec![])
        }
        fn put_grant(&self, _: &Grant) -> Result<()> {
            Ok(())
        }
        fn requests(&self) -> Result<Vec<JoinRequest>> {
            Ok(vec![])
        }
        fn request(&self, _: &JoinRequest) -> Result<()> {
            Ok(())
        }
        fn remove_request(&self, _: &str) -> Result<()> {
            Ok(())
        }
    }

    fn err(r: Result<Option<Head>>) -> HeadError {
        r.unwrap_err().downcast().unwrap()
    }

    #[test]
    fn dir_coordinator_is_compare_and_swap() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let c = DirCoordinator::new(dir.path())?;
        let (a, b) = (key(), key());
        let devices = chain(&a, &b);
        let h1 = Head::next(&a, None, "s1", 2);
        assert!(c.append(&h1)?);
        // two devices race for seq 2: exactly one wins
        let (x, y) = (
            Head::next(&a, Some(&h1), "s2a", 2),
            Head::next(&b, Some(&h1), "s2b", 2),
        );
        assert!(c.append(&x)?);
        assert!(!c.append(&y)?);
        assert_eq!(c.head()?, Some(x));
        let mut t = HeadTracker::default();
        assert_eq!(t.advance(&c, &devices)?.map(|h| h.seq), Some(2));
        Ok(())
    }

    #[test]
    fn detects_rollback_fork_forgery_and_strangers() {
        let (a, b, stranger) = (key(), key(), key());
        let devices = chain(&a, &b);
        let h1 = Head::next(&a, None, "s1", 2);
        let h2 = Head::next(&a, Some(&h1), "s2", 2);
        let evil = Evil::default();
        evil.0.lock().unwrap().extend([h1.clone(), h2.clone()]);
        let mut t = HeadTracker::default();
        assert_eq!(t.advance(&evil, &devices).unwrap(), Some(h2.clone()));

        // rollback: the coordinator drops the latest head
        _ = evil.0.lock().unwrap().pop();
        assert_eq!(
            err(t.advance(&evil, &devices)),
            HeadError::Rollback {
                known: 2,
                served: Some(1)
            }
        );

        // fork: same position, different history
        evil.0
            .lock()
            .unwrap()
            .push(Head::next(&a, Some(&h1), "other", 2));
        assert_eq!(err(t.advance(&evil, &devices)), HeadError::Fork { seq: 2 });

        let fresh = |heads: Vec<Head>| {
            *evil.0.lock().unwrap() = heads;
            HeadTracker::default().advance(&evil, &devices)
        };
        // forged: snapshot changed after signing
        let mut forged = h2.clone();
        forged.snapshot = "evil".into();
        assert_eq!(
            err(fresh(vec![h1.clone(), forged])),
            HeadError::BadSignature { seq: 2 }
        );
        // validly signed, but not by a device of the account
        assert!(matches!(
            err(fresh(vec![
                h1.clone(),
                Head::next(&stranger, Some(&h1), "s2", 2)
            ])),
            HeadError::UntrustedDevice { seq: 2, .. }
        ));
        // naming device entries the coordinator never showed
        assert_eq!(
            err(fresh(vec![h1.clone(), Head::next(&a, Some(&h1), "s2", 3)])),
            HeadError::UnknownDevices { seq: 2, devices: 3 }
        );
        // b signing as of device entry 1, before it was added
        assert!(matches!(
            err(fresh(vec![Head::next(&b, None, "s1", 1)])),
            HeadError::UntrustedDevice {
                seq: 1,
                devices: 1,
                ..
            }
        ));
        // going back to an older device list
        assert_eq!(
            err(fresh(vec![h1.clone(), Head::next(&a, Some(&h1), "s2", 1)])),
            HeadError::StaleDevices { seq: 2 }
        );
        // a head that skips its parent
        assert_eq!(
            err(fresh(vec![h1.clone(), Head::next(&a, None, "s2", 2)])),
            HeadError::Broken { seq: 1, after: 1 }
        );
    }
}
