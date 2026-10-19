//! An account's coordination records as the Worker keeps them: append-only
//! logs (create-only positions), a create-only anchor, and named records
//! (grants, join requests, nodes, bases). Bodies are opaque here;
//! [`crate::coord`] seals them. Writes need the account's capability,
//! except a join request.

use std::{collections::BTreeMap, sync::Mutex};

use anyhow::Result;

/// The Worker's coordination store. Blocking, like [`crate::registry::Registry`].
pub trait Records: Send + Sync {
    /// Make the account, with `cap` as its capability. False if it exists.
    fn create_account(&self, account: &str, cap: &str) -> Result<bool>;
    /// Replace the capability (a new epoch).
    fn replace_cap(&self, account: &str, cap: &str, new: &str) -> Result<()>;
    /// A log's records with `seq > after`, in order.
    fn log(&self, account: &str, log: &str, after: u64) -> Result<Vec<(u64, String)>>;
    /// Write at `seq` if free. False if taken.
    fn append(&self, account: &str, cap: &str, log: &str, seq: u64, body: &str) -> Result<bool>;
    fn anchor(&self, account: &str) -> Result<Option<String>>;
    /// Set the anchor once. False if set.
    fn set_anchor(&self, account: &str, cap: &str, body: &str) -> Result<bool>;
    /// Named records of `kind` (`grants`, `requests`, `nodes`, `bases`) whose name starts with `prefix`.
    fn named(&self, account: &str, kind: &str, prefix: &str) -> Result<Vec<(String, String)>>;
    /// Add or replace a named record; `cap` is None for a join request.
    fn put_named(&self, account: &str, cap: Option<&str>, kind: &str, name: &str, body: &str) -> Result<()>;
    fn delete_named(&self, account: &str, cap: &str, kind: &str, name: &str) -> Result<()>;
    /// Changes whenever anything of the account is written.
    fn marker(&self, account: &str) -> Result<u64>;
}

#[derive(Default)]
struct Account {
    cap: [u8; 32],
    version: u64,
    logs: BTreeMap<(String, u64), String>,
    anchor: Option<String>,
    named: BTreeMap<(String, String), String>,
}

/// Records in memory, with the Worker's rules: tests, development.
#[derive(Default)]
pub struct MemRecords {
    accounts: Mutex<BTreeMap<String, Account>>,
}

fn hash(cap: &str) -> [u8; 32] {
    *blake3::hash(cap.as_bytes()).as_bytes()
}

impl MemRecords {
    fn write<T>(&self, account: &str, cap: Option<&str>, f: impl FnOnce(&mut Account) -> T) -> Result<T> {
        let mut accounts = self.accounts.lock().expect("records");
        let a = accounts
            .get_mut(account)
            .ok_or_else(|| anyhow::anyhow!("no such account"))?;
        if let Some(cap) = cap {
            anyhow::ensure!(a.cap == hash(cap), "that capability doesn't write here");
        }
        a.version += 1;
        Ok(f(a))
    }

    fn read<T: Default>(&self, account: &str, f: impl FnOnce(&Account) -> T) -> T {
        self.accounts
            .lock()
            .expect("records")
            .get(account)
            .map(f)
            .unwrap_or_default()
    }
}

impl Records for MemRecords {
    fn create_account(&self, account: &str, cap: &str) -> Result<bool> {
        let mut accounts = self.accounts.lock().expect("records");
        if accounts.contains_key(account) {
            return Ok(false);
        }
        accounts.insert(
            account.to_string(),
            Account {
                cap: hash(cap),
                ..Account::default()
            },
        );
        Ok(true)
    }
    fn replace_cap(&self, account: &str, cap: &str, new: &str) -> Result<()> {
        self.write(account, Some(cap), |a| a.cap = hash(new))
    }
    fn log(&self, account: &str, log: &str, after: u64) -> Result<Vec<(u64, String)>> {
        Ok(self.read(account, |a| {
            a.logs
                .range((log.to_string(), after + 1)..=(log.to_string(), u64::MAX))
                .map(|((_, seq), body)| (*seq, body.clone()))
                .collect()
        }))
    }
    fn append(&self, account: &str, cap: &str, log: &str, seq: u64, body: &str) -> Result<bool> {
        self.write(account, Some(cap), |a| {
            let key = (log.to_string(), seq);
            if a.logs.contains_key(&key) {
                return false;
            }
            a.logs.insert(key, body.to_string());
            true
        })
    }
    fn anchor(&self, account: &str) -> Result<Option<String>> {
        Ok(self.read(account, |a| a.anchor.clone()))
    }
    fn set_anchor(&self, account: &str, cap: &str, body: &str) -> Result<bool> {
        self.write(account, Some(cap), |a| {
            if a.anchor.is_some() {
                return false;
            }
            a.anchor = Some(body.to_string());
            true
        })
    }
    fn named(&self, account: &str, kind: &str, prefix: &str) -> Result<Vec<(String, String)>> {
        Ok(self.read(account, |a| {
            a.named
                .iter()
                .filter(|((k, n), _)| k == kind && n.starts_with(prefix))
                .map(|((_, n), b)| (n.clone(), b.clone()))
                .collect()
        }))
    }
    fn put_named(&self, account: &str, cap: Option<&str>, kind: &str, name: &str, body: &str) -> Result<()> {
        self.write(account, cap, |a| {
            a.named.insert((kind.to_string(), name.to_string()), body.to_string());
        })
    }
    fn delete_named(&self, account: &str, cap: &str, kind: &str, name: &str) -> Result<()> {
        self.write(account, Some(cap), |a| {
            a.named.remove(&(kind.to_string(), name.to_string()));
        })
    }
    fn marker(&self, account: &str) -> Result<u64> {
        Ok(self.read(account, |a| a.version))
    }
}
