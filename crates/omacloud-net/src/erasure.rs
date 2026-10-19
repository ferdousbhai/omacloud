//! Erasure coding: a large object as 4 data shards and 2 parity shards,
//! each on its own holder. Any 4 rebuild it, so it survives two holders
//! going, in 1.5 times its size where two whole copies take twice it and
//! survive one. The data shards are the object itself in 4 slices, so a
//! range is read from the shards it falls in, without decoding.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Objects this large or larger are coded (restic's data packs); smaller
/// ones (index files, snapshots, small packs) are kept as whole copies.
pub const MIN: u64 = 1 << 20;
const DATA: u8 = 4;
const PARITY: u8 = 2;

/// How an object was coded: shards `0..data` are its slices (the last
/// padded with zeros), `data..data + parity` the parity.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Coded {
    pub data: u8,
    pub parity: u8,
    /// Bytes in each shard.
    pub shard: u64,
}

impl Coded {
    /// The coding for an object of `size` bytes.
    #[must_use]
    pub fn for_size(size: u64) -> Self {
        // shards must be an even number of bytes
        let shard = size.div_ceil(u64::from(DATA)).next_multiple_of(2).max(2);
        Self {
            data: DATA,
            parity: PARITY,
            shard,
        }
    }

    /// Shards in all.
    #[must_use]
    pub fn total(&self) -> usize {
        usize::from(self.data + self.parity)
    }

    /// Every shard of `object`, data then parity.
    ///
    /// # Errors
    ///
    /// If the coding doesn't fit the object.
    pub fn split(&self, object: &[u8]) -> Result<Vec<Vec<u8>>> {
        let shard = usize::try_from(self.shard)?;
        anyhow::ensure!(
            object.len() <= shard * usize::from(self.data),
            "an object larger than its coding"
        );
        let mut data: Vec<Vec<u8>> = (0..usize::from(self.data))
            .map(|i| {
                let start = (i * shard).min(object.len());
                let mut s = object[start..(start + shard).min(object.len())].to_vec();
                s.resize(shard, 0);
                s
            })
            .collect();
        let parity = reed_solomon_simd::encode(usize::from(self.data), usize::from(self.parity), &data)?;
        data.extend(parity);
        Ok(data)
    }

    /// Every shard again from any `data` of them, by index.
    ///
    /// # Errors
    ///
    /// If fewer than `data` are given, or one is the wrong size.
    pub fn rebuild(&self, have: &BTreeMap<usize, Vec<u8>>) -> Result<Vec<Vec<u8>>> {
        let (k, m) = (usize::from(self.data), usize::from(self.parity));
        anyhow::ensure!(have.len() >= k, "{} of {k} shards needed", have.len());
        let shard = usize::try_from(self.shard)?;
        anyhow::ensure!(have.values().all(|s| s.len() == shard), "a shard of the wrong size");
        let originals = have.iter().filter(|(i, _)| **i < k).map(|(i, s)| (*i, s));
        let recovery = have.iter().filter(|(i, _)| **i >= k).map(|(i, s)| (*i - k, s));
        let restored = reed_solomon_simd::decode(k, m, originals, recovery)?;
        let data: Vec<Vec<u8>> = (0..k)
            .map(|i| {
                have.get(&i)
                    .or_else(|| restored.get(&i))
                    .cloned()
                    .context("a shard the decoder didn't restore")
            })
            .collect::<Result<_>>()?;
        self.split(&data.concat())
    }

    /// The object of `size` bytes from any `data` of its shards.
    ///
    /// # Errors
    ///
    /// As [`Self::rebuild`].
    pub fn join(&self, size: u64, have: &BTreeMap<usize, Vec<u8>>) -> Result<Vec<u8>> {
        let mut object = self.rebuild(have)?[..usize::from(self.data)].concat();
        object.truncate(usize::try_from(size)?);
        Ok(object)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_four_shards_rebuild_the_object() -> Result<()> {
        let object: Vec<u8> = (0..1_000_003u32).map(|i| (i % 251) as u8).collect();
        let c = Coded::for_size(object.len() as u64);
        let shards = c.split(&object)?;
        assert_eq!(shards.len(), 6);
        // the data shards are the object's slices
        assert_eq!(shards[1][..10], object[c.shard as usize..c.shard as usize + 10]);
        for lost in [(0, 1), (2, 5), (4, 5), (0, 3)] {
            let have: BTreeMap<usize, Vec<u8>> = shards
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != lost.0 && *i != lost.1)
                .map(|(i, s)| (i, s.clone()))
                .collect();
            assert_eq!(c.join(object.len() as u64, &have)?, object, "{lost:?}");
            assert_eq!(c.rebuild(&have)?, shards, "{lost:?}");
        }
        let three: BTreeMap<usize, Vec<u8>> = shards.iter().take(3).cloned().enumerate().collect();
        assert!(c.join(object.len() as u64, &three).is_err());
        Ok(())
    }

    #[test]
    fn sizes_round_to_even_shards() {
        assert_eq!(Coded::for_size(1).shard, 2);
        assert_eq!(Coded::for_size(9).shard, 4);
        assert_eq!(Coded::for_size(MIN).shard, MIN / 4);
    }
}
