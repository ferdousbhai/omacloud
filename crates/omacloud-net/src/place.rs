//! Choosing who holds an object.
//!
//! The account's computers choose, from the whole node directory, so the
//! Worker never learns which nodes hold whose data. Copies go to different
//! networks: what one operator can fake cheaply (keys, accounts) is spread
//! across autonomous systems and address prefixes the Worker saw, which cost
//! real money to multiply.

use std::collections::BTreeSet;

use rand::Rng;
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
    /// New, without a record yet: chosen only after the established nodes.
    pub probation: bool,
    /// When the directory first saw it (Unix seconds; 0 when it didn't say).
    #[serde(default)]
    pub since: u64,
}

/// How long a record takes to count in full.
const TENURE: u64 = 90 * 24 * 3600;

impl Node {
    /// How strongly it's preferred: steady uptime counts most, and a record
    /// grows over its first three months. So holding data, and earning for
    /// it, comes with a history that a node walking away gives up.
    fn weight(&self, now: u64) -> f64 {
        let up = f64::from(self.uptime.clamp(0.01, 1.0));
        let age = now.saturating_sub(self.since).min(TENURE) as f64 / TENURE as f64;
        up * up * age.max(0.1)
    }
}

/// Up enough to be the copy that's there when the others aren't.
const ALWAYS_ON: f32 = 0.95;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PlaceError {
    #[error("the network has {found} independent nodes with room, and {wanted} are needed")]
    TooFew { found: usize, wanted: usize },
}

/// Pick `copies` holders at random among `nodes`, never one of `exclude`
/// (this account's own) or one in `taken` (holding a copy already, for a
/// repair), each on a network no other copy is on, and one of them always
/// on when any is. Nodes on probation come last: they fill copies only
/// when the established ones run out (as on a new network, where every
/// node is new).
///
/// # Errors
///
/// When too few independent nodes are left for `copies`.
pub fn choose(
    nodes: &[Node],
    exclude: &BTreeSet<NodeId>,
    taken: &[Node],
    copies: usize,
) -> Result<Vec<Node>, PlaceError> {
    let mut rng = rand::thread_rng();
    let now = crate::node::now();
    // a random order weighted to the preferred (each drawn as u^(1/w))
    let mut pool: Vec<(f64, &Node)> = nodes
        .iter()
        .filter(|n| !exclude.contains(&n.id) && !taken.iter().any(|t| t.id == n.id))
        .map(|n| (rng.r#gen::<f64>().powf(1.0 / n.weight(now)), n))
        .collect();
    pool.sort_by(|a, b| b.0.total_cmp(&a.0));
    let pool: Vec<&Node> = pool.into_iter().map(|(_, n)| n).collect();
    let mut chosen: Vec<Node> = Vec::new();
    // a node shares its own network, so this also keeps one from being chosen twice
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
        if !n.probation && free(n, &chosen) {
            chosen.push((*n).clone());
        }
    }
    for n in &pool {
        if chosen.len() >= copies {
            break;
        }
        if n.probation && free(n, &chosen) {
            chosen.push((*n).clone());
        }
    }
    if chosen.len() < copies {
        return Err(PlaceError::TooFew {
            found: chosen.len(),
            wanted: copies,
        });
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
            since: 0,
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
            let c = choose(&nodes, &BTreeSet::new(), &[], 2).unwrap();
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
            choose(&nodes, &own, &[], 2),
            Err(PlaceError::TooFew { found: 1, wanted: 2 })
        );
    }

    #[test]
    fn probation_comes_last() {
        let mut new = node(3, 300, 0.99);
        new.probation = true;
        let nodes = [node(1, 100, 0.99), new.clone()];
        for _ in 0..20 {
            // the established node first
            let c = choose(&nodes, &BTreeSet::new(), &[], 1).unwrap();
            assert_eq!(c, vec![nodes[0].clone()]);
        }
        // and the new one as a copy when there's nothing else
        let c = choose(&nodes, &BTreeSet::new(), &[], 2).unwrap();
        assert_eq!(c.len(), 2);
        assert!(choose(&[new.clone()], &BTreeSet::new(), &[], 1).is_ok());
    }

    #[test]
    fn steady_long_standing_holders_are_chosen_more() {
        let now = crate::node::now();
        let mut young = node(1, 100, 0.99);
        young.since = now - 15 * 24 * 3600;
        let mut flaky = node(2, 200, 0.5);
        flaky.since = now - 365 * 24 * 3600;
        let mut steady = node(3, 300, 0.99);
        steady.since = now - 365 * 24 * 3600;
        let nodes = [young, flaky, steady];
        let mut first = [0u32; 3];
        for _ in 0..3000 {
            // no always-on rule here: the copy is taken already
            let held = node(9, 900, 0.99);
            let c = choose(&nodes, &BTreeSet::new(), &[held], 1).unwrap();
            first[usize::from(c[0].id[0]) - 1] += 1;
        }
        assert!(first[2] > first[1] && first[1] > first[0], "{first:?}");
        assert!(first[0] > 0, "the young still get some");
    }

    #[test]
    fn a_repair_keeps_clear_of_the_copies_left() {
        let left = node(1, 100, 0.99);
        let nodes = [left.clone(), node(2, 100, 0.99), node(3, 200, 0.5)];
        let c = choose(&nodes, &BTreeSet::new(), &[left], 1).unwrap();
        assert_eq!(c[0].id, [3; 32]);
    }
}
