//! The account's coordination on the network: the core [`Coordinator`],
//! on records the Worker keeps sealed.
//!
//! The Worker orders records (a taken position stays taken) and nothing
//! more: every body is sealed with the account's key, which comes from the
//! recovery code and travels in join codes, and every head, device entry
//! and epoch record inside is signed and checked by the computers as
//! before. Writes need the epoch's capability, which a removed computer
//! never learns.

use std::{collections::HashMap, sync::Arc, sync::Mutex};

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use omacloud_core::{
    Anchor, Coordinator, Head, JoinRequest,
    devices::DeviceEntry,
    epoch::{EpochRecord, EpochSecret, Grant},
};
use serde::{Serialize, de::DeserializeOwned};

use crate::{records::Records, seal::Keys};

/// What identifies an account on the network and opens its records.
#[derive(Clone)]
pub struct Account {
    /// Its id at the Worker: 32 hex characters.
    pub id: String,
    seed: [u8; 32],
    seal: Keys,
}

impl Account {
    /// From the account root's secret (what the recovery code gives).
    #[must_use]
    pub fn from_root(root_secret: &[u8; 32]) -> Self {
        Self::from_seed(blake3::derive_key("omacloud 2026-10 account", root_secret))
    }

    /// From the seed a join code carries.
    #[must_use]
    pub fn from_seed(seed: [u8; 32]) -> Self {
        let id = hex::encode(&blake3::derive_key("omacloud 2026-10 account id", &seed)[..16]);
        Self {
            id,
            seed,
            seal: Keys::from_secret(&format!("account {}", hex::encode(seed))),
        }
    }

    /// What a join code carries: reading the account's records needs it.
    #[must_use]
    pub fn seed(&self) -> [u8; 32] {
        self.seed
    }
}

/// The capability that writes an account's records in an epoch, from the
/// epoch's repository password.
#[must_use]
pub fn capability(password: &str) -> String {
    hex::encode(blake3::derive_key(
        "omacloud 2026-10 coordination capability",
        password.as_bytes(),
    ))
}

pub struct NetworkCoordinator {
    records: Arc<dyn Records>,
    account: Account,
    cap: Mutex<Option<String>>,
    /// The last record seen in each log: `head()` asks only for newer ones.
    last: Mutex<HashMap<&'static str, (u64, String)>>,
}

impl NetworkCoordinator {
    #[must_use]
    pub fn new(records: Arc<dyn Records>, account: Account, cap: Option<String>) -> Self {
        Self {
            records,
            account,
            cap: Mutex::new(cap),
            last: Mutex::default(),
        }
    }

    /// Use `cap` for writes from now on.
    pub fn set_cap(&self, cap: String) {
        *self.cap.lock().expect("cap") = Some(cap);
    }

    fn cap(&self) -> Result<String> {
        self.cap
            .lock()
            .expect("cap")
            .clone()
            .context("this computer can't write the account's records yet")
    }

    #[must_use]
    pub fn account(&self) -> &Account {
        &self.account
    }

    fn seal<T: Serialize>(&self, place: &str, value: &T) -> Result<String> {
        Ok(STANDARD.encode(self.account.seal.seal(place, &serde_json::to_vec(value)?)))
    }

    fn open<T: DeserializeOwned>(&self, place: &str, body: &str) -> Result<T> {
        let sealed = STANDARD.decode(body).context("a record that isn't base64")?;
        let plain = self
            .account
            .seal
            .open(place, &sealed)
            .with_context(|| format!("the record at {place}"))?;
        Ok(serde_json::from_slice(&plain)?)
    }

    fn after<T: DeserializeOwned>(&self, log: &'static str, after: u64) -> Result<Vec<T>> {
        let records = self.records.log(&self.account.id, log, after)?;
        if let Some((seq, body)) = records.last() {
            self.last.lock().expect("last").insert(log, (*seq, body.clone()));
        }
        records
            .iter()
            .map(|(seq, body)| self.open(&format!("{log}/{seq}"), body))
            .collect()
    }

    fn latest<T: DeserializeOwned>(&self, log: &'static str) -> Result<Option<T>> {
        let known = self.last.lock().expect("last").get(log).cloned();
        let newer = self
            .records
            .log(&self.account.id, log, known.as_ref().map_or(0, |k| k.0))?;
        let latest = match (newer.last(), known) {
            (Some((seq, body)), _) => {
                self.last.lock().expect("last").insert(log, (*seq, body.clone()));
                Some((*seq, body.clone()))
            }
            (None, known) => known,
        };
        latest
            .map(|(seq, body)| self.open(&format!("{log}/{seq}"), &body))
            .transpose()
    }

    fn append_at<T: Serialize>(&self, log: &'static str, seq: u64, value: &T) -> Result<bool> {
        let place = format!("{log}/{seq}");
        self.records
            .append(&self.account.id, &self.cap()?, log, seq, &self.seal(&place, value)?)
    }

    /// Make the account at the Worker, writable with `cap`. False if it
    /// exists.
    ///
    /// # Errors
    ///
    /// If the Worker can't be reached.
    pub fn create(&self, cap: &str) -> Result<bool> {
        self.records.create_account(&self.account.id, cap)
    }

    /// Move the account's writes to `new` (a new epoch's capability).
    ///
    /// # Errors
    ///
    /// If this computer's capability is refused.
    pub fn replace_cap(&self, new: &str) -> Result<()> {
        self.records.replace_cap(&self.account.id, &self.cap()?, new)?;
        self.set_cap(new.to_string());
        Ok(())
    }
}

impl Coordinator for NetworkCoordinator {
    fn head(&self) -> Result<Option<Head>> {
        self.latest("heads")
    }
    fn since(&self, after: u64) -> Result<Vec<Head>> {
        self.after("heads", after)
    }
    fn append(&self, head: &Head) -> Result<bool> {
        self.append_at("heads", head.seq, head)
    }
    fn device_head(&self) -> Result<Option<DeviceEntry>> {
        self.latest("devices")
    }
    fn devices_since(&self, after: u64) -> Result<Vec<DeviceEntry>> {
        self.after("devices", after)
    }
    fn append_device(&self, entry: &DeviceEntry) -> Result<bool> {
        self.append_at("devices", entry.seq, entry)
    }
    fn anchor(&self) -> Result<Option<Anchor>> {
        self.records
            .anchor(&self.account.id)?
            .map(|body| self.open("anchor", &body))
            .transpose()
    }
    fn set_anchor(&self, anchor: &Anchor) -> Result<bool> {
        self.records
            .set_anchor(&self.account.id, &self.cap()?, &self.seal("anchor", anchor)?)
    }
    fn epochs(&self) -> Result<Vec<EpochRecord>> {
        self.after("epochs", 0)
    }
    fn append_epoch(&self, record: &EpochRecord) -> Result<bool> {
        // from 1, as logs count
        self.append_at("epochs", record.epoch + 1, record)
    }
    fn grants(&self, device: &str) -> Result<Vec<Grant>> {
        self.records
            .named(&self.account.id, "grants", &format!("{device}-"))?
            .iter()
            .map(|(name, body)| self.open(&format!("grants/{name}"), body))
            .collect()
    }
    fn put_grant(&self, grant: &Grant) -> Result<()> {
        let name = format!("{}-{}", grant.device, grant.epoch);
        let body = self.seal(&format!("grants/{name}"), grant)?;
        self.records
            .put_named(&self.account.id, Some(&self.cap()?), "grants", &name, &body)
    }
    fn requests(&self) -> Result<Vec<JoinRequest>> {
        self.records
            .named(&self.account.id, "requests", "")?
            .iter()
            .map(|(name, body)| self.open(&format!("requests/{name}"), body))
            .collect()
    }
    fn request(&self, req: &JoinRequest) -> Result<()> {
        let body = self.seal(&format!("requests/{}", req.device), req)?;
        self.records
            .put_named(&self.account.id, None, "requests", &req.device, &body)
    }
    fn remove_request(&self, device: &str) -> Result<()> {
        self.records
            .delete_named(&self.account.id, &self.cap()?, "requests", device)
    }
    fn epoch_started(&self, previous: Option<&EpochSecret>, secret: &EpochSecret) -> Result<()> {
        if let Some(previous) = previous {
            self.set_cap(capability(&previous.password));
        }
        self.replace_cap(&capability(&secret.password))
    }
    fn epoch_known(&self, secret: &EpochSecret) {
        self.set_cap(capability(&secret.password));
    }
    fn marker(&self) -> Result<Option<String>> {
        Ok(Some(self.records.marker(&self.account.id)?.to_string()))
    }
}
