//! Authorizing Dropbox: Omacloud's own Dropbox app, a public client with
//! PKCE, so no app secret ships.
//!
//! On a desktop, Dropbox sends the browser back to a fixed port here with
//! the code, so nobody copies anything; elsewhere (over ssh, say) Dropbox
//! shows the code and it's pasted. Either way the code is worthless without
//! the verifier, which never leaves this process.

use std::net::TcpListener;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

use crate::fail;
use crate::loopback::{Came, challenge, on_desktop, open_browser, random_url_safe, wait_for_code};

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

fn agent() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?)
}

fn authorize_url(extra: &str) -> String {
    format!(
        "https://www.dropbox.com/oauth2/authorize?client_id={APP_KEY}&response_type=code&token_access_type=offline{extra}"
    )
}

/// Authorize Omacloud's app and get an offline refresh token.
pub fn authorize() -> Result<String> {
    // one already allowed, for setting up where nobody is at a browser
    if let Ok(token) = std::env::var("OMACLOUD_DROPBOX_REFRESH_TOKEN") {
        return Ok(token);
    }
    let verifier = random_url_safe(48);
    let challenge = challenge(&verifier);
    let pkce = format!("&code_challenge={challenge}&code_challenge_method=S256");
    let listener = on_desktop()
        .then(|| TcpListener::bind(("127.0.0.1", REDIRECT_PORT)).ok())
        .flatten();
    let Some(listener) = listener else {
        eprintln!(
            "Open this Dropbox authorization URL, allow Omacloud, and paste the code it shows:\n\n  {}\n",
            authorize_url(&pkce)
        );
        let code = crate::read_secret("OMACLOUD_DROPBOX_AUTH_CODE", "Dropbox authorization code")?;
        return exchange(&[("code", &code), ("code_verifier", &verifier)]);
    };
    let redirect = format!("http://127.0.0.1:{REDIRECT_PORT}{REDIRECT_PATH}");
    let state = random_url_safe(16);
    let url = authorize_url(&format!("{pkce}&redirect_uri={redirect}&state={state}"));
    eprintln!("Allow Omacloud in Dropbox in your browser. If it didn't open, go to:\n\n  {url}\n");
    open_browser(&url);
    let not_allowed = |what: &str, hint: &str| fail::setup("dropbox_not_allowed", what).hint(hint);
    let code = match wait_for_code(
        &listener,
        REDIRECT_PATH,
        &state,
        ("Dropbox is connected", "Close this tab and go back to Omacloud."),
        ("Dropbox isn't connected", "Omacloud wasn't allowed."),
    )? {
        Came::Code(code) => code,
        Came::Refused(_) => {
            return Err(not_allowed("Omacloud wasn't allowed in Dropbox", "try again, and click Allow").into());
        }
        Came::TimedOut => {
            return Err(not_allowed(
                "Dropbox wasn't allowed after ten minutes",
                "try again, and click Allow in the browser",
            )
            .into());
        }
    };
    exchange(&[
        ("code", &code),
        ("code_verifier", &verifier),
        ("redirect_uri", &redirect),
    ])
}

/// A request to Dropbox's token service. One turned down as too fast wasn't
/// used, so it can go again, and asking someone to allow Omacloud again is the
/// worse way to wait.
fn token_request(form: &[(&str, &str)]) -> Result<reqwest::blocking::Response> {
    let agent = agent()?;
    let mut pause = Duration::from_secs(1);
    loop {
        let response = agent
            .post("https://api.dropboxapi.com/oauth2/token")
            .form(form)
            .send()
            .context("asking Dropbox's token service")?;
        let status = response.status().as_u16();
        if status != 429 && status < 500 {
            return Ok(response);
        }
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
    }
}

/// Trade an authorization code for an offline refresh token.
fn exchange(fields: &[(&str, &str)]) -> Result<String> {
    let mut form = vec![("grant_type", "authorization_code"), ("client_id", APP_KEY)];
    form.extend_from_slice(fields);
    let response = token_request(&form)?;
    let status = response.status().as_u16();
    if !response.status().is_success() {
        // what Dropbox says about a refused code: it never repeats the code
        let why = response.text().unwrap_or_default();
        return Err(fail::setup(
            "dropbox_code_rejected",
            format!("Dropbox refused the authorization code ({status}): {why}"),
        )
        .hint("authorize again for a new code")
        .into());
    }
    let token: Token = serde_json::from_str(&response.text()?).context("Dropbox token response")?;
    token.refresh_token.context("Dropbox returned no offline refresh token")
}

/// An address Dropbox serves one file at to anyone, for a few hours or until
/// the file is deleted: how a web link's browser reads what was left for
/// it before it has the token. `path` is from the top of the app's folder.
pub fn temporary_link(refresh_token: &str, path: &str) -> Result<String> {
    let token = token_request(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", APP_KEY),
    ])?;
    ensure!(
        token.status().is_success(),
        "Dropbox refused this computer's sign-in ({})",
        token.status().as_u16()
    );
    let access = serde_json::from_str::<Token>(&token.text()?)
        .context("Dropbox token response")?
        .access_token;
    let answer = agent()?
        .post("https://api.dropboxapi.com/2/files/get_temporary_link")
        .bearer_auth(access)
        .header("content-type", "application/json")
        .body(serde_json::json!({ "path": path }).to_string())
        .send()
        .context("asking Dropbox for a link")?;
    ensure!(
        answer.status().is_success(),
        "Dropbox gave no link ({})",
        answer.status().as_u16()
    );
    let said: serde_json::Value = serde_json::from_str(&answer.text()?).context("Dropbox's link answer")?;
    said["link"]
        .as_str()
        .map(str::to_string)
        .context("Dropbox's answer held no link")
}

/// Revoke an old authorization after every remaining computer has switched.
pub fn revoke(refresh_token: &str) -> Result<()> {
    // Dropbox refuses an empty secret: a public app sends none
    let token = token_request(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", APP_KEY),
    ])?;
    // refused: it's revoked already
    if matches!(token.status().as_u16(), 400 | 401) {
        return Ok(());
    }
    ensure!(
        token.status().is_success(),
        "the old Dropbox token could not be refreshed ({})",
        token.status()
    );
    let access: Token = serde_json::from_str(&token.text()?)?;
    let response = agent()?
        .post("https://api.dropboxapi.com/2/auth/token/revoke")
        .bearer_auth(&access.access_token)
        .header("content-type", "application/json")
        .body("null")
        .send()
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
        assert_eq!(challenge(verifier), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        // 48 bytes: 64 characters, within PKCE's 43 to 128
        assert_eq!(random_url_safe(48).len(), 64);
    }
}
