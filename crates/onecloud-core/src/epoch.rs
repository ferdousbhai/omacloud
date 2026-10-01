//! Repository keys: who holds them, and key rotation.
//!
//! A restic repository keeps one master key for life, and every device that
//! synced it holds that key. Removing a device therefore needs a new
//! repository: an *epoch*. Epoch 0 is the repository created with the
//! account; epoch `n` lives beside it (see [`crate::RepoSpec::for_epoch`]),
//! so no storage location or credential ever passes through the coordinator.
//!
//! Every epoch has a signed [`EpochRecord`] carrying the repository secret
//! sealed to each member device and to the account root, so the recovery code
//! alone can always get back in. A device approved later gets a signed
//! [`Grant`]: the current secret sealed to it. The coordinator relays both
//! and can open neither.
//!
//! Rotating copies every snapshot into the new repository, appends a head
//! that starts the new epoch, and publishes the record with the map from old
//! to new snapshot ids, so history stays readable. Blob and tree ids are
//! hashes of plaintext, so trees keep their ids across epochs; only snapshot
//! ids change.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use crypto_box::{PublicKey, SecretKey, aead::OsRng};
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use rustic_core::repofile::MasterKey;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    RepoSpec,
    devices::{DeviceChain, public_hex, verify_sig},
    head::Coordinator,
};

/// What a device needs to use an epoch's repository.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct EpochSecret {
    /// The repository password, for restoring with plain restic.
    pub password: String,
    pub key: MasterKey,
    /// For a bucket of the user's own: where it is and the credentials to
    /// reach it, so a new device gets them with the key instead of from
    /// the person typing them in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<RepoSpec>,
}

/// Seal `secret` so only the holder of `recipient`'s signing key (hex public
/// key) can open it. Ed25519 keys convert to X25519 as libsodium's
/// `crypto_sign_ed25519_pk_to_curve25519` does.
///
/// # Errors
///
/// If `recipient` isn't a valid public key.
pub fn seal(recipient: &str, secret: &EpochSecret) -> Result<String> {
    let bytes: [u8; 32] = hex::decode(recipient)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("not a 32 byte key"))?;
    let x25519 = VerifyingKey::from_bytes(&bytes)?.to_montgomery().to_bytes();
    let sealed = PublicKey::from(x25519)
        .seal(&mut OsRng, &serde_json::to_vec(secret)?)
        .map_err(|_| anyhow::anyhow!("sealing failed"))?;
    Ok(hex::encode(sealed))
}

/// Open a secret sealed to `key`.
///
/// # Errors
///
/// If it wasn't sealed to this key, or was tampered with.
pub fn open(key: &SigningKey, sealed: &str) -> Result<EpochSecret> {
    let plain = SecretKey::from(key.to_scalar_bytes())
        .unseal(&hex::decode(sealed)?)
        .map_err(|_| anyhow::anyhow!("the repository key is not sealed to this key"))?;
    Ok(serde_json::from_slice(&plain)?)
}

/// One epoch: its secret for each holder, and how it began.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct EpochRecord {
    /// From 0, the account's first repository.
    pub epoch: u64,
    /// `hash()` of the previous record; empty for epoch 0.
    pub prev: String,
    /// Seq of the head that starts this epoch (0 for epoch 0).
    pub head: u64,
    /// Device chain length the signer saw; the keys go to its members.
    pub devices: u64,
    /// The repository secret, sealed to each member and to the root (keyed
    /// by hex public key).
    pub keys: BTreeMap<String, String>,
    /// Previous epoch's snapshot id to this epoch's, for every head so far.
    pub snapshots: BTreeMap<String, String>,
    pub signer: String,
    pub sig: String,
}

#[derive(Serialize)]
struct SignedRecord<'a> {
    domain: &'static str,
    epoch: u64,
    prev: &'a str,
    head: u64,
    devices: u64,
    keys: &'a BTreeMap<String, String>,
    snapshots: &'a BTreeMap<String, String>,
    signer: &'a str,
}

impl EpochRecord {
    fn signed_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&SignedRecord {
            domain: "onecloud-epoch-v1",
            epoch: self.epoch,
            prev: &self.prev,
            head: self.head,
            devices: self.devices,
            keys: &self.keys,
            snapshots: &self.snapshots,
            signer: &self.signer,
        })
        .expect("serializing plain data")
    }

    /// Build and sign the record after `prev` (epoch 0 without one). The
    /// secret is sealed to every valid member at `devices` and to the root.
    ///
    /// # Errors
    ///
    /// If sealing fails.
    pub fn next(
        signer: &SigningKey,
        prev: Option<&Self>,
        head: u64,
        chain: &DeviceChain,
        secret: &EpochSecret,
        snapshots: BTreeMap<String, String>,
    ) -> Result<Self> {
        let mut keys = BTreeMap::new();
        for holder in chain
            .members()
            .valid
            .into_keys()
            .chain([chain.root.clone()])
        {
            let sealed = seal(&holder, secret)?;
            _ = keys.insert(holder, sealed);
        }
        let mut rec = Self {
            epoch: prev.map_or(0, |p| p.epoch + 1),
            prev: prev.map(Self::hash).unwrap_or_default(),
            head,
            devices: chain.len(),
            keys,
            snapshots,
            signer: public_hex(signer),
            sig: String::new(),
        };
        rec.sig = hex::encode(signer.sign(&rec.signed_bytes()).to_bytes());
        Ok(rec)
    }

    #[must_use]
    pub fn hash(&self) -> String {
        let mut h = Sha256::new();
        h.update(self.signed_bytes());
        h.update(self.sig.as_bytes());
        hex::encode(h.finalize())
    }
}

/// The current epoch's secret, sealed to a device approved after the epoch
/// began.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    pub epoch: u64,
    pub device: String,
    pub sealed: String,
    /// A member device, or the root (recovery code joins).
    pub signer: String,
    pub sig: String,
}

impl Grant {
    fn signed_bytes(epoch: u64, device: &str, sealed: &str, signer: &str) -> Vec<u8> {
        format!("onecloud-grant-v1\n{epoch}\n{device}\n{sealed}\n{signer}\n").into_bytes()
    }

    /// Seal `secret` for `device` and sign.
    ///
    /// # Errors
    ///
    /// If sealing fails.
    pub fn new(
        signer: &SigningKey,
        epoch: u64,
        device: &str,
        secret: &EpochSecret,
    ) -> Result<Self> {
        let sealed = seal(device, secret)?;
        let signer_hex = public_hex(signer);
        let sig = signer.sign(&Self::signed_bytes(epoch, device, &sealed, &signer_hex));
        Ok(Self {
            epoch,
            device: device.to_string(),
            sealed,
            signer: signer_hex,
            sig: hex::encode(sig.to_bytes()),
        })
    }

    fn valid(&self, chain: &DeviceChain) -> bool {
        let signed = verify_sig(
            &self.signer,
            &self.sig,
            &Self::signed_bytes(self.epoch, &self.device, &self.sealed, &self.signer),
        );
        let by_member = self.signer == chain.root
            || chain.members().valid.contains_key(&self.signer)
            || chain.members().revoked.contains_key(&self.signer);
        signed && by_member
    }
}

/// What a device refuses in the epoch records.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EpochError {
    #[error("epoch rollback: {served} records served, {known} already seen")]
    Rollback { known: u64, served: u64 },
    #[error("epoch record {epoch} does not follow the one before it")]
    Broken { epoch: u64 },
    #[error("epoch record {epoch} is not properly signed")]
    BadSignature { epoch: u64 },
    #[error("epoch record {epoch} is signed by a device that wasn't a member")]
    UntrustedSigner { epoch: u64 },
    #[error("epoch {epoch} started, but its key record isn't available yet")]
    Pending { epoch: u64 },
    #[error("no repository key for this device in epoch {epoch} yet")]
    NoKey { epoch: u64 },
}

/// The verified epoch records.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct EpochChain {
    pub records: Vec<EpochRecord>,
}

impl EpochChain {
    /// The latest epoch with a verified record.
    #[must_use]
    pub fn current(&self) -> Option<u64> {
        self.records.last().map(|r| r.epoch)
    }

    /// Verify `rec` as the next record and append it.
    ///
    /// # Errors
    ///
    /// The [`EpochError`] that describes what's wrong.
    pub fn push(&mut self, rec: EpochRecord, devices: &DeviceChain) -> Result<(), EpochError> {
        let expected = self.current().map_or(0, |e| e + 1);
        let prev = self
            .records
            .last()
            .map(EpochRecord::hash)
            .unwrap_or_default();
        if rec.epoch != expected || rec.prev != prev {
            return Err(EpochError::Broken { epoch: rec.epoch });
        }
        if !verify_sig(&rec.signer, &rec.sig, &rec.signed_bytes()) {
            return Err(EpochError::BadSignature { epoch: rec.epoch });
        }
        if rec.devices > devices.len() || !devices.is_valid_at(&rec.signer, rec.devices) {
            return Err(EpochError::UntrustedSigner { epoch: rec.epoch });
        }
        self.records.push(rec);
        Ok(())
    }

    /// Fetch and verify new records, refusing rollback and rewrites.
    ///
    /// # Errors
    ///
    /// An [`EpochError`], or an I/O error.
    pub fn advance(&mut self, coord: &dyn Coordinator, devices: &DeviceChain) -> Result<()> {
        let served = coord.epochs()?;
        let known = self.records.len() as u64;
        if (served.len() as u64) < known {
            return Err(EpochError::Rollback {
                known,
                served: served.len() as u64,
            }
            .into());
        }
        for (i, rec) in served.into_iter().enumerate() {
            match self.records.get(i) {
                Some(mine) if *mine == rec => {}
                Some(_) => return Err(EpochError::Broken { epoch: rec.epoch }.into()),
                None => self.push(rec, devices)?,
            }
        }
        Ok(())
    }

    /// The id in epoch `to` of snapshot `id` from epoch `from`.
    #[must_use]
    pub fn map(&self, id: &str, from: u64, to: u64) -> Option<String> {
        let mut id = id.to_string();
        for rec in self
            .records
            .iter()
            .filter(|r| r.epoch > from && r.epoch <= to)
        {
            id = rec.snapshots.get(&id)?.clone();
        }
        Some(id)
    }

    /// The latest epoch's secret for `key` (a device or the root), from the
    /// record or from a grant.
    ///
    /// # Errors
    ///
    /// [`EpochError::NoKey`] if neither holds one, or an I/O error.
    pub fn secret(
        &self,
        coord: &dyn Coordinator,
        chain: &DeviceChain,
        key: &SigningKey,
    ) -> Result<(u64, EpochSecret)> {
        let rec = self
            .records
            .last()
            .context("the account has no repository key yet")?;
        let me = public_hex(key);
        if let Some(sealed) = rec.keys.get(&me) {
            return Ok((rec.epoch, open(key, sealed)?));
        }
        for grant in coord.grants(&me)? {
            if grant.epoch == rec.epoch && grant.device == me && grant.valid(chain) {
                return Ok((rec.epoch, open(key, &grant.sealed)?));
            }
        }
        Err(EpochError::NoKey { epoch: rec.epoch }.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::{Action, DeviceEntry, genesis};

    fn key() -> SigningKey {
        SigningKey::generate(&mut rand::rngs::OsRng)
    }

    #[test]
    fn sealed_to_one_key_only() -> Result<()> {
        let (a, b) = (key(), key());
        let secret = EpochSecret {
            password: "pw".into(),
            key: MasterKey::new(),
            storage: None,
        };
        let sealed = seal(&public_hex(&a), &secret)?;
        assert_eq!(open(&a, &sealed)?.password, "pw");
        assert!(open(&b, &sealed).is_err());
        Ok(())
    }

    #[test]
    fn records_seal_to_members_and_root_and_map_snapshots() -> Result<()> {
        let (root, a, b, c) = (key(), key(), key(), key());
        let mut devices = DeviceChain::new(&public_hex(&root));
        devices.push(genesis(&root, &a, "a")).unwrap();
        devices
            .push(DeviceEntry::next(
                &a,
                devices.last(),
                Action::Add {
                    device: public_hex(&b),
                    name: "b".into(),
                },
            ))
            .unwrap();
        let s = |p: &str| EpochSecret {
            password: p.into(),
            key: MasterKey::new(),
            storage: None,
        };
        let snaps = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(x, y)| ((*x).to_string(), (*y).to_string()))
                .collect()
        };
        let mut chain = EpochChain::default();
        let r0 = EpochRecord::next(&a, None, 0, &devices, &s("zero"), BTreeMap::new())?;
        chain.push(r0.clone(), &devices).unwrap();

        // b is revoked; epoch 1 keys go to a and the root only
        devices
            .push(DeviceEntry::next(
                &a,
                devices.last(),
                Action::Revoke {
                    device: public_hex(&b),
                },
            ))
            .unwrap();
        let r1 = EpochRecord::next(
            &a,
            Some(&r0),
            7,
            &devices,
            &s("one"),
            snaps(&[("s1", "t1")]),
        )?;
        chain.push(r1.clone(), &devices).unwrap();
        assert!(r1.keys.contains_key(&public_hex(&a)));
        assert!(r1.keys.contains_key(&public_hex(&root)));
        assert!(!r1.keys.contains_key(&public_hex(&b)));
        assert_eq!(open(&root, &r1.keys[&public_hex(&root)])?.password, "one");
        assert_eq!(chain.map("s1", 0, 1).as_deref(), Some("t1"));
        assert_eq!(chain.map("nope", 0, 1), None);

        // the revoked device can't start an epoch, nor can a stranger
        for signer in [&b, &c] {
            let rogue =
                EpochRecord::next(signer, Some(&r1), 9, &devices, &s("x"), BTreeMap::new())?;
            assert_eq!(
                chain.clone().push(rogue, &devices),
                Err(EpochError::UntrustedSigner { epoch: 2 })
            );
        }
        Ok(())
    }
}
