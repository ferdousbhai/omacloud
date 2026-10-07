//! Chromium's bookmarks, search engines, settings and extensions, merged
//! across computers (a [`Group`] of merged documents).
//!
//! Each profile in `Local State` (`profile.info_cache`) syncs four
//! documents under `chromium/<profile>/`: `bookmarks.json`,
//! `search-engines.json`, `preferences.json` and `extensions.json`. The
//! `Default` profile is `Default` everywhere; others match by the name the
//! profile shows. History, passwords, cookies, open tabs, caches and
//! extensions' own data stay on each computer.
//!
//! Chromium is read at any time but written only while it's closed: a
//! profile's files belong to the running browser, which would overwrite
//! them (or, for `Web Data`, holds them locked). Closed means no
//! `SingletonLock` in the user data dir, or one left by a crash: Chromium's
//! lock is a symlink to `<hostname>-<pid>` (process_singleton_posix.cc), so a
//! lock naming this computer and a pid that no longer runs is stale. A lock
//! naming another computer (a shared home) counts as open.
//!
//! Only plain preferences sync, from [`PREFS`]. Preferences Chromium tracks
//! with a MAC (`protection.macs`, [`PROTECTED`]) are never written: on
//! Windows and macOS a changed one is reset, and the MACs would stop
//! matching anywhere.
//!
//! Brave or Chrome would be another entry in [`BROWSERS`] with their own
//! user data dir and external extensions directory.

pub mod bookmarks;
pub mod search;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::SystemTime,
};

use anyhow::{Context, Result, anyhow};
use serde_json::{Map, Value, json};

use crate::merged::{self, Concern, Group, Install, Item, Merged};

/// Browsers whose profiles sync: the group's name, and the user data dir
/// under home.
pub const BROWSERS: &[(&str, &str)] = &[("chromium", ".config/chromium")];

/// Plain preferences that sync: how Chromium looks and behaves, nothing
/// tied to this machine (no paths, window placement, zoom, which differs
/// by screen, device ids or account state) and nothing in [`PROTECTED`].
/// The theme follows Omarchy's, which sets it by policy. Each name is
/// Chromium's, from chrome/common/pref_names.h and the components'
/// pref_names files.
pub const PREFS: &[&str] = &[
    "bookmark_bar.show_on_all_tabs",
    "bookmark_bar.show_apps_shortcut",
    "bookmark_bar.show_tab_groups",
    "browser.show_forward_button",
    "browser.custom_chrome_frame",
    "side_panel.is_right_aligned",
    "intl.accept_languages",
    "intl.selected_languages",
    "browser.enable_spellchecking",
    "spellcheck.dictionaries",
    "spellcheck.dictionary",
    "spellcheck.use_spelling_service",
    "translate.enabled",
    "translate_blocked_languages",
    "download.prompt_for_download",
    "webkit.webprefs.default_font_size",
    "webkit.webprefs.default_fixed_font_size",
    "webkit.webprefs.minimum_font_size",
    "webkit.webprefs.minimum_logical_font_size",
    "search.suggest_enabled",
    "enable_do_not_track",
    "safebrowsing.enabled",
    "safebrowsing.enhanced",
    "profile.cookie_controls_mode",
    "credentials_enable_service",
    "autofill.profile_enabled",
    "autofill.credit_card_enabled",
    // site permissions' defaults (website_settings_info.cc names them
    // `profile.default_content_setting_values.` + the type, `-` as `_`);
    // the exceptions for each site stay, they name sites visited
    "profile.default_content_setting_values.cookies",
    "profile.default_content_setting_values.images",
    "profile.default_content_setting_values.javascript",
    "profile.default_content_setting_values.popups",
    "profile.default_content_setting_values.geolocation",
    "profile.default_content_setting_values.notifications",
    "profile.default_content_setting_values.media_stream_mic",
    "profile.default_content_setting_values.media_stream_camera",
    "profile.default_content_setting_values.automatic_downloads",
    "profile.default_content_setting_values.sound",
    "profile.default_content_setting_values.clipboard",
];

/// Preferences Chromium tracks with a MAC: `kTrackedPrefs` in
/// chrome/browser/prefs/chrome_pref_service_factory.cc, each also under
/// `account_values.` (`kAccountPreferencesPrefix`). Never written.
pub const PROTECTED: &[&str] = &[
    "browser.show_home_button",
    "homepage_is_newtabpage",
    "homepage",
    "session.restore_on_startup",
    "session.startup_urls",
    "extensions.settings",
    "google.services.last_username",
    "search_provider_overrides",
    "pinned_tabs",
    "default_search_provider_data.template_url_data",
    "prefs.preference_reset_time",
    "safebrowsing.incidents_sent",
    "google.services.account_id",
    "media.storage_id_salt",
    "media.cdm.origin_data",
    "google.services.last_signed_in_username",
    "enterprise_signin.policy_recovery_token",
    "extensions.ui.developer_mode",
    "schedule_to_flush_to_disk",
    "extensions.install.initiallist",
    "extensions.install.initialprovidername",
    // the MACs themselves
    "protection",
    "account_values",
];

/// The Chrome Web Store's update URL (extensions/common/extension_urls.cc),
/// which an external extension file points Chromium at.
const WEBSTORE_UPDATE: &str = "https://clients2.google.com/service/update2/crx";

const DOCS: [&str; 4] = [
    "bookmarks.json",
    "search-engines.json",
    "preferences.json",
    "extensions.json",
];

/// A Chromium-family browser's profiles, as a [`Group`].
#[derive(Debug, Clone)]
pub struct Chromium {
    name: String,
    /// The user data dir, like `~/.config/chromium`.
    data: PathBuf,
    /// Where a copy of `Web Data` is read.
    scratch: PathBuf,
    /// The command that opens this browser, for install links.
    command: String,
    /// What was read before, while the files it came from are unchanged.
    cache: Arc<Mutex<Cache>>,
}

/// A file's size and modification time: a different one is a new file.
type Stamp = Option<(u64, SystemTime)>;

fn stamp(path: &Path) -> Stamp {
    fs::metadata(path)
        .ok()
        .and_then(|m| Some((m.len(), m.modified().ok()?)))
}

#[derive(Debug, Default)]
struct Cache {
    profiles: Option<(Stamp, Vec<(String, String)>)>,
    docs: BTreeMap<String, (Vec<Stamp>, Option<Value>)>,
}

impl Chromium {
    #[must_use]
    pub fn new(name: &str, data: PathBuf, scratch: PathBuf) -> Self {
        Self {
            name: name.to_string(),
            data,
            scratch,
            command: name.to_string(),
            cache: Arc::default(),
        }
    }

    /// The browsers set up under `home`.
    #[must_use]
    pub fn for_home(home: &Path, scratch: &Path) -> Vec<Self> {
        BROWSERS
            .iter()
            .filter(|(_, dir)| home.join(dir).is_dir())
            .map(|(name, dir)| Self::new(name, home.join(dir), scratch.join(name)))
            .collect()
    }

    /// Whether the browser runs on this user data dir (see the module doc).
    #[must_use]
    pub fn running(&self) -> bool {
        let lock = self.data.join("SingletonLock");
        let Ok(target) = fs::read_link(&lock) else {
            // not there: closed; there but not a link: someone's, say open
            return fs::symlink_metadata(&lock).is_ok();
        };
        let target = target.to_string_lossy();
        let Some((host, pid)) = target.rsplit_once('-') else {
            return true;
        };
        let Ok(pid) = pid.parse::<u32>() else {
            return true;
        };
        if host != hostname() {
            return true;
        }
        // a pid in use again by something else reads as open: safe
        Path::new("/proc").join(pid.to_string()).exists()
    }

    /// Profiles: the name they sync under, and their directory.
    fn profiles(&self) -> Vec<(String, String)> {
        let now = stamp(&self.data.join("Local State"));
        let mut cache = self.cache.lock().expect("not poisoned");
        if let Some((_, p)) = cache.profiles.as_ref().filter(|(s, _)| *s == now) {
            return p.clone();
        }
        let p = self.read_profiles();
        cache.profiles = Some((now, p.clone()));
        p
    }

    fn read_profiles(&self) -> Vec<(String, String)> {
        let mut out = vec![("Default".to_string(), "Default".to_string())];
        let state: Option<Value> = fs::read(self.data.join("Local State"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
        let cache = state
            .as_ref()
            .and_then(|s| s.pointer("/profile/info_cache"))
            .and_then(Value::as_object);
        let mut seen: BTreeSet<String> = BTreeSet::from(["Default".to_string()]);
        for (dir, info) in cache.into_iter().flatten() {
            if dir == "Default" || dir.contains('/') {
                continue;
            }
            let name: String = info
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(dir)
                .chars()
                .map(|c| if c == '/' || c.is_control() { '-' } else { c })
                .collect();
            let name = name.trim().to_string();
            // two profiles of one name can't be told apart elsewhere
            if name.is_empty() || name.starts_with('.') || !seen.insert(name.clone()) {
                continue;
            }
            out.push((name, dir.clone()));
        }
        out.retain(|(_, dir)| self.data.join(dir).is_dir());
        out
    }

    /// A document's profile directory and kind.
    fn locate(&self, doc: &str) -> Option<(PathBuf, String, &'static str)> {
        let (profile, kind) = doc.split_once('/')?;
        let kind = DOCS.iter().find(|d| **d == kind)?;
        let (_, dir) = self.profiles().into_iter().find(|(n, _)| n == profile)?;
        Some((self.data.join(&dir), dir, kind))
    }

    /// A document from the profile's files.
    fn read_doc(&self, profile: &Path, dir: &str, kind: &str) -> Result<Option<Value>> {
        let json = |file: &str| -> Result<Option<Value>> {
            match fs::read(profile.join(file)) {
                // mid-write or damaged: look again next time
                Ok(b) => Ok(serde_json::from_slice(&b).ok()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            }
        };
        Ok(match kind {
            "bookmarks.json" => json("Bookmarks")?
                .map(|f| serde_json::to_value(bookmarks::capture(&f)))
                .transpose()?,
            "search-engines.json" => {
                search::capture(&profile.join("Web Data"), &self.scratch.join(dir))?
            }
            "preferences.json" => json("Preferences")?.map(|p| capture_preferences(&p)),
            _ => json("Preferences")?.map(|p| capture_extensions(&p)),
        })
    }

    fn preferences(profile: &Path) -> Option<Value> {
        serde_json::from_slice(&fs::read(profile.join("Preferences")).ok()?).ok()
    }

    /// The extension ids in a document.
    fn ids(v: Option<&Value>) -> BTreeSet<String> {
        v.and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Extensions in `mirror` this profile doesn't have, wasn't given to
    /// install, and wasn't told to forget (uninstalled here).
    fn missing(&self, profile: &Path, mirror: &Value) -> Vec<String> {
        let prefs = Self::preferences(profile);
        let here: BTreeSet<String> = prefs
            .as_ref()
            .and_then(|p| p.pointer("/extensions/settings"))
            .and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        let removed: BTreeSet<String> = prefs
            .as_ref()
            .and_then(|p| p.pointer("/extensions/external_uninstalls"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        Self::ids(Some(mirror))
            .into_iter()
            .filter(|id| valid_id(id) && !here.contains(id) && !removed.contains(id))
            .collect()
    }

    /// Where an extension file for `id` goes: Chromium's per-user external
    /// extensions directory, `<user data dir>/External Extensions`
    /// (`DIR_USER_EXTERNAL_EXTENSIONS` in chrome/common/chrome_paths.cc,
    /// loaded on Linux in Chromium builds by external_provider_impl.cc). It
    /// reaches every profile of this user data dir.
    fn external(&self, id: &str) -> PathBuf {
        self.data
            .join("External Extensions")
            .join(format!("{id}.json"))
    }
}

/// Extension ids are 32 letters a to p.
fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| (b'a'..=b'p').contains(&b))
}

fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most buf.len() bytes into buf
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return String::new();
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// A preference by its dotted name: Chromium nests `a.b.c` as objects.
fn pref<'a>(prefs: &'a Value, name: &str) -> Option<&'a Value> {
    name.split('.').try_fold(prefs, |v, k| v.get(k))
}

fn set_pref(prefs: &mut Value, name: &str, value: Option<&Value>) {
    let parts: Vec<&str> = name.split('.').collect();
    let (last, path) = parts.split_last().expect("names have a part");
    let mut at = prefs;
    for p in path {
        if !at.is_object() {
            return;
        }
        if value.is_none() && at.get(*p).is_none() {
            return;
        }
        at = at
            .as_object_mut()
            .expect("checked")
            .entry((*p).to_string())
            .or_insert_with(|| json!({}));
    }
    let Some(m) = at.as_object_mut() else { return };
    match value {
        Some(v) => _ = m.insert((*last).to_string(), v.clone()),
        None => _ = m.remove(*last),
    }
}

/// The synced preferences in a `Preferences` file.
#[must_use]
pub fn capture_preferences(prefs: &Value) -> Value {
    let mut out = Map::new();
    for name in PREFS {
        if let Some(v) = pref(prefs, name) {
            _ = out.insert((*name).to_string(), v.clone());
        }
    }
    Value::Object(out)
}

/// `prefs` with the document's preferences in it, a missing one back at
/// Chromium's default. Only [`PREFS`] change.
#[must_use]
pub fn write_preferences(prefs: &Value, doc: &Value) -> Value {
    let mut out = prefs.clone();
    for name in PREFS {
        set_pref(&mut out, name, doc.get(*name));
    }
    out
}

/// The Web Store extensions a profile has (`extensions.settings`, read
/// only): installed from the store (`location` 1, `kInternal`, with
/// `from_webstore`) or through an external file (6,
/// `kExternalPrefDownload`; extensions/common/mojom/manifest.mojom). Not
/// Chromium's own, policy ones, or unpacked ones like Omarchy's.
#[must_use]
pub fn capture_extensions(prefs: &Value) -> Value {
    let mut out = Map::new();
    let settings = prefs
        .pointer("/extensions/settings")
        .and_then(Value::as_object);
    for (id, e) in settings.into_iter().flatten() {
        let location = e.get("location").and_then(Value::as_i64);
        let webstore = e.get("from_webstore").and_then(Value::as_bool) == Some(true);
        let by_default = e.get("was_installed_by_default").and_then(Value::as_bool) == Some(true)
            || e.get("was_installed_by_oem").and_then(Value::as_bool) == Some(true);
        let store = matches!((location, webstore), (Some(1), true) | (Some(6), _));
        if !store || by_default || !valid_id(id) {
            continue;
        }
        let name = e
            .pointer("/manifest/name")
            .and_then(Value::as_str)
            .filter(|n| !n.starts_with("__MSG_"))
            .unwrap_or(id);
        _ = out.insert(id.clone(), json!({ "name": name }));
    }
    Value::Object(out)
}

/// The default search engine's guid in a `Preferences` file.
fn default_engine(prefs: Option<&Value>) -> Option<String> {
    prefs?
        .pointer("/default_search_provider_data/template_url_data/synced_guid")
        .and_then(Value::as_str)
        .map(str::to_string)
}

impl Group for Chromium {
    fn name(&self) -> &str {
        &self.name
    }

    fn docs(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (name, dir) in self.profiles() {
            let p = self.data.join(&dir);
            for (doc, file) in
                DOCS.iter()
                    .zip(["Bookmarks", "Web Data", "Preferences", "Preferences"])
            {
                if p.join(file).is_file() {
                    out.push(format!("{name}/{doc}"));
                }
            }
        }
        out
    }

    fn capture(&self, doc: &str, _applied: Option<&Value>) -> Result<Option<Value>> {
        let Some((profile, dir, kind)) = self.locate(doc) else {
            return Ok(None);
        };
        // read again only when its files changed: `Preferences` is
        // rewritten often, and `Web Data` is copied to be read
        let files: &[&str] = match kind {
            "bookmarks.json" => &["Bookmarks"],
            "search-engines.json" => &["Web Data", "Web Data-journal", "Web Data-wal"],
            _ => &["Preferences"],
        };
        let stamps: Vec<Stamp> = files.iter().map(|f| stamp(&profile.join(f))).collect();
        if let Some((_, v)) = self
            .cache
            .lock()
            .expect("not poisoned")
            .docs
            .get(doc)
            .filter(|(s, _)| *s == stamps)
        {
            return Ok(v.clone());
        }
        let v = self.read_doc(&profile, &dir, kind)?;
        _ = self
            .cache
            .lock()
            .expect("not poisoned")
            .docs
            .insert(doc.to_string(), (stamps, v.clone()));
        Ok(v)
    }

    fn forget(&self) {
        *self.cache.lock().expect("not poisoned") = Cache::default();
    }

    fn busy(&self, _doc: &str) -> Option<String> {
        self.running().then(|| "Chromium is open".to_string())
    }

    fn pending(&self, doc: &str, mirror: &Value, applied: Option<&Value>) -> bool {
        match self.locate(doc) {
            // extensions are only ever added; one profile installs them
            Some((profile, dir, "extensions.json")) => {
                dir == "Default"
                    && self
                        .missing(&profile, mirror)
                        .iter()
                        .any(|id| !self.external(id).exists())
            }
            Some(_) => Some(mirror) != applied,
            None => false,
        }
    }

    fn apply(
        &self,
        doc: &str,
        value: &Value,
        _applied: Option<&Value>,
        backups: &Path,
    ) -> Result<()> {
        let (profile, dir, kind) = self
            .locate(doc)
            .ok_or_else(|| anyhow!("no Chromium profile for {doc}"))?;
        // checked again as late as can be
        if self.running() {
            anyhow::bail!("Chromium is open");
        }
        let rel = |file: &str| Path::new(&self.name).join(&dir).join(file);
        match kind {
            "bookmarks.json" => {
                let doc: bookmarks::Doc = serde_json::from_value(value.clone())?;
                let path = profile.join("Bookmarks");
                let old: Option<Value> = fs::read(&path)
                    .ok()
                    .and_then(|b| serde_json::from_slice(&b).ok());
                let new = bookmarks::write(old.as_ref(), &doc);
                if old.as_ref() == Some(&new) {
                    return Ok(()); // nothing to change: nothing written
                }
                merged::back_up(backups, &rel("Bookmarks"), &path)?;
                merged::write_atomic(&path, &serde_json::to_vec_pretty(&new)?, 0o600)?;
                // Chromium prefers an encrypted copy where it's told to, and
                // falls back to `Bookmarks` without one (model_loader.cc)
                let encrypted = profile.join("EncryptedBookmarks2");
                if encrypted.exists() {
                    merged::back_up(backups, &rel("EncryptedBookmarks2"), &encrypted)?;
                    fs::remove_file(&encrypted)?;
                }
            }
            "search-engines.json" => {
                let path = profile.join("Web Data");
                let keep = default_engine(Self::preferences(&profile).as_ref());
                // the default here stays whatever the document says, so
                // what's there may already be all it would become
                let now = search::read(&path).ok();
                let mut target = value.clone();
                if let (Some(k), Some(e), Some(t)) = (
                    keep.as_ref(),
                    now.as_ref()
                        .and_then(|n| keep.as_ref().and_then(|k| n.get(k))),
                    target.as_object_mut(),
                ) {
                    _ = t.entry(k.clone()).or_insert_with(|| e.clone());
                }
                if now.as_ref() == Some(&target) {
                    return Ok(());
                }
                merged::back_up(backups, &rel("Web Data"), &path)?;
                search::write(&path, value, keep.as_deref())
                    .with_context(|| format!("writing {}", path.display()))?;
            }
            "preferences.json" => {
                let path = profile.join("Preferences");
                let old = Self::preferences(&profile)
                    .ok_or_else(|| anyhow!("{} isn't readable", path.display()))?;
                let new = write_preferences(&old, value);
                if new != old {
                    merged::back_up(backups, &rel("Preferences"), &path)?;
                    merged::write_atomic(&path, &serde_json::to_vec(&new)?, 0o600)?;
                }
            }
            _ => {
                if dir != "Default" {
                    return Ok(()); // listed to install by hand
                }
                for id in self.missing(&profile, value) {
                    let file = self.external(&id);
                    if !file.exists() {
                        let body = json!({ "external_update_url": WEBSTORE_UPDATE });
                        merged::write_atomic(&file, &serde_json::to_vec_pretty(&body)?, 0o644)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn merge(&self, doc: &str, base: Option<&Value>, ours: &Value, theirs: &Value) -> Merged {
        let kind = doc.rsplit('/').next().unwrap_or_default();
        if kind == "bookmarks.json" {
            return bookmarks::merge_values(base, ours, theirs);
        }
        let empty = Map::new();
        let map = |v: &Value| v.as_object().cloned().unwrap_or_default();
        let (o, mut t) = (map(ours), map(theirs));
        let b = base.and_then(Value::as_object);
        if kind == "search-engines.json" && b.is_none_or(Map::is_empty) {
            // two computers' first sync: an engine both made, by keyword,
            // counts once (under this computer's guid)
            let keyword = |e: &Value| e.get("keyword").cloned();
            let mine: Vec<(String, Option<Value>)> = o
                .iter()
                .filter(|(g, _)| !t.contains_key(*g))
                .map(|(g, e)| (g.clone(), keyword(e)))
                .collect();
            for (g, e) in t.clone() {
                if o.contains_key(&g) {
                    continue;
                }
                if let Some((mg, _)) = mine.iter().find(|(_, k)| k.is_some() && *k == keyword(&e)) {
                    _ = t.remove(&g);
                    _ = t.insert(mg.clone(), e);
                }
            }
        }
        let mut conflicts = Vec::new();
        let label = |k: &str| match kind {
            "search-engines.json" => {
                let name = o
                    .get(k)
                    .or(t.get(k))
                    .and_then(|e| e.get("keyword"))
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                format!("search engine {name}")
            }
            "extensions.json" => format!("extension {k}"),
            _ => format!("Chromium setting {k}"),
        };
        let fields = kind != "preferences.json";
        let value = merged::merge_map(
            b.or(Some(&empty)).filter(|m| !m.is_empty()),
            &o,
            &t,
            fields,
            &label,
            &mut conflicts,
        );
        Merged {
            value: Value::Object(value),
            conflicts,
        }
    }

    fn status(&self, mirror: &Path, waiting: &[(String, String)]) -> Vec<Item> {
        let mut out = Vec::new();
        for (name, dir) in self.profiles() {
            let profile = self.data.join(&dir);
            let prefix = format!("{name}/");
            let held: Vec<&str> = waiting
                .iter()
                .filter(|(d, _)| d.starts_with(&prefix))
                .filter_map(|(d, _)| d.strip_prefix(&prefix))
                .map(|d| match d {
                    "bookmarks.json" => "bookmarks",
                    "search-engines.json" => "search engines",
                    "preferences.json" => "settings",
                    _ => "extensions",
                })
                .collect();
            let ext = merged::read(&mirror.join(&name).join("extensions.json"));
            let names = ext.as_ref().and_then(Value::as_object);
            let missing = ext
                .as_ref()
                .map(|m| self.missing(&profile, m))
                .unwrap_or_default();
            let (queued, by_hand): (Vec<String>, Vec<String>) = missing
                .into_iter()
                .partition(|id| dir == "Default" && self.external(id).exists());
            let install = if dir == "Default" {
                Vec::new()
            } else {
                by_hand
                    .iter()
                    .map(|id| {
                        let url = format!("https://chromewebstore.google.com/detail/{id}");
                        Install {
                            id: id.clone(),
                            name: names
                                .and_then(|m| m.get(id))
                                .and_then(|e| e.get("name"))
                                .and_then(Value::as_str)
                                .unwrap_or(id)
                                .to_string(),
                            command: vec![
                                self.command.clone(),
                                format!("--profile-directory={dir}"),
                                url.clone(),
                            ],
                            url,
                        }
                    })
                    .collect()
            };
            let (state, mut detail) = if !held.is_empty() {
                (
                    "waiting",
                    format!(
                        "Changes to {} from another computer are applied once Chromium is closed",
                        held.join(", ")
                    ),
                )
            } else if !install.is_empty() {
                (
                    "install",
                    "Extensions from your other computers to add to this profile".to_string(),
                )
            } else {
                (
                    "synced",
                    "Bookmarks, search engines, settings and extensions".to_string(),
                )
            };
            if !queued.is_empty() {
                detail.push_str(&format!(
                    "; {} extension{} from your other computers install{} when Chromium starts",
                    queued.len(),
                    if queued.len() == 1 { "" } else { "s" },
                    if queued.len() == 1 { "s" } else { "" },
                ));
            }
            let mut not_synced = Vec::new();
            if profile.join("EncryptedBookmarks2").exists() && !profile.join("Bookmarks").exists() {
                not_synced.push("Bookmarks: Chromium keeps them only encrypted here".into());
            }
            out.push(Item {
                group: self.name.clone(),
                title: format!("Chromium: {name}"),
                state: state.into(),
                detail,
                install,
                not_synced,
                scope: format!("{}/{name}/", self.name),
            });
        }
        out
    }

    fn watch_dirs(&self) -> Vec<PathBuf> {
        let mut dirs = vec![self.data.clone()];
        dirs.extend(self.profiles().into_iter().map(|(_, d)| self.data.join(d)));
        dirs
    }

    fn concerns(&self, path: &Path) -> Concern {
        let Ok(rel) = path.strip_prefix(&self.data) else {
            return Concern::No;
        };
        match rel.file_name().and_then(|n| n.to_str()).unwrap_or_default() {
            // closed (or opened): what waits may go in now
            "SingletonLock" => Concern::Now,
            "Local State" | "Bookmarks" | "Preferences" | "Web Data" => Concern::Look,
            _ => Concern::No,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/chromium");

    /// A Chromium user data dir from the fixtures: `Default` with
    /// bookmarks, preferences and search engines, and a second profile.
    pub fn fixture(data: &Path) -> Chromium {
        let f = Path::new(FIXTURES);
        let default = data.join("Default");
        fs::create_dir_all(&default).unwrap();
        fs::create_dir_all(data.join("Profile 1")).unwrap();
        fs::copy(f.join("Local State"), data.join("Local State")).unwrap();
        fs::copy(f.join("Bookmarks"), default.join("Bookmarks")).unwrap();
        fs::copy(f.join("Preferences"), default.join("Preferences")).unwrap();
        fs::copy(f.join("Preferences"), data.join("Profile 1/Preferences")).unwrap();
        let db = rusqlite::Connection::open(default.join("Web Data")).unwrap();
        db.execute_batch(&fs::read_to_string(f.join("keywords.sql")).unwrap())
            .unwrap();
        Chromium::new("chromium", data.to_path_buf(), data.join("../scratch"))
    }

    fn read_json(p: &Path) -> Value {
        serde_json::from_slice(&fs::read(p).unwrap()).unwrap()
    }

    #[test]
    fn bookmarks_round_trip_and_keep_chromiums_checksum() {
        let file = read_json(&Path::new(FIXTURES).join("Bookmarks"));
        let doc = bookmarks::capture(&file);
        assert_eq!(doc.len(), 3 + 5);
        assert_eq!(
            doc["6d2f1b0e-1111-4a5b-9c3d-000000000002"].children,
            [
                "6d2f1b0e-1111-4a5b-9c3d-000000000003",
                "6d2f1b0e-1111-4a5b-9c3d-000000000004"
            ]
        );
        // the checksum Chromium wrote, computed the same way
        assert_eq!(
            bookmarks::checksums(&file).0,
            file["checksum"].as_str().unwrap()
        );
        // written back unchanged: the same file
        assert_eq!(bookmarks::write(Some(&file), &doc), file);

        // a new bookmark: a fresh id, the fields Chromium writes, a new checksum
        let mut doc2 = doc.clone();
        let g = "6d2f1b0e-1111-4a5b-9c3d-0000000000aa".to_string();
        doc2.get_mut("other").unwrap().children.insert(0, g.clone());
        _ = doc2.insert(
            g.clone(),
            bookmarks::Node {
                parent: Some("other".into()),
                kind: "url".into(),
                name: "Example added".into(),
                url: Some("https://added.example.com/".into()),
                date_added: Some("13400000900000000".into()),
                children: Vec::new(),
            },
        );
        let out = bookmarks::write(Some(&file), &doc2);
        let added = &out["roots"]["other"]["children"][0];
        assert_eq!(added["id"], "9");
        assert_eq!(added["guid"], json!(g));
        assert_eq!(added["date_last_used"], "0");
        assert_ne!(out["checksum"], file["checksum"]);
        assert_eq!(
            bookmarks::checksums(&out).0,
            out["checksum"].as_str().unwrap()
        );
        // what the document doesn't model stays
        let first = &out["roots"]["bookmark_bar"]["children"][0];
        assert_eq!(first["meta_info"], json!({"power_bookmark_meta": ""}));
        assert_eq!(first["date_last_used"], "13400000500000000");
        assert_eq!(bookmarks::capture(&out), doc2);
    }

    fn url(parent: &str, name: &str) -> bookmarks::Node {
        bookmarks::Node {
            parent: Some(parent.into()),
            kind: "url".into(),
            name: name.into(),
            url: Some(format!("https://{name}.example.com/")),
            ..Default::default()
        }
    }

    fn folder(parent: &str, name: &str, children: &[&str]) -> bookmarks::Node {
        bookmarks::Node {
            parent: Some(parent.into()),
            kind: "folder".into(),
            name: name.into(),
            children: children.iter().map(|c| (*c).to_string()).collect(),
            ..Default::default()
        }
    }

    /// bar: a, f(x, y), b; other: empty.
    fn base() -> bookmarks::Doc {
        let mut d = bookmarks::Doc::new();
        for (r, children) in [
            ("bookmark_bar", vec!["a", "f", "b"]),
            ("other", vec![]),
            ("synced", vec![]),
        ] {
            _ = d.insert(
                r.into(),
                bookmarks::Node {
                    kind: "folder".into(),
                    children: children.into_iter().map(str::to_string).collect(),
                    ..Default::default()
                },
            );
        }
        _ = d.insert("a".into(), url("bookmark_bar", "a"));
        _ = d.insert("b".into(), url("bookmark_bar", "b"));
        _ = d.insert("f".into(), folder("bookmark_bar", "f", &["x", "y"]));
        _ = d.insert("x".into(), url("f", "x"));
        _ = d.insert("y".into(), url("f", "y"));
        d
    }

    fn kids(d: &bookmarks::Doc, id: &str) -> Vec<String> {
        d[id].children.clone()
    }

    #[test]
    fn concurrent_adds_both_land_in_place() {
        let b = base();
        let (mut o, mut t) = (b.clone(), b.clone());
        _ = o.insert("o1".into(), url("f", "o1"));
        o.get_mut("f").unwrap().children.insert(1, "o1".into()); // x o1 y
        _ = t.insert("t1".into(), url("f", "t1"));
        t.get_mut("f").unwrap().children.push("t1".into()); // x y t1
        let (m, c) = bookmarks::merge(Some(&b), &o, &t);
        assert_eq!(kids(&m, "f"), ["x", "o1", "y", "t1"]);
        assert!(c.is_empty());
    }

    #[test]
    fn an_edit_beats_a_delete_and_a_deleted_folder_comes_back_for_its_new_child() {
        let b = base();
        // ours deletes a; theirs renames it
        let (mut o, mut t) = (b.clone(), b.clone());
        _ = o.remove("a");
        o.get_mut("bookmark_bar")
            .unwrap()
            .children
            .retain(|c| c != "a");
        t.get_mut("a").unwrap().name = "a renamed".into();
        let (m, c) = bookmarks::merge(Some(&b), &o, &t);
        assert_eq!(m["a"].name, "a renamed");
        assert_eq!(kids(&m, "bookmark_bar"), ["a", "f", "b"]);
        assert_eq!(c.len(), 1);

        // a plain delete on one side goes through
        let (m, c) = bookmarks::merge(Some(&b), &o, &b);
        assert!(!m.contains_key("a"));
        assert!(c.is_empty());

        // theirs deletes folder f; ours adds into it: f stays, with all
        let (mut o, mut t) = (b.clone(), b.clone());
        _ = o.insert("n".into(), url("f", "n"));
        o.get_mut("f").unwrap().children.push("n".into());
        for id in ["f", "x", "y"] {
            _ = t.remove(id);
        }
        t.get_mut("bookmark_bar")
            .unwrap()
            .children
            .retain(|c| c != "f");
        let (m, c) = bookmarks::merge(Some(&b), &o, &t);
        assert!(m.contains_key("f") && m.contains_key("n"));
        // x and y were deleted with f there, and unchanged here: deleted
        assert_eq!(kids(&m, "f"), ["n"]);
        assert!(kids(&m, "bookmark_bar").contains(&"f".to_string()));
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn a_move_and_a_rename_both_land_and_a_clash_keeps_ours() {
        let b = base();
        let (mut o, mut t) = (b.clone(), b.clone());
        // ours moves x to other; theirs renames x
        o.get_mut("x").unwrap().parent = Some("other".into());
        o.get_mut("f").unwrap().children.retain(|c| c != "x");
        o.get_mut("other").unwrap().children.push("x".into());
        t.get_mut("x").unwrap().name = "x renamed".into();
        let (m, c) = bookmarks::merge(Some(&b), &o, &t);
        assert_eq!(m["x"].parent.as_deref(), Some("other"));
        assert_eq!(m["x"].name, "x renamed");
        assert_eq!(kids(&m, "other"), ["x"]);
        assert_eq!(kids(&m, "f"), ["y"]);
        assert!(c.is_empty());

        // both rename y: ours wins, and it's recorded
        let (mut o, mut t) = (b.clone(), b.clone());
        o.get_mut("y").unwrap().name = "mine".into();
        t.get_mut("y").unwrap().name = "theirs".into();
        let (m, c) = bookmarks::merge(Some(&b), &o, &t);
        assert_eq!(m["y"].name, "mine");
        assert_eq!(c.len(), 1);
        assert!(c[0].contains("name"));
    }

    #[test]
    fn a_reorder_keeps_the_other_sides_additions() {
        let b = base();
        let (mut o, mut t) = (b.clone(), b.clone());
        t.get_mut("bookmark_bar").unwrap().children = vec!["b".into(), "f".into(), "a".into()];
        _ = o.insert("o1".into(), url("bookmark_bar", "o1"));
        o.get_mut("bookmark_bar").unwrap().children =
            vec!["a".into(), "o1".into(), "f".into(), "b".into()];
        let (m, _) = bookmarks::merge(Some(&b), &o, &t);
        assert_eq!(kids(&m, "bookmark_bar"), ["b", "f", "a", "o1"]);
    }

    #[test]
    fn crossed_moves_keep_ours_and_a_first_sync_counts_shared_bookmarks_once() {
        let mut b = base();
        _ = b.insert("g".into(), folder("bookmark_bar", "g", &[]));
        b.get_mut("bookmark_bar").unwrap().children.push("g".into());
        let (mut o, mut t) = (b.clone(), b.clone());
        // ours: f into g; theirs: g into f
        o.get_mut("f").unwrap().parent = Some("g".into());
        t.get_mut("g").unwrap().parent = Some("f".into());
        let (m, c) = bookmarks::merge(Some(&b), &o, &t);
        assert_eq!(m["f"].parent.as_deref(), Some("g"));
        assert_eq!(m["g"].parent.as_deref(), Some("bookmark_bar"));
        assert!(!c.is_empty());

        // no base: the same bookmark made on both computers counts once
        let mut t = base();
        let a = t.remove("a").unwrap();
        _ = t.insert("a-there".into(), a);
        t.get_mut("bookmark_bar").unwrap().children[0] = "a-there".into();
        _ = t.insert("t1".into(), url("other", "t1"));
        t.get_mut("other").unwrap().children.push("t1".into());
        let (m, _) = bookmarks::merge(None, &base(), &t);
        assert!(m.contains_key("a") && !m.contains_key("a-there"));
        assert_eq!(kids(&m, "other"), ["t1"]);
    }

    #[test]
    fn protected_preferences_are_never_written() {
        // no synced preference is, or is inside or around, a protected one
        for p in PREFS {
            for q in PROTECTED {
                let inside = |a: &str, b: &str| a == b || a.starts_with(&format!("{b}."));
                assert!(!inside(p, q) && !inside(q, p), "{p} touches protected {q}");
            }
        }
        let prefs = read_json(&Path::new(FIXTURES).join("Preferences"));
        // a document naming protected preferences (from a bad or hostile
        // copy) still changes only the synced ones
        let mut doc = capture_preferences(&prefs).as_object().unwrap().clone();
        for q in PROTECTED {
            _ = doc.insert((*q).to_string(), json!("changed"));
        }
        _ = doc.insert("webkit.webprefs.default_font_size".into(), json!(20));
        _ = doc.remove("bookmark_bar.show_on_all_tabs");
        let out = write_preferences(&prefs, &Value::Object(doc));
        for q in PROTECTED {
            assert_eq!(pref(&out, q), pref(&prefs, q), "{q} changed");
        }
        assert_eq!(out["webkit"]["webprefs"]["default_font_size"], 20);
        assert!(out["bookmark_bar"].get("show_on_all_tabs").is_none());
        // and nothing tied to this machine is captured
        let captured = capture_preferences(&prefs);
        assert!(captured.get("download.default_directory").is_none());
        assert_eq!(captured["intl.accept_languages"], "en-US,en,de");
    }

    #[test]
    fn search_engines_merge_and_write_back() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let c = fixture(&tmp.path().join("chromium"));
        let web = tmp.path().join("chromium/Default/Web Data");
        let doc = search::capture(&web, &tmp.path().join("scratch"))?.unwrap();
        // only the user's own engines: not built in, starter pack or auto added
        let keywords: Vec<&str> = doc
            .as_object()
            .unwrap()
            .values()
            .map(|e| e["keyword"].as_str().unwrap())
            .collect();
        assert_eq!(keywords, ["w", "m"]);

        // theirs edits w and adds q; ours deletes m
        let mut theirs = doc.clone();
        theirs["aaaaaaaa-0000-4000-8000-000000000004"]["short_name"] = json!("Wiki (edited)");
        theirs["new-guid"] = json!({"keyword": "q", "short_name": "Q", "url": "https://q.example.com/?q={searchTerms}"});
        let mut ours = doc.clone();
        _ = ours
            .as_object_mut()
            .unwrap()
            .remove("aaaaaaaa-0000-4000-8000-000000000005");
        let m = c.merge("Default/search-engines.json", Some(&doc), &ours, &theirs);
        assert!(m.conflicts.is_empty());
        search::write(&web, &m.value, None)?;
        let back = search::read(&web)?;
        assert_eq!(back, m.value);
        assert_eq!(
            back["aaaaaaaa-0000-4000-8000-000000000004"]["short_name"],
            "Wiki (edited)"
        );
        // Chromium's own rows are untouched
        let db = rusqlite::Connection::open(&web)?;
        let n: i64 = db.query_row("SELECT COUNT(*) FROM keywords WHERE prepopulate_id = 1 OR starter_pack_id = 1 OR safe_for_autoreplace = 1", [], |r| r.get(0))?;
        assert_eq!(n, 3);
        drop(db);
        // the default engine here is never deleted
        search::write(
            &web,
            &json!({}),
            Some("aaaaaaaa-0000-4000-8000-000000000004"),
        )?;
        assert_eq!(search::read(&web)?.as_object().unwrap().len(), 1);

        // first sync of two computers: the same keyword counts once
        let mine = json!({"g1": {"keyword": "w", "url": "https://w.example.com/?q={searchTerms}"}});
        let there =
            json!({"g2": {"keyword": "w", "url": "https://w.example.com/?q={searchTerms}"}});
        let m = c.merge("Default/search-engines.json", None, &mine, &there);
        assert_eq!(m.value, mine);
        Ok(())
    }

    #[test]
    fn web_store_extensions_spread_through_the_external_extensions_directory() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let data = tmp.path().join("chromium");
        let c = fixture(&data);
        let prefs = read_json(&data.join("Default/Preferences"));
        let have = capture_extensions(&prefs);
        // the store's and an external one, not Chromium's own or unpacked
        assert_eq!(
            have.as_object().unwrap().keys().collect::<Vec<_>>(),
            [
                "cjpalhdlnbpafiamejdnhcphjbkeiagm",
                "nngceckbapebfimnlniiiahkandclblb"
            ]
        );
        let mut want = have.clone();
        want["gighmmpiobklfepjocnamgkkbiglidom"] = json!({"name": "Example Tool"});
        assert!(c.pending("Default/extensions.json", &want, Some(&have)));
        c.apply("Default/extensions.json", &want, Some(&have), tmp.path())?;
        let file = data.join("External Extensions/gighmmpiobklfepjocnamgkkbiglidom.json");
        assert_eq!(
            read_json(&file),
            json!({"external_update_url": WEBSTORE_UPDATE})
        );
        // asked once; Chromium installs it at its next start
        assert!(!c.pending("Default/extensions.json", &want, Some(&have)));
        // the other profile lists it to add by hand, in that profile
        let mirror = tmp.path().join("mirror");
        merged::write(&mirror.join("Work/extensions.json"), &want)?;
        let items = c.status(&mirror, &[]);
        let work = items.iter().find(|i| i.title == "Chromium: Work").unwrap();
        assert_eq!(work.state, "install");
        assert_eq!(work.install[0].command[1], "--profile-directory=Profile 1");
        Ok(())
    }

    #[test]
    fn nothing_is_written_while_chromium_runs() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let data = tmp.path().join("chromium");
        let c = fixture(&data);
        assert!(!c.running());
        let me = format!("{}-{}", hostname(), std::process::id());
        symlink(&me, data.join("SingletonLock"))?;
        assert!(c.running());
        assert!(c.busy("Default/bookmarks.json").is_some());
        let before = fs::read(data.join("Default/Bookmarks"))?;
        assert!(
            c.apply("Default/bookmarks.json", &json!({}), None, tmp.path())
                .is_err()
        );
        assert_eq!(fs::read(data.join("Default/Bookmarks"))?, before);
        // a lock a crash left behind (its pid gone) doesn't count
        fs::remove_file(data.join("SingletonLock"))?;
        symlink(
            format!("{}-2147483646", hostname()),
            data.join("SingletonLock"),
        )?;
        assert!(!c.running());
        // another computer's, on a shared home, does
        fs::remove_file(data.join("SingletonLock"))?;
        symlink("elsewhere-1", data.join("SingletonLock"))?;
        assert!(c.running());
        Ok(())
    }

    #[test]
    fn profiles_match_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let c = fixture(&tmp.path().join("chromium"));
        assert_eq!(
            c.profiles(),
            [
                ("Default".to_string(), "Default".to_string()),
                ("Work".to_string(), "Profile 1".to_string())
            ]
        );
        let docs = c.docs();
        assert!(docs.contains(&"Default/bookmarks.json".to_string()));
        assert!(docs.contains(&"Work/preferences.json".to_string()));
        assert!(!docs.contains(&"Work/bookmarks.json".to_string()));
    }
}
