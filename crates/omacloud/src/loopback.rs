//! What asking someone's leave in their browser takes on this side, for
//! Dropbox (`dropbox.rs`) and Cloudflare (`cloudflare.rs`) alike: a verifier
//! and its challenge (PKCE), the browser sent off, and a port on this
//! computer where the answer comes back.

use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore as _;
use sha2::{Digest as _, Sha256};

pub fn random_url_safe(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    rand::rngs::OsRng.fill_bytes(&mut b);
    URL_SAFE_NO_PAD.encode(b)
}

/// PKCE's S256 challenge for a verifier.
pub fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

pub fn on_desktop() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some()
}

/// What a browser is sent to: no secret of this process goes with it.
pub fn open_browser(url: &str) {
    let mut open = std::process::Command::new("xdg-open");
    for var in [
        "OMACLOUD_RECOVERY_CODE",
        "OMACLOUD_JOIN_CODE",
        "OMACLOUD_SECRET_ACCESS_KEY",
        "OMACLOUD_DROPBOX_AUTH_CODE",
        "OMACLOUD_DROPBOX_REFRESH_TOKEN",
        "OMACLOUD_CLOUDFLARE_TOKEN",
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

fn reply(mut stream: &TcpStream, status: &str, (title, text): (&str, &str)) {
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

/// How a browser came back, or didn't.
pub enum Came {
    Code(String),
    /// Turned down, in the other side's words if it gave any.
    Refused(Option<String>),
    TimedOut,
}

/// What a browser brings back to `path` on `listener` for the sign-in that
/// `state` names, within ten minutes. The page it's shown is `allowed` or
/// `refused` (a title and a line). A request for anything else, or with
/// another state, is turned away and waited past.
///
/// # Errors
///
/// If the port can't be listened on.
pub fn wait_for_code(
    listener: &TcpListener,
    path: &str,
    state: &str,
    allowed: (&str, &str),
    refused: (&str, &str),
) -> Result<Came> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        let stream = match listener.accept() {
            Ok((s, _)) => s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() > deadline {
                    return Ok(Came::TimedOut);
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
        // GET /path?code=...&state=... HTTP/1.1
        let target = line.split_whitespace().nth(1).unwrap_or_default();
        let asked = reqwest::Url::parse(&format!("http://127.0.0.1{target}"))
            .ok()
            .filter(|url| url.path() == path);
        let Some(asked) = asked else {
            reply(&stream, "404 Not Found", ("Not here", ""));
            continue;
        };
        let said = |name: &str| {
            asked
                .query_pairs()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.into_owned())
        };
        if said("state").as_deref() != Some(state) {
            reply(
                &stream,
                "400 Bad Request",
                ("That didn't work", "Start again from Omacloud."),
            );
            continue;
        }
        if let Some(code) = said("code") {
            reply(&stream, "200 OK", allowed);
            return Ok(Came::Code(code));
        }
        reply(&stream, "200 OK", refused);
        return Ok(Came::Refused(said("error_description").or_else(|| said("error"))));
    }
}
