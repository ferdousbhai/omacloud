//! Which settings sync, following Omarchy's dots manifest.
//!
//! The manifest (omacom/omarchy PR #12037, `default/dots/manifest`) lists
//! home relative files as `shared` (travel between machines) or `local`
//! (machine specific, never synced). onecloud syncs the shared tier
//! continuously. Settings live in the folder's history under
//! [`PREFIX`], so versions, key rotation and plain restic restores cover
//! them too.
//!
//! As dots does, settings sync stands down ("dormant") when manifest paths
//! are symlinks or another dotfile manager is in use: those users already
//! have their own sync, and writing through their links would fight it.

use std::{
    fs,
    path::{Component, Path, PathBuf},
};

use anyhow::Result;

/// Where settings live in the synced tree.
pub const PREFIX: &str = ".onecloud/settings";

/// The manifest from Omarchy's dots (PR #12037), used until Omarchy ships
/// its own at `$OMARCHY_PATH/default/dots/manifest`.
const BUNDLED: &str = include_str!("dots-manifest");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Shared,
    Local,
}

/// Parsed manifest: `tier path` per line, `#` comments. A `*` matches
/// within one path component, never across `/`.
#[derive(Debug, Clone)]
pub struct Manifest {
    entries: Vec<(Tier, String)>,
}

impl Manifest {
    /// # Errors
    ///
    /// On a line that isn't `shared <path>` or `local <path>`, or a path
    /// that isn't plainly relative.
    pub fn parse(text: &str) -> Result<Self> {
        let mut entries = Vec::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (tier, path) = line
                .split_once(char::is_whitespace)
                .ok_or_else(|| anyhow::anyhow!("manifest line {}: expected `tier path`", n + 1))?;
            let tier = match tier {
                "shared" => Tier::Shared,
                "local" => Tier::Local,
                other => anyhow::bail!("manifest line {}: unknown tier `{other}`", n + 1),
            };
            let path = path.trim();
            anyhow::ensure!(
                Path::new(path)
                    .components()
                    .all(|c| matches!(c, Component::Normal(_))),
                "manifest line {}: `{path}` must be a plain relative path",
                n + 1
            );
            entries.push((tier, path.to_string()));
        }
        Ok(Self { entries })
    }

    /// The manifest from dots (PR #12037) that onecloud ships.
    ///
    /// # Panics
    ///
    /// Never: the bundled manifest is tested to parse.
    #[must_use]
    pub fn bundled() -> Self {
        Self::parse(BUNDLED).expect("bundled manifest parses")
    }

    /// Omarchy's manifest if it ships one, otherwise the bundled copy.
    ///
    /// # Errors
    ///
    /// If Omarchy's manifest exists but doesn't parse.
    pub fn load() -> Result<Self> {
        let omarchy = std::env::var_os("OMARCHY_PATH")
            .map_or_else(|| PathBuf::from("/usr/share/omarchy"), PathBuf::from)
            .join("default/dots/manifest");
        match fs::read_to_string(&omarchy) {
            Ok(text) => Self::parse(&text),
            Err(_) => Self::parse(BUNDLED),
        }
    }

    /// Whether `rel` (relative to home) is in the shared tier. A path listed
    /// as local is never shared, whatever else matches.
    #[must_use]
    pub fn is_shared(&self, rel: &Path) -> bool {
        let matching = |tier| {
            self.entries
                .iter()
                .any(|(t, pattern)| *t == tier && matches(pattern, rel))
        };
        matching(Tier::Shared) && !matching(Tier::Local)
    }

    /// Shared files present under `home`, relative to it.
    #[must_use]
    pub fn shared_files(&self, home: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for (tier, pattern) in &self.entries {
            if *tier != Tier::Shared {
                continue;
            }
            expand(home, Path::new(""), &components(pattern), &mut out);
        }
        out.retain(|p| self.is_shared(p));
        out.sort();
        out.dedup();
        out
    }

    /// Directories whose direct entries the manifest may match; a watcher
    /// watches these (not recursively).
    #[must_use]
    pub fn watch_dirs(&self, home: &Path) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = self
            .entries
            .iter()
            .filter(|(t, _)| *t == Tier::Shared)
            .map(|(_, p)| home.join(Path::new(p).parent().unwrap_or(Path::new(""))))
            .collect();
        dirs.sort();
        dirs.dedup();
        dirs
    }

    /// Why settings sync should stand down, if it should: a manifest path is
    /// a symlink, or another dotfile manager is in use.
    #[must_use]
    pub fn dormant(&self, home: &Path) -> Option<String> {
        for (_, pattern) in &self.entries {
            let mut found = Vec::new();
            expand_any(home, Path::new(""), &components(pattern), &mut found);
            if let Some(p) = found.first() {
                return Some(format!("{} is a symlink (a dotfile manager?)", p.display()));
            }
        }
        for marker in [
            ".local/share/chezmoi",
            ".local/share/yadm",
            ".config/yadm",
            ".git",
        ] {
            if fs::symlink_metadata(home.join(marker)).is_ok() {
                return Some(format!(
                    "~/{marker} exists: another dotfile manager is in use"
                ));
            }
        }
        None
    }
}

fn components(pattern: &str) -> Vec<String> {
    pattern.split('/').map(str::to_string).collect()
}

/// Whether `rel` matches `pattern` component by component.
fn matches(pattern: &str, rel: &Path) -> bool {
    let pat = components(pattern);
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    pat.len() == parts.len() && pat.iter().zip(&parts).all(|(p, s)| glob(p, s))
}

/// `*` matches any run of characters within one component.
fn glob(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == name,
        Some((head, tail)) => {
            let Some(rest) = name.strip_prefix(head) else {
                return false;
            };
            (0..=rest.len())
                .filter(|&i| rest.is_char_boundary(i))
                .any(|i| glob(tail, &rest[i..]))
        }
    }
}

/// Regular files under `home` matching the pattern components.
fn expand(home: &Path, rel: &Path, pattern: &[String], out: &mut Vec<PathBuf>) {
    let Some((first, rest)) = pattern.split_first() else {
        return;
    };
    let Ok(entries) = fs::read_dir(home.join(rel)) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !glob(first, &name) {
            continue;
        }
        let path = rel.join(&name);
        let Ok(ft) = e.file_type() else { continue };
        if rest.is_empty() {
            if ft.is_file() {
                out.push(path);
            }
        } else if ft.is_dir() {
            expand(home, &path, rest, out);
        }
    }
}

/// Symlinks anywhere along the pattern's matches.
fn expand_any(home: &Path, rel: &Path, pattern: &[String], out: &mut Vec<PathBuf>) {
    let Some((first, rest)) = pattern.split_first() else {
        return;
    };
    let Ok(entries) = fs::read_dir(home.join(rel)) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !glob(first, &name) {
            continue;
        }
        let path = rel.join(&name);
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_symlink() {
            out.push(path);
        } else if ft.is_dir() && !rest.is_empty() {
            expand_any(home, &path, rest, out);
        }
    }
}

/// Three way merge of text settings: `None` when the two sides changed the
/// same lines, or a side isn't UTF-8 text.
#[must_use]
pub fn merge(base: &[u8], ours: &[u8], theirs: &[u8]) -> Option<Vec<u8>> {
    let (Ok(base), Ok(ours), Ok(theirs)) = (
        std::str::from_utf8(base),
        std::str::from_utf8(ours),
        std::str::from_utf8(theirs),
    ) else {
        return None;
    };
    diffy::merge(base, ours, theirs)
        .ok()
        .map(String::into_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_and_wildcards() -> Result<()> {
        let m = Manifest::parse(BUNDLED)?;
        let yes = |p: &str| assert!(m.is_shared(Path::new(p)), "{p} should be shared");
        let no = |p: &str| assert!(!m.is_shared(Path::new(p)), "{p} should not be shared");
        yes(".bashrc");
        yes(".config/hypr/bindings.lua");
        yes(".config/alacritty/alacritty.toml");
        yes(".config/omarchy/extensions/x.jsonc");
        no(".config/hypr/monitors.lua"); // local tier
        no(".config/hypr/hyprland.lua");
        no(".config/alacritty/themes/x.toml"); // * stays within one component
        no(".ssh/id_ed25519");
        no(".config/chromium/Default/Preferences");
        assert!(Manifest::parse("secret .x").is_err());
        assert!(Manifest::parse("shared ../escape").is_err());
        Ok(())
    }

    #[test]
    fn expands_present_files_and_stands_down_for_symlinks() -> Result<()> {
        let home = tempfile::tempdir()?;
        let h = home.path();
        fs::create_dir_all(h.join(".config/hypr"))?;
        fs::create_dir_all(h.join(".config/alacritty/themes"))?;
        fs::write(h.join(".bashrc"), "x")?;
        fs::write(h.join(".config/hypr/bindings.lua"), "x")?;
        fs::write(h.join(".config/hypr/monitors.lua"), "x")?;
        fs::write(h.join(".config/alacritty/alacritty.toml"), "x")?;
        fs::write(h.join(".config/alacritty/themes/dark.toml"), "x")?;
        let m = Manifest::parse(BUNDLED)?;
        assert_eq!(
            m.shared_files(h),
            [
                ".bashrc",
                ".config/alacritty/alacritty.toml",
                ".config/hypr/bindings.lua"
            ]
            .map(PathBuf::from)
        );
        assert_eq!(m.dormant(h), None);
        std::os::unix::fs::symlink(".bashrc", h.join(".XCompose"))?;
        assert!(m.dormant(h).unwrap().contains(".XCompose"));
        Ok(())
    }

    #[test]
    fn merges_separate_changes_and_refuses_overlapping_ones() {
        let base = b"a = 1\nb = 2\nc = 3\n";
        let ours = b"a = 10\nb = 2\nc = 3\n";
        let theirs = b"a = 1\nb = 2\nc = 30\n";
        assert_eq!(
            merge(base, ours, theirs).unwrap(),
            b"a = 10\nb = 2\nc = 30\n"
        );
        assert!(merge(base, b"a = 5\nb = 2\nc = 3\n", ours).is_none());
        assert!(merge(b"\xff", b"x", b"y").is_none());
    }
}
