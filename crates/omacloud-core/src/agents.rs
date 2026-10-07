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

use crate::merged::{self, Concern, Group, Item, ME, Merged};

/// Each file: its document, where it is under home, and its kind.
const FILES: &[(&str, &str, Kind)] = &[
    ("codex/config.json", ".codex/config.toml", Kind::CodexToml),
    ("pi/settings.json", ".pi/agent/settings.json", Kind::PiJson),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Without its `projects` tables.
    CodexToml,
    /// Without `lastChangelogVersion`.
    PiJson,
}

/// The machine-only keys of pi's settings.
const PI_LOCAL: &[&str] = &["lastChangelogVersion"];

/// The files, as a [`Group`].
#[derive(Debug, Clone)]
pub struct Agents {
    home: PathBuf,
}

impl Agents {
    #[must_use]
    pub fn new(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
        }
    }

    fn file(doc: &str) -> Option<(&'static str, Kind)> {
        FILES
            .iter()
            .find(|(d, _, _)| *d == doc)
            .map(|(_, f, k)| (*f, *k))
    }

    /// A file of the home that's a link isn't ours to write through.
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
        conflicts.push(format!(
            "~/.codex/config.toml: not TOML on one side; kept {ME}'s"
        ));
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
        if let (Some(toml_edit::Item::Table(_)), Some(toml_edit::Item::Table(tt))) =
            (ours.get(&k), t)
        {
            let bt = b.and_then(toml_edit::Item::as_table);
            if let Some(ot) = ours.get_mut(&k).and_then(toml_edit::Item::as_table_mut) {
                merge_table(bt, ot, tt, &format!("{name}."), conflicts);
            }
            continue;
        }
        let (o, b, tv) = (ours.get(&k).map(plain), b.map(plain), t.map(plain));
        let clash = || {
            format!(
                "~/.codex/config.toml: {name} differs on {ME} and another computer; kept {ME}'s"
            )
        };
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
            Kind::PiJson => serde_json::from_str::<Value>(&text).ok().map(|mut v| {
                if let Some(m) = v.as_object_mut() {
                    for k in PI_LOCAL {
                        _ = m.remove(*k);
                    }
                }
                v
            }),
        })
    }

    fn busy(&self, _doc: &str) -> Option<String> {
        None
    }

    fn apply(
        &self,
        doc: &str,
        value: &Value,
        _applied: Option<&Value>,
        backups: &Path,
    ) -> Result<()> {
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
            Kind::PiJson => {
                let mut v = value.clone();
                let mine: Option<Value> =
                    here.as_deref().and_then(|h| serde_json::from_str(h).ok());
                if let (Some(m), Some(mine)) = (v.as_object_mut(), mine) {
                    for k in PI_LOCAL {
                        if let Some(x) = mine.get(*k) {
                            _ = m.insert((*k).to_string(), x.clone());
                        }
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
                let label = |k: &str| format!("pi setting {k}");
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
                    title: "Codex and pi settings".into(),
                    state: "unavailable".into(),
                    detail: "Linked in from elsewhere: left as they are".into(),
                    install: Vec::new(),
                    not_synced: linked,
                    scope: "agents/".into(),
                }]
            };
        }
        let names: Vec<&str> = docs
            .iter()
            .map(|d| {
                if d.starts_with("codex") {
                    "Codex"
                } else {
                    "pi"
                }
            })
            .collect();
        vec![Item {
            group: "agents".into(),
            title: format!("{} settings", names.join(" and ")),
            state: if waiting.is_empty() {
                "synced"
            } else {
                "waiting"
            }
            .into(),
            detail:
                "Synced without what belongs to this computer (trusted projects, version notes)"
                    .into(),
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
        let more =
            format!("{CODEX}\n[projects.\"/home/someone/new\"]\ntrust_level = \"trusted\"\n");
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
        g.apply(
            "codex/config.json",
            &m.value,
            None,
            &home.path().join("backups"),
        )?;
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
        assert_eq!(
            now,
            json!({"theme": "light", "lastChangelogVersion": "1.0.0"})
        );
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
        assert_eq!(
            fs::read_to_string(h.join("dots/codex/config.toml"))?,
            "model = \"x\"\n"
        );
        let items = g.status(h, &[]);
        assert_eq!(items[0].not_synced.len(), 2);
        Ok(())
    }
}
