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
NoNewPrivileges=yes
ProtectSystem=strict
ReadWritePaths=$DATA
PrivateTmp=yes

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable --now iroh-relay omacloud-seed
systemctl --no-pager --lines=5 status iroh-relay omacloud-seed || true
