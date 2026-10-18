//! The account's keys for one epoch on the network, and the sealing of
//! its index entries.
//!
//! Everything comes from the epoch's repository password, which every
//! member holds and nothing else does: a new epoch (a computer removed)
//! gets keys the removed one never saw. The Worker keeps the index under
//! an id and entry names that are keyed hashes, and entries it can't read
//! or alter unnoticed.

use anyhow::Result;
use chacha20::{
    XChaCha20,
    cipher::{KeyIvInit, StreamCipher},
};

use crate::blob::BlobKey;

/// Everything an account's computer derives for one epoch.
#[derive(Clone)]
pub struct Keys {
    pub blob: BlobKey,
    /// The bucket's id at the Worker: unlinkable to other epochs.
    pub scope: String,
    names: [u8; 32],
    enc: [u8; 32],
    mac: [u8; 32],
}

const TAG: usize = 32;
const NONCE: usize = 24;

impl Keys {
    #[must_use]
    pub fn from_secret(password: &str) -> Self {
        let derive = |what: &str| blake3::derive_key(&format!("omacloud 2026-10 {what}"), password.as_bytes());
        Self {
            blob: BlobKey::from_secret(password),
            scope: hex::encode(&derive("index scope")[..16]),
            names: derive("index names"),
            enc: derive("index encryption"),
            mac: derive("index authentication"),
        }
    }

    /// An object's entry name in the index: the Worker never sees the key.
    #[must_use]
    pub fn entry_id(&self, object: &str) -> String {
        hex::encode(&blake3::keyed_hash(&self.names, object.as_bytes()).as_bytes()[..16])
    }

    /// Encrypt then authenticate: nonce, ciphertext, tag. The entry id is
    /// bound in, so the Worker can't swap two entries.
    #[must_use]
    pub fn seal(&self, entry_id: &str, plain: &[u8]) -> Vec<u8> {
        let nonce: [u8; NONCE] = rand::random();
        let mut out = Vec::with_capacity(NONCE + plain.len() + TAG);
        out.extend_from_slice(&nonce);
        let mut body = plain.to_vec();
        XChaCha20::new(&self.enc.into(), &nonce.into()).apply_keystream(&mut body);
        out.extend_from_slice(&body);
        let tag = self.tag(entry_id, &out);
        out.extend_from_slice(&tag);
        out
    }

    /// Check and decrypt what [`Self::seal`] made for `entry_id`.
    ///
    /// # Errors
    ///
    /// If it was altered, or sealed for another entry or key.
    pub fn open(&self, entry_id: &str, sealed: &[u8]) -> Result<Vec<u8>> {
        anyhow::ensure!(sealed.len() >= NONCE + TAG, "an index entry too short to be one");
        let (signed, tag) = sealed.split_at(sealed.len() - TAG);
        anyhow::ensure!(
            blake3::Hash::from(self.tag(entry_id, signed)) == blake3::Hash::from(<[u8; TAG]>::try_from(tag)?),
            "an index entry that doesn't verify"
        );
        let (nonce, body) = signed.split_at(NONCE);
        let mut plain = body.to_vec();
        XChaCha20::new(&self.enc.into(), <&[u8; NONCE]>::try_from(nonce)?.into()).apply_keystream(&mut plain);
        Ok(plain)
    }

    fn tag(&self, entry_id: &str, signed: &[u8]) -> [u8; TAG] {
        let mut h = blake3::Hasher::new_keyed(&self.mac);
        h.update(&(entry_id.len() as u64).to_le_bytes());
        h.update(entry_id.as_bytes());
        h.update(signed);
        *h.finalize().as_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_entries_open_only_as_themselves() {
        let k = Keys::from_secret("pw");
        let id = k.entry_id("data/ab/abcd");
        let sealed = k.seal(&id, b"entry");
        assert_eq!(k.open(&id, &sealed).unwrap(), b"entry");
        assert!(k.open(&k.entry_id("data/ab/abce"), &sealed).is_err());
        assert!(Keys::from_secret("other").open(&id, &sealed).is_err());
        let mut altered = sealed.clone();
        altered[30] ^= 1;
        assert!(k.open(&id, &altered).is_err());
        // same entry twice: different ciphertexts
        assert_ne!(sealed, k.seal(&id, b"entry"));
    }

    #[test]
    fn epochs_get_unrelated_scopes_and_names() {
        let (a, b) = (Keys::from_secret("one"), Keys::from_secret("two"));
        assert_ne!(a.scope, b.scope);
        assert_ne!(a.entry_id("config"), b.entry_id("config"));
    }
}
