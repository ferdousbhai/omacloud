#!/usr/bin/env bash
# The sync engine against the real coordinator service: runs service/
# under `wrangler dev` (local workerd, no Cloudflare account needed) with a
# SeaweedFS container as the managed tier's bucket, and the Rust end to end
# tests against it.
#
#   scripts/coordinator-e2e.sh [path/to/service]
set -euo pipefail

WEB=${1:-service}
PORT=${PORT:-8799}
S3_PORT=${S3_PORT:-8334}
work=$(mktemp -d)
cleanup() {
  kill "$server" 2>/dev/null || true
  docker stop onecloud-e2e-s3 >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

# the bucket
echo '{"identities":[{"name":"e2e","credentials":[{"accessKey":"e2e","secretKey":"e2e-secret"}],"actions":["Admin","Read","Write","List","Tagging"]}]}' >"$work/s3.json"
docker run -d --rm --name onecloud-e2e-s3 -p "$S3_PORT:8333" -v "$work/s3.json:/etc/s3.json:ro" \
  chrislusf/seaweedfs server -s3 -s3.config=/etc/s3.json -dir=/data >/dev/null
for _ in $(seq 1 60); do curl -s -o /dev/null "http://127.0.0.1:$S3_PORT" && break; sleep 1; done
curl -sf -X PUT --aws-sigv4 "aws:amz:us-east-1:s3" --user e2e:e2e-secret "http://127.0.0.1:$S3_PORT/onecloud"

# the service
(cd "$WEB" && exec npx wrangler dev --port "$PORT" --ip 127.0.0.1 \
  --var "STORAGE_ENDPOINT:http://127.0.0.1:$S3_PORT" --var STORAGE_REGION:us-east-1 \
  --var STORAGE_BUCKET:onecloud --var STORAGE_KEY_ID:e2e --var STORAGE_SECRET:e2e-secret >"$work/wrangler.log" 2>&1) &
server=$!
for _ in $(seq 1 60); do curl -sf "http://127.0.0.1:$PORT/" >/dev/null && break; sleep 1; done
curl -sf "http://127.0.0.1:$PORT/" >/dev/null || { cat "$work/wrangler.log"; exit 1; }

ONECLOUD_COORDINATOR_URL="http://127.0.0.1:$PORT" ONECLOUD_SERVICE_STORAGE=1 ONECLOUD_BENCH=${ONECLOUD_BENCH:-} \
  cargo test -p onecloud-core --test http -- --nocapture --test-threads 1

# after rotation, only the new epoch's objects remain in the bucket
keys=$(curl -s --aws-sigv4 "aws:amz:us-east-1:s3" --user e2e:e2e-secret \
  "http://127.0.0.1:$S3_PORT/onecloud?list-type=2&max-keys=10000" | grep -o '<Key>[^<]*</Key>' |
  sed 's/<\/*Key>//g' | grep -v '/$' || true)
# (other tests' accounts never rotated; look at the ones that did)
rotated=$(grep -oE '^accounts/[0-9a-f]{64}/e1/' <<<"$keys" | sort -u | cut -d/ -f2 || true)
[ -n "$rotated" ] || { echo "no rotated account in the bucket"; exit 1; }
for acc in $rotated; do
  epoch1=$(grep -c "^accounts/$acc/e1/" <<<"$keys" || true)
  left=$(grep "^accounts/$acc/" <<<"$keys" | grep -v "^accounts/$acc/e1/" || true)
  [ -z "$left" ] || { echo "objects outside epoch 1 remain:"; echo "$left"; exit 1; }
  echo "bucket: account ${acc:0:8} has $epoch1 objects, all in epoch 1"
done
# the CLI on managed storage, and leaving with the data: `export --to`
# copies the repository out, and plain restic restores the copy
cargo build -q -p onecloud
oc() { target/debug/onecloud --config "$work/cli.toml" "$@"; }
mkdir -p "$work/folder/docs"
echo "managed, then exported" >"$work/folder/docs/leave.txt"
oc init --folder "$work/folder" --repo onecloud --coordinator "http://127.0.0.1:$PORT" --device cli >/dev/null 2>&1
oc sync >/dev/null 2>&1
oc export --to "$work/exported" >"$work/export.txt" 2>/dev/null
password=$(sed -n 's/^password: *//p' "$work/export.txt")
RESTIC=${RESTIC:-restic}
RESTIC_PASSWORD=$password $RESTIC -r "$work/exported" --no-lock restore latest --target "$work/restored" >/dev/null
diff -r "$work/folder" "$work/restored"
echo "exported and restored with plain restic"
oc status 2>/dev/null | grep -E '^storage +[0-9.]+ [KMG]?B of ([0-9.]+ [KMGT]?B|no limit)$' >/dev/null || { oc status; exit 1; }
# deleting the account takes its objects out of the bucket
acc=$(oc status 2>/dev/null | sed -n 's/^account *//p')
oc delete-account --confirm "$acc" >/dev/null 2>&1
[ ! -e "$work/cli.toml" ] || { echo "config left after delete-account"; exit 1; }
leftover=$(curl -s --aws-sigv4 "aws:amz:us-east-1:s3" --user e2e:e2e-secret \
  "http://127.0.0.1:$S3_PORT/onecloud?list-type=2&prefix=accounts/$acc/" | grep -c '<Key>[^<]*[^/]</Key>' || true)
[ "$leftover" = 0 ] || { echo "$leftover objects left after delete-account"; exit 1; }
echo "account deleted, bucket clean"

# sharing between two people: alice shares a folder, bob asks to join and
# alice approves after the fingerprint check, both edit, then alice removes
# bob and changes the folder's key
U="http://127.0.0.1:$PORT"
alice() { target/debug/onecloud --config "$work/alice.toml" "$@"; }
bob() { target/debug/onecloud --config "$work/bob.toml" "$@"; }
alice init --folder "$work/alice" --coordinator "$U" --device alice-laptop >/dev/null 2>&1
bob init --folder "$work/bob" --coordinator "$U" --device bob-laptop >/dev/null 2>&1
mkdir -p "$work/alice-photos"
echo "from alice" >"$work/alice-photos/a.txt"
id=$(alice share create photos --folder "$work/alice-photos" 2>/dev/null |
  sed -n 's/.*onecloud share join \([0-9a-f]\{64\}\) .*/\1/p')
[ -n "$id" ] || { echo "no share id"; exit 1; }
fp=$(bob share join "$id" --name photos --folder "$work/bob-photos" 2>/dev/null |
  sed -n "s/^this device's fingerprint: //p")
alice share list 2>/dev/null | grep -q "asking to join: $fp" || { echo "request not visible"; exit 1; }
alice share approve photos "$fp" >/dev/null 2>&1
alice sync >/dev/null 2>&1
bob sync >/dev/null 2>&1
diff -r "$work/alice-photos" "$work/bob-photos"
echo "from bob" >"$work/bob-photos/b.txt"
bob sync >/dev/null 2>&1
alice sync >/dev/null 2>&1
diff -r "$work/alice-photos" "$work/bob-photos"
# each person's own folder stays theirs
[ ! -e "$work/bob/a.txt" ] && [ ! -e "$work/alice/b.txt" ]
alice share remove photos "$fp" --rotate >/dev/null 2>&1
echo "after bob" >"$work/alice-photos/c.txt"
alice sync >/dev/null 2>&1
if bob sync >"$work/bob-sync.txt" 2>&1; then echo "bob still syncs"; exit 1; fi
grep -q "photos: .*removed" "$work/bob-sync.txt" || { cat "$work/bob-sync.txt"; exit 1; }
[ ! -e "$work/bob-photos/c.txt" ]
echo "shared a folder, synced both ways, removed a member with a new key"
echo "coordinator e2e: ok"
