//! The Worker's part kept in a directory instead: a network on one
//! machine, for development and tests, with the same rules (create-only
//! positions, capabilities by hash). Nodes listed here are each given a
//! network of their own, as if every one were in a different place.

use std::{
    fs,
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use iroh::EndpointAddr;

use crate::{
    blob::NodeId,
    place::Node,
    records::Records,
    registry::{Listed, Registry},
};

pub struct DirStore {
    root: PathBuf,
}

fn hash(s: &str) -> String {
    hex::encode(blake3::hash(s.as_bytes()).as_bytes())
}

fn safe(name: &str) -> Result<&str> {
    anyhow::ensure!(
        !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "not a name: {name}"
    );
    Ok(name)
}

/// Write `path` only if it doesn't exist. False if it does.
fn create(path: &Path, body: &[u8]) -> Result<bool> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    match fs::OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut f) => {
            f.write_all(body)?;
            Ok(true)
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn read(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Remove `path`; gone already is fine.
fn remove(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

fn names(dir: &Path) -> Result<Vec<String>> {
    match fs::read_dir(dir) {
        Ok(it) => {
            let mut v: Vec<String> = it
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| !n.starts_with('.'))
                .collect();
            v.sort();
            Ok(v)
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

impl DirStore {
    /// # Errors
    ///
    /// If the directory can't be made.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// List a node, as a heartbeat would.
    ///
    /// # Errors
    ///
    /// If it can't be written.
    pub fn list_node(&self, id: &NodeId, addr: &EndpointAddr, capacity: u64) -> Result<()> {
        let path = self.root.join("nodes").join(hex::encode(id));
        if capacity == 0 {
            _ = fs::remove_file(path);
            return Ok(());
        }
        fs::create_dir_all(self.root.join("nodes"))?;
        let listed = Listed {
            node: Node {
                id: *id,
                asn: u32::from_be_bytes(id[..4].try_into().expect("4 bytes")),
                prefix: hex::encode(&id[..8]),
                uptime: 1.0,
                probation: false,
                since: 0,
            },
            addr: addr.clone(),
        };
        fs::write(path, serde_json::to_vec(&listed)?)?;
        Ok(())
    }

    fn account(&self, account: &str) -> Result<PathBuf> {
        Ok(self.root.join("coord").join(safe(account)?))
    }

    fn check(&self, account: &str, cap: &str) -> Result<PathBuf> {
        let dir = self.account(account)?;
        let want = read(&dir.join("cap"))?.context("no such account")?;
        anyhow::ensure!(want == hash(cap), "that capability doesn't write here");
        Ok(dir)
    }

    /// The account's version: it goes up with every write, so a computer
    /// can tell when to read again.
    fn version(dir: &Path) -> Result<u64> {
        Ok(read(&dir.join("version"))?
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0))
    }

    fn bump(dir: &Path) -> Result<()> {
        fs::write(dir.join("version"), (Self::version(dir)? + 1).to_string())?;
        Ok(())
    }

    /// `create` in the account at `dir`, which changes only if it made it.
    fn create_in(dir: &Path, path: &Path, body: &[u8]) -> Result<bool> {
        let made = create(path, body)?;
        if made {
            Self::bump(dir)?;
        }
        Ok(made)
    }
}

impl Registry for DirStore {
    fn nodes(&self) -> Result<Vec<Listed>> {
        let dir = self.root.join("nodes");
        names(&dir)?
            .iter()
            .map(|n| Ok(serde_json::from_str(&fs::read_to_string(dir.join(n))?)?))
            .collect()
    }
    fn index(&self, scope: &str) -> Result<Vec<(String, Vec<u8>)>> {
        let dir = self.root.join("index").join(hash(scope));
        names(&dir)?
            .into_iter()
            .map(|n| {
                let sealed = STANDARD.decode(fs::read_to_string(dir.join(&n))?)?;
                Ok((n, sealed))
            })
            .collect()
    }
    fn entry(&self, scope: &str, id: &str) -> Result<Option<Vec<u8>>> {
        read(&self.root.join("index").join(hash(scope)).join(safe(id)?))?
            .map(|s| Ok(STANDARD.decode(s)?))
            .transpose()
    }
    fn put_entry(&self, scope: &str, id: &str, sealed: &[u8]) -> Result<()> {
        let dir = self.root.join("index").join(hash(scope));
        fs::create_dir_all(&dir)?;
        fs::write(dir.join(safe(id)?), STANDARD.encode(sealed))?;
        Ok(())
    }
    fn delete_entry(&self, scope: &str, id: &str) -> Result<()> {
        remove(&self.root.join("index").join(hash(scope)).join(safe(id)?))
    }
}

impl Records for DirStore {
    fn create_account(&self, account: &str, cap: &str) -> Result<bool> {
        let dir = self.account(account)?;
        Self::create_in(&dir, &dir.join("cap"), hash(cap).as_bytes())
    }
    fn replace_cap(&self, account: &str, cap: &str, new: &str) -> Result<()> {
        let dir = self.check(account, cap)?;
        fs::write(dir.join("cap"), hash(new))?;
        Self::bump(&dir)
    }
    fn log(&self, account: &str, log: &str, after: u64) -> Result<Vec<(u64, String)>> {
        let dir = self.account(account)?.join(safe(log)?);
        let mut out = Vec::new();
        for n in names(&dir)? {
            let Ok(seq) = n.parse::<u64>() else { continue };
            if seq > after {
                out.push((seq, fs::read_to_string(dir.join(&n))?));
            }
        }
        out.sort_by_key(|(s, _)| *s);
        Ok(out)
    }
    fn append(&self, account: &str, cap: &str, log: &str, seq: u64, body: &str) -> Result<bool> {
        let dir = self.check(account, cap)?;
        Self::create_in(&dir, &dir.join(safe(log)?).join(format!("{seq:020}")), body.as_bytes())
    }
    fn anchor(&self, account: &str) -> Result<Option<String>> {
        read(&self.account(account)?.join("anchor"))
    }
    fn set_anchor(&self, account: &str, cap: &str, body: &str) -> Result<bool> {
        let dir = self.check(account, cap)?;
        Self::create_in(&dir, &dir.join("anchor"), body.as_bytes())
    }
    fn named(&self, account: &str, kind: &str, prefix: &str) -> Result<Vec<(String, String)>> {
        let dir = self.account(account)?.join("named").join(safe(kind)?);
        names(&dir)?
            .into_iter()
            .filter(|n| n.starts_with(prefix))
            .map(|n| Ok((n.clone(), fs::read_to_string(dir.join(&n))?)))
            .collect()
    }
    fn put_named(&self, account: &str, cap: Option<&str>, kind: &str, name: &str, body: &str) -> Result<()> {
        let dir = match cap {
            Some(cap) => self.check(account, cap)?,
            // a join request, to an account there is (as on the Worker)
            None => {
                let dir = self.account(account)?;
                anyhow::ensure!(dir.join("cap").exists(), "no such account");
                dir
            }
        };
        let path = dir.join("named").join(safe(kind)?).join(safe(name)?);
        fs::create_dir_all(path.parent().expect("a parent"))?;
        fs::write(path, body)?;
        Self::bump(&dir)
    }
    fn delete_named(&self, account: &str, cap: &str, kind: &str, name: &str) -> Result<()> {
        let dir = self.check(account, cap)?;
        remove(&dir.join("named").join(safe(kind)?).join(safe(name)?))?;
        Self::bump(&dir)
    }
    fn marker(&self, account: &str) -> Result<u64> {
        Self::version(&self.account(account)?)
    }
}
