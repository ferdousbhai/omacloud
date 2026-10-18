//! What the network keeps centrally, at the Worker: the node directory and
//! each bucket's sealed index. Neither is trusted for integrity: entries
//! are sealed by the account's keys, and every byte read is checked
//! against the blob id an entry names.

use std::{collections::BTreeMap, sync::Mutex};

use anyhow::Result;
use iroh::EndpointAddr;
use serde::{Deserialize, Serialize};

use crate::{blob::NodeId, place::Node};

/// A node in the directory, with where to reach it.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Listed {
    #[serde(flatten)]
    pub node: Node,
    pub addr: EndpointAddr,
}

/// The Worker's part. Blocking calls: the Worker is one HTTPS request away.
pub trait Registry: Send + Sync {
    /// Every node with room.
    fn nodes(&self) -> Result<Vec<Listed>>;
    /// A bucket's whole index: entry id and sealed entry.
    fn index(&self, scope: &str) -> Result<Vec<(String, Vec<u8>)>>;
    /// One entry of a bucket's index.
    fn entry(&self, scope: &str, id: &str) -> Result<Option<Vec<u8>>>;
    /// Add or replace an entry.
    fn put_entry(&self, scope: &str, id: &str, sealed: &[u8]) -> Result<()>;
    /// Remove an entry, if there.
    fn delete_entry(&self, scope: &str, id: &str) -> Result<()>;
}

/// A registry in memory: tests, and a network on one machine.
#[derive(Default)]
pub struct MemRegistry {
    nodes: Mutex<Vec<Listed>>,
    index: Mutex<BTreeMap<String, BTreeMap<String, Vec<u8>>>>,
}

impl MemRegistry {
    pub fn add_node(&self, listed: Listed) {
        let mut nodes = self.nodes.lock().expect("registry");
        nodes.retain(|n| n.node.id != listed.node.id);
        nodes.push(listed);
    }

    pub fn remove_node(&self, id: &NodeId) {
        self.nodes.lock().expect("registry").retain(|n| n.node.id != *id);
    }
}

impl Registry for MemRegistry {
    fn nodes(&self) -> Result<Vec<Listed>> {
        Ok(self.nodes.lock().expect("registry").clone())
    }
    fn index(&self, scope: &str) -> Result<Vec<(String, Vec<u8>)>> {
        Ok(self
            .index
            .lock()
            .expect("registry")
            .get(scope)
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default())
    }
    fn entry(&self, scope: &str, id: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .index
            .lock()
            .expect("registry")
            .get(scope)
            .and_then(|m| m.get(id).cloned()))
    }
    fn put_entry(&self, scope: &str, id: &str, sealed: &[u8]) -> Result<()> {
        self.index
            .lock()
            .expect("registry")
            .entry(scope.to_string())
            .or_default()
            .insert(id.to_string(), sealed.to_vec());
        Ok(())
    }
    fn delete_entry(&self, scope: &str, id: &str) -> Result<()> {
        if let Some(m) = self.index.lock().expect("registry").get_mut(scope) {
            m.remove(id);
        }
        Ok(())
    }
}
