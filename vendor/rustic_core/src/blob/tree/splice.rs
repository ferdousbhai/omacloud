//! Edit a tree at given paths without walking the rest of it.
//!
//! Only the trees on the paths from each edit to the root are read and
//! rewritten; every other subtree keeps its id. This makes the cost of a new
//! snapshot proportional to the change instead of the size of the tree, which
//! is what continuous sync needs.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    io::Read,
    path::{Component, Path, PathBuf},
};

use jiff::Timestamp;
use rayon::prelude::*;

use crate::{
    BlobId, DataId, ErrorKind, RusticError, RusticResult,
    backend::{
        decrypt::DecryptFullBackend,
        node::{Metadata, Node, NodeType},
    },
    blob::{
        BlobType,
        packer::{PackSizer, Packer},
        tree::{Tree, TreeId},
    },
    chunker::ChunkIter,
    crypto::hasher::hash,
    index::{ReadGlobalIndex, indexer::Indexer},
    repofile::ConfigFile,
};

/// Go's `os.ModeDir`, as restic stores directory modes.
const GO_MODE_DIR: u32 = 0x8000_0000;

/// One change to a tree, at a path relative to its root.
pub enum TreeEdit {
    /// Put this node as is. Its `content` or `subtree` must already be in
    /// the repository. A directory without a subtree means "this directory
    /// exists": an existing one keeps its contents, a new one starts empty,
    /// and it stays even if other edits leave it empty.
    Put(Node),
    /// Put this file node, with its content read, chunked and stored from
    /// `reader`.
    PutFile {
        /// The file node; its `content` is replaced.
        node: Node,
        /// Where to read the file's content from.
        reader: Box<dyn Read + Send>,
    },
    /// Remove whatever is at the path; a directory goes with its contents.
    Delete,
}

impl std::fmt::Debug for TreeEdit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Put(node) => f.debug_tuple("Put").field(node).finish(),
            Self::PutFile { node, .. } => f.debug_struct("PutFile").field("node", node).finish(),
            Self::Delete => f.write_str("Delete"),
        }
    }
}

/// The path decides the name, whatever name the node was built with.
fn named(node: Node, name: &OsStr) -> Node {
    let escaped = Node::new_node(name, NodeType::File, Metadata::default()).name;
    Node {
        name: escaped,
        ..node
    }
}

fn empty_tree_id() -> TreeId {
    Tree::new()
        .serialize()
        .map(|(_, id)| id)
        .expect("serializing an empty tree")
}

/// Edits grouped by directory.
#[derive(Default)]
struct Group {
    here: BTreeMap<OsString, TreeEdit>,
    sub: BTreeMap<OsString, Self>,
}

impl Group {
    fn insert(&mut self, path: &Path, edit: TreeEdit) -> RusticResult<()> {
        let mut names: Vec<&OsStr> = Vec::new();
        for c in path.components() {
            let Component::Normal(name) = c else {
                return Err(RusticError::new(
                    ErrorKind::InvalidInput,
                    "Tree edit path `{path}` must be relative, without `.` or `..`.",
                )
                .attach_context("path", path.display().to_string()));
            };
            names.push(name);
        }
        let Some((last, dirs)) = names.split_last() else {
            return Err(RusticError::new(
                ErrorKind::InvalidInput,
                "Tree edit path must not be empty.",
            ));
        };
        let mut group = self;
        for dir in dirs {
            group = group.sub.entry((*dir).to_owned()).or_default();
        }
        _ = group.here.insert((*last).to_owned(), edit);
        Ok(())
    }
}

/// Applies edits to a tree, writing new data and tree blobs as needed.
pub(crate) struct TreeSplicer<'a, BE: DecryptFullBackend, I: ReadGlobalIndex> {
    be: &'a BE,
    index: &'a I,
    config: &'a ConfigFile,
    indexer: crate::index::indexer::SharedIndexer<BE>,
    data: Packer<BE>,
    trees: Packer<BE>,
}

impl<'a, BE: DecryptFullBackend, I: ReadGlobalIndex> TreeSplicer<'a, BE, I> {
    pub(crate) fn new(be: &'a BE, index: &'a I, config: &'a ConfigFile) -> RusticResult<Self> {
        let indexer = Indexer::new(be.clone()).into_shared();
        let sizer = |tpe| PackSizer::from_config(config, tpe, index.total_size(tpe));
        let data = Packer::new(
            be.clone(),
            BlobType::Data,
            indexer.clone(),
            sizer(BlobType::Data),
        )?;
        let trees = Packer::new(
            be.clone(),
            BlobType::Tree,
            indexer.clone(),
            sizer(BlobType::Tree),
        )?;
        Ok(Self {
            be,
            index,
            config,
            indexer,
            data,
            trees,
        })
    }

    /// Applies `edits` to `root` (`None` is an empty tree) and returns the id
    /// of the new root. Writes at most one index file.
    pub(crate) fn splice(
        self,
        root: Option<TreeId>,
        edits: impl IntoIterator<Item = (impl AsRef<Path>, TreeEdit)>,
    ) -> RusticResult<TreeId> {
        // chunk and store file contents in parallel; the trees are built
        // sequentially afterwards
        let edits: Vec<(PathBuf, TreeEdit)> = edits
            .into_iter()
            .map(|(path, edit)| (path.as_ref().to_path_buf(), edit))
            .collect();
        let edits: Vec<(PathBuf, TreeEdit)> = edits
            .into_par_iter()
            .map(|(path, edit)| match edit {
                TreeEdit::PutFile { mut node, reader } => {
                    node.content = Some(self.store(reader, node.meta.size)?);
                    Ok((path, TreeEdit::Put(node)))
                }
                other => Ok((path, other)),
            })
            .collect::<RusticResult<_>>()?;
        let mut group = Group::default();
        for (path, edit) in edits {
            group.insert(&path, edit)?;
        }
        let new_root = match self.tree(root, group)? {
            Some(id) => id,
            None => self.save(&Tree::new())?,
        };
        _ = self.data.finalize()?;
        _ = self.trees.finalize()?;
        self.indexer.write().unwrap().finalize()?;
        Ok(new_root)
    }

    /// Returns `None` when the resulting directory is empty.
    fn tree(&self, id: Option<TreeId>, group: Group) -> RusticResult<Option<TreeId>> {
        // the empty tree may have been made in this very splice, so it isn't
        // in the index yet; there is nothing to read anyway
        let id = id.filter(|id| *id != empty_tree_id());
        let mut nodes: BTreeMap<OsString, Node> = match id {
            Some(id) => Tree::from_backend(self.be, self.index, id)?
                .nodes
                .into_iter()
                .map(|node| (node.name().into_owned(), node))
                .collect(),
            None => BTreeMap::new(),
        };
        // directories put explicitly stay, even when empty
        let mut kept = BTreeSet::new();
        for (name, edit) in group.here {
            match edit {
                TreeEdit::Delete => _ = nodes.remove(&name),
                TreeEdit::Put(mut node) => {
                    if node.is_dir() && node.subtree.is_none() {
                        // "this directory exists": an existing one keeps its
                        // contents, a new one starts empty
                        _ = kept.insert(name.clone());
                        match nodes.get(&name).filter(|n| n.is_dir()) {
                            Some(existing) => node.subtree = existing.subtree,
                            None => node.subtree = Some(self.save(&Tree::new())?),
                        }
                    } else if node.is_dir() {
                        _ = kept.insert(name.clone());
                    }
                    let node = named(node, &name);
                    _ = nodes.insert(name, node);
                }
                TreeEdit::PutFile { mut node, reader } => {
                    node.content = Some(self.store(reader, node.meta.size)?);
                    let node = named(node, &name);
                    _ = nodes.insert(name, node);
                }
            }
        }
        for (name, sub) in group.sub {
            let existing = nodes.get(&name).filter(|node| node.is_dir()).cloned();
            match self.tree(existing.as_ref().and_then(|node| node.subtree), sub)? {
                None if kept.contains(&name) => {
                    let empty = self.save(&Tree::new())?;
                    if let Some(node) = nodes.get_mut(&name) {
                        node.subtree = Some(empty);
                    }
                }
                None => _ = nodes.remove(&name),
                Some(subtree) => {
                    let mut node = existing.unwrap_or_else(|| {
                        let meta = Metadata {
                            mode: Some(GO_MODE_DIR | 0o755),
                            mtime: Some(Timestamp::now()),
                            ..Metadata::default()
                        };
                        Node::new_node(&name, NodeType::Dir, meta)
                    });
                    node.subtree = Some(subtree);
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
                .ask_report()
        })?;
        if !self.index.has_tree(&id) {
            self.trees.add(chunk.into(), BlobId::from(*id))?;
        }
        Ok(id)
    }

    fn store(&self, reader: Box<dyn Read + Send>, size: u64) -> RusticResult<Vec<DataId>> {
        ChunkIter::from_config(
            self.config,
            reader,
            usize::try_from(size).unwrap_or(usize::MAX),
        )?
        .map(|chunk| {
            let chunk = chunk?;
            let id = DataId::from(hash(&chunk));
            if !self.index.has_data(&id) {
                self.data.add(chunk.into(), BlobId::from(*id))?;
            }
            Ok(id)
        })
        .collect()
    }
}
