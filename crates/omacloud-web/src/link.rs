//! What a drive link hands a browser, sealed: a computer leaves it in the
//! account's own bucket for a few minutes, and the link carries the seed that
//! opens it, in the part of an address no server is sent. So the bucket holds
//! only what it can't read, and the link alone, once the minutes are up, opens
//! nothing.
//!
//! Sealed as the repository seals its files (AES-256-CTR with Poly1305-AES),
//! so the page needs no cipher of its own.

use aes256ctr_poly1305aes::{
    Aes256CtrPoly1305Aes,
    aead::{Aead, Nonce},
};
use sha2::{Digest, Sha512};

use crate::{Key, Result};

/// How long the seed is.
pub const SEED: usize = 32;

fn key(seed: &[u8]) -> Result<Key> {
    if seed.len() != SEED {
        return Err("the link is damaged".into());
    }
    let mut hash = Sha512::new();
    hash.update(b"omacloud drive link 1");
    hash.update(seed);
    Ok(Key(hash.finalize().into()))
}

/// Seal `plain` under `seed`. `nonce` must be random and used once: this
/// crate does no I/O, so the caller brings it.
///
/// # Errors
///
/// If the seed isn't [`SEED`] bytes.
pub fn seal(seed: &[u8], nonce: [u8; 16], plain: &[u8]) -> Result<Vec<u8>> {
    let sealed = Aes256CtrPoly1305Aes::new((&key(seed)?.0).into())
        .encrypt(Nonce::<Aes256CtrPoly1305Aes>::from_slice(&nonce), plain)
        .map_err(|_| "sealing failed".to_string())?;
    Ok([&nonce[..], &sealed].concat())
}

/// Open what [`seal`] made.
///
/// # Errors
///
/// If the seed is another's, or the data was changed.
pub fn unseal(seed: &[u8], sealed: &[u8]) -> Result<Vec<u8>> {
    key(seed)?
        .decrypt(sealed)
        .map_err(|_| "this link doesn't open what was left for it".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_with_its_seed_only() {
        let sealed = seal(&[7; SEED], [1; 16], b"the keys").unwrap();
        assert_eq!(unseal(&[7; SEED], &sealed).unwrap(), b"the keys");
        assert!(unseal(&[8; SEED], &sealed).is_err());
        let mut changed = sealed.clone();
        *changed.last_mut().unwrap() ^= 1;
        assert!(unseal(&[7; SEED], &changed).is_err());
        assert!(unseal(&[7; 5], &sealed).is_err());
        assert!(!sealed.windows(8).any(|w| w == b"the keys"));
    }
}
