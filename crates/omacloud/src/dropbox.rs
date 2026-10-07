//! One-time OAuth code exchange for a user's own Dropbox app.

use std::time::Duration;

pub const OLD_TOKEN_FILE: &str = "dropbox-old-token";

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

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

pub fn authorize_url(client_id: &str) -> Result<String> {
    ensure!(
        !client_id.is_empty() && client_id.bytes().all(|b| b.is_ascii_alphanumeric()),
        "give the Dropbox app key as --opt client_id=..."
    );
    Ok(format!(
        "https://www.dropbox.com/oauth2/authorize?client_id={client_id}&response_type=code&token_access_type=offline"
    ))
}

pub fn exchange(client_id: &str, client_secret: &str, code: &str) -> Result<String> {
    let mut response = agent()
        .post("https://api.dropboxapi.com/oauth2/token")
        .send_form([
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", client_id),
            ("client_secret", client_secret),
        ])
        .context("exchanging the Dropbox authorization code")?;
    ensure!(
        response.status().is_success(),
        "Dropbox refused the authorization code ({})",
        response.status()
    );
    let token: Token = response
        .body_mut()
        .read_json()
        .context("Dropbox token response")?;
    token
        .refresh_token
        .context("Dropbox returned no offline refresh token")
}

/// Revoke an old authorization after every remaining computer has switched.
pub fn revoke(client_id: &str, client_secret: &str, refresh_token: &str) -> Result<()> {
    let agent = agent();
    let mut token = agent
        .post("https://api.dropboxapi.com/oauth2/token")
        .send_form([
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
            ("client_secret", client_secret),
        ])
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
