//! A file a browser sends to its account: the browser's half of
//! `omacloud_core::inbox`, which has the reasons. The session's key signs
//! where the file goes and what it is; a member computer's endorsement of
//! that key rides along; the repository's key seals the lot.

use ed25519_dalek::{Signer, SigningKey};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{Reader, Result};

/// What a computer's inbox takes: sealed with the repository's key, a
/// header's length (4 bytes, little endian), the header, and the content.
/// `seed` is the session's signing key and `endorsement` a member's word for
/// it, both as the drive link carried them; `mtime` is when the file last
/// changed (Unix milliseconds); `nonce` must be random and used once.
///
/// # Errors
///
/// If the seed isn't 32 bytes or the endorsement isn't JSON.
pub fn make(
    reader: &Reader,
    seed: &[u8],
    endorsement: &str,
    path: &str,
    mtime: u64,
    nonce: [u8; 16],
    content: &[u8],
) -> Result<Vec<u8>> {
    let seed: [u8; 32] = seed.try_into().map_err(|_| "the upload key is damaged".to_string())?;
    let endorsement: Value = serde_json::from_str(endorsement).map_err(|_| "the upload key is damaged".to_string())?;
    // as `omacloud_core::inbox::upload_signed_bytes` spells it
    let signed = format!("omacloud-upload-v1\n{path}\n{}\n", hex::encode(Sha256::digest(content)));
    let sig = SigningKey::from_bytes(&seed).sign(signed.as_bytes());
    let header = json!({
        "v": 1,
        "path": path,
        "mtime": mtime,
        "endorsement": endorsement,
        "sig": hex::encode(sig.to_bytes()),
    })
    .to_string();
    let len = u32::try_from(header.len()).map_err(|_| "the header is too long".to_string())?;
    let mut plain = Vec::with_capacity(4 + header.len() + content.len());
    plain.extend_from_slice(&len.to_le_bytes());
    plain.extend_from_slice(header.as_bytes());
    plain.extend_from_slice(content);
    reader.seal(nonce, &plain)
}
