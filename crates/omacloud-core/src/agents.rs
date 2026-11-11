//! AI agents' settings files with parts that belong to one machine, merged
//! across computers without them (a [`Group`] of merged documents).
//!
//! - Codex's `~/.codex/config.toml` holds a `[projects."<path>"]` table for
//!   each folder trusted on that computer: absolute paths, appended as you
//!   trust projects, that would collide between computers. They're taken
//!   out of what syncs, and each computer's own are put back when a change
//!   from elsewhere is written in. The rest keeps its comments and order
//!   (`toml_edit`).
//! - pi's `~/.pi/agent/settings.json` records `lastChangelogVersion`, which
//!   moves with each computer's updates: kept on each computer likewise.
//! - Claude Code's `~/.claude/settings.json` and Gemini's
//!   `~/.gemini/settings.json` hold `hooks`, a person's own beside those
//!   other programs put there, with nothing to say whose each is. A hook
//!   travels when everything it runs travels with it: its command names no
//!   file, or only files that sync as settings (a script in
//!   `~/.claude/hooks`). One naming a program's install, a checkout or a
//!   script a program manages stays on its computer.
//!
//! Everything else the agents keep goes through the dots manifest as plain
//! settings files.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};

use crate::{
    merged::{self, Concern, Group, Item, ME, Merged},
    settings::Manifest,
};

/// Each file: its document, where it is under home, and its kind.
const FILES: &[(&str, &str, Kind)] = &[
    ("codex/config.json", ".codex/config.toml", Kind::CodexToml),
    ("pi/settings.json", ".pi/agent/settings.json", Kind::Json(PI_LOCAL)),
    ("claude/settings.json", ".claude/settings.json", Kind::Hooked),
    ("gemini/settings.json", ".gemini/settings.json", Kind::Hooked),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Without its `projects` tables.
    CodexToml,
    /// JSON, without these machine-only keys.
    Json(&'static [&'static str]),
    /// JSON whose `hooks` travel only in part (see [`Agents::travels`]).
    Hooked,
}

/// The machine-only keys of pi's settings.
const PI_LOCAL: &[&str] = &["lastChangelogVersion"];

/// Where Claude Code's and Gemini's settings keep their hooks.
const HOOKS: &str = "hooks";

/// `hooks` (each event's groups, each group's handlers) as two: the handlers
/// `travels` passes, and the rest. A part with nothing in it is `None`;
/// hooks laid out any other way all stay.
fn split_hooks(hooks: &Value, travels: &dyn Fn(&Value) -> bool) -> (Option<Value>, Option<Value>) {
    let Some(events) = hooks.as_object() else {
        return (None, Some(hooks.clone()));
    };
    let (mut shared, mut local) = (serde_json::Map::new(), serde_json::Map::new());
    for (event, groups) in events {
        let Some(groups) = groups.as_array() else {
            _ = local.insert(event.clone(), groups.clone());
            continue;
        };
        let (mut go, mut stay) = (Vec::new(), Vec::new());
        for group in groups {
            let Some(handlers) = group.get(HOOKS).and_then(Value::as_array) else {
                stay.push(group.clone());
                continue;
            };
            let (a, b): (Vec<Value>, Vec<Value>) = handlers.iter().cloned().partition(|h| travels(h));
            for (part, into) in [(a, &mut go), (b, &mut stay)] {
                if !part.is_empty() {
                    let mut g = group.clone();
                    g[HOOKS] = Value::Array(part);
                    into.push(g);
                }
            }
        }
        for (part, into) in [(go, &mut shared), (stay, &mut local)] {
            if !part.is_empty() {
                _ = into.insert(event.clone(), Value::Array(part));
            }
        }
    }
    let some = |m: serde_json::Map<String, Value>| (!m.is_empty()).then_some(Value::Object(m));
    (some(shared), some(local))
}

/// The hooks that travel with this computer's own after them.
fn join_hooks(shared: Option<Value>, local: Option<Value>) -> Option<Value> {
    let (Some(mut all), Some(local)) = (shared.clone(), local.clone()) else {
        return shared.or(local);
    };
    let (Some(events), Some(mine)) = (all.as_object_mut(), local.as_object()) else {
        return Some(local);
    };
    for (event, groups) in mine {
        match (events.get_mut(event).and_then(Value::as_array_mut), groups.as_array()) {
            (Some(there), Some(groups)) => there.extend(groups.iter().cloned()),
            _ => _ = events.insert(event.clone(), groups.clone()),
        }
    }
    Some(all)
}

/// The agent a document belongs to, as people call it.
fn agent(doc: &str) -> &'static str {
    match doc.split('/').next() {
        Some("codex") => "Codex",
        Some("claude") => "Claude Code",
        Some("gemini") => "Gemini",
        _ => "pi",
    }
}

/// The files, as a [`Group`].
#[derive(Debug, Clone)]
pub struct Agents {
    home: PathBuf,
    /// Which files of home sync as settings: a hook travels with those.
    manifest: Manifest,
}

impl Agents {
    #[must_use]
    pub fn new(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
            manifest: Manifest::bundled(),
        }
    }

    /// With the manifest in use here, instead of the bundled one.
    #[must_use]
    pub fn with_manifest(mut self, manifest: Manifest) -> Self {
        self.manifest = manifest;
        self
    }

    /// Whether a hook's handler travels: one with no command to run (a
    /// prompt), or whose command names no file but ones that sync as
    /// settings. Any other path (a program's install, a checkout, a script a
    /// program manages) belongs to this computer, and so does the hook.
    /// Whether a file is here yet doesn't count: a script may arrive after
    /// the hook that runs it.
    fn travels(&self, handler: &Value) -> bool {
        let Some(command) = handler.get("command").and_then(Value::as_str) else {
            return true;
        };
        let home = self.home.to_string_lossy();
        command
            .split(|c: char| c.is_whitespace() || "'\"`;|&<>()=".contains(c))
            .filter_map(|word| {
                let rest = ["~/", "$HOME/", "${HOME}/"].iter().find_map(|p| word.strip_prefix(p));
                match rest {
                    Some(rest) => Some(format!("{home}/{rest}")),
                    None => word.starts_with('/').then(|| word.to_string()),
                }
            })
            .filter(|path| !["/dev/null", "/dev/stdout", "/dev/stderr"].contains(&path.as_str()))
            .all(|path| {
                let full = Path::new(&path);
                full.strip_prefix(&self.home).is_ok_and(|rel| {
                    self.manifest.is_shared(rel)
                        && !self.manifest.through_link(&self.home, rel)
                        && (!full.exists() || self.manifest.admits(rel, full))
                })
            })
    }

    fn file(doc: &str) -> Option<(&'static str, Kind)> {
        FILES.iter().find(|(d, _, _)| *d == doc).map(|(_, f, k)| (*f, *k))
    }

    /// A file of the home, unless it or a folder on the way to it is a link
    /// (`~/.codex` pointing at a dotfiles repo): that isn't ours to read or
    /// write through.
    fn path(&self, rel: &str) -> Option<PathBuf> {
        (!self.linked(rel)).then(|| self.home.join(rel))
    }

    fn linked(&self, rel: &str) -> bool {
        let mut at = self.home.clone();
        Path::new(rel).components().any(|c| {
            at.push(c);
            fs::symlink_metadata(&at).is_ok_and(|m| m.file_type().is_symlink())
        })
    }
}

/// Codex's config merged three ways: line by line where that settles,
/// otherwise table by table and key by key (two computers' first meeting,
/// with no common base: both sides' servers and settings are kept). A key
/// set differently on both keeps this computer's value, said in
/// `conflicts`.
#[must_use]
pub fn codex_merge(base: &str, ours: &str, theirs: &str, conflicts: &mut Vec<String>) -> String {
    if !base.is_empty()
        && let Some(m) = crate::settings::merge(base.as_bytes(), ours.as_bytes(), theirs.as_bytes())
            .and_then(|m| String::from_utf8(m).ok())
            .filter(|m| m.parse::<toml_edit::DocumentMut>().is_ok())
    {
        return m;
    }
    let parse = |t: &str| t.parse::<toml_edit::DocumentMut>().ok();
    let (Some(mut o), Some(t)) = (parse(ours), parse(theirs)) else {
        conflicts.push(format!("~/.codex/config.toml: not TOML on one side; kept {ME}'s"));
        return ours.to_string();
    };
    let b = parse(base);
    merge_table(
        b.as_ref().map(|b| b.as_table()),
        o.as_table_mut(),
        t.as_table(),
        "",
        conflicts,
    );
    o.to_string()
}

/// An item's value without its spacing and comments, to compare.
fn plain(i: &toml_edit::Item) -> String {
    match i {
        toml_edit::Item::Value(v) => {
            let mut v = v.clone();
            v.decor_mut().clear();
            v.to_string()
        }
        other => other.to_string(),
    }
}

fn merge_table(
    base: Option<&toml_edit::Table>,
    ours: &mut toml_edit::Table,
    theirs: &toml_edit::Table,
    path: &str,
    conflicts: &mut Vec<String>,
) {
    let mut keys: Vec<String> = ours.iter().map(|(k, _)| k.to_string()).collect();
    for k in theirs
        .iter()
        .map(|(k, _)| k)
        .chain(base.into_iter().flat_map(|b| b.iter().map(|(k, _)| k)))
    {
        if !keys.iter().any(|x| x == k) {
            keys.push(k.to_string());
        }
    }
    for k in keys {
        let b = base.and_then(|b| b.get(&k));
        let t = theirs.get(&k);
        let name = format!("{path}{k}");
        if let (Some(toml_edit::Item::Table(_)), Some(toml_edit::Item::Table(tt))) = (ours.get(&k), t) {
            let bt = b.and_then(toml_edit::Item::as_table);
            if let Some(ot) = ours.get_mut(&k).and_then(toml_edit::Item::as_table_mut) {
                merge_table(bt, ot, tt, &format!("{name}."), conflicts);
            }
            continue;
        }
        let (o, b, tv) = (ours.get(&k).map(plain), b.map(plain), t.map(plain));
        let clash = || format!("~/.codex/config.toml: {name} differs on {ME} and another computer; kept {ME}'s");
        match (o, tv) {
            (Some(o), Some(tv)) if o == tv => {}
            (Some(o), Some(_)) if Some(&o) == b.as_ref() => {
                _ = ours.insert(&k, t.expect("there").clone());
            }
            (Some(_), Some(tv)) if Some(&tv) == b.as_ref() => {}
            (Some(_), Some(_)) => conflicts.push(clash()),
            (Some(o), None) if b.is_some() && Some(&o) == b.as_ref() => {
                _ = ours.remove(&k);
            }
            (Some(_), None) => {}
            (None, Some(tv)) if b.as_ref() == Some(&tv) => {}
            (None, Some(_)) => {
                _ = ours.insert(&k, t.expect("there").clone());
            }
            (None, None) => {}
        }
    }
}

/// Codex's config without its `projects` tables, `None` if it isn't TOML.
#[must_use]
pub fn codex_shared(text: &str) -> Option<String> {
    let mut doc: toml_edit::DocumentMut = text.parse().ok()?;
    _ = doc.remove("projects");
    Some(doc.to_string())
}

/// `shared` with the `projects` tables of `here` (this computer's file).
///
/// # Errors
///
/// When `shared` isn't TOML.
pub fn codex_here(shared: &str, here: Option<&str>) -> Result<String> {
    let mut doc: toml_edit::DocumentMut = shared.parse().context("synced Codex config")?;
    let projects = here
        .and_then(|h| h.parse::<toml_edit::DocumentMut>().ok())
        .and_then(|mut h| h.remove("projects"));
    _ = doc.remove("projects");
    if let Some(p) = projects {
        _ = doc.insert("projects", p);
    }
    Ok(doc.to_string())
}

impl Group for Agents {
    fn name(&self) -> &str {
        "agents"
    }

    fn docs(&self) -> Vec<String> {
        FILES
            .iter()
            .filter(|(_, f, _)| self.path(f).is_some_and(|p| p.is_file()))
            .map(|(d, _, _)| (*d).to_string())
            .collect()
    }

    fn capture(&self, doc: &str, _applied: Option<&Value>) -> Result<Option<Value>> {
        let Some((rel, kind)) = Self::file(doc) else {
            return Ok(None);
        };
        let Some(text) = self.path(rel).and_then(|p| fs::read_to_string(p).ok()) else {
            return Ok(None);
        };
        // mid-write or damaged: look again next time
        Ok(match kind {
            Kind::CodexToml => codex_shared(&text).map(|t| json!({ "text": t })),
            Kind::Json(local) => serde_json::from_str::<Value>(&text).ok().map(|mut v| {
                if let Some(m) = v.as_object_mut() {
                    for k in local {
                        _ = m.remove(*k);
                    }
                }
                v
            }),
            Kind::Hooked => serde_json::from_str::<Value>(&text).ok().map(|mut v| {
                if let Some(m) = v.as_object_mut()
                    && let Some(hooks) = m.remove(HOOKS)
                    && let (Some(shared), _) = split_hooks(&hooks, &|h| self.travels(h))
                {
                    _ = m.insert(HOOKS.to_string(), shared);
                }
                v
            }),
        })
    }

    fn busy(&self, _doc: &str) -> Option<String> {
        None
    }

    fn apply(&self, doc: &str, value: &Value, _applied: Option<&Value>, backups: &Path) -> Result<()> {
        let (rel, kind) = Self::file(doc).ok_or_else(|| anyhow!("no agent file for {doc}"))?;
        let path = self
            .path(rel)
            .ok_or_else(|| anyhow!("~/{rel} is a link; left as it is"))?;
        let here = fs::read_to_string(&path).ok();
        let new = match kind {
            Kind::CodexToml => codex_here(
                value
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("synced Codex config has no text"))?,
                here.as_deref(),
            )?,
            Kind::Json(local) => {
                let mut v = value.clone();
                let mine: Option<Value> = here.as_deref().and_then(|h| serde_json::from_str(h).ok());
                if let (Some(m), Some(mine)) = (v.as_object_mut(), mine) {
                    // whatever another computer's older version sent along
                    for k in local {
                        _ = m.remove(*k);
                    }
                    for k in local {
                        if let Some(x) = mine.get(*k) {
                            _ = m.insert((*k).to_string(), x.clone());
                        }
                    }
                }
                let mut t = serde_json::to_string_pretty(&v)?;
                t.push('\n');
                t
            }
            Kind::Hooked => {
                let mut v = value.clone();
                let mine: Option<Value> = here.as_deref().and_then(|h| serde_json::from_str(h).ok());
                if let Some(m) = v.as_object_mut() {
                    // this computer's own hooks stay, after the ones that travel
                    let travels = |h: &Value| self.travels(h);
                    let shared = m.remove(HOOKS).and_then(|h| split_hooks(&h, &travels).0);
                    let local = mine
                        .as_ref()
                        .and_then(|mine| mine.get(HOOKS))
                        .and_then(|h| split_hooks(h, &travels).1);
                    if let Some(hooks) = join_hooks(shared, local) {
                        _ = m.insert(HOOKS.to_string(), hooks);
                    }
                }
                let mut t = serde_json::to_string_pretty(&v)?;
                t.push('\n');
                t
            }
        };
        if here.as_deref() == Some(new.as_str()) {
            return Ok(());
        }
        merged::back_up(backups, Path::new(rel), &path)?;
        let mode = fs::metadata(&path).map_or(0o600, |m| m.permissions().mode() & 0o777);
        merged::write_atomic(&path, new.as_bytes(), mode)
    }

    fn merge(&self, doc: &str, base: Option<&Value>, ours: &Value, theirs: &Value) -> Merged {
        let mut conflicts = Vec::new();
        match Self::file(doc) {
            Some((_, Kind::CodexToml)) => {
                let text = |v: Option<&Value>| {
                    v.and_then(|v| v.get("text"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string()
                };
                let (b, o, t) = (text(base), text(Some(ours)), text(Some(theirs)));
                let value = codex_merge(&b, &o, &t, &mut conflicts);
                Merged {
                    value: json!({ "text": value }),
                    conflicts,
                }
            }
            _ => {
                let map = |v: &Value| v.as_object().cloned().unwrap_or_default();
                let label = |k: &str| format!("{} setting {k}", agent(doc));
                let value = merged::merge_map(
                    base.and_then(Value::as_object),
                    &map(ours),
                    &map(theirs),
                    false,
                    &label,
                    &mut conflicts,
                );
                Merged {
                    value: Value::Object(value),
                    conflicts,
                }
            }
        }
    }

    fn status(&self, _mirror: &Path, waiting: &[(String, String)]) -> Vec<Item> {
        let docs = self.docs();
        let linked: Vec<String> = FILES
            .iter()
            .filter(|(_, f, _)| self.linked(f))
            .map(|(_, f, _)| format!("~/{f}: a link, left as it is"))
            .collect();
        if docs.is_empty() {
            return if linked.is_empty() {
                Vec::new()
            } else {
                vec![Item {
                    group: "agents".into(),
                    title: "Agents' settings".into(),
                    state: "unavailable".into(),
                    detail: "Linked in from elsewhere: left as they are".into(),
                    install: Vec::new(),
                    not_synced: linked,
                    scope: "agents/".into(),
                }]
            };
        }
        let names: Vec<&str> = docs.iter().map(|d| agent(d)).collect();
        vec![Item {
            group: "agents".into(),
            title: format!("{} settings", names.join(", ")),
            state: if waiting.is_empty() { "synced" } else { "waiting" }.into(),
            detail: "Synced without what belongs to this computer (trusted projects, version notes)".into(),
            install: Vec::new(),
            not_synced: linked,
            scope: "agents/".into(),
        }]
    }

    fn watch_dirs(&self) -> Vec<PathBuf> {
        FILES
            .iter()
            .filter_map(|(_, f, _)| self.home.join(f).parent().map(Path::to_path_buf))
            .collect()
    }

    fn concerns(&self, path: &Path) -> Concern {
        if FILES.iter().any(|(_, f, _)| self.home.join(f) == path) {
            Concern::Look
        } else {
            Concern::No
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODEX: &str = r#"# my model
model = "gpt-5"   # the default

[projects."/home/someone/work"]
trust_level = "trusted"

[mcp_servers.docs]
url = "https://mcp.example.com"

[projects."/home/someone/play"]
trust_level = "trusted"
"#;

    #[test]
    fn codex_trust_stays_on_each_computer() -> Result<()> {
        let shared = codex_shared(CODEX).unwrap();
        assert!(!shared.contains("projects"));
        assert!(shared.contains("# my model") && shared.contains("# the default"));
        assert!(shared.contains("[mcp_servers.docs]"));
        // another computer's change to the model, written here: this
        // computer's projects come back
        let theirs = shared.replace("gpt-5", "gpt-6");
        let here = codex_here(&theirs, Some(CODEX))?;
        assert!(here.contains("gpt-6"));
        assert!(here.contains("[projects.\"/home/someone/work\"]"));
        assert!(here.contains("[projects.\"/home/someone/play\"]"));
        assert_eq!(codex_shared(&here).unwrap(), theirs);

        // trusting a project here isn't a change to sync
        let home = tempfile::tempdir()?;
        fs::create_dir_all(home.path().join(".codex"))?;
        fs::write(home.path().join(".codex/config.toml"), CODEX)?;
        let g = Agents::new(home.path());
        let before = g.capture("codex/config.json", None)?.unwrap();
        let more = format!("{CODEX}\n[projects.\"/home/someone/new\"]\ntrust_level = \"trusted\"\n");
        fs::write(home.path().join(".codex/config.toml"), more)?;
        assert_eq!(g.capture("codex/config.json", None)?.unwrap(), before);
        // two computers editing different lines merge
        let base = json!({ "text": shared });
        let o = json!({ "text": shared.replace("gpt-5", "gpt-6") });
        let t = json!({ "text": shared.replace("mcp.example.com", "mcp2.example.com") });
        let m = g.merge("codex/config.json", Some(&base), &o, &t);
        assert!(m.conflicts.is_empty());
        let text = m.value["text"].as_str().unwrap();
        assert!(text.contains("gpt-6") && text.contains("mcp2.example.com"));
        // and writing it keeps this computer's trust
        g.apply("codex/config.json", &m.value, None, &home.path().join("backups"))?;
        let now = fs::read_to_string(home.path().join(".codex/config.toml"))?;
        assert!(now.contains("gpt-6") && now.contains("/home/someone/new"));
        Ok(())
    }

    #[test]
    fn pis_version_note_stays() -> Result<()> {
        let home = tempfile::tempdir()?;
        let p = home.path().join(".pi/agent/settings.json");
        fs::create_dir_all(p.parent().unwrap())?;
        fs::write(&p, r#"{"theme": "dark", "lastChangelogVersion": "1.0.0"}"#)?;
        let g = Agents::new(home.path());
        let v = g.capture("pi/settings.json", None)?.unwrap();
        assert_eq!(v, json!({"theme": "dark"}));
        g.apply(
            "pi/settings.json",
            &json!({"theme": "light"}),
            None,
            &home.path().join("b"),
        )?;
        let now: Value = serde_json::from_str(&fs::read_to_string(&p)?)?;
        assert_eq!(now, json!({"theme": "light", "lastChangelogVersion": "1.0.0"}));
        Ok(())
    }

    #[test]
    fn two_computers_first_codex_configs_are_both_kept() {
        let a = "model = \"gpt-5\"\n\n[mcp_servers.docs]\nurl = \"https://a.example.com\"\n";
        let b = "model = \"gpt-6\"\n\n[mcp_servers.tracker]\nurl = \"https://b.example.com\"\n";
        let mut conflicts = Vec::new();
        let m = codex_merge("", a, b, &mut conflicts);
        assert!(
            m.contains("[mcp_servers.docs]") && m.contains("[mcp_servers.tracker]"),
            "{m}"
        );
        assert!(m.contains("gpt-5") && !m.contains("gpt-6"));
        assert_eq!(conflicts.len(), 1);
        assert!(conflicts[0].contains("model"));
        // and with a base, a delete on one side goes through
        let base = a;
        let ours = "model = \"gpt-5\"\n";
        let mut c = Vec::new();
        let m = codex_merge(base, ours, a, &mut c);
        assert!(!m.contains("mcp_servers"), "{m}");
    }

    #[test]
    fn linked_agent_folders_are_left_alone() -> Result<()> {
        let home = tempfile::tempdir()?;
        let h = home.path();
        fs::create_dir_all(h.join("dots/codex"))?;
        fs::write(h.join("dots/codex/config.toml"), "model = \"x\"\n")?;
        std::os::unix::fs::symlink(h.join("dots/codex"), h.join(".codex"))?;
        fs::create_dir_all(h.join("dots/pi"))?;
        fs::write(h.join("dots/pi/settings.json"), "{}")?;
        fs::create_dir_all(h.join(".pi"))?;
        std::os::unix::fs::symlink(h.join("dots/pi"), h.join(".pi/agent"))?;
        let g = Agents::new(h);
        assert!(g.docs().is_empty());
        assert_eq!(g.capture("codex/config.json", None)?, None);
        assert!(
            g.apply(
                "codex/config.json",
                &json!({"text": "model = \"y\"\n"}),
                None,
                &h.join("b")
            )
            .is_err()
        );
        assert_eq!(fs::read_to_string(h.join("dots/codex/config.toml"))?, "model = \"x\"\n");
        let items = g.status(h, &[]);
        assert_eq!(items[0].not_synced.len(), 2);
        Ok(())
    }

    #[test]
    fn a_hook_travels_when_what_it_runs_does() -> Result<()> {
        let home = tempfile::tempdir()?;
        let h = home.path().to_string_lossy().into_owned();
        let file = home.path().join(".claude/settings.json");
        fs::create_dir_all(home.path().join(".claude/hooks"))?;
        fs::write(home.path().join(".claude/hooks/mine.sh"), "#!/bin/sh\necho mine\n")?;
        fs::write(
            home.path().join(".claude/hooks/theirs.sh"),
            "#!/bin/sh\n# managed by a program; updating overwrites this file.\n",
        )?;
        let hook = |command: &str| json!({ "type": "command", "command": command });
        let mine = hook(&format!("bash '{h}/.claude/hooks/mine.sh' stop"));
        let bare = hook("notify-send done > /dev/null");
        let prompt = json!({ "type": "prompt", "prompt": "Check the work." });
        let managed = hook(&format!("bash '{h}/.claude/hooks/theirs.sh' session"));
        let installed = hook("node '/usr/lib/a-program/hook.mjs' claude");
        let checkout = hook(&format!("LOG='{h}/.local/state/x.log' node {h}/code/tool/hook.mjs"));
        let here = json!({
            "model": "opus",
            "hooks": {
                "Stop": [
                    { "hooks": [installed.clone(), mine.clone()] },
                    { "matcher": "x", "hooks": [bare.clone(), prompt.clone()] },
                ],
                "SessionStart": [{ "hooks": [managed.clone(), checkout.clone()] }],
            },
        });
        fs::write(&file, serde_json::to_string(&here)?)?;
        let g = Agents::new(home.path());
        let shared = g.capture("claude/settings.json", None)?.unwrap();
        assert_eq!(
            shared,
            json!({
                "model": "opus",
                "hooks": { "Stop": [{ "hooks": [mine] }, { "matcher": "x", "hooks": [bare, prompt] }] },
            })
        );
        // another computer's model and a hook of its person's own arrive:
        // this computer's own hooks stay, and nothing is taken for a change
        let theirs_own = hook("notify-send started");
        let mut theirs = shared.clone();
        theirs["model"] = json!("sonnet");
        theirs["hooks"]["SessionStart"] = json!([{ "hooks": [theirs_own] }]);
        g.apply("claude/settings.json", &theirs, Some(&shared), home.path())?;
        let now: Value = serde_json::from_str(&fs::read_to_string(&file)?)?;
        assert_eq!(now["model"], "sonnet");
        let text = now["hooks"].to_string();
        for kept in [
            "/usr/lib/a-program/hook.mjs",
            "theirs.sh",
            "code/tool/hook.mjs",
            "notify-send started",
        ] {
            assert!(text.contains(kept), "{kept} missing from {text}");
        }
        assert_eq!(g.capture("claude/settings.json", None)?.unwrap(), theirs);
        // hooks laid out some other way all stay
        fs::write(&file, r#"{"hooks":["odd"]}"#)?;
        assert_eq!(g.capture("claude/settings.json", None)?.unwrap(), json!({}));
        Ok(())
    }
}
