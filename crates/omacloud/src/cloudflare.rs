//! Putting the connector for agents (`connector/`) into a person's own
//! Cloudflare account, through Cloudflare's API and nothing else: no wrangler,
//! no node. The token is the person's, with the one permission Workers
//! Scripts: Edit; it's used for these calls and kept nowhere.

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::fail::{self, CliError};

/// Cloudflare's API, or a stand-in for it (`OMACLOUD_CLOUDFLARE_API`, for tests).
pub struct Api {
    client: reqwest::blocking::Client,
    base: String,
    token: String,
}

/// What Cloudflare said no to, in its words.
fn refused(what: &str, said: &Value, status: reqwest::StatusCode) -> CliError {
    let words: Vec<String> = said["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["message"].as_str().map(str::to_string))
        .collect();
    let words = if words.is_empty() {
        format!("it answered {status}")
    } else {
        words.join("; ")
    };
    let hint = if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        "the token needs the permission Workers Scripts: Edit, for this account (Cloudflare dashboard > My Profile > API Tokens)"
    } else {
        "check the account and the token, and try again"
    };
    fail::setup("cloudflare_refused", format!("Cloudflare refused {what}: {words}")).hint(hint)
}

impl Api {
    /// # Errors
    ///
    /// If no client can be made.
    pub fn new(token: String) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            // workers.dev and the API turn away clients that don't say what they are
            .user_agent(concat!("omacloud/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .context("making a client for Cloudflare")?;
        let base = std::env::var("OMACLOUD_CLOUDFLARE_API")
            .unwrap_or_else(|_| "https://api.cloudflare.com/client/v4".to_string());
        Ok(Self {
            client,
            base: base.trim_end_matches('/').to_string(),
            token: token.trim().to_string(),
        })
    }

    fn send(&self, what: &str, request: reqwest::blocking::RequestBuilder) -> Result<Value> {
        let response = request
            .bearer_auth(&self.token)
            .send()
            .map_err(|e| fail::unavailable(format!("Cloudflare didn't answer: {e}")))?;
        let status = response.status();
        let said: Value = response
            .text()
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or(Value::Null);
        if !status.is_success() || said["success"] == json!(false) {
            return Err(refused(what, &said, status).into());
        }
        Ok(said["result"].clone())
    }

    /// The account to deploy into: the one given, or the only one the token
    /// sees.
    ///
    /// # Errors
    ///
    /// If the token sees none or several, or Cloudflare refuses.
    pub fn account(&self, given: Option<&str>) -> Result<String> {
        if let Some(id) = given {
            return Ok(id.to_string());
        }
        let found = self.send(
            "listing your accounts",
            self.client.get(format!("{}/accounts", self.base)),
        )?;
        let ids: Vec<&str> = found
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| a["id"].as_str())
            .collect();
        match ids.as_slice() {
            [one] => Ok((*one).to_string()),
            [] => Err(fail::usage("this token doesn't say which Cloudflare account it's for")
                .hint("give it: `--account <id>` (the id is in the dashboard's address, and on the Workers overview)")
                .into()),
            _ => Err(fail::usage("this token reaches several Cloudflare accounts")
                .hint("say which: `--account <id>`")
                .into()),
        }
    }

    /// Upload `modules` (name and bytes; the first is the one that runs) as
    /// the Worker `name`, with `settings` as its secret `OMACLOUD`, and turn
    /// its workers.dev address on: that address.
    ///
    /// # Errors
    ///
    /// If Cloudflare refuses a step or can't be reached.
    pub fn deploy(&self, account: &str, name: &str, modules: &[(String, Vec<u8>)], settings: &str) -> Result<String> {
        let main = &modules.first().context("the connector has no files")?.0;
        let metadata = json!({
            "main_module": main,
            "compatibility_date": "2026-10-05",
            "bindings": [{ "type": "secret_text", "name": "OMACLOUD", "text": settings }],
        });
        // multipart/form-data by hand: each module is a part named for itself
        let boundary = format!("omacloud-{}", hex::encode(rand::random::<[u8; 16]>()));
        let mut body = Vec::new();
        let mut part = |name: &str, kind: &str, content: &[u8]| {
            body.extend_from_slice(
                format!(
                    "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{name}\"\r\nContent-Type: \
                     {kind}\r\n\r\n"
                )
                .as_bytes(),
            );
            body.extend_from_slice(content);
            body.extend_from_slice(b"\r\n");
        };
        part("metadata", "application/json", metadata.to_string().as_bytes());
        for (module, content) in modules {
            let kind = if module.ends_with(".wasm") {
                "application/wasm"
            } else {
                "application/javascript+module"
            };
            part(module, kind, content);
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        let script = format!("{}/accounts/{account}/workers/scripts/{name}", self.base);
        _ = self.send(
            "the connector",
            self.client
                .put(&script)
                .header("Content-Type", format!("multipart/form-data; boundary={boundary}"))
                .body(body),
        )?;
        _ = self.send(
            "giving the connector its address",
            self.client
                .post(format!("{script}/subdomain"))
                .header("Content-Type", "application/json")
                .body(json!({ "enabled": true }).to_string()),
        )?;
        let subdomain = self.send(
            "saying your workers.dev name",
            self.client
                .get(format!("{}/accounts/{account}/workers/subdomain", self.base)),
        )?;
        let subdomain = subdomain["subdomain"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                fail::setup(
                    "cloudflare_refused",
                    "this Cloudflare account has no workers.dev name yet",
                )
                .hint("open Workers & Pages in the Cloudflare dashboard once, which gives it one, and run this again")
            })?;
        Ok(format!("https://{name}.{subdomain}.workers.dev"))
    }

    /// Delete the Worker `name`; one that's already gone is no error.
    ///
    /// # Errors
    ///
    /// If Cloudflare refuses or can't be reached.
    pub fn remove(&self, account: &str, name: &str) -> Result<()> {
        let request = self
            .client
            .delete(format!("{}/accounts/{account}/workers/scripts/{name}", self.base));
        match self.send("removing the connector", request) {
            Err(e) if e.to_string().contains("not found") || e.to_string().contains("does not exist") => Ok(()),
            other => other.map(|_| ()),
        }
    }
}
