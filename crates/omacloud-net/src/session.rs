//! The network for one run of omacloud: a runtime, the gateway the sync
//! engine talks S3 to, and the way to the Worker. Blocking callers (the
//! CLI, the daemon) use it from outside the runtime.
//!
//! Two identities: an account's computer reaches holders from a fresh,
//! throwaway key each run, so a holder can't tie the blobs it's given to
//! the node key this computer serves its own disk under.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};
pub use iroh::SecretKey;
use iroh::{Endpoint, RelayMap, RelayMode, RelayUrl, endpoint::presets};
use omacloud_core::RepoSpec;

use crate::{
    blob::NodeId,
    coord::{Account, NetworkCoordinator},
    credit::{IssuerKey, Token},
    dirstore::DirStore,
    gateway::{BUCKET, Gateway},
    mirror::Mirror,
    node::{Config, Node},
    records::Records,
    registry::{Listed, Registry},
    upkeep::{self, Report},
    worker::Worker,
};

/// Where the Worker's part lives: the Worker, or a directory (`dir:<path>`)
/// for a network on one machine.
enum Place {
    Worker(Worker),
    Dir(DirStore),
}

/// The Worker's part of the network, and this computer's copy of what it
/// read and wrote there ([`Mirror`]), which reads fall back to when the
/// Worker is out of reach.
pub struct Network {
    place: Place,
    mirror: Option<Mirror>,
}

impl Network {
    /// # Errors
    ///
    /// If a `dir:` network's directory, or the mirror, can't be opened.
    pub fn open(url: &str, mirror: Option<&Path>) -> Result<Self> {
        Ok(Self {
            place: match url.strip_prefix("dir:") {
                Some(path) => Place::Dir(DirStore::new(path)?),
                None => Place::Worker(Worker::new(url)),
            },
            mirror: mirror.map(Mirror::open).transpose()?,
        })
    }

    /// Say this node is up, where, with how much room.
    ///
    /// # Errors
    ///
    /// If the Worker can't be reached or refuses.
    pub fn heartbeat(&self, key: &SecretKey, node: &Node, capacity: u64) -> Result<()> {
        match &self.place {
            Place::Worker(w) => w.heartbeat(key, &node.addr(), capacity).map(|_| ()),
            Place::Dir(d) => d.list_node(&node.id(), &node.addr(), capacity),
        }
    }

    /// The token key, when storage is paid for (not on a `dir:` network).
    ///
    /// # Errors
    ///
    /// If the Worker can't be reached.
    pub fn issuer(&self) -> Result<Option<IssuerKey>> {
        match &self.place {
            Place::Worker(w) => w.issuer().map(Some),
            Place::Dir(_) => Ok(None),
        }
    }

    /// The starter grant: tokens for a computer that holds none yet.
    ///
    /// # Errors
    ///
    /// If the grant for this network is used up.
    pub fn starter(&self, n: usize) -> Result<Vec<Token>> {
        match &self.place {
            Place::Worker(w) => w.starter(&w.issuer()?, n),
            Place::Dir(_) => Ok(Vec::new()),
        }
    }

    /// Trade tokens taken as a holder for fresh ones.
    ///
    /// # Errors
    ///
    /// If the Worker refuses them.
    pub fn exchange(&self, tokens: &[Token]) -> Result<Vec<Token>> {
        match &self.place {
            Place::Worker(w) => w.exchange(&w.issuer()?, tokens),
            Place::Dir(_) => Ok(Vec::new()),
        }
    }

    /// This computer's copy, when it keeps one.
    #[must_use]
    pub fn mirror(&self) -> Option<&Mirror> {
        self.mirror.as_ref()
    }

    /// Put this computer's copy back where the Worker lacks it: the
    /// account's records (made with `cap` if the account is gone), then
    /// the index entries missing from the buckets of `scopes` (the current
    /// epoch's, and live shares': older ones are let go). Nothing already there is
    /// changed: logs and the anchor are create-only. Returns how many
    /// records and entries were put back.
    ///
    /// # Errors
    ///
    /// If there's no mirror, or the Worker can't be reached or refuses.
    pub fn republish(&self, account: &str, cap: &str, scopes: &[String]) -> Result<(u64, u64)> {
        let m = self
            .mirror
            .as_ref()
            .context("this computer keeps no copy to republish")?;
        let (records, registry) = (self.records(), self.registry());
        records.create_account(account, cap)?;
        let mut put = 0;
        for log in ["devices", "epochs", "heads"] {
            for (seq, body) in m.log(account, log, 0)? {
                put += u64::from(records.append(account, cap, log, seq, &body)?);
            }
        }
        if let Some(body) = m.anchor(account)? {
            put += u64::from(records.set_anchor(account, cap, &body)?);
        }
        for kind in ["grants", "nodes"] {
            let there: BTreeSet<String> = records.named(account, kind, "")?.into_iter().map(|(n, _)| n).collect();
            for (name, body) in m.named(account, kind, "")? {
                if !there.contains(&name) {
                    records.put_named(account, Some(cap), kind, &name, &body)?;
                    put += 1;
                }
            }
        }
        let mut entries = 0;
        for scope in scopes {
            let there: BTreeSet<String> = registry.index(scope)?.into_iter().map(|(id, _)| id).collect();
            for (id, sealed) in m.index(scope)? {
                if !there.contains(&id) {
                    registry.put_entry(scope, &id, &sealed)?;
                    entries += 1;
                }
            }
        }
        Ok((put, entries))
    }

    fn registry(&self) -> &dyn Registry {
        match &self.place {
            Place::Worker(w) => w,
            Place::Dir(d) => d,
        }
    }

    fn records(&self) -> &dyn Records {
        match &self.place {
            Place::Worker(w) => w,
            Place::Dir(d) => d,
        }
    }

    /// Keep what the Worker answered in the mirror; a mirror that can't be
    /// written only logs.
    fn keep(&self, f: impl FnOnce(&Mirror) -> Result<()>) {
        if let Some(m) = &self.mirror
            && let Err(e) = f(m)
        {
            log::warn!("keeping a copy of the network's records: {e:#}");
        }
    }

    /// The Worker's answer, or, when it can't be had, the mirror's.
    fn or_kept<T>(&self, live: Result<T>, kept: impl FnOnce(&Mirror) -> Result<Option<T>>) -> Result<T> {
        let err = match live {
            Ok(v) => return Ok(v),
            Err(e) => e,
        };
        match self.mirror.as_ref().map(kept).transpose()?.flatten() {
            Some(v) => {
                log::warn!("the Worker didn't answer ({err:#}); using this computer's copy");
                Ok(v)
            }
            None => Err(err),
        }
    }
}

impl Registry for Network {
    fn nodes(&self) -> Result<Vec<Listed>> {
        let live = self.registry().nodes();
        if let Ok(nodes) = &live {
            self.keep(|m| m.set_nodes(&serde_json::to_string(nodes)?));
        }
        self.or_kept(live, |m| {
            m.nodes()?
                .map(|j| serde_json::from_str(&j).map_err(Into::into))
                .transpose()
        })
    }
    fn index(&self, scope: &str) -> Result<Vec<(String, Vec<u8>)>> {
        let live = self.registry().index(scope);
        if let Ok(entries) = &live {
            self.keep(|m| m.set_index(scope, entries));
        }
        self.or_kept(live, |m| m.index(scope).map(Some))
    }
    fn entry(&self, scope: &str, id: &str) -> Result<Option<Vec<u8>>> {
        let live = self.registry().entry(scope, id);
        // kept when found; one the Worker says it hasn't stays here, as the
        // Worker may have lost it
        if let Ok(Some(sealed)) = &live {
            self.keep(|m| m.put_entry(scope, id, Some(sealed)));
        }
        self.or_kept(live, |m| m.entry(scope, id).map(|e| e.map(Some)))
    }
    fn put_entry(&self, scope: &str, id: &str, sealed: &[u8]) -> Result<()> {
        self.registry().put_entry(scope, id, sealed)?;
        self.keep(|m| m.put_entry(scope, id, Some(sealed)));
        Ok(())
    }
    fn delete_entry(&self, scope: &str, id: &str) -> Result<()> {
        self.registry().delete_entry(scope, id)?;
        self.keep(|m| m.put_entry(scope, id, None));
        Ok(())
    }
}

impl Records for Network {
    fn create_account(&self, account: &str, cap: &str) -> Result<bool> {
        self.records().create_account(account, cap)
    }
    fn replace_cap(&self, account: &str, cap: &str, new: &str) -> Result<()> {
        self.records().replace_cap(account, cap, new)
    }
    fn log(&self, account: &str, log: &str, after: u64) -> Result<Vec<(u64, String)>> {
        let live = self.records().log(account, log, after);
        if let Ok(records) = &live {
            self.keep(|m| m.add_log(account, log, records));
        }
        self.or_kept(live, |m| m.log(account, log, after).map(Some))
    }
    fn append(&self, account: &str, cap: &str, log: &str, seq: u64, body: &str) -> Result<bool> {
        let added = self.records().append(account, cap, log, seq, body)?;
        if added {
            self.keep(|m| m.add_log(account, log, &[(seq, body.to_string())]));
        }
        Ok(added)
    }
    fn anchor(&self, account: &str) -> Result<Option<String>> {
        let live = self.records().anchor(account);
        if let Ok(Some(body)) = &live {
            self.keep(|m| m.set_anchor(account, body));
        }
        self.or_kept(live, |m| m.anchor(account).map(|a| a.map(Some)))
    }
    fn set_anchor(&self, account: &str, cap: &str, body: &str) -> Result<bool> {
        let set = self.records().set_anchor(account, cap, body)?;
        if set {
            self.keep(|m| m.set_anchor(account, body));
        }
        Ok(set)
    }
    fn named(&self, account: &str, kind: &str, prefix: &str) -> Result<Vec<(String, String)>> {
        let live = self.records().named(account, kind, prefix);
        if let Ok(records) = &live {
            self.keep(|m| m.set_named(account, kind, prefix, records));
        }
        self.or_kept(live, |m| m.named(account, kind, prefix).map(Some))
    }
    fn put_named(&self, account: &str, cap: Option<&str>, kind: &str, name: &str, body: &str) -> Result<()> {
        self.records().put_named(account, cap, kind, name, body)?;
        self.keep(|m| m.put_named(account, kind, name, Some(body)));
        Ok(())
    }
    fn delete_named(&self, account: &str, cap: &str, kind: &str, name: &str) -> Result<()> {
        self.records().delete_named(account, cap, kind, name)?;
        self.keep(|m| m.put_named(account, kind, name, None));
        Ok(())
    }
    fn marker(&self, account: &str) -> Result<u64> {
        self.records().marker(account)
    }
}

/// How a session reaches the network.
pub struct Options {
    /// The Worker's URL, or `dir:<path>`.
    pub url: String,
    /// Relay servers; none means direct connections only.
    pub relays: Vec<String>,
    /// Copies of each object.
    pub copies: usize,
    /// This computer's own node, never a holder of its copies.
    pub own: Option<NodeId>,
    /// Where this computer keeps its copy of the Worker's records, if it
    /// keeps one (an account's computers do; a seed doesn't).
    pub mirror: Option<PathBuf>,
}

pub struct Session {
    rt: tokio::runtime::Runtime,
    network: Arc<Network>,
    relays: Vec<String>,
    gateway: Gateway,
}

async fn endpoint(relays: &[String], key: Option<SecretKey>) -> Result<Endpoint> {
    let mode = if relays.is_empty() {
        RelayMode::Disabled
    } else {
        let urls: Vec<RelayUrl> = relays
            .iter()
            .map(|r| r.parse().with_context(|| format!("relay {r}")))
            .collect::<Result<_>>()?;
        RelayMode::Custom(urls.into_iter().collect::<RelayMap>())
    };
    let mut b = Endpoint::builder(presets::Minimal).relay_mode(mode);
    match key {
        Some(key) => b = b.secret_key(key),
        // reaching holders through relays only, when there are relays: a
        // holder then never learns this computer's address
        None if !relays.is_empty() => b = b.clear_ip_transports(),
        None => {}
    }
    Ok(b.bind().await?)
}

impl Session {
    /// # Errors
    ///
    /// If the runtime, endpoint or gateway can't start.
    pub fn start(opts: Options) -> Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("omacloud-net")
            .build()?;
        let network = Arc::new(Network::open(&opts.url, opts.mirror.as_deref())?);
        let own: BTreeSet<NodeId> = opts.own.into_iter().collect();
        let gateway = rt.block_on(async {
            let client = endpoint(&opts.relays, None).await?;
            Gateway::start(client, network.clone(), own, opts.copies).await
        })?;
        Ok(Self {
            rt,
            network,
            relays: opts.relays,
            gateway,
        })
    }

    #[must_use]
    pub fn network(&self) -> Arc<Network> {
        self.network.clone()
    }

    /// The repository spec that reaches the account's bucket through the
    /// gateway; the sync engine adds each epoch's key.
    #[must_use]
    pub fn repo(&self, root: &str) -> RepoSpec {
        RepoSpec {
            repository: "opendal:s3".into(),
            options: [
                ("endpoint", self.gateway.endpoint()),
                ("bucket", BUCKET.to_string()),
                ("region", "auto".to_string()),
                ("root", root.to_string()),
                (omacloud_core::repo::NETWORK, "1".to_string()),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
        }
    }

    #[must_use]
    pub fn coordinator(&self, account: Account) -> NetworkCoordinator {
        NetworkCoordinator::new(self.network.clone(), account, None)
    }

    /// Serve this computer's disk as a node under `key`, keeping blobs in
    /// `dir`, up to `capacity` bytes, and list it.
    ///
    /// # Errors
    ///
    /// If the node can't start or the Worker can't be reached.
    pub fn serve(&self, key: SecretKey, dir: PathBuf, capacity: u64) -> Result<Node> {
        let issuer = self.network.issuer()?;
        let node = self.rt.block_on(async {
            let ep = endpoint(&self.relays, Some(key.clone())).await?;
            // listed with its relay, so computers behind routers can reach
            // it: give the relay a moment to answer first
            if !self.relays.is_empty() {
                _ = tokio::time::timeout(std::time::Duration::from_secs(15), ep.online()).await;
            }
            Node::spawn(ep, Config { dir, capacity, issuer }).await
        })?;
        self.network.heartbeat(&key, &node, capacity)?;
        Ok(node)
    }

    /// Share `data` as a file named `name`: the secret its link carries.
    ///
    /// # Errors
    ///
    /// If no holder takes it.
    pub fn share(&self, name: &str, data: &[u8]) -> Result<String> {
        let secret = crate::share::secret();
        let bucket = self.gateway.bucket(&crate::share::bucket_key(&secret));
        self.rt.block_on(crate::share::put(&bucket, &secret, name, data))?;
        Ok(secret)
    }

    /// The file shared under `secret`: its name and contents.
    ///
    /// # Errors
    ///
    /// If it's gone, can't be read, or doesn't verify.
    pub fn receive(&self, secret: &str) -> Result<(String, Vec<u8>)> {
        let bucket = self.gateway.bucket(&crate::share::bucket_key(secret));
        self.rt.block_on(crate::share::get(&bucket, secret))
    }

    /// Take back the share under `secret`.
    ///
    /// # Errors
    ///
    /// If it's gone already, or the Worker can't be reached.
    pub fn unshare(&self, secret: &str) -> Result<()> {
        let bucket = self.gateway.bucket(&crate::share::bucket_key(secret));
        self.rt.block_on(crate::share::remove(&bucket))
    }

    /// Never choose these holders (failed this computer lately) for new copies.
    pub fn set_avoid(&self, avoid: BTreeSet<NodeId>) {
        self.gateway.set_avoid(avoid);
    }

    /// Never choose these nodes (the account's own) as holders.
    pub fn set_own(&self, own: BTreeSet<NodeId>) {
        self.gateway.set_own(own);
    }

    /// Run `f` on the session's runtime.
    pub fn block_on<F: Future>(&self, f: F) -> F::Output {
        self.rt.block_on(f)
    }

    /// One round of upkeep for the bucket an epoch's `access_key` reaches.
    ///
    /// # Errors
    ///
    /// If the index can't be read or written.
    pub fn upkeep(
        &self,
        access_key: &str,
        audits: usize,
        renew: Option<upkeep::Renew>,
        pay: &mut (dyn FnMut(u64) -> Result<Vec<Token>> + Send),
    ) -> Result<Report> {
        let bucket = self.gateway.bucket(access_key);
        self.rt.block_on(upkeep::round(&bucket, audits, renew, pay))
    }
}

/// A node key from a file, made the first time.
///
/// # Errors
///
/// If the file can't be read or written.
pub fn node_key(path: &std::path::Path) -> Result<SecretKey> {
    use std::os::unix::fs::OpenOptionsExt;
    match std::fs::read(path) {
        Ok(bytes) => {
            let bytes: [u8; 32] = hex::decode(String::from_utf8(bytes)?.trim())?
                .try_into()
                .map_err(|_| anyhow::anyhow!("{} isn't a node key", path.display()))?;
            Ok(SecretKey::from_bytes(&bytes))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let key = SecretKey::from_bytes(&rand::random());
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            use std::io::Write;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)?
                .write_all(hex::encode(key.to_bytes()).as_bytes())?;
            Ok(key)
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_the_worker_out_of_reach_reads_come_from_the_mirror() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let mirror = tmp.path().join("mirror.db");
        let account = "0123456789abcdef0123456789abcdef";
        {
            let net = Network::open(&format!("dir:{}", tmp.path().join("worker").display()), Some(&mirror))?;
            net.create_account(account, "cap")?;
            assert!(net.append(account, "cap", "heads", 1, "sealed head")?);
            net.put_named(account, Some("cap"), "grants", "dev-1", "sealed grant")?;
            net.put_entry("scope", "id1", b"sealed entry")?;
        }
        // nothing listens there: every request fails
        let net = Network::open("http://127.0.0.1:9", Some(&mirror))?;
        assert_eq!(net.log(account, "heads", 0)?, [(1, "sealed head".to_string())]);
        assert_eq!(
            net.named(account, "grants", "dev")?,
            [("dev-1".into(), "sealed grant".into())]
        );
        assert_eq!(net.entry("scope", "id1")?, Some(b"sealed entry".to_vec()));
        // writes still need the Worker
        assert!(net.append(account, "cap", "heads", 2, "another").is_err());
        // and without a mirror, reads fail as before
        assert!(
            Network::open("http://127.0.0.1:9", None)?
                .log(account, "heads", 0)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn republish_puts_back_only_what_the_worker_lacks() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let mirror = tmp.path().join("mirror.db");
        let worker = tmp.path().join("worker");
        let account = "0123456789abcdef0123456789abcdef";
        let net = Network::open(&format!("dir:{}", worker.display()), Some(&mirror))?;
        net.create_account(account, "cap")?;
        net.append(account, "cap", "devices", 1, "d1")?;
        net.append(account, "cap", "heads", 1, "h1")?;
        net.set_anchor(account, "cap", "anchor")?;
        net.put_named(account, Some("cap"), "nodes", "laptop", "node")?;
        net.put_entry("scope", "id1", b"e1")?;
        net.put_entry("old epoch", "id2", b"e2")?;
        let scopes = ["scope".to_string()];
        assert_eq!(net.republish(account, "cap", &scopes)?, (0, 0));

        std::fs::remove_dir_all(&worker)?;
        let net = Network::open(&format!("dir:{}", worker.display()), Some(&mirror))?;
        // reads from the Worker that lost it don't wipe the copy
        assert!(net.index("scope")?.is_empty());
        assert_eq!(net.entry("scope", "id1")?, None);
        assert_eq!(net.republish(account, "cap", &scopes)?, (4, 1));
        assert_eq!(net.records().log(account, "heads", 0)?, [(1, "h1".to_string())]);
        assert_eq!(net.records().anchor(account)?.as_deref(), Some("anchor"));
        assert_eq!(net.registry().entry("scope", "id1")?, Some(b"e1".to_vec()));
        assert!(net.registry().index("old epoch")?.is_empty());
        assert_eq!(net.republish(account, "cap", &scopes)?, (0, 0));
        Ok(())
    }
}
