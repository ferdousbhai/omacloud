//! onecloud fork: change driven tree writes for live sync.
//!
//! `Repository::splice` writes content for the changed files only and
//! rewrites only the trees on the paths from those files to the root. Every
//! other subtree keeps its id, so a push costs the size of the change, not the
//! size of the folder. Upstream candidate.

use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fs::File,
    path::{Component, Path, PathBuf},
};

use crate::{
    BlobId, DataId, ErrorKind, RusticError, RusticResult,
    blob::{
        BlobType,
        packer::{PackSizer, Packer},
        tree::{Tree, TreeId},
    },
    chunker::ChunkIter,
    crypto::hasher::hash,
    index::{ReadIndex, indexer::Indexer},
    backend::decrypt::DecryptWriteBackend,
    repofile::{Metadata, Node, NodeType, SnapshotFile, SnapshotId},
    repository::{IndexedIds, Repository},
};

/// One change to apply at a path relative to the tree root.
#[derive(Debug)]
pub enum Edit {
    /// Put this node. For a file, `source` is read, chunked and stored, and
    /// the node's content is set from it.
    Put {
        /// The node to store at the path.
        node: Node,
        /// The local file to read the content from.
        source: Option<PathBuf>,
    },
    /// Remove whatever is at the path (a subtree goes with it).
    Delete,
}

#[derive(Default)]
struct Group {
    here: BTreeMap<OsString, Edit>,
    sub: BTreeMap<OsString, Group>,
}

impl Group {
    fn insert(&mut self, path: &Path, edit: Edit) -> RusticResult<()> {
        let mut names: Vec<&OsStr> = Vec::new();
        for c in path.components() {
            match c {
                Component::Normal(n) => names.push(n),
                _ => {
                    return Err(RusticError::new(
                        ErrorKind::InvalidInput,
                        "splice path `{path}` must be relative and normal",
                    )
                    .attach_context("path", path.display().to_string()));
                }
            }
        }
        let Some((last, dirs)) = names.split_last() else {
            return Err(RusticError::new(ErrorKind::InvalidInput, "empty splice path"));
        };
        let mut g = self;
        for d in dirs {
            g = g.sub.entry((*d).to_owned()).or_default();
        }
        _ = g.here.insert((*last).to_owned(), edit);
        Ok(())
    }
}

impl<S: crate::repository::Open> Repository<S> {
    /// Save one snapshot and return its id (`save_snapshots` doesn't).
    ///
    /// # Errors
    ///
    /// * If writing to the backend fails.
    pub fn save_snapshot(&self, snap: &SnapshotFile) -> RusticResult<SnapshotId> {
        let mut snap = snap.clone();
        snap.id = SnapshotId::default();
        Ok(SnapshotId::from(self.dbe().save_file(&snap)?))
    }
}

impl<S: IndexedIds> Repository<S> {
    /// Apply `edits` to the tree `root` (None = empty) and return the new root.
    /// Writes one data pack, one tree pack and one index file at most.
    ///
    /// # Errors
    ///
    /// * If a source file can't be read, or writing to the backend fails.
    pub fn splice(
        &self,
        root: Option<TreeId>,
        edits: impl IntoIterator<Item = (PathBuf, Edit)>,
    ) -> RusticResult<TreeId> {
        let mut group = Group::default();
        for (path, edit) in edits {
            group.insert(&path, edit)?;
        }
        let be = self.dbe();
        let index = self.index();
        let indexer = Indexer::new(be.clone()).into_shared();
        let sizer = |t| PackSizer::from_config(self.config(), t, index.total_size(t));
        let data = Packer::new(be.clone(), BlobType::Data, indexer.clone(), sizer(BlobType::Data))?;
        let trees = Packer::new(be.clone(), BlobType::Tree, indexer.clone(), sizer(BlobType::Tree))?;

        let splicer = Splicer {
            repo: self,
            data: &data,
            trees: &trees,
        };
        let new_root = match splicer.tree(root, group)? {
            Some(id) => id,
            None => splicer.save(&Tree::new())?,
        };
        _ = data.finalize()?;
        _ = trees.finalize()?;
        indexer.write().unwrap().finalize()?;
        Ok(new_root)
    }
}

struct Splicer<'a, S: IndexedIds> {
    repo: &'a Repository<S>,
    data: &'a Packer<crate::backend::decrypt::DecryptBackend<crate::crypto::aespoly1305::Key>>,
    trees: &'a Packer<crate::backend::decrypt::DecryptBackend<crate::crypto::aespoly1305::Key>>,
}

impl<S: IndexedIds> Splicer<'_, S> {
    /// Returns None when the resulting directory is empty.
    fn tree(&self, id: Option<TreeId>, group: Group) -> RusticResult<Option<TreeId>> {
        let mut nodes: BTreeMap<OsString, Node> = match id {
            Some(id) => Tree::from_backend(self.repo.dbe(), self.repo.index(), id)?
                .nodes
                .into_iter()
                .map(|n| (n.name().into_owned(), n))
                .collect(),
            None => BTreeMap::new(),
        };
        for (name, edit) in group.here {
            match edit {
                Edit::Delete => _ = nodes.remove(&name),
                Edit::Put { mut node, source } => {
                    if let Some(src) = source {
                        node.content = Some(self.store(&src, node.meta.size)?);
                    }
                    _ = nodes.insert(name, node);
                }
            }
        }
        for (name, sub) in group.sub {
            let existing = nodes.get(&name).filter(|n| n.is_dir()).cloned();
            match self.tree(existing.as_ref().and_then(|n| n.subtree), sub)? {
                None => _ = nodes.remove(&name),
                Some(sub_id) => {
                    let mut node = existing.unwrap_or_else(|| {
                        // Go's os.ModeDir | 0755, as restic stores directory modes
                        let meta = Metadata {
                            mode: Some(0x8000_0000 | 0o755),
                            // unset reads as Go's zero time in restic
                            mtime: Some(jiff::Timestamp::now()),
                            ..Metadata::default()
                        };
                        Node::new_node(&name, NodeType::Dir, meta)
                    });
                    node.subtree = Some(sub_id);
                    _ = nodes.insert(name, node);
                }
            }
        }
        if nodes.is_empty() {
            return Ok(None);
        }
        let mut tree = Tree::new();
        for node in nodes.into_values() {
            tree.add(node);
        }
        Ok(Some(self.save(&tree)?))
    }

    fn save(&self, tree: &Tree) -> RusticResult<TreeId> {
        let (chunk, id) = tree.serialize().map_err(|err| {
            RusticError::with_source(ErrorKind::Internal, "Failed to serialize tree.", err)
        })?;
        if !self.repo.index().has_tree(&id) {
            self.trees.add(chunk.into(), BlobId::from(*id))?;
        }
        Ok(id)
    }

    fn store(&self, path: &Path, size: u64) -> RusticResult<Vec<DataId>> {
        let file = File::open(path).map_err(|err| {
            RusticError::with_source(ErrorKind::InputOutput, "Failed to open `{path}`", err)
                .attach_context("path", path.display().to_string())
        })?;
        ChunkIter::from_config(
            self.repo.config(),
            file,
            usize::try_from(size).unwrap_or(usize::MAX),
        )?
        .map(|chunk| {
            let chunk = chunk?;
            let id = DataId::from(hash(&chunk));
            if !self.repo.index().has_data(&id) {
                self.data.add(chunk.into(), BlobId::from(*id))?;
            }
            Ok(id)
        })
        .collect()
    }
}
