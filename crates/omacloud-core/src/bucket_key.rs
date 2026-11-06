//! Changing the key of an account's bucket.
//!
//! Every computer of an account holds the bucket's key, so a
//! lost one can reach the bucket until the key changes. A member publishes
//! a [`KeyRecord`]: the new key sealed to each member it trusts now (and the
//! root), signed, stored beside the other coordination records with the old
//! key. The others pick it up on their next sync, switch, and say so with an
//! acknowledgment; once all have, the old key can be deleted at the
//! provider, and a removed or lost computer, which got nothing sealed to
//! it, is cut off.

use std::collections::BTreeMap;

use anyhow::{Context, Result, ensure};
use ed25519_dalek::{Signer, SigningKey};
use serde::{Deserialize, Serialize};

use crate::{
    devices::{DeviceChain, public_hex, verify_sig},
    epoch::{open, seal_to_holders},
};

/// A bucket's key, or what stands for one at Dropbox.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(untagged)]
pub enum Credentials {
    S3 {
        access_key_id: String,
        secret_access_key: String,
    },
    Dropbox {
        refresh_token: String,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct KeyRecord {
    /// From 1; the latest one holds.
    pub seq: u64,
    /// How many device chain entries the signer had seen: the members the
    /// key is sealed to are the valid ones at that point.
    pub devices: u64,
    /// The key sealed to each member (hex public key) and the root.
    pub keys: BTreeMap<String, String>,
    pub signer: String,
    pub sig: String,
}

impl KeyRecord {
    fn signed_bytes(&self) -> Vec<u8> {
        let body = serde_json::json!({
            "domain": "omacloud-bucket-key-v1",
            "seq": self.seq,
            "devices": self.devices,
            "keys": self.keys,
            "signer": self.signer,
        });
        serde_json::to_vec(&body).expect("json")
    }

    /// Seal `creds` to every valid member of `chain` and to its root, as
    /// record `seq`, signed by `signer` (a member).
    ///
    /// # Errors
    ///
    /// If `signer` isn't a member, or a key can't be sealed to.
    pub fn new(signer: &SigningKey, seq: u64, chain: &DeviceChain, creds: &Credentials) -> Result<Self> {
        let me = public_hex(signer);
        ensure!(
            chain.members().valid.contains_key(&me),
            "only a member can change the bucket key"
        );
        let keys = seal_to_holders(chain, creds)?;
        let mut record = Self {
            seq,
            devices: chain.len(),
            keys,
            signer: me,
            sig: String::new(),
        };
        record.sig = hex::encode(signer.sign(&record.signed_bytes()).to_bytes());
        Ok(record)
    }

    /// Check the record against the device chain: signed by a device valid
    /// at the point it names.
    ///
    /// # Errors
    ///
    /// If the signature or signer doesn't check out.
    pub fn verify(&self, chain: &DeviceChain) -> Result<()> {
        ensure!(
            verify_sig(&self.signer, &self.sig, &self.signed_bytes()),
            "bucket key record {} has a bad signature",
            self.seq
        );
        ensure!(
            self.devices <= chain.len(),
            "bucket key record {} names devices this computer hasn't seen",
            self.seq
        );
        ensure!(
            chain.is_valid_at(&self.signer, self.devices),
            "bucket key record {} is signed by a key that wasn't a member",
            self.seq
        );
        Ok(())
    }

    /// The key, opened with `key` (a member's device key or the root).
    ///
    /// # Errors
    ///
    /// If it wasn't sealed to `key`: this computer was removed before the
    /// change.
    pub fn open(&self, key: &SigningKey) -> Result<Credentials> {
        let sealed = self
            .keys
            .get(&public_hex(key))
            .context("the new bucket key wasn't sealed to this computer: it was removed before the change")?;
        open(key, sealed)
    }
}
