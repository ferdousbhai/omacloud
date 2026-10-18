//! A bucket on the network: objects by key, each kept as scrambled copies
//! on holders the account's computer chose, found again through its sealed
//! index entry.

use std::{
    collections::BTreeSet,
    ops::Range,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use iroh::Endpoint;
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};

use crate::{
    blob::NodeId,
    client,
    place::{self, PlaceError},
    proto::BlobId,
    registry::{Listed, Registry},
    seal::Keys,
};

/// One object's index entry, sealed at the Worker.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Entry {
    pub key: String,
    pub size: u64,
    pub holders: Vec<Held>,
}

/// A copy: who holds it, under which blob id.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Held {
    #[serde(with = "hex::serde")]
    pub node: NodeId,
    pub blob: BlobId,
}

/// How long a directory read is used before reading it again.
const DIRECTORY_FRESH: Duration = Duration::from_secs(60);

/// The account's own nodes, as known now.
pub type Own = Arc<std::sync::RwLock<BTreeSet<NodeId>>>;

pub struct Bucket {
    ep: Endpoint,
    registry: Arc<dyn Registry>,
    keys: Keys,
    /// This account's own nodes: never holders of its copies. Shared, as
    /// they're learned after the bucket is made.
    own: Own,
    copies: usize,
    directory: Mutex<Option<(Instant, Vec<Listed>)>>,
}

impl Bucket {
    #[must_use]
    pub fn new(ep: Endpoint, registry: Arc<dyn Registry>, keys: Keys, own: Own, copies: usize) -> Self {
        Self {
            ep,
            registry,
            keys,
            own,
            copies,
            directory: Mutex::new(None),
        }
    }

    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&dyn Registry) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let registry = self.registry.clone();
        tokio::task::spawn_blocking(move || f(registry.as_ref())).await?
    }

    async fn directory(&self, fresh: bool) -> Result<Vec<Listed>> {
        if !fresh
            && let Some((at, nodes)) = &*self.directory.lock().expect("directory")
            && at.elapsed() < DIRECTORY_FRESH
        {
            return Ok(nodes.clone());
        }
        let nodes = self.blocking(|r| r.nodes()).await?;
        *self.directory.lock().expect("directory") = Some((Instant::now(), nodes.clone()));
        Ok(nodes)
    }

    /// The entry for `key`, if there is one.
    ///
    /// # Errors
    ///
    /// If the Worker can't be reached, or the entry doesn't verify.
    pub async fn entry(&self, key: &str) -> Result<Option<Entry>> {
        let (scope, id) = (self.keys.scope.clone(), self.keys.entry_id(key));
        let sealed = {
            let id = id.clone();
            self.blocking(move |r| r.entry(&scope, &id)).await?
        };
        let Some(sealed) = sealed else { return Ok(None) };
        let entry: Entry = serde_json::from_slice(&self.keys.open(&id, &sealed)?)?;
        anyhow::ensure!(entry.key == key, "the index answered for another object");
        Ok(Some(entry))
    }

    async fn put_entry(&self, entry: &Entry) -> Result<()> {
        let (scope, id) = (self.keys.scope.clone(), self.keys.entry_id(&entry.key));
        let sealed = self.keys.seal(&id, &serde_json::to_vec(entry)?);
        self.blocking(move |r| r.put_entry(&scope, &id, &sealed)).await
    }

    /// Keep `data` under `key`: a copy on each of `copies` holders on
    /// different networks, or as many as the network has room for, at
    /// least one.
    ///
    /// # Errors
    ///
    /// If no holder took a copy, or the index can't be written.
    pub async fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        let holders = if data.is_empty() {
            Vec::new()
        } else {
            let holders = self.place(key, data, Vec::new(), &BTreeSet::new()).await?;
            anyhow::ensure!(!holders.is_empty(), "storing {key}: the network has no node with room");
            holders
        };
        self.put_entry(&Entry {
            key: key.to_string(),
            size: data.len() as u64,
            holders,
        })
        .await
    }

    /// Add copies of `data` next to `held` until there are `copies`,
    /// never on a node in `avoid`. Returns all the copies; fewer than
    /// wanted when the network is short of independent nodes.
    async fn place(&self, key: &str, data: &[u8], mut held: Vec<Held>, avoid: &BTreeSet<NodeId>) -> Result<Vec<Held>> {
        let nodes = self.directory(false).await?;
        let mut refused: BTreeSet<NodeId> = self.own.read().expect("own").union(avoid).copied().collect();
        refused.extend(held.iter().map(|h| h.node));
        while held.len() < self.copies {
            let taken: Vec<_> = nodes
                .iter()
                .filter(|n| held.iter().any(|h| h.node == n.node.id))
                .map(|n| n.node.clone())
                .collect();
            let all: Vec<_> = nodes.iter().map(|n| n.node.clone()).collect();
            let chosen = match place::choose(&all, &refused, &taken, self.copies - held.len(), 0) {
                Ok(c) => c,
                // fewer copies than wanted, on as many networks as there are
                Err(PlaceError::TooFew { found, .. }) => {
                    place::choose(&all, &refused, &taken, found, 0).unwrap_or_default()
                }
            };
            if chosen.is_empty() {
                break;
            }
            for node in chosen {
                let addr = &nodes.iter().find(|n| n.node.id == node.id).expect("listed").addr;
                let mut copy = data.to_vec();
                self.keys.blob.apply(key, &node.id, &mut copy);
                match client::store(&self.ep, addr, &copy).await {
                    Ok(blob) => held.push(Held { node: node.id, blob }),
                    Err(e) => log::debug!("{e:#}"),
                }
                refused.insert(node.id);
            }
        }
        Ok(held)
    }

    /// Drop the copies on `gone` and add new ones from a good copy until
    /// there are enough, never on a node in `avoid` (those that just
    /// failed, for any object). True when the entry changed.
    ///
    /// # Errors
    ///
    /// If no good copy can be read, or the index can't be written.
    pub async fn repair(&self, entry: &Entry, gone: &BTreeSet<NodeId>, avoid: &BTreeSet<NodeId>) -> Result<bool> {
        let good: Vec<Held> = entry
            .holders
            .iter()
            .filter(|h| !gone.contains(&h.node))
            .cloned()
            .collect();
        if good.len() >= self.copies && good.len() == entry.holders.len() {
            return Ok(false);
        }
        let from = Entry {
            holders: good.clone(),
            ..entry.clone()
        };
        // a holder that stopped keeping a copy may still have its bytes
        let data = match self.read(&from, None).await {
            Ok(data) => data,
            Err(e) if good.len() < entry.holders.len() => {
                log::debug!("{e:#}");
                self.read(entry, None).await?
            }
            Err(e) => return Err(e),
        };
        let avoid: BTreeSet<NodeId> = gone.union(avoid).copied().collect();
        let holders = self.place(&entry.key, &data, good, &avoid).await?;
        if holders == entry.holders {
            return Ok(false);
        }
        self.put_entry(&Entry {
            holders,
            ..entry.clone()
        })
        .await?;
        Ok(true)
    }

    /// The node directory, read again.
    ///
    /// # Errors
    ///
    /// If the Worker can't be reached.
    pub async fn nodes(&self) -> Result<Vec<Listed>> {
        self.directory(true).await
    }

    #[must_use]
    pub fn endpoint(&self) -> &Endpoint {
        &self.ep
    }

    /// How many copies each object should have.
    #[must_use]
    pub fn copies(&self) -> usize {
        self.copies
    }

    /// The object under `key`, all of it or `range` (which must lie
    /// inside it), from the first holder that has it.
    ///
    /// # Errors
    ///
    /// If no holder serves it.
    pub async fn get(&self, key: &str, range: Option<Range<u64>>) -> Result<Option<Vec<u8>>> {
        let Some(entry) = self.entry(key).await? else {
            return Ok(None);
        };
        if entry.size == 0 || range.as_ref().is_some_and(|r| r.is_empty()) {
            return Ok(Some(Vec::new()));
        }
        Ok(Some(self.read(&entry, range).await?))
    }

    /// Read an entry's object from its holders.
    ///
    /// # Errors
    ///
    /// If no holder serves it.
    pub async fn read(&self, entry: &Entry, range: Option<Range<u64>>) -> Result<Vec<u8>> {
        let mut holders = entry.holders.clone();
        holders.shuffle(&mut rand::thread_rng());
        let mut last_error = None;
        for fresh in [false, true] {
            let nodes = self.directory(fresh).await?;
            for h in &holders {
                let Some(listed) = nodes.iter().find(|n| n.node.id == h.node) else {
                    continue;
                };
                match client::read(&self.ep, &listed.addr, h.blob, range.clone()).await {
                    Ok(mut data) => {
                        let offset = range.as_ref().map_or(0, |r| r.start);
                        self.keys.blob.apply_at(&entry.key, &h.node, offset, &mut data);
                        return Ok(data);
                    }
                    Err(e) => {
                        log::debug!("{e:#}");
                        last_error = Some(e);
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("none of its holders is in the directory")))
            .with_context(|| format!("reading {}", entry.key))
    }

    /// Every object whose key starts with `prefix`, sorted by key.
    ///
    /// # Errors
    ///
    /// If the Worker can't be reached, or an entry doesn't verify.
    pub async fn list(&self, prefix: &str) -> Result<Vec<Entry>> {
        let scope = self.keys.scope.clone();
        let index = self.blocking(move |r| r.index(&scope)).await?;
        let mut out = Vec::new();
        for (id, sealed) in index {
            let entry: Entry = serde_json::from_slice(&self.keys.open(&id, &sealed)?)?;
            anyhow::ensure!(
                self.keys.entry_id(&entry.key) == id,
                "an index entry filed under another name"
            );
            if entry.key.starts_with(prefix) {
                out.push(entry);
            }
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    /// Forget `key`. Its holders drop their copies once nobody renews them.
    ///
    /// # Errors
    ///
    /// If the Worker can't be reached.
    pub async fn delete(&self, key: &str) -> Result<()> {
        let (scope, id) = (self.keys.scope.clone(), self.keys.entry_id(key));
        self.blocking(move |r| r.delete_entry(&scope, &id)).await
    }
}
