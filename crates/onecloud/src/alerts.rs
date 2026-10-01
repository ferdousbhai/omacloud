//! Desktop notifications from the daemon, through `notify-send` (libnotify;
//! mako on Omarchy): a device asking to join, a setting that needs a choice,
//! this device being removed. "Open" starts the OneCloud app on the page the
//! notification is about. Each thing is announced once per run.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use onecloud_core::{Engine, fingerprint};

/// How often to look for join requests; the change feed only wakes the
/// daemon for new versions.
const REQUESTS_EVERY: Duration = Duration::from_secs(60);

pub struct Alerts {
    /// The config to open the OneCloud app with, when it isn't the default.
    config: Option<PathBuf>,
    requests: BTreeSet<String>,
    held: BTreeSet<PathBuf>,
    requests_at: Option<Instant>,
}

impl Alerts {
    pub fn new(config: Option<&Path>) -> Self {
        Self {
            config: config.map(Path::to_path_buf),
            requests: BTreeSet::new(),
            held: BTreeSet::new(),
            requests_at: None,
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
        if self
            .requests_at
            .is_some_and(|t| t.elapsed() < REQUESTS_EVERY)
        {
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
                "--app-name=OneCloud",
                "--icon=folder-remote",
                "--urgency=critical",
                "This device was removed",
                "Another device removed it from the account, so it has stopped syncing. Your files here stay.",
            ])
            .stderr(Stdio::null())
            .status();
    }

    fn show(&self, summary: &str, body: &str, page: &str) {
        let (summary, body, page) = (summary.to_string(), body.to_string(), page.to_string());
        let config = self.config.clone();
        // `--action` waits for the notification to close; don't hold up sync
        std::thread::spawn(move || {
            let Ok(out) = Command::new("notify-send")
                .args([
                    "--app-name=OneCloud",
                    "--icon=folder-remote",
                    "--action=open=Open",
                    &summary,
                    &body,
                ])
                .stderr(Stdio::null())
                .output()
            else {
                return;
            };
            if String::from_utf8_lossy(&out.stdout).trim() == "open" {
                let mut app = Command::new("onecloud-app");
                app.arg(&page);
                if let Some(config) = config {
                    app.env("ONECLOUD_CONFIG", config);
                }
                _ = app.spawn();
            }
        });
    }
}
