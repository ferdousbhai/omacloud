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

/// A name for a new bucket: `omacloud-` and ten random letters and digits,
/// so it says nothing about whose it is.
#[must_use]
pub fn new_bucket_name() -> String {
    use rand::Rng;
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rngs::OsRng;
    let tail: String = (0..10)
        .map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char)
        .collect();
    format!("omacloud-{tail}")
}

/// What became of a bucket creation.
#[derive(Debug, PartialEq, Eq)]
pub enum Created {
    /// Made now, or already this key's.
    Ours,
    /// Someone else has the name: pick another.
    Taken,
}

/// Create the private bucket `name` at `endpoint` (an S3 endpoint such as
/// `https://fsn1.your-objectstorage.com`, path style), signing with the
/// key. Providers that want the location spelled out get it on a second
/// try.
///
/// # Errors
///
/// If the provider refuses for another reason, such as a key that can't
/// create buckets, or can't be reached.
pub fn create_bucket(
    endpoint: &str,
    region: &str,
    access_key: &str,
    secret: &str,
    name: &str,
) -> Result<Created> {
    let mut answer = put_bucket(endpoint, region, access_key, secret, name, "")?;
    if answer.0 >= 400 && answer.1.contains("LocationConstraint") {
        let body = format!(
            "<CreateBucketConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
             <LocationConstraint>{region}</LocationConstraint></CreateBucketConfiguration>"
        );
        answer = put_bucket(endpoint, region, access_key, secret, name, &body)?;
    }
    match answer {
        (200..=299, _) => Ok(Created::Ours),
        (_, text) if text.contains("BucketAlreadyOwnedByYou") => Ok(Created::Ours),
        (_, text) if text.contains("BucketAlreadyExists") => Ok(Created::Taken),
        (status, text) => {
            let code = text
                .split("<Code>")
                .nth(1)
                .and_then(|t| t.split("</Code>").next())
                .unwrap_or("");
            match code {
                "SignatureDoesNotMatch" | "InvalidAccessKeyId" => anyhow::bail!(
                    "the provider refused the key: check the access key and the secret key"
                ),
                "AccessDenied" => anyhow::bail!(
                    "this key can't create buckets: make one at the provider and give its name"
                ),
                "TooManyBuckets" => anyhow::bail!(
                    "the provider allows no more buckets here: delete one, or give an existing \
                     bucket's name"
                ),
                _ => anyhow::bail!("the provider didn't create the bucket ({status} {code})"),
            }
        }
    }
}

/// PUT /`name` signed with AWS signature version 4. Returns the status and
/// the response body.
fn put_bucket(
    endpoint: &str,
    region: &str,
    access_key: &str,
    secret: &str,
    name: &str,
    body: &str,
) -> Result<(u16, String)> {
    use hmac::{Hmac, Mac};
    use sha2::{Digest, Sha256};
    let hmac = |key: &[u8], data: &str| -> Vec<u8> {
        let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("any key length");
        mac.update(data.as_bytes());
        mac.finalize().into_bytes().to_vec()
    };
    let endpoint = endpoint.trim_end_matches('/');
    let host = endpoint
        .split_once("://")
        .map_or(endpoint, |(_, rest)| rest)
        .split('/')
        .next()
        .unwrap_or_default();
    let now = rustic_core::jiff::Timestamp::now();
    let stamp = now.strftime("%Y%m%dT%H%M%SZ").to_string();
    let day = &stamp[..8];
    let payload = hex::encode(Sha256::digest(body.as_bytes()));
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";
    let canonical = format!(
        "PUT\n/{name}\n\nhost:{host}\nx-amz-content-sha256:{payload}\nx-amz-date:{stamp}\n\n\
         {signed_headers}\n{payload}"
    );
    let scope = format!("{day}/{region}/s3/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{}",
        hex::encode(Sha256::digest(canonical.as_bytes()))
    );
    let mut key = hmac(format!("AWS4{secret}").as_bytes(), day);
    for part in [region, "s3", "aws4_request"] {
        key = hmac(&key, part);
    }
    let signature = hex::encode(hmac(&key, &to_sign));
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let mut response = agent
        .put(&format!("{endpoint}/{name}"))
        .header("x-amz-date", &stamp)
        .header("x-amz-content-sha256", &payload)
        .header(
            "authorization",
            &format!(
                "AWS4-HMAC-SHA256 Credential={access_key}/{scope}, \
                 SignedHeaders={signed_headers}, Signature={signature}"
            ),
        )
        .send(body)
        .with_context(|| format!("reaching {endpoint}"))?;
    let status = response.status().as_u16();
    let text = response.body_mut().read_to_string().unwrap_or_default();
    Ok((status, text))
}
