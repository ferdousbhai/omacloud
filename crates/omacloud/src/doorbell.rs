//! The doorbell: a computer that pushed says so straight to the account's
//! other computers, over Tailscale or the local network, and they look at
//! the storage at once instead of at their next poll. It carries nothing
//! but "changed": the storage stays the only place anything is read from,
//! and a computer the bell doesn't reach still finds the change by polling.
//!
//! Omarchy's firewall refuses what comes in unasked, so each computer
//! greets the others every 20 seconds from the same port it listens on:
//! a greeting both ways makes the other's packets replies, which the
//! firewall lets through. Every packet is a fresh nonce and a tag made
//! with the account's repository password, so a stranger on the network
//! can neither ring it nor tell whose computer sent it. Inside, masked
//! afresh each time, is which computer sent it: so each knows which of the
//! others it can hear, and when that's all of them, it barely polls.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use log::{debug, info};
use sha2::{Digest, Sha256};

/// Where it listens, unless `OMACLOUD_DOORBELL_PORT` says otherwise.
pub const PORT: u16 = 41760;
const GREET_EVERY: Duration = Duration::from_secs(20);
const HELLO: u8 = 0;
const CHANGED: u8 = 1;
const LEN: usize = 2 + 1 + 16 + 16 + 16;
/// A computer not heard from for this long may not hear this one either.
const HEARD_WITHIN: Duration = Duration::from_secs(70);
/// The port this daemon's doorbell listens on, 0 for none.
static LISTENING: AtomicU16 = AtomicU16::new(0);

/// Where this computer's doorbell can be reached on its network, to
/// publish with its choices: none when it has none.
#[must_use]
pub fn reach() -> Option<String> {
    let port = LISTENING.load(Ordering::Relaxed);
    (port != 0).then(|| lan_ip().map(|ip| SocketAddr::new(ip, port).to_string()))?
}

pub struct Doorbell {
    socket: UdpSocket,
    secret: Arc<Mutex<String>>,
    /// Addresses the other computers published (`ip:port`).
    published: Arc<Mutex<Vec<SocketAddr>>>,
    rung: Arc<AtomicBool>,
    /// Each computer heard from, by its masked-in id, and when.
    heard: Arc<Mutex<HashMap<[u8; 16], Instant>>>,
    /// This computer's device key, which its id comes from.
    me: String,
    port: u16,
}

/// The port to use, or none: the default account uses [`PORT`]; a test
/// account only when given one.
#[must_use]
pub fn port(default_account: bool) -> Option<u16> {
    match std::env::var("OMACLOUD_DOORBELL_PORT") {
        Ok(p) => p.parse().ok(),
        Err(_) => default_account.then_some(PORT),
    }
}

impl Doorbell {
    /// Listen, and greet the others every 20 seconds. `wake` is called when
    /// another computer rings.
    pub fn start(port: u16, secret: String, me: String, wake: impl Fn() + Send + 'static) -> std::io::Result<Self> {
        let socket = UdpSocket::bind(("0.0.0.0", port))?;
        let doorbell = Self {
            socket,
            secret: Arc::new(Mutex::new(secret)),
            published: Arc::new(Mutex::new(Vec::new())),
            rung: Arc::new(AtomicBool::new(false)),
            heard: Arc::new(Mutex::new(HashMap::new())),
            me,
            port,
        };
        let listener = doorbell.clone_for_thread()?;
        std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            loop {
                let Ok((n, from)) = listener.socket.recv_from(&mut buf) else {
                    continue;
                };
                let key = listener.key();
                let Some((kind, id)) = verify(&key, &buf[..n]) else {
                    continue;
                };
                if id == device_id(&key, &listener.me) {
                    continue;
                }
                if let Ok(mut h) = listener.heard.lock() {
                    _ = h.insert(id, Instant::now());
                }
                if kind == CHANGED {
                    debug!("rung by {from}");
                    listener.rung.store(true, Ordering::SeqCst);
                    wake();
                }
            }
        });
        let greeter = doorbell.clone_for_thread()?;
        std::thread::spawn(move || {
            loop {
                greeter.send(HELLO);
                std::thread::sleep(GREET_EVERY);
            }
        });
        LISTENING.store(port, Ordering::Relaxed);
        info!("doorbell on port {port}");
        Ok(doorbell)
    }

    fn clone_for_thread(&self) -> std::io::Result<Self> {
        Ok(Self {
            socket: self.socket.try_clone()?,
            secret: self.secret.clone(),
            published: self.published.clone(),
            rung: self.rung.clone(),
            heard: self.heard.clone(),
            me: self.me.clone(),
            port: self.port,
        })
    }

    fn key(&self) -> String {
        self.secret.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// How many other computers were heard from lately: they hear this one
    /// ring too.
    #[must_use]
    pub fn heard(&self) -> usize {
        self.heard
            .lock()
            .map(|h| h.values().filter(|t| t.elapsed() < HEARD_WITHIN).count())
            .unwrap_or(0)
    }

    /// Tell the others something changed.
    pub fn ring(&self) {
        self.send(CHANGED);
    }

    /// Whether another computer rang since this was last asked.
    pub fn was_rung(&self) -> bool {
        self.rung.swap(false, Ordering::SeqCst)
    }

    /// The account's password changed (a new epoch).
    pub fn set_secret(&self, secret: String) {
        if let Ok(mut s) = self.secret.lock() {
            *s = secret;
        }
    }

    /// The addresses other computers published.
    pub fn set_published(&self, addrs: Vec<SocketAddr>) {
        if let Ok(mut p) = self.published.lock() {
            *p = addrs;
        }
    }

    fn send(&self, kind: u8) {
        let key = self.key();
        if key.is_empty() {
            return;
        }
        let packet = make(&key, kind, &self.me);
        let mine = lan_ip();
        // a published address only on this network: one from a network
        // this computer left may be someone else's now
        let mut to: Vec<SocketAddr> = self
            .published
            .lock()
            .map(|p| p.iter().filter(|a| same_network(mine, a.ip())).copied().collect())
            .unwrap_or_default();
        to.extend(tailscale_peers().into_iter().map(|ip| SocketAddr::new(ip, self.port)));
        to.sort();
        to.dedup();
        for addr in to {
            if Some(addr.ip()) == mine && addr.port() == self.port {
                continue;
            }
            _ = self.socket.send_to(&packet, addr);
        }
    }
}

fn hash16(parts: &[&[u8]]) -> [u8; 16] {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    let mut out = [0u8; 16];
    out.copy_from_slice(&h.finalize()[..16]);
    out
}

/// Which computer this is, as only the account can tell.
fn device_id(key: &str, me: &str) -> [u8; 16] {
    hash16(&[b"omacloud doorbell id\0", key.as_bytes(), me.as_bytes()])
}

fn mask(key: &str, nonce: &[u8]) -> [u8; 16] {
    hash16(&[b"omacloud doorbell mask\0", key.as_bytes(), nonce])
}

fn tag(key: &str, body: &[u8]) -> [u8; 16] {
    hash16(&[b"omacloud doorbell\0", key.as_bytes(), body])
}

/// `OC`, the kind, a nonce, the sender's id masked with it, and a tag.
fn make(key: &str, kind: u8, me: &str) -> Vec<u8> {
    static COUNT: AtomicU64 = AtomicU64::new(0);
    // unique, not secret: the tag is what can't be forged
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let nonce = hash16(&[
        &now.to_le_bytes(),
        &COUNT.fetch_add(1, Ordering::Relaxed).to_le_bytes(),
        &std::process::id().to_le_bytes(),
    ]);
    let (id, m) = (device_id(key, me), mask(key, &nonce));
    let mut p = Vec::with_capacity(LEN);
    p.extend_from_slice(b"OC");
    p.push(kind);
    p.extend_from_slice(&nonce);
    p.extend(id.iter().zip(m).map(|(a, b)| a ^ b));
    let t = tag(key, &p[2..]);
    p.extend_from_slice(&t);
    p
}

/// The kind and sender of a genuine packet.
fn verify(key: &str, p: &[u8]) -> Option<(u8, [u8; 16])> {
    if key.is_empty() || p.len() != LEN || &p[..2] != b"OC" {
        return None;
    }
    if tag(key, &p[2..LEN - 16]) != p[LEN - 16..] {
        return None;
    }
    let (nonce, masked) = (&p[3..19], &p[19..35]);
    let mut id = [0u8; 16];
    for ((o, a), b) in id.iter_mut().zip(masked).zip(mask(key, nonce)) {
        *o = a ^ b;
    }
    Some((p[2], id))
}

/// This computer's address on its local network: the one it would send
/// from (nothing is sent).
fn lan_ip() -> Option<IpAddr> {
    let s = UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    s.connect(("192.0.2.1", 9)).ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_unspecified()).then_some(ip)
}

/// Whether `ip` is on this computer's local network, as a home or office
/// network usually is (the same /24).
fn same_network(mine: Option<IpAddr>, ip: IpAddr) -> bool {
    match (mine, ip) {
        (Some(IpAddr::V4(a)), IpAddr::V4(b)) => a.octets()[..3] == b.octets()[..3],
        _ => false,
    }
}

/// The other Linux computers on this tailnet that are online, when
/// Tailscale runs.
fn tailscale_peers() -> Vec<IpAddr> {
    let Ok(out) = std::process::Command::new("tailscale")
        .args(["status", "--json"])
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
        return Vec::new();
    };
    v["Peer"]
        .as_object()
        .into_iter()
        .flatten()
        // Omarchy computers only: a phone's radio needn't wake for these
        .filter(|(_, p)| p["Online"].as_bool() == Some(true) && p["OS"] == "linux")
        .filter_map(|(_, p)| {
            p["TailscaleIPs"]
                .as_array()?
                .iter()
                .filter_map(|ip| ip.as_str()?.parse::<IpAddr>().ok())
                .find(IpAddr::is_ipv4)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_account_can_ring() {
        let p = make("secret", CHANGED, "laptop");
        assert_eq!(verify("secret", &p), Some((CHANGED, device_id("secret", "laptop"))));
        assert_eq!(verify("other", &p), None);
        let mut forged = p.clone();
        forged[2] = HELLO;
        assert_eq!(verify("secret", &forged), None);
        // two packets from one computer look unrelated
        let q = make("secret", CHANGED, "laptop");
        assert_ne!(p[19..35], q[19..35]);
    }
}
