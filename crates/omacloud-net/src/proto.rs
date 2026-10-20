//! The node protocol: what an account's computer asks a holder. Blob
//! contents are read with iroh-blobs (BLAKE3 verified ranges), which any
//! node serves to anyone; this protocol covers what changes a holder's disk.
//!
//! One request per stream: a length-prefixed JSON request (a store's bytes
//! follow it), then a length-prefixed JSON reply.

use anyhow::{Context, Result};
use iroh::endpoint::{RecvStream, SendStream};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::credit::Token;

pub const ALPN: &[u8] = b"omacloud/node/1";

/// The largest object a holder takes: restic packs stay under 64 MiB.
pub const MAX_OBJECT: u64 = 128 << 20;

/// The largest request or reply, bytes of a store aside.
const MAX_MESSAGE: u32 = 4 << 20;

/// A blob id, as hex on the wire.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlobId(#[serde(with = "hex::serde")] pub [u8; 32]);

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Keep a blob, `size` bytes, which follow on the stream. It's kept
    /// for a first, short lease, unpaid; a renewal keeps it longer, paying
    /// for the time it was kept.
    Store { blob: BlobId, size: u64 },
    /// Keep these blobs until `until` (unix seconds), paying with `tokens`
    /// for the time they were kept since last paid for: a holder is paid
    /// for what it served, not ahead. Asked without enough, a holder
    /// answers [`Reply::Owed`].
    Renew {
        blobs: Vec<BlobId>,
        until: u64,
        tokens: Vec<Token>,
    },
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    Stored,
    /// Per blob asked, whether it's kept now, and how many of the tokens
    /// given it took (the first ones: no more than owed; none said, from
    /// an older holder: all).
    Renewed {
        kept: Vec<bool>,
        #[serde(default)]
        taken: Option<u64>,
    },
    Refused {
        why: String,
    },
    /// What a renewal of these blobs costs now, in tokens.
    Owed {
        tokens: u64,
    },
}

pub(crate) async fn send<T: Serialize>(s: &mut SendStream, msg: &T) -> Result<()> {
    let body = serde_json::to_vec(msg)?;
    let len = u32::try_from(body.len()).context("message too long")?;
    s.write_all(&len.to_be_bytes()).await?;
    s.write_all(&body).await?;
    Ok(())
}

pub(crate) async fn recv<T: DeserializeOwned>(r: &mut RecvStream) -> Result<T> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len);
    anyhow::ensure!(len <= MAX_MESSAGE, "message of {len} bytes is too long");
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}
