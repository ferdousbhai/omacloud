//! The Omarchy theme a person chose, on all their computers (a [`Group`]
//! with one merged document).
//!
//! Omarchy keeps the choice as a name in
//! `~/.local/state/omarchy/current/theme.name`, beside the theme's files
//! staged for the running desktop. Copying those files wouldn't change a
//! running desktop, so the name syncs, and a change from another computer
//! is applied with `omarchy-theme-set`, as choosing it in the menu does. A
//! theme that isn't installed here waits until it is.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, anyhow, ensure};
use serde_json::{Value, json};

use crate::merged::{self, Concern, Group, Item, Merged};

const DOC: &str = "theme.json";

#[derive(Debug, Clone)]
pub struct Theme {
    /// Where Omarchy keeps what's current: `theme.name` is in it.
    current: PathBuf,
    /// The folders installed themes are in, each a folder by its name.
    installed: Vec<PathBuf>,
    /// The command that applies a theme, given its name.
    set: PathBuf,
}

impl Theme {
    /// As Omarchy lays it out under `home`, with its own themes in
    /// `omarchy` (`/usr/share/omarchy`).
    #[must_use]
    pub fn for_home(home: &Path, omarchy: &Path) -> Self {
        Self::new(
            home.join(".local/state/omarchy/current"),
            vec![home.join(".config/omarchy/themes"), omarchy.join("themes")],
            omarchy.join("bin/omarchy-theme-set"),
        )
    }

    #[must_use]
    pub fn new(current: PathBuf, installed: Vec<PathBuf>, set: PathBuf) -> Self {
        Self {
            current,
            installed,
            set,
        }
    }

    fn name_file(&self) -> PathBuf {
        self.current.join("theme.name")
    }

    /// The theme in use, by the name Omarchy keeps (`tokyo-night`).
    fn name(&self) -> Option<String> {
        let name = fs::read_to_string(self.name_file()).ok()?.trim().to_string();
        (!name.is_empty()).then_some(name)
    }
}

/// A theme's name as its folder is called: nothing that leaves the themes'
/// folder or reads as an option to the command.
fn plain(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && name != "."
        && name != ".."
}

impl Group for Theme {
    fn name(&self) -> &str {
        "theme"
    }

    fn docs(&self) -> Vec<String> {
        if self.name().is_some() {
            vec![DOC.to_string()]
        } else {
            Vec::new()
        }
    }

    fn capture(&self, _doc: &str, _applied: Option<&Value>) -> Result<Option<Value>> {
        Ok(self.name().map(|name| json!({ "name": name })))
    }

    fn busy(&self, _doc: &str) -> Option<String> {
        None
    }

    fn apply(&self, _doc: &str, value: &Value, _applied: Option<&Value>, _backups: &Path) -> Result<()> {
        let name = value
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("the synced theme has no name"))?;
        if self.name().as_deref() == Some(name) {
            return Ok(());
        }
        ensure!(plain(name), "{name:?} isn't a theme's name");
        ensure!(
            self.installed.iter().any(|dir| dir.join(name).is_dir()),
            "the theme {name} isn't installed on this computer"
        );
        // the command calls Omarchy's others by name
        let bin = self.set.parent().unwrap_or(Path::new("/"));
        let path = std::env::var_os("PATH").unwrap_or_default();
        let path = std::env::join_paths(std::iter::once(bin.to_path_buf()).chain(std::env::split_paths(&path)))?;
        let done = Command::new(&self.set)
            .arg(name)
            .env("PATH", path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .with_context(|| format!("running {}", self.set.display()))?;
        ensure!(
            done.status.success(),
            "the theme {name} couldn't be set: {}",
            String::from_utf8_lossy(&done.stderr).trim()
        );
        Ok(())
    }

    fn merge(&self, _doc: &str, base: Option<&Value>, ours: &Value, theirs: &Value) -> Merged {
        let mut conflicts = Vec::new();
        let map = |v: &Value| v.as_object().cloned().unwrap_or_default();
        let value = merged::merge_map(
            base.and_then(Value::as_object),
            &map(ours),
            &map(theirs),
            false,
            &|_| "the theme".to_string(),
            &mut conflicts,
        );
        Merged {
            value: Value::Object(value),
            conflicts,
        }
    }

    fn status(&self, _mirror: &Path, waiting: &[(String, String)]) -> Vec<Item> {
        let Some(name) = self.name() else {
            return Vec::new();
        };
        vec![Item {
            group: "theme".into(),
            title: "Theme".into(),
            state: if waiting.is_empty() { "synced" } else { "waiting" }.into(),
            detail: name,
            install: Vec::new(),
            not_synced: Vec::new(),
            scope: "theme/".into(),
        }]
    }

    fn watch_dirs(&self) -> Vec<PathBuf> {
        vec![self.current.clone()]
    }

    fn concerns(&self, path: &Path) -> Concern {
        if path == self.name_file() {
            Concern::Look
        } else {
            Concern::No
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// A home with two themes installed and a command that sets one as
    /// Omarchy's does, by writing its name.
    fn theme(dir: &Path) -> Theme {
        let current = dir.join("current");
        fs::create_dir_all(&current).unwrap();
        fs::write(current.join("theme.name"), "tokyo-night\n").unwrap();
        for name in ["tokyo-night", "nord"] {
            fs::create_dir_all(dir.join("themes").join(name)).unwrap();
        }
        let set = dir.join("bin/omarchy-theme-set");
        fs::create_dir_all(set.parent().unwrap()).unwrap();
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$1\" > '{}'\n",
            current.join("theme.name").display()
        );
        fs::write(&set, script).unwrap();
        fs::set_permissions(&set, fs::Permissions::from_mode(0o755)).unwrap();
        Theme::new(current, vec![dir.join("themes")], set)
    }

    #[test]
    fn a_theme_chosen_elsewhere_is_set_here() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let g = theme(dir.path());
        let here = g.capture(DOC, None)?.unwrap();
        assert_eq!(here, json!({ "name": "tokyo-night" }));
        g.apply(DOC, &json!({ "name": "nord" }), Some(&here), dir.path())?;
        assert_eq!(g.capture(DOC, None)?.unwrap(), json!({ "name": "nord" }));
        Ok(())
    }

    #[test]
    fn a_theme_not_installed_here_waits_and_an_odd_name_is_refused() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let g = theme(dir.path());
        let e = g
            .apply(DOC, &json!({ "name": "gruvbox" }), None, dir.path())
            .unwrap_err();
        assert!(e.to_string().contains("isn't installed"), "{e}");
        for odd in ["../nord", "--help", "a b", ""] {
            assert!(
                g.apply(DOC, &json!({ "name": odd }), None, dir.path()).is_err(),
                "{odd}"
            );
        }
        assert_eq!(g.capture(DOC, None)?.unwrap(), json!({ "name": "tokyo-night" }));
        Ok(())
    }

    #[test]
    fn a_theme_changed_on_one_computer_wins_and_on_both_keeps_this_ones() {
        let g = theme(tempfile::tempdir().unwrap().path());
        let (base, ours, theirs) = (json!({ "name": "a" }), json!({ "name": "a" }), json!({ "name": "b" }));
        assert_eq!(g.merge(DOC, Some(&base), &ours, &theirs).value, theirs);
        let m = g.merge(DOC, Some(&base), &json!({ "name": "c" }), &theirs);
        assert_eq!(m.value, json!({ "name": "c" }));
        assert_eq!(m.conflicts.len(), 1);
    }
}
