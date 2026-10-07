//! Which settings sync, following Omarchy's dots manifest.
//!
//! The manifest (omacom/omarchy PR #12037, `default/dots/manifest`) lists
//! home relative files as `shared` (travel between machines) or `local`
//! (machine specific, never synced). omacloud syncs the shared tier
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
pub const PREFIX: &str = ".omacloud/settings";

/// The manifest from Omarchy's dots (PR #12037), used until Omarchy ships
/// its own at `$OMARCHY_PATH/default/dots/manifest`, then Omacloud's
/// additions after [`ADDITIONS`].
const BUNDLED: &str = include_str!("dots-manifest");

/// The line in [`BUNDLED`] that begins Omacloud's additions, which apply
/// over Omarchy's own manifest too.
const ADDITIONS: &str = "# Omacloud's additions";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Shared,
    Local,
}

/// Parsed manifest: `tier path` per line, `#` comments. A `*` matches
/// within one path component, never across `/`; a last component `**`
/// matches every file below, as [`DEEP`] allows.
#[derive(Debug, Clone)]
pub struct Manifest {
    entries: Vec<(Tier, String)>,
    /// How many entries are Omarchy's own: the rest are Omacloud's
    /// additions (after [`ADDITIONS`]).
    omarchy: usize,
}

impl Manifest {
    /// # Errors
    ///
    /// On a line that isn't `shared <path>` or `local <path>`, or a path
    /// that isn't plainly relative.
    pub fn parse(text: &str) -> Result<Self> {
        let mut entries = Vec::new();
        let mut omarchy = None;
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.starts_with(ADDITIONS) && omarchy.is_none() {
                omarchy = Some(entries.len());
            }
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
                !path.contains("**") || (path.ends_with("/**") && path.matches("**").count() == 1),
                "manifest line {}: `**` can only end a path",
                n + 1
            );
            anyhow::ensure!(
                Path::new(path)
                    .components()
                    .all(|c| matches!(c, Component::Normal(_))),
                "manifest line {}: `{path}` must be a plain relative path",
                n + 1
            );
            entries.push((tier, path.to_string()));
        }
        let omarchy = omarchy.unwrap_or(entries.len());
        Ok(Self { entries, omarchy })
    }

    /// The manifest from dots (PR #12037) that omacloud ships.
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
            Ok(text) => Self::parse(&format!("{text}\n{}", additions())),
            Err(_) => Self::parse(BUNDLED),
        }
    }

    /// Whether `rel` (relative to home) is in the shared tier. A path listed
    /// as local is never shared, whatever else matches.
    /// Sign-ins and tokens never are, whatever matches (see [`secret_name`]).
    #[must_use]
    pub fn is_shared(&self, rel: &Path) -> bool {
        let matching = |tier| {
            self.entries
                .iter()
                .any(|(t, pattern)| *t == tier && matches(pattern, rel))
        };
        matching(Tier::Shared) && !matching(Tier::Local) && !secret_name(rel)
    }

    /// Whether `rel` is shared only by Omacloud's additions.
    fn addition(&self, rel: &Path) -> bool {
        !self.entries[..self.omarchy]
            .iter()
            .any(|(t, p)| *t == Tier::Shared && matches(p, rel))
    }

    /// Links among what Omacloud's additions would share (a file, or a
    /// directory on the way to one): left as they are, never read or
    /// written through.
    #[must_use]
    pub fn linked(&self, home: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for (tier, pattern) in &self.entries[self.omarchy..] {
            if *tier != Tier::Shared {
                continue;
            }
            let mut parts = components(pattern);
            if parts.last().is_some_and(|p| p == "**") {
                _ = parts.pop();
            }
            expand_any(home, Path::new(""), &parts, &mut out);
        }
        out.sort();
        out.dedup();
        out
    }

    /// Whether `rel` is shared only through a `**` entry, and so must pass
    /// [`DEEP`]'s checks.
    fn deep(&self, rel: &Path) -> bool {
        !self
            .entries
            .iter()
            .any(|(t, p)| *t == Tier::Shared && !p.ends_with("/**") && matches(p, rel))
    }

    /// Whether a shared file at `full` (`rel` under home) can sync: one
    /// shared through `**` is text of at most [`DEEP`]'s size.
    #[must_use]
    pub fn admits(&self, rel: &Path, full: &Path) -> bool {
        !self.deep(rel) || plain_text(full)
    }

    /// Whether a shared path under `home` passes through a symlink below a
    /// `**` entry's directory, or is one: never synced (writing through a
    /// link would land outside it).
    #[must_use]
    /// The same goes for anything shared only by Omacloud's additions: a
    /// linked `~/.claude` or `CLAUDE.md` is left as it is, instead of
    /// standing settings sync down (see [`Manifest::linked`]).
    pub fn through_link(&self, home: &Path, rel: &Path) -> bool {
        if !self.deep(rel) && !self.addition(rel) {
            return false;
        }
        let mut at = home.to_path_buf();
        rel.components().any(|c| {
            at.push(c);
            fs::symlink_metadata(&at).is_ok_and(|m| m.file_type().is_symlink())
        })
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

    /// Directories the manifest may match in, and whether to watch each
    /// recursively (for a `**` entry) or only its direct entries. A `*` in
    /// a directory's name stands for each directory there now
    /// (`.config/nvim/lua/*/`).
    #[must_use]
    pub fn watch_dirs(&self, home: &Path) -> Vec<(PathBuf, bool)> {
        let mut dirs = Vec::new();
        for (tier, pattern) in &self.entries {
            if *tier != Tier::Shared {
                continue;
            }
            let mut parts = components(pattern);
            let deep = parts.pop().as_deref() == Some("**");
            let mut found = Vec::new();
            if parts.iter().any(|p| p.contains('*')) {
                expand_dirs(home, Path::new(""), &parts, &mut found);
            } else {
                found.push(home.join(parts.join("/")));
            }
            dirs.extend(found.into_iter().map(|d| (d, deep)));
        }
        dirs.sort();
        dirs.dedup_by(|a, b| {
            a.0 == b.0 && {
                b.1 |= a.1;
                true
            }
        });
        dirs
    }

    /// Why settings sync should stand down, if it should: a manifest path is
    /// a symlink, or another dotfile manager is in use.
    #[must_use]
    pub fn dormant(&self, home: &Path) -> Option<String> {
        // only Omarchy's own entries: Omacloud's additions skip links
        // instead (see `through_link`)
        for (_, pattern) in &self.entries[..self.omarchy] {
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

/// Omacloud's additions to the manifest, from [`BUNDLED`].
fn additions() -> &'static str {
    BUNDLED.find(ADDITIONS).map_or("", |i| &BUNDLED[i..])
}

/// Which group a shared setting belongs to, as `omacloud settings` and the
/// app list them: the shell and its dotfiles, AI agents, or the desktop and
/// apps.
#[must_use]
pub fn group_of(rel: &Path) -> &'static str {
    let mut parts = rel.components().map(|c| c.as_os_str().to_string_lossy());
    match (parts.next().as_deref(), parts.next().as_deref()) {
        (Some(".claude" | ".codex" | ".gemini" | ".cursor" | ".pi" | ".grok" | ".copilot"), _)
        | (Some(".config"), Some("opencode")) => "agents",
        (
            Some(".config"),
            Some("fish" | "git" | "nvim" | "mise" | "starship.toml" | "tmux" | "herdr"),
        ) => "shell",
        (Some(".config"), _) => "apps",
        _ => "shell",
    }
}

/// What a `**` entry takes: text files of at most this size, outside
/// directories of installed packages, version control and caches.
pub const DEEP: (u64, &[&str]) = (
    2 << 20,
    &[
        "node_modules",
        ".git",
        "__pycache__",
        "cache",
        ".cache",
        "caches",
    ],
);

/// Whether the file at `path` is text and small enough for a `**` entry.
fn plain_text(path: &Path) -> bool {
    use std::io::Read;
    let Ok(m) = fs::symlink_metadata(path) else {
        return false;
    };
    if !m.is_file() || m.len() > DEEP.0 {
        return false;
    }
    let mut head = Vec::new();
    fs::File::open(path)
        .and_then(|f| f.take(8192).read_to_end(&mut head))
        .is_ok_and(|_| !head.contains(&0))
}

/// A file name that holds a sign-in, a token or a key: never synced as a
/// setting, whatever a manifest says. Those go in the sealed secrets
/// bundle, or stay.
#[must_use]
pub fn secret_name(rel: &Path) -> bool {
    let name = rel
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    [
        "credential",
        "token",
        "oauth",
        "secret",
        "password",
        "cookie",
    ]
    .iter()
    .any(|w| name.contains(w))
        || name == "auth.json"
        || name.starts_with("auth.") && name.ends_with(".lock")
        || name == ".env"
        || name.starts_with(".env.")
        || [".pem", ".key", ".p12", ".pfx"]
            .iter()
            .any(|e| name.ends_with(e))
        || name.starts_with("id_rsa")
        || name.starts_with("id_ed25519")
}

fn components(pattern: &str) -> Vec<String> {
    pattern.split('/').map(str::to_string).collect()
}

/// Whether `rel` matches `pattern` component by component; a last `**`
/// takes any path below, but not through [`DEEP`]'s directories.
fn matches(pattern: &str, rel: &Path) -> bool {
    let pat = components(pattern);
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if pat.last().is_some_and(|p| p == "**") {
        let base = &pat[..pat.len() - 1];
        return parts.len() > base.len()
            && base.iter().zip(&parts).all(|(p, s)| glob(p, s))
            && !parts[base.len()..]
                .iter()
                .any(|s| DEEP.1.contains(&s.as_str()));
    }
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
    if first == "**" {
        // every file below, links neither taken nor followed
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let path = rel.join(&name);
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() && !DEEP.1.contains(&name.as_str()) {
                expand(home, &path, pattern, out);
            } else if ft.is_file() && plain_text(&home.join(&path)) {
                out.push(path);
            }
        }
        return;
    }
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

/// Directories under `home` matching every pattern component.
fn expand_dirs(home: &Path, rel: &Path, pattern: &[String], out: &mut Vec<PathBuf>) {
    let Some((first, rest)) = pattern.split_first() else {
        out.push(home.join(rel));
        return;
    };
    let Ok(entries) = fs::read_dir(home.join(rel)) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if glob(first, &name) && e.file_type().is_ok_and(|t| t.is_dir()) {
            expand_dirs(home, &rel.join(&name), rest, out);
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

    #[test]
    fn omacloud_adds_dotfiles_but_not_histories_or_tokens() -> Result<()> {
        let m = Manifest::parse(BUNDLED)?;
        for p in [
            ".bash_aliases",
            ".gitconfig",
            ".config/git/ignore",
            ".config/fish/config.fish",
            ".config/fish/functions/ll.fish",
            ".config/nvim/init.lua",
            ".config/nvim/lua/plugins/theme.lua",
            ".config/nvim/lua/a/b/deep.lua",
            ".config/mpv/mpv.conf",
            ".config/fastfetch/config.jsonc",
        ] {
            assert!(m.is_shared(Path::new(p)), "{p} should be shared");
        }
        for p in [
            ".bash_history",
            ".bash_profile",
            ".profile",
            ".config/fish/conf.d/installer.fish",
            ".config/fish/fish_variables",
            ".config/fish/fish_history",
            ".config/git/credentials",
            ".config/nvim/lazy-lock.json",
            ".npmrc",
        ] {
            assert!(!m.is_shared(Path::new(p)), "{p} should not be shared");
        }
        // over a manifest Omarchy ships, the additions still apply
        let omarchy = Manifest::parse(&format!("shared .bashrc\n{}", additions()))?;
        assert!(omarchy.is_shared(Path::new(".gitconfig")));
        assert!(!omarchy.is_shared(Path::new(".config/git/credentials")));
        assert!(!omarchy.is_shared(Path::new(".config/hypr/bindings.lua")));
        assert_eq!(group_of(Path::new(".config/fish/config.fish")), "shell");
        assert_eq!(group_of(Path::new(".config/hypr/bindings.lua")), "apps");
        Ok(())
    }

    #[test]
    fn watches_each_directory_a_pattern_reaches() -> Result<()> {
        let home = tempfile::tempdir()?;
        let h = home.path();
        fs::create_dir_all(h.join(".config/x/one"))?;
        fs::create_dir_all(h.join(".config/x/two"))?;
        let dirs = Manifest::parse("shared .config/x/*/*.conf\n")?.watch_dirs(h);
        for d in [".config/x/one", ".config/x/two"] {
            assert!(dirs.contains(&(h.join(d), false)), "{d} should be watched");
        }
        // Omarchy links Neovim's theme in: watched, skipped, and no reason
        // to stand down
        fs::create_dir_all(h.join(".config/nvim/lua/plugins"))?;
        fs::write(h.join(".config/nvim/lua/plugins/editor.lua"), "return {}\n")?;
        std::os::unix::fs::symlink(
            "/elsewhere/theme.lua",
            h.join(".config/nvim/lua/plugins/theme.lua"),
        )?;
        let m = Manifest::parse(BUNDLED)?;
        assert!(
            m.watch_dirs(h)
                .contains(&(h.join(".config/nvim/lua"), true))
        );
        assert_eq!(m.dormant(h), None);
        assert_eq!(
            m.shared_files(h),
            [PathBuf::from(".config/nvim/lua/plugins/editor.lua")]
        );
        assert!(!dirs.iter().any(|d| d.0.to_string_lossy().contains('*')));
        Ok(())
    }

    #[test]
    fn deep_entries_take_text_below_and_never_sign_ins() -> Result<()> {
        let home = tempfile::tempdir()?;
        let h = home.path();
        let put = |rel: &str, body: &[u8]| {
            let p = h.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, body).unwrap();
        };
        put(".claude/settings.json", b"{}");
        put(".claude/skills/review/SKILL.md", b"# Review\n");
        put(".claude/skills/review/scripts/run.sh", b"echo\n");
        put(".claude/skills/review/node_modules/x/index.js", b"x");
        put(".claude/skills/review/logo.png", b"\x89PNG\0\0");
        put(
            ".claude/skills/review/big.txt",
            &vec![b'a'; (DEEP.0 + 1) as usize],
        );
        put(".claude/skills/synced/managed/SKILL.md", b"managed\n");
        put(".claude/skills/review/token.json", b"{}");
        put(".claude/.credentials.json", b"{}");
        put(".claude/settings.local.json", b"{}");
        put(".claude/projects/x/session.jsonl", b"{}");
        put(".codex/auth.json", b"{}");
        put(".config/herdr/config.toml", b"x = 1\n");
        put(".config/herdr/session.json", b"{}");
        std::os::unix::fs::symlink("/etc", h.join(".claude/skills/outside"))?;
        let m = Manifest::parse(BUNDLED)?;
        let files = m.shared_files(h);
        let has = |p: &str| files.contains(&PathBuf::from(p));
        assert!(has(".claude/settings.json"));
        assert!(has(".claude/skills/review/SKILL.md"));
        assert!(has(".claude/skills/review/scripts/run.sh"));
        assert!(has(".config/herdr/config.toml"));
        for p in [
            ".claude/skills/review/node_modules/x/index.js",
            ".claude/skills/review/logo.png",
            ".claude/skills/review/big.txt",
            ".claude/skills/synced/managed/SKILL.md",
            ".claude/skills/review/token.json",
            ".claude/.credentials.json",
            ".claude/settings.local.json",
            ".claude/projects/x/session.jsonl",
            ".codex/auth.json",
            ".config/herdr/session.json",
        ] {
            assert!(!has(p), "{p} must not sync");
        }
        assert!(
            !files
                .iter()
                .any(|f| f.starts_with(".claude/skills/outside"))
        );
        // a link below a deep entry: neither synced nor a dotfile manager
        assert!(m.through_link(h, Path::new(".claude/skills/outside/passwd")));
        assert_eq!(m.dormant(h), None);
        // the backstop holds whatever an entry says
        let wide = Manifest::parse("shared .tool/**\n")?;
        for p in [
            ".tool/auth.json",
            ".tool/oauth_creds.json",
            ".tool/api-token",
            ".tool/.env",
            ".tool/x.pem",
        ] {
            assert!(!wide.is_shared(Path::new(p)), "{p}");
        }
        assert!(wide.is_shared(Path::new(".tool/keybindings.json")));
        // watched recursively
        assert!(m.watch_dirs(h).contains(&(h.join(".claude/skills"), true)));
        assert!(Manifest::parse("shared a/**/b\n").is_err());
        assert_eq!(
            group_of(Path::new(".claude/skills/review/SKILL.md")),
            "agents"
        );
        assert_eq!(
            group_of(Path::new(".config/opencode/opencode.json")),
            "agents"
        );
        assert_eq!(group_of(Path::new(".config/herdr/config.toml")), "shell");
        Ok(())
    }

    #[test]
    fn a_linked_agent_file_is_skipped_not_a_reason_to_stand_down() -> Result<()> {
        let home = tempfile::tempdir()?;
        let h = home.path();
        fs::write(h.join("AGENTS.md"), "shared notes\n")?;
        fs::create_dir_all(h.join(".claude"))?;
        std::os::unix::fs::symlink(h.join("AGENTS.md"), h.join(".claude/CLAUDE.md"))?;
        fs::create_dir_all(h.join("dots/codex"))?;
        fs::write(h.join("dots/codex/AGENTS.md"), "x\n")?;
        std::os::unix::fs::symlink(h.join("dots/codex"), h.join(".codex"))?;
        fs::write(h.join(".bashrc"), "x\n")?;
        let m = Manifest::parse(BUNDLED)?;
        assert_eq!(m.dormant(h), None);
        assert!(m.through_link(h, Path::new(".claude/CLAUDE.md")));
        assert!(m.through_link(h, Path::new(".codex/AGENTS.md")));
        assert!(!m.through_link(h, Path::new(".bashrc")));
        assert_eq!(
            m.linked(h),
            [".claude/CLAUDE.md", ".codex"].map(PathBuf::from)
        );
        assert_eq!(m.shared_files(h), [PathBuf::from(".bashrc")]);
        // a link among Omarchy's own still stands settings sync down
        fs::remove_file(h.join(".bashrc"))?;
        std::os::unix::fs::symlink(h.join("AGENTS.md"), h.join(".bashrc"))?;
        assert!(m.dormant(h).is_some());
        // and over a manifest Omarchy ships, the same
        let over = Manifest::parse(&format!("shared .bashrc\n{}", additions()))?;
        fs::remove_file(h.join(".bashrc"))?;
        assert_eq!(over.dormant(h), None);
        Ok(())
    }
}
