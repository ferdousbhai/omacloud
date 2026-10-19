//! The account's files as one head has them, read on demand: what
//! `omacloud mount` shows. Nothing is downloaded until a file is read, so
//! a computer can reach a folder it doesn't keep (one skipped there).

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{Result, anyhow};
use rustic_core::{
    TreeId,
    jiff::Timestamp,
    repofile::{Node, NodeType},
    vfs::OpenFile,
};

use crate::repo::Repo;

/// What a path is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
}

/// A file, directory or symlink in the view.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub kind: Kind,
    pub size: u64,
    pub mtime: Option<Timestamp>,
    pub mode: Option<u32>,
}

/// A file opened for reading.
pub struct Opened(OpenFile);

pub struct Browse {
    repo: Repo,
    root: Option<TreeId>,
    /// The head shown.
    pub seq: u64,
    trees: Mutex<HashMap<TreeId, Arc<Vec<Node>>>>,
}

/// Omacloud's own files (settings, package lists, secrets) stay out of
/// the view.
const HIDDEN: &str = ".omacloud";

impl Browse {
    pub(crate) fn new(repo: Repo, root: Option<TreeId>, seq: u64) -> Self {
        Self {
            repo,
            root,
            seq,
            trees: Mutex::new(HashMap::new()),
        }
    }

    fn tree(&self, id: Option<TreeId>) -> Result<Arc<Vec<Node>>> {
        let Some(id) = id else {
            return Ok(Arc::default());
        };
        if let Some(t) = self.trees.lock().unwrap().get(&id) {
            return Ok(t.clone());
        }
        let t = Arc::new(self.repo.get_tree(&id)?.nodes);
        _ = self.trees.lock().unwrap().insert(id, t.clone());
        Ok(t)
    }

    fn node(&self, path: &Path) -> Result<Option<Node>> {
        let mut tree = self.root;
        let mut node = None;
        for (i, c) in path.components().enumerate() {
            let name = c.as_os_str();
            if i == 0 && name == HIDDEN {
                return Ok(None);
            }
            let Some(n) = self.tree(tree)?.iter().find(|n| n.name() == name).cloned() else {
                return Ok(None);
            };
            tree = n.subtree;
            node = Some(n);
        }
        Ok(node)
    }

    /// The entry at `path`; the empty path is the top.
    ///
    /// # Errors
    ///
    /// If a tree can't be read.
    pub fn entry(&self, path: &Path) -> Result<Option<Entry>> {
        if path.as_os_str().is_empty() {
            return Ok(Some(Entry {
                name: String::new(),
                kind: Kind::Dir,
                size: 0,
                mtime: None,
                mode: Some(0o755),
            }));
        }
        Ok(self.node(path)?.as_ref().and_then(entry))
    }

    /// What's in the directory at `path`.
    ///
    /// # Errors
    ///
    /// If it isn't a directory, or a tree can't be read.
    pub fn list(&self, path: &Path) -> Result<Vec<Entry>> {
        let tree = if path.as_os_str().is_empty() {
            self.root
        } else {
            self.node(path)?
                .filter(Node::is_dir)
                .ok_or_else(|| anyhow!("{} is not a directory", path.display()))?
                .subtree
        };
        let top = path.as_os_str().is_empty();
        Ok(self
            .tree(tree)?
            .iter()
            .filter(|n| !(top && n.name() == std::ffi::OsStr::new(HIDDEN)))
            .filter_map(entry)
            .collect())
    }

    /// Where the symlink at `path` points.
    ///
    /// # Errors
    ///
    /// If it isn't a symlink.
    pub fn link(&self, path: &Path) -> Result<PathBuf> {
        match self.node(path)? {
            Some(n) if matches!(n.node_type, NodeType::Symlink { .. }) => Ok(n.node_type.to_link().to_path_buf()),
            _ => Err(anyhow!("{} is not a symlink", path.display())),
        }
    }

    /// Open the file at `path` for reading.
    ///
    /// # Errors
    ///
    /// If it isn't a file, or its blobs aren't in the index.
    pub fn open(&self, path: &Path) -> Result<Opened> {
        let node = self
            .node(path)?
            .filter(Node::is_file)
            .ok_or_else(|| anyhow!("{} is not a file", path.display()))?;
        Ok(Opened(self.repo.open_file(&node)?))
    }

    /// Up to `len` bytes of an opened file from `offset`, fetched now.
    ///
    /// # Errors
    ///
    /// If the storage can't be read.
    pub fn read(&self, file: &Opened, offset: u64, len: usize) -> Result<Vec<u8>> {
        Ok(self.repo.read_file_at(&file.0, usize::try_from(offset)?, len)?.to_vec())
    }
}

fn entry(n: &Node) -> Option<Entry> {
    let kind = match n.node_type {
        NodeType::File => Kind::File,
        NodeType::Dir => Kind::Dir,
        NodeType::Symlink { .. } => Kind::Symlink,
        _ => return None,
    };
    Some(Entry {
        name: n.name().to_string_lossy().into_owned(),
        kind,
        size: n.meta.size,
        mtime: n.meta.mtime,
        mode: n.meta.mode,
    })
}
