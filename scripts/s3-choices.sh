#!/usr/bin/env bash
# Two running daemons keep up while files change and folders are chosen: edits
# and deletes travel, a folder skipped and taken back loses nothing, a subfolder
# stays out, and a folder removed from the cloud stays on both computers and can
# come back. No daemon is restarted along the way.
#
#   scripts/s3-choices.sh [path/to/omacloud]
#
# On scripts/fake-s3.py. Each computer's home is a directory of the test's.
set -euo pipefail

OMACLOUD=${1:-target/debug/omacloud}
OMACLOUD=$(realpath "$OMACLOUD")
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
server= watch_a= watch_b=
cleanup() {
  for pid in "$watch_a" "$watch_b" "$server"; do
    [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
  done
  wait 2>/dev/null || true
  rm -rf "${work:?}"
}
trap cleanup EXIT

printf 'key-one\n' >"$work/keys"
python3 "$here/fake-s3.py" "$work/port" "$work/keys" one &
server=$!
for _ in $(seq 50); do
  [ -s "$work/port" ] && break
  sleep 0.1
done
endpoint=http://127.0.0.1:$(cat "$work/port")
export OMACLOUD_SECRET_ACCESS_KEY=secret-one

mkdir -p "$work/ha/Pictures" "$work/ha/Documents/Notes" "$work/hb"
oc() { local dev=$1; shift; HOME=$work/h$dev "$OMACLOUD" --config "$work/$dev.toml" --styled "$@"; }
oc_tty() {
  local dev=$1; shift
  HOME=$work/h$dev SHELL=/bin/bash script -qefc "$(printf '%q ' "$OMACLOUD" --config "$work/$dev.toml" "$@")" /dev/null </dev/null | tr -d '\r'
}
json() { local dev=$1; shift; HOME=$work/h$dev "$OMACLOUD" --config "$work/$dev.toml" --json "$@"; }
# wait for a condition: the daemons sync on their own, or this fails
until_ok() {
  local what=$1; shift
  for _ in $(seq 120); do
    "$@" 2>/dev/null && { echo "$what: ok"; return; }
    sleep 0.5
  done
  echo "$what: not within a minute"; tail -n 8 "$work/a.log" "$work/b.log"; json b folders list || true; exit 1
}
has() { [ "$(cat "$1")" = "$2" ]; }
stays_absent() {
  sleep 8
  [ ! -e "$1" ] || { echo "$1 should have stayed out"; exit 1; }
}

echo hello >"$work/ha/Pictures/cat"
echo keep >"$work/ha/Documents/Notes/note"
echo top >"$work/ha/Documents/top"
oc_tty a init --folders Pictures,Documents --provider s3 --endpoint "$endpoint" --region us-east-1 \
  --bucket one --access-key-id key-one --device a >/dev/null
code=$(oc_tty a join-code | grep '^omacloud-join-')
fp=$(OMACLOUD_JOIN_CODE=$code oc b init --folders Pictures,Documents --device b 2>&1 | sed -n "s/^this device's fingerprint: //p")
oc a devices approve "$fp" >/dev/null
oc a sync >/dev/null
oc b sync >/dev/null

HOME=$work/ha "$OMACLOUD" --config "$work/a.toml" watch --poll 1 >"$work/a.log" 2>&1 &
watch_a=$!
HOME=$work/hb "$OMACLOUD" --config "$work/b.toml" watch --poll 1 >"$work/b.log" 2>&1 &
watch_b=$!
A=$work/ha B=$work/hb

# files as they change
echo edited >"$A/Pictures/cat"
until_ok "an edit travels" has "$B/Pictures/cat" edited
mkdir -p "$B/Pictures/trip/day1"
echo sea >"$B/Pictures/trip/day1/photo"
until_ok "a new folder of files travels" has "$A/Pictures/trip/day1/photo" sea
mv "$A/Pictures/trip" "$A/Pictures/holiday"
until_ok "a rename travels" has "$B/Pictures/holiday/day1/photo" sea
until_ok "and the old name goes" test ! -e "$B/Pictures/trip"
rm "$B/Pictures/cat"
until_ok "a delete travels" test ! -e "$A/Pictures/cat"

# a folder skipped: the files stay, and neither side's changes cross
oc a folders skip Documents >/dev/null
sleep 3
echo "a alone" >"$A/Documents/while-off"
echo "b's" >"$B/Documents/from-b"
stays_absent "$A/Documents/from-b"
[ ! -e "$B/Documents/while-off" ] || { echo "a skipped folder sent a file"; exit 1; }
has "$A/Documents/top" top
echo "skipping holds: ok"

# and taken back: both computers end with both files
oc a folders unskip Documents >/dev/null
until_ok "a folder back takes in others' files" has "$A/Documents/from-b" "b's"
until_ok "a folder back sends its own" has "$B/Documents/while-off" "a alone"

# a subfolder left out
oc a folders exclude Documents/Notes >/dev/null
sleep 3
echo private >"$A/Documents/Notes/private"
echo more >"$A/Documents/more"
until_ok "the rest of the folder keeps syncing" has "$B/Documents/more" more
stays_absent "$B/Documents/Notes/private"
echo "a subfolder left out stays out: ok"
oc a folders include Documents/Notes >/dev/null
until_ok "a subfolder taken back syncs" has "$B/Documents/Notes/private" private

# removed from the cloud: both keep their files, and stop syncing it
oc a folders remove Documents --yes >/dev/null
removed_at() {
  json "$1" folders list | python3 -c 'import json, sys; sys.exit(json.load(sys.stdin)["data"]["removed_from_cloud"] != ["Documents"])'
}
until_ok "the other computer hears of a removal" removed_at b
has "$A/Documents/Notes/private" private
has "$B/Documents/Notes/private" private
echo "only b" >"$B/Documents/only-b"
stays_absent "$A/Documents/only-b"
echo "pictures still" >"$A/Pictures/still"
until_ok "other folders go on" has "$B/Pictures/still" "pictures still"

# and brought back from either
oc a folders add Documents >/dev/null
until_ok "a removed folder comes back" has "$A/Documents/only-b" "only b"

json a folders list | python3 -c '
import json, sys
d = json.load(sys.stdin)["data"]
sizes = {f["name"]: f["size"] for f in d["synced"]}
assert sizes["Documents"] > 0, sizes
assert d["removed_from_cloud"] == [], d
'
echo "folders list reports sizes: ok"

kill -0 "$watch_a" && kill -0 "$watch_b" || { echo "a daemon stopped"; exit 1; }
echo "s3 choices: all ok"
