//! This computer's copy of what it reads from and writes to the Worker:
//! its buckets' sealed index entries, its account's sealed coordination
//! records, and the node directory. Everything in it is as the Worker
//! keeps it, sealed. With it the account outlives the Worker: reads fall
//! back to it while the Worker is out of reach, and `republish` puts it
//! back on a Worker that lost it (or a new one).

use std::{path::Path, sync::Mutex};

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

pub struct Mirror {
    db: Mutex<Connection>,
}

impl Mirror {
    /// Open (or make) the mirror at `path`.
    ///
    /// # Errors
    ///
    /// If the database can't be opened.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let db = Connection::open(path)?;
        db.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS entries (scope TEXT, id TEXT, sealed BLOB, PRIMARY KEY (scope, id));
             CREATE TABLE IF NOT EXISTS logs (account TEXT, log TEXT, seq INTEGER, body TEXT, PRIMARY KEY (account, log, seq));
             CREATE TABLE IF NOT EXISTS anchors (account TEXT PRIMARY KEY, body TEXT);
             CREATE TABLE IF NOT EXISTS named (account TEXT, kind TEXT, name TEXT, body TEXT, PRIMARY KEY (account, kind, name));
             CREATE TABLE IF NOT EXISTS nodes (one INTEGER PRIMARY KEY CHECK (one = 1), json TEXT);",
        )?;
        Ok(Self { db: Mutex::new(db) })
    }

    fn db(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.db.lock().expect("mirror")
    }

    /// A bucket's whole index as the Worker listed it. A listing that
    /// would drop more than half of what's kept looks like the Worker
    /// losing it, not like deletes: then it only adds.
    pub(crate) fn set_index(&self, scope: &str, entries: &[(String, Vec<u8>)]) -> Result<()> {
        let mut db = self.db();
        let tx = db.transaction()?;
        let kept: i64 = tx.query_row("SELECT count(*) FROM entries WHERE scope = ?1", [scope], |r| r.get(0))?;
        if lost(kept, entries.len()) {
            log::warn!(
                "the Worker lists {} of the {kept} index entries kept here: keeping them",
                entries.len()
            );
        } else {
            tx.execute("DELETE FROM entries WHERE scope = ?1", [scope])?;
        }
        for (id, sealed) in entries {
            tx.execute(
                "INSERT OR REPLACE INTO entries VALUES (?1, ?2, ?3)",
                params![scope, id, sealed],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn index(&self, scope: &str) -> Result<Vec<(String, Vec<u8>)>> {
        let db = self.db();
        let mut q = db.prepare("SELECT id, sealed FROM entries WHERE scope = ?1 ORDER BY id")?;
        let rows = q.query_map([scope], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub(crate) fn entry(&self, scope: &str, id: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .db()
            .query_row(
                "SELECT sealed FROM entries WHERE scope = ?1 AND id = ?2",
                [scope, id],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub(crate) fn put_entry(&self, scope: &str, id: &str, sealed: Option<&[u8]>) -> Result<()> {
        let db = self.db();
        match sealed {
            Some(s) => db.execute(
                "INSERT OR REPLACE INTO entries VALUES (?1, ?2, ?3)",
                params![scope, id, s],
            )?,
            None => db.execute("DELETE FROM entries WHERE scope = ?1 AND id = ?2", [scope, id])?,
        };
        Ok(())
    }

    pub(crate) fn add_log(&self, account: &str, log: &str, records: &[(u64, String)]) -> Result<()> {
        let db = self.db();
        for (seq, body) in records {
            db.execute(
                "INSERT OR IGNORE INTO logs VALUES (?1, ?2, ?3, ?4)",
                params![account, log, i64::try_from(*seq)?, body],
            )?;
        }
        Ok(())
    }

    pub(crate) fn log(&self, account: &str, log: &str, after: u64) -> Result<Vec<(u64, String)>> {
        let db = self.db();
        let mut q =
            db.prepare("SELECT seq, body FROM logs WHERE account = ?1 AND log = ?2 AND seq > ?3 ORDER BY seq")?;
        let rows = q.query_map(params![account, log, i64::try_from(after)?], |r| {
            Ok((u64::try_from(r.get::<_, i64>(0)?).unwrap_or(0), r.get(1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub(crate) fn set_anchor(&self, account: &str, body: &str) -> Result<()> {
        self.db()
            .execute("INSERT OR REPLACE INTO anchors VALUES (?1, ?2)", [account, body])?;
        Ok(())
    }

    pub(crate) fn anchor(&self, account: &str) -> Result<Option<String>> {
        Ok(self
            .db()
            .query_row("SELECT body FROM anchors WHERE account = ?1", [account], |r| r.get(0))
            .optional()?)
    }

    /// The named records of a kind whose name starts with `prefix`, as the
    /// Worker listed them.
    pub(crate) fn set_named(
        &self,
        account: &str,
        kind: &str,
        prefix: &str,
        records: &[(String, String)],
    ) -> Result<()> {
        let mut db = self.db();
        let tx = db.transaction()?;
        let kept: i64 = tx.query_row(
            "SELECT count(*) FROM named WHERE account = ?1 AND kind = ?2 AND substr(name, 1, length(?3)) = ?3",
            [account, kind, prefix],
            |r| r.get(0),
        )?;
        if !lost(kept, records.len()) {
            tx.execute(
                "DELETE FROM named WHERE account = ?1 AND kind = ?2 AND substr(name, 1, length(?3)) = ?3",
                [account, kind, prefix],
            )?;
        }
        for (name, body) in records {
            tx.execute(
                "INSERT OR REPLACE INTO named VALUES (?1, ?2, ?3, ?4)",
                [account, kind, name, body],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn put_named(&self, account: &str, kind: &str, name: &str, body: Option<&str>) -> Result<()> {
        let db = self.db();
        match body {
            Some(b) => db.execute(
                "INSERT OR REPLACE INTO named VALUES (?1, ?2, ?3, ?4)",
                [account, kind, name, b],
            )?,
            None => db.execute(
                "DELETE FROM named WHERE account = ?1 AND kind = ?2 AND name = ?3",
                [account, kind, name],
            )?,
        };
        Ok(())
    }

    pub(crate) fn named(&self, account: &str, kind: &str, prefix: &str) -> Result<Vec<(String, String)>> {
        let db = self.db();
        let mut q = db.prepare(
            "SELECT name, body FROM named WHERE account = ?1 AND kind = ?2 AND substr(name, 1, length(?3)) = ?3 ORDER BY name",
        )?;
        let rows = q.query_map([account, kind, prefix], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub(crate) fn set_nodes(&self, json: &str) -> Result<()> {
        self.db()
            .execute("INSERT OR REPLACE INTO nodes VALUES (1, ?1)", [json])?;
        Ok(())
    }

    pub(crate) fn nodes(&self) -> Result<Option<String>> {
        Ok(self
            .db()
            .query_row("SELECT json FROM nodes WHERE one = 1", [], |r| r.get(0))
            .optional()?)
    }
}

/// Whether a listing of `listed` against `kept` looks like loss: fewer
/// than half remain.
fn lost(kept: i64, listed: usize) -> bool {
    kept > 0 && i64::try_from(listed).unwrap_or(i64::MAX) * 2 < kept
}
