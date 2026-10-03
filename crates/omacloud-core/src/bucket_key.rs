//! Changing a self-hosted account's bucket key.
//!
//! Every computer of a self-hosted account holds the bucket's key, so a
//! lost one can reach the bucket until the key changes. A member publishes
//! a [`KeyRecord`]: the new key sealed to each member it trusts now (and the
//! root), signed, stored beside the other coordination records with the old
//! key. The others pick it up on their next sync, switch, and say so with an
//! acknowledgment; once all have, the old key can be deleted at the
//! provider, and a removed or lost computer, which got nothing sealed to
//! it, is cut off.

use std::collections::BTreeMap;

use anyhow::{Context, Result, ensure};
use crypto_box::{PublicKey, SecretKey, aead::OsRng};
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::devices::{DeviceChain, public_hex, verify_sig};

/// A bucket key's options: `access_key_id` and `secret_access_key`, or the
/// provider's equivalents.
pub type Credentials = BTreeMap<String, String>;

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
    pub fn new(
        signer: &SigningKey,
        seq: u64,
        chain: &DeviceChain,
        creds: &Credentials,
    ) -> Result<Self> {
        let me = public_hex(signer);
        let members = chain.members();
        ensure!(
            members.valid.contains_key(&me),
            "only a member can change the bucket key"
        );
        let plain = serde_json::to_vec(creds)?;
        let mut keys = BTreeMap::new();
        for recipient in members.valid.keys().chain(std::iter::once(&chain.root)) {
            _ = keys.insert(recipient.clone(), seal(recipient, &plain)?);
        }
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
    /// at the point it names (or the root).
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
            self.signer == chain.root
                || chain
                    .members_at(self.devices)
                    .valid
                    .contains_key(&self.signer),
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
        let sealed = self.keys.get(&public_hex(key)).context(
            "the new bucket key wasn't sealed to this computer: it was removed before the change",
        )?;
        let plain = SecretKey::from(key.to_scalar_bytes())
            .unseal(&hex::decode(sealed)?)
            .map_err(|_| anyhow::anyhow!("the bucket key doesn't open"))?;
        Ok(serde_json::from_slice(&plain)?)
    }
}

fn seal(recipient: &str, plain: &[u8]) -> Result<String> {
    let bytes: [u8; 32] = hex::decode(recipient)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("not a 32 byte key"))?;
    let x25519 = VerifyingKey::from_bytes(&bytes)?.to_montgomery().to_bytes();
    let sealed = PublicKey::from(x25519)
        .seal(&mut OsRng, plain)
        .map_err(|_| anyhow::anyhow!("sealing failed"))?;
    Ok(hex::encode(sealed))
}

/// The credential options of a location: what a key change replaces.
#[must_use]
pub fn credentials_of(options: &BTreeMap<String, String>) -> Credentials {
    options
        .iter()
        .filter(|(k, _)| crate::repo::CREDENTIAL_OPTIONS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}
