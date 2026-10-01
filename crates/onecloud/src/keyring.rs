//! A self-hosted computer's bucket key, kept in the desktop keyring (the
//! Secret Service, through `secret-tool`) instead of the config file.
//!
//! One entry per config file. Without a keyring (no Secret Service, or
//! `ONECLOUD_KEYRING=0`) the key stays in the config file, which is private
//! to the user.

use std::{
    collections::BTreeMap,
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail, ensure};

const LABEL: &str = "OneCloud bucket key";

fn enabled() -> bool {
    std::env::var("ONECLOUD_KEYRING").map_or(true, |v| v != "0")
}

/// The entry's attributes: the config it belongs to, by absolute path.
fn attributes(config: &Path) -> Result<[String; 4]> {
    let path = std::path::absolute(config)?;
    let path = path.to_str().context("config path isn't UTF-8")?;
    Ok([
        "service".into(),
        "onecloud".into(),
        "config".into(),
        path.into(),
    ])
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
        .write_all(serde_json::to_string(key)?.as_bytes())?;
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
        "this computer's bucket key is in the keyring, and ONECLOUD_KEYRING=0 turns it off"
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
             `onecloud bucket set-key --access-key-id <id> --here-only` with the account's key"
        );
    }
    serde_json::from_slice(&out.stdout).context("the keyring's bucket key entry")
}
