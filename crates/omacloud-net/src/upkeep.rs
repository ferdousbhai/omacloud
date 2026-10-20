//! What an account's computer does for its copies, now and then: checks
//! that holders still have them, pays to keep them, and puts copies
//! elsewhere when a holder fails or leaves.
//!
//! An audit is an ordinary read of a random range, checked against the
//! blob's BLAKE3 tree: a holder can't tell it from a restore, and can't
//! answer it without the bytes. Nothing about audits goes to the Worker; a
//! holder that fails one simply isn't used or paid again.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use futures_util::stream::{FuturesUnordered, StreamExt};
use rand::{Rng, seq::SliceRandom};

use crate::{
    blob::NodeId,
    bucket::{Bucket, Entry, Held},
    client::{self, Renewal},
    credit::{Token, tokens_for},
    node::MONTH,
    proto::BlobId,
};

/// Blobs renewed a request: well inside a request's size limit.
const RENEW_BATCH: usize = 10_000;

/// A renewal: keep every good copy until `until`, paying each holder for
/// the time it kept them since the last renewal, at `last` (0: none yet).
#[derive(Debug, Clone, Copy)]
pub struct Renew {
    pub until: u64,
    pub last: u64,
}

/// What a round of upkeep found and did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Copies read back in an audit.
    pub audited: usize,
    /// Copies that failed one, or whose holder left or didn't renew.
    pub failed: usize,
    /// Objects given new copies.
    pub repaired: usize,
    /// Objects with no copy left anywhere.
    pub lost: Vec<String>,
    /// Bytes stored, one copy of each object.
    pub stored: u64,
    /// Tokens a month keeping every copy the index names costs (failed
    /// ones too, until repaired), as holders charge.
    pub monthly: u64,
    /// Holders that failed: an audit, a renewal, or asked more than they
    /// could have earned.
    pub failed_nodes: BTreeSet<NodeId>,
}

/// The most a holder keeping `bytes` can have earned since the renewal at
/// `last` (0: none): every byte for every whole month since, and one more
/// for a month that was under way then, at most a longest lease's worth.
#[must_use]
fn most_earned(bytes: u64, last: u64, now: u64) -> u64 {
    // a holder charges whole months, and a part of one carries over to the
    // next renewal: so a month more than have passed since the last
    let longest = crate::node::LONGEST_LEASE.div_ceil(MONTH);
    let months = if last == 0 {
        longest
    } else {
        (now.saturating_sub(last) / MONTH + 1).min(longest)
    };
    tokens_for(bytes * months)
}

/// One round: audit up to `audits` random copies, renew every copy until
/// `until` when asked (paying with `pay`, which is given a token count),
/// and repair, fewest copies first.
///
/// # Errors
///
/// If the index can't be read or written.
pub async fn round(
    bucket: &Bucket,
    audits: usize,
    renew: Option<Renew>,
    pay: &mut (dyn FnMut(u64) -> Result<Vec<Token>> + Send),
) -> Result<Report> {
    let mut report = Report::default();
    let entries = bucket.list("").await?;
    let nodes = bucket.nodes().await?;
    let listed: BTreeSet<NodeId> = nodes.iter().map(|n| n.node.id).collect();
    let mut bad: BTreeMap<String, BTreeSet<NodeId>> = BTreeMap::new();

    // a holder gone from the directory has gone for good
    for e in &entries {
        for h in &e.holders {
            if !listed.contains(&h.node) {
                bad.entry(e.key.clone()).or_default().insert(h.node);
            }
        }
    }

    let mut copies: Vec<(&Entry, &Held)> = entries
        .iter()
        .flat_map(|e| e.holders.iter().map(move |h| (e, h)))
        .filter(|(e, h)| !bad.get(&e.key).is_some_and(|b| b.contains(&h.node)))
        .collect();
    copies.shuffle(&mut rand::thread_rng());
    // all at once: a holder that's gone costs its timeout, not the round's
    let mut checks: FuturesUnordered<_> = copies
        .into_iter()
        .take(audits)
        .filter_map(|(e, h)| {
            let addr = nodes.iter().find(|n| n.node.id == h.node)?.addr.clone();
            let (start, len) = {
                let mut rng = rand::thread_rng();
                let len = e.blob_size().min(1024);
                (rng.gen_range(0..=e.blob_size() - len), len)
            };
            Some(async move {
                let read = client::read(bucket.endpoint(), &addr, h.blob, Some(start..start + len)).await;
                (e, h, read)
            })
        })
        .collect();
    while let Some((e, h, read)) = checks.next().await {
        report.audited += 1;
        if let Err(err) = read {
            log::info!("{} failed an audit of {}: {err:#}", hex::encode(&h.node[..4]), e.key);
            bad.entry(e.key.clone()).or_default().insert(h.node);
        }
    }

    if let Some(Renew { until, last }) = renew {
        let mut per_holder: BTreeMap<NodeId, Vec<(&Entry, BlobId)>> = BTreeMap::new();
        for e in &entries {
            for h in &e.holders {
                if !bad.get(&e.key).is_some_and(|b| b.contains(&h.node)) {
                    per_holder.entry(h.node).or_default().push((e, h.blob));
                }
            }
        }
        let now = crate::node::now();
        for (node, held) in per_holder {
            let Some(addr) = nodes.iter().find(|n| n.node.id == node).map(|n| n.addr.clone()) else {
                continue;
            };
            // in batches a request can carry
            for held in held.chunks(RENEW_BATCH) {
                let bytes: u64 = held.iter().map(|(e, _)| e.blob_size()).sum();
                let blobs: Vec<BlobId> = held.iter().map(|(_, b)| *b).collect();
                // asked first without paying: the holder says what it's owed
                let mut answer = client::renew(bucket.endpoint(), &addr, blobs.clone(), until, Vec::new()).await;
                if let Ok(Renewal::Owed(owed)) = answer {
                    if owed > most_earned(bytes, last, now) {
                        log::info!("{} asked {owed} tokens, more than it served", hex::encode(&node[..4]));
                        answer = Err(anyhow::anyhow!("asked too much"));
                    } else {
                        answer = client::renew(bucket.endpoint(), &addr, blobs, until, pay(owed)?).await;
                    }
                }
                match answer {
                    Ok(Renewal::Kept(kept)) => {
                        for ((e, _), k) in held.iter().zip(kept) {
                            if !k {
                                bad.entry(e.key.clone()).or_default().insert(node);
                            }
                        }
                    }
                    Ok(Renewal::Owed(_)) | Err(_) => {
                        if let Err(err) = &answer {
                            log::info!("renewing with {}: {err:#}", hex::encode(&node[..4]));
                        }
                        for (e, _) in held {
                            bad.entry(e.key.clone()).or_default().insert(node);
                        }
                    }
                }
            }
        }
    }

    report.failed = bad.values().map(BTreeSet::len).sum();
    report.failed_nodes = bad.values().flatten().copied().collect();
    report.stored = entries.iter().map(|e| e.size).sum();
    let mut per_holder: BTreeMap<NodeId, u64> = BTreeMap::new();
    for e in &entries {
        for h in &e.holders {
            *per_holder.entry(h.node).or_default() += e.blob_size();
        }
    }
    report.monthly = per_holder.values().map(|b| tokens_for(*b)).sum();
    let mut repairs: Vec<&Entry> = entries
        .iter()
        .filter(|e| e.size > 0 && (bad.contains_key(&e.key) || e.holders.len() < e.wanted(bucket.copies())))
        .collect();
    // holders to spare: those with none are next to being lost
    repairs.sort_by_key(|e| {
        let good = e.holders.len() - bad.get(&e.key).map_or(0, BTreeSet::len);
        good.saturating_sub(e.needed())
    });
    // a node that failed any copy gets none of the new ones
    let suspects = report.failed_nodes.clone();
    for e in repairs {
        let gone = bad.remove(&e.key).unwrap_or_default();
        match bucket.repair(e, &gone, &suspects).await {
            Ok(true) => report.repaired += 1,
            Ok(false) => {}
            Err(err) => {
                log::warn!("repairing {}: {err:#}", e.key);
                if e.holders.iter().filter(|h| !gone.contains(&h.node)).count() < e.needed() {
                    report.lost.push(e.key.clone());
                }
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_holder_can_ask_for_the_months_it_served_and_no_more() {
        let gib = 1 << 30;
        let now = 100 * MONTH;
        // renewed yesterday: a month that was under way may have ended since
        assert_eq!(most_earned(gib, now - 24 * 3600, now), 1);
        assert_eq!(most_earned(gib, now - MONTH, now), 2);
        assert_eq!(most_earned(3 * gib, now - 10, now), 3, "at least a month");
        assert_eq!(most_earned(gib, now - 12 * MONTH, now), 3, "at most a longest lease");
        assert_eq!(most_earned(gib, 0, now), 3);
        assert_eq!(most_earned(1000, now - MONTH, now), 1);
    }
}
