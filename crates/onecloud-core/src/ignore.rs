//! Which paths don't sync, in gitignore syntax.
//!
//! Rules come from built-in defaults, the user's own list, and a
//! `.onecloudignore` file at the folder root. That file syncs like any other,
//! so every device follows the same rules. An ignored path is neither pushed
//! nor pulled; copies already in history stay there.

use std::path::Path;

use anyhow::Result;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// Prefix of the temp files a pull writes before renaming into place.
pub const TEMP_PREFIX: &str = ".~onecloud-";

/// Rules file at the root of a synced folder.
pub const FOLDER_FILE: &str = ".onecloudignore";

/// Always ignored: our temp files, editor swap files, desktop junk, and, as
/// in iCloud, anything named `*.nosync`: a file or folder that stays on this
/// computer.
pub const DEFAULTS: &[&str] = &[
    "*.nosync",
    ".~onecloud-*",
    "*.swp",
    "*.swx",
    ".#*",
    ".DS_Store",
    "Thumbs.db",
    ".Trash-*/",
];

#[derive(Debug, Clone)]
pub struct Ignores {
    rules: Gitignore,
}

impl Ignores {
    /// Defaults, then `extra` (one rule per entry, gitignore syntax), then the
    /// folder's `.onecloudignore` if present. Later rules win, so `!pattern`
    /// can re-include.
    ///
    /// # Errors
    ///
    /// If a rule doesn't parse or the folder file can't be read.
    pub fn load(folder: &Path, extra: &[String]) -> Result<Self> {
        let mut b = GitignoreBuilder::new(folder);
        for rule in DEFAULTS
            .iter()
            .copied()
            .chain(extra.iter().map(String::as_str))
        {
            _ = b.add_line(None, rule)?;
        }
        let file = folder.join(FOLDER_FILE);
        if file.is_file()
            && let Some(err) = b.add(&file)
        {
            return Err(err.into());
        }
        Ok(Self { rules: b.build()? })
    }

    /// Only the built-in defaults.
    #[must_use]
    pub fn defaults() -> Self {
        let mut b = GitignoreBuilder::new("");
        for rule in DEFAULTS {
            _ = b.add_line(None, rule);
        }
        Self {
            rules: b.build().unwrap_or_else(|_| Gitignore::empty()),
        }
    }

    /// Whether `rel` (relative to the folder) or any of its parent
    /// directories is ignored. `is_dir` matters for rules ending in `/`.
    #[must_use]
    pub fn is_ignored(&self, rel: &Path, is_dir: bool) -> bool {
        if rel
            .components()
            .any(|c| c.as_os_str().to_string_lossy().starts_with(TEMP_PREFIX))
        {
            return true;
        }
        self.rules
            .matched_path_or_any_parents(rel, is_dir)
            .is_ignore()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_user_rules_and_folder_file() -> Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join(FOLDER_FILE), "build/\n*.log\n!keep.log\n")?;
        let ig = Ignores::load(dir.path(), &["node_modules/".into()])?;
        let yes = |p: &str, d| assert!(ig.is_ignored(Path::new(p), d), "{p} should be ignored");
        let no = |p: &str, d| assert!(!ig.is_ignored(Path::new(p), d), "{p} should sync");
        yes(".~onecloud-notes.md", false);
        yes("docs/.notes.md.swp", false);
        yes("web/node_modules/react/index.js", false);
        yes("build", true);
        yes("build/out.o", false);
        yes("server/debug.log", false);
        no("server/keep.log", false);
        no("notes.md", false);
        no(FOLDER_FILE, false);
        // as in iCloud: a name ending in .nosync stays on this computer
        yes("photos/raw.nosync", true);
        yes("photos/raw.nosync/img.cr3", false);
        yes("cache.db.nosync", false);
        no("nosync.md", false);
        Ok(())
    }
}
