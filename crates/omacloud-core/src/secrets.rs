//! The secrets bundle: ssh and gpg keys, tokens. Never synced as files.
//!
//! `omacloud secrets save` packs the listed files into one bundle sealed to
//! the account root, and it travels in the folder's history like a package
//! list (one per device, under [`crate::SECRETS`]). Any device can seal to the
//! root, but only the recovery code opens a bundle: a device holding the
//! repository key, even one removed later, can't read another machine's
//! keys, and a restore always takes the person's own code and consent.

use std::{
    fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use crypto_box::{PublicKey, SecretKey, aead::OsRng};
use ed25519_dalek::{SigningKey, VerifyingKey};

/// What a bundle holds unless `~/.config/omacloud/secrets` lists otherwise.
pub const DEFAULT_PATHS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".netrc",
    ".git-credentials",
    ".config/git/credentials",
    // npm registry tokens; kept out of settings for that reason
    ".npmrc",
    ".config/gh/hosts.yml",
    ".config/icloud-md",
];

/// Files in `.gnupg` that belong to the running agent, not to the keys.
const SKIP: &[&str] = &["random_seed", ".#lk"];

/// A bundle stays small: keys and tokens, not data.
const MAX_BYTES: u64 = 16 << 20;

const MAGIC: &[u8] = b"omacloud-secrets-v1\n";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Relative to home.
    pub path: PathBuf,
    /// Permission bits.
    pub mode: u32,
    pub data: Vec<u8>,
}

/// The paths to bundle: one per line (relative to home, `#` comments) from
/// `list`, or [`DEFAULT_PATHS`] when there is no list.
///
/// # Errors
///
/// If a listed path isn't plainly relative.
pub fn paths(list: Option<&str>) -> Result<Vec<PathBuf>> {
    let Some(list) = list else {
        return Ok(DEFAULT_PATHS.iter().map(PathBuf::from).collect());
    };
    list.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| {
            let p = PathBuf::from(l);
            ensure!(plain(&p), "`{l}` must be a plain path relative to home");
            Ok(p)
        })
        .collect()
}

fn plain(p: &Path) -> bool {
    p.components().next().is_some() && p.components().all(|c| matches!(c, Component::Normal(_)))
}

/// Regular files at or under `paths`, relative to `home`. Symlinks, sockets
/// and missing paths are skipped.
///
/// # Errors
///
/// If a file can't be read, or the files add up to more than 16 MB.
pub fn collect(home: &Path, paths: &[PathBuf]) -> Result<Vec<Entry>> {
    fn go(home: &Path, rel: &Path, out: &mut Vec<Entry>, total: &mut u64) -> Result<()> {
        let full = home.join(rel);
        let Ok(meta) = fs::symlink_metadata(&full) else {
            return Ok(());
        };
        if meta.is_dir() {
            let mut names: Vec<_> = fs::read_dir(&full)?
                .map(|e| e.map(|e| e.file_name()))
                .collect::<std::io::Result<_>>()?;
            names.sort();
            for name in names {
                go(home, &rel.join(name), out, total)?;
            }
        } else if meta.is_file() {
            let name = rel.file_name().unwrap_or_default().to_string_lossy();
            if SKIP.contains(&name.as_ref()) {
                return Ok(());
            }
            *total += meta.len();
            ensure!(
                *total <= MAX_BYTES,
                "the secrets add up to more than 16 MB; list fewer paths in ~/.config/omacloud/secrets"
            );
            out.push(Entry {
                path: rel.to_path_buf(),
                mode: meta.permissions().mode() & 0o7777,
                data: fs::read(&full).with_context(|| format!("reading {}", full.display()))?,
            });
        }
        Ok(())
    }
    let (mut out, mut total) = (Vec::new(), 0);
    for p in paths {
        go(home, p, &mut out, &mut total)?;
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out.dedup_by(|a, b| a.path == b.path);
    Ok(out)
}

fn encode(entries: &[Entry]) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    for e in entries {
        let path = e.path.to_string_lossy();
        out.extend((path.len() as u32).to_le_bytes());
        out.extend(path.as_bytes());
        out.extend(e.mode.to_le_bytes());
        out.extend((e.data.len() as u64).to_le_bytes());
        out.extend(&e.data);
    }
    out
}

fn decode(bytes: &[u8]) -> Result<Vec<Entry>> {
    fn take<'a>(b: &mut &'a [u8], n: usize) -> Result<&'a [u8]> {
        ensure!(b.len() >= n, "the secrets bundle is cut short");
        let (head, rest) = b.split_at(n);
        *b = rest;
        Ok(head)
    }
    let mut b = bytes
        .strip_prefix(MAGIC)
        .context("not an omacloud secrets bundle")?;
    let mut out = Vec::new();
    while !b.is_empty() {
        let n = u32::from_le_bytes(take(&mut b, 4)?.try_into()?) as usize;
        let path = PathBuf::from(std::str::from_utf8(take(&mut b, n)?)?);
        ensure!(plain(&path), "the bundle holds a path outside home");
        let mode = u32::from_le_bytes(take(&mut b, 4)?.try_into()?);
        let len = usize::try_from(u64::from_le_bytes(take(&mut b, 8)?.try_into()?))?;
        let data = take(&mut b, len)?.to_vec();
        out.push(Entry { path, mode, data });
    }
    Ok(out)
}

/// Seal `entries` to the account root (hex public key).
///
/// # Errors
///
/// If `root` isn't a valid public key.
pub fn seal(root: &str, entries: &[Entry]) -> Result<Vec<u8>> {
    let bytes: [u8; 32] = hex::decode(root)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("not a 32 byte key"))?;
    let x25519 = VerifyingKey::from_bytes(&bytes)?.to_montgomery().to_bytes();
    PublicKey::from(x25519)
        .seal(&mut OsRng, &encode(entries))
        .map_err(|_| anyhow::anyhow!("sealing failed"))
}

/// Open a bundle with the root key (from the recovery code).
///
/// # Errors
///
/// If the bundle wasn't sealed to this root, or was tampered with.
pub fn open(root_key: &SigningKey, sealed: &[u8]) -> Result<Vec<Entry>> {
    let plain = SecretKey::from(root_key.to_scalar_bytes())
        .unseal(sealed)
        .map_err(|_| anyhow::anyhow!("that recovery code doesn't open this bundle"))?;
    decode(&plain)
}

/// What restoring changes: files that are new or differ here.
#[must_use]
pub fn changes<'a>(home: &Path, entries: &'a [Entry]) -> Vec<&'a Entry> {
    entries
        .iter()
        .filter(|e| fs::read(home.join(&e.path)).map_or(true, |d| d != e.data))
        .collect()
}

/// Write `entries` under `home`. A file that exists and differs is first
/// copied to `backups` (same relative path); new directories are private.
/// Returns the paths written.
///
/// # Errors
///
/// If a file can't be backed up or written; a symlink in the way is
/// refused rather than followed.
pub fn restore(home: &Path, entries: &[Entry], backups: &Path) -> Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    for e in changes(home, entries) {
        let dest = home.join(&e.path);
        let mut dir = home.to_path_buf();
        for c in e.path.parent().into_iter().flat_map(Path::components) {
            dir.push(c);
            match fs::symlink_metadata(&dir) {
                Ok(m) if m.is_dir() => {}
                Ok(_) => bail!("{} is in the way", dir.display()),
                Err(_) => fs::DirBuilder::new().mode(0o700).create(&dir)?,
            }
        }
        match fs::symlink_metadata(&dest) {
            Ok(m) if m.file_type().is_symlink() => {
                bail!("{} is a symlink; not writing through it", dest.display())
            }
            Ok(_) => {
                let backup = backups.join(&e.path);
                fs::create_dir_all(backup.parent().unwrap_or(backups))?;
                fs::copy(&dest, &backup)?;
            }
            Err(_) => {}
        }
        let tmp = dest.with_file_name(format!(
            ".{}.omacloud-tmp",
            dest.file_name().unwrap_or_default().to_string_lossy()
        ));
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(&e.data)?;
        f.sync_all()?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(e.mode))?;
        fs::rename(&tmp, &dest)?;
        written.push(e.path.clone());
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::{public_hex, root_key};

    #[test]
    fn round_trip_only_through_the_root() -> Result<()> {
        let home = tempfile::tempdir()?;
        let h = home.path();
        fs::create_dir_all(h.join(".ssh"))?;
        fs::create_dir_all(h.join(".gnupg"))?;
        fs::write(h.join(".ssh/id_ed25519"), "private\n")?;
        fs::set_permissions(h.join(".ssh/id_ed25519"), fs::Permissions::from_mode(0o600))?;
        fs::write(h.join(".ssh/config"), "Host x\n")?;
        fs::write(h.join(".gnupg/random_seed"), "noise")?;
        std::os::unix::fs::symlink("config", h.join(".ssh/link"))?;
        let entries = collect(h, &paths(None)?)?;
        let names: Vec<_> = entries.iter().map(|e| e.path.clone()).collect();
        assert_eq!(names, [".ssh/config", ".ssh/id_ed25519"].map(PathBuf::from));

        let code = crate::devices::new_recovery_code();
        let root = root_key(&code)?;
        let sealed = seal(&public_hex(&root), &entries)?;
        let other = SigningKey::generate(&mut rand::rngs::OsRng);
        assert!(open(&other, &sealed).is_err());
        assert_eq!(open(&root, &sealed)?, entries);

        // on a machine with an older config, the old one is backed up
        let fresh = tempfile::tempdir()?;
        let f = fresh.path();
        fs::create_dir_all(f.join(".ssh"))?;
        fs::write(f.join(".ssh/config"), "old\n")?;
        let backups = f.join("backups");
        let opened = open(&root, &sealed)?;
        assert_eq!(changes(f, &opened).len(), 2);
        assert_eq!(restore(f, &opened, &backups)?.len(), 2);
        assert_eq!(fs::read_to_string(f.join(".ssh/id_ed25519"))?, "private\n");
        let mode = fs::metadata(f.join(".ssh/id_ed25519"))?
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(fs::read_to_string(backups.join(".ssh/config"))?, "old\n");
        assert!(changes(f, &opened).is_empty());
        Ok(())
    }

    #[test]
    fn refuses_paths_outside_home_and_symlinks_in_the_way() -> Result<()> {
        assert!(paths(Some("../etc/passwd")).is_err());
        assert!(paths(Some("/etc/passwd")).is_err());
        assert_eq!(paths(Some("# mine\n.ssh\n"))?, [PathBuf::from(".ssh")]);
        let bad = encode(&[Entry {
            path: "../x".into(),
            mode: 0o600,
            data: vec![],
        }]);
        assert!(decode(&bad).is_err());
        assert!(decode(&MAGIC[..5]).is_err());

        let home = tempfile::tempdir()?;
        let h = home.path();
        let outside = tempfile::tempdir()?;
        std::os::unix::fs::symlink(outside.path(), h.join(".ssh"))?;
        let entry = Entry {
            path: ".ssh/id".into(),
            mode: 0o600,
            data: b"k".to_vec(),
        };
        assert!(restore(h, &[entry], &h.join("b")).is_err());
        assert!(fs::read_dir(outside.path())?.next().is_none());
        Ok(())
    }
}
