#!/usr/bin/env bash
# The Omacloud network on this machine: three seed nodes give space, then
# two computers of one account sync through them, joined by a join code
# and approval, and survive a key rotation. The Worker's part is a
# directory (OMACLOUD_NETWORK=dir:...), so it needs no server.
#
#   scripts/network.sh [path/to/omacloud]
set -euo pipefail

OMACLOUD=${1:-target/debug/omacloud}
work=$(mktemp -d)
pids=()
cleanup() {
  for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done
  wait 2>/dev/null || true
  rm -rf "${work:?}"
}
trap cleanup EXIT
export OMACLOUD_NETWORK=${OMACLOUD_NETWORK:-dir:$work/worker}
oc() { local dev=$1; shift; "$OMACLOUD" --config "$work/$dev.toml" --styled "$@"; }
# what prints a secret prints only to a terminal: give it one
oc_tty() {
  local dev=$1; shift
  SHELL=/bin/bash script -qefc "$(printf '%q ' "$OMACLOUD" --config "$work/$dev.toml" "$@")" /dev/null </dev/null | tr -d '\r'
}

for s in s1 s2 s3; do
  "$OMACLOUD" --config "$work/$s.toml" node run --give 1G >"$work/$s.log" 2>&1 &
  pids+=($!)
done
listed() {
  case $OMACLOUD_NETWORK in
    dir:*) ls "${OMACLOUD_NETWORK#dir:}/nodes" 2>/dev/null | wc -l ;;
    *) curl -s "$OMACLOUD_NETWORK/api/net/nodes" | grep -o '"id"' | wc -l ;;
  esac
}
for _ in $(seq 1 100); do
  [ "$(listed)" -ge 3 ] && break
  sleep 0.1
done
[ "$(listed)" -ge 3 ] || { echo "seed nodes didn't list themselves"; cat "$work"/s*.log; exit 1; }

oc_tty a init --folder "$work/A" --device a >"$work/init.txt"
grep -q "on the Omacloud network" "$work/init.txt" || { cat "$work/init.txt"; exit 1; }
recovery=$(sed -n 's/^recovery code: *//p' "$work/init.txt")
[ -n "$recovery" ] || { echo "no recovery code in init output"; exit 1; }

# b asks to join with a join code, and a approves it by fingerprint
code=$(oc_tty a join-code | grep '^omacloud-join-')
OMACLOUD_JOIN_CODE=$code oc b init --folder "$work/B" --device b >"$work/join.txt" 2>&1
fp=$(sed -n "s/^this device's fingerprint: *//p" "$work/join.txt")
[ -n "$fp" ] || { cat "$work/join.txt"; exit 1; }
oc a devices approve "$fp" >/dev/null

mkdir -p "$work/A/docs/deep" "$work/B/src"
echo "from a" >"$work/A/docs/deep/a.txt"
head -c 3000000 /dev/urandom >"$work/A/blob.bin"
echo "from b" >"$work/B/src/b.rs"
oc a sync >/dev/null 2>&1
oc b sync >/dev/null 2>&1
oc a sync >/dev/null 2>&1
diff -r --no-dereference "$work/A" "$work/B"

# c recovers with the recovery code alone
OMACLOUD_RECOVERY_CODE=$recovery oc c init --folder "$work/C" --device c >/dev/null 2>&1
oc c sync >/dev/null 2>&1
diff -r --no-dereference "$work/A" "$work/C"

# a new epoch: new keys, new bucket, and everyone follows
oc a rotate --yes >/dev/null 2>&1
echo "after rotation" >"$work/A/docs/rotated.txt"
oc a sync >/dev/null 2>&1
oc b sync >/dev/null 2>&1
diff -r --no-dereference "$work/A" "$work/B"

# leaving: the repository copied to a folder, which plain restic reads
oc_tty a export --to "$work/copy" >"$work/export.txt"
password=$(sed -n 's/^password: *//p' "$work/export.txt")
[ -n "$password" ] && [ -f "$work/copy/config" ] || { cat "$work/export.txt"; exit 1; }
if command -v "${RESTIC:-restic}" >/dev/null; then
  RESTIC_PASSWORD=$password ${RESTIC:-restic} -r "$work/copy" --no-lock restore latest --target "$work/restored" >/dev/null
  diff -r --no-dereference "$work/A" "$work/restored"
fi

# a file shared by link opens on a computer with no account
link=$(oc_tty a share "$work/A/docs/deep/a.txt" | grep '/s#')
[ -n "$link" ] || { echo "no share link"; exit 1; }
OMACLOUD_LINK=$link oc c receive --to "$work/got" >/dev/null
cmp "$work/A/docs/deep/a.txt" "$work/got/a.txt"

# the Worker's part holds nothing readable: no file names, no contents
if [ -d "$work/worker" ] && grep -rqa -e "from a" -e "a.txt" -e "docs" "$work/worker"; then
  echo "the Worker's part holds something readable"; exit 1
fi
echo "network: ok"
