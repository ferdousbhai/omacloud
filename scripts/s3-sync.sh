#!/usr/bin/env bash
# Two devices sync through an S3 repository and converge.
#
#   S3_ENDPOINT=http://localhost:8333 S3_BUCKET=omacloud-ci \
#   S3_KEY=... S3_SECRET=... scripts/s3-sync.sh [path/to/omacloud]
#
# The bucket (or S3_ROOT inside it) must exist and be empty.
set -euo pipefail

OMACLOUD=${1:-target/debug/omacloud}
: "${S3_ENDPOINT:?} ${S3_BUCKET:?} ${S3_KEY:?} ${S3_SECRET:?}"
work=$(mktemp -d)
cleanup() {
  for f in "$work"/*.toml "$work"/inbucket/*.toml; do
    secret-tool clear service omacloud config "$f" 2>/dev/null || true
  done
  rm -rf "${work:?}"
}
trap cleanup EXIT
oc() { local dev=$1; shift; "$OMACLOUD" --config "$work/$dev.toml" --styled "$@"; }
# what prints a secret (a new account's recovery code, a join code, export)
# prints only to a terminal: give it one, as stdin and stdout (stderr shows
# there too)
on_tty() { SHELL=/bin/bash script -qefc "$(printf '%q ' "$@")" /dev/null </dev/null | tr -d '\r'; }
oc_tty() { local dev=$1; shift; on_tty "$OMACLOUD" --config "$work/$dev.toml" "$@"; }
s3=(--repo opendal:s3 --opt "endpoint=$S3_ENDPOINT" --opt "bucket=$S3_BUCKET"
    --opt "access_key_id=$S3_KEY"
    --opt "region=${S3_REGION:-us-east-1}")
[ -n "${S3_ROOT:-}" ] && s3+=(--opt "root=$S3_ROOT")

OMACLOUD_SECRET_ACCESS_KEY=$S3_SECRET oc_tty a init --folder "$work/A" "${s3[@]}" --coordinator "$work/coord" --device a >/dev/null
# b asks to join, knowing nothing of the bucket; a approves it by fingerprint
oc b init --folder "$work/B" --coordinator "$work/coord" --device b >/dev/null 2>&1
fp=$(oc b device-key 2>/dev/null | sed -n 's/^fingerprint //p')
oc a devices 2>/dev/null | grep -q "$fp" || { echo "request not visible on a"; exit 1; }
oc a devices approve "$fp" >/dev/null 2>&1

mkdir -p "$work/A/docs"
echo "over s3" >"$work/A/docs/a.txt"
head -c 5000000 /dev/urandom >"$work/A/big.bin"
oc a sync >/dev/null 2>&1
oc b sync >/dev/null 2>&1
echo "back from b" >"$work/B/docs/b.txt"
rm "$work/B/big.bin"
oc b sync >/dev/null 2>&1
oc a sync >/dev/null 2>&1
diff -r --no-dereference "$work/A" "$work/B"
[ ! -e "$work/A/big.bin" ]
# the bucket's credentials reached b sealed, and no config file holds them
if grep -q "$S3_SECRET" "$work/a.toml" "$work/b.toml"; then
  echo "bucket credentials in a config file"; exit 1
fi

# key rotation moves the repository to a new prefix in the same bucket
oc a rotate --yes >/dev/null 2>&1
echo "after rotation" >"$work/A/docs/rotated.txt"
oc a sync >/dev/null 2>&1
oc b sync >/dev/null 2>&1
diff -r --no-dereference "$work/A" "$work/B"
oc_tty b export | grep -q "root = \"${S3_ROOT:-}/omacloud-e1\""
echo "s3 sync: ok"

# coordination in the bucket too, and a new
# device joins with a code from an existing one
nw="$work/inbucket"
mkdir -p "$nw/A"
echo "mine alone" >"$nw/A/mine.txt"
ns() { local dev=$1; shift; "$OMACLOUD" --config "$nw/$dev.toml" --styled "$@"; }
ns_tty() { local dev=$1; shift; on_tty "$OMACLOUD" --config "$nw/$dev.toml" "$@"; }
root_opt=${S3_ROOT:-}/inbucket
OMACLOUD_SECRET_ACCESS_KEY=$S3_SECRET ns_tty a init --folder "$nw/A" "${s3[@]}" --opt "root=$root_opt" --coordinator bucket --device a >/dev/null
ns a sync >/dev/null 2>&1
code=$(ns_tty a join-code | grep '^omacloud-join-')
# the code holds the bucket's keys: never on a command line
fp=$(OMACLOUD_JOIN_CODE=$code ns b init --folder "$nw/B" --device b 2>&1 | sed -n "s/^this device's fingerprint: //p")
ns a devices approve "$fp" >/dev/null 2>&1
ns b sync >/dev/null 2>&1
echo "from b" >"$nw/B/b.txt"
ns b sync >/dev/null 2>&1
ns a sync >/dev/null 2>&1
diff -r "$nw/A" "$nw/B"
[ "$(stat -c %a "$nw/a.toml")" = 600 ] || { echo "config with bucket keys is readable by others"; exit 1; }
echo "coordination in the bucket: ok"
