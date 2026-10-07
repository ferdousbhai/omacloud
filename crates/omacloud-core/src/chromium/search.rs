//! Chromium's search engines as a mergeable document.
//!
//! They live in the `keywords` table of the profile's `Web Data`, an SQLite
//! database (components/search_engines/keyword_table.cc). The engines
//! synced are the ones the user made or edited: not built in
//! (`prepopulate_id` 0), not a starter pack shortcut (`starter_pack_id` 0,
//! like @bookmarks), not set by policy, and not ones Chromium added on its
//! own from sites visited (`safe_for_autoreplace` 1). They're keyed by
//! `sync_guid`, which Chromium keeps for each engine and which travels with
//! it.
//!
//! The table's `url_hash` column is checked only on Windows
//! (`GetKeywordDataFromStatement`); here new rows leave it NULL, as
//! Chromium's own migration does where it has no hash.
//!
//! Which engine is the default is a protected preference
//! (`default_search_provider_data.template_url_data`, tracked with a MAC in
//! chrome_pref_service_factory.cc): it isn't synced, and the engine that is
//! the default here is never deleted from here.

use std::{collections::BTreeSet, fs, path::Path};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, params_from_iter, types::Value as Sql};
use serde_json::{Map, Value, json};

/// The engine fields that sync; the rest (row id, usage counts, last
/// visit, the hash) are this computer's.
const FIELDS: &[&str] = &[
    "short_name",
    "keyword",
    "favicon_url",
    "url",
    "suggest_url",
    "alternate_urls",
    "image_url",
    "search_url_post_params",
    "suggest_url_post_params",
    "image_url_post_params",
    "new_tab_url",
    "input_encodings",
    "originating_url",
    "date_created",
    "last_modified",
    "is_active",
];

/// Which rows sync (see the module's doc).
const SYNCED: &str = "prepopulate_id = 0 AND created_by_policy = 0 AND safe_for_autoreplace = 0";

fn columns(db: &Connection) -> Result<BTreeSet<String>> {
    let mut st = db.prepare("SELECT name FROM pragma_table_info('keywords')")?;
    let cols = st
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<BTreeSet<_>, _>>()?;
    Ok(cols)
}

fn synced_rows(cols: &BTreeSet<String>) -> String {
    let mut w = SYNCED.to_string();
    if cols.contains("starter_pack_id") {
        w.push_str(" AND starter_pack_id = 0");
    }
    if cols.contains("enforced_by_policy") {
        w.push_str(" AND enforced_by_policy = 0");
    }
    w
}

fn to_json(v: Sql) -> Value {
    match v {
        Sql::Null => Value::Null,
        Sql::Integer(i) => json!(i),
        Sql::Real(f) => json!(f),
        Sql::Text(s) => json!(s),
        Sql::Blob(_) => Value::Null,
    }
}

fn to_sql(v: &Value) -> Sql {
    match v {
        Value::Null => Sql::Null,
        Value::Bool(b) => Sql::Integer((*b).into()),
        Value::Number(n) => n
            .as_i64()
            .map_or_else(|| Sql::Real(n.as_f64().unwrap_or(0.0)), Sql::Integer),
        Value::String(s) => Sql::Text(s.clone()),
        other => Sql::Text(other.to_string()),
    }
}

/// The engines in a `Web Data` file, read from a copy: Chromium may have it
/// open (and locked), so the file and its journal are copied into `scratch`
/// and read there. `None` if it changed while being copied, or a write was
/// under way (a journal with something in it): the next look tries again.
///
/// # Errors
///
/// When the copy can't be made or read.
pub fn capture(web_data: &Path, scratch: &Path) -> Result<Option<Value>> {
    let journal = web_data.with_file_name("Web Data-journal");
    let wal = web_data.with_file_name("Web Data-wal");
    if fs::metadata(&journal).is_ok_and(|m| m.len() > 0)
        || fs::metadata(&wal).is_ok_and(|m| m.len() > 0)
    {
        return Ok(None);
    }
    let stamp = |p: &Path| fs::metadata(p).ok().map(|m| (m.len(), m.modified().ok()));
    let before = stamp(web_data);
    fs::create_dir_all(scratch)?;
    let copy = scratch.join("Web Data");
    _ = fs::copy(web_data, &copy).with_context(|| format!("copying {}", web_data.display()))?;
    if stamp(web_data) != before {
        _ = fs::remove_file(&copy);
        return Ok(None);
    }
    let out = read(&copy);
    _ = fs::remove_file(&copy);
    out.map(Some)
}

/// The engines in a `Web Data` file nothing else has open.
///
/// # Errors
///
/// When it isn't a database with a `keywords` table.
pub fn read(path: &Path) -> Result<Value> {
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let cols = columns(&db)?;
    let fields: Vec<&str> = FIELDS
        .iter()
        .copied()
        .filter(|f| cols.contains(*f))
        .collect();
    let sql = format!(
        "SELECT sync_guid, {} FROM keywords WHERE {}",
        fields.join(", "),
        synced_rows(&cols)
    );
    let mut st = db.prepare(&sql)?;
    let mut rows = st.query([])?;
    let mut out = Map::new();
    while let Some(r) = rows.next()? {
        let Some(guid) = r.get::<_, Option<String>>(0)?.filter(|g| !g.is_empty()) else {
            continue;
        };
        let mut e = Map::new();
        for (i, f) in fields.iter().enumerate() {
            let v = to_json(r.get::<_, Sql>(i + 1)?);
            // empty is unset: written back as empty, it reads the same
            if !v.is_null() && v != "" {
                _ = e.insert((*f).to_string(), v);
            }
        }
        _ = out.insert(guid, Value::Object(e));
    }
    Ok(Value::Object(out))
}

/// Make the synced engines in the `Web Data` at `path` the document's, in
/// one transaction; `keep` (the default engine here) is never deleted. The
/// database must not be open elsewhere (Chromium closed).
///
/// # Errors
///
/// When the database can't be changed; then nothing is.
pub fn write(path: &Path, doc: &Value, keep: Option<&str>) -> Result<()> {
    let empty = Map::new();
    let doc = doc.as_object().unwrap_or(&empty);
    let mut db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    let cols = columns(&db)?;
    let tx = db.transaction()?;
    let here: BTreeSet<String> = {
        let mut st = tx.prepare(&format!(
            "SELECT sync_guid FROM keywords WHERE {}",
            synced_rows(&cols)
        ))?;
        st.query_map([], |r| r.get::<_, Option<String>>(0))?
            .filter_map(|g| g.ok().flatten())
            .collect()
    };
    for guid in &here {
        if !doc.contains_key(guid) && Some(guid.as_str()) != keep {
            _ = tx.execute("DELETE FROM keywords WHERE sync_guid = ?1", [guid])?;
        }
    }
    for (guid, e) in doc {
        let Some(e) = e.as_object() else { continue };
        // an engine needs a url and a keyword (KeywordTable drops rows
        // without a url)
        if e.get("url")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
            || e.get("keyword")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
        {
            continue;
        }
        let fields: Vec<&str> = FIELDS
            .iter()
            .copied()
            .filter(|f| cols.contains(*f))
            .collect();
        let values: Vec<Sql> = fields
            .iter()
            .map(|f| match (e.get(*f), *f) {
                (Some(v), _) => to_sql(v),
                // NOT NULL columns
                (None, "short_name" | "favicon_url") => Sql::Text(String::new()),
                (None, _) => Sql::Null,
            })
            .collect();
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM keywords WHERE sync_guid = ?1)",
            [guid],
            |r| r.get(0),
        )?;
        if exists {
            if !here.contains(guid) {
                continue; // a built in or policy engine with that guid: Chromium's
            }
            let set: Vec<String> = fields.iter().map(|f| format!("{f} = ?")).collect();
            let mut args = values.clone();
            args.push(Sql::Text(guid.clone()));
            _ = tx.execute(
                &format!("UPDATE keywords SET {} WHERE sync_guid = ?", set.join(", ")),
                params_from_iter(args),
            )?;
        } else {
            let mut names: Vec<&str> = fields.clone();
            names.extend([
                "sync_guid",
                "safe_for_autoreplace",
                "prepopulate_id",
                "created_by_policy",
            ]);
            let mut args = values.clone();
            args.extend([
                Sql::Text(guid.clone()),
                Sql::Integer(0),
                Sql::Integer(0),
                Sql::Integer(0),
            ]);
            let marks = vec!["?"; names.len()].join(", ");
            _ = tx.execute(
                &format!(
                    "INSERT INTO keywords ({}) VALUES ({marks})",
                    names.join(", ")
                ),
                params_from_iter(args),
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}
