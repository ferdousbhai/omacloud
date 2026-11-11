//! Desktop notifications from the daemon, through `notify-send` (libnotify;
//! mako on Omarchy): a device asking to join, a setting that needs a choice,
//! this device being removed, the storage refusing this device's key. "Open" starts the Omacloud app on the page the
//! notification is about. Each thing is announced once per run, so once
//! more after the daemon restarts (an upgrade, a changed config).

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use omacloud_core::{Engine, fingerprint};

/// How often to look for join requests; the change feed only wakes the
/// daemon for new versions.
const REQUESTS_EVERY: Duration = Duration::from_secs(60);

/// How often to look for new folders in home: one directory listing.
const FOLDERS_EVERY: Duration = Duration::from_secs(10);

/// How long a new folder keeps its name before it's offered.
const NAME_SETTLES: Duration = Duration::from_secs(20);

/// The folders of home already seen, one name a line, in the data directory.
const HOME_FOLDERS_FILE: &str = "home-folders";

/// The folders of home seen from now on, and the ones to offer: those in
/// `now` that `seen` lacks and `settled` passes. One not settled yet is left
/// unseen, for a later look. With nothing seen before, all are seen and none
/// offered.
fn new_in_home(
    seen: Option<&BTreeSet<String>>,
    now: &[String],
    settled: impl Fn(&str) -> bool,
) -> (BTreeSet<String>, Vec<String>) {
    let Some(seen) = seen else {
        return (now.iter().cloned().collect(), Vec::new());
    };
    let mut all = seen.clone();
    let mut fresh = Vec::new();
    for name in now {
        if !seen.contains(name) && settled(name) && all.insert(name.clone()) {
            fresh.push(name.clone());
        }
    }
    (all, fresh)
}

/// How long ago the folder last changed: renamed, or a file put in it.
fn changed_since(path: &Path) -> Option<Duration> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::symlink_metadata(path).ok()?;
    let then = std::time::UNIX_EPOCH + Duration::from_secs(u64::try_from(meta.ctime()).ok()?);
    then.elapsed().ok()
}

/// The message of a failed command's JSON envelope.
fn failure(stdout: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(stdout)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
        .unwrap_or_default()
}

pub struct Alerts {
    /// The config to open the Omacloud app with, when it isn't the default.
    config: Option<PathBuf>,
    requests: BTreeSet<String>,
    held: BTreeSet<PathBuf>,
    requests_at: Option<Instant>,
    key_refused: bool,
    folders_at: Option<Instant>,
}

impl Alerts {
    pub fn new(config: Option<&Path>) -> Self {
        Self {
            config: config.map(Path::to_path_buf),
            requests: BTreeSet::new(),
            held: BTreeSet::new(),
            requests_at: None,
            key_refused: false,
            folders_at: None,
        }
    }

    /// Announce what is new since the last look.
    pub fn check(&mut self, engine: &mut Engine) {
        for path in engine.settings_status().held {
            if self.held.insert(path.clone()) {
                self.show(
                    "A setting changed on two machines",
                    &format!(
                        "~/{} changed here and on another device. This machine keeps its version until you choose.",
                        path.display()
                    ),
                    "settings",
                );
            }
        }
        if self.requests_at.is_some_and(|t| t.elapsed() < REQUESTS_EVERY) {
            return;
        }
        self.requests_at = Some(Instant::now());
        let Ok(requests) = engine.requests() else {
            return;
        };
        for r in requests {
            if self.requests.insert(r.device.clone()) {
                self.show(
                    &format!("{} asks to join", r.name),
                    &format!(
                        "Approve it only if it shows the fingerprint {}.",
                        fingerprint(&r.device)
                    ),
                    "devices",
                );
            }
        }
    }

    /// Say that this device was removed. Waits for `notify-send` to hand
    /// it over, since the daemon exits right after.
    pub fn removed(&self) {
        _ = Command::new("notify-send")
            .args([
                "--app-name=Omacloud",
                "--icon=folder-remote",
                "--urgency=critical",
                "This device was removed",
                "Another device removed it from the account, so it has stopped syncing. Your files here stay.",
            ])
            .stderr(Stdio::null())
            .status();
    }

    /// Say that the storage refuses this device's key: it retries, but only
    /// a person can give it a new one.
    pub fn key_refused(&mut self) {
        if !std::mem::replace(&mut self.key_refused, true) {
            self.show(
                "This device can't reach your storage",
                "Your storage refused its key. If it changed while this device was away, run omacloud bucket \
                 set-key --here-only (Dropbox asks you to allow it again), with --access-key-id <id> for a bucket.",
                "overview",
            );
        }
    }

    /// Offer to sync a folder that's new directly in home (`now`: its
    /// folders, by name), once each. The folders already seen are kept in
    /// `data`, so the first look only notes what's there.
    pub fn new_folders(&mut self, home: &Path, data: &Path, now: &[String], synced: &[String]) {
        if self.folders_at.is_some_and(|t| t.elapsed() < FOLDERS_EVERY) {
            return;
        }
        self.folders_at = Some(Instant::now());
        let file = data.join(HOME_FOLDERS_FILE);
        let seen: Option<BTreeSet<String>> = fs::read_to_string(&file)
            .ok()
            .map(|s| s.lines().map(str::to_string).collect());
        let (settled, fresh) = new_in_home(seen.as_ref(), now, |name| {
            // a file manager makes "New Folder" and renames it: wait until
            // the name has stood a while
            changed_since(&home.join(name)).is_none_or(|age| age >= NAME_SETTLES)
        });
        if seen.as_ref() != Some(&settled) {
            let list: Vec<&str> = settled.iter().map(String::as_str).collect();
            _ = fs::write(&file, list.join("\n"));
        }
        for name in fresh.into_iter().filter(|n| !synced.contains(n)) {
            self.offer_folder(home, &name);
        }
    }

    /// Clicking the notification is its `default` action: the only one
    /// Omarchy's shell runs, as it shows no buttons.
    fn offer_folder(&self, home: &Path, name: &str) {
        let (name, path) = (name.to_string(), home.join(name));
        let config = self.config.clone();
        std::thread::spawn(move || {
            let Ok(out) = Command::new("notify-send")
                .args([
                    "--app-name=Omacloud",
                    "--icon=folder-remote",
                    // as long as Omarchy's shell shows one
                    "--expire-time=30000",
                    &format!("--action=default=Sync {name}"),
                    &format!("Sync {name}?"),
                    &format!("~/{name} is new on this computer. Click to sync it on all your computers."),
                ])
                .stderr(Stdio::null())
                .output()
            else {
                return;
            };
            if String::from_utf8_lossy(&out.stdout).trim() != "default" {
                return;
            }
            let mut add = Command::new("omacloud");
            add.args(["folders", "add", "--json"]).arg(&path);
            if let Some(config) = config {
                add.env("OMACLOUD_CONFIG", config);
            }
            let (summary, body) = match add.stdin(Stdio::null()).stderr(Stdio::null()).output() {
                Ok(out) if out.status.success() => (format!("{name} now syncs"), String::new()),
                Ok(out) => (format!("{name} couldn't be added"), failure(&out.stdout)),
                Err(e) => (format!("{name} couldn't be added"), e.to_string()),
            };
            _ = Command::new("notify-send")
                .args(["--app-name=Omacloud", "--icon=folder-remote", &summary, &body])
                .stderr(Stdio::null())
                .status();
        });
    }

    fn show(&self, summary: &str, body: &str, page: &str) {
        let (summary, body, page) = (summary.to_string(), body.to_string(), page.to_string());
        let config = self.config.clone();
        // `--action` waits for the notification to close; don't hold up sync
        std::thread::spawn(move || {
            let Ok(out) = Command::new("notify-send")
                .args([
                    "--app-name=Omacloud",
                    "--icon=folder-remote",
                    "--action=default=Open",
                    &summary,
                    &body,
                ])
                .stderr(Stdio::null())
                .output()
            else {
                return;
            };
            if String::from_utf8_lossy(&out.stdout).trim() == "default" {
                let mut app = Command::new("omacloud-app");
                app.arg(&page);
                if let Some(config) = config {
                    app.env("OMACLOUD_CONFIG", config);
                }
                _ = app.spawn();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn a_new_folder_of_home_is_offered_once() {
        // the first look only notes what's there
        let (seen, fresh) = new_in_home(None, &names(&["Documents", "go"]), |_| true);
        assert!(fresh.is_empty());
        // one made since is offered, then seen
        let now = names(&["Documents", "Works", "go"]);
        let (seen, fresh) = new_in_home(Some(&seen), &now, |_| true);
        assert_eq!(fresh, names(&["Works"]));
        assert!(new_in_home(Some(&seen), &now, |_| true).1.is_empty());
        // deleted and made again, it isn't asked about twice
        let (seen, _) = new_in_home(Some(&seen), &names(&["Documents", "go"]), |_| true);
        assert!(new_in_home(Some(&seen), &now, |_| true).1.is_empty());
    }

    #[test]
    fn a_folder_still_being_named_waits() {
        let (seen, _) = new_in_home(None, &names(&["Documents"]), |_| true);
        let now = names(&["Documents", "New Folder"]);
        let (seen, fresh) = new_in_home(Some(&seen), &now, |_| false);
        assert!(fresh.is_empty());
        assert!(!seen.contains("New Folder"));
        let now = names(&["Documents", "Works"]);
        assert_eq!(new_in_home(Some(&seen), &now, |_| true).1, names(&["Works"]));
    }
}
