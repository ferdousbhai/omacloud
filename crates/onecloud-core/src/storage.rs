//! Repository storage through the onecloud service, for the managed tiers.
//!
//! The device never holds bucket credentials. For each object it asks its
//! account's coordinator for a presigned URL and moves the bytes straight to
//! or from the bucket; listing and deleting go through the service. The
//! service scopes every path to the account and refuses removed devices, so
//! losing a device cuts it off from storage at once, not only from new keys.

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use rustic_core::{
    BytesList, ErrorKind, FileType, Id, ReadBackend, RusticError, RusticResult, WriteBackend,
};

use std::{
    collections::HashMap,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use crate::http::HttpCoordinator;

/// Requests to the service for presigned URLs, and to the bucket, since the
/// process started; for measuring.
pub static SIGN_REQUESTS: AtomicU64 = AtomicU64::new(0);
pub static OBJECT_REQUESTS: AtomicU64 = AtomicU64::new(0);

/// A restic repository stored with the service, under `prefix` (empty for
/// epoch 0, `e<n>/` after key rotations).
pub struct ServiceStorage {
    service: HttpCoordinator,
    prefix: String,
    /// Presigned GET URLs by path. A pull reads many files as ranges of the
    /// same few packs; one URL per pack spares the service a request per
    /// file.
    reads: Mutex<HashMap<String, (String, Instant)>>,
}

/// How long a cached GET URL is reused; the service signs for 15 minutes.
const URL_REUSE: Duration = Duration::from_secs(600);

fn backend_error(what: &str, err: &anyhow::Error) -> Box<RusticError> {
    RusticError::new(ErrorKind::Backend, format!("{what}: {err:#}"))
}

impl ServiceStorage {
    pub(crate) fn new(service: HttpCoordinator, prefix: &str) -> Self {
        let prefix = if prefix.is_empty() {
            String::new()
        } else {
            format!("{}/", prefix.trim_matches('/'))
        };
        Self {
            service,
            prefix,
            reads: Mutex::new(HashMap::new()),
        }
    }

    fn path(&self, tpe: FileType, id: &Id) -> String {
        let hex = id.to_hex();
        let hex = hex.as_str();
        let rel = match tpe {
            FileType::Config => "config".to_string(),
            FileType::Pack => format!("data/{}/{hex}", &hex[..2]),
            _ => format!("{}/{hex}", tpe.dirname()),
        };
        format!("{}{rel}", self.prefix)
    }

    fn read_url(&self, path: &str) -> Result<String> {
        if let Some((url, at)) = self.reads.lock().unwrap().get(path)
            && at.elapsed() < URL_REUSE
        {
            return Ok(url.clone());
        }
        SIGN_REQUESTS.fetch_add(1, Ordering::Relaxed);
        let url = self.service.presign("GET", path, None)?;
        _ = self
            .reads
            .lock()
            .unwrap()
            .insert(path.to_string(), (url.clone(), Instant::now()));
        Ok(url)
    }

    fn get(&self, path: &str, range: Option<(u32, u32)>) -> Result<Bytes> {
        OBJECT_REQUESTS.fetch_add(1, Ordering::Relaxed);
        let url = self.read_url(path)?;
        let mut req = self.service.agent().get(&url);
        if let Some((offset, length)) = range {
            let end = u64::from(offset) + u64::from(length) - 1;
            req = req.header("range", &format!("bytes={offset}-{end}"));
        }
        let res = req.call()?;
        let status = res.status().as_u16();
        if !(200..300).contains(&status) {
            bail!("reading {path}: storage answered {status}");
        }
        let body = res
            .into_body()
            .with_config()
            .limit(u64::MAX)
            .read_to_vec()?;
        Ok(Bytes::from(body))
    }

    fn put(&self, path: &str, content: &[u8]) -> Result<()> {
        SIGN_REQUESTS.fetch_add(1, Ordering::Relaxed);
        OBJECT_REQUESTS.fetch_add(1, Ordering::Relaxed);
        let url = self
            .service
            .presign("PUT", path, Some(content.len() as u64))?;
        let res = self.service.agent().put(&url).send(content)?;
        let status = res.status().as_u16();
        if !(200..300).contains(&status) {
            bail!("writing {path}: storage answered {status}");
        }
        Ok(())
    }

    fn list(&self, tpe: FileType) -> Result<Vec<(Id, u32)>> {
        if tpe == FileType::Config {
            let path = format!("{}config", self.prefix);
            return Ok(self
                .service
                .list_storage(&path)?
                .into_iter()
                .filter(|(p, _)| *p == path)
                .map(|(_, size)| (Id::default(), u32::try_from(size).unwrap_or(u32::MAX)))
                .collect());
        }
        let dir = format!("{}{}/", self.prefix, tpe.dirname());
        let mut out = Vec::new();
        for (path, size) in self.service.list_storage(&dir)? {
            let name = path.rsplit('/').next().unwrap_or_default();
            let id: Id = name
                .parse()
                .with_context(|| format!("unexpected object {path}"))?;
            out.push((id, u32::try_from(size).context("object too large")?));
        }
        Ok(out)
    }
}

impl ReadBackend for ServiceStorage {
    fn location(&self) -> String {
        format!("onecloud:{}", self.prefix)
    }

    fn list_with_size(&self, tpe: FileType) -> RusticResult<Vec<(Id, u32)>> {
        self.list(tpe).map_err(|e| backend_error("listing", &e))
    }

    fn read_full(&self, tpe: FileType, id: &Id) -> RusticResult<Bytes> {
        self.get(&self.path(tpe, id), None)
            .map_err(|e| backend_error("reading", &e))
    }

    fn read_partial(
        &self,
        tpe: FileType,
        id: &Id,
        _cacheable: bool,
        offset: u32,
        length: u32,
    ) -> RusticResult<Bytes> {
        self.get(&self.path(tpe, id), Some((offset, length)))
            .map_err(|e| backend_error("reading", &e))
    }

    fn warmup_path(&self, tpe: FileType, id: &Id) -> String {
        self.path(tpe, id)
    }
}

impl WriteBackend for ServiceStorage {
    fn write_bytes(
        &self,
        tpe: FileType,
        id: &Id,
        _cacheable: bool,
        content: BytesList,
    ) -> RusticResult<()> {
        let bytes: Vec<u8> = content.into_vec().concat();
        self.put(&self.path(tpe, id), &bytes)
            .map_err(|e| backend_error("writing", &e))
    }

    fn remove(&self, tpe: FileType, id: &Id, _cacheable: bool) -> RusticResult<()> {
        let path = self.path(tpe, id);
        _ = self.reads.lock().unwrap().remove(&path);
        self.service
            .remove_storage(&[path])
            .map_err(|e| backend_error("removing", &e))
    }
}
