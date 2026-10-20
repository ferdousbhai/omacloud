//! An account's computer talking to holders: storing, renewing, reading.

use std::{ops::Range, time::Duration};

use anyhow::{Context, Result};
use futures_util::StreamExt;
use iroh::{Endpoint, EndpointAddr, endpoint::Connection};
use iroh_blobs::{
    api::remote::GetProgressItem,
    protocol::{ChunkRanges, ChunkRangesExt, GetRequest},
    store::mem::MemStore,
};

use crate::{
    credit::Token,
    node::hash_of,
    proto::{self, BlobId, Reply, Request},
};

/// How long reaching a holder may take: one that's gone shouldn't hold up
/// a read that others can answer.
const CONNECT: Duration = Duration::from_secs(10);

/// Read context for errors about a node.
fn about(addr: &EndpointAddr) -> String {
    format!("node {}", addr.id.fmt_short())
}

async fn connect(ep: &Endpoint, to: &EndpointAddr, alpn: &[u8]) -> Result<Connection> {
    tokio::time::timeout(CONNECT, ep.connect(to.clone(), alpn))
        .await
        .map_err(|_| anyhow::anyhow!("no answer in {}s", CONNECT.as_secs()))
        .and_then(|c| Ok(c?))
        .with_context(|| format!("reaching {}", about(to)))
}

/// How long a holder may go quiet once reached: holders are untrusted,
/// and one that never answers mustn't hold up a store or a read.
const STALL: Duration = Duration::from_secs(60);
/// The slowest pace a body is given time for, in bytes a second.
const PACE: u64 = 64 << 10;

async fn ask(ep: &Endpoint, to: &EndpointAddr, request: &Request, body: Option<&[u8]>) -> Result<Reply> {
    let conn = connect(ep, to, proto::ALPN).await?;
    let limit = STALL + Duration::from_secs(body.map_or(0, |b| b.len() as u64 / PACE));
    let reply = tokio::time::timeout(limit, exchange(&conn, request, body))
        .await
        .map_err(|_| anyhow::anyhow!("{} didn't answer in {}s", about(to), limit.as_secs()))
        .and_then(|r| r);
    conn.close(0u32.into(), b"done");
    reply
}

async fn exchange(conn: &Connection, request: &Request, body: Option<&[u8]>) -> Result<Reply> {
    let (mut send, mut recv) = conn.open_bi().await?;
    proto::send(&mut send, request).await?;
    // a holder that won't take the body (it has the blob, or no room)
    // answers first and stops reading: its answer is what counts
    let sent = async {
        if let Some(body) = body {
            send.write_all(body).await?;
        }
        send.finish()?;
        anyhow::Ok(())
    }
    .await;
    proto::recv(&mut recv).await.map_err(|e| sent.err().unwrap_or(e))
}

/// Give `data` (a scrambled blob) to a holder. Returns its id.
///
/// # Errors
///
/// If the holder can't be reached or refuses it.
pub async fn store(ep: &Endpoint, to: &EndpointAddr, data: &[u8]) -> Result<BlobId> {
    let blob = BlobId(crate::blob::blob_id(data));
    let request = Request::Store {
        blob,
        size: data.len() as u64,
    };
    match ask(ep, to, &request, Some(data)).await? {
        Reply::Stored => Ok(blob),
        Reply::Refused { why } => anyhow::bail!("{} refused the blob: {why}", about(to)),
        other => anyhow::bail!("{} answered {other:?}", about(to)),
    }
}

/// What a holder said to a renewal.
#[derive(Debug, PartialEq, Eq)]
pub enum Renewal {
    /// Per blob, whether it's kept until the date asked.
    Kept(Vec<bool>),
    /// Not enough was paid: this many tokens are owed.
    Owed(u64),
}

/// Ask a holder to keep `blobs` until `until`, paying with `tokens` for
/// the time it kept them since last paid.
///
/// # Errors
///
/// If the holder can't be reached or refuses.
pub async fn renew(
    ep: &Endpoint,
    to: &EndpointAddr,
    blobs: Vec<BlobId>,
    until: u64,
    tokens: Vec<Token>,
) -> Result<Renewal> {
    match ask(ep, to, &Request::Renew { blobs, until, tokens }, None).await? {
        Reply::Renewed { kept } => Ok(Renewal::Kept(kept)),
        Reply::Owed { tokens } => Ok(Renewal::Owed(tokens)),
        Reply::Refused { why } => anyhow::bail!("{} refused the renewal: {why}", about(to)),
        other => anyhow::bail!("{} answered {other:?}", about(to)),
    }
}

/// Read a blob from a holder, all of it or `range`, every byte checked
/// against the blob's BLAKE3 tree on the way in: a holder can't serve
/// anything but the blob's own bytes. An audit is the same read.
///
/// # Errors
///
/// If the holder can't be reached, doesn't have the blob, or sends bytes
/// that don't verify.
pub async fn read(ep: &Endpoint, from: &EndpointAddr, blob: BlobId, range: Option<Range<u64>>) -> Result<Vec<u8>> {
    let hash = hash_of(&blob);
    let conn = connect(ep, from, iroh_blobs::ALPN).await?;
    let mem = MemStore::new();
    let request = match &range {
        Some(r) => GetRequest::blob_ranges(hash, ChunkRanges::bytes(r.clone())),
        None => GetRequest::blob(hash),
    };
    let got = async {
        let mut progress = std::pin::pin!(mem.remote().execute_get(conn.clone(), request).stream());
        loop {
            match tokio::time::timeout(STALL, progress.next()).await {
                Err(_) => anyhow::bail!("nothing came in {}s", STALL.as_secs()),
                Ok(Some(GetProgressItem::Progress(_))) => {}
                Ok(Some(GetProgressItem::Done(_))) => return Ok(()),
                Ok(Some(GetProgressItem::Error(e))) => return Err(e.into()),
                Ok(None) => anyhow::bail!("the read ended early"),
            }
        }
    }
    .await
    .with_context(|| format!("reading a blob from {}", about(from)));
    conn.close(0u32.into(), b"done");
    got?;
    let data = match range {
        Some(r) => mem.blobs().export_ranges(hash, r).concatenate().await?,
        None => mem.blobs().get_bytes(hash).await?.to_vec(),
    };
    Ok(data)
}
