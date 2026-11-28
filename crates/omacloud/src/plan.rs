//! The paid tier (`server/src/paid.ts`), from a computer's side. A
//! subscription is a key this computer made and keeps; omacloud.computer
//! knows only its hash, and that Stripe says it's paid for. No sign-in, no
//! email: the key in a request is the whole of it. `OMACLOUD_SITE` names
//! another copy of the site, for trying one.

use std::{fs, io::Write as _, os::unix::fs::OpenOptionsExt as _, path::Path, time::Duration};

use anyhow::{Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::fail;

pub struct Plan {
    key: String,
    site: String,
    client: reqwest::blocking::Client,
}

fn site() -> String {
    std::env::var("OMACLOUD_SITE")
        .unwrap_or_else(|_| "https://omacloud.computer".into())
        .trim_end_matches('/')
        .to_string()
}

impl Plan {
    fn with(key: String) -> Option<Self> {
        let client = reqwest::blocking::Client::builder()
            .user_agent(concat!("omacloud/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(90))
            .build()
            .ok()?;
        Some(Self {
            key,
            site: site(),
            client,
        })
    }

    /// The subscription this computer has a key for, if any.
    pub fn read(path: &Path) -> Option<Self> {
        let key = fs::read_to_string(path).ok()?.trim().to_string();
        (key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| Self::with(key))
            .flatten()
    }

    /// The same, made now if there's none.
    ///
    /// # Errors
    ///
    /// If the key can't be written.
    pub fn make(path: &Path) -> Result<Self> {
        if let Some(plan) = Self::read(path) {
            return Ok(plan);
        }
        let key = hex::encode(rand::random::<[u8; 32]>());
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .and_then(|mut f| f.write_all(key.as_bytes()))
            .with_context(|| format!("writing {}", path.display()))?;
        Self::with(key).context("making a client for omacloud.computer")
    }

    /// The key itself: what a browser needs to keep a passkey with the site.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Where paying for it starts. It names the key's hash, never the key.
    pub fn subscribe_url(&self) -> String {
        format!(
            "{}/subscribe/{}",
            self.site,
            hex::encode(Sha256::digest(self.key.as_bytes()))
        )
    }

    fn call(&self, method: reqwest::Method, path: &str, body: Option<String>) -> Result<Value> {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.site))
            .bearer_auth(&self.key);
        if let Some(body) = body {
            request = request.header("Content-Type", "application/json").body(body);
        }
        let response = request
            .send()
            .map_err(|e| fail::unavailable(format!("omacloud.computer didn't answer: {e}")))?;
        let status = response.status();
        let said: Value = response
            .text()
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or(Value::Null);
        if status.is_success() {
            return Ok(said);
        }
        let message = said["error"]["message"]
            .as_str()
            .map_or_else(|| format!("omacloud.computer answered {status}"), str::to_string);
        // in the site's words: it knows whether one ended or never was
        Err(if status == reqwest::StatusCode::PAYMENT_REQUIRED {
            fail::setup("not_subscribed", message)
                .hint("`omacloud subscribe` starts one")
                .into()
        } else if status.is_client_error() {
            fail::usage(message).into()
        } else {
            fail::unavailable(message).into()
        })
    }

    /// Where the subscription stands: `active`, `status`, `until`, and the
    /// connectors and passkeys it keeps.
    ///
    /// # Errors
    ///
    /// If the site can't be reached.
    pub fn status(&self) -> Result<Value> {
        self.call(reqwest::Method::GET, "/api/subscription", None)
    }

    /// Whether it's paid for now; `false` too when the site can't say.
    pub fn active(&self) -> bool {
        self.status().is_ok_and(|s| s["active"] == Value::Bool(true))
    }

    /// Stripe's page for changing the card or ending the subscription.
    ///
    /// # Errors
    ///
    /// If there's no subscription, or the site can't be reached.
    pub fn portal(&self) -> Result<String> {
        let said = self.call(reqwest::Method::POST, "/api/portal", None)?;
        said["url"]
            .as_str()
            .map(str::to_string)
            .context("the site gave no address")
    }

    /// Have the site run the connector `name` with `settings`: its address.
    ///
    /// # Errors
    ///
    /// If the subscription isn't paid for, or the site refuses or can't be reached.
    pub fn connect(&self, name: &str, settings: &str) -> Result<String> {
        let said = self.call(
            reqwest::Method::PUT,
            &format!("/api/connectors/{name}"),
            Some(settings.to_string()),
        )?;
        said["url"]
            .as_str()
            .map(str::to_string)
            .context("the site gave no address")
    }

    /// Have the site take the connector `name` down.
    ///
    /// # Errors
    ///
    /// If the site can't be reached.
    pub fn disconnect(&self, name: &str) -> Result<()> {
        self.call(reqwest::Method::DELETE, &format!("/api/connectors/{name}"), None)
            .map(|_| ())
    }

    /// Have the site drop every passkey's sealed note: after the account
    /// moved to new keys, they open nothing.
    ///
    /// # Errors
    ///
    /// If the site can't be reached.
    pub fn forget_passkeys(&self) -> Result<()> {
        self.call(reqwest::Method::DELETE, "/api/passkeys", None).map(|_| ())
    }
}
