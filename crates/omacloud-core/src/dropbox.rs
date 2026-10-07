//! Dropbox coordination writes. OpenDAL handles ordinary files, but its
//! Dropbox writer overwrites existing paths. Account logs need an atomic
//! create-only write, provided by Dropbox's `files/upload` add mode.

use std::{
    collections::{BTreeMap, HashSet},
    sync::Mutex,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

const API: &str = "https://api.dropboxapi.com";
const CONTENT: &str = "https://content.dropboxapi.com";

#[derive(Deserialize)]
struct Token {
    access_token: String,
    expires_in: u64,
}

#[derive(Deserialize)]
struct DropboxError {
    error_summary: String,
}

fn is_conflict(summary: &str, kind: &str) -> bool {
    summary.split('/').take(3).eq(["path", "conflict", kind])
}

/// The create-only part of a Dropbox coordinator. The refresh token and app
/// secret stay in the same keyring entry as the repository's credentials.
pub struct DropboxCreate {
    root: String,
    client_id: String,
    client_secret: String,
    refresh_token: String,
    token: Mutex<Option<(String, Instant)>>,
    directories: Mutex<HashSet<String>>,
    agent: ureq::Agent,
    api: String,
    content: String,
}

impl DropboxCreate {
    /// Build a Dropbox writer from OpenDAL options. Long-running sync needs
    /// a refresh token rather than a four-hour access token.
    pub fn new(options: &BTreeMap<String, String>) -> Result<Self> {
        let get = |key: &str| options.get(key).filter(|v| !v.is_empty()).cloned();
        let root = options
            .get("root")
            .map_or("", String::as_str)
            .trim_matches('/');
        ensure!(
            root.is_ascii()
                && (root.is_empty()
                    || !root
                        .split('/')
                        .any(|part| part.is_empty() || part == "." || part == "..")),
            "Dropbox root must be an ASCII path without empty or dot components"
        );
        Ok(Self {
            root: root.to_string(),
            client_id: get("client_id").context("Dropbox needs --opt client_id=...")?,
            client_secret: get("client_secret").context("Dropbox needs an app secret")?,
            refresh_token: get("refresh_token")
                .context("Dropbox needs an offline refresh token")?,
            token: Mutex::new(None),
            directories: Mutex::new(HashSet::new()),
            agent: ureq::Agent::config_builder()
                .http_status_as_error(false)
                .timeout_global(Some(Duration::from_secs(60)))
                .build()
                .into(),
            api: API.into(),
            content: CONTENT.into(),
        })
    }

    fn path(&self, key: &str) -> Result<String> {
        ensure!(
            !key.is_empty()
                && !key
                    .split('/')
                    .any(|p| p.is_empty() || p == "." || p == ".."),
            "invalid Dropbox object path"
        );
        Ok(if self.root.is_empty() {
            format!("/{key}")
        } else {
            format!("/{}/{key}", self.root)
        })
    }

    fn access_token(&self) -> Result<String> {
        let mut cached = self.token.lock().unwrap();
        if let Some((token, until)) = cached.as_ref()
            && Instant::now() < *until
        {
            return Ok(token.clone());
        }
        let mut response = self
            .agent
            .post(&format!("{}/oauth2/token", self.api))
            .send_form([
                ("grant_type", "refresh_token"),
                ("refresh_token", self.refresh_token.as_str()),
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
            ])
            .context("refreshing Dropbox access")?;
        ensure!(
            response.status().is_success(),
            "Dropbox refused this refresh token ({}); authorize again with `omacloud bucket set-key --here-only`",
            response.status()
        );
        let token: Token = response
            .body_mut()
            .read_json()
            .context("Dropbox token response")?;
        ensure!(
            !token.access_token.is_empty(),
            "Dropbox returned an empty access token"
        );
        let until = Instant::now() + Duration::from_secs(token.expires_in.saturating_sub(120));
        *cached = Some((token.access_token.clone(), until));
        Ok(token.access_token)
    }

    fn create_dir(&self, path: &str) -> Result<()> {
        if self.directories.lock().unwrap().contains(path) {
            return Ok(());
        }
        let token = self.access_token()?;
        let mut response = self
            .agent
            .post(&format!("{}/2/files/create_folder_v2", self.api))
            .header("authorization", &format!("Bearer {token}"))
            .send_json(serde_json::json!({ "path": path, "autorename": false }))
            .with_context(|| format!("creating Dropbox folder {path}"))?;
        match response.status().as_u16() {
            200 => {}
            409 => {
                let error: DropboxError = response
                    .body_mut()
                    .read_json()
                    .context("Dropbox folder error")?;
                ensure!(
                    is_conflict(&error.error_summary, "folder"),
                    "Dropbox folder {path}: {}",
                    error.error_summary
                );
            }
            status => bail!("Dropbox folder {path}: HTTP {status}"),
        }
        self.directories.lock().unwrap().insert(path.to_string());
        Ok(())
    }

    /// Create the root and each parent folder. Dropbox does not create
    /// missing parents when uploading a file.
    pub fn prepare_root(&self) -> Result<()> {
        self.prepare_dir(&self.root)
    }

    /// Ensure the folders of an ordinary OpenDAL write exist.
    pub fn prepare_key(&self, key: &str) -> Result<()> {
        let path = self.path(key)?;
        self.prepare_parent(&path)
    }

    fn prepare_parent(&self, path: &str) -> Result<()> {
        let (parent, _) = path.rsplit_once('/').expect("an absolute Dropbox path");
        self.prepare_dir(parent)
    }

    fn prepare_dir(&self, dir: &str) -> Result<()> {
        let mut path = String::new();
        for part in dir
            .trim_start_matches('/')
            .split('/')
            .filter(|p| !p.is_empty())
        {
            path.push('/');
            path.push_str(part);
            self.create_dir(&path)?;
        }
        Ok(())
    }

    /// Atomically add a coordination record. A conflict means another
    /// computer won that log position. `strict_conflict` also makes an
    /// identical existing file a conflict.
    pub fn create(&self, key: &str, bytes: &[u8]) -> Result<bool> {
        let path = self.path(key)?;
        self.prepare_parent(&path)?;
        let token = self.access_token()?;
        let args = serde_json::json!({
            "path": path, "mode": "add", "autorename": false,
            "mute": true, "strict_conflict": true,
        });
        let mut response = self
            .agent
            .post(&format!("{}/2/files/upload", self.content))
            .header("authorization", &format!("Bearer {token}"))
            .header("content-type", "application/octet-stream")
            .header("Dropbox-API-Arg", &args.to_string())
            .send(bytes)
            .with_context(|| format!("adding Dropbox record {key}"))?;
        match response.status().as_u16() {
            200 => Ok(true),
            409 => {
                let error: DropboxError = response
                    .body_mut()
                    .read_json()
                    .context("Dropbox upload error")?;
                if is_conflict(&error.error_summary, "file") {
                    Ok(false)
                } else {
                    bail!("Dropbox record {key}: {}", error.error_summary)
                }
            }
            status => bail!("Dropbox record {key}: HTTP {status}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::TcpListener,
    };

    #[test]
    fn only_a_file_at_the_target_is_a_create_collision() {
        assert!(is_conflict("path/conflict/file/...", "file"));
        assert!(!is_conflict("path/conflict/file_ancestor/...", "file"));
        assert!(!is_conflict("path/conflict/folder/...", "file"));
    }

    #[test]
    fn add_conflict_is_not_an_overwrite() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let url = format!("http://{}", listener.local_addr()?);
        let server = std::thread::spawn(move || -> Result<()> {
            for step in 0..4 {
                let (mut stream, _) = listener.accept()?;
                stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                let mut reader = BufReader::new(stream.try_clone()?);
                let mut line = String::new();
                reader.read_line(&mut line)?;
                let expected = match step {
                    0 => "/oauth2/token",
                    1 => "/2/files/create_folder_v2",
                    _ => "/2/files/upload",
                };
                assert!(line.contains(expected), "{line}");
                let mut len = 0;
                loop {
                    line.clear();
                    reader.read_line(&mut line)?;
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = value.trim().parse()?;
                    }
                    if step >= 2 && line.to_ascii_lowercase().starts_with("dropbox-api-arg:") {
                        assert!(line.contains("strict_conflict"));
                        assert!(line.contains("add"));
                    }
                }
                let mut body = vec![0; len];
                reader.read_exact(&mut body)?;
                let (status, answer) = match step {
                    0 => (
                        "200 OK",
                        r#"{"access_token":"temporary","expires_in":14400}"#,
                    ),
                    1 | 2 => ("200 OK", "{}"),
                    _ => (
                        "409 Conflict",
                        r#"{"error_summary":"path/conflict/file/..."}"#,
                    ),
                };
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{answer}",
                    answer.len()
                )?;
            }
            Ok(())
        });
        let opts = [
            ("client_id".into(), "app".into()),
            ("client_secret".into(), "secret".into()),
            ("refresh_token".into(), "refresh".into()),
        ]
        .into();
        let mut writer = DropboxCreate::new(&opts)?;
        writer.api = url.clone();
        writer.content = url;
        assert!(writer.create("heads/0001.json", b"first")?);
        assert!(!writer.create("heads/0001.json", b"second")?);
        server.join().expect("mock Dropbox server")?;
        Ok(())
    }
}
