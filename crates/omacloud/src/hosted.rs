//! Omacloud storage: signing in with Google gets this computer a key to
//! its own space at omacloud.computer, in place of a bucket of your own.
//!
//! The browser signs in; the server sends it back to a port this computer
//! listens on with a one-time code, which this computer trades, with a
//! secret only it has, for the key, and for the account's trusted contact
//! pad when it recovers with a contact's card. Keeping a new pad takes a
//! sign-in too: the pad only goes to and from the account's Google identity.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Where to sign in, unless `OMACLOUD_SERVICE_URL` says otherwise.
pub const SERVICE: &str = "https://omacloud.computer";

/// What the server gives a computer that signed in.
#[derive(Deserialize)]
pub struct Storage {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub email: String,
    /// The trusted contact pad, when asked for and the account has one
    #[serde(default)]
    pub contact_pad: Option<String>,
}

/// A finished browser sign-in: the one-time code and the secret that
/// redeems it.
struct Grant {
    code: String,
    verifier: String,
}

fn random_hex(bytes: usize) -> String {
    use rand::RngCore;
    let mut b = vec![0u8; bytes];
    rand::rngs::OsRng.fill_bytes(&mut b);
    hex::encode(b)
}

/// The decoded value of `name` in a query string of hex and short words.
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

/// Sign in with Google in the browser, and get this computer's key.
pub fn sign_in(service: &str) -> Result<Storage> {
    credentials(service, &browser(service)?, false)
}

/// [`sign_in`], and get the account's trusted contact pad too.
pub fn sign_in_with_pad(service: &str) -> Result<Storage> {
    credentials(service, &browser(service)?, true)
}

/// Sign in with Google and keep a new trusted contact pad for `account`
/// (its bucket), or none. Returns the email signed in with.
pub fn set_contact(service: &str, account: &str, pad: Option<&str>) -> Result<String> {
    let grant = browser(service)?;
    let mut response = post(
        service,
        "/api/contact",
        &serde_json::json!({
            "code": grant.code,
            "verifier": grant.verifier,
            "account": account,
            "pad": pad,
        }),
    )?;
    let answer: serde_json::Value = response.body_mut().read_json().unwrap_or_default();
    let email = answer["email"].as_str().unwrap_or_default().to_string();
    match response.status().as_u16() {
        200 => Ok(email),
        409 => bail!(
            "{email} isn't the Google account this computer's storage belongs to: \
             sign in with that one"
        ),
        status => bail!("that didn't work ({status}): try again"),
    }
}

fn post(
    service: &str,
    path: &str,
    body: &serde_json::Value,
) -> Result<ureq::http::Response<ureq::Body>> {
    let service = service.trim_end_matches('/');
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    agent
        .post(&format!("{service}{path}"))
        .send_json(body)
        .with_context(|| format!("reaching {service}"))
}

/// Trade a sign-in for this computer's key (and the contact pad).
fn credentials(service: &str, grant: &Grant, contact: bool) -> Result<Storage> {
    let mut response = post(
        service,
        "/api/credentials",
        &serde_json::json!({ "code": grant.code, "verifier": grant.verifier, "contact": contact }),
    )?;
    if !response.status().is_success() {
        bail!("sign-in didn't work ({}): try again", response.status());
    }
    response
        .body_mut()
        .read_json()
        .context("reading the key from Omacloud")
}

/// Sign in with Google in the browser.
fn browser(service: &str) -> Result<Grant> {
    let service = service.trim_end_matches('/');
    let listener = TcpListener::bind("127.0.0.1:0").context("listening for the browser")?;
    let port = listener.local_addr()?.port();
    let (state, verifier) = (random_hex(16), random_hex(32));
    let challenge = hex::encode(Sha256::digest(verifier.as_bytes()));
    let url = format!("{service}/signin?port={port}&state={state}&challenge={challenge}");
    eprintln!("Sign in with Google in your browser. If it didn't open, go to:\n\n  {url}\n");
    _ = std::process::Command::new("xdg-open")
        .arg(&url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();

    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + Duration::from_secs(600);
    let code = loop {
        let stream = match listener.accept() {
            Ok((s, _)) => s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() > deadline {
                    bail!("no sign-in after ten minutes: try again");
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
        // GET /callback?state=...&code=... HTTP/1.1
        let target = line.split_whitespace().nth(1).unwrap_or_default();
        let Some(query) = target.strip_prefix("/callback?") else {
            reply(&stream, "404 Not Found", "Not here", "");
            continue;
        };
        if param(query, "state") != Some(state.as_str()) {
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
                "You're signed in",
                "Close this tab and go back to Omacloud.",
            );
            break code.to_string();
        }
        let why = match param(query, "error") {
            Some("invite") => {
                "Omacloud storage is by invitation for now: this Google account isn't invited yet"
            }
            Some("closed") => "this Omacloud account is closed",
            Some("cancelled") => "sign-in was cancelled",
            _ => "sign-in didn't work: try again",
        };
        reply(
            &stream,
            "200 OK",
            "Not signed in",
            &format!("{}.", capitalize(why)),
        );
        bail!("{why}");
    };
    Ok(Grant { code, verifier })
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

/// What the key API made of a request signed with this computer's key.
pub enum Asked<T> {
    Done(T),
    /// The key isn't in use anymore: sign in again.
    Refused,
}

/// A request to the key API at `path`, signed with `key`.
fn ask(
    service: &str,
    path: &str,
    region: &str,
    key: (&str, &str),
) -> Result<Asked<serde_json::Value>> {
    let url = format!("{}{path}", service.trim_end_matches('/'));
    let (status, text) =
        omacloud_core::bucket::signed_request("POST", &url, region, key.0, key.1, "")?;
    match status {
        200 => Ok(Asked::Done(
            serde_json::from_str(&text).context("reading Omacloud's answer")?,
        )),
        403 => Ok(Asked::Refused),
        _ => bail!("Omacloud storage answered {status}: {}", text.trim()),
    }
}

/// A new storage key for this account, asked for with the current one.
pub fn new_key(service: &str, region: &str, key: (&str, &str)) -> Result<Asked<(String, String)>> {
    Ok(match ask(service, "/api/keys", region, key)? {
        Asked::Refused => Asked::Refused,
        Asked::Done(v) => {
            let field = |k: &str| {
                v[k].as_str()
                    .map(str::to_string)
                    .with_context(|| format!("Omacloud's answer has no {k}"))
            };
            Asked::Done((field("access_key_id")?, field("secret_access_key")?))
        }
    })
}

/// Revoke every key of the account but `key`; how many went.
pub fn retire_others(service: &str, region: &str, key: (&str, &str)) -> Result<Asked<u64>> {
    Ok(match ask(service, "/api/keys/retire", region, key)? {
        Asked::Refused => Asked::Refused,
        Asked::Done(v) => Asked::Done(v["retired"].as_u64().unwrap_or(0)),
    })
}
