//! The Worker over HTTPS (docs/network-api.md): the node directory and
//! heartbeats, the sealed index, coordination records, and tokens.

use std::time::Duration;

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use iroh::{EndpointAddr, SecretKey};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    credit::{self, IssuerKey, Token},
    records::Records,
    registry::{Listed, Registry},
};

/// Where the network's Worker answers.
pub const URL: &str = "https://omacloud.computer";

pub struct Worker {
    base: String,
    agent: ureq::Agent,
}

/// A request's answer: status and JSON body (Null when empty).
struct Answer {
    status: u16,
    body: Value,
}

impl Worker {
    #[must_use]
    pub fn new(base: &str) -> Self {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(60)))
            .build()
            .into();
        Self {
            base: base.trim_end_matches('/').to_string(),
            agent,
        }
    }

    fn call(&self, method: &str, path: &str, headers: &[(&str, String)], body: Option<&Value>) -> Result<Answer> {
        let url = format!("{}{path}", self.base);
        let mut req = ureq::http::Request::builder().method(method).uri(&url);
        for (k, v) in headers {
            req = req.header(*k, v);
        }
        let bytes = body.map(serde_json::to_vec).transpose()?.unwrap_or_default();
        if body.is_some() {
            req = req.header("content-type", "application/json");
        }
        let mut res = self
            .agent
            .run(req.body(bytes)?)
            .with_context(|| format!("reaching the network at {}", self.base))?;
        let status = res.status().as_u16();
        let text = res.body_mut().read_to_string().unwrap_or_default();
        let body = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::String(text))
        };
        if status >= 500 {
            anyhow::bail!("the network's Worker failed ({status}): {}", message(&body));
        }
        Ok(Answer { status, body })
    }

    fn ok(&self, method: &str, path: &str, headers: &[(&str, String)], body: Option<&Value>) -> Result<Value> {
        let a = self.call(method, path, headers, body)?;
        anyhow::ensure!(
            (200..300).contains(&a.status),
            "the network refused {method} {path} ({}): {}",
            a.status,
            message(&a.body)
        );
        Ok(a.body)
    }

    /// Tell the directory this node is up, where, and how much room it
    /// has. Answers the network and prefix the Worker saw it on.
    ///
    /// # Errors
    ///
    /// If the Worker can't be reached or refuses it.
    pub fn heartbeat(&self, key: &SecretKey, addr: &EndpointAddr, capacity: u64) -> Result<(u32, String)> {
        let path = "/api/net/nodes";
        let body = json!({ "addr": addr, "capacity": capacity });
        let bytes = serde_json::to_vec(&body)?;
        let time = crate::node::now().to_string();
        let signed = format!(
            "omacloud-node-v1\nPOST\n{path}\n{time}\n{}",
            hex::encode(Sha256::digest(&bytes))
        );
        let sig = key.sign(signed.as_bytes());
        let headers = [
            ("x-node", hex::encode(key.public().as_bytes())),
            ("x-time", time),
            ("x-sig", hex::encode(sig.to_bytes())),
        ];
        let v = self.ok("POST", path, &headers, Some(&body))?;
        Ok((
            u32::try_from(v["asn"].as_u64().unwrap_or(0))?,
            v["prefix"].as_str().unwrap_or_default().to_string(),
        ))
    }

    /// The Worker's token key.
    ///
    /// # Errors
    ///
    /// If it can't be fetched or isn't a key.
    pub fn issuer(&self) -> Result<IssuerKey> {
        let v = self.ok("GET", "/api/net/credits/key", &[], None)?;
        let der = STANDARD.decode(v["spki"].as_str().context("no key")?)?;
        // WebCrypto exports a plain RSA key; RFC 9474 also allows the PSS form
        IssuerKey::from_der(&der)
            .or_else(|_| IssuerKey::from_spki(&der))
            .map_err(|e| anyhow::anyhow!("the token key: {e}"))
    }

    /// The starter tokens a new install may have.
    ///
    /// # Errors
    ///
    /// If the grant for this network is used up, or the Worker refuses.
    pub fn starter(&self, issuer: &IssuerKey, n: usize) -> Result<Vec<Token>> {
        let (blind, pending) = credit::ask(issuer, n)?;
        let v = self.ok(
            "POST",
            "/api/net/credits/starter",
            &[],
            Some(&json!({ "blind": blind.iter().map(hex::encode).collect::<Vec<_>>() })),
        )?;
        pending.finish(issuer, &signed(&v)?)
    }

    /// Trade tokens taken as a holder for fresh ones the Worker can't link
    /// to them.
    ///
    /// # Errors
    ///
    /// If a token was spent already or isn't the Worker's.
    pub fn exchange(&self, issuer: &IssuerKey, tokens: &[Token]) -> Result<Vec<Token>> {
        let (blind, pending) = credit::ask(issuer, tokens.len())?;
        let v = self.ok(
            "POST",
            "/api/net/credits/exchange",
            &[],
            Some(&json!({
                "tokens": tokens,
                "blind": blind.iter().map(hex::encode).collect::<Vec<_>>(),
            })),
        )?;
        pending.finish(issuer, &signed(&v)?)
    }
}

fn signed(v: &Value) -> Result<Vec<Vec<u8>>> {
    v["signed"]
        .as_array()
        .context("no signatures")?
        .iter()
        .map(|s| Ok(hex::decode(s.as_str().context("not a signature")?)?))
        .collect()
}

fn message(body: &Value) -> String {
    body["error"].as_str().map_or_else(|| body.to_string(), str::to_string)
}

fn from<T: DeserializeOwned>(v: Value) -> Result<T> {
    Ok(serde_json::from_value(v)?)
}

#[derive(Deserialize)]
struct Nodes {
    nodes: Vec<Listed>,
}

#[derive(Serialize, Deserialize)]
struct Sealed {
    id: Option<String>,
    sealed: String,
}

#[derive(Deserialize)]
struct Logged {
    seq: u64,
    body: String,
}

#[derive(Deserialize)]
struct Named {
    name: String,
    body: String,
}

impl Registry for Worker {
    fn nodes(&self) -> Result<Vec<Listed>> {
        Ok(from::<Nodes>(self.ok("GET", "/api/net/nodes", &[], None)?)?.nodes)
    }
    fn index(&self, scope: &str) -> Result<Vec<(String, Vec<u8>)>> {
        // in pages, each starting after the last one's end
        let mut out = Vec::new();
        let mut after = String::new();
        loop {
            let path = if after.is_empty() {
                "/api/net/index".to_string()
            } else {
                format!("/api/net/index?after={after}")
            };
            let v = self.ok("GET", &path, &[("x-scope", scope.to_string())], None)?;
            let entries: Vec<Sealed> = from(v["entries"].clone())?;
            for e in entries {
                out.push((e.id.context("an entry without an id")?, STANDARD.decode(e.sealed)?));
            }
            match v["next"].as_str() {
                Some(next) if next > after.as_str() => after = next.to_string(),
                _ => return Ok(out),
            }
        }
    }
    fn entry(&self, scope: &str, id: &str) -> Result<Option<Vec<u8>>> {
        let a = self.call(
            "GET",
            &format!("/api/net/index/{id}"),
            &[("x-scope", scope.to_string())],
            None,
        )?;
        match a.status {
            404 => Ok(None),
            200 => Ok(Some(STANDARD.decode(from::<Sealed>(a.body)?.sealed)?)),
            s => anyhow::bail!("the network refused the index entry ({s}): {}", message(&a.body)),
        }
    }
    fn put_entry(&self, scope: &str, id: &str, sealed: &[u8]) -> Result<()> {
        self.ok(
            "PUT",
            &format!("/api/net/index/{id}"),
            &[("x-scope", scope.to_string())],
            Some(&json!({ "sealed": STANDARD.encode(sealed) })),
        )?;
        Ok(())
    }
    fn delete_entry(&self, scope: &str, id: &str) -> Result<()> {
        self.ok(
            "DELETE",
            &format!("/api/net/index/{id}"),
            &[("x-scope", scope.to_string())],
            None,
        )?;
        Ok(())
    }
}

impl Records for Worker {
    fn create_account(&self, account: &str, cap: &str) -> Result<bool> {
        let a = self.call(
            "PUT",
            &format!("/api/net/coord/{account}"),
            &[("x-cap", cap.to_string())],
            None,
        )?;
        match a.status {
            201 => Ok(true),
            409 => Ok(false),
            s => anyhow::bail!("the network refused the account ({s}): {}", message(&a.body)),
        }
    }
    fn replace_cap(&self, account: &str, cap: &str, new: &str) -> Result<()> {
        self.ok(
            "PUT",
            &format!("/api/net/coord/{account}/cap"),
            &[("x-cap", cap.to_string())],
            Some(&json!({ "cap": new })),
        )?;
        Ok(())
    }
    fn log(&self, account: &str, log: &str, after: u64) -> Result<Vec<(u64, String)>> {
        let v = self.ok(
            "GET",
            &format!("/api/net/coord/{account}/{log}?after={after}"),
            &[],
            None,
        )?;
        Ok(from::<Vec<Logged>>(v["records"].clone())?
            .into_iter()
            .map(|r| (r.seq, r.body))
            .collect())
    }
    fn append(&self, account: &str, cap: &str, log: &str, seq: u64, body: &str) -> Result<bool> {
        let a = self.call(
            "PUT",
            &format!("/api/net/coord/{account}/{log}/{seq}"),
            &[("x-cap", cap.to_string())],
            Some(&json!({ "body": body })),
        )?;
        match a.status {
            201 => Ok(true),
            409 => Ok(false),
            s => anyhow::bail!("the network refused {log} {seq} ({s}): {}", message(&a.body)),
        }
    }
    fn anchor(&self, account: &str) -> Result<Option<String>> {
        let a = self.call("GET", &format!("/api/net/coord/{account}/anchor"), &[], None)?;
        match a.status {
            404 => Ok(None),
            200 => Ok(a.body["body"].as_str().map(str::to_string)),
            s => anyhow::bail!("the network refused the anchor ({s}): {}", message(&a.body)),
        }
    }
    fn set_anchor(&self, account: &str, cap: &str, body: &str) -> Result<bool> {
        let a = self.call(
            "PUT",
            &format!("/api/net/coord/{account}/anchor"),
            &[("x-cap", cap.to_string())],
            Some(&json!({ "body": body })),
        )?;
        match a.status {
            201 => Ok(true),
            409 => Ok(false),
            s => anyhow::bail!("the network refused the anchor ({s}): {}", message(&a.body)),
        }
    }
    fn named(&self, account: &str, kind: &str, prefix: &str) -> Result<Vec<(String, String)>> {
        let query = if prefix.is_empty() {
            String::new()
        } else {
            format!("?name={prefix}")
        };
        let v = self.ok("GET", &format!("/api/net/coord/{account}/{kind}{query}"), &[], None)?;
        Ok(from::<Vec<Named>>(v["records"].clone())?
            .into_iter()
            .map(|r| (r.name, r.body))
            .collect())
    }
    fn put_named(&self, account: &str, cap: Option<&str>, kind: &str, name: &str, body: &str) -> Result<()> {
        let headers: Vec<(&str, String)> = cap.map(|c| ("x-cap", c.to_string())).into_iter().collect();
        self.ok(
            "PUT",
            &format!("/api/net/coord/{account}/{kind}/{name}"),
            &headers,
            Some(&json!({ "body": body })),
        )?;
        Ok(())
    }
    fn delete_named(&self, account: &str, cap: &str, kind: &str, name: &str) -> Result<()> {
        self.ok(
            "DELETE",
            &format!("/api/net/coord/{account}/{kind}/{name}"),
            &[("x-cap", cap.to_string())],
            None,
        )?;
        Ok(())
    }
    fn marker(&self, account: &str) -> Result<u64> {
        let v = self.ok("GET", &format!("/api/net/coord/{account}/marker"), &[], None)?;
        v["version"].as_u64().context("no version")
    }
}
