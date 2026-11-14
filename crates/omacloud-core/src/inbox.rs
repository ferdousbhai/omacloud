//! Files a browser sends to an account. A browser can't push a version: it
//! holds no device key, so it signs no head. Instead it leaves each file in
//! the bucket, in an inbox beside the repository, encrypted with the
//! repository's key, and one of the account's computers takes it in: saves it
//! into the folder, where the next sync carries it as any file made there.
//!
//! What makes a computer trust such a file is a chain of two signatures. A
//! member computer endorses a key made for one browser session (it does so
//! when it makes a drive link, and the link carries the key). The browser
//! signs each upload with that key. So an upload stands on a member's word,
//! as a pushed version does, and whoever holds only the bucket's key and the
//! repository's password (a removed computer) can't make one.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use ed25519_dalek::{Signer, SigningKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::devices::{public_hex, verify_sig};

/// A member computer's word for a browser session's key, until a time.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Endorsement {
    /// The session's public key (hex).
    pub key: String,
    /// Uploads written to the bucket after this (Unix seconds) don't count.
    pub until: u64,
    /// The member that signed (hex).
    pub device: String,
    pub sig: String,
}

impl Endorsement {
    fn signed_bytes(key: &str, until: u64, device: &str) -> Vec<u8> {
        format!("omacloud-upload-key-v1\n{key}\n{until}\n{device}\n").into_bytes()
    }

    #[must_use]
    pub fn new(device: &SigningKey, key: &str, until: u64) -> Self {
        let signer = public_hex(device);
        let sig = device.sign(&Self::signed_bytes(key, until, &signer));
        Self {
            key: key.to_string(),
            until,
            device: signer,
            sig: hex::encode(sig.to_bytes()),
        }
    }

    /// Whether the device it names signed it.
    #[must_use]
    pub fn signed(&self) -> bool {
        verify_sig(
            &self.device,
            &self.sig,
            &Self::signed_bytes(&self.key, self.until, &self.device),
        )
    }
}

/// A key for one browser session: its seed, which the drive link carries to
/// the browser, and its public half (hex), which a member endorses.
#[must_use]
pub fn session_key() -> ([u8; 32], String) {
    let key = SigningKey::generate(&mut rand::rngs::OsRng);
    (key.to_bytes(), public_hex(&key))
}

/// What the session's key signs for one file: where it goes and what it is.
#[must_use]
pub fn upload_signed_bytes(path: &str, content: &[u8]) -> Vec<u8> {
    format!("omacloud-upload-v1\n{path}\n{}\n", hex::encode(Sha256::digest(content))).into_bytes()
}

#[derive(Deserialize)]
struct Header {
    v: u8,
    path: String,
    /// When the file was last changed where it came from (Unix milliseconds).
    #[serde(default)]
    mtime: Option<u64>,
    endorsement: Endorsement,
    sig: String,
}

/// A file a browser sent, checked.
#[derive(Debug, PartialEq, Eq)]
pub struct Upload {
    /// Where it goes in the synced folder.
    pub path: PathBuf,
    pub mtime: Option<u64>,
    pub content: Vec<u8>,
}

/// A path a browser may name: inside the folder, through no `..`, nothing
/// hidden, and never Omacloud's own files.
fn safe(path: &str) -> Option<PathBuf> {
    let path = Path::new(path);
    let plain = path.components().all(|c| match c {
        Component::Normal(name) => !name.to_string_lossy().starts_with('.'),
        _ => false,
    });
    (plain && path.components().count() > 0).then(|| path.to_path_buf())
}

/// Check what the repository's key opened from the inbox (`plain`: a length,
/// a header, the content) and give the file. `written` is when the bucket
/// says the object was written (Unix seconds), and `member` whether a device
/// is in the account now.
///
/// # Errors
///
/// If it isn't an upload, a member didn't vouch for the key that signed it,
/// the vouching had run out when it was written, or its path isn't one a
/// browser may name.
pub fn open(plain: &[u8], written: u64, member: impl Fn(&str) -> bool) -> Result<Upload> {
    let (len, rest) = plain.split_first_chunk::<4>().context("too short to be an upload")?;
    let len = u32::from_le_bytes(*len) as usize;
    ensure!(len <= rest.len(), "an upload's header runs past its end");
    let (header, content) = rest.split_at(len);
    let header: Header = serde_json::from_slice(header).context("an upload's header doesn't read")?;
    ensure!(header.v == 1, "an upload of a kind this version doesn't know");
    let endorsement = &header.endorsement;
    ensure!(
        endorsement.signed(),
        "the upload's key isn't vouched for by the device it names"
    );
    ensure!(
        member(&endorsement.device),
        "the upload's key is vouched for by a device that isn't in the account"
    );
    ensure!(
        written <= endorsement.until,
        "the upload was written after its key ran out"
    );
    if !verify_sig(
        &endorsement.key,
        &header.sig,
        &upload_signed_bytes(&header.path, content),
    ) {
        bail!("the upload isn't signed by the key vouched for");
    }
    let path = safe(&header.path).with_context(|| format!("an upload may not go to {:?}", header.path))?;
    Ok(Upload {
        path,
        mtime: header.mtime,
        content: content.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> SigningKey {
        SigningKey::generate(&mut rand::rngs::OsRng)
    }

    /// What the browser's side makes, before the repository's key seals it.
    fn upload(session: &SigningKey, endorsement: &Endorsement, path: &str, content: &[u8]) -> Vec<u8> {
        let header = serde_json::json!({
            "v": 1,
            "path": path,
            "mtime": 1_800_000_000_000_u64,
            "endorsement": endorsement,
            "sig": hex::encode(session.sign(&upload_signed_bytes(path, content)).to_bytes()),
        })
        .to_string();
        let mut plain = u32::try_from(header.len()).unwrap().to_le_bytes().to_vec();
        plain.extend_from_slice(header.as_bytes());
        plain.extend_from_slice(content);
        plain
    }

    #[test]
    fn an_upload_stands_on_a_members_word() {
        let (device, session, other) = (key(), key(), key());
        let member = public_hex(&device);
        let is_member = |d: &str| d == member;
        let endorsement = Endorsement::new(&device, &public_hex(&session), 1000);
        let plain = upload(&session, &endorsement, "Work/plan.md", b"the plan");
        let got = open(&plain, 900, is_member).unwrap();
        assert_eq!(got.path, Path::new("Work/plan.md"));
        assert_eq!(got.content, b"the plan");
        assert_eq!(got.mtime, Some(1_800_000_000_000));

        // written after the vouching ran out
        assert!(open(&plain, 1001, is_member).is_err());
        // vouched for by a device that isn't (or is no longer) in the account
        assert!(open(&plain, 900, |_| false).is_err());
        // signed by a key nobody vouched for
        let forged = upload(&other, &endorsement, "Work/plan.md", b"the plan");
        assert!(open(&forged, 900, is_member).is_err());
        // a vouching the device didn't sign: its time moved later
        let mut longer = endorsement.clone();
        longer.until = 5000;
        assert!(open(&upload(&session, &longer, "Work/plan.md", b"x"), 900, is_member).is_err());
        // the content changed after signing
        let mut changed = plain.clone();
        *changed.last_mut().unwrap() ^= 1;
        assert!(open(&changed, 900, is_member).is_err());
        assert!(open(b"no", 900, is_member).is_err());
    }

    #[test]
    fn an_upload_goes_only_inside_the_folder() {
        let (device, session) = (key(), key());
        let member = public_hex(&device);
        let endorsement = Endorsement::new(&device, &public_hex(&session), 1000);
        for bad in [
            "../x",
            "/etc/passwd",
            "Work/../../x",
            ".omacloud/settings",
            "Work/.hidden",
            "",
        ] {
            let plain = upload(&session, &endorsement, bad, b"x");
            assert!(open(&plain, 1, |d| d == member).is_err(), "{bad}");
        }
    }
}
