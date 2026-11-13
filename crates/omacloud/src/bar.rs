//! How sync stands, for Omarchy's bar: the daemon writes a line of JSON to
//! `omacloud/bar.json` in the session's runtime directory whenever that
//! changes, and again every half minute, and the bar widget
//! (`contrib/omarchy-plugin`) watches the file. What it says includes when:
//! a daemon that was killed says nothing more, and the widget takes a line
//! gone old for a daemon gone. Also puts the widget in the bar, or takes it
//! out.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde_json::json;

/// The widget's id in Omarchy's shell.
const PLUGIN: &str = "ferdousbhai.omacloud";

/// Where the package puts the widget; `OMACLOUD_BAR_PLUGIN` names another
/// copy, for trying one from a checkout.
fn packaged() -> PathBuf {
    std::env::var_os("OMACLOUD_BAR_PLUGIN").map_or_else(|| "/usr/share/omacloud/omarchy-plugin".into(), PathBuf::from)
}

/// What the bar shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Standing {
    Synced,
    Syncing,
    Paused,
    /// Something only a person settles: a refused key, a removed computer,
    /// a join request, a setting changed on two machines.
    Attention,
    Stopped,
}

impl Standing {
    fn name(self) -> &'static str {
        match self {
            Self::Synced => "synced",
            Self::Syncing => "syncing",
            Self::Paused => "paused",
            Self::Attention => "attention",
            Self::Stopped => "stopped",
        }
    }
}

/// The daemon's side: remembers what it last said, so the file is written
/// only when something changed.
pub struct Bar {
    file: PathBuf,
    said: Option<(Standing, String, Instant)>,
}

/// How often what was said is said again, so it doesn't go old.
const AGAIN: Duration = Duration::from_secs(30);

impl Bar {
    /// For the account the packaged daemon runs, the file the widget
    /// watches, in the session's runtime directory (in memory, gone at
    /// logout). Any other account's daemon keeps its own in `data`, so a
    /// test never speaks for the real one.
    pub fn new(data: &Path, default: bool) -> Self {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").filter(|_| default);
        let dir = runtime.map_or_else(|| data.to_path_buf(), |r| PathBuf::from(r).join("omacloud"));
        Self {
            file: dir.join("bar.json"),
            said: None,
        }
    }

    /// Say how sync stands. A failure to write only leaves the bar behind.
    pub fn say(&mut self, standing: Standing, text: &str) {
        if self
            .said
            .as_ref()
            .is_some_and(|(s, t, at)| *s == standing && t == text && at.elapsed() < AGAIN)
        {
            return;
        }
        let at = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let line = json!({ "state": standing.name(), "text": text, "at": at }).to_string();
        // written whole beside it, then moved: the widget never reads half
        let part = self.file.with_extension("json.part");
        let written = self
            .file
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|()| fs::write(&part, line))
            .and_then(|()| fs::rename(&part, &self.file));
        if written.is_ok() {
            self.said = Some((standing, text.to_string(), Instant::now()));
        }
    }

    /// Still here: say the same again once it's getting old.
    pub fn beat(&mut self) {
        if let Some((standing, text, _)) = self.said.clone() {
            self.say(standing, &text);
        }
    }
}

impl Drop for Bar {
    fn drop(&mut self) {
        self.say(Standing::Stopped, "not running");
    }
}

fn omarchy(args: &[&str]) -> Result<()> {
    let out = Command::new(args[0])
        .args(&args[1..])
        .output()
        .with_context(|| format!("running {}", args[0]))?;
    if !out.status.success() {
        bail!(
            "{} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn plugin_link(config_home: &Path) -> PathBuf {
    config_home.join("omarchy/plugins").join(PLUGIN)
}

/// Put the widget in the bar: a link to the packaged widget among Omarchy's
/// plugins, so a package update updates it, then enabled in the shell.
///
/// # Errors
///
/// If this isn't Omarchy with its shell, or the shell refuses the widget.
pub fn show(config_home: &Path) -> Result<()> {
    let from = packaged();
    if !from.join("manifest.json").is_file() {
        bail!("the bar widget isn't installed at {}", from.display());
    }
    let link = plugin_link(config_home);
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::read_link(&link) {
        Ok(to) if to == from => {}
        Ok(_) => {
            fs::remove_file(&link)?;
            std::os::unix::fs::symlink(&from, &link)?;
        }
        Err(_) if link.exists() => bail!("{} is something else's", link.display()),
        Err(_) => std::os::unix::fs::symlink(&from, &link)?,
    }
    omarchy(&["omarchy-shell", "shell", "rescanPlugins"])?;
    omarchy(&["omarchy", "plugin", "enable", PLUGIN])
}

/// Take the widget out of the bar and from among the plugins.
///
/// # Errors
///
/// If the link can't be removed.
pub fn hide(config_home: &Path) -> Result<()> {
    // the shell may not know it: gone is what was asked for
    _ = omarchy(&["omarchy", "plugin", "disable", PLUGIN]);
    let link = plugin_link(config_home);
    if fs::read_link(&link).is_ok() {
        fs::remove_file(&link)?;
        _ = omarchy(&["omarchy-shell", "shell", "rescanPlugins"]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_says_how_sync_stands_and_stopped_at_the_end() {
        let dir = tempfile::tempdir().unwrap();
        let read = || fs::read_to_string(dir.path().join("bar.json")).unwrap();
        let mut bar = Bar::new(dir.path(), false);
        bar.say(Standing::Syncing, "Syncing");
        assert!(read().contains(r#""state":"syncing","text":"Syncing""#), "{}", read());
        bar.say(Standing::Synced, "Up to date");
        assert!(read().contains(r#""state":"synced""#));
        drop(bar);
        assert!(read().contains(r#""state":"stopped""#));
    }
}
