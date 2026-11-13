//! Reading an Omacloud repository where the caller does the fetching: a web
//! page gets the bytes from the person's own storage and hands them here,
//! compiled to WebAssembly, to decrypt. So nothing here touches a network or
//! a disk, and the storage's key and the repository's password never leave
//! the browser.
//!
//! An Omacloud repository is a plain restic one (format 2): this reads the
//! key file, unpacked files (config, snapshots, indexes) and blobs in packs.

use std::{collections::HashMap, io::Read};

use aes256ctr_poly1305aes::{
    Aes256CtrPoly1305Aes,
    aead::{Aead, Nonce},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[cfg(target_arch = "wasm32")]
mod abi;
pub mod link;

pub type Result<T> = std::result::Result<T, String>;

type Id = [u8; 32];

/// A blob's id from its hex.
fn id(hex_id: &str) -> Result<Id> {
    let mut id = [0; 32];
    hex::decode_to_slice(hex_id, &mut id).map_err(|_| format!("{hex_id:?} isn't an id"))?;
    Ok(id)
}

/// A repository's key: AES-256 key, then Poly1305-AES's `k` and `r`.
#[derive(Clone)]
struct Key([u8; 64]);

impl Key {
    /// `data` is a nonce, the ciphertext and its MAC.
    fn decrypt(&self, data: &[u8]) -> Result<Vec<u8>> {
        if data.len() < 32 {
            return Err("too short to be encrypted data".into());
        }
        Aes256CtrPoly1305Aes::new((&self.0).into())
            .decrypt(Nonce::<Aes256CtrPoly1305Aes>::from_slice(&data[..16]), &data[16..])
            .map_err(|_| "wrong key, or the data was changed".to_string())
    }
}

#[derive(Deserialize)]
struct KeyFile {
    #[serde(rename = "N")]
    n: u32,
    r: u32,
    p: u32,
    data: String,
    salt: String,
}

#[derive(Deserialize)]
struct MasterKey {
    mac: Mac,
    encrypt: String,
}

#[derive(Deserialize)]
struct Mac {
    k: String,
    r: String,
}

fn base64(s: &str) -> Result<Vec<u8>> {
    STANDARD
        .decode(s)
        .map_err(|_| "a key file field isn't base64".to_string())
}

fn unzstd(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    ruzstd::decoding::StreamingDecoder::new(data)
        .map_err(|e| format!("not zstd: {e}"))?
        .read_to_end(&mut out)
        .map_err(|e| format!("not zstd: {e}"))?;
    Ok(out)
}

#[derive(Deserialize)]
struct IndexFile {
    #[serde(default)]
    packs: Vec<IndexPack>,
}

#[derive(Deserialize)]
struct IndexPack {
    id: String,
    blobs: Vec<IndexBlob>,
}

#[derive(Deserialize)]
struct IndexBlob {
    id: String,
    offset: u32,
    length: u32,
    uncompressed_length: Option<u32>,
}

/// Where a blob is: `length` bytes at `offset` of the pack.
#[derive(Clone, Serialize, Debug, PartialEq, Eq)]
pub struct Location {
    pub pack: String,
    pub offset: u32,
    pub length: u32,
    #[serde(skip)]
    compressed: bool,
}

/// An opened repository: its key, and where each blob is once the indexes
/// are in.
pub struct Reader {
    key: Key,
    blobs: HashMap<Id, Location>,
}

impl Reader {
    /// Open with the repository's password and one of its key files
    /// (`keys/<id>`, as stored). The wrong password for this file is an error.
    ///
    /// # Errors
    ///
    /// If the key file is damaged or the password doesn't open it.
    pub fn open(password: &str, key_file: &[u8]) -> Result<Self> {
        let file: KeyFile = serde_json::from_slice(key_file).map_err(|e| format!("not a key file: {e}"))?;
        let log_n = file.n.checked_ilog2().filter(|l| 1 << l == file.n);
        let params = log_n
            .and_then(|l| u8::try_from(l).ok())
            .and_then(|l| scrypt::Params::new(l, file.r, file.p).ok())
            .ok_or("the key file's settings are damaged")?;
        let mut user = [0; 64];
        scrypt::scrypt(password.as_bytes(), &base64(&file.salt)?, &params, &mut user).map_err(|e| e.to_string())?;
        let master = Key(user)
            .decrypt(&base64(&file.data)?)
            .map_err(|_| "the password doesn't open this repository".to_string())?;
        let master: MasterKey = serde_json::from_slice(&master).map_err(|e| format!("not a master key: {e}"))?;
        let mut key = [0; 64];
        let parts = [
            (0, 32, base64(&master.encrypt)?),
            (32, 16, base64(&master.mac.k)?),
            (48, 16, base64(&master.mac.r)?),
        ];
        for (at, len, part) in parts {
            if part.len() != len {
                return Err("the master key is damaged".into());
            }
            key[at..at + len].copy_from_slice(&part);
        }
        Ok(Self {
            key: Key(key),
            blobs: HashMap::new(),
        })
    }

    /// The JSON of a config, snapshot or index file, as stored.
    ///
    /// # Errors
    ///
    /// If it doesn't decrypt with this repository's key.
    pub fn file(&self, stored: &[u8]) -> Result<Vec<u8>> {
        let plain = self.key.decrypt(stored)?;
        match plain.first() {
            Some(b'{' | b'[') => Ok(plain),
            Some(2) => unzstd(&plain[1..]),
            _ => Err("an unknown file format".into()),
        }
    }

    /// Take in an index file, as stored. Returns how many blobs it lists.
    ///
    /// # Errors
    ///
    /// If it doesn't decrypt or isn't an index.
    pub fn add_index(&mut self, stored: &[u8]) -> Result<usize> {
        let index: IndexFile = serde_json::from_slice(&self.file(stored)?).map_err(|e| format!("not an index: {e}"))?;
        let mut n = 0;
        for pack in index.packs {
            for blob in pack.blobs {
                _ = self.blobs.insert(
                    id(&blob.id)?,
                    Location {
                        pack: pack.id.clone(),
                        offset: blob.offset,
                        length: blob.length,
                        compressed: blob.uncompressed_length.is_some(),
                    },
                );
                n += 1;
            }
        }
        Ok(n)
    }

    /// Where the blob is, to fetch that range of its pack.
    #[must_use]
    pub fn locate(&self, blob: &str) -> Option<&Location> {
        self.blobs.get(&id(blob).ok()?)
    }

    /// A blob's content from its bytes in the pack: a tree's JSON, or a
    /// piece of a file. Checked against the blob's id, so storage can't
    /// swap one blob for another.
    ///
    /// # Errors
    ///
    /// If the blob isn't indexed, doesn't decrypt, or isn't what its id says.
    pub fn blob(&self, blob: &str, stored: &[u8]) -> Result<Vec<u8>> {
        let want = id(blob)?;
        let at = self.blobs.get(&want).ok_or("not in the index")?;
        let mut plain = self.key.decrypt(stored)?;
        if at.compressed {
            plain = unzstd(&plain)?;
        }
        if Sha256::digest(&plain).as_slice() != want {
            return Err("the blob isn't what its id says".into());
        }
        Ok(plain)
    }
}
