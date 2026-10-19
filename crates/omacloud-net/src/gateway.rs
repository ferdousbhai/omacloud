//! The bucket as S3, on this computer's loopback: what restic (through
//! OpenDAL), rustic or plain restic talk to. Only the calls they make are
//! served: objects (get, ranges, head, put, delete), ListObjectsV2, batch
//! delete and multipart uploads.
//!
//! It answers only on 127.0.0.1. The access key a request names is the
//! epoch's network key (made from the repository password): it picks the
//! bucket's scope at the Worker and the keys that scramble and seal, so one
//! gateway serves any epoch of any account whose key it's given, and a
//! made-up key reaches an empty bucket of its own. Requests aren't checked
//! for a SigV4 signature: whoever has the key can sign.

use std::{
    collections::{BTreeSet, HashMap},
    convert::Infallible,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use anyhow::Result;
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{Method, Request, Response, StatusCode, body::Incoming, header, service::service_fn};
use hyper_util::rt::TokioIo;
use iroh::Endpoint;
use tokio::net::TcpListener;

use crate::{blob::NodeId, bucket::Bucket, proto::MAX_OBJECT, registry::Registry, seal::Keys};

/// The name the gateway's one bucket answers to.
pub const BUCKET: &str = "omacloud";

/// A running gateway.
pub struct Gateway {
    addr: SocketAddr,
    state: Arc<State>,
    task: tokio::task::JoinHandle<()>,
}

impl Gateway {
    /// Serve buckets on a free loopback port: through `ep`, with holders
    /// from `registry`, `copies` of each object, never on `own` nodes.
    ///
    /// # Errors
    ///
    /// If no port can be bound.
    pub async fn start(
        ep: Endpoint,
        registry: Arc<dyn Registry>,
        own: BTreeSet<NodeId>,
        copies: usize,
    ) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let state = Arc::new(State {
            ep,
            registry,
            own: Arc::new(std::sync::RwLock::new(own)),
            avoid: Arc::default(),
            copies,
            buckets: Mutex::default(),
            uploads: Mutex::default(),
        });
        let serving = state.clone();
        let task = tokio::spawn(async move {
            let state = serving;
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    continue;
                };
                let state = state.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |req| {
                        let state = state.clone();
                        async move { Ok::<_, Infallible>(state.handle(req).await) }
                    });
                    if let Err(e) = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await
                    {
                        log::debug!("gateway connection: {e}");
                    }
                });
            }
        });
        Ok(Self { addr, state, task })
    }

    /// Never choose these holders for new copies, from now on.
    pub fn set_avoid(&self, avoid: BTreeSet<NodeId>) {
        *self.state.avoid.write().expect("avoid") = avoid;
    }

    /// The account's own nodes are these, from now on.
    pub fn set_own(&self, own: BTreeSet<NodeId>) {
        *self.state.own.write().expect("own") = own;
    }

    /// The bucket a key reaches.
    #[must_use]
    pub fn bucket(&self, access_key: &str) -> Arc<Bucket> {
        self.state.bucket(access_key)
    }

    /// The S3 endpoint URL.
    #[must_use]
    pub fn endpoint(&self) -> String {
        format!("http://{}", self.addr)
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct State {
    ep: Endpoint,
    registry: Arc<dyn Registry>,
    own: crate::bucket::Own,
    /// Holders that failed this computer lately: never chosen again.
    avoid: crate::bucket::Own,
    copies: usize,
    /// One bucket per key used.
    buckets: Mutex<HashMap<String, Arc<Bucket>>>,
    /// Multipart uploads in progress, by id.
    uploads: Mutex<HashMap<String, Upload>>,
}

/// A multipart upload's key, and its parts by number.
type Upload = (String, HashMap<u32, Bytes>);

type Reply = Response<Full<Bytes>>;

fn reply(status: StatusCode, body: impl Into<Bytes>) -> Reply {
    let mut r = Response::new(Full::new(body.into()));
    *r.status_mut() = status;
    r
}

fn error(status: StatusCode, code: &str, message: &str) -> Reply {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Error><Code>{code}</Code><Message>{}</Message></Error>",
        escape(message)
    );
    let mut r = reply(status, body);
    r.headers_mut()
        .insert(header::CONTENT_TYPE, "application/xml".parse().expect("header"));
    r
}

fn xml(body: String) -> Reply {
    let mut r = reply(
        StatusCode::OK,
        format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n{body}"),
    );
    r.headers_mut()
        .insert(header::CONTENT_TYPE, "application/xml".parse().expect("header"));
    r
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// `%xx` decoding, as S3 paths and queries are encoded; in a query, `+`
/// is a space too.
fn decode(s: &str, in_query: bool) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
        } else if in_query && b[i] == b'+' {
            out.push(b' ');
            i += 1;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query(req: &Request<Incoming>) -> HashMap<String, String> {
    req.uri()
        .query()
        .unwrap_or("")
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (decode(k, true), decode(v, true))
        })
        .collect()
}

/// A Range header against an object of `size` bytes: the bytes to send,
/// or None for all of them. Err when it can't be satisfied.
fn range(header: Option<&str>, size: u64) -> Result<Option<std::ops::Range<u64>>, ()> {
    let Some(spec) = header.and_then(|h| h.strip_prefix("bytes=")) else {
        return Ok(None);
    };
    let (a, b) = spec.split_once('-').ok_or(())?;
    let r = match (a.parse::<u64>(), b.parse::<u64>()) {
        (Ok(a), Ok(b)) => a..(b + 1).min(size),
        (Ok(a), Err(_)) if b.is_empty() => a..size,
        (Err(_), Ok(n)) if a.is_empty() => size.saturating_sub(n)..size,
        _ => return Err(()),
    };
    if r.start >= size || r.start >= r.end {
        return Err(());
    }
    Ok(Some(r))
}

impl State {
    fn bucket(&self, access_key: &str) -> Arc<Bucket> {
        self.buckets
            .lock()
            .expect("buckets")
            .entry(access_key.to_string())
            .or_insert_with(|| {
                Arc::new(
                    Bucket::new(
                        self.ep.clone(),
                        self.registry.clone(),
                        Keys::from_secret(access_key),
                        self.own.clone(),
                        self.copies,
                    )
                    .avoiding(self.avoid.clone()),
                )
            })
            .clone()
    }

    async fn handle(&self, req: Request<Incoming>) -> Reply {
        match self.route(req).await {
            Ok(r) => r,
            Err(e) => {
                log::warn!("gateway: {e:#}");
                error(StatusCode::SERVICE_UNAVAILABLE, "ServiceUnavailable", &format!("{e:#}"))
            }
        }
    }

    async fn route(&self, req: Request<Incoming>) -> Result<Reply> {
        let key = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.split("Credential=").nth(1))
            .and_then(|c| c.split('/').next())
            .filter(|k| k.len() >= 32)
            .map(str::to_string);
        let Some(key) = key else {
            return Ok(error(StatusCode::FORBIDDEN, "InvalidAccessKeyId", "no network key"));
        };
        let bucket = self.bucket(&key);
        self.serve(&bucket, req).await
    }

    async fn serve(&self, bucket: &Bucket, req: Request<Incoming>) -> Result<Reply> {
        let path = decode(req.uri().path(), false);
        let rest = path.trim_start_matches('/');
        let (name, key) = rest.split_once('/').unwrap_or((rest, ""));
        if name != BUCKET {
            return Ok(error(StatusCode::NOT_FOUND, "NoSuchBucket", name));
        }
        let key = key.to_string();
        let q = query(&req);
        let method = req.method().clone();
        match (method, key.is_empty()) {
            (Method::GET, true) => self.list(bucket, &q).await,
            (Method::POST, true) if q.contains_key("delete") => self.delete_many(bucket, req).await,
            (Method::POST, false) if q.contains_key("uploads") => Ok(self.begin_upload(&key)),
            (Method::POST, false) if q.contains_key("uploadId") => self.finish_upload(bucket, &key, &q).await,
            (Method::PUT, false) if q.contains_key("uploadId") => self.put_part(req, &q).await,
            (Method::DELETE, false) if q.contains_key("uploadId") => {
                self.uploads.lock().expect("uploads").remove(&q["uploadId"]);
                Ok(reply(StatusCode::NO_CONTENT, Bytes::new()))
            }
            (Method::GET | Method::HEAD, false) => self.get(bucket, &req, &key).await,
            (Method::PUT, false) => {
                let data = body(req).await?;
                bucket.put(&key, &data).await?;
                Ok(etag(reply(StatusCode::OK, Bytes::new()), &data))
            }
            (Method::DELETE, false) => {
                bucket.delete(&key).await?;
                Ok(reply(StatusCode::NO_CONTENT, Bytes::new()))
            }
            _ => Ok(error(StatusCode::NOT_IMPLEMENTED, "NotImplemented", "not served here")),
        }
    }

    async fn get(&self, bucket: &Bucket, req: &Request<Incoming>, key: &str) -> Result<Reply> {
        let Some(entry) = bucket.entry(key).await? else {
            return Ok(error(StatusCode::NOT_FOUND, "NoSuchKey", key));
        };
        let wanted = req.headers().get(header::RANGE).and_then(|h| h.to_str().ok());
        let r = match range(wanted, entry.size) {
            Ok(r) => r,
            Err(()) if entry.size == 0 => None,
            Err(()) => return Ok(error(StatusCode::RANGE_NOT_SATISFIABLE, "InvalidRange", key)),
        };
        let len = r.as_ref().map_or(entry.size, |r| r.end - r.start);
        let mut res = if *req.method() == Method::HEAD {
            reply(StatusCode::OK, Bytes::new())
        } else {
            let data = if entry.size == 0 {
                Vec::new()
            } else {
                bucket.read(&entry, r.clone()).await?
            };
            reply(
                if r.is_some() {
                    StatusCode::PARTIAL_CONTENT
                } else {
                    StatusCode::OK
                },
                data,
            )
        };
        let h = res.headers_mut();
        h.insert(header::CONTENT_LENGTH, len.into());
        h.insert(header::LAST_MODIFIED, "Thu, 01 Jan 2026 00:00:00 GMT".parse()?);
        if let Some(r) = &r {
            h.insert(
                header::CONTENT_RANGE,
                format!("bytes {}-{}/{}", r.start, r.end - 1, entry.size).parse()?,
            );
        }
        if let Some(first) = entry.holders.first() {
            h.insert(
                header::ETAG,
                format!("\"{}\"", hex::encode(&first.blob.0[..16])).parse()?,
            );
        }
        Ok(res)
    }

    async fn list(&self, bucket: &Bucket, q: &HashMap<String, String>) -> Result<Reply> {
        let prefix = q.get("prefix").cloned().unwrap_or_default();
        let after = q
            .get("continuation-token")
            .and_then(|t| hex::decode(t).ok())
            .map(|t| String::from_utf8_lossy(&t).into_owned())
            .or_else(|| q.get("start-after").cloned())
            .unwrap_or_default();
        let max: usize = q.get("max-keys").and_then(|m| m.parse().ok()).unwrap_or(1000).min(1000);
        let delimiter = q.get("delimiter").filter(|d| !d.is_empty()).cloned();
        let entries = bucket.list(&prefix).await?;
        let mut contents = String::new();
        let mut prefixes = std::collections::BTreeSet::new();
        let mut count = 0;
        let mut last = None;
        let mut truncated = false;
        for e in entries.iter().filter(|e| e.key > after) {
            if count == max {
                truncated = true;
                break;
            }
            if let Some(d) = &delimiter
                && let Some(i) = e.key[prefix.len()..].find(d.as_str())
            {
                let common = &e.key[..prefix.len() + i + d.len()];
                if prefixes.insert(common.to_string()) {
                    count += 1;
                }
                last = Some(e.key.clone());
                continue;
            }
            contents.push_str(&format!(
                "<Contents><Key>{}</Key><Size>{}</Size><LastModified>2026-01-01T00:00:00.000Z</LastModified></Contents>",
                escape(&e.key),
                e.size
            ));
            count += 1;
            last = Some(e.key.clone());
        }
        let common: String = prefixes
            .iter()
            .map(|p| format!("<CommonPrefixes><Prefix>{}</Prefix></CommonPrefixes>", escape(p)))
            .collect();
        let next = match (truncated, last) {
            (true, Some(k)) => format!("<NextContinuationToken>{}</NextContinuationToken>", hex::encode(k)),
            _ => String::new(),
        };
        Ok(xml(format!(
            "<ListBucketResult><Name>{BUCKET}</Name><Prefix>{}</Prefix><KeyCount>{count}</KeyCount>\
             <MaxKeys>{max}</MaxKeys><IsTruncated>{truncated}</IsTruncated>{next}{contents}{common}</ListBucketResult>",
            escape(&prefix)
        )))
    }

    async fn delete_many(&self, bucket: &Bucket, req: Request<Incoming>) -> Result<Reply> {
        let body = String::from_utf8_lossy(&body(req).await?).into_owned();
        let mut deleted = String::new();
        for part in body.split("<Key>").skip(1) {
            let key = unescape(part.split("</Key>").next().unwrap_or(""));
            bucket.delete(&key).await?;
            deleted.push_str(&format!("<Deleted><Key>{}</Key></Deleted>", escape(&key)));
        }
        Ok(xml(format!("<DeleteResult>{deleted}</DeleteResult>")))
    }

    fn begin_upload(&self, key: &str) -> Reply {
        let id = hex::encode(rand::random::<[u8; 16]>());
        self.uploads
            .lock()
            .expect("uploads")
            .insert(id.clone(), (key.to_string(), HashMap::new()));
        xml(format!(
            "<InitiateMultipartUploadResult><Bucket>{BUCKET}</Bucket><Key>{}</Key><UploadId>{id}</UploadId></InitiateMultipartUploadResult>",
            escape(key)
        ))
    }

    async fn put_part(&self, req: Request<Incoming>, q: &HashMap<String, String>) -> Result<Reply> {
        let n: u32 = q.get("partNumber").and_then(|n| n.parse().ok()).unwrap_or(0);
        let id = q["uploadId"].clone();
        let data = body(req).await?;
        let mut uploads = self.uploads.lock().expect("uploads");
        let Some((_, parts)) = uploads.get_mut(&id) else {
            return Ok(error(StatusCode::NOT_FOUND, "NoSuchUpload", &id));
        };
        parts.insert(n, data.clone());
        Ok(etag(reply(StatusCode::OK, Bytes::new()), &data))
    }

    async fn finish_upload(&self, bucket: &Bucket, key: &str, q: &HashMap<String, String>) -> Result<Reply> {
        let id = &q["uploadId"];
        let Some((k, parts)) = self.uploads.lock().expect("uploads").remove(id) else {
            return Ok(error(StatusCode::NOT_FOUND, "NoSuchUpload", id));
        };
        anyhow::ensure!(k == key, "upload {id} is for another key");
        let mut numbers: Vec<_> = parts.keys().copied().collect();
        numbers.sort_unstable();
        let mut data = Vec::new();
        for n in numbers {
            data.extend_from_slice(&parts[&n]);
        }
        bucket.put(key, &data).await?;
        Ok(xml(format!(
            "<CompleteMultipartUploadResult><Bucket>{BUCKET}</Bucket><Key>{}</Key><ETag>\"{}\"</ETag></CompleteMultipartUploadResult>",
            escape(key),
            tag_of(&data)
        )))
    }
}

/// An object's entity tag, without its quotes.
fn tag_of(data: &[u8]) -> String {
    hex::encode(&blake3::hash(data).as_bytes()[..16])
}

fn etag(mut r: Reply, data: &[u8]) -> Reply {
    if let Ok(v) = format!("\"{}\"", tag_of(data)).parse() {
        r.headers_mut().insert(header::ETAG, v);
    }
    r
}

async fn body(req: Request<Incoming>) -> Result<Bytes> {
    let limit = usize::try_from(MAX_OBJECT)?;
    Limited::new(req.into_body(), limit)
        .collect()
        .await
        .map(http_body_util::Collected::to_bytes)
        .map_err(|e| anyhow::anyhow!("reading the request: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_as_s3_reads_them() {
        assert_eq!(range(None, 10), Ok(None));
        assert_eq!(range(Some("bytes=2-4"), 10), Ok(Some(2..5)));
        assert_eq!(range(Some("bytes=2-"), 10), Ok(Some(2..10)));
        assert_eq!(range(Some("bytes=-3"), 10), Ok(Some(7..10)));
        assert_eq!(range(Some("bytes=5-100"), 10), Ok(Some(5..10)));
        assert_eq!(range(Some("bytes=10-12"), 10), Err(()));
        assert_eq!(range(Some("bytes=x"), 10), Err(()));
    }

    #[test]
    fn keys_decode_and_xml_escapes() {
        assert_eq!(decode("data%2Fab%20c+d", false), "data/ab c+d");
        assert_eq!(decode("a+b", true), "a b");
        assert_eq!(unescape(&escape("a<&>'\"b")), "a<&>'\"b");
    }
}
