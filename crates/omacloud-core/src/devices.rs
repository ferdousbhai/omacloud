//! The signed device chain: which devices belong to the account.
//!
//! The account's root key comes from the recovery code and never lives on a
//! device. The root signs the first device; after that, a device is valid
//! only if the root or an already valid device signed its `Add` entry, and
//! until an entry revokes it. Entries are hash chained, so a coordinator can
//! relay them but not add a device of its own, drop a revocation, or show
//! devices different histories.
//!
//! A device pins the root's public key. The first device derives it from the
//! recovery code. A device joining by approval takes it from the coordinator's
//! [`Anchor`] and signs it into its [`JoinRequest`]; the approving member,
//! which already pins the real root, refuses a request naming another one.
//! So a coordinator that shows a new device a fake account is caught at
//! approval, the same moment the user compares device fingerprints.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::head::Coordinator;

/// Short, comparable form of a public key: `3f2a-91c0-7be4-0d15`.
#[must_use]
pub fn fingerprint(key_hex: &str) -> String {
    let digest = hex::encode(Sha256::digest(key_hex.as_bytes()));
    digest.as_bytes()[..16]
        .chunks(4)
        .map(|c| std::str::from_utf8(c).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("-")
}

/// Hex public key of a signing key.
#[must_use]
pub fn public_hex(key: &SigningKey) -> String {
    hex::encode(key.verifying_key().as_bytes())
}

/// What recovery codes are written with: no 0, 1, i, l or o to mistake.
pub(crate) const ALPHABET: &[u8] = b"23456789abcdefghjkmnpqrstuvwxyz";

/// 28 random characters from an unambiguous alphabet, about 140 bits.
#[must_use]
pub fn random_secret() -> String {
    let mut rng = rand::rngs::OsRng;
    (0..28)
        .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
        .collect()
}

/// A new recovery code: a [`random_secret`] shown in groups of four.
#[must_use]
pub fn new_recovery_code() -> String {
    random_secret()
        .as_bytes()
        .chunks(4)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect::<Vec<_>>()
        .join("-")
}

/// The account root key a recovery code stands for. Case, spaces and dashes
/// don't matter. The code carries enough entropy that a plain hash suffices.
///
/// # Errors
///
/// If the code doesn't have 28 characters.
pub fn root_key(recovery_code: &str) -> Result<SigningKey> {
    let code: String = recovery_code
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    anyhow::ensure!(code.len() == 28, "a recovery code has 28 characters");
    let seed: [u8; 32] = Sha256::new()
        .chain_update(b"omacloud-recovery-v1\n")
        .chain_update(code.as_bytes())
        .finalize()
        .into();
    Ok(SigningKey::from_bytes(&seed))
}

/// Which account a coordinator serves: the root public key. A hint only;
/// see the module docs for how joining devices confirm it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Anchor {
    pub root: String,
}

/// What an entry does.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum Action {
    Add { device: String, name: String },
    Revoke { device: String },
}

/// One signed entry in the device chain.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct DeviceEntry {
    pub seq: u64,
    pub prev: String,
    #[serde(flatten)]
    pub action: Action,
    /// Public key (hex) of the root or device that signed.
    pub signer: String,
    pub sig: String,
}

#[derive(Serialize)]
struct Signed<'a> {
    domain: &'static str,
    seq: u64,
    prev: &'a str,
    #[serde(flatten)]
    action: &'a Action,
    signer: &'a str,
}

impl DeviceEntry {
    fn signed_bytes(seq: u64, prev: &str, action: &Action, signer: &str) -> Vec<u8> {
        serde_json::to_vec(&Signed {
            domain: "omacloud-device-v1",
            seq,
            prev,
            action,
            signer,
        })
        .expect("serializing plain data")
    }

    /// Build and sign the entry that follows `prev`.
    #[must_use]
    pub fn next(key: &SigningKey, prev: Option<&Self>, action: Action) -> Self {
        let seq = prev.map_or(1, |p| p.seq + 1);
        let prev = prev.map(Self::hash).unwrap_or_default();
        let signer = public_hex(key);
        let sig = key.sign(&Self::signed_bytes(seq, &prev, &action, &signer));
        Self {
            seq,
            prev,
            action,
            signer,
            sig: hex::encode(sig.to_bytes()),
        }
    }

    #[must_use]
    pub fn hash(&self) -> String {
        let mut h = Sha256::new();
        h.update(Self::signed_bytes(
            self.seq,
            &self.prev,
            &self.action,
            &self.signer,
        ));
        h.update(self.sig.as_bytes());
        hex::encode(h.finalize())
    }

    fn signature_ok(&self) -> bool {
        verify_sig(
            &self.signer,
            &self.sig,
            &Self::signed_bytes(self.seq, &self.prev, &self.action, &self.signer),
        )
    }
}

pub(crate) fn verify_sig(key_hex: &str, sig_hex: &str, msg: &[u8]) -> bool {
    let (Ok(key), Ok(sig)) = (hex::decode(key_hex), hex::decode(sig_hex)) else {
        return false;
    };
    let (Ok(key), Ok(sig)) = (<[u8; 32]>::try_from(key), <[u8; 64]>::try_from(sig)) else {
        return false;
    };
    let Ok(key) = VerifyingKey::from_bytes(&key) else {
        return false;
    };
    key.verify(msg, &Signature::from_bytes(&sig)).is_ok()
}

/// What a device refuses in a device chain.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DeviceError {
    #[error("device chain rollback: {served:?} entries served, {known} already seen")]
    Rollback { known: u64, served: Option<u64> },
    #[error("device chain fork at entry {seq}")]
    Fork { seq: u64 },
    #[error("device entry {seq} is not properly signed")]
    BadSignature { seq: u64 },
    #[error("device entry {seq} does not follow entry {after}")]
    Broken { seq: u64, after: u64 },
    #[error(
        "device entry {seq} is signed by {signer}, which is neither the root nor a valid device"
    )]
    UnauthorizedSigner { seq: u64, signer: String },
    #[error("device entry {seq}: the first entry must be the root adding a device")]
    BadGenesis { seq: u64 },
    #[error("device entry {seq} adds {device}, which was revoked or is already present")]
    BadAdd { seq: u64, device: String },
    #[error("device entry {seq} revokes {device}, which is not a valid device")]
    BadRevoke { seq: u64, device: String },
}

/// A verified device chain.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceChain {
    /// The account root public key (hex) every entry traces back to.
    pub root: String,
    /// Every verified entry, in order.
    pub entries: Vec<DeviceEntry>,
}

/// Devices at some point of the chain.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Members {
    /// Valid devices: key (hex) to name.
    pub valid: BTreeMap<String, String>,
    /// Keys revoked so far, with their names.
    pub revoked: BTreeMap<String, String>,
}

impl DeviceChain {
    #[must_use]
    pub fn new(root: &str) -> Self {
        Self {
            root: root.to_string(),
            entries: Vec::new(),
        }
    }

    #[must_use]
    pub fn len(&self) -> u64 {
        self.entries.len() as u64
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub fn last(&self) -> Option<&DeviceEntry> {
        self.entries.last()
    }

    /// Members after the first `seq` entries.
    #[must_use]
    pub fn members_at(&self, seq: u64) -> Members {
        let mut m = Members::default();
        for e in self
            .entries
            .iter()
            .take(usize::try_from(seq).unwrap_or(usize::MAX))
        {
            match &e.action {
                Action::Add { device, name } => _ = m.valid.insert(device.clone(), name.clone()),
                Action::Revoke { device } => {
                    if let Some(name) = m.valid.remove(device) {
                        _ = m.revoked.insert(device.clone(), name);
                    }
                }
            }
        }
        m
    }

    /// Current members.
    #[must_use]
    pub fn members(&self) -> Members {
        self.members_at(self.len())
    }

    /// Whether `key` could sign at chain position `seq`.
    #[must_use]
    pub fn is_valid_at(&self, key: &str, seq: u64) -> bool {
        self.members_at(seq).valid.contains_key(key)
    }

    /// Verify `entry` as the next one and append it.
    ///
    /// # Errors
    ///
    /// The [`DeviceError`] that describes what's wrong.
    pub fn push(&mut self, entry: DeviceEntry) -> Result<(), DeviceError> {
        let seq = entry.seq;
        let after = self.len();
        let prev = self.last().map(DeviceEntry::hash).unwrap_or_default();
        if seq != after + 1 || entry.prev != prev {
            return Err(DeviceError::Broken { seq, after });
        }
        if !entry.signature_ok() {
            return Err(DeviceError::BadSignature { seq });
        }
        let members = self.members();
        let by_root = entry.signer == self.root;
        if after == 0 && !(by_root && matches!(entry.action, Action::Add { .. })) {
            return Err(DeviceError::BadGenesis { seq });
        }
        if !by_root && !members.valid.contains_key(&entry.signer) {
            return Err(DeviceError::UnauthorizedSigner {
                seq,
                signer: entry.signer,
            });
        }
        match &entry.action {
            Action::Add { device, .. } => {
                if members.valid.contains_key(device)
                    || members.revoked.contains_key(device)
                    || *device == self.root
                {
                    return Err(DeviceError::BadAdd {
                        seq,
                        device: device.clone(),
                    });
                }
            }
            Action::Revoke { device } => {
                if !members.valid.contains_key(device) {
                    return Err(DeviceError::BadRevoke {
                        seq,
                        device: device.clone(),
                    });
                }
            }
        }
        self.entries.push(entry);
        Ok(())
    }

    /// Fetch and verify new entries from the coordinator, refusing rollback
    /// and forks.
    ///
    /// # Errors
    ///
    /// A [`DeviceError`], or an I/O error.
    pub fn advance(&mut self, coord: &dyn Coordinator) -> Result<()> {
        let served = coord.device_head()?;
        let known = self.last().cloned();
        match (&known, &served) {
            (Some(k), None) => {
                return Err(DeviceError::Rollback {
                    known: k.seq,
                    served: None,
                }
                .into());
            }
            (Some(k), Some(s)) if s.seq < k.seq => {
                return Err(DeviceError::Rollback {
                    known: k.seq,
                    served: Some(s.seq),
                }
                .into());
            }
            (Some(k), Some(s)) if s.seq == k.seq => {
                if s != k {
                    return Err(DeviceError::Fork { seq: s.seq }.into());
                }
                return Ok(());
            }
            (None, None) => return Ok(()),
            _ => {}
        }
        for entry in coord.devices_since(self.len())? {
            self.push(entry)?;
        }
        if self.len() < served.map_or(0, |s| s.seq) {
            return Err(DeviceError::Fork {
                seq: self.len() + 1,
            }
            .into());
        }
        Ok(())
    }

    /// Sign and append an entry through the coordinator.
    ///
    /// # Errors
    ///
    /// If the entry is invalid here, or another entry took its place (fetch
    /// with [`DeviceChain::advance`] and try again).
    pub fn append(
        &mut self,
        coord: &dyn Coordinator,
        key: &SigningKey,
        action: Action,
    ) -> Result<()> {
        let entry = DeviceEntry::next(key, self.last(), action);
        let mut check = self.clone();
        check.push(entry.clone())?;
        anyhow::ensure!(
            coord.append_device(&entry)?,
            "another device changed the device list first; try again"
        );
        *self = check;
        Ok(())
    }
}

/// A device asking to join, left with the coordinator for an existing device
/// to approve after comparing fingerprints. Signed by the joining device, so
/// the coordinator can't change the name or the root it names.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct JoinRequest {
    pub device: String,
    pub name: String,
    /// The account root the joining device was shown and pins.
    pub root: String,
    pub sig: String,
}

impl JoinRequest {
    fn signed_bytes(device: &str, name: &str, root: &str) -> Vec<u8> {
        format!("omacloud-join-v1\n{device}\n{name}\n{root}\n").into_bytes()
    }

    #[must_use]
    pub fn new(key: &SigningKey, name: &str, root: &str) -> Self {
        let device = public_hex(key);
        let sig = key.sign(&Self::signed_bytes(&device, name, root));
        Self {
            device,
            name: name.to_string(),
            root: root.to_string(),
            sig: hex::encode(sig.to_bytes()),
        }
    }

    /// Whether the joining device signed this request.
    #[must_use]
    pub fn signed(&self) -> bool {
        verify_sig(
            &self.device,
            &self.sig,
            &Self::signed_bytes(&self.device, &self.name, &self.root),
        )
    }
}

/// All devices of a fresh account: the root adds the first device.
#[must_use]
pub fn genesis(root: &SigningKey, device: &SigningKey, name: &str) -> DeviceEntry {
    DeviceEntry::next(
        root,
        None,
        Action::Add {
            device: public_hex(device),
            name: name.to_string(),
        },
    )
}

/// The keys in `members`, for callers that only need membership.
#[must_use]
pub fn keys(members: &Members) -> BTreeSet<String> {
    members.valid.keys().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> SigningKey {
        SigningKey::generate(&mut rand::rngs::OsRng)
    }

    fn add(k: &SigningKey, name: &str) -> Action {
        Action::Add {
            device: public_hex(k),
            name: name.into(),
        }
    }

    #[test]
    fn recovery_code_is_the_root() -> Result<()> {
        let code = new_recovery_code();
        assert_eq!(code.len(), 28 + 6);
        let a = root_key(&code)?;
        let b = root_key(&code.to_uppercase().replace('-', " "))?;
        assert_eq!(public_hex(&a), public_hex(&b));
        assert!(root_key("too-short").is_err());
        Ok(())
    }

    #[test]
    fn membership_rules() {
        let root = key();
        let (a, b, c, stranger) = (key(), key(), key(), key());
        let mut chain = DeviceChain::new(&public_hex(&root));

        // the first entry must be the root adding a device
        let e = DeviceEntry::next(&a, None, add(&a, "a"));
        assert_eq!(
            chain.clone().push(e),
            Err(DeviceError::BadGenesis { seq: 1 })
        );
        chain.push(genesis(&root, &a, "a")).unwrap();

        // a valid device adds another; a stranger can't
        let e = DeviceEntry::next(&stranger, chain.last(), add(&stranger, "s"));
        assert!(matches!(
            chain.clone().push(e),
            Err(DeviceError::UnauthorizedSigner { seq: 2, .. })
        ));
        chain
            .push(DeviceEntry::next(&a, chain.last(), add(&b, "b")))
            .unwrap();
        assert_eq!(chain.members().valid.len(), 2);

        // b revokes a; a can no longer sign, and can't be added back
        chain
            .push(DeviceEntry::next(
                &b,
                chain.last(),
                Action::Revoke {
                    device: public_hex(&a),
                },
            ))
            .unwrap();
        let e = DeviceEntry::next(&a, chain.last(), add(&c, "c"));
        assert!(matches!(
            chain.clone().push(e),
            Err(DeviceError::UnauthorizedSigner { seq: 4, .. })
        ));
        let e = DeviceEntry::next(&root, chain.last(), add(&a, "a again"));
        assert!(matches!(
            chain.clone().push(e),
            Err(DeviceError::BadAdd { seq: 4, .. })
        ));

        // history keeps its meaning: a was valid until entry 3
        assert!(chain.is_valid_at(&public_hex(&a), 2));
        assert!(!chain.is_valid_at(&public_hex(&a), 3));

        // tampering breaks the signature
        let mut e = DeviceEntry::next(&b, chain.last(), add(&c, "c"));
        e.action = add(&stranger, "c");
        assert_eq!(
            chain.clone().push(e),
            Err(DeviceError::BadSignature { seq: 4 })
        );
    }
}
