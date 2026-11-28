//! Putting the connector for agents (`connector/`) into a person's own
//! Cloudflare account, through Cloudflare's API and nothing else: no wrangler,
//! no node. What lets it is the person's leave, asked of Cloudflare in their
//! browser (OAuth, for editing Workers and nothing more), or an API token of
//! theirs with the one permission Workers Scripts: Edit where there's no
//! browser. Either is used for these calls and kept nowhere.

use std::{
    io::{BufRead as _, BufReader},
    net::TcpListener,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::{
    dropbox::{challenge, on_desktop, open_browser, param, random_url_safe, reply},
    fail::{self, CliError},
};

/// Omacloud as Cloudflare knows it (Manage Account, OAuth clients, in
/// SummonGhost's account): a public client, so there's no secret to keep.
const CLIENT: &str = "6b4a9ea3ec241e507a1bbc6b4e06ebd5";
/// The one thing asked for: putting up and removing Workers. (Workers
/// Admin: Workers Editor can't upload one, and new clients can't ask for
/// the older Workers Scripts Write that an API token has.)
const SCOPE: &str = "workers-scripts.admin";

/// Leave to edit Workers in an account the person picks, asked of Cloudflare
/// in their browser: a token for that, good for a short while.
/// `OMACLOUD_CLOUDFLARE_OAUTH` names a stand-in for Cloudflare's sign-in, and
/// `OMACLOUD_CLOUDFLARE_CLIENT` another client, both for tests.
fn allowed_in_browser(client: &str) -> Result<String> {
    let stand_in = std::env::var("OMACLOUD_CLOUDFLARE_OAUTH").ok();
    let oauth = stand_in
        .clone()
        .unwrap_or_else(|| "https://dash.cloudflare.com/oauth2".into());
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
    eprintln!(
        "Allow Omacloud to put the connector in your Cloudflare account, in your browser. If it didn't open, go          to:\n\n  {url}\n"
    );
    if stand_in.is_none() {
        open_browser(url.as_str());
    }
    let code = wait_for_code(&listener, &state)?;
    let response = reqwest::blocking::Client::builder()
        .user_agent(concat!("omacloud/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(60))
        .build()?
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

/// `%41` and the like in a part of an address, as what they stand for.
fn unescaped(part: &str) -> String {
    let bytes = part.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = (bytes[i] == b'%')
            .then(|| part.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok()))
            .flatten();
        if let Some(byte) = hex {
            out.push(byte);
            i += 3;
        } else {
            out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The code Cloudflare sends the browser back with.
fn wait_for_code(listener: &TcpListener, state: &str) -> Result<String> {
    let refused = |why: &str| {
        fail::setup(
            "cloudflare_refused",
            format!("Omacloud wasn't allowed in Cloudflare: {why}"),
        )
        .hint("try again and press Allow, or use an API token: put it in OMACLOUD_CLOUDFLARE_TOKEN")
    };
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        let stream = match listener.accept() {
            Ok((s, _)) => s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() > deadline {
                    return Err(refused("nothing came back in ten minutes").into());
                }
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
            Err(e) => return Err(e).context("waiting for the browser"),
        };
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        let mut line = String::new();
        _ = BufReader::new(&stream).read_line(&mut line);
        // GET /callback?code=...&state=... HTTP/1.1
        let target = line.split_whitespace().nth(1).unwrap_or_default();
        let Some(query) = target.strip_prefix("/callback?") else {
            reply(&stream, "404 Not Found", "Not here", "");
            continue;
        };
        if param(query, "state").map(unescaped).as_deref() != Some(state) {
            reply(
                &stream,
                "400 Bad Request",
                "That didn't work",
                "Start again from Omacloud.",
            );
            continue;
        }
        if let Some(code) = param(query, "code") {
            reply(
                &stream,
                "200 OK",
                "Omacloud is allowed",
                "Close this tab and go back to your terminal.",
            );
            return Ok(unescaped(code));
        }
        reply(
            &stream,
            "200 OK",
            "Omacloud wasn't allowed",
            "Nothing was put in your Cloudflare account.",
        );
        let why = param(query, "error_description")
            .or_else(|| param(query, "error"))
            .map_or_else(|| "it wasn't allowed".to_string(), unescaped);
        return Err(refused(&why).into());
    }
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
        "the token needs the permission Workers Scripts: Edit, for this account (Cloudflare dashboard > My Profile > API Tokens)"
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
        if !by_token && !client.is_empty() && (on_desktop() || std::env::var_os("OMACLOUD_CLOUDFLARE_OAUTH").is_some())
        {
            return Self::new(allowed_in_browser(&client)?);
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
