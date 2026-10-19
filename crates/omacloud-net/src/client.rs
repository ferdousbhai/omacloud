//! An account's computer talking to holders: storing, renewing, reading.

use std::{ops::Range, time::Duration};

use anyhow::{Context, Result};
use iroh::{Endpoint, EndpointAddr, endpoint::Connection};
use iroh_blobs::{
    protocol::{ChunkRanges, ChunkRangesExt, GetRequest},
    store::mem::MemStore,
};

use crate::{
    credit::Token,
    node::{about, hash_of},
    proto::{self, BlobId, Reply, Request},
};

/// How long reaching a holder may take: one that's gone shouldn't hold up
/// a read that others can answer.
const CONNECT: Duration = Duration::from_secs(10);

async fn connect(ep: &Endpoint, to: &EndpointAddr, alpn: &[u8]) -> Result<Connection> {
    tokio::time::timeout(CONNECT, ep.connect(to.clone(), alpn))
        .await
        .map_err(|_| anyhow::anyhow!("no answer in {}s", CONNECT.as_secs()))
        .and_then(|c| Ok(c?))
        .with_context(|| format!("reaching {}", about(to)))
}

async fn ask(ep: &Endpoint, to: &EndpointAddr, request: &Request, body: Option<&[u8]>) -> Result<Reply> {
    let conn = connect(ep, to, proto::ALPN).await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    proto::send(&mut send, request).await?;
    if let Some(body) = body {
        send.write_all(body).await?;
    }
    send.finish()?;
    let reply = proto::recv(&mut recv).await;
    conn.close(0u32.into(), b"done");
    reply
}

/// Give `data` (a scrambled blob) to a holder. Returns its id.
///
/// # Errors
///
/// If the holder can't be reached or refuses it.
pub async fn store(ep: &Endpoint, to: &EndpointAddr, data: &[u8]) -> Result<BlobId> {
    let blob = BlobId(*blake3::hash(data).as_bytes());
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

/// Pay a holder to keep `blobs` until `until`; which it keeps.
///
/// # Errors
///
/// If the holder can't be reached or refuses the payment.
pub async fn renew(
    ep: &Endpoint,
    to: &EndpointAddr,
    blobs: Vec<BlobId>,
    until: u64,
    tokens: Vec<Token>,
) -> Result<Vec<bool>> {
    match ask(ep, to, &Request::Renew { blobs, until, tokens }, None).await? {
        Reply::Renewed { kept } => Ok(kept),
        Reply::Refused { why } => anyhow::bail!("{} refused the renewal: {why}", about(to)),
        other => anyhow::bail!("{} answered {other:?}", about(to)),
    }
}

/// Which of `blobs` a holder says it has.
///
/// # Errors
///
/// If the holder can't be reached.
pub async fn has(ep: &Endpoint, to: &EndpointAddr, blobs: Vec<BlobId>) -> Result<Vec<bool>> {
    match ask(ep, to, &Request::Has { blobs }, None).await? {
        Reply::Have { have } => Ok(have),
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
    mem.remote()
        .execute_get(conn.clone(), request)
        .complete()
        .await
        .with_context(|| format!("reading a blob from {}", about(from)))?;
    conn.close(0u32.into(), b"done");
    let data = match range {
        Some(r) => mem.blobs().export_ranges(hash, r).concatenate().await?,
        None => mem.blobs().get_bytes(hash).await?.to_vec(),
    };
    Ok(data)
}
