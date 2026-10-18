//! The network for one run of omacloud: a runtime, the gateway the sync
//! engine talks S3 to, and the way to the Worker. Blocking callers (the
//! CLI, the daemon) use it from outside the runtime.
//!
//! Two identities: an account's computer reaches holders from a fresh,
//! throwaway key each run, so a holder can't tie the blobs it's given to
//! the node key this computer serves its own disk under.

use std::{collections::BTreeSet, path::PathBuf, sync::Arc};

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
    node::{Config, Node},
    records::Records,
    registry::{Listed, Registry},
    upkeep::{self, Report},
    worker::Worker,
};

/// Where the Worker's part lives: the Worker, or a directory (`dir:<path>`)
/// for a network on one machine.
pub enum Network {
    Worker(Worker),
    Dir(DirStore),
}

impl Network {
    /// # Errors
    ///
    /// If a `dir:` network's directory can't be made.
    pub fn open(url: &str) -> Result<Self> {
        Ok(match url.strip_prefix("dir:") {
            Some(path) => Self::Dir(DirStore::new(path)?),
            None => Self::Worker(Worker::new(url)),
        })
    }

    /// Say this node is up, where, with how much room.
    ///
    /// # Errors
    ///
    /// If the Worker can't be reached or refuses.
    pub fn heartbeat(&self, key: &SecretKey, node: &Node, capacity: u64) -> Result<()> {
        match self {
            Self::Worker(w) => w.heartbeat(key, &node.addr(), capacity).map(|_| ()),
            Self::Dir(d) => d.list_node(&node.id(), &node.addr(), capacity),
        }
    }

    /// The token key, when storage is paid for (not on a `dir:` network).
    ///
    /// # Errors
    ///
    /// If the Worker can't be reached.
    pub fn issuer(&self) -> Result<Option<IssuerKey>> {
        match self {
            Self::Worker(w) => w.issuer().map(Some),
            Self::Dir(_) => Ok(None),
        }
    }

    /// The starter grant: tokens for a computer that holds none yet.
    ///
    /// # Errors
    ///
    /// If the grant for this network is used up.
    pub fn starter(&self, n: usize) -> Result<Vec<Token>> {
        match self {
            Self::Worker(w) => w.starter(&w.issuer()?, n),
            Self::Dir(_) => Ok(Vec::new()),
        }
    }

    /// Trade tokens taken as a holder for fresh ones.
    ///
    /// # Errors
    ///
    /// If the Worker refuses them.
    pub fn exchange(&self, tokens: &[Token]) -> Result<Vec<Token>> {
        match self {
            Self::Worker(w) => w.exchange(&w.issuer()?, tokens),
            Self::Dir(_) => Ok(Vec::new()),
        }
    }

    fn registry(&self) -> &dyn Registry {
        match self {
            Self::Worker(w) => w,
            Self::Dir(d) => d,
        }
    }

    fn records(&self) -> &dyn Records {
        match self {
            Self::Worker(w) => w,
            Self::Dir(d) => d,
        }
    }
}

impl Registry for Network {
    fn nodes(&self) -> Result<Vec<Listed>> {
        self.registry().nodes()
    }
    fn index(&self, scope: &str) -> Result<Vec<(String, Vec<u8>)>> {
        self.registry().index(scope)
    }
    fn entry(&self, scope: &str, id: &str) -> Result<Option<Vec<u8>>> {
        self.registry().entry(scope, id)
    }
    fn put_entry(&self, scope: &str, id: &str, sealed: &[u8]) -> Result<()> {
        self.registry().put_entry(scope, id, sealed)
    }
    fn delete_entry(&self, scope: &str, id: &str) -> Result<()> {
        self.registry().delete_entry(scope, id)
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
        self.records().log(account, log, after)
    }
    fn append(&self, account: &str, cap: &str, log: &str, seq: u64, body: &str) -> Result<bool> {
        self.records().append(account, cap, log, seq, body)
    }
    fn anchor(&self, account: &str) -> Result<Option<String>> {
        self.records().anchor(account)
    }
    fn set_anchor(&self, account: &str, cap: &str, body: &str) -> Result<bool> {
        self.records().set_anchor(account, cap, body)
    }
    fn named(&self, account: &str, kind: &str, prefix: &str) -> Result<Vec<(String, String)>> {
        self.records().named(account, kind, prefix)
    }
    fn put_named(&self, account: &str, cap: Option<&str>, kind: &str, name: &str, body: &str) -> Result<()> {
        self.records().put_named(account, cap, kind, name, body)
    }
    fn delete_named(&self, account: &str, cap: &str, kind: &str, name: &str) -> Result<()> {
        self.records().delete_named(account, cap, kind, name)
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
        let network = Arc::new(Network::open(&opts.url)?);
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
        renew_until: Option<u64>,
        pay: &mut (dyn FnMut(u64) -> Result<Vec<Token>> + Send),
    ) -> Result<Report> {
        let bucket = self.gateway.bucket(access_key);
        self.rt.block_on(upkeep::round(&bucket, audits, renew_until, pay))
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
