//! Saved Wi-Fi networks, merged across computers (a [`Group`] of one
//! document, `wifi/networks.json`), through NetworkManager without root.
//!
//! NetworkManager answers `nmcli` for the user at the desktop: under its
//! default polkit rules an active local session may read saved networks'
//! keys and change saved networks (`org.freedesktop.NetworkManager.
//! settings.modify.system`, or `.modify.own` for networks only this user
//! sees). `nmcli general permissions` says which applies; with neither,
//! Wi-Fi sync says so and does nothing. With iwd as the backend instead,
//! saved networks are files only root can read (`/var/lib/iwd`), and Wi-Fi
//! sync is unavailable rather than asking for root.
//!
//! Each network is keyed by its name (SSID) and security, and holds its key
//! (WPA/SAE), whether it's hidden, autoconnect and priority. Interface,
//! MAC address, uuid and timestamps stay with each computer. Enterprise
//! (802.1X) networks, WEP and hotspots aren't synced, nor a network whose
//! key a keyring agent keeps; status lists them. Keys are passed to
//! `nmcli` on its standard input, never on its command line, and network
//! names and keys are never logged.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value, json};

use crate::merged::{self, Concern, Group, Item, ME, Merged};

/// How long the answer to whether Wi-Fi sync works here stands.
const FRESH: Duration = Duration::from_secs(300);

/// How often a sync looks at the saved networks at most (see
/// [`Group::due`]).
const LOOK: Duration = Duration::from_secs(60);

/// A saved network, as it syncs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Network {
    pub ssid: String,
    /// `open`, `owe`, `wpa-psk` or `sae`.
    pub security: String,
    pub psk: Option<String>,
    pub hidden: bool,
    pub autoconnect: bool,
    pub priority: i64,
}

impl Network {
    #[must_use]
    pub fn key(&self) -> String {
        format!("{} {}", self.security, self.ssid)
    }

    fn to_json(&self) -> Value {
        let mut m = Map::new();
        _ = m.insert("ssid".into(), json!(self.ssid));
        _ = m.insert("security".into(), json!(self.security));
        if let Some(p) = &self.psk {
            _ = m.insert("psk".into(), json!(p));
        }
        _ = m.insert("hidden".into(), json!(self.hidden));
        _ = m.insert("autoconnect".into(), json!(self.autoconnect));
        _ = m.insert("priority".into(), json!(self.priority));
        Value::Object(m)
    }

    fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            ssid: v.get("ssid")?.as_str()?.to_string(),
            security: v.get("security")?.as_str()?.to_string(),
            psk: v.get("psk").and_then(Value::as_str).map(str::to_string),
            hidden: v.get("hidden").and_then(Value::as_bool).unwrap_or(false),
            autoconnect: v
                .get("autoconnect")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            priority: v.get("priority").and_then(Value::as_i64).unwrap_or(0),
        })
    }
}

/// A saved connection here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    pub uuid: String,
    /// The name it shows, for status.
    pub name: String,
    /// Which network it is (security and name, as [`Network::key`]) when
    /// that's known, even if it doesn't sync: another computer's network of
    /// that key is then this one, not one to add.
    pub key: Option<String>,
    /// The network, or why it doesn't sync.
    pub network: Result<Network, String>,
}

/// What Wi-Fi sync needs from NetworkManager; [`Nmcli`] in use, a stand-in
/// in tests.
pub trait Manager: Send + Sync {
    /// Whether saved networks can be read and changed here, and if not why.
    ///
    /// # Errors
    ///
    /// The reason, for the user.
    fn ready(&self) -> Result<(), String>;
    /// Saved Wi-Fi connections.
    ///
    /// # Errors
    ///
    /// When NetworkManager can't be asked.
    fn saved(&self) -> Result<Vec<Saved>>;
    /// Connections in use now.
    fn active(&self) -> Vec<String>;
    /// Save a new connection.
    ///
    /// # Errors
    ///
    /// When NetworkManager refuses.
    fn add(&self, n: &Network) -> Result<()>;
    /// Change a saved connection.
    ///
    /// # Errors
    ///
    /// When NetworkManager refuses.
    fn change(&self, uuid: &str, n: &Network) -> Result<()>;
    /// Forget a saved connection.
    ///
    /// # Errors
    ///
    /// When NetworkManager refuses.
    fn forget(&self, uuid: &str) -> Result<()>;
    /// A saved connection's settings, keys included, for a backup.
    ///
    /// # Errors
    ///
    /// When NetworkManager can't be asked.
    fn export(&self, uuid: &str) -> Result<String>;
}

/// Saved Wi-Fi networks, as a [`Group`].
pub struct Wifi {
    nm: Box<dyn Manager>,
    /// When a sync last looked at the saved networks.
    looked: Mutex<Option<Instant>>,
    /// The last answer to whether Wi-Fi sync can work here.
    ready: Mutex<Option<(Instant, Result<(), String>)>>,
    /// What the last write held back and why, with the connections in use
    /// then: when those change, it's tried again.
    held: Mutex<Option<(String, Vec<String>)>>,
}

/// Set on a network whose key differed between computers when they first
/// met: each keeps its own key, and only a computer without the network
/// takes the synced one.
const OWN_KEYS: &str = "own_keys";

impl Wifi {
    #[must_use]
    pub fn new(nm: Box<dyn Manager>) -> Self {
        Self {
            nm,
            looked: Mutex::new(None),
            ready: Mutex::new(None),
            held: Mutex::new(None),
        }
    }

    fn ready(&self) -> Result<(), String> {
        let mut ready = self.ready.lock().expect("not poisoned");
        if let Some((_, r)) = ready.as_ref().filter(|(at, _)| at.elapsed() < FRESH) {
            return r.clone();
        }
        let r = self.nm.ready();
        *ready = Some((Instant::now(), r.clone()));
        r
    }

    /// The document of what's saved here: per key, the first of the
    /// connections that share one. A network that can't be read this time
    /// but is still saved (a key a keyring keeps now) stays as `applied`
    /// last had it: unknown is not gone.
    fn doc(saved: &[Saved], applied: Option<&Value>) -> Value {
        let mut m = Map::new();
        for s in saved {
            match (&s.network, &s.key) {
                (Ok(n), _) => _ = m.entry(n.key()).or_insert_with(|| n.to_json()),
                (Err(_), Some(k)) => {
                    if let Some(v) = applied.and_then(|a| a.get(k)) {
                        _ = m.entry(k.clone()).or_insert_with(|| v.clone());
                    }
                }
                (Err(_), None) => {}
            }
        }
        Value::Object(m)
    }

    /// `mirror` as this computer would hold it: a network whose computers
    /// keep their own keys has this computer's key, from `applied`.
    fn as_here(mirror: &Value, applied: Option<&Value>) -> Value {
        let mut out = mirror.clone();
        for (k, v) in out.as_object_mut().into_iter().flatten() {
            let Some(here) = applied.and_then(|a| a.get(k)) else {
                continue;
            };
            if v.get(OWN_KEYS) == Some(&json!(true)) {
                let m = v.as_object_mut().expect("networks are objects");
                _ = m.remove(OWN_KEYS);
                match here.get("psk") {
                    Some(p) => _ = m.insert("psk".into(), p.clone()),
                    None => _ = m.remove("psk"),
                }
            }
        }
        out
    }
}

impl Group for Wifi {
    fn name(&self) -> &str {
        "wifi"
    }

    fn docs(&self) -> Vec<String> {
        if self.ready().is_ok() {
            vec!["networks.json".into()]
        } else {
            Vec::new()
        }
    }

    fn capture(&self, _doc: &str, applied: Option<&Value>) -> Result<Option<Value>> {
        if self.ready().is_err() {
            return Ok(None);
        }
        *self.looked.lock().expect("not poisoned") = Some(Instant::now());
        Ok(Some(Self::doc(&self.nm.saved()?, applied)))
    }

    /// Nothing tells Omacloud when saved networks change (NetworkManager's
    /// files are root's), so a sync looks at most every [`LOOK`]; a write
    /// always looks first.
    fn due(&self) -> bool {
        if self
            .looked
            .lock()
            .expect("not poisoned")
            .is_none_or(|t| t.elapsed() >= LOOK)
        {
            return true;
        }
        // held back for a network in use: due once that changes
        let held = self.held.lock().expect("not poisoned").clone();
        held.is_some_and(|(_, active)| self.nm.active() != active)
    }

    fn busy(&self, _doc: &str) -> Option<String> {
        None
    }

    fn pending(&self, _doc: &str, mirror: &Value, applied: Option<&Value>) -> bool {
        Some(&Self::as_here(mirror, applied)) != applied
    }

    fn apply(
        &self,
        _doc: &str,
        value: &Value,
        applied: Option<&Value>,
        backups: &Path,
    ) -> Result<()> {
        let saved = self.nm.saved()?;
        let mut here: BTreeMap<String, (String, Network)> = BTreeMap::new();
        // networks here that don't sync: never added again, changed or
        // forgotten because of a document
        let mut unknown = BTreeSet::new();
        for s in &saved {
            match (&s.network, &s.key) {
                (Ok(n), _) => {
                    _ = here
                        .entry(n.key())
                        .or_insert_with(|| (s.uuid.clone(), n.clone()));
                }
                (Err(_), Some(k)) => _ = unknown.insert(k.clone()),
                (Err(_), None) => {}
            }
        }
        let want: BTreeMap<String, Network> = Self::as_here(value, applied)
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(k, v)| Some((k.clone(), Network::from_json(v)?)))
            .collect();
        let active = self.nm.active();
        // a connection's settings, keys and all, before they're replaced:
        // once, not again while they're the same as the last copy
        let back_up = |uuid: &str| -> Result<()> {
            let settings = self.nm.export(uuid)?;
            let rel = Path::new("wifi").join(format!("{uuid}.nmconnection"));
            merged::back_up_bytes(backups, &rel, settings.as_bytes())
        };
        // NetworkManager saying no is a refusal, kept until something
        // changes; anything else (a backup, nmcli not answering) is tried
        // again later
        let (mut refused, mut in_use) = (0, 0);
        let mut error: Option<anyhow::Error> = None;
        let mut count = |r: Result<()>| {
            if let Err(e) = r {
                if e.downcast_ref::<merged::Refused>().is_some() {
                    refused += 1;
                } else {
                    error.get_or_insert(e);
                }
            }
        };
        // forgotten elsewhere: only what the last look here saw (one saved
        // since is new here, not forgotten), and not the one in use now
        let seen = applied.and_then(Value::as_object);
        for (key, (uuid, _)) in &here {
            let forgotten = !want.contains_key(key) && seen.is_some_and(|m| m.contains_key(key));
            if !forgotten {
                continue;
            }
            if active.contains(uuid) {
                in_use += 1;
            } else {
                count(back_up(uuid).and_then(|()| self.nm.forget(uuid)));
            }
        }
        for (key, n) in &want {
            if unknown.contains(key) {
                continue;
            }
            let r = match here.get(key) {
                Some((_, now)) if now == n => Ok(()),
                Some((uuid, _)) => back_up(uuid).and_then(|()| self.nm.change(uuid, n)),
                None => self.nm.add(n),
            };
            count(r);
        }
        *self.held.lock().expect("not poisoned") = (in_use > 0).then(|| {
            (
                "a network forgotten on another computer is in use here; it goes once disconnected"
                    .to_string(),
                active.clone(),
            )
        });
        if let Some(e) = error {
            return Err(e);
        }
        if refused > 0 {
            return Err(merged::Refused(format!(
                "NetworkManager refused {refused} change(s) to saved Wi-Fi networks"
            ))
            .into());
        }
        Ok(())
    }

    fn held(&self, _doc: &str) -> Option<String> {
        self.held
            .lock()
            .expect("not poisoned")
            .as_ref()
            .map(|(why, _)| why.clone())
    }

    fn merge(&self, _doc: &str, base: Option<&Value>, ours: &Value, theirs: &Value) -> Merged {
        let map = |v: &Value| v.as_object().cloned().unwrap_or_default();
        let (o, mut t) = (map(ours), map(theirs));
        let mut conflicts = Vec::new();
        // a network both computers saved before they met, with different
        // keys: neither key replaces the other (a wrong one would cut the
        // computer off); each keeps its own
        for (k, ov) in &o {
            let met = base.and_then(|b| b.get(k)).is_some();
            let Some(tv) = t.get_mut(k) else { continue };
            if !met && ov.get("psk") != tv.get("psk") && tv.get(OWN_KEYS).is_none() {
                conflicts.push(format!(
                    "a Wi-Fi network has different keys on {ME} and another computer; each keeps its own"
                ));
                if let Some(m) = tv.as_object_mut() {
                    _ = m.insert(OWN_KEYS.into(), json!(true));
                    match ov.get("psk") {
                        Some(p) => _ = m.insert("psk".into(), p.clone()),
                        None => _ = m.remove("psk"),
                    }
                }
            }
        }
        // the network's name stays out of the record
        let label = |_: &str| "a Wi-Fi network".to_string();
        let mut value = merged::merge_map(
            base.and_then(Value::as_object),
            &o,
            &t,
            true,
            &label,
            &mut conflicts,
        );
        // once set, it stays (a computer's capture never has it), until
        // the key is changed on a computer: a new password is meant for all
        for (k, v) in &mut value {
            let before = base.and_then(|b| b.get(k));
            let rekeyed =
                before.is_some_and(|b| b.get("psk") != o.get(k).and_then(|x| x.get("psk")));
            let Some(m) = v.as_object_mut() else { continue };
            if rekeyed {
                _ = m.remove(OWN_KEYS);
            } else if t.get(k).and_then(|x| x.get(OWN_KEYS)).is_some() {
                _ = m.insert(OWN_KEYS.into(), json!(true));
            }
        }
        Merged {
            value: Value::Object(value),
            conflicts,
        }
    }

    fn status(&self, mirror: &Path, waiting: &[(String, String)]) -> Vec<Item> {
        let (state, detail, not_synced) = match self.ready() {
            Err(why) => ("unavailable", why, Vec::new()),
            Ok(()) => match self.nm.saved() {
                Err(e) => (
                    "unavailable",
                    format!("NetworkManager didn't answer: {e:#}"),
                    Vec::new(),
                ),
                Ok(saved) => {
                    let synced = saved.iter().filter(|s| s.network.is_ok()).count();
                    let not: Vec<String> = saved
                        .iter()
                        .filter_map(|s| {
                            s.network
                                .as_ref()
                                .err()
                                .map(|why| format!("{}: {why}", s.name))
                        })
                        .collect();
                    let refused = waiting
                        .iter()
                        .find_map(|(_, w)| w.strip_prefix("refused: "));
                    // a network forgotten elsewhere and in use here
                    let doc = merged::read(&mirror.join("networks.json"));
                    let active = self.nm.active();
                    let in_use = !waiting.is_empty()
                        && saved.iter().any(|s| {
                            active.contains(&s.uuid)
                                && s.network.as_ref().is_ok_and(|n| {
                                    doc.as_ref().is_some_and(|d| d.get(n.key()).is_none())
                                })
                        });
                    let state = match (waiting.is_empty(), refused) {
                        (true, _) => "synced",
                        (false, Some(_)) => "refused",
                        (false, None) => "waiting",
                    };
                    let mut detail = format!(
                        "{synced} saved network{} with {}",
                        if synced == 1 { "" } else { "s" },
                        if synced == 1 { "its key" } else { "their keys" }
                    );
                    if let Some(why) = refused {
                        detail.push_str(&format!(
                            "; changes from another computer not applied: {why} (tried again when something changes)"
                        ));
                    } else if in_use {
                        detail.push_str(
                            "; a network forgotten on another computer is in use here, and goes once disconnected",
                        );
                    } else if !waiting.is_empty() {
                        detail.push_str(
                            "; changes from another computer are applied at the next sync",
                        );
                    }
                    (state, detail, not)
                }
            },
        };
        vec![Item {
            group: "wifi".into(),
            title: "Wi-Fi".into(),
            state: state.into(),
            detail,
            install: Vec::new(),
            not_synced,
            scope: "wifi/".into(),
        }]
    }

    fn watch_dirs(&self) -> Vec<PathBuf> {
        Vec::new()
    }

    fn concerns(&self, _path: &Path) -> Concern {
        Concern::No
    }
}

/// NetworkManager through `nmcli`.
pub struct Nmcli {
    /// The user's name, for networks only this user sees.
    user: String,
}

impl Nmcli {
    #[must_use]
    pub fn new(user: &str) -> Self {
        Self {
            user: user.to_string(),
        }
    }

    fn run(args: &[&str], stdin: Option<&str>) -> Result<String> {
        let mut child = Command::new("nmcli")
            .args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow!("running nmcli: {e}"))?;
        if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
            pipe.write_all(text.as_bytes())?;
        }
        let out = child.wait_with_output()?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        // nmcli's editor echoes what it's given, keys too: never pass its
        // output on
        let what = args.iter().find(|a| !a.starts_with('-')).unwrap_or(&"");
        let failed = !out.status.success() || (stdin.is_some() && stdout.contains("Error"));
        // nmcli's exit codes: 3 a timeout, 8 NetworkManager not running, no
        // code a signal; those are worth another try, not a refusal
        let transient = matches!(out.status.code(), Some(3 | 8) | None);
        let change = matches!(args.get(1), Some(&"edit" | &"delete")) && *what == "connection";
        if failed && change && !transient {
            // NetworkManager answered, and said no
            return Err(merged::Refused(
                format!("NetworkManager refused a {} ", args[1])
                    .trim()
                    .to_string(),
            )
            .into());
        }
        if failed {
            bail!("nmcli {what} failed");
        }
        Ok(stdout)
    }

    /// `can-modify-system` or `can-modify-own`, from `nmcli general
    /// permissions`, or why neither.
    fn scope(&self) -> Result<bool, String> {
        let out = Self::run(&["-t", "general", "permissions"], None)
            .map_err(|_| "NetworkManager didn't say what this user may change".to_string())?;
        let yes = |p: &str| {
            out.lines().any(|l| {
                l.strip_prefix(p)
                    .is_some_and(|v| v.trim_start_matches(':') == "yes")
            })
        };
        if yes("org.freedesktop.NetworkManager.settings.modify.system") {
            Ok(true)
        } else if yes("org.freedesktop.NetworkManager.settings.modify.own") {
            Ok(false)
        } else {
            Err(
                "NetworkManager doesn't let this user change saved networks without a password"
                    .into(),
            )
        }
    }

    /// The editor commands that make a connection `n`.
    fn commands(&self, n: &Network, new: bool, system: bool) -> String {
        // a value runs to the end of its line
        let clean = |s: &str| s.replace(['\n', '\r'], "");
        let mut c = String::new();
        if new {
            c.push_str(&format!("set connection.id {}\n", clean(&n.ssid)));
            if !system {
                c.push_str(&format!(
                    "set connection.permissions user:{}\n",
                    clean(&self.user)
                ));
            }
        }
        c.push_str(&format!("set 802-11-wireless.ssid {}\n", clean(&n.ssid)));
        c.push_str(&format!(
            "set 802-11-wireless.hidden {}\n",
            yes_no(n.hidden)
        ));
        c.push_str(&format!(
            "set connection.autoconnect {}\n",
            yes_no(n.autoconnect)
        ));
        c.push_str(&format!(
            "set connection.autoconnect-priority {}\n",
            n.priority
        ));
        match n.security.as_str() {
            "open" => {
                if !new {
                    c.push_str("remove 802-11-wireless-security\n");
                }
            }
            sec => {
                c.push_str(&format!("set 802-11-wireless-security.key-mgmt {sec}\n"));
                if let Some(psk) = &n.psk {
                    c.push_str(&format!(
                        "set 802-11-wireless-security.psk {}\n",
                        clean(psk)
                    ));
                    c.push_str("set 802-11-wireless-security.psk-flags 0\n");
                }
            }
        }
        c.push_str("save persistent\nquit\n");
        c
    }
}

fn yes_no(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

/// One `field:value` line of `nmcli -t -e yes`, its value unescaped.
fn field(line: &str) -> Option<(&str, String)> {
    let (name, value) = line.split_once(':')?;
    let mut out = String::new();
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    Some((name, out))
}

/// A saved connection from `nmcli -s -t -e yes connection show` fields.
#[must_use]
pub fn parse(uuid: &str, text: &str) -> Saved {
    let f: BTreeMap<&str, String> = text.lines().filter_map(field).collect();
    let get = |k: &str| f.get(k).cloned().unwrap_or_default();
    let name = get("connection.id");
    let ssid = get("802-11-wireless.ssid");
    let mode = get("802-11-wireless.mode");
    let mgmt = get("802-11-wireless-security.key-mgmt");
    let psk = get("802-11-wireless-security.psk");
    let security = match mgmt.as_str() {
        "" => Some("open"),
        "owe" | "wpa-psk" | "sae" => Some(mgmt.as_str()),
        "none" => Some("wep"),
        _ => Some("enterprise"),
    }
    .filter(|_| !ssid.is_empty() && (mode.is_empty() || mode == "infrastructure"));
    let key = security.map(|sec| format!("{sec} {ssid}"));
    let network = (|| {
        if ssid.is_empty() {
            return Err("no network name".to_string());
        }
        if !mode.is_empty() && mode != "infrastructure" {
            return Err("a hotspot this computer makes".into());
        }
        let security = match mgmt.as_str() {
            "" => "open",
            "owe" => "owe",
            "wpa-psk" => "wpa-psk",
            "sae" => "sae",
            "none" => return Err("WEP, not synced".into()),
            _ => return Err("enterprise (802.1X), not synced".into()),
        };
        let needs_key = matches!(security, "wpa-psk" | "sae");
        if needs_key && psk.is_empty() {
            return Err("its key isn't readable here (a keyring keeps it)".into());
        }
        Ok(Network {
            ssid: ssid.clone(),
            security: security.into(),
            psk: needs_key.then(|| psk.clone()),
            hidden: get("802-11-wireless.hidden") == "yes",
            autoconnect: get("connection.autoconnect") != "no",
            priority: get("connection.autoconnect-priority").parse().unwrap_or(0),
        })
    })();
    Saved {
        uuid: uuid.to_string(),
        name: if name.is_empty() { ssid } else { name },
        key,
        network,
    }
}

impl Manager for Nmcli {
    fn ready(&self) -> Result<(), String> {
        match Self::run(&["-t", "-f", "RUNNING", "general"], None) {
            Ok(out) if out.trim() == "running" => self.scope().map(|_| ()),
            _ => {
                let iwd = Command::new("systemctl")
                    .args(["is-active", "--quiet", "iwd"])
                    .status()
                    .is_ok_and(|s| s.success());
                Err(if iwd {
                    "Wi-Fi here is managed by iwd, whose saved networks only root can read (/var/lib/iwd)".into()
                } else {
                    "NetworkManager isn't running".into()
                })
            }
        }
    }

    fn saved(&self) -> Result<Vec<Saved>> {
        let list = Self::run(
            &["-t", "-e", "yes", "-f", "TYPE,UUID", "connection", "show"],
            None,
        )?;
        let mut out = Vec::new();
        for line in list.lines() {
            let Some((kind, uuid)) = line.split_once(':') else {
                continue;
            };
            if kind != "802-11-wireless" {
                continue;
            }
            let text = Self::run(
                &[
                    "-s",
                    "-t",
                    "-e",
                    "yes",
                    "-f",
                    "connection.id,connection.autoconnect,connection.autoconnect-priority,\
                     802-11-wireless.ssid,802-11-wireless.hidden,802-11-wireless.mode,\
                     802-11-wireless-security.key-mgmt,802-11-wireless-security.psk",
                    "connection",
                    "show",
                    "uuid",
                    uuid,
                ],
                None,
            )?;
            out.push(parse(uuid, &text));
        }
        Ok(out)
    }

    fn active(&self) -> Vec<String> {
        Self::run(
            &["-t", "-f", "UUID", "connection", "show", "--active"],
            None,
        )
        .map(|o| o.lines().map(str::to_string).collect())
        .unwrap_or_default()
    }

    fn add(&self, n: &Network) -> Result<()> {
        let system = self.scope().map_err(|e| anyhow!(e))?;
        Self::run(
            &["connection", "edit", "type", "wifi"],
            Some(&self.commands(n, true, system)),
        )
        .map(|_| ())
    }

    fn change(&self, uuid: &str, n: &Network) -> Result<()> {
        let system = self.scope().map_err(|e| anyhow!(e))?;
        Self::run(
            &["connection", "edit", "uuid", uuid],
            Some(&self.commands(n, false, system)),
        )
        .map(|_| ())
    }

    fn forget(&self, uuid: &str) -> Result<()> {
        Self::run(&["connection", "delete", "uuid", uuid], None).map(|_| ())
    }

    fn export(&self, uuid: &str) -> Result<String> {
        Self::run(&["-s", "connection", "show", "uuid", uuid], None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_nmcli_and_skips_what_doesnt_sync() {
        let home = parse(
            "u1",
            "connection.id:Home\nconnection.autoconnect:yes\nconnection.autoconnect-priority:5\n\
             802-11-wireless.ssid:Ho\\:me\n802-11-wireless.hidden:no\n802-11-wireless.mode:infrastructure\n\
             802-11-wireless-security.key-mgmt:wpa-psk\n802-11-wireless-security.psk:not-a-real-key\n",
        );
        let n = home.network.unwrap();
        assert_eq!(
            (n.ssid.as_str(), n.security.as_str(), n.priority),
            ("Ho:me", "wpa-psk", 5)
        );
        assert_eq!(n.psk.as_deref(), Some("not-a-real-key"));
        let eap = parse(
            "u2",
            "connection.id:Work\n802-11-wireless.ssid:Work\n802-11-wireless-security.key-mgmt:wpa-eap\n",
        );
        assert!(eap.network.unwrap_err().contains("enterprise"));
        let agent = parse(
            "u3",
            "connection.id:Cafe\n802-11-wireless.ssid:Cafe\n802-11-wireless-security.key-mgmt:sae\n802-11-wireless-security.psk:\n",
        );
        assert!(agent.network.is_err());
        let open = parse("u4", "connection.id:Free\n802-11-wireless.ssid:Free\n");
        assert_eq!(open.network.unwrap().security, "open");
    }

    #[test]
    fn keys_go_on_stdin_and_own_networks_stay_the_users() {
        let nm = Nmcli::new("me");
        let n = Network {
            ssid: "Home".into(),
            security: "wpa-psk".into(),
            psk: Some("k".into()),
            hidden: false,
            autoconnect: true,
            priority: 0,
        };
        let c = nm.commands(&n, true, false);
        assert!(c.contains("set 802-11-wireless-security.psk k\n"));
        assert!(c.contains("set connection.permissions user:me\n"));
        assert!(c.ends_with("save persistent\nquit\n"));
        assert!(!nm.commands(&n, true, true).contains("permissions"));
    }
}
