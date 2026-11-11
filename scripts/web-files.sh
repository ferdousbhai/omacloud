#!/usr/bin/env bash
# The web page (web/) on a throwaway account: builds the WebAssembly reader,
# starts scripts/fake-s3.py with a CORS rule for the page, syncs a few files
# into it, and serves the page. Prints what to enter; Ctrl-C ends it.
#
#   scripts/web-files.sh [path/to/omacloud]
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"
OMACLOUD=$(realpath "${1:-target/debug/omacloud}")
port=${WEB_PORT:-8097}
work=$(mktemp -d)
pids=()
cleanup() {
  for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
  rm -rf "${work:?}"
}
trap cleanup EXIT

cargo build --locked --release -q -p omacloud-web --target wasm32-unknown-unknown
mkdir -p "$work/site" "$work/A/Notes" "$work/A/Photos"
cp web/*.html web/*.css web/*.js "$work/site/"
cp "${CARGO_TARGET_DIR:-target}/wasm32-unknown-unknown/release/omacloud_web.wasm" "$work/site/"

echo key-one >"$work/keys"
FAKE_S3_CORS=http://127.0.0.1:$port python3 scripts/fake-s3.py "$work/port" "$work/keys" files &
pids+=($!)
for _ in $(seq 50); do
  [ -s "$work/port" ] && break
  sleep 0.1
done
endpoint=http://127.0.0.1:$(cat "$work/port")

echo "hello from the browser" >"$work/A/Notes/hello.txt"
head -c 3000000 /dev/urandom >"$work/A/Photos/big.bin"
on_tty() { SHELL=/bin/bash script -qefc "$(printf '%q ' "$@")" /dev/null </dev/null | tr -d '\r'; }
export OMACLOUD_SECRET_ACCESS_KEY=secret-one
on_tty "$OMACLOUD" --config "$work/a.toml" init --folder "$work/A" --provider s3 --endpoint "$endpoint" \
  --region us-east-1 --bucket files --access-key-id key-one --device a >/dev/null
"$OMACLOUD" --config "$work/a.toml" --styled sync >/dev/null 2>&1
password=$(on_tty "$OMACLOUD" --config "$work/a.toml" export | sed -n "s/.*RESTIC_PASSWORD='\([^']*\)'.*/\1/p")

(cd "$work/site" && python3 -m http.server "$port" --bind 127.0.0.1 >/dev/null 2>&1) &
pids+=($!)
cat <<END
Open http://127.0.0.1:$port and enter:
  Endpoint             $endpoint
  Bucket               files
  Region               us-east-1
  Access key ID        key-one
  Secret access key    secret-one
  Repository password  $password
(a throwaway account; Ctrl-C ends it)
END
wait
