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

/// How far a holder's clock may be from this computer's.
const SLACK: u64 = 3600;

/// How long a copy's index entry may come after it was stored: one
/// object's upload. Well under a day, so only one daily renewal can find a
/// copy stored before it but indexed after.
const INDEX_LAG: u64 = 6 * 3600;

/// Since when a copy may be owed, its holder last renewed at `last` (0:
/// never): since then, or since it was stored when never; a copy a repair
/// placed since may be owed from when the holder held it before (0: up to
/// a longest lease); one stored just before that renewal may have reached
/// the index after it, so is owed since it was stored. Each allowance
/// holds for one renewal only.
fn owed_since(placed: u64, stored: u64, last: u64) -> u64 {
    if placed > 0 && placed + SLACK > last {
        0
    } else if stored > 0 && stored + INDEX_LAG > last {
        // never renewed here: since it was stored
        if last == 0 { stored } else { stored.min(last) }
    } else {
        last
    }
}

/// Blobs renewed a request: well inside a request's size limit.
const RENEW_BATCH: usize = 10_000;

/// Where renewals are paid from.
pub trait Wallet: Send {
    /// `n` tokens, taken out.
    ///
    /// # Errors
    ///
    /// If it holds too few, and can't get more.
    fn pay(&mut self, n: u64) -> Result<Vec<Token>>;
    /// Tokens paid that the holder says it didn't take (another of the
    /// account's computers paid it first), put back: as fresh ones, as it
    /// may have.
    fn put_back(&mut self, tokens: Vec<Token>);
}

/// A wallet for storage that costs nothing (no token key: tests, `dir:`
/// networks).
pub struct Free;

impl Wallet for Free {
    fn pay(&mut self, _: u64) -> Result<Vec<Token>> {
        Ok(Vec::new())
    }
    fn put_back(&mut self, _: Vec<Token>) {}
}

/// A renewal: keep every good copy until `until`, paying each holder for
/// the month ends since it was last renewed, as `last` says (a holder not
/// in it: none known). Per holder, as what each is owed is: one left
/// unrenewed (unpaid, out of reach) keeps its date, and its next charge
/// stays within what it's allowed.
#[derive(Debug, Clone, Default)]
pub struct Renew {
    pub until: u64,
    pub last: BTreeMap<NodeId, u64>,
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
    /// Holders whose every copy asked was renewed this round, or that were
    /// paid: what they're owed counts from now.
    pub renewed: BTreeSet<NodeId>,
}

/// The most a holder keeping `bytes` can have earned since the renewal at
/// `last` (0: none): every byte for every month end passed since (holders
/// charge at month ends shared by all), at most a longest lease's worth. A
/// holder asking anything between month ends is cheating.
#[must_use]
fn most_earned(bytes: u64, last: u64, now: u64) -> u64 {
    let longest = crate::node::LONGEST_LEASE.div_ceil(MONTH);
    // an hour either side, for the holder's clock against this one's
    let months = if last == 0 {
        longest
    } else {
        ((now + SLACK) / MONTH)
            .saturating_sub(last.saturating_sub(SLACK) / MONTH)
            .min(longest)
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
pub async fn round(bucket: &Bucket, audits: usize, renew: Option<Renew>, wallet: &mut dyn Wallet) -> Result<Report> {
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
    // whether any holder answered at all: none did, and it's this computer
    // that's out of reach, not every holder at once
    let mut answered = false;
    let mut asked = false;
    while let Some((e, h, read)) = checks.next().await {
        report.audited += 1;
        asked = true;
        answered |= read.is_ok();
        if let Err(err) = read {
            log::info!("{} failed an audit of {}: {err:#}", hex::encode(&h.node[..4]), e.key);
            bad.entry(e.key.clone()).or_default().insert(h.node);
        }
    }

    if let Some(Renew { until, last }) = renew {
        let mut per_holder: BTreeMap<NodeId, Vec<(&Entry, &Held)>> = BTreeMap::new();
        for e in &entries {
            for h in &e.holders {
                if !bad.get(&e.key).is_some_and(|b| b.contains(&h.node)) {
                    per_holder.entry(h.node).or_default().push((e, h));
                }
            }
        }
        for (node, held) in per_holder {
            let Some(addr) = nodes.iter().find(|n| n.node.id == node).map(|n| n.addr.clone()) else {
                continue;
            };
            let last = last.get(&node).copied().unwrap_or(0);
            // what it's owed counts from now only if every copy it holds was
            // renewed or paid for: none left out (a failed audit), none in a
            // batch that wasn't
            let mut covered = !bad.values().any(|b| b.contains(&node));
            // in batches a request can carry
            for held in held.chunks(RENEW_BATCH) {
                // tokens handed over are gone whatever the holder says next: a
                // holder that took them and asks again is held to what's
                // been earned since
                let mut paid = false;
                let mut given = Vec::new();
                let mut by_since: BTreeMap<u64, u64> = BTreeMap::new();
                for (e, h) in held {
                    *by_since.entry(owed_since(h.placed, h.stored, last)).or_default() += e.blob_size();
                }
                // as of now, holder by holder: a round can run long
                let allowed = |now| -> u64 {
                    by_since
                        .iter()
                        .map(|(since, bytes)| most_earned(*bytes, *since, now))
                        .sum()
                };
                let blobs: Vec<BlobId> = held.iter().map(|(_, h)| h.blob).collect();
                // asked first without paying: the holder says what it's owed
                let mut answer = client::renew(bucket.endpoint(), &addr, blobs.clone(), until, Vec::new()).await;
                asked = true;
                answered |= answer.is_ok();
                if let Ok(Renewal::Owed(owed)) = answer {
                    if owed > allowed(crate::node::now()) {
                        log::info!("{} asked {owed} tokens, more than it served", hex::encode(&node[..4]));
                        answer = Err(anyhow::anyhow!("asked too much"));
                    } else {
                        match wallet.pay(owed) {
                            Ok(mut tokens) => {
                                paid = !tokens.is_empty();
                                given.clone_from(&tokens);
                                answer =
                                    client::renew(bucket.endpoint(), &addr, blobs.clone(), until, tokens.clone()).await;
                                // a month end passed between asking and paying:
                                // once more, the same tokens and what the new
                                // month adds (the holder kept none of them)
                                if let Ok(Renewal::Owed(more)) = answer
                                    && more > owed
                                    && more <= allowed(crate::node::now())
                                {
                                    match wallet.pay(more - owed) {
                                        Ok(extra) => {
                                            tokens.extend(extra);
                                            given.clone_from(&tokens);
                                            answer =
                                                client::renew(bucket.endpoint(), &addr, blobs, until, tokens.clone())
                                                    .await;
                                        }
                                        // as when the first payment fails: not
                                        // the holder's doing (an honest one
                                        // took none of the first tokens)
                                        Err(e) => {
                                            log::warn!("paying {}: {e:#}", hex::encode(&node[..4]));
                                            covered = false;
                                            continue;
                                        }
                                    }
                                }
                            }
                            // not the holder's doing: it isn't counted bad, and
                            // the other holders and the repairs go on
                            Err(e) => {
                                log::warn!("paying {}: {e:#}", hex::encode(&node[..4]));
                                covered = false;
                                continue;
                            }
                        }
                    }
                }
                match answer {
                    Ok(Renewal::Kept(kept, taken)) => {
                        if let Ok(taken) = usize::try_from(taken)
                            && taken < given.len()
                        {
                            wallet.put_back(given.split_off(taken));
                        }
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
                        // paid and still owed: it took them (an error may
                        // have come before it did)
                        covered &= paid && answer.is_ok();
                        for (e, _) in held {
                            bad.entry(e.key.clone()).or_default().insert(node);
                        }
                    }
                }
            }
            if covered {
                report.renewed.insert(node);
            }
        }
    }

    if asked && !answered {
        anyhow::bail!("no holder answered: this computer seems to be offline");
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
    fn an_allowance_for_a_copy_holds_for_one_renewal() {
        let day = 24 * 3600;
        let t = 100 * MONTH;
        // renewed since it was placed by a repair or stored: since then
        assert_eq!(owed_since(0, t, t + 2 * day), t + 2 * day);
        // placed by a repair after the last renewal: up to a longest lease
        assert_eq!(owed_since(t + day, t, t), 0);
        // never renewed here (a new computer, a new key): since it was stored
        assert_eq!(owed_since(0, t, 0), t);
        assert_eq!(owed_since(0, 0, 0), 0);
        assert_eq!(owed_since(t, t, 0), 0);
        // stored an hour before a renewal that couldn't see it yet: since
        // it was stored, that once; the next day's renewal, as any other
        assert_eq!(owed_since(0, t, t + 3600), t);
        assert_eq!(owed_since(0, t, t + day), t + day);
    }

    #[test]
    fn a_holder_can_ask_for_the_months_it_served_and_no_more() {
        let gib = 1 << 30;
        let now = 100 * MONTH + 3600;
        // renewed yesterday, with a month end since: that month
        assert_eq!(most_earned(gib, now - 24 * 3600, now), 1);
        // renewed a day after it: nothing
        assert_eq!(most_earned(gib, now + 24 * 3600 - 60, now + 24 * 3600), 0);
        // within an hour of a month end, either side: that month (clocks)
        assert_eq!(most_earned(gib, now - 60, now), 1);
        assert_eq!(most_earned(3 * gib, now - 2 * MONTH, now), 6);
        assert_eq!(most_earned(gib, now - 12 * MONTH, now), 3, "at most a longest lease");
        assert_eq!(most_earned(gib, 0, now), 3);
    }
}
