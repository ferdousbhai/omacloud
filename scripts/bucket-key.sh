#!/usr/bin/env bash
# A self-hosted account changes its bucket key while one computer is
# offline; the old key is deleted before that computer comes back, and it
# catches up with `bucket set-key --here-only`. Runs its own SeaweedFS
# container, with keys made through its admin shell so one can be deleted.
#
#   scripts/bucket-key.sh [path/to/onecloud]
set -euo pipefail

ONECLOUD=${1:-target/debug/onecloud}
S3_PORT=${S3_PORT:-8337}
S3_ENDPOINT=http://127.0.0.1:$S3_PORT
work=$(mktemp -d)
cleanup() {
  docker stop onecloud-bucket-key >/dev/null 2>&1 || true
  for f in "$work"/*.toml; do secret-tool clear service onecloud config "$f" 2>/dev/null || true; done
  rm -rf "${work:?}"
}
trap cleanup EXIT

weed() { docker exec -i onecloud-bucket-key weed shell >/dev/null 2>&1 <<<"$1"; }
reaches() { # key secret: can it list the bucket?
  curl -sf -o /dev/null --aws-sigv4 "aws:amz:us-east-1:s3" --user "$1:$2" "$S3_ENDPOINT/onecloud?list-type=2"
}
docker run -d --rm --name onecloud-bucket-key -p "$S3_PORT:8333" \
  chrislusf/seaweedfs server -s3 -dir=/data >/dev/null
for _ in $(seq 1 60); do curl -s -o /dev/null "$S3_ENDPOINT" && break; sleep 1; done
for k in old new; do
  for _ in $(seq 1 30); do
    weed "s3.configure -user=$k -access_key=$k -secret_key=$k-secret -actions=Admin,Read,Write,List,Tagging -apply" && break
    sleep 1
  done
done
for _ in $(seq 1 30); do
  curl -sf -X PUT --aws-sigv4 "aws:amz:us-east-1:s3" --user old:old-secret "$S3_ENDPOINT/onecloud" && break
  sleep 1
done
reaches new new-secret

oc() { local dev=$1; shift; "$ONECLOUD" --config "$work/$dev.toml" "$@"; }
say() { echo "bucket key: $*"; }

# a, b and c on one self-hosted account, all with the old key
mkdir -p "$work/A"
echo "from a" >"$work/A/a.txt"
ONECLOUD_SECRET_ACCESS_KEY=old-secret oc a init --folder "$work/A" --repo opendal:s3 \
  --opt "endpoint=$S3_ENDPOINT" --opt bucket=onecloud --opt access_key_id=old --opt region=us-east-1 \
  --coordinator bucket --device a >/dev/null 2>&1
oc a sync >/dev/null 2>&1
for d in b c; do
  code=$(oc a join-code 2>/dev/null)
  fp=$(ONECLOUD_JOIN_CODE=$code oc "$d" init --folder "$work/${d^^}" --device "$d" 2>&1 |
    sed -n "s/^this device's fingerprint: //p")
  [ -n "$fp" ] || { echo "$d didn't get a fingerprint"; exit 1; }
  oc a devices approve "$fp" >/dev/null 2>&1
  oc "$d" sync >/dev/null 2>&1
done
diff -r "$work/A" "$work/C"
say "three computers on the old key"

# a changes the key while c is offline; b follows on its next sync
if ONECLOUD_SECRET_ACCESS_KEY=old-secret oc a bucket set-key --access-key-id old >/dev/null 2>&1; then
  echo "a changed to the key already in use"; exit 1
fi
ONECLOUD_SECRET_ACCESS_KEY=new-secret oc a bucket set-key --access-key-id new >/dev/null 2>&1
oc b sync 2>/dev/null | grep -q "switched to the account's new bucket key" ||
  { echo "b didn't switch"; exit 1; }
status=$(oc a bucket status 2>/dev/null)
grep -q "1 computers still to switch" <<<"$status" || { echo "$status"; exit 1; }
say "a changed it, b switched, c still to"

# the old key is deleted before c comes back: c can't reach the bucket
weed "s3.configure -user=old -access_key=old -delete -apply"
for _ in $(seq 1 30); do reaches old old-secret || break; sleep 1; done
! reaches old old-secret || { echo "the old key still works"; exit 1; }
echo "while c was away" >"$work/A/away.txt"
oc a sync >/dev/null 2>&1
if out=$(oc c sync 2>&1); then echo "c synced with a deleted key"; exit 1; fi
grep -q -- "--here-only" <<<"$out" || { echo "no way back offered: $out"; exit 1; }
say "old key deleted, c cut off"

# c takes the new key by hand and catches up
if ONECLOUD_SECRET_ACCESS_KEY=wrong oc c bucket set-key --access-key-id new --here-only >/dev/null 2>&1; then
  echo "c took a key that doesn't reach the bucket"; exit 1
fi
ONECLOUD_SECRET_ACCESS_KEY=new-secret oc c bucket set-key --access-key-id new --here-only 2>/dev/null |
  grep -q "this computer uses the new key" || { echo "c didn't take the new key"; exit 1; }
oc c sync >/dev/null 2>&1
echo "from c" >"$work/C/c.txt"
oc c sync >/dev/null 2>&1
oc a sync >/dev/null 2>&1
oc b sync >/dev/null 2>&1
diff -r "$work/A" "$work/C"
diff -r "$work/A" "$work/B"
status=$(oc a bucket status 2>/dev/null)
grep -q "every computer switched" <<<"$status" || { echo "$status"; exit 1; }
if grep -q "old-secret" "$work"/{a,b,c}.toml; then echo "a config still holds the old key"; exit 1; fi
# with a keyring, no config file holds a key at all
if secret-tool lookup service onecloud config "$work/a.toml" >/dev/null 2>&1; then
  if grep -q "secret" "$work"/{a,b,c}.toml; then echo "a key in a config file beside the keyring"; exit 1; fi
  say "keys in the keyring, none in config files"
  # the keyring loses c's key: c says how to get back, and --here-only does
  secret-tool clear service onecloud config "$work/c.toml"
  if out=$(oc c sync 2>&1); then echo "c synced without a key"; exit 1; fi
  grep -q -- "--here-only" <<<"$out" || { echo "no way back offered: $out"; exit 1; }
  ONECLOUD_SECRET_ACCESS_KEY=new-secret oc c bucket set-key --access-key-id new --here-only >/dev/null 2>&1
  oc c sync >/dev/null 2>&1
  say "c lost its keyring entry and got back"
fi
say "c caught up with --here-only; every computer switched"
