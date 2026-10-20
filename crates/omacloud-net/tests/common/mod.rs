//! What the tests share: local endpoints, and holders listed in a registry.

// each test binary uses only some of it
#![allow(dead_code)]

use std::path::Path;

use anyhow::Result;
use iroh::{Endpoint, RelayMode, endpoint::presets};
use omacloud_net::{
    node::{Config, Node},
    place,
    registry::{Listed, MemRegistry},
};

/// An endpoint on this machine, no relays.
pub async fn endpoint() -> Result<Endpoint> {
    Ok(Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind()
        .await?)
}

/// `n` holders under `dir`, each listed in `registry` on a network of its
/// own.
pub async fn holders(dir: &Path, n: u8, registry: &MemRegistry) -> Result<Vec<Node>> {
    let mut nodes = Vec::new();
    for i in 0..n {
        let node = Node::spawn(
            endpoint().await?,
            Config {
                dir: dir.join(format!("node{i}")),
                capacity: 1 << 30,
                issuer: None,
                trade: None,
            },
        )
        .await?;
        registry.add_node(Listed {
            node: place::Node {
                id: node.id(),
                asn: 64500 + u32::from(i),
                prefix: format!("192.0.{i}.0/24"),
                uptime: 0.99,
                probation: false,
                since: 0,
            },
            addr: node.addr(),
        });
        nodes.push(node);
    }
    Ok(nodes)
}
