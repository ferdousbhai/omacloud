//! Signed requests to the coordinator service.
//!
//! Writes carry their own signatures (heads, device entries, records), so
//! the service checks those. Reads show device names and history metadata,
//! so they are signed by the reading key:
//!
//! `Authorization: OneCloud <public key hex> <unix seconds> <signature hex>`
//!
//! over `onecloud-auth-v1\n<METHOD>\n<path and query>\n<seconds>\n<sha256 of body, hex>\n`.
//! The service accepts a timestamp within five minutes of its clock.

use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};

use crate::devices::public_hex;

/// The bytes a request signature covers.
#[must_use]
pub fn signed_bytes(method: &str, path: &str, ts: u64, body: &[u8]) -> Vec<u8> {
    let body_hash = hex::encode(Sha256::digest(body));
    format!("onecloud-auth-v1\n{method}\n{path}\n{ts}\n{body_hash}\n").into_bytes()
}

/// The `Authorization` header value for a request.
#[must_use]
pub fn header(key: &SigningKey, method: &str, path: &str, ts: u64, body: &[u8]) -> String {
    let sig = key.sign(&signed_bytes(method, path, ts, body));
    format!(
        "OneCloud {} {ts} {}",
        public_hex(key),
        hex::encode(sig.to_bytes())
    )
}
