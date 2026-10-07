//! Settings kept as merged documents rather than as files: what an app keeps
//! in its own store (Chromium's profile, NetworkManager's saved networks) is
//! read into a canonical JSON document, which syncs under [`PREFIX`] and
//! merges three ways, and is written back into the app when the app allows.
//!
//! Each computer keeps two copies of every document:
//! - the *mirror*, the document as it syncs (`merged/synced`), and
//! - the *applied* copy, the document as the app last held it
//!   (`merged/applied`).
//!
//! A capture that differs from the applied copy is a change made here, and
//! merges into the mirror. A mirror that differs from the applied copy holds
//! changes from elsewhere, written into the app once it isn't busy (Chromium
//! open). The two never lose each other's edits: a change made here while
//! another waits merges with it, and the merge settles the rest the same way
//! every time (see [`merge_map`]).

use std::{
    collections::BTreeSet,
    fmt, fs,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow};
use rustic_core::jiff::{Timestamp, tz::TimeZone};
use serde::Serialize;
use serde_json::{Map, Value};

/// Where merged documents live in the synced tree:
/// `.omacloud/merged/<group>/<document>`.
pub const PREFIX: &str = ".omacloud/merged";

/// Stands for the merging computer's name in conflict lines.
pub const ME: &str = "{me}";

/// An app turned a change down (NetworkManager refused it): not tried
/// again until the document or the app changes. Any other failure of
/// [`Group::apply`] is tried again at the next sync.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Refused(pub String);

/// Whether a changed file matters to a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Concern {
    No,
    /// Worth a look, at most every so often: the app writes it often.
    Look,
    /// Look now: the app may now allow writing (Chromium closed).
    Now,
}

/// A merge's result, with a line for each edit made on both sides that the
/// merge had to settle (the computer merging keeps its own). The lines say
/// `{me}` for the computer merging; [`ME`] is replaced by its name when
/// they're recorded, so the other computers read who kept what.
#[derive(Debug, Clone, PartialEq)]
pub struct Merged {
    pub value: Value,
    pub conflicts: Vec<String>,
}

/// One state shown for a settings group, as `omacloud settings` and the app
/// list them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Item {
    /// `shell`, `apps`, `chromium`, `wifi`.
    pub group: String,
    /// Like "Chromium: Default".
    pub title: String,
    /// `synced`, `new` (nothing synced yet), `waiting` (for an app to
    /// close, or to take the change), `refused` (the app turned a change
    /// down; not tried again until something changes), `install` (something to add by hand),
    /// `settled` (edits made on two computers were settled lately),
    /// `conflict` (a choice to make), `unavailable`.
    pub state: String,
    pub detail: String,
    /// Things to add here by hand, each with a page and a command.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub install: Vec<Install>,
    /// What stays on this computer, and why.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub not_synced: Vec<String>,
    /// The documents it covers, as a prefix under [`PREFIX`]
    /// (`chromium/Default/`), to find the conflicts settled in them.
    #[serde(skip)]
    pub scope: String,
}

/// Something to install by hand (a browser extension).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Install {
    pub id: String,
    pub name: String,
    pub url: String,
    /// Opens `url` where it installs (the right browser profile).
    pub command: Vec<String>,
}

/// An app whose settings sync as merged documents.
pub trait Group: Send + Sync {
    /// Its directory under [`PREFIX`], like `chromium`.
    fn name(&self) -> &str;

    /// The documents this computer has, relative to the group's directory
    /// (`Default/bookmarks.json`).
    fn docs(&self) -> Vec<String>;

    /// The document as the app holds it now; `None` when it can't be read
    /// now, and then it neither syncs from here nor is written here.
    /// `applied` is what the last capture found, for what can't be read
    /// this time but isn't gone (a Wi-Fi key a keyring keeps).
    ///
    /// # Errors
    ///
    /// When reading fails in a way worth reporting.
    fn capture(&self, doc: &str, applied: Option<&Value>) -> Result<Option<Value>>;

    /// Drop anything kept from earlier looks: the next capture reads the
    /// app afresh.
    fn forget(&self) {}

    /// Whether a sync should look at the app now; one with nothing to watch
    /// is looked at every so often instead of every sync.
    fn due(&self) -> bool {
        true
    }

    /// Why the document can't be written now, if it can't.
    fn busy(&self, doc: &str) -> Option<String>;

    /// Write `value` into the app, which held `applied` at the last
    /// capture. Anything replaced is copied into `backups` first.
    ///
    /// # Errors
    ///
    /// When writing fails; nothing is half written.
    fn apply(
        &self,
        doc: &str,
        value: &Value,
        applied: Option<&Value>,
        backups: &Path,
    ) -> Result<()>;

    /// What the last [`Group::apply`] of `doc` held back for now, and why
    /// (a Wi-Fi network in use isn't forgotten under the user): it's tried
    /// again at the next sync.
    fn held(&self, _doc: &str) -> Option<String> {
        None
    }

    fn merge(&self, doc: &str, base: Option<&Value>, ours: &Value, theirs: &Value) -> Merged;

    /// Whether the synced `mirror` holds anything to write here, given what
    /// the app held at the last capture.
    fn pending(&self, _doc: &str, mirror: &Value, applied: Option<&Value>) -> bool {
        Some(mirror) != applied
    }

    /// What `omacloud settings` shows. `mirror` is the group's directory of
    /// synced documents; `waiting` lists documents with changes held back,
    /// and why.
    fn status(&self, mirror: &Path, waiting: &[(String, String)]) -> Vec<Item>;

    /// Directories to watch (not recursively) for the app's changes.
    fn watch_dirs(&self) -> Vec<PathBuf>;

    /// Whether a change at `path` may matter to this group.
    fn concerns(&self, path: &Path) -> Concern;
}

impl fmt::Debug for dyn Group {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Group({})", self.name())
    }
}

/// A document's file under `dir`, `None` if it's missing or isn't JSON
/// (a damaged copy is captured or synced again).
#[must_use]
pub fn read(path: &Path) -> Option<Value> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

/// Write a document readable by this user alone, the way an editor saves.
///
/// # Errors
///
/// When the file can't be written.
pub fn write(path: &Path, value: &Value) -> Result<()> {
    let mut text = serde_json::to_vec_pretty(value)?;
    text.push(b'\n');
    write_atomic(path, &text, 0o600)
}

/// Replace `path` with `content` through a temp file beside it, flushed and
/// renamed over it, with `mode`. Parents are made as needed.
///
/// # Errors
///
/// When the file can't be written.
pub fn write_atomic(path: &Path, content: &[u8], mode: u32) -> Result<()> {
    let (parent, name) = path
        .parent()
        .zip(path.file_name())
        .ok_or_else(|| anyhow!("bad path {}", path.display()))?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".omacloud-tmp-{}", name.to_string_lossy()));
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(&tmp)
        .with_context(|| format!("writing {}", tmp.display()))?;
    f.write_all(content)?;
    f.set_permissions(fs::Permissions::from_mode(mode))?;
    f.sync_all()?;
    fs::rename(&tmp, path)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

/// Keep `content` in this second's backup in `backups/history`, as `rel`,
/// readable by this user alone.
///
/// # Errors
///
/// When it can't be written.
pub fn back_up_bytes(backups: &Path, rel: &Path, content: &[u8]) -> Result<()> {
    // the same as the newest copy: nothing new to keep (a write retried)
    if newest(backups, rel).is_some_and(|p| fs::read(p).is_ok_and(|b| b == content)) {
        return Ok(());
    }
    let stamp = Timestamp::now()
        .to_zoned(TimeZone::system())
        .strftime("%Y%m%d-%H%M%S")
        .to_string();
    let dest = backups.join("history").join(stamp).join(rel);
    write_atomic(&dest, content, 0o600)?;
    let mut dir = dest.parent();
    while let Some(d) = dir.filter(|d| d.starts_with(backups) && *d != backups) {
        fs::set_permissions(d, fs::Permissions::from_mode(0o700))?;
        dir = d.parent();
    }
    Ok(())
}

/// Copy `src` into this second's backup in `backups/history`, as `rel`,
/// readable by this user alone, as settings backups are.
///
/// # Errors
///
/// When the copy fails.
pub fn back_up(backups: &Path, rel: &Path, src: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if !src.is_file() {
        return Ok(());
    }
    if newest(backups, rel).is_some_and(|p| same_file(&p, src)) {
        return Ok(());
    }
    let stamp = Timestamp::now()
        .to_zoned(TimeZone::system())
        .strftime("%Y%m%d-%H%M%S")
        .to_string();
    let dest = backups.join("history").join(stamp).join(rel);
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dest.parent().ok_or_else(|| anyhow!("bad path"))?)?;
    fs::copy(src, &dest)?;
    fs::set_permissions(&dest, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// The newest copy of `rel` in `backups/history`, if any.
#[must_use]
pub fn newest(backups: &Path, rel: &Path) -> Option<PathBuf> {
    let mut stamps: Vec<PathBuf> = fs::read_dir(backups.join("history"))
        .ok()?
        .flatten()
        .map(|e| e.path())
        .collect();
    stamps.sort();
    stamps
        .iter()
        .rev()
        .map(|d| d.join(rel))
        .find(|p| p.is_file())
}

/// Whether two files hold the same bytes.
fn same_file(a: &Path, b: &Path) -> bool {
    let (Ok(ma), Ok(mb)) = (fs::metadata(a), fs::metadata(b)) else {
        return false;
    };
    ma.len() == mb.len() && fs::read(a).ok() == fs::read(b).ok()
}

/// Three way merge of a map of entries, as every merged document is.
///
/// - A change on one side wins over no change on the other.
/// - An entry deleted on one side and changed on the other is kept, with
///   the change: an edit beats a delete, as for files.
/// - Added on both sides, or changed on both sides to different values:
///   `ours` (the computer merging) wins, field by field when `fields` and
///   both are objects, and a line in `conflicts` says so (`label` names the
///   entry there, without its contents).
pub fn merge_map(
    base: Option<&Map<String, Value>>,
    ours: &Map<String, Value>,
    theirs: &Map<String, Value>,
    fields: bool,
    label: &dyn Fn(&str) -> String,
    conflicts: &mut Vec<String>,
) -> Map<String, Value> {
    let keys: BTreeSet<&String> = ours
        .keys()
        .chain(theirs.keys())
        .chain(base.into_iter().flat_map(Map::keys))
        .collect();
    let mut out = Map::new();
    for key in keys {
        let b = base.and_then(|m| m.get(key));
        let merged = match (ours.get(key), theirs.get(key)) {
            (Some(o), Some(t)) => Some(merge_value(b, o, t, fields, &|| label(key), conflicts)),
            (Some(o), None) if b.is_none() => Some(o.clone()),
            (Some(o), None) if Some(o) == b => None,
            (Some(o), None) => {
                conflicts.push(format!(
                    "{}: changed on {ME}, removed on another computer; kept",
                    label(key)
                ));
                Some(o.clone())
            }
            (None, Some(t)) if b.is_none() => Some(t.clone()),
            (None, Some(t)) if Some(t) == b => None,
            (None, Some(t)) => {
                conflicts.push(format!(
                    "{}: removed on {ME}, changed on another computer; kept",
                    label(key)
                ));
                Some(t.clone())
            }
            (None, None) => None,
        };
        if let Some(v) = merged {
            _ = out.insert(key.clone(), v);
        }
    }
    out
}

/// One value changed on both sides (see [`merge_map`]).
fn merge_value(
    b: Option<&Value>,
    o: &Value,
    t: &Value,
    fields: bool,
    label: &dyn Fn() -> String,
    conflicts: &mut Vec<String>,
) -> Value {
    if o == t || Some(t) == b {
        return o.clone();
    }
    if Some(o) == b {
        return t.clone();
    }
    if let (true, Value::Object(om), Value::Object(tm)) = (fields, o, t) {
        let bm = b.and_then(Value::as_object);
        let mut out = Map::new();
        let names: BTreeSet<&String> = om.keys().chain(tm.keys()).collect();
        for name in names {
            let (fb, fo, ft) = (bm.and_then(|m| m.get(name)), om.get(name), tm.get(name));
            let v = match (fo, ft) {
                (Some(fo), Some(ft)) => {
                    if fo != ft && Some(fo) != fb && Some(ft) != fb {
                        conflicts.push(format!(
                            "{}: {name} changed on {ME} and on another computer; kept {ME}'s",
                            label()
                        ));
                    }
                    Some(if Some(fo) == fb { ft } else { fo })
                }
                // a field one side dropped, unchanged on the other: dropped
                (Some(fo), None) => (Some(fo) != fb).then_some(fo),
                (None, Some(ft)) => (Some(ft) != fb).then_some(ft),
                (None, None) => None,
            };
            if let Some(v) = v {
                _ = out.insert(name.clone(), v.clone());
            }
        }
        return Value::Object(out);
    }
    conflicts.push(format!(
        "{}: changed on {ME} and on another computer; kept {ME}'s",
        label()
    ));
    o.clone()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn merges_entries_and_settles_conflicts_the_same_way() {
        let base = obj(json!({"a": 1, "b": 2, "c": 3, "d": {"x": 1, "y": 1}}));
        let ours = obj(json!({"a": 10, "b": 2, "d": {"x": 2, "y": 1}, "e": 5, "same": 1}));
        let theirs = obj(json!({"a": 11, "c": 3, "d": {"x": 1, "y": 2}, "f": 6, "same": 1}));
        let mut notes = Vec::new();
        let label = |k: &str| format!("entry {k}");
        let m = merge_map(Some(&base), &ours, &theirs, true, &label, &mut notes);
        // a: both changed, ours wins; b: theirs deleted, ours unchanged: gone;
        // c: ours deleted, theirs unchanged: gone; d: fields merge
        assert_eq!(
            Value::Object(m),
            json!({"a": 10, "d": {"x": 2, "y": 2}, "e": 5, "f": 6, "same": 1})
        );
        assert_eq!(notes.len(), 1);
        assert!(notes[0].starts_with("entry a"));

        // an edit beats a delete, either way round
        let mut notes = Vec::new();
        let m = merge_map(
            Some(&obj(json!({"k": 1}))),
            &obj(json!({"k": 2})),
            &obj(json!({})),
            false,
            &label,
            &mut notes,
        );
        assert_eq!(Value::Object(m), json!({"k": 2}));
        assert_eq!(notes.len(), 1);
    }

    #[test]
    fn a_backup_like_the_last_isnt_kept_twice() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let (backups, src) = (tmp.path().join("b"), tmp.path().join("Web Data"));
        fs::write(&src, "one")?;
        let rel = Path::new("chromium/Default/Web Data");
        back_up(&backups, rel, &src)?;
        std::thread::sleep(std::time::Duration::from_millis(1100));
        back_up(&backups, rel, &src)?;
        back_up_bytes(&backups, rel, b"one")?;
        assert_eq!(fs::read_dir(backups.join("history"))?.count(), 1);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(&src, "two")?;
        back_up(&backups, rel, &src)?;
        assert_eq!(fs::read_dir(backups.join("history"))?.count(), 2);
        Ok(())
    }
}
