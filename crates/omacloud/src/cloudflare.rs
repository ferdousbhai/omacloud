//! Putting the connector for agents (`connector/`) into a person's own
//! Cloudflare account, through Cloudflare's API and nothing else: no wrangler,
//! no node. What lets it is the person's leave, asked of Cloudflare in their
//! browser (OAuth, for editing Workers and nothing more), or an API token of
//! theirs with the one permission Workers Scripts: Edit where there's no
//! browser. Either is used for these calls and kept nowhere.

use std::{net::TcpListener, time::Duration};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::{
    fail::{self, CliError},
    loopback::{Came, challenge, on_desktop, open_browser, random_url_safe, wait_for_code},
};

/// Omacloud as Cloudflare knows it (Manage Account, OAuth clients, in
/// SummonGhost's account): a public client, so there's no secret to keep.
const CLIENT: &str = "6b4a9ea3ec241e507a1bbc6b4e06ebd5";
/// The one thing asked for: putting up and removing Workers. (Workers
/// Admin: Workers Editor can't upload one, and new clients can't ask for
/// the older Workers Scripts Write that an API token has.)
const SCOPE: &str = "workers-scripts.admin";

fn agent() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        // workers.dev and the API turn away clients that don't say what they are
        .user_agent(concat!("omacloud/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(60))
        .build()
        .context("making a client for Cloudflare")
}

/// Leave to edit Workers in an account the person picks, asked of Cloudflare
/// in their browser: a token for that, good for a short while.
/// `OMACLOUD_CLOUDFLARE_OAUTH` names a stand-in for Cloudflare's sign-in, and
/// `OMACLOUD_CLOUDFLARE_CLIENT` another client, both for tests.
fn allowed_in_browser(client: &str, stand_in: Option<&str>) -> Result<String> {
    let oauth = stand_in.unwrap_or("https://dash.cloudflare.com/oauth2");
    let listener = TcpListener::bind(("127.0.0.1", 0)).context("listening for Cloudflare's answer")?;
    let port = listener.local_addr()?.port();
    // Cloudflare returns people to an https address: the site's, which sends
    // them on to this port, the first thing the state says (server/src/index.ts)
    let redirect = format!("{}/cloudflare/callback", crate::plan::site());
    let verifier = random_url_safe(48);
    let state = format!("{port}.{}", random_url_safe(16));
    let url = reqwest::Url::parse_with_params(
        &format!("{oauth}/auth"),
        [
            ("response_type", "code"),
            ("client_id", client),
            ("redirect_uri", redirect.as_str()),
            ("scope", SCOPE),
            ("state", state.as_str()),
            ("code_challenge", challenge(&verifier).as_str()),
            ("code_challenge_method", "S256"),
        ],
    )?;
    eprintln!("Allow Omacloud in your Cloudflare account, in your browser. If it didn't open, go to:\n\n  {url}\n");
    if stand_in.is_none() {
        open_browser(url.as_str());
    }
    let refused = |why: &str| {
        fail::setup(
            "cloudflare_refused",
            format!("Omacloud wasn't allowed in Cloudflare: {why}"),
        )
        .hint("try again and press Authorize, or use an API token: put it in OMACLOUD_CLOUDFLARE_TOKEN")
    };
    let code = match wait_for_code(
        &listener,
        "/callback",
        &state,
        ("Omacloud is allowed", "Close this tab and go back to your terminal."),
        ("Omacloud wasn't allowed", "Nothing was put in your Cloudflare account."),
    )? {
        Came::Code(code) => code,
        Came::Refused(why) => return Err(refused(why.as_deref().unwrap_or("it wasn't allowed")).into()),
        Came::TimedOut => return Err(refused("nothing came back in ten minutes").into()),
    };
    let response = agent()?
        .post(format!("{oauth}/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", redirect.as_str()),
            ("client_id", client),
            ("code_verifier", verifier.as_str()),
        ])
        .send()
        .map_err(|e| fail::unavailable(format!("Cloudflare didn't answer: {e}")))?;
    let said: Value = response
        .text()
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Null);
    said["access_token"].as_str().map(str::to_string).ok_or_else(|| {
        let why = said["error_description"]
            .as_str()
            .or_else(|| said["error"].as_str())
            .unwrap_or("it gave no token");
        fail::setup(
            "cloudflare_refused",
            format!("Cloudflare didn't let Omacloud in: {why}"),
        )
        .hint("try again, or use an API token: put it in OMACLOUD_CLOUDFLARE_TOKEN")
        .into()
    })
}

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
        "pick this account and allow Workers Admin when Cloudflare asks; an API token needs the permission Workers Scripts: Edit for it"
    } else {
        "check the account and the token, and try again"
    };
    fail::setup("cloudflare_refused", format!("Cloudflare refused {what}: {words}")).hint(hint)
}

impl Api {
    /// Cloudflare's API with the person's leave: a token in
    /// `OMACLOUD_CLOUDFLARE_TOKEN` if there is one, else asked of Cloudflare
    /// in their browser, else (no browser here, or no client registered) a
    /// token asked for.
    ///
    /// # Errors
    ///
    /// If they don't allow it, or give no token.
    pub fn with_leave() -> Result<Self> {
        let client = std::env::var("OMACLOUD_CLOUDFLARE_CLIENT").unwrap_or_else(|_| CLIENT.to_string());
        let by_token = std::env::var_os("OMACLOUD_CLOUDFLARE_TOKEN").is_some();
        let stand_in = std::env::var("OMACLOUD_CLOUDFLARE_OAUTH").ok();
        if !by_token && !client.is_empty() && (on_desktop() || stand_in.is_some()) {
            return Self::new(allowed_in_browser(&client, stand_in.as_deref())?);
        }
        if !by_token {
            eprintln!(
                "This needs an API token for your Cloudflare account, with the permission Workers Scripts: Edit.\nMake \
                 one at https://dash.cloudflare.com/profile/api-tokens (Create Token, then Edit Cloudflare \
                 Workers).\nIt's used now and kept nowhere. Or have Omacloud run it: `omacloud subscribe`."
            );
        }
        Self::new(crate::read_secret("OMACLOUD_CLOUDFLARE_TOKEN", "Cloudflare API token")?)
    }

    /// # Errors
    ///
    /// If no client can be made.
    pub fn new(token: String) -> Result<Self> {
        let client = agent()?;
        let base = std::env::var("OMACLOUD_CLOUDFLARE_API")
            .unwrap_or_else(|_| "https://api.cloudflare.com/client/v4".to_string());
        Ok(Self {
            client,
            base: base.trim_end_matches('/').to_string(),
            token: token.trim().to_string(),
        })
    }

    fn send(&self, what: &str, request: reqwest::blocking::RequestBuilder) -> Result<Value> {
        self.sent(what, request).map(|(_, said)| said)
    }

    /// The same, with how Cloudflare answered; a 404 is no failure here,
    /// for the caller to take as it means.
    fn sent(&self, what: &str, request: reqwest::blocking::RequestBuilder) -> Result<(reqwest::StatusCode, Value)> {
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
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok((status, Value::Null));
        }
        if !status.is_success() || said["success"] == json!(false) {
            return Err(refused(what, &said, status).into());
        }
        Ok((status, said["result"].clone()))
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
        self.sent("removing the connector", request).map(|_| ())
    }
}
