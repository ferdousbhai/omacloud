//! Spike 2: change driven sync with Syncthing style conflict copies.
//!
//! A device keeps `base`: the snapshot its folder matched after its last sync.
//! A sync cycle:
//!   1. local changes = candidate paths (from inotify in the daemon) whose
//!      size or mtime differ from `base`. Files we wrote during a pull carry
//!      the snapshot's mtime, so they aren't echoed back.
//!   2. if the coordinator's head moved past `base`: diff base..head by tree
//!      id (equal subtrees skipped), apply remote changes to disk, resolve
//!      paths changed on both sides with a conflict copy, move base to head.
//!   3. push local changes with `splice` onto base, save a snapshot whose
//!      parent is base, and compare-and-swap the head. If another device got
//!      there first, go around again; uploaded blobs are reused.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Mutex,
    time::SystemTime,
};

use anyhow::{Result, anyhow};
use rustic_core::{
    Credentials, IndexedFullStatus, Repository, RepositoryOptions, SnapshotOptions, TreeId,
    jiff::{Timestamp, tz::TimeZone},
    omacloud::Edit,
    repofile::{MasterKey, Metadata, Node, NodeType, SnapshotId},
};

use crate::backends;

type Repo = Repository<IndexedFullStatus>;

/// Stands in for the per account Durable Object: orders heads, nothing else.
#[derive(Default)]
pub struct Coordinator {
    head: Mutex<Option<SnapshotId>>,
}

impl Coordinator {
    pub fn head(&self) -> Option<SnapshotId> {
        *self.head.lock().unwrap()
    }

    fn cas(&self, expected: Option<SnapshotId>, new: SnapshotId) -> bool {
        let mut head = self.head.lock().unwrap();
        if *head != expected {
            return false;
        }
        *head = Some(new);
        true
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub pushed: usize,
    pub pulled: usize,
    pub conflicts: usize,
    pub retries: usize,
}

enum Local {
    Put(Node),
    Delete,
}

pub struct Device<'a> {
    pub name: String,
    pub dir: PathBuf,
    repo: &'a str,
    key: MasterKey,
    coord: &'a Coordinator,
    base: Option<(SnapshotId, TreeId)>,
    pending: BTreeSet<PathBuf>,
}

impl<'a> Device<'a> {
    pub fn new(name: &str, dir: &Path, repo: &'a str, key: MasterKey, coord: &'a Coordinator) -> Self {
        Self {
            name: name.to_string(),
            dir: dir.to_path_buf(),
            repo,
            key,
            coord,
            base: None,
            pending: BTreeSet::new(),
        }
    }

    /// The daemon holds the master key, so opening skips scrypt.
    fn open(&self) -> Result<Repo> {
        Ok(Repository::new(&RepositoryOptions::default(), &backends(self.repo)?)?
            .open(&Credentials::Masterkey(self.key.clone()))?
            .to_indexed()?)
    }

    pub fn base_tree(&self) -> Option<TreeId> {
        self.base.map(|b| b.1)
    }

    pub fn sync(&mut self, candidates: impl IntoIterator<Item = PathBuf>) -> Result<Stats> {
        self.pending.extend(candidates);
        let mut stats = Stats::default();
        loop {
            // head first, then the index: a writer's index lands before it
            // moves the head, so this index covers every blob head needs
            let head = self.coord.head();
            let repo = self.open()?;
            let mut local = self.local_changes(&repo)?;

            if head != self.base.map(|b| b.0) {
                let head = head.unwrap(); // base only ever trails a real head
                let snap = repo
                    .get_snapshots(&[head.to_string()])?
                    .pop()
                    .ok_or_else(|| anyhow!("head {head} missing"))?;
                let mut remote = BTreeMap::new();
                diff(&repo, self.base_tree(), Some(snap.tree), Path::new(""), &mut remote)?;
                for (path, rnode) in remote {
                    stats.pulled += 1;
                    match (local.get(&path), rnode) {
                        (None, r) => self.apply(&repo, &path, r.as_ref())?,
                        (Some(Local::Delete), None) => _ = local.remove(&path),
                        // a change beats a delete, as in Syncthing
                        (Some(Local::Put(_)), None) => {}
                        (Some(Local::Delete), Some(r)) => {
                            _ = local.remove(&path);
                            self.apply(&repo, &path, Some(&r))?;
                        }
                        (Some(Local::Put(l)), Some(r)) => {
                            stats.conflicts += 1;
                            // newer mtime wins, ties go to the larger device name
                            let local_wins =
                                (l.meta.mtime, &self.name) > (r.meta.mtime, &snap.hostname);
                            if local_wins {
                                let copy = conflict_name(&path, r.meta.mtime, &snap.hostname);
                                self.apply(&repo, &copy, Some(&r))?;
                                _ = self.pending.insert(copy);
                            } else {
                                let copy = conflict_name(&path, l.meta.mtime, &self.name);
                                fs::rename(self.dir.join(&path), self.dir.join(&copy))?;
                                _ = self.pending.insert(copy);
                                _ = local.remove(&path);
                                self.apply(&repo, &path, Some(&r))?;
                            }
                        }
                    }
                }
                self.base = Some((head, snap.tree));
                // conflict copies are new local files; re-read against the new base
                local = self.local_changes(&repo)?;
            }

            if local.is_empty() {
                self.pending.clear();
                return Ok(stats);
            }

            let edits = local.into_iter().map(|(path, l)| {
                let edit = match l {
                    Local::Put(node) => Edit::Put {
                        node,
                        source: Some(self.dir.join(&path)),
                    },
                    Local::Delete => Edit::Delete,
                };
                (path, edit)
            });
            let edits: Vec<_> = edits.collect();
            let count = edits.len();
            let tree = repo.splice(self.base_tree(), edits)?;
            let mut snap = SnapshotOptions::default()
                .host(Some(self.name.clone()))
                .to_snapshot()?;
            snap.tree = tree;
            snap.parent = self.base.map(|b| b.0);
            snap.paths = "/sync".parse()?;
            let id = repo.save_snapshot(&snap)?;
            if self.coord.cas(self.base.map(|b| b.0), id) {
                self.base = Some((id, tree));
                self.pending.clear();
                stats.pushed += count;
                return Ok(stats);
            }
            // lost the race: the orphan snapshot is left for forget/prune
            stats.retries += 1;
        }
    }

    fn local_changes(&self, repo: &Repo) -> Result<BTreeMap<PathBuf, Local>> {
        let mut out = BTreeMap::new();
        for path in &self.pending {
            let base = lookup(repo, self.base_tree(), path)?.filter(|n| n.is_file());
            match fs::symlink_metadata(self.dir.join(path)) {
                Ok(m) if m.is_file() => {
                    let mtime = Timestamp::try_from(m.modified()?)?;
                    let same = base
                        .as_ref()
                        .is_some_and(|b| b.meta.size == m.len() && b.meta.mtime == Some(mtime));
                    if !same {
                        let meta = Metadata {
                            mode: Some(m.mode() & 0o777),
                            mtime: Some(mtime),
                            uid: Some(m.uid()),
                            gid: Some(m.gid()),
                            size: m.len(),
                            ..Metadata::default()
                        };
                        let name = path.file_name().unwrap();
                        let node = Node::new_node(name, NodeType::File, meta);
                        _ = out.insert(path.clone(), Local::Put(node));
                    }
                }
                _ if base.is_some() => _ = out.insert(path.clone(), Local::Delete),
                _ => {}
            }
        }
        Ok(out)
    }

    fn apply(&self, repo: &Repo, path: &Path, node: Option<&Node>) -> Result<()> {
        let dest = self.dir.join(path);
        let Some(node) = node else {
            match fs::remove_file(&dest) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
            // drop directories this left empty, up to the root
            let mut dir = dest.parent();
            while let Some(d) = dir.filter(|d| *d != self.dir) {
                if fs::remove_dir(d).is_err() {
                    break;
                }
                dir = d.parent();
            }
            return Ok(());
        };
        let parent = dest.parent().unwrap();
        fs::create_dir_all(parent)?;
        let tmp = parent.join(format!(".~omacloud-{}", node.name().to_string_lossy()));
        let mut f = fs::File::create(&tmp)?;
        repo.dump(node, &mut f)?;
        if let Some(mtime) = node.meta.mtime {
            f.set_modified(SystemTime::from(mtime))?;
        }
        if let Some(mode) = node.meta.mode {
            f.set_permissions(fs::Permissions::from_mode(mode & 0o777))?;
        }
        drop(f);
        fs::rename(&tmp, &dest)?;
        Ok(())
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

fn lookup(repo: &Repo, root: Option<TreeId>, path: &Path) -> Result<Option<Node>> {
    let mut tree = root;
    let mut node = None;
    for c in path.components() {
        let Some(n) = nodes(repo, tree)?.remove(&*c.as_os_str().to_string_lossy()) else {
            return Ok(None);
        };
        tree = n.subtree;
        node = Some(n);
    }
    Ok(node)
}

/// Changed files between two trees: path -> new node, or None if removed.
/// Subtrees with equal ids are skipped, so cost follows the change.
pub fn diff(
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
        let x_file = x.filter(|n| n.is_file());
        match (x_file, y.filter(|n| n.is_file())) {
            (Some(_), None) => _ = out.insert(path, None),
            (x, Some(y))
                if x.is_none_or(|x| {
                    x.content != y.content || x.meta.mtime != y.meta.mtime || x.meta.mode != y.meta.mode
                }) =>
            {
                _ = out.insert(path, Some(y.clone()));
            }
            _ => {}
        }
    }
    Ok(())
}

/// `notes.md` -> `notes.sync-conflict-20260928-104512-laptop.md`, Syncthing's form.
fn conflict_name(path: &Path, mtime: Option<Timestamp>, device: &str) -> PathBuf {
    let when = mtime
        .unwrap_or_else(Timestamp::now)
        .to_zoned(TimeZone::UTC)
        .strftime("%Y%m%d-%H%M%S");
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let name = match path.extension() {
        Some(ext) => format!("{stem}.sync-conflict-{when}-{device}.{}", ext.to_string_lossy()),
        None => format!("{stem}.sync-conflict-{when}-{device}"),
    };
    path.with_file_name(name)
}
