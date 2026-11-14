//! Taking in what a browser uploaded (see `omacloud_core::inbox`): the daemon
//! looks at the inbox's marker once a minute, and when it moved, opens each
//! file waiting, checks it, and saves it into the synced folder. The file
//! watcher then sees a new file, and the next sync carries it like any other.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use log::{info, warn};
use omacloud_core::{Engine, Inbox, RepoSpec, inbox};

/// How often the marker is read: one small read.
const LOOK_EVERY: Duration = Duration::from_secs(60);

/// How long a file that fails its checks is left before it's removed: a key
/// rotation under way can make a good one unreadable for a while.
const KEEP_BAD: u64 = 24 * 3600;

pub struct Uploads {
    inbox: Inbox,
    /// The marker as last acted on; the outer `None` before the first look.
    seen: Option<Option<String>>,
    looked: Option<Instant>,
}

impl Uploads {
    /// For an account at a provider; `None` for one kept in a folder.
    pub fn new(repo: &RepoSpec) -> Option<Self> {
        repo.is_remote()
            .then(|| Inbox::beside(repo).ok())
            .flatten()
            .map(|inbox| Self {
                inbox,
                seen: None,
                looked: None,
            })
    }

    /// Take in what's waiting, if it's time to look and something changed.
    /// `folders` are the names this computer syncs, or `None` when it syncs
    /// the whole of `root`. A failure is only logged: the next look tries
    /// again.
    pub fn take_in(&mut self, engine: &mut Engine, root: &Path, folders: Option<&[String]>) {
        if self.looked.is_some_and(|t| t.elapsed() < LOOK_EVERY) {
            return;
        }
        self.looked = Some(Instant::now());
        let marker = match self.inbox.marker() {
            Ok(marker) => marker,
            Err(e) => return warn!("looking at the inbox: {e:#}"),
        };
        if self.seen.as_ref() == Some(&marker) {
            return;
        }
        // nothing was ever uploaded: nothing to list
        if marker.is_none() {
            self.seen = Some(marker);
            return;
        }
        match self.take_all(engine, root, folders) {
            // only when all of it went through: what's left is tried again
            Ok(true) => self.seen = Some(marker),
            Ok(false) => {}
            Err(e) => warn!("taking in uploads: {e:#}"),
        }
    }

    /// Whether nothing is left to try again.
    fn take_all(&self, engine: &mut Engine, root: &Path, folders: Option<&[String]>) -> Result<bool> {
        let waiting = self.inbox.waiting()?;
        if waiting.is_empty() {
            return Ok(true);
        }
        // who is in the account now decides whose word counts
        _ = engine.refresh_devices()?;
        let (spec, secret, _) = engine.repository()?;
        let reader = spec
            .key_files()?
            .iter()
            .find_map(|key| omacloud_web::Reader::open(&secret.password, key).ok())
            .context("the repository's key didn't open")?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let mut done = true;
        for (name, written) in waiting {
            let opened = self
                .inbox
                .read(&name)
                .and_then(|sealed| reader.unseal(&sealed).map_err(anyhow::Error::msg))
                .and_then(|plain| inbox::open(&plain, written, |device| engine.is_member(device)));
            let upload = match opened {
                Ok(upload) => upload,
                Err(e) => {
                    warn!("an upload was turned away: {e:#}");
                    if now.saturating_sub(written) > KEEP_BAD {
                        self.inbox.remove(&name)?;
                    }
                    continue;
                }
            };
            // a folder this computer doesn't sync is another computer's to take
            let top = upload
                .path
                .components()
                .next()
                .map(|c| c.as_os_str().to_string_lossy().into_owned());
            if folders.is_some_and(|f| !top.is_some_and(|t| f.contains(&t))) {
                done = false;
                continue;
            }
            let to = save(root, &upload)?;
            info!("took in {} from a browser", to.display());
            self.inbox.remove(&name)?;
        }
        Ok(done)
    }
}

/// A name beside `path` that nothing has: `plan (2).md`.
fn free_name(path: &Path) -> PathBuf {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (2..)
        .map(|n| path.with_file_name(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .unwrap_or_else(|| path.to_path_buf())
}

/// Save an upload under `root`, never over a file that's there (unless it's
/// the same file, taken in before): where it was saved.
fn save(root: &Path, upload: &inbox::Upload) -> Result<PathBuf> {
    let mut to = root.join(&upload.path);
    if let Ok(there) = fs::read(&to) {
        if there == upload.content {
            return Ok(to);
        }
        to = free_name(&to);
    }
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    // whole under another name first (one that stays on this computer): a sync never sees half of it
    let part = to.with_file_name(format!(
        ".{}.part.nosync",
        to.file_name().unwrap_or_default().to_string_lossy()
    ));
    fs::write(&part, &upload.content)?;
    if let Some(mtime) = upload.mtime {
        let file = fs::File::options().write(true).open(&part)?;
        _ = file.set_modified(UNIX_EPOCH + Duration::from_millis(mtime));
    }
    fs::rename(&part, &to).with_context(|| format!("saving {}", to.display()))?;
    Ok(to)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_upload_is_saved_beside_never_over() {
        let dir = tempfile::tempdir().unwrap();
        let upload = |content: &[u8]| inbox::Upload {
            path: PathBuf::from("Work/plan.md"),
            mtime: Some(1_700_000_000_000),
            content: content.to_vec(),
        };
        assert_eq!(
            save(dir.path(), &upload(b"one")).unwrap(),
            dir.path().join("Work/plan.md")
        );
        // the same file again (two computers took it in): still one
        assert_eq!(
            save(dir.path(), &upload(b"one")).unwrap(),
            dir.path().join("Work/plan.md")
        );
        assert_eq!(
            save(dir.path(), &upload(b"two")).unwrap(),
            dir.path().join("Work/plan (2).md")
        );
        assert_eq!(
            save(dir.path(), &upload(b"three")).unwrap(),
            dir.path().join("Work/plan (3).md")
        );
        assert_eq!(fs::read(dir.path().join("Work/plan.md")).unwrap(), b"one");
        let changed = fs::metadata(dir.path().join("Work/plan.md"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(changed, UNIX_EPOCH + Duration::from_secs(1_700_000_000));
        assert_eq!(fs::read_dir(dir.path().join("Work")).unwrap().count(), 3);
    }
}
