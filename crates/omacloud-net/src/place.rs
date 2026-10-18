//! Choosing who holds an object.
//!
//! The account's computers choose, from the whole node directory, so the
//! Worker never learns which nodes hold whose data. Copies go to different
//! networks: what one operator can fake cheaply (keys, accounts) is spread
//! across autonomous systems and address prefixes the Worker saw, which cost
//! real money to multiply.

use std::collections::BTreeSet;

use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};

use crate::blob::NodeId;

/// A node as the Worker's directory lists it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Node {
    #[serde(with = "hex::serde")]
    pub id: NodeId,
    /// The autonomous system it connects from, as the Worker saw.
    pub asn: u32,
    /// Its /24 (IPv4) or /48 (IPv6), as the Worker saw.
    pub prefix: String,
    /// Share of the last month it answered heartbeats, 0 to 1.
    pub uptime: f32,
    /// New, without a record yet: only ever an extra copy.
    pub probation: bool,
}

/// Up enough to be the copy that's there when the others aren't.
pub const ALWAYS_ON: f32 = 0.95;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PlaceError {
    #[error("the network has {found} independent nodes with room, and {wanted} are needed")]
    TooFew { found: usize, wanted: usize },
}

/// Pick `copies` holders at random among `nodes`, never one of `exclude`
/// (this account's own) or one in `taken` (holding a copy already, for a
/// repair), each on a network no other copy is on, and one of them always
/// on when any is. Nodes on probation only fill `extra` places beyond that.
///
/// # Errors
///
/// When too few independent nodes are left for `copies`.
pub fn choose(
    nodes: &[Node],
    exclude: &BTreeSet<NodeId>,
    taken: &[Node],
    copies: usize,
    extra: usize,
) -> Result<Vec<Node>, PlaceError> {
    let mut rng = rand::thread_rng();
    let mut pool: Vec<&Node> = nodes
        .iter()
        .filter(|n| !exclude.contains(&n.id) && !taken.iter().any(|t| t.id == n.id))
        .collect();
    pool.shuffle(&mut rng);
    let mut chosen: Vec<Node> = Vec::new();
    let free = |n: &Node, chosen: &[Node]| {
        taken
            .iter()
            .chain(chosen)
            .all(|c| c.asn != n.asn && c.prefix != n.prefix)
    };
    // one always-on copy first, unless one is held already
    if copies > 0
        && !taken.iter().any(|t| t.uptime >= ALWAYS_ON)
        && let Some(n) = pool
            .iter()
            .find(|n| !n.probation && n.uptime >= ALWAYS_ON && free(n, &chosen))
    {
        chosen.push((*n).clone());
    }
    for n in &pool {
        if chosen.len() >= copies {
            break;
        }
        if !n.probation && !chosen.iter().any(|c| c.id == n.id) && free(n, &chosen) {
            chosen.push((*n).clone());
        }
    }
    if chosen.len() < copies {
        return Err(PlaceError::TooFew {
            found: chosen.len(),
            wanted: copies,
        });
    }
    let mut added = 0;
    for n in &pool {
        if added >= extra {
            break;
        }
        if n.probation && free(n, &chosen) {
            chosen.push((*n).clone());
            added += 1;
        }
    }
    Ok(chosen)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: u8, asn: u32, uptime: f32) -> Node {
        Node {
            id: [id; 32],
            asn,
            prefix: format!("10.{id}.0.0/24"),
            uptime,
            probation: false,
        }
    }

    #[test]
    fn copies_go_to_different_networks_and_one_is_always_on() {
        let nodes = [
            node(1, 100, 0.5),
            node(2, 100, 0.5),
            node(3, 200, 0.5),
            node(4, 300, 0.99),
        ];
        for _ in 0..50 {
            let c = choose(&nodes, &BTreeSet::new(), &[], 2, 0).unwrap();
            assert_eq!(c.len(), 2);
            assert_ne!(c[0].asn, c[1].asn);
            assert!(c.iter().any(|n| n.uptime >= ALWAYS_ON));
        }
    }

    #[test]
    fn never_its_own_nodes_nor_a_shared_prefix() {
        let mut same_prefix = node(2, 200, 0.99);
        same_prefix.prefix = "10.1.0.0/24".into();
        let nodes = [node(1, 100, 0.99), same_prefix, node(3, 300, 0.5)];
        let own: BTreeSet<NodeId> = [[3; 32]].into();
        assert_eq!(
            choose(&nodes, &own, &[], 2, 0),
            Err(PlaceError::TooFew { found: 1, wanted: 2 })
        );
    }

    #[test]
    fn probation_is_only_ever_extra() {
        let mut new = node(3, 300, 0.99);
        new.probation = true;
        let nodes = [node(1, 100, 0.99), new.clone()];
        assert!(choose(&nodes, &BTreeSet::new(), &[], 2, 0).is_err());
        let c = choose(&nodes, &BTreeSet::new(), &[], 1, 1).unwrap();
        assert_eq!(c, vec![nodes[0].clone(), new]);
    }

    #[test]
    fn a_repair_keeps_clear_of_the_copies_left() {
        let left = node(1, 100, 0.99);
        let nodes = [left.clone(), node(2, 100, 0.99), node(3, 200, 0.5)];
        let c = choose(&nodes, &BTreeSet::new(), &[left], 1, 0).unwrap();
        assert_eq!(c[0].id, [3; 32]);
    }
}
