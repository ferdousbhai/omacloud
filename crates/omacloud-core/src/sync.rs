//! Live sync of one folder through a restic repository.
//!
//! A device keeps `base`: the head its folder matched after its last sync.
//! One cycle:
//!   1. Verify the coordinator's head (see [`crate::head`]).
//!   2. If the head moved past `base`, diff the two snapshot trees by tree id
//!      (equal subtrees are skipped) and apply the remote changes to disk.
//!      Paths changed on both sides keep both versions: the older one becomes
//!      `name.sync-conflict-<date>-<time>-<device>.ext`; a change beats a
//!      delete. Then `base` moves to the head.
//!   3. Local changes are the candidate paths (from the watcher, or a full
//!      rescan) whose size, mtime or link target differ from `base`. Files a
//!      pull wrote carry the snapshot's mtime, so they aren't echoed back.
//!   4. Push them with `splice_tree` onto `base`, sign the next head and append
//!      it. If another device appended first, go around again; blobs already
//!      uploaded are reused.
//!
//! Regular files, directories and symlinks are synced. Empty directories are
//! not kept.

use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, HashMap},
    fs::{self, File},
    io::{self, ErrorKind, Read},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, Result, anyhow, bail};
use ed25519_dalek::SigningKey;
use log::{debug, info, trace, warn};
use rustic_core::{
    CheckOptions, Id, SnapshotOptions, TreeEdit, TreeId,
    jiff::{Timestamp, tz::TimeZone},
    repofile::{Metadata, Node, NodeType},
};
use serde::{Deserialize, Serialize};

use crate::{
    devices::{Action, DeviceChain, JoinRequest, public_hex, random_secret},
    epoch::{EpochChain, EpochError, EpochRecord, EpochSecret, Grant},
    head::{Coordinator, Head, HeadTracker, write_private},
    ignore::{Ignores, TEMP_PREFIX},
    repo::{Repo, RepoSpec, copy_snapshots, hex, snapshot_hex},
    settings::{self, Manifest, PREFIX as SETTINGS},
};

/// Go's `os.ModeDir`, as restic stores directory modes.
const GO_MODE_DIR: u32 = 0x8000_0000;
/// Bound on re-pushing files that changed while being uploaded, per sync.
const MAX_ROUNDS: usize = 8;
/// A file modified this recently may still be rewritten within the same
/// timestamp tick, invisibly to a size and mtime check.
const RACY_WINDOW: Duration = Duration::from_secs(2);
/// Racy files larger than this are pushed again instead of compared.
const MAX_COMPARE: u64 = 64 << 20;
/// Where a file goes in a pull's queue: small before big, then by folder,
/// then newest first.
type PullOrder = (bool, u8, std::cmp::Reverse<Option<Timestamp>>);

/// Files larger than this come after all the smaller ones in a pull.
const BIG_FILE: u64 = 1 << 20;
/// Concurrent fetches during a pull; they wait on the network, not the CPU.
const PULL_THREADS: usize = 32;

/// What a device needs to sync one folder.
#[derive(Debug, Clone)]
pub struct Setup {
    /// This device's name, recorded in snapshots and conflict copies.
    pub device: String,
    /// The folder to keep in sync.
    pub folder: PathBuf,
    /// The account's first repository; later epochs live beside it.
    pub repo: RepoSpec,
    /// This device's signing key.
    pub signing: SigningKey,
    /// The account root public key (hex) this device pins.
    pub root: String,
    /// Where to keep sync state between runs; `None` keeps it in memory.
    pub state_path: Option<PathBuf>,
    /// Settings sync on this device, if on.
    pub settings: Option<SettingsSetup>,
    /// Sync chosen folders of home (`folder` is then home), as iCloud syncs
    /// Desktop and Documents; `None` syncs all of `folder`.
    pub folders: Option<Folders>,
}

/// Which top-level folders of home sync. The account's list is every folder
/// in its history plus the ones added here; a device can skip some.
#[derive(Debug, Clone, Default)]
pub struct Folders {
    /// Folders this device adds to the account, like `Documents`.
    pub add: BTreeSet<String>,
    /// Folders this device doesn't sync.
    pub skip: BTreeSet<String>,
    /// What a folder is called here, where that differs from its name in
    /// the account: the account says `Documents`, a German machine keeps it
    /// in `Dokumente` (from its XDG user dirs).
    pub local_names: BTreeMap<String, String>,
}

/// Settings sync for a device (see [`crate::settings`]).
#[derive(Debug, Clone)]
pub struct SettingsSetup {
    /// The home directory the manifest's paths are relative to.
    pub home: PathBuf,
    pub manifest: Manifest,
    /// Where earlier versions of settings go before a pull replaces them,
    /// and where held remote versions wait for `resolve`.
    pub backups: PathBuf,
    /// Where each device's package list lives locally (see
    /// [`Engine::update_package_list`]).
    pub packages: PathBuf,
    /// Where each device's sealed secrets bundle lives locally (see
    /// [`crate::secrets`]).
    pub secrets: PathBuf,
}

/// Where package lists live in the synced tree, one file per device.
pub const PACKAGES: &str = ".omacloud/packages";

/// Where sealed secrets bundles live in the synced tree, one per device.
pub const SECRETS: &str = ".omacloud/secrets";

/// Settings sync on this device, as `omacloud settings` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsStatus {
    pub on: bool,
    /// Why it stands down, if it does.
    pub dormant: Option<String>,
    /// Settings changed on both sides that didn't merge, waiting for
    /// `resolve`: path relative to home.
    pub held: Vec<PathBuf>,
}

/// The outcome of [`Engine::rotate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rotated {
    pub epoch: u64,
    /// Snapshots copied into the new repository.
    pub snapshots: usize,
    /// Files of the old repository that delete protection kept; deleting
    /// them is retried about daily.
    pub left: usize,
}

/// Why this device may not sync.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Membership {
    #[error("this device is not approved yet: approve it from another device")]
    NotApproved,
    #[error("this device was removed from the account")]
    Revoked,
}

/// The head the folder matched after the last sync.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Base {
    pub seq: u64,
    pub snapshot: String,
    pub tree: String,
}

/// What a device persists between runs.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct State {
    pub base: Option<Base>,
    pub heads: HeadTracker,
    /// The verified device chain, pinned to the account root.
    pub devices: DeviceChain,
    /// The verified epoch records (repository keys and rotations).
    #[serde(default)]
    pub epochs: EpochChain,
    /// This device's key for the epoch it syncs in.
    #[serde(default)]
    pub secret: Option<(u64, EpochSecret)>,
    /// Settings changed on both sides that didn't merge: tree path to the
    /// parked remote version. Held back from pushing until resolved.
    #[serde(default)]
    pub held: BTreeMap<PathBuf, PathBuf>,
    /// Paths whose size and mtime can't be trusted to show a change: file
    /// timestamps are coarse, so another write within the same tick can
    /// leave both unchanged (git's "racy" index entries). These are compared
    /// by content until their mtime is safely in the past.
    #[serde(default)]
    pub racy: BTreeSet<PathBuf>,
    /// Old repository epochs not fully deleted after a rotation: delete
    /// protection on the bucket holds their files until retention ends.
    /// Deletion is retried about daily.
    #[serde(default)]
    pub leftover: BTreeSet<u64>,
    /// When deleting leftovers was last tried, in seconds since the epoch.
    #[serde(default)]
    pub cleaned_at: u64,
    /// When this device last finished a sync with nothing left to push, in
    /// seconds since the epoch.
    #[serde(default)]
    pub synced_at: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// Paths pushed (puts and deletes).
    pub pushed: usize,
    /// Remote changes applied or merged.
    pub pulled: usize,
    /// Paths changed on both sides; each left a conflict copy.
    pub conflicts: usize,
    /// Pushes lost to another device and retried.
    pub retries: usize,
}

impl std::ops::AddAssign for Stats {
    fn add_assign(&mut self, o: Self) {
        self.pushed += o.pushed;
        self.pulled += o.pulled;
        self.conflicts += o.conflicts;
        self.retries += o.retries;
    }
}

/// One version of a path, see [`Engine::versions`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub seq: u64,
    pub time: Timestamp,
    pub device: String,
    /// `None` where the version is a deletion.
    pub size: Option<u64>,
}

/// A local change found against `base`.
#[derive(Debug)]
enum Local {
    Put {
        node: Box<Node>,
        source: Option<PathBuf>,
    },
    Delete,
}

pub struct Engine {
    device: String,
    folder: PathBuf,
    repo: RepoSpec,
    signing: SigningKey,
    coord: Arc<dyn Coordinator>,
    state: State,
    state_path: Option<PathBuf>,
    pending: BTreeSet<PathBuf>,
    rescan: bool,
    ignore_rules: Vec<String>,
    ignores: Ignores,
    /// Conflict copy names claimed during the current pull.
    reserved: BTreeSet<PathBuf>,
    settings: Option<SettingsSetup>,
    /// Why settings sync stands down right now, if it does.
    dormant: Option<String>,
    folders: Option<Folders>,
    /// The folders syncing here, when syncing chosen folders of home:
    /// the account's, less the skipped ones.
    synced: BTreeSet<String>,
}

impl Engine {
    /// Set up a device. State is loaded from `setup.state_path` when it
    /// exists and saved there after every sync.
    ///
    /// # Errors
    ///
    /// If the state file exists but can't be read, or was made for a
    /// different account root.
    pub fn new(setup: Setup, coord: Arc<dyn Coordinator>) -> Result<Self> {
        let Setup {
            device,
            folder,
            repo,
            signing,
            root,
            state_path,
            settings,
            folders,
        } = setup;
        let root = root.as_str();
        let state: State = match state_path.as_deref().map(fs::read) {
            Some(Ok(bytes)) => serde_json::from_slice(&bytes).context("reading sync state")?,
            Some(Err(e)) if e.kind() != ErrorKind::NotFound => return Err(e.into()),
            _ => State {
                devices: DeviceChain::new(root),
                ..State::default()
            },
        };
        anyhow::ensure!(
            state.devices.root == root,
            "sync state belongs to another account root"
        );
        Ok(Self {
            device,
            folder,
            repo,
            signing,
            coord,
            state,
            state_path,
            pending: BTreeSet::new(),
            rescan: true,
            ignore_rules: Vec::new(),
            ignores: Ignores::defaults(),
            reserved: BTreeSet::new(),
            settings,
            dormant: None,
            synced: folders
                .as_ref()
                .map(|f| f.add.difference(&f.skip).cloned().collect())
                .unwrap_or_default(),
            folders,
        })
    }

    /// Publish a new bucket key for a self-hosted account, sealed to every
    /// member and the root; returns its number. The old key must still work:
    /// the record is stored with it.
    ///
    /// # Errors
    ///
    /// If this device isn't a member, or another change landed at the same
    /// time.
    pub fn change_bucket_key(&mut self, creds: &crate::bucket_key::Credentials) -> Result<u64> {
        self.refresh_devices()?;
        let seq = self.coord.key_records()?.last().map_or(0, |r| r.seq) + 1;
        let record =
            crate::bucket_key::KeyRecord::new(&self.signing, seq, &self.state.devices, creds)?;
        anyhow::ensure!(
            self.coord.append_key_record(&record)?,
            "another computer changed the bucket key at the same time; look again"
        );
        self.coord.ack_key(seq, &self.device_key())?;
        Ok(seq)
    }

    /// The latest bucket key change, checked against the device chain and
    /// opened for this device: `(number, key)`. No key when this device
    /// joined after the change: it came with a key at least as new.
    ///
    /// # Errors
    ///
    /// If a record doesn't verify, or wasn't sealed to this device although
    /// it was a member then (it was removed before the change).
    #[allow(clippy::type_complexity)]
    pub fn latest_bucket_key(
        &mut self,
    ) -> Result<Option<(u64, Option<crate::bucket_key::Credentials>)>> {
        let Some(record) = self.coord.key_records()?.pop() else {
            return Ok(None);
        };
        self.state.devices.advance(self.coord.as_ref())?;
        record.verify(&self.state.devices)?;
        let me = self.device_key();
        let then = self.state.devices.members_at(record.devices);
        if !record.keys.contains_key(&me)
            && !then.valid.contains_key(&me)
            && !then.revoked.contains_key(&me)
        {
            return Ok(Some((record.seq, None)));
        }
        Ok(Some((record.seq, Some(record.open(&self.signing)?))))
    }

    /// Say this device switched to key change `seq`.
    ///
    /// # Errors
    ///
    /// If the acknowledgment can't be stored.
    pub fn ack_bucket_key(&self, seq: u64) -> Result<()> {
        self.coord.ack_key(seq, &self.device_key())
    }

    /// The latest key change, and which members switched to it.
    ///
    /// # Errors
    ///
    /// On coordinator errors.
    pub fn bucket_key_status(&mut self) -> Result<Option<(u64, Vec<String>)>> {
        let Some(record) = self.coord.key_records()?.pop() else {
            return Ok(None);
        };
        Ok(Some((record.seq, self.coord.key_acks(record.seq)?)))
    }

    /// Change which folders sync here (after `omacloud folders` edited the
    /// config); takes effect on the next sync.
    pub fn set_folders(&mut self, add: BTreeSet<String>, skip: BTreeSet<String>) {
        if let Some(f) = self.folders.as_mut() {
            f.add = add;
            f.skip = skip;
            // dropped folders go now; new ones come with the next refresh
            self.synced.retain(|n| !f.skip.contains(n));
        }
    }

    /// The folders of home syncing on this device, when syncing chosen
    /// folders; a watcher watches each.
    #[must_use]
    pub fn synced_folders(&self) -> Vec<PathBuf> {
        self.synced
            .iter()
            .map(|n| self.on_disk(Path::new(n)))
            .collect()
    }

    /// The folders syncing here: account name, and where it is here.
    #[must_use]
    pub fn folder_list(&self) -> Vec<(String, PathBuf)> {
        self.synced
            .iter()
            .map(|n| (n.clone(), self.on_disk(Path::new(n))))
            .collect()
    }

    /// Read the account's folders from the last synced version, without
    /// syncing; [`Engine::synced_folders`] then lists them all.
    ///
    /// # Errors
    ///
    /// If the repository can't be opened.
    pub fn load_folders(&mut self) -> Result<()> {
        if self.folders.is_none() || self.state.base.is_none() {
            return Ok(());
        }
        let repo = self.open_repo()?;
        let base = self.base_tree()?;
        self.refresh_synced(&repo, base)
    }

    /// Every folder in the account (in `tree`) and added here, less the
    /// skipped ones.
    fn refresh_synced(&mut self, repo: &Repo, tree: Option<TreeId>) -> Result<()> {
        let Some(f) = &self.folders else {
            return Ok(());
        };
        let mut names: BTreeSet<String> = f.add.clone();
        for (name, node) in nodes(repo, tree)? {
            if node.is_dir() && !name.starts_with('.') {
                _ = names.insert(name);
            }
        }
        for name in &f.skip {
            _ = names.remove(name);
        }
        if names != self.synced {
            for new in names.difference(&self.synced) {
                info!("syncing folder {new}");
                // pick up what's already there
                _ = self.pending.insert(PathBuf::from(new));
            }
            self.synced = names;
        }
        Ok(())
    }

    /// Where a path of the synced folder lives on disk: under the folder,
    /// with a synced folder's account name turned into its name here.
    fn on_disk(&self, path: &Path) -> PathBuf {
        let mut parts = path.components();
        let local = self.folders.as_ref().and_then(|f| {
            let first = parts.next()?.as_os_str().to_str()?;
            f.local_names.get(first)
        });
        match local {
            Some(name) => self.folder.join(name).join(parts.as_path()),
            None => self.folder.join(path),
        }
    }

    /// The synced path for a path on disk under the folder, if it is one:
    /// the reverse of [`Self::on_disk`].
    fn synced_path(&self, rel: &Path) -> Option<PathBuf> {
        let Some(f) = &self.folders else {
            return Some(rel.to_path_buf());
        };
        let mut parts = rel.components();
        let first = parts.next()?.as_os_str().to_str()?;
        let name = f
            .local_names
            .iter()
            .find(|(_, local)| *local == first)
            .map_or(first, |(name, _)| name.as_str());
        self.synced
            .contains(name)
            .then(|| Path::new(name).join(parts.as_path()))
    }

    /// A synced folder itself (`Documents`), as opposed to what's in it.
    fn is_folder_root(&self, path: &Path) -> bool {
        self.folders.is_some() && path.components().count() == 1 && !path.starts_with(".omacloud")
    }

    /// Extra ignore rules (gitignore syntax), on top of the defaults and the
    /// folder's `.omacloudignore`.
    pub fn set_ignore_rules(&mut self, rules: Vec<String>) {
        self.ignore_rules = rules;
    }

    /// This device's public key (hex).
    #[must_use]
    pub fn device_key(&self) -> String {
        public_hex(&self.signing)
    }

    /// Fetch and verify the device chain.
    ///
    /// # Errors
    ///
    /// A [`crate::devices::DeviceError`] if the coordinator misbehaves.
    pub fn refresh_devices(&mut self) -> Result<&DeviceChain> {
        self.state.devices.advance(self.coord.as_ref())?;
        self.save_state()?;
        Ok(&self.state.devices)
    }

    /// The coordinator's change marker, if it keeps one (see
    /// [`Coordinator::marker`]).
    ///
    /// # Errors
    ///
    /// If the coordinator can't be reached.
    pub fn remote_marker(&self) -> Result<Option<String>> {
        self.coord.marker()
    }

    /// Local changes noticed and not pushed yet.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Join requests waiting for approval.
    ///
    /// # Errors
    ///
    /// If the coordinator can't be reached.
    pub fn requests(&self) -> Result<Vec<JoinRequest>> {
        self.coord.requests()
    }

    /// Add a device to the account, signed by this device. Compare the
    /// device's fingerprint with the one it shows before approving.
    ///
    /// # Errors
    ///
    /// If this device isn't a member, or the entry can't be appended.
    pub fn approve(&mut self, req: &JoinRequest) -> Result<()> {
        self.refresh_devices()?;
        self.check_membership()?;
        anyhow::ensure!(
            req.signed(),
            "the join request is not signed by the device it names"
        );
        // the joining device pins whatever root the coordinator showed it; a
        // different root here means it was shown someone else's account
        anyhow::ensure!(
            req.root == self.state.devices.root,
            "the joining device was shown a different account (root {}); don't approve it",
            crate::devices::fingerprint(&req.root)
        );
        self.ensure_key()?;
        if !self.state.devices.members().valid.contains_key(&req.device) {
            let action = Action::Add {
                device: req.device.clone(),
                name: req.name.clone(),
            };
            self.state
                .devices
                .append(self.coord.as_ref(), &self.signing, action)?;
        }
        let (epoch, secret) = self.secret()?;
        let grant = Grant::new(&self.signing, epoch, &req.device, &secret)?;
        self.coord.put_grant(&grant)?;
        self.coord.remove_request(&req.device)?;
        self.save_state()
    }

    fn secret(&self) -> Result<(u64, EpochSecret)> {
        self.state
            .secret
            .clone()
            .ok_or_else(|| EpochError::NoKey { epoch: 0 }.into())
    }

    /// The repository this device currently syncs with: location, secret
    /// and epoch.
    ///
    /// # Errors
    ///
    /// If this device has no repository key yet.
    pub fn repository(&self) -> Result<(RepoSpec, EpochSecret, u64)> {
        let (epoch, secret) = self.secret()?;
        let spec = self.base_spec(&secret);
        anyhow::ensure!(
            !spec.repository.is_empty(),
            "this account's key doesn't say where its repository is; set this device up with --repo"
        );
        Ok((spec.for_epoch(epoch), secret, epoch))
    }

    /// Where the repository lives: the bucket sealed in the epoch secret,
    /// or the location this device was set up with. This device's transfer
    /// limits apply either way.
    fn base_spec(&self, secret: &EpochSecret) -> RepoSpec {
        let Some(storage) = &secret.storage else {
            return self.repo.clone();
        };
        let mut spec = storage.clone();
        // a self-hosted device keeps the bucket key in its own config, and a
        // key change updates it there
        for k in crate::repo::CREDENTIAL_OPTIONS {
            if let Some(v) = self.repo.options.get(*k) {
                _ = spec.options.insert((*k).to_string(), v.clone());
            }
        }
        for k in crate::repo::LOCAL_OPTIONS {
            if let Some(v) = self.repo.options.get(*k) {
                _ = spec.options.insert((*k).to_string(), v.clone());
            }
        }
        spec
    }

    fn open_repo(&self) -> Result<Repo> {
        let (spec, secret, _) = self.repository()?;
        spec.open(&secret.key)
    }

    /// Verify the epoch records and hold the latest epoch's key. On moving
    /// to a new epoch, the base snapshot is carried over by the record's map.
    fn ensure_key(&mut self) -> Result<()> {
        self.state
            .epochs
            .advance(self.coord.as_ref(), &self.state.devices)?;
        let latest = self.state.epochs.current();
        if latest.is_some() && self.state.secret.as_ref().map(|s| s.0) == latest {
            return Ok(());
        }
        let (epoch, secret) =
            self.state
                .epochs
                .secret(self.coord.as_ref(), &self.state.devices, &self.signing)?;
        let from = self.state.secret.as_ref().map_or(0, |s| s.0);
        if let Some(base) = self.state.base.as_mut()
            && let Some(id) = self.state.epochs.map(&base.snapshot, from, epoch)
        {
            base.snapshot = id;
        }
        if from != epoch {
            info!("now syncing in repository epoch {epoch}");
        }
        self.state.secret = Some((epoch, secret));
        self.save_state()
    }

    /// Remove a device from the account. Its heads from before stay valid;
    /// it can't sign new ones. (It keeps the repository key it already has:
    /// rotating that is separate.)
    ///
    /// # Errors
    ///
    /// If this device isn't a member, `device` isn't one, or the entry can't
    /// be appended.
    pub fn revoke(&mut self, device: &str) -> Result<()> {
        self.refresh_devices()?;
        let action = Action::Revoke {
            device: device.to_string(),
        };
        self.state
            .devices
            .append(self.coord.as_ref(), &self.signing, action)?;
        self.save_state()
    }

    /// Move to a new repository with a new key, so devices removed so far
    /// lose access: copy every snapshot across, start the new epoch with a
    /// head, publish the key sealed to each member and the root, check the
    /// new repository, then delete the old one.
    ///
    /// Everything is uploaded again, so this takes as long as a first sync.
    ///
    /// # Errors
    ///
    /// If this device isn't a member, another device rotated at the same
    /// time, or any step fails. The old repository is only deleted once the
    /// new one is in place and checked.
    pub fn rotate(&mut self) -> Result<Rotated> {
        self.sync()?; // up to date, a member, holding the current key
        let (old_epoch, old_secret) = self.secret()?;
        let epoch = old_epoch + 1;
        let base = self.base_spec(&old_secret);
        let old_spec = base.for_epoch(old_epoch);
        let new_spec = base.for_epoch(epoch);
        let password = random_secret();
        let secret = EpochSecret {
            key: new_spec
                .init(&password)
                .context("creating the next epoch's repository")?,
            password,
            storage: old_secret.storage.clone(),
        };

        // copy, then claim the next head; if another device pushed in the
        // meantime, copy what it added and try again
        let mut map = BTreeMap::new();
        let record = loop {
            self.state.devices.advance(self.coord.as_ref())?;
            let head = self
                .state
                .heads
                .advance(self.coord.as_ref(), &self.state.devices)?
                .context("nothing synced yet: there is nothing to rotate")?;
            anyhow::ensure!(
                head.epoch == old_epoch,
                "another device rotated the key at the same time"
            );
            let ids: Vec<String> = self
                .coord
                .since(0)?
                .iter()
                .filter_map(|h| self.state.epochs.map(&h.snapshot, h.epoch, old_epoch))
                .filter(|id| !map.contains_key(id))
                .collect();
            let src = old_spec.open(&old_secret.key)?;
            map.extend(copy_snapshots(&src, &new_spec, &secret.key, &ids)?);
            let current = self
                .state
                .epochs
                .map(&head.snapshot, head.epoch, old_epoch)
                .and_then(|id| map.get(&id).cloned())
                .context("latest snapshot missing from the copy")?;
            let next = Head::next_in(
                &self.signing,
                Some(&head),
                &current,
                self.state.devices.len(),
                epoch,
            );
            let record = EpochRecord::next(
                &self.signing,
                self.state.epochs.records.last(),
                next.seq,
                &self.state.devices,
                &secret,
                map.clone(),
            )?;
            if self.coord.append_rotation(&next, &record)? {
                self.state.heads.accept_own(&next, &self.state.devices)?;
                break record;
            }
        };

        self.state.epochs.push(record, &self.state.devices)?;
        if let Some(base) = self.state.base.as_mut()
            && let Some(id) = map.get(&base.snapshot)
        {
            base.snapshot = id.clone();
        }
        self.state.secret = Some((epoch, secret.clone()));
        self.save_state()?;

        // the old repository goes only once the new one checks out
        new_spec
            .open(&secret.key)?
            .check(CheckOptions::default())?
            .is_ok()?;
        let left = old_spec.destroy()?;
        if left > 0 {
            warn!(
                "{left} files of repository epoch {old_epoch} are under delete protection; \
                 deleting them is retried later"
            );
            _ = self.state.leftover.insert(old_epoch);
            self.state.cleaned_at = unix_now();
            self.save_state()?;
        }
        info!(
            "rotated to repository epoch {epoch}: {} snapshots",
            map.len()
        );
        Ok(Rotated {
            epoch,
            snapshots: map.len(),
            left,
        })
    }

    /// Try again to delete old epochs that delete protection held back,
    /// about once a day. Best effort: failures only log.
    fn clean_leftovers(&mut self) {
        const EVERY: u64 = 24 * 60 * 60;
        let now = unix_now();
        if self.state.leftover.is_empty() || now.saturating_sub(self.state.cleaned_at) < EVERY {
            return;
        }
        self.state.cleaned_at = now;
        for epoch in std::mem::take(&mut self.state.leftover) {
            let Ok((_, secret)) = self.secret() else {
                return;
            };
            match self.base_spec(&secret).for_epoch(epoch).destroy() {
                Ok(0) => info!("deleted leftover repository epoch {epoch}"),
                Ok(left) => {
                    debug!("{left} files of repository epoch {epoch} still protected");
                    _ = self.state.leftover.insert(epoch);
                }
                Err(e) => {
                    warn!("deleting leftover repository epoch {epoch}: {e:#}");
                    _ = self.state.leftover.insert(epoch);
                }
            }
        }
        if let Err(e) = self.save_state() {
            warn!("saving sync state: {e:#}");
        }
    }

    fn check_membership(&self) -> Result<()> {
        let me = self.device_key();
        let members = self.state.devices.members();
        if members.valid.contains_key(&me) {
            return Ok(());
        }
        if members.revoked.contains_key(&me) {
            bail!(Membership::Revoked);
        }
        bail!(Membership::NotApproved)
    }

    #[must_use]
    pub fn state(&self) -> &State {
        &self.state
    }

    #[must_use]
    pub fn folder(&self) -> &Path {
        &self.folder
    }

    fn settings_on(&self) -> Option<&SettingsSetup> {
        self.settings.as_ref().filter(|_| self.dormant.is_none())
    }

    /// Where a tree path lives on disk: settings under home, everything else
    /// in the folder. `None` for settings while settings sync is off.
    fn local(&self, path: &Path) -> Option<PathBuf> {
        if let Ok(name) = path.strip_prefix(PACKAGES) {
            return self.settings.as_ref().map(|s| s.packages.join(name));
        }
        if let Ok(name) = path.strip_prefix(SECRETS) {
            return self.settings.as_ref().map(|s| s.secrets.join(name));
        }
        match path.strip_prefix(SETTINGS) {
            Ok(rest) => self.settings_on().map(|s| s.home.join(rest)),
            Err(_) => Some(self.on_disk(path)),
        }
    }

    /// Whether a tree path is out of sync's reach: ignored in the folder,
    /// reserved, or a setting outside the manifest's shared tier.
    fn excluded(&self, path: &Path, is_dir: bool) -> bool {
        // package lists: one file per device, whatever the dotfiles do
        if let Ok(name) = path.strip_prefix(PACKAGES) {
            return self.settings.is_none()
                || is_dir
                || name.components().count() != 1
                || name.extension().is_none_or(|e| e != "txt");
        }
        // sealed secrets bundles, likewise
        if let Ok(name) = path.strip_prefix(SECRETS) {
            return self.settings.is_none()
                || is_dir
                || name.components().count() != 1
                || name.extension().is_none_or(|e| e != "sealed");
        }
        match path.strip_prefix(SETTINGS) {
            // settings are files the manifest shares; directories under
            // home are never managed
            Ok(rest) => self
                .settings_on()
                .is_none_or(|s| is_dir || !s.manifest.is_shared(rest)),
            Err(_) => {
                path.starts_with(".omacloud")
                    || (self.folders.is_some()
                        && path.components().next().is_none_or(|c| {
                            !self.synced.contains(&*c.as_os_str().to_string_lossy())
                        }))
                    || self.ignores.is_ignored(path, is_dir)
            }
        }
    }

    /// Record this device's package list (see `omacloud packages`), if it
    /// changed. It syncs with the next sync.
    ///
    /// # Errors
    ///
    /// If settings sync is off, or the file can't be written.
    pub fn update_package_list(&mut self, list: &str) -> Result<bool> {
        let s = self
            .settings
            .as_ref()
            .ok_or_else(|| anyhow!("settings sync is off"))?;
        let name = format!("{}.txt", self.device);
        let dest = s.packages.join(&name);
        if fs::read_to_string(&dest).is_ok_and(|old| old == list) {
            return Ok(false);
        }
        fs::create_dir_all(&s.packages)?;
        fs::write(&dest, list)?;
        _ = self.pending.insert(Path::new(PACKAGES).join(name));
        Ok(true)
    }

    /// Store this device's sealed secrets bundle; it syncs with the next
    /// sync, replacing the one saved before.
    ///
    /// # Errors
    ///
    /// If settings sync is off, or the file can't be written.
    pub fn put_secrets(&mut self, sealed: &[u8]) -> Result<()> {
        let s = self
            .settings
            .as_ref()
            .ok_or_else(|| anyhow!("settings sync is off"))?;
        let name = format!("{}.sealed", self.device);
        fs::create_dir_all(&s.secrets)?;
        let tmp = s.secrets.join(format!(".{name}.tmp"));
        fs::write(&tmp, sealed)?;
        fs::rename(&tmp, s.secrets.join(&name))?;
        _ = self.pending.insert(Path::new(SECRETS).join(name));
        Ok(())
    }

    /// The devices with a secrets bundle, as of the last sync.
    #[must_use]
    pub fn secrets_devices(&self) -> Vec<String> {
        let Some(s) = self.settings.as_ref() else {
            return Vec::new();
        };
        let mut out: Vec<String> = fs::read_dir(&s.secrets)
            .map(|it| {
                it.flatten()
                    .filter_map(|e| {
                        let name = e.file_name().to_string_lossy().into_owned();
                        name.strip_suffix(".sealed")
                            .filter(|n| !n.starts_with('.'))
                            .map(str::to_string)
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.sort();
        out
    }

    /// `device`'s sealed secrets bundle, as of the last sync.
    ///
    /// # Errors
    ///
    /// If settings sync is off or there is no bundle from `device`.
    pub fn secrets_of(&self, device: &str) -> Result<Vec<u8>> {
        let s = self
            .settings
            .as_ref()
            .ok_or_else(|| anyhow!("settings sync is off"))?;
        fs::read(s.secrets.join(format!("{device}.sealed")))
            .with_context(|| format!("no secrets bundle from {device}"))
    }

    /// Settings sync on this device.
    #[must_use]
    pub fn settings_status(&self) -> SettingsStatus {
        let dormant = self
            .settings
            .as_ref()
            .and_then(|s| s.manifest.dormant(&s.home));
        SettingsStatus {
            on: self.settings.is_some(),
            dormant,
            held: self
                .state
                .held
                .keys()
                .filter_map(|p| p.strip_prefix(SETTINGS).ok().map(Path::to_path_buf))
                .collect(),
        }
    }

    /// Directories a watcher should watch (not recursively) for settings.
    #[must_use]
    pub fn settings_watch_dirs(&self) -> Vec<PathBuf> {
        self.settings_on()
            .map(|s| s.manifest.watch_dirs(&s.home))
            .unwrap_or_default()
    }

    /// Settle a setting that changed on both sides: keep this device's
    /// version, or take the other one. `path` is relative to home.
    ///
    /// # Errors
    ///
    /// If the setting isn't held, or files can't be written.
    pub fn resolve_setting(&mut self, path: &Path, keep_remote: bool) -> Result<()> {
        let tree_path = Path::new(SETTINGS).join(path);
        let parked = self
            .state
            .held
            .remove(&tree_path)
            .ok_or_else(|| anyhow!("{} is not waiting to be resolved", path.display()))?;
        if keep_remote {
            let dest = self
                .local(&tree_path)
                .ok_or_else(|| anyhow!("settings sync is off"))?;
            self.back_up(&tree_path)?;
            fs::copy(&parked, &dest)?;
        }
        _ = fs::remove_file(&parked);
        _ = self.pending.insert(tree_path);
        self.save_state()
    }

    /// Copy a setting aside before a pull replaces or deletes it.
    fn back_up(&self, path: &Path) -> Result<()> {
        let (Some(s), Ok(rest)) = (self.settings_on(), path.strip_prefix(SETTINGS)) else {
            return Ok(());
        };
        let src = s.home.join(rest);
        if !src.is_file() {
            return Ok(());
        }
        let stamp = Timestamp::now()
            .to_zoned(TimeZone::system())
            .strftime("%Y%m%d-%H%M%S")
            .to_string();
        let dest = s.backups.join("history").join(stamp).join(rest);
        fs::create_dir_all(dest.parent().ok_or_else(|| anyhow!("bad path"))?)?;
        fs::copy(&src, &dest)?;
        Ok(())
    }

    /// Mark paths as possibly changed: absolute under the folder or (for
    /// settings) under home, or relative to the folder.
    pub fn notice(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        for p in paths {
            // (home can be both the folder and where settings live)
            let rel = if let Some(rel) = p
                .strip_prefix(&self.folder)
                .ok()
                .and_then(|rel| self.synced_path(rel))
            {
                rel
            } else if let Some(rest) = self
                .settings_on()
                .and_then(|s| p.strip_prefix(&s.home).ok())
            {
                Path::new(SETTINGS).join(rest)
            } else {
                p
            };
            if rel.as_os_str().is_empty() || rel.is_absolute() || self.excluded(&rel, false) {
                continue;
            }
            _ = self.pending.insert(rel);
        }
    }

    /// Compare every local and every synced path on the next sync, to catch
    /// changes made while no watcher was running. Set on startup.
    pub fn request_rescan(&mut self) {
        self.rescan = true;
    }

    /// Run one sync cycle.
    ///
    /// # Errors
    ///
    /// On I/O and repository errors, and when the coordinator serves a
    /// history this device can't verify (see [`crate::head::HeadError`]).
    pub fn sync(&mut self) -> Result<Stats> {
        self.clean_leftovers();
        let mut stats = Stats::default();
        for _ in 0..MAX_ROUNDS {
            // the folder's rules file syncs too, so reread it every cycle
            self.ignores = Ignores::load(&self.folder, &self.ignore_rules)?;
            let dormant = self
                .settings
                .as_ref()
                .and_then(|s| s.manifest.dormant(&s.home));
            if dormant != self.dormant {
                match &dormant {
                    Some(why) => warn!("settings sync stands down: {why}"),
                    None if self.settings.is_some() => info!("settings sync is on"),
                    None => {}
                }
                self.dormant = dormant;
            }
            // head first, then the index: a writer's index lands before its
            // head, so this index covers every blob the head needs
            self.state.devices.advance(self.coord.as_ref())?;
            self.check_membership()?;
            self.ensure_key()?;
            let head = self
                .state
                .heads
                .advance(self.coord.as_ref(), &self.state.devices)?;
            let epoch = self.secret()?.0;
            if let Some(h) = head.as_ref().filter(|h| h.epoch > epoch) {
                // a rotation's head lands before its key record
                bail!(EpochError::Pending { epoch: h.epoch });
            }
            let repo = self.open_repo()?;
            let base = self.base_tree()?;
            self.refresh_synced(&repo, base)?;
            if std::mem::take(&mut self.rescan) {
                self.queue_rescan(&repo)?;
            }
            let mut local = self.local_changes(&repo)?;

            if head.as_ref().map(|h| h.seq) != self.state.base.as_ref().map(|b| b.seq) {
                let head = head.ok_or_else(|| anyhow!("base without a head"))?;
                self.pull(&repo, &head, &mut local, &mut stats)?;
                self.save_state()?;
                local = self.local_changes(&repo)?;
            }

            if local.is_empty() {
                self.pending.clear();
                self.state.synced_at = unix_now();
                // the device chain may have moved even when no files did
                self.save_state()?;
                return Ok(stats);
            }
            match self.push(&repo, local)? {
                Some((pushed, moved)) => {
                    stats.pushed += pushed;
                    self.pending = moved;
                    if self.pending.is_empty() {
                        self.state.synced_at = unix_now();
                        self.save_state()?;
                        return Ok(stats);
                    }
                    self.save_state()?;
                    debug!(
                        "{} files changed during upload, pushing again",
                        self.pending.len()
                    );
                }
                None => stats.retries += 1,
            }
        }
        Ok(stats)
    }

    /// Every version of `path` in the folder's history, oldest first: one
    /// entry per head where the path's content, link target or mode changed.
    /// The history is verified the same way a sync verifies it.
    ///
    /// # Errors
    ///
    /// On repository errors, or a history that fails verification.
    pub fn versions(&mut self, path: &Path) -> Result<Vec<Version>> {
        let heads = self.verified_history()?;
        let repo = self.open_repo()?;
        let ids: Vec<String> = heads.iter().filter_map(|h| self.snapshot_now(h)).collect();
        let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
        let snaps: BTreeMap<String, _> = repo
            .get_snapshots(&ids)?
            .into_iter()
            .map(|s| (snapshot_hex(&s.id), s))
            .collect();
        let trees = Trees::new(&repo);
        let mut out: Vec<Version> = Vec::new();
        let mut last: Option<Node> = None;
        for head in &heads {
            let snap = self
                .snapshot_now(head)
                .and_then(|id| snaps.get(&id))
                .ok_or_else(|| anyhow!("snapshot of head {} missing", head.seq))?;
            let node = trees.lookup(Some(snap.tree), path)?.filter(is_leaf);
            let changed = match (&last, &node) {
                (None, None) => false,
                (Some(a), Some(b)) => {
                    a.content != b.content
                        || a.node_type != b.node_type
                        || a.meta.mode != b.meta.mode
                }
                _ => true,
            };
            if changed {
                out.push(Version {
                    seq: head.seq,
                    time: snap.time.timestamp(),
                    device: snap.hostname.clone(),
                    size: node.as_ref().map(|n| n.meta.size),
                });
            }
            last = node;
        }
        Ok(out)
    }

    /// Write `path` as it was at head `seq` to `dest`, outside the synced
    /// folder or inside it (then it syncs like any new file).
    ///
    /// # Errors
    ///
    /// If the head or the path at that head doesn't exist, or on I/O errors.
    pub fn restore_version(&mut self, path: &Path, seq: u64, dest: &Path) -> Result<()> {
        let heads = self.verified_history()?;
        let head = heads
            .iter()
            .find(|h| h.seq == seq)
            .ok_or_else(|| anyhow!("no head {seq}"))?;
        let repo = self.open_repo()?;
        let id = self
            .snapshot_now(head)
            .ok_or_else(|| anyhow!("snapshot of head {seq} was not kept"))?;
        let snap = repo
            .get_snapshots(&[id.as_str()])?
            .pop()
            .ok_or_else(|| anyhow!("snapshot of head {seq} missing"))?;
        let node = Trees::new(&repo)
            .lookup(Some(snap.tree), path)?
            .filter(|n| n.is_file())
            .ok_or_else(|| anyhow!("{} is not a file at head {seq}", path.display()))?;
        if let Some(dir) = dest.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut f = File::create(dest)?;
        repo.dump(&node, &mut f)?;
        if let Some(mode) = node.meta.mode {
            f.set_permissions(fs::Permissions::from_mode(mode & 0o777))?;
        }
        Ok(())
    }

    /// The id in the current epoch of the snapshot `head` names.
    fn snapshot_now(&self, head: &Head) -> Option<String> {
        let now = self.state.secret.as_ref()?.0;
        self.state.epochs.map(&head.snapshot, head.epoch, now)
    }

    /// The whole chain, checked from the first head.
    fn verified_history(&mut self) -> Result<Vec<Head>> {
        self.state.devices.advance(self.coord.as_ref())?;
        self.ensure_key()?;
        self.state
            .heads
            .advance(self.coord.as_ref(), &self.state.devices)?;
        let heads = self.coord.since(0)?;
        let mut prev: Option<&Head> = None;
        for h in &heads {
            crate::head::check(prev, h, &self.state.devices)?;
            prev = Some(h);
        }
        // must end where this device already verified it, or later
        let known = self.state.heads.known.as_ref().map_or(0, |k| k.seq);
        if prev.map_or(0, |p| p.seq) < known {
            bail!(crate::head::HeadError::Rollback {
                known,
                served: prev.map(|p| p.seq),
            });
        }
        Ok(heads)
    }

    fn save_state(&self) -> Result<()> {
        if let Some(path) = &self.state_path {
            write_private(path, &serde_json::to_vec_pretty(&self.state)?)?;
        }
        Ok(())
    }

    fn base_tree(&self) -> Result<Option<TreeId>> {
        self.state
            .base
            .as_ref()
            .map(|b| Ok(TreeId::from(b.tree.parse::<Id>()?)))
            .transpose()
    }

    fn queue_rescan(&mut self, repo: &Repo) -> Result<()> {
        let mut paths = BTreeSet::new();
        let skip = |p: &Path, d: bool| self.excluded(p, d);
        if self.folders.is_some() {
            // each synced folder, under its name here
            for name in &self.synced {
                let dir = self.on_disk(Path::new(name));
                if !dir.is_dir() {
                    continue;
                }
                let mut found = BTreeSet::new();
                let skip_in = |p: &Path, d: bool| self.excluded(&Path::new(name).join(p), d);
                walk_local(&dir, Path::new(""), &skip_in, &mut found)?;
                paths.extend(found.into_iter().map(|p| Path::new(name).join(p)));
                _ = paths.insert(PathBuf::from(name));
            }
        } else {
            walk_local(&self.folder, Path::new(""), &skip, &mut paths)?;
        }
        walk_tree(repo, self.base_tree()?, Path::new(""), &skip, &mut paths)?;
        if let Some(s) = self.settings_on() {
            paths.extend(
                s.manifest
                    .shared_files(&s.home)
                    .into_iter()
                    .map(|p| Path::new(SETTINGS).join(p)),
            );
        }
        self.pending.extend(paths);
        Ok(())
    }

    fn local_changes(&mut self, repo: &Repo) -> Result<BTreeMap<PathBuf, Local>> {
        let base_tree = self.base_tree()?;
        let trees = Trees::new(repo);
        let mut out = BTreeMap::new();
        let mut queue: Vec<PathBuf> = self.pending.iter().cloned().collect();
        let mut seen = BTreeSet::new();
        while let Some(path) = queue.pop() {
            if !seen.insert(path.clone()) {
                continue;
            }
            // settings waiting for `resolve` stay out of pushes
            if self.state.held.contains_key(&path) {
                continue;
            }
            let Some(full) = self.local(&path) else {
                continue;
            };
            let base = trees.lookup(base_tree, &path)?;
            let meta = match fs::symlink_metadata(&full) {
                Ok(m) => Some(m),
                Err(e) if e.kind() == ErrorKind::NotFound => None,
                Err(e) => return Err(e).with_context(|| format!("stat {}", full.display())),
            };
            let is_dir = meta.as_ref().map_or(
                base.as_ref().is_some_and(Node::is_dir),
                fs::Metadata::is_dir,
            );
            if self.excluded(&path, is_dir) {
                continue;
            }
            let Some(meta) = meta else {
                // a synced folder missing here (a new machine, or not made
                // yet) is not a delete of everything in it
                if self.is_folder_root(&path) {
                    continue;
                }
                if base.is_some() {
                    // the folder it was in may now be empty, and stays
                    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                        queue.push(parent.to_path_buf());
                    }
                    _ = out.insert(path, Local::Delete);
                }
                continue;
            };
            if meta.is_dir() {
                // a directory in place of a file or link replaces it
                if base.as_ref().is_some_and(|b| !b.is_dir()) {
                    _ = out.insert(path.clone(), Local::Delete);
                }
                let mut children = 0;
                for e in fs::read_dir(&full)? {
                    let e = e?;
                    let rel = path.join(e.file_name());
                    if !self.excluded(&rel, e.file_type()?.is_dir()) {
                        children += 1;
                    }
                    queue.push(rel);
                }
                // an empty directory syncs as one, unless the base already
                // has it empty
                let base_empty = match base.as_ref().filter(|b| b.is_dir()) {
                    Some(b) => trees.is_empty(b.subtree)?,
                    None => false,
                };
                if children == 0 && !base_empty {
                    let name = path.file_name().ok_or_else(|| anyhow!("bad path"))?;
                    let mtime = Timestamp::try_from(meta.modified()?)?;
                    let mut dir_meta = node_meta(&meta, mtime);
                    dir_meta.mode = Some(GO_MODE_DIR | (meta.mode() & 0o777));
                    let node = Node::new_node(name, NodeType::Dir, dir_meta);
                    _ = out.insert(
                        path,
                        Local::Put {
                            node: Box::new(node),
                            source: None,
                        },
                    );
                }
                continue;
            }
            let name = path.file_name().ok_or_else(|| anyhow!("bad path"))?;
            let mtime = Timestamp::try_from(meta.modified()?)?;
            if meta.file_type().is_symlink() {
                let target = fs::read_link(&full)?;
                let same = base.as_ref().is_some_and(|b| {
                    matches!(b.node_type, NodeType::Symlink { .. })
                        && b.node_type.to_link() == target
                });
                if !same {
                    let node =
                        Node::new_node(name, NodeType::from_link(&target), node_meta(&meta, mtime));
                    _ = out.insert(
                        path,
                        Local::Put {
                            node: Box::new(node),
                            source: None,
                        },
                    );
                }
            } else if meta.is_file() {
                let mut same = base.as_ref().is_some_and(|b| {
                    b.is_file() && b.meta.size == meta.len() && b.meta.mtime == Some(mtime)
                });
                if same && self.state.racy.contains(&path) {
                    let b = base.as_ref().expect("same implies a base node");
                    same = b.meta.size <= MAX_COMPARE && file_matches(repo, &full, b)?;
                    let settled = SystemTime::now()
                        .duration_since(meta.modified()?)
                        .is_ok_and(|age| age > RACY_WINDOW);
                    if same && settled {
                        // any later write gets a later mtime: stat is enough again
                        _ = self.state.racy.remove(&path);
                    }
                }
                if !same {
                    let node = Node::new_node(name, NodeType::File, node_meta(&meta, mtime));
                    _ = out.insert(
                        path,
                        Local::Put {
                            node: Box::new(node),
                            source: Some(full),
                        },
                    );
                }
            }
            // sockets, fifos and devices are not synced
        }
        Ok(out)
    }

    fn pull(
        &mut self,
        repo: &Repo,
        head: &Head,
        local: &mut BTreeMap<PathBuf, Local>,
        stats: &mut Stats,
    ) -> Result<()> {
        let id = self
            .snapshot_now(head)
            .ok_or_else(|| anyhow!("snapshot of head {} is not in this epoch", head.seq))?;
        let snap = repo
            .get_snapshots(&[id.as_str()])?
            .pop()
            .ok_or_else(|| anyhow!("snapshot {id} of head {} missing", head.seq))?;
        if snapshot_hex(&snap.id) != id {
            bail!(
                "snapshot {} does not match head {}",
                snapshot_hex(&snap.id),
                head.seq
            );
        }
        // a folder another device added syncs here too
        self.refresh_synced(repo, Some(snap.tree))?;
        let mut remote = BTreeMap::new();
        diff(
            repo,
            self.base_tree()?,
            Some(snap.tree),
            Path::new(""),
            &mut remote,
        )?;
        info!(
            "pull head {}: {} changes from {}",
            head.seq,
            remote.len(),
            snap.hostname
        );

        let base_tree = self.base_tree()?;
        let (mut deletes, mut writes) = (Vec::new(), Vec::new());
        for (path, rnode) in remote {
            if self.excluded(&path, false) {
                continue;
            }
            stats.pulled += 1;
            if path.starts_with(SETTINGS) {
                self.pull_setting(
                    repo,
                    base_tree,
                    path,
                    rnode,
                    local,
                    &mut writes,
                    &mut deletes,
                    stats,
                )?;
                continue;
            }
            let mine = local.get(&path);
            trace!(
                "{} pull {}: {} remote={} local={}",
                self.device,
                head.seq,
                path.display(),
                describe(rnode.as_ref()),
                match mine {
                    None => "unchanged".to_string(),
                    Some(Local::Delete) => "deleted".to_string(),
                    Some(Local::Put { node, .. }) => describe(Some(node)),
                }
            );
            match (mine, rnode) {
                (None, None) => deletes.push(path),
                (None, Some(r)) => writes.push((path, r)),
                (Some(Local::Delete), None) => _ = local.remove(&path),
                // a change beats a delete
                (Some(Local::Put { .. }), None) => {}
                (Some(Local::Delete), Some(r)) => {
                    _ = local.remove(&path);
                    writes.push((path, r));
                }
                // both made the same empty directory: nothing to decide
                (Some(Local::Put { node: l, .. }), Some(r)) if l.is_dir() && r.is_dir() => {
                    _ = local.remove(&path);
                    writes.push((path, r));
                }
                (Some(Local::Put { node: l, .. }), Some(r)) => {
                    // the same bytes on both sides (for example a pull that
                    // was cut off before its state was saved) are no conflict
                    if self.same_content(repo, &path, l, &r)? {
                        _ = local.remove(&path);
                        self.adopt_metadata(&path, &r)?;
                        continue;
                    }
                    stats.conflicts += 1;
                    let local_wins = (l.meta.mtime, &self.device) > (r.meta.mtime, &snap.hostname);
                    if local_wins {
                        // the kept version may match the new base by size and
                        // mtime alone, so make sure it's compared by content
                        _ = self.state.racy.insert(path.clone());
                        let copy = self.free_conflict_name(&path, r.meta.mtime, &snap.hostname);
                        writes.push((copy, r));
                    } else {
                        let device = self.device.clone();
                        let copy = self.free_conflict_name(&path, l.meta.mtime, &device);
                        fs::rename(self.on_disk(&path), self.on_disk(&copy))?;
                        // (settings never get here: they merge instead)
                        _ = self.pending.insert(copy);
                        _ = local.remove(&path);
                        writes.push((path, r));
                    }
                }
            }
        }
        // deepest first, so directories empty out before they're removed and
        // a directory replaced by a file is gone before the file is written
        for path in deletes.iter().rev() {
            self.remove(path)?;
        }
        self.write_all(repo, &writes)?;
        self.reserved.clear(); // the copies exist on disk now
        for (path, _) in &writes {
            // conflict copies are new here; push them like any local file
            if path.to_string_lossy().contains(".sync-conflict-") {
                _ = self.pending.insert(path.clone());
            }
        }
        // one flush for the whole pull, before the new base is recorded: if
        // the machine dies first, the old base makes the next sync redo it
        if !writes.is_empty() || !deletes.is_empty() {
            sync_filesystem(&self.folder)?;
            if let Some(s) = self.settings_on() {
                sync_filesystem(&s.home)?;
            }
        }
        self.state.base = Some(Base {
            seq: head.seq,
            snapshot: id,
            tree: hex(&snap.tree),
        });
        Ok(())
    }

    /// A remote change to a setting. Changed on both sides: a three way merge
    /// against the base; if the changes overlap, the local file stays and the
    /// remote version is parked, held back until `resolve`. No conflict
    /// copies: apps load their config directories by pattern, and a copy
    /// could be loaded as live config.
    #[allow(clippy::too_many_arguments)]
    fn pull_setting(
        &mut self,
        repo: &Repo,
        base_tree: Option<TreeId>,
        path: PathBuf,
        rnode: Option<Node>,
        local: &mut BTreeMap<PathBuf, Local>,
        writes: &mut Vec<(PathBuf, Node)>,
        deletes: &mut Vec<PathBuf>,
        stats: &mut Stats,
    ) -> Result<()> {
        let held = self.state.held.contains_key(&path);
        let dump = |n: &Node| -> Result<Vec<u8>> {
            let mut out = Vec::new();
            repo.dump(n, &mut out)?;
            Ok(out)
        };
        match (local.get(&path), rnode) {
            (None, None) if !held => deletes.push(path),
            (None, Some(r)) if !held => writes.push((path, r)),
            // held, and changed again remotely: park the newer version
            (None, Some(r)) => self.park(&path, &dump(&r)?)?,
            // held, and deleted remotely: this device's version stands
            (None, None) => {
                if let Some(parked) = self.state.held.remove(&path) {
                    _ = fs::remove_file(parked);
                }
                _ = self.pending.insert(path);
            }
            (Some(Local::Delete), None) => _ = local.remove(&path),
            (Some(Local::Put { .. }), None) => {}
            (Some(Local::Delete), Some(r)) => {
                _ = local.remove(&path);
                writes.push((path, r));
            }
            (Some(Local::Put { node: l, .. }), Some(r)) => {
                if self.same_content(repo, &path, l, &r)? {
                    _ = local.remove(&path);
                    self.adopt_metadata(&path, &r)?;
                    return Ok(());
                }
                let dest = self
                    .local(&path)
                    .ok_or_else(|| anyhow!("settings sync is off"))?;
                let base = Trees::new(repo)
                    .lookup(base_tree, &path)?
                    .filter(Node::is_file);
                let ancestor = base.as_ref().map(dump).transpose()?.unwrap_or_default();
                let (ours, theirs) = (fs::read(&dest)?, dump(&r)?);
                _ = local.remove(&path);
                if let Some(merged) = settings::merge(&ancestor, &ours, &theirs) {
                    info!("merged {} with {}'s changes", path.display(), self.device);
                    self.back_up(&path)?;
                    fs::write(&dest, merged)?;
                    _ = self.pending.insert(path);
                } else {
                    stats.conflicts += 1;
                    warn!(
                        "{} changed here and on another device; kept this version, the other waits for `omacloud settings resolve`",
                        path.display()
                    );
                    self.park(&path, &theirs)?;
                }
            }
        }
        Ok(())
    }

    /// Keep another device's version of a held setting for `resolve`.
    fn park(&mut self, path: &Path, content: &[u8]) -> Result<()> {
        let (Some(s), Ok(rest)) = (self.settings.as_ref(), path.strip_prefix(SETTINGS)) else {
            return Ok(());
        };
        let parked = s.backups.join("held").join(rest);
        fs::create_dir_all(parked.parent().ok_or_else(|| anyhow!("bad path"))?)?;
        fs::write(&parked, content)?;
        _ = self.state.held.insert(path.to_path_buf(), parked);
        Ok(())
    }

    /// Returns the number of paths pushed and the files that changed while
    /// uploading, or `None` if another device took the next head first.
    fn push(
        &mut self,
        repo: &Repo,
        local: BTreeMap<PathBuf, Local>,
    ) -> Result<Option<(usize, BTreeSet<PathBuf>)>> {
        let count = local.len();
        for (path, change) in &local {
            let what = match change {
                Local::Delete => "delete".to_string(),
                Local::Put { node, .. } => describe(Some(node)),
            };
            trace!(
                "{} push onto {:?}: {} {what}",
                self.device,
                self.state.base.as_ref().map(|b| b.seq),
                path.display()
            );
        }
        let mut sent = Vec::new();
        let edits: Vec<(PathBuf, TreeEdit)> = local
            .into_iter()
            .map(|(path, change)| {
                let edit = match change {
                    Local::Delete => TreeEdit::Delete,
                    Local::Put { node, source: None } => TreeEdit::Put(*node),
                    Local::Put {
                        node,
                        source: Some(src),
                    } => {
                        sent.push((path.clone(), node.meta.size, node.meta.mtime));
                        TreeEdit::PutFile {
                            node: *node,
                            reader: Box::new(LazyFile::new(src)),
                        }
                    }
                };
                (path, edit)
            })
            .collect();
        let tree = repo.splice_tree(self.base_tree()?, edits)?;

        let mut snap = SnapshotOptions::default()
            .host(Some(self.device.clone()))
            .to_snapshot()?;
        snap.tree = tree;
        snap.paths = self.folder.to_string_lossy().parse()?;
        let id = snapshot_hex(&repo.save_snapshot(&snap)?);

        let head = Head::next_in(
            &self.signing,
            self.state.heads.known.as_ref(),
            &id,
            self.state.devices.len(),
            self.secret()?.0,
        );
        if !self.coord.append(&head)? {
            debug!("head {} taken by another device, retrying", head.seq);
            return Ok(None);
        }
        self.state.heads.accept_own(&head, &self.state.devices)?;
        self.state.base = Some(Base {
            seq: head.seq,
            snapshot: id,
            tree: hex(&tree),
        });
        info!("pushed head {}: {count} changes", head.seq);

        // a file written during upload may have mixed contents: send it again
        let mut moved = BTreeSet::new();
        let pushed_at = SystemTime::now();
        for (path, size, mtime) in sent {
            _ = self.state.racy.remove(&path);
            let now = self.local(&path).and_then(|p| fs::symlink_metadata(p).ok());
            let same = now.is_some_and(|m| {
                m.len() == size
                    && m.modified().ok().and_then(|t| Timestamp::try_from(t).ok()) == mtime
            });
            if !same {
                _ = moved.insert(path);
            } else if mtime.is_some_and(|m| {
                pushed_at
                    .duration_since(SystemTime::from(m))
                    .map_or(true, |age| age <= RACY_WINDOW)
            }) {
                _ = self.state.racy.insert(path);
            }
        }
        Ok(Some((count, moved)))
    }

    fn remove(&self, path: &Path) -> Result<()> {
        let Some(dest) = self.local(path) else {
            return Ok(());
        };
        // Documents stays, even emptied: it's a place, not a synced file
        if self.is_folder_root(path) {
            return Ok(());
        }
        if path.starts_with(SETTINGS) {
            // a setting goes to history first, and home's directories stay
            self.back_up(path)?;
            match fs::remove_file(&dest) {
                Err(e) if e.kind() != ErrorKind::NotFound => return Err(e.into()),
                _ => return Ok(()),
            }
        }
        if fs::symlink_metadata(&dest).is_ok_and(|m| m.is_dir()) {
            // only an empty directory is removed; anything in it stays
            _ = fs::remove_dir(&dest);
        } else {
            match fs::remove_file(&dest) {
                Err(e) if e.kind() != ErrorKind::NotFound => {
                    return Err(e).with_context(|| format!("removing {}", dest.display()));
                }
                _ => {}
            }
        }
        let mut dir = dest.parent();
        let top = |d: &Path| self.folders.is_some() && d.parent() == Some(self.folder.as_path());
        while let Some(d) = dir.filter(|d| *d != self.folder && !top(d)) {
            if fs::remove_dir(d).is_err() {
                break;
            }
            dir = d.parent();
        }
        Ok(())
    }

    /// A conflict copy name for `path` that nothing on disk or planned in
    /// this pull uses yet. Names have one second resolution, so two versions
    /// losing within a second would otherwise overwrite each other's copy.
    fn free_conflict_name(
        &mut self,
        path: &Path,
        mtime: Option<Timestamp>,
        device: &str,
    ) -> PathBuf {
        let mut n = 0;
        loop {
            let name = conflict_name(path, mtime, device, n);
            let taken = self.reserved.contains(&name)
                || self
                    .local(&name)
                    .is_some_and(|p| fs::symlink_metadata(p).is_ok());
            if !taken {
                _ = self.reserved.insert(name.clone());
                return name;
            }
            n += 1;
        }
    }

    /// Make room for `path`: anything in the way that isn't a directory
    /// moves aside as a conflict copy, and the parent directories exist.
    fn prepare_write(&mut self, path: &Path, node: &Node) -> Result<()> {
        let dest = self
            .local(path)
            .ok_or_else(|| anyhow!("settings sync is off"))?;
        if path.starts_with(SETTINGS) {
            self.back_up(path)?;
            fs::create_dir_all(dest.parent().ok_or_else(|| anyhow!("bad path"))?)?;
            return Ok(());
        }
        let mut prefix = PathBuf::new();
        for c in path.parent().into_iter().flat_map(Path::components) {
            prefix.push(c);
            self.move_aside_unless_dir(&prefix)?;
        }
        let in_the_way = fs::symlink_metadata(&dest).is_ok_and(|m| {
            if node.is_dir() {
                !m.is_dir()
            } else {
                m.is_dir()
            }
        });
        if in_the_way {
            let device = self.device.clone();
            let copy = self.free_conflict_name(path, None, &device);
            fs::rename(&dest, self.on_disk(&copy))?;
            _ = self.pending.insert(copy);
        }
        let parent = dest.parent().ok_or_else(|| anyhow!("bad path"))?;
        fs::create_dir_all(parent)?;
        Ok(())
    }

    /// Write every node, fetching contents concurrently: each file is at
    /// least one request to the repository, and in sequence a pull of many
    /// small files over a slow link would take minutes.
    fn write_all(&mut self, repo: &Repo, writes: &[(PathBuf, Node)]) -> Result<()> {
        for (path, node) in writes {
            self.prepare_write(path, node)?;
        }
        // what a person reaches for first arrives first: small files before
        // large, documents before pictures, newest first. A new machine is
        // usable while its photos still stream in.
        let mut dests: Vec<(PathBuf, &Node, PullOrder)> = writes
            .iter()
            .filter_map(|(path, node)| {
                let order = (
                    node.meta.size > BIG_FILE,
                    self.folder_rank(path),
                    std::cmp::Reverse(node.meta.mtime),
                );
                self.local(path).map(|d| (d, node, order))
            })
            .collect();
        dests.sort_by_key(|d| d.2);
        let total: u64 = dests.iter().map(|(_, n, _)| n.meta.size).sum();
        let next = std::sync::atomic::AtomicUsize::new(0);
        let done_bytes = std::sync::atomic::AtomicU64::new(0);
        let failed: Mutex<Option<anyhow::Error>> = Mutex::new(None);
        let last_report = Mutex::new(Instant::now());
        std::thread::scope(|scope| {
            for _ in 0..PULL_THREADS.min(dests.len()) {
                scope.spawn(|| {
                    use std::sync::atomic::Ordering::Relaxed;
                    loop {
                        if failed.lock().unwrap().is_some() {
                            return;
                        }
                        let i = next.fetch_add(1, Relaxed);
                        let Some((dest, node, _)) = dests.get(i) else {
                            return;
                        };
                        if let Err(e) = write_file(dest, repo, node) {
                            _ = failed.lock().unwrap().get_or_insert(e);
                            return;
                        }
                        let done = done_bytes.fetch_add(node.meta.size, Relaxed) + node.meta.size;
                        let mut last = last_report.lock().unwrap();
                        if last.elapsed() >= Duration::from_secs(5) {
                            *last = Instant::now();
                            info!(
                                "pulling: {} of {} files, {} of {} MB",
                                (i + 1).min(dests.len()),
                                dests.len(),
                                done / 1_000_000,
                                total / 1_000_000
                            );
                        }
                    }
                });
            }
        });
        failed.into_inner().unwrap().map_or(Ok(()), Err)
    }

    /// Which folders come first in a pull: Desktop and Documents (and
    /// settings) before other folders, pictures, music and videos last.
    fn folder_rank(&self, path: &Path) -> u8 {
        if self.folders.is_none() || path.starts_with(".omacloud") {
            return 0;
        }
        match path
            .components()
            .next()
            .and_then(|c| c.as_os_str().to_str())
        {
            Some("Desktop" | "Documents") => 0,
            Some("Pictures" | "Music" | "Videos") => 2,
            _ => 1,
        }
    }

    fn same_content(&self, repo: &Repo, path: &Path, local: &Node, remote: &Node) -> Result<bool> {
        let Some(full) = self.local(path) else {
            return Ok(false);
        };
        if let (NodeType::Symlink { .. }, NodeType::Symlink { .. }) =
            (&local.node_type, &remote.node_type)
        {
            return Ok(local.node_type.to_link() == remote.node_type.to_link());
        }
        if !(local.is_file() && remote.is_file())
            || local.meta.size != remote.meta.size
            || remote.meta.size > MAX_COMPARE
        {
            return Ok(false);
        }
        file_matches(repo, &full, remote)
    }

    /// Give an identical local file the synced version's mtime and mode, so
    /// it matches the base and isn't pushed back.
    fn adopt_metadata(&self, path: &Path, node: &Node) -> Result<()> {
        let Some(full) = self.local(path) else {
            return Ok(());
        };
        if !node.is_file() {
            return Ok(());
        }
        let f = File::options().write(true).open(&full)?;
        if let Some(mode) = node.meta.mode {
            f.set_permissions(fs::Permissions::from_mode(mode & 0o777))?;
        }
        if let Some(mtime) = node.meta.mtime {
            f.set_modified(SystemTime::from(mtime))?;
        }
        Ok(())
    }

    fn move_aside_unless_dir(&mut self, rel: &Path) -> Result<()> {
        let full = self.on_disk(rel);
        match fs::symlink_metadata(&full) {
            Ok(m) if !m.is_dir() => {
                let device = self.device.clone();
                let copy = self.free_conflict_name(rel, None, &device);
                fs::rename(&full, self.on_disk(&copy))?;
                _ = self.pending.insert(copy);
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

/// Write `node` at `path` through a temp file renamed into place, with the
/// synced mtime and mode. The parent directory must exist.
fn write_file(dest: &Path, repo: &Repo, node: &Node) -> Result<()> {
    let dest = dest.to_path_buf();
    if node.is_dir() {
        fs::create_dir_all(&dest)?;
        if let Some(mode) = node.meta.mode {
            fs::set_permissions(&dest, fs::Permissions::from_mode(mode & 0o777))?;
        }
        return Ok(());
    }
    // already here from a pull that was cut off: same size and time, and
    // (chunked locally, nothing downloaded) the same content
    if node.is_file()
        && fs::symlink_metadata(&dest).is_ok_and(|m| {
            m.is_file()
                && m.len() == node.meta.size
                && m.modified().ok().and_then(|t| Timestamp::try_from(t).ok()) == node.meta.mtime
        })
        && file_matches(repo, &dest, node)?
    {
        return Ok(());
    }
    let parent = dest.parent().ok_or_else(|| anyhow!("bad path"))?;
    let tmp = parent.join(format!("{TEMP_PREFIX}{}", node.name().to_string_lossy()));
    _ = fs::remove_file(&tmp);
    if let NodeType::Symlink { .. } = node.node_type {
        std::os::unix::fs::symlink(node.node_type.to_link(), &tmp)?;
    } else {
        let mut f = File::create(&tmp)?;
        repo.dump(node, &mut f)?;
        if let Some(mode) = node.meta.mode {
            f.set_permissions(fs::Permissions::from_mode(mode & 0o777))?;
        }
        if let Some(mtime) = node.meta.mtime {
            f.set_modified(SystemTime::from(mtime))?;
        }
    }
    fs::rename(&tmp, &dest)?;
    Ok(())
}

/// Flush the filesystem holding `dir` to disk.
fn sync_filesystem(dir: &Path) -> Result<()> {
    use std::os::fd::AsRawFd;
    let d = File::open(dir)?;
    // SAFETY: syncfs on a descriptor we own for the duration of the call
    if unsafe { libc::syncfs(d.as_raw_fd()) } != 0 {
        return Err(io::Error::last_os_error()).context("syncfs");
    }
    Ok(())
}

/// Whether the file at `path` holds exactly the content of `node`.
fn file_matches(repo: &Repo, path: &Path, node: &Node) -> Result<bool> {
    let mut theirs = Vec::with_capacity(usize::try_from(node.meta.size).unwrap_or(0));
    repo.dump(node, &mut theirs)?;
    Ok(fs::read(path)? == theirs)
}

fn describe(node: Option<&Node>) -> String {
    node.map_or_else(
        || "gone".to_string(),
        |n| format!("{}B@{:?}", n.meta.size, n.meta.mtime),
    )
}

fn node_meta(m: &fs::Metadata, mtime: Timestamp) -> Metadata {
    Metadata {
        mode: Some(m.mode() & 0o777),
        mtime: Some(mtime),
        uid: Some(m.uid()),
        gid: Some(m.gid()),
        size: if m.is_file() { m.len() } else { 0 },
        ..Metadata::default()
    }
}

/// Opens the file on first read, so a large push doesn't hold a descriptor
/// per file.
struct LazyFile {
    path: PathBuf,
    file: Option<File>,
}

impl LazyFile {
    fn new(path: PathBuf) -> Self {
        Self { path, file: None }
    }
}

impl Read for LazyFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.file.is_none() {
            self.file = Some(File::open(&self.path)?);
        }
        let n = self.file.as_mut().unwrap().read(buf)?;
        if n == 0 {
            self.file = None; // done: release the descriptor
        }
        Ok(n)
    }
}

fn nodes(repo: &Repo, id: Option<TreeId>) -> Result<BTreeMap<String, Node>> {
    let Some(id) = id else {
        return Ok(BTreeMap::new());
    };
    Ok(repo
        .get_tree(&id)?
        .nodes
        .into_iter()
        .map(|n| (n.name().to_string_lossy().into_owned(), n))
        .collect())
}

/// Trees read during one operation, each parsed once. Looking up many paths
/// in the same directories (a rescan) would otherwise reparse a directory's
/// tree for every file in it.
struct Trees<'r> {
    repo: &'r Repo,
    cache: RefCell<HashMap<TreeId, Rc<BTreeMap<String, Node>>>>,
}

impl<'r> Trees<'r> {
    fn new(repo: &'r Repo) -> Self {
        Self {
            repo,
            cache: RefCell::new(HashMap::new()),
        }
    }

    fn nodes(&self, id: TreeId) -> Result<Rc<BTreeMap<String, Node>>> {
        if let Some(n) = self.cache.borrow().get(&id) {
            return Ok(n.clone());
        }
        let n = Rc::new(nodes(self.repo, Some(id))?);
        _ = self.cache.borrow_mut().insert(id, n.clone());
        Ok(n)
    }

    fn is_empty(&self, tree: Option<TreeId>) -> Result<bool> {
        Ok(match tree {
            Some(id) => self.nodes(id)?.is_empty(),
            None => true,
        })
    }

    fn lookup(&self, root: Option<TreeId>, path: &Path) -> Result<Option<Node>> {
        let mut tree = root;
        let mut node = None;
        for c in path.components() {
            let Some(id) = tree else {
                return Ok(None);
            };
            let Some(n) = self
                .nodes(id)?
                .get(&*c.as_os_str().to_string_lossy())
                .cloned()
            else {
                return Ok(None);
            };
            tree = n.subtree;
            node = Some(n);
        }
        Ok(node)
    }
}

fn is_leaf(n: &Node) -> bool {
    n.is_file() || matches!(n.node_type, NodeType::Symlink { .. })
}

/// Changed leaves (files, symlinks) between two trees: path to new node, or
/// `None` if removed. Subtrees with equal ids are skipped.
fn diff(
    repo: &Repo,
    a: Option<TreeId>,
    b: Option<TreeId>,
    prefix: &Path,
    out: &mut BTreeMap<PathBuf, Option<Node>>,
) -> Result<()> {
    if a == b {
        return Ok(());
    }
    let (an, bn) = (nodes(repo, a)?, nodes(repo, b)?);
    let names: BTreeSet<&String> = an.keys().chain(bn.keys()).collect();
    for name in names {
        let path = prefix.join(name);
        let (x, y) = (an.get(name), bn.get(name));
        let x_dir = x.filter(|n| n.is_dir()).and_then(|n| n.subtree);
        let y_dir = y.filter(|n| n.is_dir()).and_then(|n| n.subtree);
        if x_dir.is_some() || y_dir.is_some() {
            diff(repo, x_dir, y_dir, &path, out)?;
        }
        // empty directories are changes of their own
        let (x_is_dir, y_is_dir) = (x.is_some_and(Node::is_dir), y.is_some_and(Node::is_dir));
        if y_is_dir && (!x_is_dir || x_dir != y_dir) && nodes(repo, y_dir)?.is_empty() {
            _ = out.insert(path.clone(), y.cloned());
        } else if x_is_dir && !y_is_dir && nodes(repo, x_dir)?.is_empty() {
            _ = out.insert(path.clone(), None);
        }
        match (x.filter(|n| is_leaf(n)), y.filter(|n| is_leaf(n))) {
            (Some(_), None) => _ = out.insert(path, None),
            (x, Some(y))
                if x.is_none_or(|x| {
                    x.node_type != y.node_type
                        || x.content != y.content
                        || x.meta.mtime != y.meta.mtime
                        || x.meta.mode != y.meta.mode
                }) =>
            {
                _ = out.insert(path, Some(y.clone()));
            }
            _ => {}
        }
    }
    Ok(())
}

fn walk_local(
    root: &Path,
    rel: &Path,
    skip: &dyn Fn(&Path, bool) -> bool,
    out: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    let mut empty = true;
    for e in fs::read_dir(root.join(rel))? {
        let e = e?;
        let path = rel.join(e.file_name());
        let is_dir = e.file_type()?.is_dir();
        if skip(&path, is_dir) {
            continue;
        }
        empty = false;
        if is_dir {
            walk_local(root, &path, skip, out)?;
        } else {
            _ = out.insert(path);
        }
    }
    if empty && !rel.as_os_str().is_empty() {
        _ = out.insert(rel.to_path_buf());
    }
    Ok(())
}

fn walk_tree(
    repo: &Repo,
    tree: Option<TreeId>,
    rel: &Path,
    skip: &dyn Fn(&Path, bool) -> bool,
    out: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    for (name, node) in nodes(repo, tree)? {
        let path = rel.join(name);
        // settings directories are walked for the files in them
        let settings_dir = node.is_dir()
            && [SETTINGS, PACKAGES, SECRETS]
                .iter()
                .any(|p| Path::new(p).starts_with(&path) || path.starts_with(p));
        if !settings_dir && skip(&path, node.is_dir()) {
            continue;
        }
        if node.is_dir() {
            if nodes(repo, node.subtree)?.is_empty() {
                _ = out.insert(path.clone());
            }
            walk_tree(repo, node.subtree, &path, skip, out)?;
        } else {
            _ = out.insert(path);
        }
    }
    Ok(())
}

/// `notes.md` -> `notes.sync-conflict-20260928-104512-laptop.md`, Syncthing's
/// form. The time is the losing version's mtime, or now. `n > 0` makes the
/// name unique when that one is taken: `...-laptop-2.md`.
fn conflict_name(path: &Path, mtime: Option<Timestamp>, device: &str, n: u32) -> PathBuf {
    let device = if n == 0 {
        device.to_string()
    } else {
        format!("{device}-{}", n + 1)
    };
    let when = mtime
        .unwrap_or_else(Timestamp::now)
        .to_zoned(TimeZone::UTC)
        .strftime("%Y%m%d-%H%M%S");
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let name = match path.extension() {
        Some(ext) => format!(
            "{stem}.sync-conflict-{when}-{device}.{}",
            ext.to_string_lossy()
        ),
        None => format!("{stem}.sync-conflict-{when}-{device}"),
    };
    path.with_file_name(name)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
