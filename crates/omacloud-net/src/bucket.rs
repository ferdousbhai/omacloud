//! A bucket on the network: objects by key, each kept as scrambled copies
//! (or, when large, erasure-coded shards) on holders the account's computer
//! chose, found again through its sealed index entry.

use std::{
    collections::{BTreeMap, BTreeSet},
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
    erasure::{self, Coded},
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
    /// How it was coded, when its holders keep shards, not whole copies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coded: Option<Coded>,
}

impl Entry {
    /// Bytes each holder keeps.
    #[must_use]
    pub fn blob_size(&self) -> u64 {
        self.coded.map_or(self.size, |c| c.shard)
    }

    /// Holders it should have: `copies`, or one per shard.
    #[must_use]
    pub fn wanted(&self, copies: usize) -> usize {
        self.coded.map_or(copies, |c| c.total())
    }

    /// Holders it can't be read with fewer of.
    #[must_use]
    pub fn needed(&self) -> usize {
        self.coded.map_or(1, |c| usize::from(c.data))
    }
}

/// A copy, or a shard: who holds it, under which blob id.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Held {
    #[serde(with = "hex::serde")]
    pub node: NodeId,
    pub blob: BlobId,
    /// Which shard, for a coded object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shard: Option<u8>,
}

/// What a holder's blob is scrambled under: each shard its own name, so
/// two shards never share a key stream.
fn blob_name(key: &str, shard: Option<u8>) -> String {
    match shard {
        Some(i) => format!("{key}#{i}"),
        None => key.to_string(),
    }
}

/// How long a directory read is used before reading it again.
const DIRECTORY_FRESH: Duration = Duration::from_secs(60);
/// How long a holder that didn't answer is asked last.
const DOWN_FOR: Duration = Duration::from_secs(300);

/// The account's own nodes, as known now.
pub type Own = Arc<std::sync::RwLock<BTreeSet<NodeId>>>;

pub struct Bucket {
    ep: Endpoint,
    registry: Arc<dyn Registry>,
    keys: Keys,
    /// This account's own nodes: never holders of its copies. Shared, as
    /// they're learned after the bucket is made.
    own: Own,
    /// Holders that failed this computer lately, never chosen again.
    avoid: Own,
    copies: usize,
    directory: Mutex<Option<(Instant, Vec<Listed>)>>,
    /// Holders that didn't answer lately, and when.
    down: Mutex<BTreeMap<NodeId, Instant>>,
}

impl Bucket {
    #[must_use]
    pub fn new(ep: Endpoint, registry: Arc<dyn Registry>, keys: Keys, own: Own, copies: usize) -> Self {
        Self {
            ep,
            registry,
            keys,
            own,
            avoid: Own::default(),
            copies,
            directory: Mutex::new(None),
            down: Mutex::new(BTreeMap::new()),
        }
    }

    /// Never choose the holders in `avoid` (shared, as it changes).
    #[must_use]
    pub fn avoiding(mut self, avoid: Own) -> Self {
        self.avoid = avoid;
        self
    }

    /// Who may not hold a new copy: the account's own nodes, those that
    /// failed lately, and `also`.
    fn refused(&self, also: &BTreeSet<NodeId>) -> BTreeSet<NodeId> {
        let mut r: BTreeSet<NodeId> = self.own.read().expect("own").union(also).copied().collect();
        r.extend(self.avoid.read().expect("avoid").iter().copied());
        r
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
        if data.len() as u64 >= erasure::MIN {
            let coded = Coded::for_size(data.len() as u64);
            let shards = coded.split(data)?.into_iter().enumerate();
            let shards: Vec<(u8, Vec<u8>)> = shards.map(|(i, s)| (i as u8, s)).collect();
            // coded only where there are holders for every shard on as many
            // networks; else (a small network) whole copies
            let holders = self.place_shards(key, shards, Vec::new(), &BTreeSet::new()).await?;
            if holders.len() == coded.total() {
                return self
                    .put_entry(&Entry {
                        key: key.to_string(),
                        size: data.len() as u64,
                        holders,
                        coded: Some(coded),
                    })
                    .await;
            }
            // the shards placed are never paid for: they go after the first lease
        }
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
            coded: None,
        })
        .await
    }

    /// Give each of `shards` (index, bytes) a holder on a network none of
    /// `held` or the others is on, never a node in `avoid`. Returns `held`
    /// with the new ones; short of some when the network is.
    async fn place_shards(
        &self,
        key: &str,
        mut shards: Vec<(u8, Vec<u8>)>,
        mut held: Vec<Held>,
        avoid: &BTreeSet<NodeId>,
    ) -> Result<Vec<Held>> {
        let nodes = self.directory(false).await?;
        let all: Vec<_> = nodes.iter().map(|n| n.node.clone()).collect();
        let mut refused = self.refused(avoid);
        refused.extend(held.iter().map(|h| h.node));
        // all or none of a new object's shards: there's no use for some
        if held.is_empty() && place::choose(&all, &refused, &[], shards.len(), 0).is_err() {
            return Ok(held);
        }
        while !shards.is_empty() {
            let taken: Vec<_> = nodes
                .iter()
                .filter(|n| held.iter().any(|h| h.node == n.node.id))
                .map(|n| n.node.clone())
                .collect();
            let chosen = match place::choose(&all, &refused, &taken, shards.len(), 0) {
                Ok(c) => c,
                Err(PlaceError::TooFew { found, .. }) => {
                    place::choose(&all, &refused, &taken, found, 0).unwrap_or_default()
                }
            };
            if chosen.is_empty() {
                break;
            }
            let mut left = Vec::new();
            for ((i, shard), node) in shards
                .drain(..)
                .zip(chosen.iter().map(Some).chain(std::iter::repeat(None)))
            {
                let Some(node) = node else {
                    left.push((i, shard));
                    continue;
                };
                let addr = &nodes.iter().find(|n| n.node.id == node.id).expect("listed").addr;
                let mut blob = shard.clone();
                self.keys.blob.apply(&blob_name(key, Some(i)), &node.id, &mut blob);
                match client::store(&self.ep, addr, &blob).await {
                    Ok(id) => held.push(Held {
                        node: node.id,
                        blob: id,
                        shard: Some(i),
                    }),
                    Err(e) => {
                        log::debug!("{e:#}");
                        left.push((i, shard));
                    }
                }
                refused.insert(node.id);
            }
            shards = left;
        }
        held.sort_by_key(|h| h.shard);
        Ok(held)
    }

    /// Add copies of `data` next to `held` until there are `copies`,
    /// never on a node in `avoid`. Returns all the copies; fewer than
    /// wanted when the network is short of independent nodes.
    async fn place(&self, key: &str, data: &[u8], mut held: Vec<Held>, avoid: &BTreeSet<NodeId>) -> Result<Vec<Held>> {
        let nodes = self.directory(false).await?;
        let mut refused = self.refused(avoid);
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
                    Ok(blob) => held.push(Held {
                        node: node.id,
                        blob,
                        shard: None,
                    }),
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
        if let Some(coded) = entry.coded {
            if good.len() >= coded.total() && good.len() == entry.holders.len() {
                return Ok(false);
            }
            let have = self.read_shards(entry, &good).await?;
            let all = coded.rebuild(&have)?;
            let kept: BTreeSet<u8> = good.iter().filter_map(|h| h.shard).collect();
            let missing: Vec<(u8, Vec<u8>)> = all
                .into_iter()
                .enumerate()
                .map(|(i, s)| (i as u8, s))
                .filter(|(i, _)| !kept.contains(i))
                .collect();
            let avoid: BTreeSet<NodeId> = gone.union(avoid).copied().collect();
            let holders = self.place_shards(&entry.key, missing, good, &avoid).await?;
            if holders == entry.holders {
                return Ok(false);
            }
            self.put_entry(&Entry {
                holders,
                ..entry.clone()
            })
            .await?;
            return Ok(true);
        }
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
    /// If no holder (or, coded, too few) serves it.
    pub async fn read(&self, entry: &Entry, range: Option<Range<u64>>) -> Result<Vec<u8>> {
        match entry.coded {
            Some(coded) => self.read_coded(entry, coded, range.unwrap_or(0..entry.size)).await,
            None => self.read_copy(entry, range).await,
        }
        .with_context(|| format!("reading {}", entry.key))
    }

    fn is_down(&self, node: &NodeId) -> bool {
        self.down
            .lock()
            .expect("down")
            .get(node)
            .is_some_and(|t| t.elapsed() < DOWN_FOR)
    }

    /// `holders` shuffled, those that didn't answer lately last.
    fn in_order(&self, holders: &[Held]) -> Vec<Held> {
        let mut holders = holders.to_vec();
        holders.shuffle(&mut rand::thread_rng());
        holders.sort_by_key(|h| self.is_down(&h.node));
        holders
    }

    async fn read_copy(&self, entry: &Entry, range: Option<Range<u64>>) -> Result<Vec<u8>> {
        let holders = self.in_order(&entry.holders);
        let mut last_error = None;
        for h in &holders {
            match self.read_held(&entry.key, h, range.clone()).await {
                Ok(data) => return Ok(data),
                Err(e) => {
                    log::debug!("{e:#}");
                    last_error = Some(e);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("it has no holders")))
    }

    /// A range of a coded object: from the data shards it falls in when
    /// their holders answer, else from any four whole shards.
    async fn read_coded(&self, entry: &Entry, coded: Coded, range: Range<u64>) -> Result<Vec<u8>> {
        let s = coded.shard;
        let mut out = Vec::with_capacity(usize::try_from(range.end - range.start)?);
        let mut fast = true;
        for i in range.start / s..range.end.div_ceil(s) {
            let shard = u8::try_from(i)?;
            let part = range.start.max(i * s) - i * s..range.end.min((i + 1) * s) - i * s;
            let Some(h) = entry
                .holders
                .iter()
                .find(|h| h.shard == Some(shard) && !self.is_down(&h.node))
            else {
                fast = false;
                break;
            };
            match self.read_held(&entry.key, h, Some(part)).await {
                Ok(data) => out.extend(data),
                Err(e) => {
                    log::debug!("{e:#}");
                    fast = false;
                    break;
                }
            }
        }
        if fast {
            return Ok(out);
        }
        let have = self.read_shards(entry, &entry.holders).await?;
        let object = coded.join(entry.size, &have)?;
        Ok(object[usize::try_from(range.start)?..usize::try_from(range.end)?].to_vec())
    }

    /// Whole shards from `holders`, asked all at once, until there are
    /// enough to rebuild: one that's gone costs no more than its timeout.
    async fn read_shards(&self, entry: &Entry, holders: &[Held]) -> Result<BTreeMap<usize, Vec<u8>>> {
        use futures_util::stream::{FuturesUnordered, StreamExt};
        let needed = entry.needed();
        let mut reads: FuturesUnordered<_> = self
            .in_order(holders)
            .into_iter()
            .filter_map(|h| {
                let i = h.shard?;
                Some(async move { (i, self.read_held(&entry.key, &h, None).await) })
            })
            .collect();
        let mut have = BTreeMap::new();
        while have.len() < needed
            && let Some((i, read)) = reads.next().await
        {
            match read {
                Ok(data) => _ = have.insert(usize::from(i), data),
                Err(e) => log::debug!("{e:#}"),
            }
        }
        anyhow::ensure!(have.len() >= needed, "{} of {needed} shards could be read", have.len());
        Ok(have)
    }

    /// One holder's copy or shard, all of it or `range`, unscrambled.
    async fn read_held(&self, key: &str, h: &Held, range: Option<Range<u64>>) -> Result<Vec<u8>> {
        let mut last_error = None;
        let mut tried = None;
        for fresh in [false, true] {
            let nodes = self.directory(fresh).await?;
            let Some(listed) = nodes.iter().find(|n| n.node.id == h.node) else {
                continue;
            };
            // asked again only where the directory says it moved
            if tried.as_ref() == Some(&listed.addr) {
                break;
            }
            tried = Some(listed.addr.clone());
            match client::read(&self.ep, &listed.addr, h.blob, range.clone()).await {
                Ok(mut data) => {
                    _ = self.down.lock().expect("down").remove(&h.node);
                    let offset = range.as_ref().map_or(0, |r| r.start);
                    self.keys
                        .blob
                        .apply_at(&blob_name(key, h.shard), &h.node, offset, &mut data);
                    return Ok(data);
                }
                Err(e) => last_error = Some(e),
            }
        }
        _ = self.down.lock().expect("down").insert(h.node, Instant::now());
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("{} isn't in the directory", hex::encode(&h.node[..4]))))
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
