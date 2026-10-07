//! Chromium's bookmarks as a mergeable document.
//!
//! `<profile>/Bookmarks` is JSON (components/bookmarks/browser/
//! bookmark_codec.cc): `roots` holds `bookmark_bar`, `other` and `synced`,
//! each a folder; every node has an `id` (unique in the file), a `guid`
//! (stable everywhere), `name`, `type` (`url` or `folder`), `date_added`,
//! and `url` or `children`. The document keeps each node by guid, with its
//! parent and, for folders, its children in order; the roots go by their
//! key. What it doesn't model (ids, `date_last_used`, `meta_info`, the file's
//! `sync_metadata`) stays as each computer has it.
//!
//! The file's `checksum` is an MD5 over each node's id, title (UTF-16) and
//! type, and a url's spec, roots first, depth first (`EncodeNode`,
//! `UpdateChecksumWith*Node`). Chromium writes it and, since it stopped
//! reading it back (`Decode` ignores it; see `model_loader.cc`), doesn't
//! check it on load; older versions that did only saved the file again on a
//! mismatch. Omacloud recomputes it, and `checksum_sha256` when the file has
//! one, so the file reads as Chromium itself would have written it.

use std::collections::{BTreeMap, BTreeSet};

use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::Sha256;

use crate::merged::{ME, Merged};

/// The roots' keys in the file, in the order Chromium writes them.
pub const ROOTS: [&str; 3] = ["bookmark_bar", "other", "synced"];

/// The roots' fixed guids (components/bookmarks/browser/bookmark_uuids.cc).
const ROOT_GUIDS: [&str; 3] = [
    "0bc5d13f-2cba-5d74-951f-3f233fe6c908",
    "82b081ec-3dd3-529c-8475-ab6c344590dd",
    "4cf2e351-0e85-532b-bb37-df045d8f8d0f",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Node {
    /// A root's key or a folder's guid; `None` for a root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// `url` or `folder` (roots are folders).
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date_added: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<String>,
}

impl Node {
    /// What an edit changes, beside where it sits among its siblings.
    fn content(&self) -> (&Option<String>, &str, &str, &Option<String>) {
        (&self.parent, &self.kind, &self.name, &self.url)
    }
}

pub type Doc = BTreeMap<String, Node>;

/// The document of a `Bookmarks` file.
#[must_use]
pub fn capture(file: &Value) -> Doc {
    fn walk(v: &Value, parent: &str, doc: &mut Doc, children: &mut Vec<String>) {
        let Some(guid) = v.get("guid").and_then(Value::as_str) else {
            return; // Chromium gives every node one; without it, no identity
        };
        let guid = guid.to_ascii_lowercase();
        if doc.contains_key(&guid) || ROOTS.contains(&guid.as_str()) {
            return; // a duplicate Chromium would give a new guid on load
        }
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("url");
        let mut node = Node {
            parent: Some(parent.to_string()),
            kind: kind.to_string(),
            name: str_of(v, "name"),
            url: (kind == "url").then(|| str_of(v, "url")),
            date_added: v
                .get("date_added")
                .and_then(Value::as_str)
                .map(str::to_string),
            children: Vec::new(),
        };
        children.push(guid.clone());
        let mut mine = Vec::new();
        _ = doc.insert(guid.clone(), Node::default());
        if kind == "folder" {
            for c in v
                .get("children")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                walk(c, &guid, doc, &mut mine);
            }
        }
        node.children = mine;
        _ = doc.insert(guid, node);
    }
    let mut doc = Doc::new();
    for root in ROOTS {
        let mut children = Vec::new();
        if let Some(r) = file.pointer(&format!("/roots/{root}")) {
            for c in r
                .get("children")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                walk(c, root, &mut doc, &mut children);
            }
        }
        _ = doc.insert(
            root.to_string(),
            Node {
                kind: "folder".into(),
                children,
                ..Node::default()
            },
        );
    }
    doc
}

fn str_of(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Three way merge of bookmark documents, node by node (keyed by guid).
///
/// - Adds, deletes, renames, moves and reorders on either side all land.
/// - The same field (name, url, folder) changed on both sides: this
///   computer's (`ours`) wins, and a line in `conflicts` says so.
/// - Deleted on one side and changed on the other: kept. A folder deleted on
///   one side that the other added to or changed something in comes back.
/// - Folders reordered on both sides take this computer's order, with the
///   other side's additions placed after the bookmark they followed there.
/// - Moves on both sides that would put a folder inside itself: this
///   computer's moves win.
/// - With no common base (two computers' first sync), a bookmark both have,
///   same folder, name and url, counts once.
#[must_use]
pub fn merge(base: Option<&Doc>, ours: &Doc, theirs: &Doc) -> (Doc, Vec<String>) {
    let mut conflicts = Vec::new();
    let empty = Doc::new();
    let theirs = if base.is_none_or(Doc::is_empty) {
        alias_same(ours, theirs)
    } else {
        theirs.clone()
    };
    let base = base.unwrap_or(&empty);
    let label = |id: &str, n: &Node| {
        if ROOTS.contains(&id) {
            format!("bookmarks folder {id}")
        } else {
            format!("bookmark \"{}\"", n.name)
        }
    };

    // each node: kept or not, and its fields
    let ids: BTreeSet<&String> = ours
        .keys()
        .chain(theirs.keys())
        .chain(base.keys())
        .collect();
    let mut out = Doc::new();
    for id in &ids {
        let (b, o, t) = (base.get(*id), ours.get(*id), theirs.get(*id));
        let node = match (o, t) {
            (Some(o), Some(t)) => Some(merge_node(id, b, o, t, &label, &mut conflicts)),
            (Some(o), None) => match b {
                None => Some(o.clone()),
                Some(b) if b.content() == o.content() => None,
                Some(_) => {
                    conflicts.push(format!(
                        "{}: changed on {ME}, deleted on another computer; kept",
                        label(id, o)
                    ));
                    Some(o.clone())
                }
            },
            (None, Some(t)) => match b {
                None => Some(t.clone()),
                Some(b) if b.content() == t.content() => None,
                Some(_) => {
                    conflicts.push(format!(
                        "{}: deleted on {ME}, changed on another computer; kept",
                        label(id, t)
                    ));
                    Some(t.clone())
                }
            },
            (None, None) => None,
        };
        if let Some(n) = node {
            _ = out.insert((*id).clone(), n);
        }
    }
    for root in ROOTS {
        if !out.contains_key(root) {
            let n = ours
                .get(root)
                .or(theirs.get(root))
                .cloned()
                .unwrap_or(Node {
                    kind: "folder".into(),
                    ..Node::default()
                });
            _ = out.insert(root.to_string(), n);
        }
    }

    // a kept node's folder stays too, from whichever side still has it
    loop {
        let orphans: Vec<(String, String)> = out
            .iter()
            .filter_map(|(id, n)| {
                let p = n.parent.as_ref()?;
                (!out.contains_key(p)).then(|| (id.clone(), p.clone()))
            })
            .collect();
        if orphans.is_empty() {
            break;
        }
        for (id, p) in orphans {
            if let Some(n) = ours.get(&p).or(theirs.get(&p)).or(base.get(&p)) {
                if !out.contains_key(&p) {
                    conflicts.push(format!(
                        "{}: deleted on one computer while the other added to it; kept",
                        label(&p, n)
                    ));
                    _ = out.insert(p.clone(), n.clone());
                }
            } else if let Some(n) = out.get_mut(&id) {
                n.parent = Some("other".into());
            }
        }
    }

    // moves on both sides that make a cycle: this computer's moves win
    for _ in 0..out.len() {
        let Some(cycle) = find_cycle(&out) else { break };
        for id in cycle {
            let back = ours
                .get(&id)
                .and_then(|n| n.parent.clone())
                .unwrap_or_else(|| "other".into());
            if let Some(n) = out.get_mut(&id) {
                n.parent = Some(back);
            }
        }
        conflicts.push(
            "bookmarks moved into each other on {ME} and another computer; kept {ME}'s moves"
                .into(),
        );
    }
    // can't happen (ours has no cycles), but never loop: park under other
    while let Some(cycle) = find_cycle(&out) {
        if let Some(n) = out.get_mut(&cycle[0]) {
            n.parent = Some("other".into());
        }
    }

    // each folder's children, in a merged order
    let mut members: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (id, n) in &out {
        if let Some(p) = &n.parent {
            _ = members.entry(p.clone()).or_default().insert(id.clone());
        }
    }
    let folders: Vec<String> = out
        .iter()
        .filter(|(_, n)| n.kind == "folder")
        .map(|(id, _)| id.clone())
        .collect();
    for f in folders {
        let m = members.remove(&f).unwrap_or_default();
        let seq = |d: &Doc| d.get(&f).map(|n| n.children.clone()).unwrap_or_default();
        let order = merge_order(&seq(base), &seq(ours), &seq(&theirs), &m);
        if let Some(n) = out.get_mut(&f) {
            n.children = order;
        }
    }
    for n in out.values_mut() {
        if n.kind != "folder" {
            n.children.clear();
        }
    }
    (out, conflicts)
}

fn merge_node(
    id: &str,
    b: Option<&Node>,
    o: &Node,
    t: &Node,
    label: &dyn Fn(&str, &Node) -> String,
    conflicts: &mut Vec<String>,
) -> Node {
    let mut n = o.clone();
    let mut clash = Vec::new();
    macro_rules! pick {
        ($f:ident, $what:literal) => {
            let (bf, of, tf) = (b.map(|b| &b.$f), &o.$f, &t.$f);
            if of != tf {
                if Some(of) == bf {
                    n.$f = tf.clone();
                } else if Some(tf) != bf {
                    clash.push($what);
                }
            }
        };
    }
    pick!(parent, "folder");
    pick!(kind, "type");
    pick!(name, "name");
    pick!(url, "url");
    pick!(date_added, "date added");
    if clash.contains(&"name") {
        // the name that gave way, so it can be put back by hand
        clash.retain(|c| *c != "name");
        clash.insert(0, "name");
        conflicts.push(format!(
            "{}: {} changed on {ME} and on another computer (\"{}\" there); kept {ME}'s",
            label(id, o),
            clash.join(" and "),
            t.name
        ));
    } else if !clash.is_empty() {
        conflicts.push(format!(
            "{}: {} changed on {ME} and on another computer; kept {ME}'s",
            label(id, o),
            clash.join(" and ")
        ));
    }
    n
}

/// A folder's children: the side that reordered (this computer's when both
/// did) gives the order, and the other side's additions go after the
/// sibling they followed there.
fn merge_order(
    base: &[String],
    ours: &[String],
    theirs: &[String],
    members: &BTreeSet<String>,
) -> Vec<String> {
    let reordered = |side: &[String]| {
        let common: Vec<&String> = side.iter().filter(|x| base.contains(x)).collect();
        let was: Vec<&String> = base.iter().filter(|x| side.contains(x)).collect();
        common != was
    };
    let (skeleton, other) = if reordered(theirs) && !reordered(ours) {
        (theirs, ours)
    } else {
        (ours, theirs)
    };
    let mut out: Vec<String> = Vec::new();
    for x in skeleton {
        if members.contains(x) && !out.contains(x) {
            out.push(x.clone());
        }
    }
    let mut prev: Option<&String> = None;
    for x in other {
        if members.contains(x) && !out.contains(x) {
            let at = prev
                .and_then(|p| out.iter().position(|y| y == p))
                .map_or(0, |i| i + 1);
            out.insert(at, x.clone());
        }
        if out.contains(x) {
            prev = Some(x);
        }
    }
    for x in members {
        if !out.contains(x) {
            out.push(x.clone());
        }
    }
    out
}

/// Nodes on a parent cycle, if any.
fn find_cycle(doc: &Doc) -> Option<Vec<String>> {
    for start in doc.keys() {
        let mut seen = Vec::new();
        let mut at = start.clone();
        while let Some(p) = doc.get(&at).and_then(|n| n.parent.clone()) {
            if let Some(i) = seen.iter().position(|s| *s == at) {
                return Some(seen[i..].to_vec());
            }
            seen.push(at.clone());
            at = p;
        }
    }
    None
}

/// With no base, `theirs` with each node that matches one only in `ours`
/// (same folder, type, name and url) under `ours`'s guid.
fn alias_same(ours: &Doc, theirs: &Doc) -> Doc {
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    // parents first: walk from the roots
    let mut queue: Vec<String> = ROOTS.iter().map(|r| (*r).to_string()).collect();
    let mut taken: BTreeSet<String> = BTreeSet::new();
    while let Some(tp) = queue.pop() {
        let op = map.get(&tp).cloned().unwrap_or_else(|| tp.clone());
        let Some(tn) = theirs.get(&tp) else { continue };
        for c in &tn.children {
            queue.push(c.clone());
            if ours.contains_key(c) {
                continue;
            }
            let Some(t) = theirs.get(c) else { continue };
            let same = ours
                .get(&op)
                .into_iter()
                .flat_map(|n| &n.children)
                .find(|oc| {
                    !theirs.contains_key(*oc)
                        && !taken.contains(*oc)
                        && ours
                            .get(*oc)
                            .is_some_and(|o| o.kind == t.kind && o.name == t.name && o.url == t.url)
                });
            if let Some(oc) = same {
                _ = taken.insert(oc.clone());
                _ = map.insert(c.clone(), oc.clone());
            }
        }
    }
    let rename = |id: &String| map.get(id).cloned().unwrap_or_else(|| id.clone());
    theirs
        .iter()
        .map(|(id, n)| {
            let mut n = n.clone();
            n.parent = n.parent.as_ref().map(rename);
            n.children = n.children.iter().map(rename).collect();
            (rename(id), n)
        })
        .collect()
}

/// `file` (the profile's `Bookmarks`, or `None` for none yet) with the
/// document's bookmarks in it. Nodes already there keep what the document
/// doesn't model; new ones get fresh ids. The checksum is recomputed.
#[must_use]
pub fn write(file: Option<&Value>, doc: &Doc) -> Value {
    let mut old: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
    let mut max_id: u64 = 0;
    fn index(v: &Value, old: &mut BTreeMap<String, Map<String, Value>>, max_id: &mut u64) {
        if let Some(id) = v
            .get("id")
            .and_then(Value::as_str)
            .and_then(|s| s.parse().ok())
        {
            *max_id = (*max_id).max(id);
        }
        if let (Some(g), Some(m)) = (v.get("guid").and_then(Value::as_str), v.as_object()) {
            _ = old
                .entry(g.to_ascii_lowercase())
                .or_insert_with(|| m.clone());
        }
        for c in v
            .get("children")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            index(c, old, max_id);
        }
    }
    let mut main = file.and_then(Value::as_object).cloned().unwrap_or_default();
    if let Some(roots) = main.get("roots") {
        for r in ROOTS {
            if let Some(v) = roots.get(r) {
                index(v, &mut old, &mut max_id);
            }
        }
    }
    let mut used: BTreeSet<u64> = BTreeSet::new();
    let mut next_id = || {
        max_id += 1;
        max_id
    };

    struct Ctx<'a> {
        doc: &'a Doc,
        old: &'a BTreeMap<String, Map<String, Value>>,
    }
    fn build(
        ctx: &Ctx,
        guid: &str,
        used: &mut BTreeSet<u64>,
        next_id: &mut dyn FnMut() -> u64,
    ) -> Option<Value> {
        let n = ctx.doc.get(guid)?;
        let mut m = ctx.old.get(guid).cloned().unwrap_or_default();
        let id = m
            .get("id")
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|id| *id > 0 && !used.contains(id))
            .unwrap_or_else(&mut *next_id);
        _ = used.insert(id);
        _ = m.insert("id".into(), json!(id.to_string()));
        _ = m.insert("guid".into(), json!(guid));
        _ = m.insert("name".into(), json!(n.name));
        _ = m.insert("type".into(), json!(n.kind));
        let added = n.date_added.clone().unwrap_or_else(|| "0".into());
        _ = m.entry("date_added").or_insert_with(|| json!(added));
        if n.date_added.is_some() {
            _ = m.insert("date_added".into(), json!(added));
        }
        _ = m.entry("date_last_used").or_insert_with(|| json!("0"));
        if n.kind == "folder" {
            _ = m.remove("url");
            _ = m.entry("date_modified").or_insert_with(|| json!("0"));
            let children: Vec<Value> = n
                .children
                .iter()
                .filter_map(|c| build(ctx, c, used, next_id))
                .collect();
            _ = m.insert("children".into(), Value::Array(children));
        } else {
            _ = m.remove("children");
            _ = m.remove("date_modified");
            _ = m.insert("url".into(), json!(n.url.clone().unwrap_or_default()));
        }
        Some(Value::Object(m))
    }

    let ctx = Ctx { doc, old: &old };
    let old_roots = main
        .get("roots")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut roots = old_roots.clone();
    for (root, guid) in ROOTS.iter().zip(ROOT_GUIDS) {
        let mut r = old_roots
            .get(*root)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_else(|| {
                let mut m = Map::new();
                _ = m.insert("date_added".into(), json!("0"));
                _ = m.insert("date_last_used".into(), json!("0"));
                _ = m.insert("date_modified".into(), json!("0"));
                _ = m.insert("guid".into(), json!(guid));
                _ = m.insert("name".into(), json!(""));
                _ = m.insert("type".into(), json!("folder"));
                m
            });
        let id = r
            .get("id")
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|id| *id > 0 && !used.contains(id))
            .unwrap_or_else(&mut next_id);
        _ = used.insert(id);
        _ = r.insert("id".into(), json!(id.to_string()));
        let children: Vec<Value> = doc
            .get(*root)
            .map(|n| n.children.clone())
            .unwrap_or_default()
            .iter()
            .filter_map(|c| build(&ctx, c, &mut used, &mut next_id))
            .collect();
        _ = r.insert("children".into(), Value::Array(children));
        _ = roots.insert((*root).to_string(), Value::Object(r));
    }
    _ = main.insert("roots".into(), Value::Object(roots));
    _ = main.entry("version").or_insert(json!(1));
    let (md5, sha) = checksums(&Value::Object(main.clone()));
    _ = main.insert("checksum".into(), json!(md5));
    if main.contains_key("checksum_sha256") {
        _ = main.insert("checksum_sha256".into(), json!(sha));
    }
    Value::Object(main)
}

/// The checksums Chromium writes for a `Bookmarks` file (MD5, SHA-256):
/// `BookmarkCodec::Encode` hashes each node, roots in order, depth first.
#[must_use]
pub fn checksums(file: &Value) -> (String, String) {
    fn walk(v: &Value, md5: &mut Md5, sha: &mut Sha256) {
        let mut put = |b: &[u8]| {
            md5.update(b);
            sha2::Digest::update(sha, b);
        };
        put(str_of(v, "id").as_bytes());
        // the title as UTF-16, in memory order (little endian here)
        let title: Vec<u8> = str_of(v, "name")
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        put(&title);
        if v.get("type").and_then(Value::as_str) == Some("url") {
            put(b"url");
            put(str_of(v, "url").as_bytes());
        } else {
            put(b"folder");
            for c in v
                .get("children")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                walk(c, md5, sha);
            }
        }
    }
    let (mut md5, mut sha) = (Md5::new(), <Sha256 as sha2::Digest>::new());
    for r in ROOTS {
        if let Some(v) = file.pointer(&format!("/roots/{r}")) {
            walk(v, &mut md5, &mut sha);
        }
    }
    (
        hex::encode(md5.finalize()),
        hex::encode(sha2::Digest::finalize(sha)),
    )
}

/// [`merge`] over JSON documents.
#[must_use]
pub fn merge_values(base: Option<&Value>, ours: &Value, theirs: &Value) -> Merged {
    let parse = |v: &Value| serde_json::from_value::<Doc>(v.clone()).unwrap_or_default();
    let base = base.map(parse);
    let (doc, conflicts) = merge(base.as_ref(), &parse(ours), &parse(theirs));
    Merged {
        value: serde_json::to_value(doc).unwrap_or_default(),
        conflicts,
    }
}
