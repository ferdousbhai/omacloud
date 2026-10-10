//! A computer's bucket key, kept in the desktop keyring (the
//! Secret Service, through `secret-tool`) instead of the config file.
//!
//! One entry per config file. Without a keyring (no Secret Service, or
//! `OMACLOUD_KEYRING=0`) the key stays in the config file, which is private
//! to the user.
//!
//! The entry holds the key as JSON in standard base64, after `base64:`.
//! GNOME Keyring's unencrypted keyring file writes a text secret as it is but
//! reads it back unescaped, so a `\` or a newline in one comes back changed,
//! or not at all, once the file is read again (gnome-keyring#158). An entry
//! without `base64:` is plain JSON from before this, read as it is; the next
//! save writes it in base64.

use std::{
    collections::BTreeMap,
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};

const LABEL: &str = "Omacloud bucket key";
const BASE64: &str = "base64:";

fn enabled() -> bool {
    std::env::var("OMACLOUD_KEYRING").map_or(true, |v| v != "0")
}

/// The entry's attributes: the config it belongs to, by absolute path.
fn attributes(config: &Path) -> Result<[String; 4]> {
    let path = std::path::absolute(config)?;
    let path = path.to_str().context("config path isn't UTF-8")?;
    Ok([
        "service".into(),
        "omacloud".into(),
        "config".into(),
        path.into(),
    ])
}

fn encode(key: &BTreeMap<String, String>) -> Result<String> {
    let json = serde_json::to_string(key)?;
    Ok(format!("{BASE64}{}", STANDARD.encode(json)))
}

fn decode(stored: &[u8]) -> Result<BTreeMap<String, String>> {
    let json = match stored.strip_prefix(BASE64.as_bytes()) {
        Some(b64) => STANDARD.decode(b64)?,
        None => stored.to_vec(),
    };
    Ok(serde_json::from_slice(&json)?)
}

/// Keep `key` for `config`. False when there's no keyring to keep it in.
pub fn store(config: &Path, key: &BTreeMap<String, String>) -> Result<bool> {
    if !enabled() {
        return Ok(false);
    }
    let mut child = match Command::new("secret-tool")
        .args(["store", "--label", LABEL])
        .args(attributes(config)?)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).context("running secret-tool"),
    };
    // the secret goes through stdin, never the command line
    child
        .stdin
        .take()
        .context("secret-tool's stdin")?
        .write_all(encode(key)?.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        log::warn!(
            "no keyring to keep the bucket key in ({}); it stays in the config file",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return Ok(false);
    }
    Ok(true)
}

/// The key kept for `config`.
pub fn lookup(config: &Path) -> Result<BTreeMap<String, String>> {
    ensure!(
        enabled(),
        "this computer's bucket key is in the keyring, and OMACLOUD_KEYRING=0 turns it off"
    );
    let out = Command::new("secret-tool")
        .arg("lookup")
        .args(attributes(config)?)
        .stderr(Stdio::null())
        .output()
        .context("running secret-tool to read the bucket key from the keyring")?;
    if !out.status.success() || out.stdout.is_empty() {
        bail!(
            "the bucket key isn't in the keyring (is it unlocked?). If it's gone, run \
             `omacloud bucket set-key --access-key-id <id> --here-only` with the account's key"
        );
    }
    decode(&out.stdout).context("the keyring's bucket key entry")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_round_trips_through_base64() -> Result<()> {
        let key = BTreeMap::from([
            ("access_key_id".to_string(), "id".to_string()),
            (
                "secret_access_key".to_string(),
                "a\\\"b\\\\c\nd".to_string(),
            ),
        ]);
        let stored = encode(&key)?;
        assert!(!stored.contains(['\\', '\n']), "{stored}");
        assert_eq!(decode(stored.as_bytes())?, key);
        // an entry from before base64
        assert_eq!(decode(serde_json::to_string(&key)?.as_bytes())?, key);
        Ok(())
    }
}
