#!/usr/bin/env bash
# Everything an account does in a bucket: computers converge, a rotation, an
# invite starting a second person's account, a second bucket for records, a lost
# computer removed and cut off by a key change, and a daemon following a new key.
#
#   scripts/s3-sync.sh [path/to/omacloud]
#
# On scripts/fake-s3.py, which needs nothing installed. To prove a provider,
# set S3_ENDPOINT, S3_REGION, S3_KEY, S3_SECRET and four empty buckets S3_BUCKET{,2,3,4};
# with S3_KEY2 and S3_SECRET2, a second key, the key change runs there too (a
# provider can't be told to retire the first from here).
set -euo pipefail
trap 'echo "failed at line $LINENO: $BASH_COMMAND"' ERR

OMACLOUD=${1:-target/debug/omacloud}
OMACLOUD=$(realpath "$OMACLOUD")
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
server=
cleanup() {
  [ -n "$server" ] && kill "$server" 2>/dev/null || true
  rm -rf "${work:?}"
}
trap cleanup EXIT

keys=
if [ -z "${S3_ENDPOINT:-}" ]; then
  S3_KEY=key-one S3_SECRET=secret-one S3_KEY2=key-two S3_SECRET2=secret-two
  S3_BUCKET=one S3_BUCKET2=two S3_BUCKET3=three S3_BUCKET4=four
  keys=$work/keys
  printf '%s\n%s\n' "$S3_KEY" "$S3_KEY2" >"$keys"
  python3 "$here/fake-s3.py" "$work/port" "$keys" one two three four &
  server=$!
  for _ in $(seq 50); do
    [ -s "$work/port" ] && break
    sleep 0.1
  done
  S3_ENDPOINT=http://127.0.0.1:$(cat "$work/port")
fi

oc() { local dev=$1; shift; "$OMACLOUD" --config "$work/$dev.toml" --styled "$@"; }
# what prints a secret prints only to a terminal: give it one, as stdin and
# stdout (stderr shows there too)
on_tty() { SHELL=/bin/bash script -qefc "$(printf '%q ' "$@")" /dev/null </dev/null | tr -d '\r'; }
oc_tty() { local dev=$1; shift; on_tty "$OMACLOUD" --config "$work/$dev.toml" "$@"; }
s3() { echo --provider s3 --endpoint "$S3_ENDPOINT" --region "${S3_REGION:-us-east-1}" --bucket "$1" --access-key-id "$S3_KEY"; }
# computer $2, its files in folder $4, asks to join with code $3, and $1
# approves it by fingerprint
join() {
  local fp
  fp=$(OMACLOUD_JOIN_CODE=$3 oc "$2" init --folder "$work/$4" --device "$2" 2>&1 | sed -n "s/^this device's fingerprint: //p")
  oc "$1" devices 2>/dev/null | grep -q "$fp" || { echo "request not visible on $1"; exit 1; }
  oc "$1" devices approve "$fp" >/dev/null
}
export OMACLOUD_SECRET_ACCESS_KEY=$S3_SECRET

# shellcheck disable=SC2046
oc_tty a init --folder "$work/A" $(s3 "$S3_BUCKET") --device a >/dev/null
[ "$(stat -c %a "$work/a.toml")" = 600 ] || { echo "config with the bucket's key is readable by others"; exit 1; }

# a second computer joins with a code, and is approved by fingerprint
join a b "$(oc_tty a join-code | grep '^omacloud-join-')" B

mkdir -p "$work/A/docs"
echo "over s3" >"$work/A/docs/a.txt"
head -c 5000000 /dev/urandom >"$work/A/big.bin"
oc a sync >/dev/null
oc b sync >/dev/null
echo "back from b" >"$work/B/docs/b.txt"
rm "$work/B/big.bin"
oc b sync >/dev/null
oc a sync >/dev/null
diff -r --no-dereference "$work/A" "$work/B"
[ ! -e "$work/A/big.bin" ]
echo "two computers: ok"

# a provider having a moment: reads are tried again, and nothing shows
if [ -n "$keys" ]; then
  echo flaky >>"$keys"
  echo "through a bad moment" >"$work/A/docs/flaky.txt"
  oc a sync >/dev/null
  oc b sync >/dev/null
  diff -r --no-dereference "$work/A" "$work/B"
  sed -i '/^flaky$/d' "$keys"
  echo "retries: ok"
fi

# the status names the bucket, and never its key
oc a status | grep -q "bucket $S3_BUCKET"
if "$OMACLOUD" --config "$work/a.toml" status --json | grep -q "$S3_SECRET"; then
  echo "the bucket's secret in status"; exit 1
fi

# a wrong code for this bucket is refused
OMACLOUD_RECOVERY_CODE=nonsense oc x init --folder "$work/X" $(s3 "$S3_BUCKET") --device x >/dev/null 2>&1 \
  && { echo "a bad recovery code got in"; exit 1; }

# rotation moves the repository to a new root in the same bucket
oc a rotate --yes >/dev/null
echo "after rotation" >"$work/A/docs/rotated.txt"
oc a sync >/dev/null
oc b sync >/dev/null
diff -r --no-dereference "$work/A" "$work/B"
echo "rotation: ok"

# leaving: plain restic reads the bucket with what export prints
if command -v restic >/dev/null; then
  cmd=$(oc_tty a export | sed -n '/^AWS_ACCESS_KEY_ID/,$p' | sed "s#<dir>#$work/restored#")
  bash -c "$cmd" >/dev/null
  [ "$(find "$work/restored" -name rotated.txt | wc -l)" = 1 ] || { echo "restic restored nothing"; exit 1; }
  echo "plain restic: ok"
else
  oc_tty a export | grep -q "restic -r s3:$S3_ENDPOINT/$S3_BUCKET/omacloud-e1"
  echo "plain restic: skipped, not installed"
fi

# an invite: a second person's own account, that the first can't open
# shellcheck disable=SC2046
invite=$(oc_tty none invite $(s3 "$S3_BUCKET2") | grep '^omacloud-join-')
OMACLOUD_JOIN_CODE=$invite oc_tty kid init --folder "$work/Kid" --device kid >"$work/kid.out"
grep -q 'recovery code:' "$work/kid.out" || { cat "$work/kid.out"; echo "the invite made no account"; exit 1; }
echo "theirs" >"$work/Kid/theirs.txt"
oc kid sync >/dev/null
# the same invite again: that person's second computer asks to join
join kid kid2 "$invite" Kid2
oc kid2 sync >/dev/null
diff -r "$work/Kid" "$work/Kid2"
# and a bucket holding an account takes no invite
# shellcheck disable=SC2046
refused=$(oc_tty none invite $(s3 "$S3_BUCKET2") 2>&1 || true)
echo "$refused" | grep -q 'already holds an account' || { echo "$refused"; exit 1; }
echo "invite: ok"

# object-lock accounts keep their records in a second bucket
# shellcheck disable=SC2046
oc_tty lock init --folder "$work/Lock" $(s3 "$S3_BUCKET3") --root /data --coordination-bucket "$S3_BUCKET4" --device lock >"$work/lock.out" \
  || { grep -a -i 'error\|hint' "$work/lock.out"; exit 1; }
echo "kept" >"$work/Lock/kept.txt"
oc lock sync >/dev/null
echo "second bucket: ok"

# a lost computer: removed, then the key changes, sealed to the rest
join a c "$(oc_tty a join-code | grep '^omacloud-join-')" C
oc a sync >/dev/null
oc b sync >/dev/null
oc c sync >/dev/null
fpb=$(oc b device-key | sed -n 's/^fingerprint //p')
oc a devices revoke "$fpb" --rotate --yes >/dev/null
oc b sync >/dev/null 2>&1 && { echo "a removed computer still syncs"; exit 1; }
refused=$(oc a bucket set-key --access-key-id "$S3_KEY" 2>&1 || true)
echo "$refused" | grep -q 'is the key in use' || { echo "$refused"; exit 1; }
if [ -n "${S3_KEY2:-}" ]; then
  OMACLOUD_SECRET_ACCESS_KEY=$S3_SECRET2 oc a bucket set-key --access-key-id "$S3_KEY2" >/dev/null
  oc c sync >/dev/null
  oc a bucket status | grep -q 'every computer switched' || { oc a bucket status; echo "c didn't switch"; exit 1; }
  oc a bucket status | grep -q '^c .*switched$' || { oc a bucket status; echo "status doesn't name c"; exit 1; }
  # a daemon running when its key changes restarts into the new one
  ended=0
  timeout 30 "$OMACLOUD" --config "$work/a.toml" watch --poll 1 >/dev/null 2>&1 &
  daemon=$!
  sleep 2
  OMACLOUD_SECRET_ACCESS_KEY=$S3_SECRET oc a bucket set-key --access-key-id "$S3_KEY" --here-only >/dev/null
  wait "$daemon" || ended=$?
  [ "$ended" = 75 ] || { echo "the daemon went on with the old key: it ended with $ended"; exit 1; }
  OMACLOUD_SECRET_ACCESS_KEY=$S3_SECRET2 oc a bucket set-key --access-key-id "$S3_KEY2" --here-only >/dev/null
  if [ -n "$keys" ]; then
    # the old key goes at the provider: the removed computer has only that
    echo "$S3_KEY2" >"$keys"
    b_says=$("$OMACLOUD" --config "$work/b.toml" sync --json || true)
    echo "$b_says" | grep -q '"key_refused"' || { echo "$b_says"; echo "the old key still works"; exit 1; }
  fi
  echo "after the key change" >"$work/A/docs/rekeyed.txt"
  oc a sync >/dev/null
  oc c sync >/dev/null
  diff -r --no-dereference "$work/A" "$work/C"
fi
echo "lost computer: ok"

echo "s3 sync: ok"
