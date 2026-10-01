//! The coordinator service over HTTP (`service/`'s `Account` Durable
//! Object). Every request is signed with this device's key (see
//! [`crate::auth`]); the service verifies every write against the same rules
//! the client applies, and the client still verifies every read.

use std::{
    sync::RwLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use ed25519_dalek::SigningKey;
use serde::{Serialize, de::DeserializeOwned};
use ureq::Agent;

use crate::{
    auth,
    devices::{Anchor, DeviceEntry, JoinRequest},
    epoch::{EpochRecord, Grant},
    head::{Coordinator, Head},
    sync::Membership,
};

/// Longest a head wait may take on the service, plus a margin.
const WAIT_TIMEOUT: Duration = Duration::from_secs(40);

pub struct HttpCoordinator {
    base: String,
    /// The account root (hex); set by `set_anchor` when creating one.
    account: RwLock<Option<String>>,
    key: SigningKey,
    agent: Agent,
    /// Sent when creating an account, which needs one during the beta.
    invite: Option<String>,
}

enum Reply {
    Ok(Vec<u8>),
    Conflict,
    NotFound,
}

impl HttpCoordinator {
    /// A client for `base` (like `https://coordinator.example`), talking
    /// about `account` (root public key, hex; `None` until one is created),
    /// signing requests with `key`.
    #[must_use]
    pub fn new(base: &str, account: Option<String>, key: SigningKey) -> Self {
        let agent = Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(WAIT_TIMEOUT))
            .build()
            .new_agent();
        Self {
            base: base.trim_end_matches('/').to_string(),
            account: RwLock::new(account),
            key,
            agent,
            invite: None,
        }
    }

    /// Send `code` when creating an account.
    #[must_use]
    pub fn with_invite(mut self, code: Option<String>) -> Self {
        self.invite = code;
        self
    }

    fn path(&self, rest: &str) -> Result<String> {
        let account = self
            .account
            .read()
            .unwrap()
            .clone()
            .context("no account chosen for the coordinator yet")?;
        Ok(format!("/v1/accounts/{account}/{rest}"))
    }

    fn call(&self, method: &str, rest: &str, body: Option<Vec<u8>>) -> Result<Reply> {
        let path = self.path(rest)?;
        let url = format!("{}{path}", self.base);
        let bytes = body.unwrap_or_default();
        let ts = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let authz = auth::header(&self.key, method, &path, ts, &bytes);
        let res = match method {
            "GET" => self.agent.get(&url).header("authorization", &authz).call(),
            "DELETE" => self
                .agent
                .delete(&url)
                .header("authorization", &authz)
                .call(),
            "POST" => self
                .agent
                .post(&url)
                .header("authorization", &authz)
                .header("content-type", "application/json")
                .send(&bytes[..]),
            "PUT" => self
                .agent
                .put(&url)
                .header("authorization", &authz)
                .header("x-onecloud-invite", self.invite.as_deref().unwrap_or(""))
                .header("content-type", "application/json")
                .send(&bytes[..]),
            _ => bail!("unsupported method {method}"),
        }
        .with_context(|| format!("{method} {url}"))?;
        let status = res.status().as_u16();
        let body = res.into_body().with_config().limit(1 << 30).read_to_vec()?;
        match status {
            200 => Ok(Reply::Ok(body)),
            409 => Ok(Reply::Conflict),
            404 => Ok(Reply::NotFound),
            _ => {
                let err: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                if err["error"] == "revoked" {
                    bail!(Membership::Revoked);
                }
                if err["error"] == "invite" {
                    bail!("creating an account needs an invite code (--invite)");
                }
                if err["error"] == "quota" {
                    bail!("{}", err["message"].as_str().unwrap_or("storage full"));
                }
                Err(anyhow!(
                    "coordinator refused {method} {rest}: {} ({status})",
                    err["error"].as_str().unwrap_or("unknown error")
                ))
            }
        }
    }

    fn get<T: DeserializeOwned>(&self, rest: &str) -> Result<T> {
        match self.call("GET", rest, None)? {
            Reply::Ok(b) => Ok(serde_json::from_slice(&b)?),
            _ => bail!("coordinator has no {rest}"),
        }
    }

    /// `Ok(false)` when the position was taken.
    fn send<T: Serialize>(&self, method: &str, rest: &str, value: &T) -> Result<bool> {
        match self.call(method, rest, Some(serde_json::to_vec(value)?))? {
            Reply::Ok(_) => Ok(true),
            Reply::Conflict => Ok(false),
            Reply::NotFound => bail!("coordinator has no {rest}"),
        }
    }
}

/// Storage for the managed tiers (see [`crate::storage`]).
impl HttpCoordinator {
    /// A presigned URL for one repository object. A PUT gives the upload's
    /// size: the URL accepts exactly that many bytes, and the service counts
    /// them against the account's quota.
    ///
    /// # Errors
    ///
    /// If the service refuses (not a member, a bad path, storage full).
    pub fn presign(&self, method: &str, path: &str, size: Option<u64>) -> Result<String> {
        let ops = serde_json::json!({ "ops": [{ "method": method, "path": path, "size": size }] });
        match self.call("POST", "storage/sign", Some(serde_json::to_vec(&ops)?))? {
            Reply::Ok(b) => {
                let v: serde_json::Value = serde_json::from_slice(&b)?;
                v["urls"][0]
                    .as_str()
                    .map(str::to_string)
                    .context("no URL from the service")
            }
            _ => bail!("the service can't sign storage requests"),
        }
    }

    /// Bytes stored for the account, and its quota (0 for none).
    ///
    /// # Errors
    ///
    /// If the service can't be reached or refuses.
    pub fn usage(&self) -> Result<(u64, u64)> {
        let v: serde_json::Value = self.get("storage/usage")?;
        Ok((
            v["used"].as_u64().unwrap_or(0),
            v["quota"].as_u64().unwrap_or(0),
        ))
    }

    /// Delete the account on the service: every stored object and every
    /// record. Cannot be undone.
    ///
    /// # Errors
    ///
    /// If the service refuses or can't be reached.
    pub fn delete_account(&self) -> Result<()> {
        match self.call("DELETE", "account", None)? {
            Reply::Ok(_) => Ok(()),
            _ => bail!("the service did not delete the account"),
        }
    }

    /// Objects under `prefix`, as repository paths with sizes.
    pub(crate) fn list_storage(&self, prefix: &str) -> Result<Vec<(String, u64)>> {
        #[derive(serde::Deserialize)]
        struct Entry {
            path: String,
            size: u64,
        }
        let entries: Vec<Entry> = self.get(&format!("storage/list?prefix={prefix}"))?;
        Ok(entries.into_iter().map(|e| (e.path, e.size)).collect())
    }

    pub(crate) fn remove_storage(&self, paths: &[String]) -> Result<()> {
        let body = serde_json::json!({ "paths": paths });
        self.send("POST", "storage/remove", &body).map(|_| ())
    }

    /// The HTTP agent, for presigned requests (which carry no signature of
    /// ours).
    pub(crate) fn agent(&self) -> &Agent {
        &self.agent
    }
}

impl Coordinator for HttpCoordinator {
    fn head(&self) -> Result<Option<Head>> {
        self.get("heads/latest")
    }
    fn since(&self, after: u64) -> Result<Vec<Head>> {
        self.get(&format!("heads?after={after}"))
    }
    fn append(&self, head: &Head) -> Result<bool> {
        self.send("POST", "heads", head)
    }
    fn append_rotation(&self, head: &Head, record: &EpochRecord) -> Result<bool> {
        self.send(
            "POST",
            "rotation",
            &serde_json::json!({ "head": head, "record": record }),
        )
    }
    fn wait(&self, after: u64) -> Result<u64> {
        let v: serde_json::Value = self.get(&format!("heads/wait?after={after}"))?;
        Ok(v["seq"].as_u64().unwrap_or(after))
    }
    fn device_head(&self) -> Result<Option<DeviceEntry>> {
        Ok(self.devices_since(0)?.pop())
    }
    fn devices_since(&self, after: u64) -> Result<Vec<DeviceEntry>> {
        self.get(&format!("devices?after={after}"))
    }
    fn append_device(&self, entry: &DeviceEntry) -> Result<bool> {
        self.send("POST", "devices", entry)
    }
    fn anchor(&self) -> Result<Option<Anchor>> {
        if self.account.read().unwrap().is_none() {
            return Ok(None);
        }
        match self.call("GET", "anchor", None)? {
            Reply::Ok(b) => Ok(Some(serde_json::from_slice(&b)?)),
            _ => Ok(None),
        }
    }
    fn set_anchor(&self, anchor: &Anchor) -> Result<bool> {
        // creating an account picks which account this client talks about
        *self.account.write().unwrap() = Some(anchor.root.clone());
        self.send("PUT", "anchor", anchor)
    }
    fn epochs(&self) -> Result<Vec<EpochRecord>> {
        self.get("epochs")
    }
    fn append_epoch(&self, record: &EpochRecord) -> Result<bool> {
        self.send("POST", "epochs", record)
    }
    fn grants(&self, device: &str) -> Result<Vec<Grant>> {
        self.get(&format!("grants/{device}"))
    }
    fn put_grant(&self, grant: &Grant) -> Result<()> {
        self.send("PUT", "grants", grant).map(|_| ())
    }
    fn requests(&self) -> Result<Vec<JoinRequest>> {
        self.get("requests")
    }
    fn request(&self, req: &JoinRequest) -> Result<()> {
        self.send("POST", "requests", req).map(|_| ())
    }
    fn remove_request(&self, device: &str) -> Result<()> {
        self.call("DELETE", &format!("requests/{device}"), None)
            .map(|_| ())
    }
}
