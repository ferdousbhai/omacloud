//! Authorizing Dropbox: Omacloud's own Dropbox app, a public client with
//! PKCE, so no app secret ships.
//!
//! On a desktop, Dropbox sends the browser back to a fixed port here with
//! the code, so nobody copies anything; elsewhere (over ssh, say) Dropbox
//! shows the code and it's pasted. Either way the code is worthless without
//! the verifier, which never leaves this process.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::fail;

pub const OLD_TOKEN_FILE: &str = "dropbox-old-token";

/// Omacloud's Dropbox app (App folder access, files read and write).
pub const APP_KEY: &str = "337rl43cn3fcoj8";

/// Where Dropbox sends the browser back: registered with the app, so the
/// port is fixed.
const REDIRECT_PORT: u16 = 53691;
const REDIRECT_PATH: &str = "/dropbox";

#[derive(Deserialize)]
struct Token {
    access_token: String,
    refresh_token: Option<String>,
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(60)))
        .build()
        .into()
}

fn random_url_safe(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    rand::rngs::OsRng.fill_bytes(&mut b);
    URL_SAFE_NO_PAD.encode(b)
}

/// What a browser is sent to: no secret of this process goes with it.
fn open_browser(url: &str) {
    let mut open = std::process::Command::new("xdg-open");
    for var in [
        "OMACLOUD_RECOVERY_CODE",
        "OMACLOUD_JOIN_CODE",
        "OMACLOUD_SECRET_ACCESS_KEY",
        "OMACLOUD_DROPBOX_AUTH_CODE",
        "OMACLOUD_DROPBOX_REFRESH_TOKEN",
    ] {
        open.env_remove(var);
    }
    _ = open
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn param<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query
        .split('&')
        .filter_map(|p| p.split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

fn reply(mut stream: &TcpStream, status: &str, title: &str, text: &str) {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>Omacloud</title>\
         <body style=\"font:16px system-ui;max-width:32em;margin:4em auto;padding:0 1em\">\
         <h1>{title}</h1><p>{text}</p>"
    );
    _ = write!(
        stream,
        "HTTP/1.1 {status}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\n\
         connection: close\r\n\r\n{body}",
        body.len()
    );
}

fn authorize_url(client_id: &str, extra: &str) -> String {
    format!(
        "https://www.dropbox.com/oauth2/authorize?client_id={client_id}&response_type=code&token_access_type=offline{extra}"
    )
}

/// A desktop session, where a browser here can reach this computer's
/// loopback port.
fn on_desktop() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some()
}

/// Authorize Omacloud's app and get an offline refresh token.
pub fn authorize(ask_code: impl FnOnce() -> Result<String>) -> Result<String> {
    // one already allowed, for setting up where nobody is at a browser
    if let Ok(token) = std::env::var("OMACLOUD_DROPBOX_REFRESH_TOKEN") {
        return Ok(token);
    }
    let client_id = APP_KEY;
    let verifier = random_url_safe(48);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let pkce = format!("&code_challenge={challenge}&code_challenge_method=S256");
    let listener = on_desktop()
        .then(|| TcpListener::bind(("127.0.0.1", REDIRECT_PORT)).ok())
        .flatten();
    let Some(listener) = listener else {
        eprintln!(
            "Open this Dropbox authorization URL, allow Omacloud, and paste the code it shows:\n\n  {}\n",
            authorize_url(client_id, &pkce)
        );
        let code = ask_code()?;
        return exchange(client_id, &[("code", &code), ("code_verifier", &verifier)]);
    };
    let redirect = format!("http://127.0.0.1:{REDIRECT_PORT}{REDIRECT_PATH}");
    let state = random_url_safe(16);
    let url = authorize_url(client_id, &format!("{pkce}&redirect_uri={redirect}&state={state}"));
    eprintln!("Allow Omacloud in Dropbox in your browser. If it didn't open, go to:\n\n  {url}\n");
    open_browser(&url);
    let code = wait_for_code(&listener, &state)?;
    exchange(
        client_id,
        &[
            ("code", &code),
            ("code_verifier", &verifier),
            ("redirect_uri", &redirect),
        ],
    )
}

/// The code Dropbox sends the browser back with.
fn wait_for_code(listener: &TcpListener, state: &str) -> Result<String> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        let stream = match listener.accept() {
            Ok((s, _)) => s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() > deadline {
                    return Err(
                        fail::setup("dropbox_not_allowed", "Dropbox wasn't allowed after ten minutes")
                            .hint("try again, and click Allow in the browser")
                            .into(),
                    );
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
        // GET /dropbox?code=...&state=... HTTP/1.1
        let target = line.split_whitespace().nth(1).unwrap_or_default();
        let Some(query) = target.strip_prefix(REDIRECT_PATH).and_then(|q| q.strip_prefix('?')) else {
            reply(&stream, "404 Not Found", "Not here", "");
            continue;
        };
        if param(query, "state") != Some(state) {
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
                "Dropbox is connected",
                "Close this tab and go back to Omacloud.",
            );
            return Ok(code.to_string());
        }
        reply(&stream, "200 OK", "Dropbox isn't connected", "Omacloud wasn't allowed.");
        return Err(fail::setup("dropbox_not_allowed", "Omacloud wasn't allowed in Dropbox")
            .hint("try again, and click Allow")
            .into());
    }
}

/// Trade an authorization code for an offline refresh token.
fn exchange(client_id: &str, fields: &[(&str, &str)]) -> Result<String> {
    let mut form = vec![("grant_type", "authorization_code"), ("client_id", client_id)];
    form.extend_from_slice(fields);
    // a code turned down as too fast wasn't used: it can go again, and asking
    // the person to allow Omacloud again is the worse way to wait
    let agent = agent();
    let mut pause = Duration::from_secs(1);
    let mut response = loop {
        let response = agent
            .post("https://api.dropboxapi.com/oauth2/token")
            .send_form(form.iter().copied())
            .context("exchanging the Dropbox authorization code")?;
        let status = response.status().as_u16();
        if status != 429 && status < 500 {
            break response;
        }
        log::info!("Dropbox answered {status} to the authorization code; trying again");
        if pause > Duration::from_secs(30) {
            return Err(fail::unavailable(format!("Dropbox answered {status}"))
                .hint("try again in a minute")
                .into());
        }
        let asked = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
            .map(Duration::from_secs)
            .unwrap_or_default();
        std::thread::sleep(asked.max(pause));
        pause *= 2;
    };
    let status = response.status().as_u16();
    if !response.status().is_success() {
        // what Dropbox says about a refused code: it never repeats the code
        let why = response.body_mut().read_to_string().unwrap_or_default();
        return Err(fail::setup(
            "dropbox_code_rejected",
            format!("Dropbox refused the authorization code ({status}): {why}"),
        )
        .hint("authorize again for a new code")
        .into());
    }
    let token: Token = response.body_mut().read_json().context("Dropbox token response")?;
    token.refresh_token.context("Dropbox returned no offline refresh token")
}

/// Revoke an old authorization after every remaining computer has switched.
pub fn revoke(refresh_token: &str) -> Result<()> {
    let agent = agent();
    // Dropbox refuses an empty secret: a public app sends none
    let form = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", APP_KEY),
    ];
    let mut token = agent
        .post("https://api.dropboxapi.com/oauth2/token")
        .send_form(form)
        .context("refreshing the old Dropbox token for revocation")?;
    ensure!(
        token.status().is_success(),
        "the old Dropbox token could not be refreshed ({})",
        token.status()
    );
    let access: Token = token.body_mut().read_json()?;
    let response = agent
        .post("https://api.dropboxapi.com/2/auth/token/revoke")
        .header("authorization", &format!("Bearer {}", access.access_token))
        .header("content-type", "application/json")
        .send("null")
        .context("revoking the old Dropbox token")?;
    ensure!(
        response.status().is_success(),
        "Dropbox did not revoke the old token ({})",
        response.status()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_challenge_is_s256_of_the_verifier() {
        // RFC 7636, appendix B
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        // 48 bytes: 64 characters, within PKCE's 43 to 128
        assert_eq!(random_url_safe(48).len(), 64);
    }
}
