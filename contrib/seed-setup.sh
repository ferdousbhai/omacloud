#!/usr/bin/env bash
# Set up a seed: a server that gives space to the Omacloud network and runs
# a relay for computers behind routers. Run as root on a fresh Ubuntu, with
# omacloud's source unpacked in /root/omacloud (a checkout or `git archive`).
#
#   RELAY_HOST=relay1.omacloud.computer GIVE=90G bash seed-setup.sh
#
# A Hetzner volume attached to the server, if any, holds the seed's data;
# otherwise it lives on the server's own disk. RELAY_HOST must already point
# at this server, as Let's Encrypt checks it.
set -euo pipefail

RELAY_HOST=${RELAY_HOST:?name of this server for the relay, e.g. relay1.omacloud.computer}
GIVE=${GIVE:-30G}
CONTACT=${CONTACT:-privacy@omacloud.computer}
SRC=${SRC:-/root/omacloud}
DATA=/var/lib/omacloud-seed

# the volume, mounted where the seed keeps its data
vol=$(ls /dev/disk/by-id/scsi-0HC_Volume_* 2>/dev/null | head -n1 || true)
mkdir -p "$DATA"
if [ -n "$vol" ] && ! mountpoint -q "$DATA"; then
  blkid "$vol" >/dev/null || mkfs.ext4 -q "$vol"
  grep -q "$DATA" /etc/fstab || echo "$vol $DATA ext4 discard,nofail,defaults 0 0" >>/etc/fstab
  mount "$DATA"
fi

export DEBIAN_FRONTEND=noninteractive
apt-get update -q
apt-get install -yq build-essential pkg-config curl git >/dev/null
if ! command -v cargo >/dev/null && [ ! -x /root/.cargo/bin/cargo ]; then
  curl -sSf https://sh.rustup.rs | sh -s -- -y -q --profile minimal
fi
export PATH=/root/.cargo/bin:$PATH

# omacloud (the CLI alone: no app on a server) and the relay
(cd "$SRC" && cargo build --locked --release -p omacloud)
install -m755 "$SRC/target/release/omacloud" /usr/local/bin/omacloud
cargo install --locked --quiet iroh-relay@1.3.0 --features server
install -m755 /root/.cargo/bin/iroh-relay /usr/local/bin/iroh-relay

id omacloud-seed >/dev/null 2>&1 || useradd --system --home-dir "$DATA" --shell /usr/sbin/nologin omacloud-seed
chown -R omacloud-seed: "$DATA"

mkdir -p /etc/iroh-relay
cat >/etc/iroh-relay/relay.toml <<EOF
enable_relay = true
http_bind_addr = "[::]:80"
enable_quic_addr_discovery = true
enable_metrics = false

[tls]
hostname = "$RELAY_HOST"
cert_mode = "LetsEncrypt"
cert_dir = "/var/lib/iroh-relay"
contact = "$CONTACT"
EOF

cat >/etc/systemd/system/iroh-relay.service <<'EOF'
[Unit]
Description=iroh relay for the Omacloud network
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/local/bin/iroh-relay --config-path /etc/iroh-relay/relay.toml
Restart=on-failure
RestartSec=5
# a public relay holds many connections; past the default 1024 it fails
# every accept, and logs each failure
LimitNOFILE=65536
LogRateLimitIntervalSec=30s
LogRateLimitBurst=200
AmbientCapabilities=CAP_NET_BIND_SERVICE
DynamicUser=yes
StateDirectory=iroh-relay
NoNewPrivileges=yes
ProtectSystem=strict
PrivateTmp=yes

[Install]
WantedBy=multi-user.target
EOF

cat >/etc/systemd/system/omacloud-seed.service <<EOF
[Unit]
Description=Give space to the Omacloud network
After=network-online.target iroh-relay.service
Wants=network-online.target

[Service]
User=omacloud-seed
Environment=HOME=$DATA
ExecStart=/usr/local/bin/omacloud node run --give $GIVE --relay https://$RELAY_HOST
Restart=on-failure
RestartSec=10
LimitNOFILE=65536
LogRateLimitIntervalSec=30s
LogRateLimitBurst=200
NoNewPrivileges=yes
ProtectSystem=strict
ReadWritePaths=$DATA
PrivateTmp=yes

[Install]
WantedBy=multi-user.target
EOF

# Nothing of ours reaches the relay through Cloudflare's proxy (relay
# hosts are DNS-only), but others' proxied domains can still point at an
# address a server had before: seed 1 got hundreds of websocket clients of
# such a domain, every few seconds, until it ran out of files. Refuse
# Cloudflare's addresses on the web ports.
cf4=$(curl -fsS https://www.cloudflare.com/ips-v4/ | paste -sd, -)
cf6=$(curl -fsS https://www.cloudflare.com/ips-v6/ | paste -sd, -)
[ -n "$cf4" ] && [ -n "$cf6" ]
cat >/etc/omacloud-seed.nft <<EOF
table inet omacloud_seed {}
delete table inet omacloud_seed
table inet omacloud_seed {
  chain input {
    type filter hook input priority 0; policy accept;
    tcp dport { 80, 443 } ip saddr { $cf4 } reject with tcp reset
    tcp dport { 80, 443 } ip6 saddr { $cf6 } reject with tcp reset
  }
}
EOF
cat >/etc/systemd/system/omacloud-seed-firewall.service <<'EOF'
[Unit]
Description=Keep proxied traffic off the Omacloud relay
Before=iroh-relay.service

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/sbin/nft -f /etc/omacloud-seed.nft

[Install]
WantedBy=multi-user.target
EOF

# rotate syslog by size too, not only weekly
if [ -f /etc/logrotate.d/rsyslog ] && ! grep -q maxsize /etc/logrotate.d/rsyslog; then
  sed -i 's/^\(\s*\)rotate \([0-9]*\)/\1rotate \2\n\1maxsize 200M/' /etc/logrotate.d/rsyslog
fi

systemctl daemon-reload
systemctl enable omacloud-seed-firewall iroh-relay omacloud-seed
systemctl restart omacloud-seed-firewall
# a rerun (an upgrade) starts the new binaries and settings
systemctl restart iroh-relay omacloud-seed
systemctl --no-pager --lines=5 status iroh-relay omacloud-seed || true
