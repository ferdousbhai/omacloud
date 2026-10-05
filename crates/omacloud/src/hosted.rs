//! Omacloud storage: signing in with Google gets this computer a key to
//! its own space at omacloud.computer, in place of a bucket of your own.
//!
//! The browser signs in; the server sends it back to a port this computer
//! listens on with a one-time code, which this computer trades, with a
//! secret only it has, for the key.

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

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let mut response = agent
        .post(&format!("{service}/api/credentials"))
        .send_json(serde_json::json!({ "code": code, "verifier": verifier }))
        .with_context(|| format!("reaching {service}"))?;
    if !response.status().is_success() {
        bail!("sign-in didn't work ({}): try again", response.status());
    }
    let storage: Storage = response
        .body_mut()
        .read_json()
        .context("reading the key from Omacloud")?;
    Ok(storage)
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}
