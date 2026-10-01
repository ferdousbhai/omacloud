//! Coordination in the user's own bucket: nothing in between.
//!
//! The same layout as [`crate::DirCoordinator`] (`heads/`, `devices/`,
//! `epochs/`, `grants/`, `requests/`, `account.json`), as objects. An append
//! is a create-only write (`If-None-Match: *`), which the bucket refuses if
//! another device took that position first: a compare and swap from the
//! bucket itself. S3, R2, MinIO and
//! Hetzner support it on unversioned buckets (Hetzner not on versioned ones,
//! so a bucket with object lock keeps its coordination in a second, plain
//! bucket).
//!
//! Devices learn of changes by polling, and cutting off a lost device's
//! storage takes changing the bucket key, since every device holds it.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Mutex, OnceLock},
};

use anyhow::{Context, Result};
use opendal::{ErrorKind, blocking::Operator, options};
use serde::{Serialize, de::DeserializeOwned};

use crate::{
    Head,
    devices::{Anchor, DeviceEntry, JoinRequest},
    epoch::{EpochRecord, Grant},
    head::Coordinator,
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

/// Parallel reads when catching up on a long history.
const READERS: usize = 16;

/// Whether `err` is the bucket refusing this computer's key: deleted at the
/// provider, most likely after a key change this computer missed.
#[must_use]
pub fn key_refused(err: &anyhow::Error) -> bool {
    err.chain().any(|e| {
        e.downcast_ref::<opendal::Error>()
            .is_some_and(|e| e.kind() == ErrorKind::PermissionDenied)
    })
}

pub struct BucketCoordinator {
    op: Operator,
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
        let op = opendal::Operator::via_iter(scheme, options.clone())
            .with_context(|| format!("opening the {scheme} location for coordination"))?;
        Ok(Self {
            op: Operator::new(op)?,
            last: Mutex::new(HashMap::new()),
        })
    }

    fn key(kind: &str, seq: u64) -> String {
        format!("{kind}/{seq:020}.json")
    }

    fn get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        let _guard = runtime().enter();
        match self.op.read(key) {
            Ok(buf) => Ok(Some(serde_json::from_slice(&buf.to_vec())?)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {key}")),
        }
    }

    /// Write only if nothing is there: `Ok(false)` if something is.
    fn create<T: Serialize>(&self, key: &str, value: &T) -> Result<bool> {
        let _guard = runtime().enter();
        let opts = options::WriteOptions {
            if_not_exists: true,
            ..Default::default()
        };
        match self.op.write_options(key, serde_json::to_vec(value)?, opts) {
            Ok(_) => Ok(true),
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::ConditionNotMatch | ErrorKind::AlreadyExists
                ) =>
            {
                Ok(false)
            }
            Err(e) => Err(e).with_context(|| format!("writing {key}")),
        }
    }

    fn put<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        let _guard = runtime().enter();
        self.op
            .write(key, serde_json::to_vec(value)?)
            .with_context(|| format!("writing {key}"))?;
        Ok(())
    }

    fn names(&self, dir: &str) -> Result<Vec<String>> {
        let _guard = runtime().enter();
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

    /// The last seq in a log: listed once, then probed forward.
    fn last_seq(&self, kind: &'static str) -> Result<u64> {
        let known = self.last.lock().unwrap().get(kind).copied();
        let mut seq = match known {
            Some(s) => s,
            None => self
                .names(kind)?
                .iter()
                .filter_map(|n| n.strip_suffix(".json")?.parse::<u64>().ok())
                .max()
                .unwrap_or(0),
        };
        let _guard = runtime().enter();
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
        let last = self.last_seq(kind)?;
        let seqs: Vec<u64> = (after + 1..=last).collect();
        let mut out: Vec<Option<T>> = Vec::with_capacity(seqs.len());
        out.resize_with(seqs.len(), || None);
        let next = std::sync::atomic::AtomicUsize::new(0);
        let slots: Vec<Mutex<Option<T>>> = out.into_iter().map(Mutex::new).collect();
        let failed: Mutex<Option<anyhow::Error>> = Mutex::new(None);
        std::thread::scope(|scope| {
            for _ in 0..READERS.min(seqs.len()) {
                scope.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(seq) = seqs.get(i) else { return };
                        match self.get::<T>(&Self::key(kind, *seq)) {
                            Ok(Some(v)) => *slots[i].lock().unwrap() = Some(v),
                            Ok(None) => {
                                _ = failed
                                    .lock()
                                    .unwrap()
                                    .get_or_insert(anyhow::anyhow!("{kind} {seq} is missing"));
                                return;
                            }
                            Err(e) => {
                                _ = failed.lock().unwrap().get_or_insert(e);
                                return;
                            }
                        }
                    }
                });
            }
        });
        if let Some(e) = failed.into_inner().unwrap() {
            return Err(e);
        }
        Ok(slots
            .into_iter()
            .filter_map(|m| m.into_inner().unwrap())
            .collect())
    }

    fn append_seq<T: Serialize>(&self, kind: &'static str, seq: u64, value: &T) -> Result<bool> {
        if seq != self.last_seq(kind)? + 1 {
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
    fn head(&self) -> Result<Option<Head>> {
        match self.last_seq("heads")? {
            0 => Ok(None),
            s => self.get(&Self::key("heads", s)),
        }
    }
    fn since(&self, after: u64) -> Result<Vec<Head>> {
        self.after("heads", after)
    }
    fn append(&self, head: &Head) -> Result<bool> {
        self.append_seq("heads", head.seq, head)
    }
    fn device_head(&self) -> Result<Option<DeviceEntry>> {
        match self.last_seq("devices")? {
            0 => Ok(None),
            s => self.get(&Self::key("devices", s)),
        }
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
        self.names("grants")?
            .iter()
            .filter(|n| n.starts_with(&prefix) && n.ends_with(".json"))
            .filter_map(|n| self.get(&format!("grants/{n}")).transpose())
            .collect()
    }
    fn put_grant(&self, grant: &Grant) -> Result<()> {
        self.put(
            &format!("grants/{}-{}.json", grant.device, grant.epoch),
            grant,
        )
    }
    fn requests(&self) -> Result<Vec<JoinRequest>> {
        self.names("requests")?
            .iter()
            .filter(|n| n.ends_with(".json"))
            .filter_map(|n| self.get(&format!("requests/{n}")).transpose())
            .collect()
    }
    fn request(&self, req: &JoinRequest) -> Result<()> {
        self.put(&format!("requests/{}.json", req.device), req)
    }
    fn key_records(&self) -> Result<Vec<crate::bucket_key::KeyRecord>> {
        self.after("keys", 0)
    }
    fn append_key_record(&self, record: &crate::bucket_key::KeyRecord) -> Result<bool> {
        self.append_seq("keys", record.seq, record)
    }
    fn ack_key(&self, seq: u64, device: &str) -> Result<()> {
        self.put(&format!("key-acks/{seq}/{device}"), &true)
    }
    fn key_acks(&self, seq: u64) -> Result<Vec<String>> {
        self.names(&format!("key-acks/{seq}"))
    }
    fn remove_request(&self, device: &str) -> Result<()> {
        let _guard = runtime().enter();
        self.op
            .delete(&format!("requests/{device}.json"))
            .context("removing a join request")?;
        Ok(())
    }
}
