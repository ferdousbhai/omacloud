//! Opening restic repositories through rustic_core.

use std::collections::BTreeMap;

use std::sync::Arc;

use anyhow::{Context, Result};
use ed25519_dalek::SigningKey;
use rustic_backend::BackendOptions;
use rustic_core::{
    ALL_FILE_TYPES, ConfigOptions, Credentials, FileType, Id, IndexedFullStatus, IndexedIdsStatus,
    KeyOptions, Repository, RepositoryBackends, RepositoryOptions,
    repofile::{MasterKey, SnapshotFile, SnapshotId},
};
use serde::{Deserialize, Serialize};

use crate::{http::HttpCoordinator, storage::ServiceStorage};

pub(crate) type Repo = Repository<IndexedFullStatus>;

/// Where a repository lives: a local path; an opendal URL such as
/// `opendal:s3` with its options (endpoint, bucket, keys); or `onecloud:`,
/// storage through the service (options `service` and `account`; see
/// [`crate::storage`]).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RepoSpec {
    pub repository: String,
    #[serde(default)]
    pub options: BTreeMap<String, String>,
    /// The device key that signs requests to the service, for `onecloud:`
    /// repositories. Never stored.
    #[serde(skip)]
    pub signer: Option<SigningKey>,
}

impl PartialEq for RepoSpec {
    fn eq(&self, other: &Self) -> bool {
        self.repository == other.repository && self.options == other.options
    }
}

impl Eq for RepoSpec {}

/// The repository kind for storage through the service.
pub const SERVICE_REPOSITORY: &str = "onecloud:";

/// Backend options that are credentials: kept out of config files, sealed
/// in the epoch secret instead.
pub const CREDENTIAL_OPTIONS: &[&str] = &[
    "access_key_id",
    "secret_access_key",
    "session_token",
    "security_token",
    "application_key_id",
    "application_key",
    "password",
];

/// Backend options that are this device's own business (transfer limits),
/// not the repository's.
pub const LOCAL_OPTIONS: &[&str] = &["bandwidth", "connections"];

impl RepoSpec {
    /// Storage through the service at `url`.
    #[must_use]
    pub fn service(url: &str) -> Self {
        Self {
            repository: SERVICE_REPOSITORY.into(),
            options: [("service".to_string(), url.to_string())].into(),
            signer: None,
        }
    }

    #[must_use]
    pub fn is_service(&self) -> bool {
        self.repository == SERVICE_REPOSITORY
    }

    /// Attach what storage through the service needs: the account and the
    /// key that signs for this device. Other kinds are returned unchanged.
    /// A bucket of the user's own (OpenDAL, such as S3), as opposed to the
    /// service or a local path.
    #[must_use]
    pub fn is_bucket(&self) -> bool {
        self.repository.starts_with("opendal:")
    }

    /// The same location without credentials, for config files and display.
    #[must_use]
    pub fn without_credentials(&self) -> Self {
        let mut spec = self.clone();
        spec.options
            .retain(|k, _| !CREDENTIAL_OPTIONS.contains(&k.as_str()));
        spec
    }

    /// The same location without this device's transfer limits: what is
    /// shared with other devices.
    #[must_use]
    pub fn without_local_options(&self) -> Self {
        let mut spec = self.clone();
        spec.options
            .retain(|k, _| !LOCAL_OPTIONS.contains(&k.as_str()));
        spec
    }

    #[must_use]
    pub fn for_device(mut self, account: &str, signer: &SigningKey) -> Self {
        if self.is_service() {
            _ = self.options.insert("account".into(), account.into());
            self.signer = Some(signer.clone());
        }
        self
    }

    fn backends(&self) -> Result<RepositoryBackends> {
        let backends = self.raw_backends()?;
        // `bandwidth`: this device's cap, whatever the backend
        match self.options.get("bandwidth") {
            Some(rate) => {
                let rate = crate::throttle::parse_rate(rate)?;
                let throttled = crate::throttle::Throttled::new(backends.repository(), rate);
                Ok(RepositoryBackends::new(
                    Arc::new(throttled),
                    backends.repo_hot(),
                ))
            }
            None => Ok(backends),
        }
    }

    fn raw_backends(&self) -> Result<RepositoryBackends> {
        if self.is_service() {
            let service = self.options.get("service").context("no service URL")?;
            let account = self.options.get("account").context("no account")?;
            let signer = self
                .signer
                .clone()
                .context("no device key for the service")?;
            let client = HttpCoordinator::new(service, Some(account.clone()), signer);
            let prefix = self.options.get("root").map_or("", String::as_str);
            let storage = ServiceStorage::new(client, prefix);
            return Ok(RepositoryBackends::new(Arc::new(storage), None));
        }
        let mut options = self.options.clone();
        _ = options.remove("bandwidth");
        if !self.is_bucket() {
            options.retain(|k, _| !LOCAL_OPTIONS.contains(&k.as_str()));
        }
        Ok(BackendOptions::default()
            .repository(&self.repository)
            .options(options)
            .to_backends()?)
    }

    pub fn init(&self, password: &str) -> Result<MasterKey> {
        let repo = Repository::new(&RepositoryOptions::default(), &self.backends()?)?.init(
            &Credentials::password(password),
            &KeyOptions::default(),
            &ConfigOptions::default().set_pack_padding(true),
        )?;
        Ok(repo.key())
    }

    /// Open an existing repository with its password and return the master key.
    ///
    /// # Errors
    ///
    /// If the backend is unreachable or the password is wrong.
    pub fn unlock(&self, password: &str) -> Result<MasterKey> {
        let repo = Repository::new(&RepositoryOptions::default(), &self.backends()?)?
            .open(&Credentials::password(password))?;
        Ok(repo.key())
    }

    /// Where epoch `n` of this repository lives: beside it, so storage
    /// credentials never leave the device. A local path gets a `.e<n>`
    /// suffix; an opendal repository gets its own root inside the bucket.
    #[must_use]
    pub fn for_epoch(&self, n: u64) -> Self {
        if n == 0 {
            return self.clone();
        }
        let mut spec = self.clone();
        if spec.is_service() {
            _ = spec.options.insert("root".into(), format!("e{n}"));
        } else if spec.repository.starts_with("opendal:") {
            let root = spec
                .options
                .get("root")
                .map_or("", |r| r.trim_end_matches('/'));
            let new_root = format!("{root}/onecloud-e{n}");
            _ = spec.options.insert("root".into(), new_root);
        } else {
            spec.repository = format!("{}.e{n}", self.repository);
        }
        spec
    }

    /// Delete every file of the repository it can, then (for a local path)
    /// the directory itself. Returns how many files remain: a bucket with
    /// delete protection (object lock, R2 bucket locks) refuses to delete
    /// files still under retention, and those wait for a later try; the
    /// first refusal ends this one. On
    /// object storage, empty directory markers some servers keep may remain;
    /// they hold nothing.
    ///
    /// # Errors
    ///
    /// If listing fails.
    pub fn destroy(&self) -> Result<usize> {
        // a protected object stays protected: retrying a refused delete only
        // waits (OpenDAL takes R2's 409 for a temporary error and backs off)
        let mut spec = self.clone();
        if spec.repository.starts_with("opendal:") {
            _ = spec.options.insert("retry".into(), "off".into());
        }
        let be = spec.backends()?.repository();
        let mut left = 0;
        for tpe in ALL_FILE_TYPES {
            for id in be.list(tpe)? {
                if left > 0 {
                    left += 1;
                } else if let Err(e) = be.remove(tpe, &id, false) {
                    log::debug!("keeping {tpe:?} {id}: {e}");
                    left = 1;
                }
            }
        }
        // the config file has no content id, so listing doesn't return it;
        // it goes last, so a partly deleted repository still says what it is
        if left == 0 {
            _ = be.remove(FileType::Config, &Id::default(), false);
            let local = self
                .repository
                .strip_prefix("local:")
                .unwrap_or(&self.repository);
            if !local.contains(':') && std::path::Path::new(local).is_dir() {
                std::fs::remove_dir_all(local)?;
            }
        }
        Ok(left)
    }

    /// Copy every file of this repository, as stored, to `dest`, so it
    /// opens there with the same password. How a managed tier user leaves
    /// with their data. Returns the number of files copied.
    ///
    /// # Errors
    ///
    /// If `dest` already holds a repository, or reading or writing fails.
    pub fn copy_files_to(&self, dest: &Self) -> Result<usize> {
        let (from, to) = (self.backends()?.repository(), dest.backends()?.repository());
        anyhow::ensure!(
            to.list(FileType::Config)?.is_empty(),
            "{} already holds a repository",
            dest.repository
        );
        to.create()?;
        let mut n = 0;
        // config last: until it exists, the copy isn't a repository yet
        for tpe in [
            FileType::Key,
            FileType::Pack,
            FileType::Index,
            FileType::Snapshot,
            FileType::Config,
        ] {
            for id in from.list(tpe)? {
                let bytes = from.read_full(tpe, &id)?;
                to.write_bytes(tpe, &id, false, bytes.into())?;
                n += 1;
            }
        }
        Ok(n)
    }

    pub(crate) fn open_ids(&self, key: &MasterKey) -> Result<Repository<IndexedIdsStatus>> {
        Ok(
            Repository::new(&RepositoryOptions::default(), &self.backends()?)?
                .open(&Credentials::Masterkey(key.clone()))?
                .to_indexed_ids()?,
        )
    }

    /// Open with the master key (no key derivation) and load the full index.
    pub(crate) fn open(&self, key: &MasterKey) -> Result<Repo> {
        Ok(
            Repository::new(&RepositoryOptions::default(), &self.backends()?)?
                .open(&Credentials::Masterkey(key.clone()))?
                .to_indexed()?,
        )
    }
}

/// Copy snapshots (full hex ids) from `src` into `dest`, re-encrypting with
/// `dest`'s key; snapshots already there are not copied again. Returns each
/// source id with its id in `dest`.
pub(crate) fn copy_snapshots(
    src: &Repo,
    dest: &RepoSpec,
    dest_key: &MasterKey,
    ids: &[String],
) -> Result<BTreeMap<String, String>> {
    // a copy is the same snapshot under a new id: match on what it records
    let ident = |s: &SnapshotFile| (hex(&s.tree), s.time.timestamp(), s.hostname.clone());
    let wanted = src.get_snapshots(ids)?;
    let present: BTreeMap<_, _> = dest
        .open(dest_key)?
        .get_all_snapshots()?
        .into_iter()
        .map(|s| (ident(&s), snapshot_hex(&s.id)))
        .collect();
    let missing: Vec<_> = wanted
        .iter()
        .filter(|s| !present.contains_key(&ident(s)))
        .collect();
    if !missing.is_empty() {
        src.copy(&dest.open_ids(dest_key)?, missing)?;
    }
    let now: BTreeMap<_, _> = dest
        .open(dest_key)?
        .get_all_snapshots()?
        .into_iter()
        .map(|s| (ident(&s), snapshot_hex(&s.id)))
        .collect();
    wanted
        .iter()
        .map(|s| {
            let to = now
                .get(&ident(s))
                .with_context(|| format!("snapshot {} missing after copy", snapshot_hex(&s.id)))?;
            Ok((snapshot_hex(&s.id), to.clone()))
        })
        .collect()
}

/// Full length hex of an id; rustic's `Display` may abbreviate.
pub(crate) fn hex(id: &Id) -> String {
    id.to_hex().as_str().to_string()
}

pub(crate) fn snapshot_hex(id: &SnapshotId) -> String {
    hex(id)
}
