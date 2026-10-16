//! Authorizing Dropbox: Omacloud's own Dropbox app, a public client with
//! PKCE, so no app secret ships; or a user's own app with its secret.
//!
//! On a desktop, Dropbox sends the browser back to a fixed port here with
//! the code, so nobody copies anything; elsewhere (over ssh, say) Dropbox
//! shows the code and it's pasted. Either way the code is worthless without
//! the verifier, which never leaves this process.

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::fail;

pub const OLD_TOKEN_FILE: &str = "dropbox-old-token";

/// Omacloud's Dropbox app (App folder access, files read and write).
pub const APP_KEY: &str = "k8jkvsr48k2fvio";

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

fn checked_app_key(client_id: &str) -> Result<()> {
    if client_id.is_empty() || !client_id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(fail::usage("give the Dropbox app key as --opt client_id=...").into());
    }
    Ok(())
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

/// Authorize the app and get an offline refresh token. With `secret`, it's
/// the user's own app and the code is pasted (`ask_code`); without, PKCE.
pub fn authorize(client_id: &str, secret: Option<&str>, ask_code: impl FnOnce() -> Result<String>) -> Result<String> {
    checked_app_key(client_id)?;
    if let Some(secret) = secret {
        if std::env::var_os("OMACLOUD_DROPBOX_AUTH_CODE").is_none() {
            eprintln!(
                "Open this Dropbox authorization URL and paste its code:\n\n  {}\n",
                authorize_url(client_id, "")
            );
        }
        let code = ask_code()?;
        return exchange(client_id, &[("client_secret", secret), ("code", &code)]);
    }
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
    crate::hosted::open_browser(&url);
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
            crate::hosted::reply(&stream, "404 Not Found", "Not here", "");
            continue;
        };
        if crate::hosted::param(query, "state") != Some(state) {
            crate::hosted::reply(
                &stream,
                "400 Bad Request",
                "That didn't work",
                "Start again from Omacloud.",
            );
            continue;
        }
        if let Some(code) = crate::hosted::param(query, "code") {
            crate::hosted::reply(
                &stream,
                "200 OK",
                "Dropbox is connected",
                "Close this tab and go back to Omacloud.",
            );
            return Ok(code.to_string());
        }
        crate::hosted::reply(&stream, "200 OK", "Dropbox isn't connected", "Omacloud wasn't allowed.");
        return Err(fail::setup("dropbox_not_allowed", "Omacloud wasn't allowed in Dropbox")
            .hint("try again, and click Allow")
            .into());
    }
}

/// Trade an authorization code for an offline refresh token.
fn exchange(client_id: &str, fields: &[(&str, &str)]) -> Result<String> {
    let mut form = vec![("grant_type", "authorization_code"), ("client_id", client_id)];
    form.extend_from_slice(fields);
    let mut response = agent()
        .post("https://api.dropboxapi.com/oauth2/token")
        .send_form(form)
        .context("exchanging the Dropbox authorization code")?;
    let status = response.status().as_u16();
    if status == 429 || status >= 500 {
        return Err(fail::unavailable(format!("Dropbox answered {status}"))
            .hint("try again in a minute")
            .into());
    }
    if !response.status().is_success() {
        return Err(fail::setup(
            "dropbox_code_rejected",
            format!("Dropbox refused the authorization code ({status})"),
        )
        .hint("authorize again for a new code")
        .into());
    }
    let token: Token = response.body_mut().read_json().context("Dropbox token response")?;
    token.refresh_token.context("Dropbox returned no offline refresh token")
}

/// Revoke an old authorization after every remaining computer has switched.
pub fn revoke(client_id: &str, client_secret: Option<&str>, refresh_token: &str) -> Result<()> {
    let agent = agent();
    let mut form = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
    ];
    // Dropbox refuses an empty secret: a public app sends none
    if let Some(secret) = client_secret {
        form.push(("client_secret", secret));
    }
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

    #[test]
    fn app_keys_are_plain() {
        assert!(checked_app_key(APP_KEY).is_ok());
        assert!(checked_app_key("").is_err());
        assert!(checked_app_key("a&b").is_err());
    }
}
