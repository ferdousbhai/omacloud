//! What a holder stores: a restic object scrambled for that holder alone.
//!
//! Two holders of the same object get different bytes, so neither can answer
//! for the other: a holder that dropped its copy can't fetch another's when
//! asked to prove it still has it. Only the account's computers can make or
//! undo the scrambling, as the key comes from the epoch's repository secret.
//! The object is already restic ciphertext; this adds no secrecy, only
//! uniqueness.

use chacha20::{
    ChaCha20,
    cipher::{KeyIvInit, StreamCipher, StreamCipherSeek},
};

/// A node's public key: its iroh endpoint id.
pub type NodeId = [u8; 32];

/// The account's scrambling key for one epoch.
#[derive(Clone)]
pub struct BlobKey([u8; 32]);

impl BlobKey {
    /// From the epoch's repository password, which every member holds.
    #[must_use]
    pub fn from_secret(password: &str) -> Self {
        Self(blake3::derive_key(
            "omacloud 2026-10 blob scrambling",
            password.as_bytes(),
        ))
    }

    /// Scramble `data` in place for `holder`; doing it again undoes it.
    /// `object` is the object's key in the bucket, so no two objects share
    /// a key stream.
    pub fn apply(&self, object: &str, holder: &NodeId, data: &mut [u8]) {
        self.apply_at(object, holder, 0, data);
    }

    /// [`Self::apply`] to the part of a blob that starts `offset` bytes in.
    pub fn apply_at(&self, object: &str, holder: &NodeId, offset: u64, data: &mut [u8]) {
        let mut h = blake3::Hasher::new_keyed(&self.0);
        h.update(&(object.len() as u64).to_le_bytes());
        h.update(object.as_bytes());
        h.update(holder);
        let nonce: [u8; 12] = h.finalize().as_bytes()[..12].try_into().expect("12 bytes");
        let mut cipher = ChaCha20::new(&self.0.into(), &nonce.into());
        cipher.seek(offset);
        cipher.apply_keystream(data);
    }
}

/// A blob's id: the BLAKE3 hash holders store and serve it under, and
/// audits check ranges of.
#[must_use]
pub fn blob_id(data: &[u8]) -> [u8; 32] {
    *blake3::hash(data).as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrambling_is_undone_by_doing_it_again() {
        let key = BlobKey::from_secret("password");
        let plain = b"restic pack bytes".to_vec();
        let mut data = plain.clone();
        key.apply("data/ab/abcd", &[1; 32], &mut data);
        assert_ne!(data, plain);
        key.apply("data/ab/abcd", &[1; 32], &mut data);
        assert_eq!(data, plain);
        // a part, from where it starts in the blob
        key.apply("data/ab/abcd", &[1; 32], &mut data);
        let mut part = data[5..9].to_vec();
        key.apply_at("data/ab/abcd", &[1; 32], 5, &mut part);
        assert_eq!(part, &plain[5..9]);
    }

    #[test]
    fn holders_objects_and_accounts_get_different_blobs() {
        let key = BlobKey::from_secret("password");
        let blob = |key: &BlobKey, object: &str, holder: u8| {
            let mut d = vec![0u8; 64];
            key.apply(object, &[holder; 32], &mut d);
            blob_id(&d)
        };
        let a = blob(&key, "data/ab/abcd", 1);
        assert_ne!(a, blob(&key, "data/ab/abcd", 2));
        assert_ne!(a, blob(&key, "data/ab/abce", 1));
        assert_ne!(a, blob(&BlobKey::from_secret("other"), "data/ab/abcd", 1));
        // a name that runs into the holder id can't collide with another
        assert_ne!(blob(&key, "a", 1), blob(&key, "a\u{1}", 1));
    }
}
