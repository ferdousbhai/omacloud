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

mkdir -p "$work/A/Work/Reports"
echo "hello from the browser" >"$work/A/Notes/hello.txt"
printf '# Plan\n\n- one\n- two\n' >"$work/A/Work/plan.md"
printf 'quarter,sales\nQ1,10\nQ2,14\n' >"$work/A/Work/Reports/sales.csv"
head -c 3000000 /dev/urandom >"$work/A/Work/archive.bin"
# a few pictures, drawn here: plain colour gradients
python3 - "$work/A/Photos" <<'PY'
import struct, sys, zlib
def png(path, w, h, colour):
    rows = b"".join(b"\0" + b"".join(bytes(colour(x / w, y / h)) for x in range(w)) for y in range(h))
    chunk = lambda kind, data: struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
    head = struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)
    open(path, "wb").write(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", head) + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b""))
png(f"{sys.argv[1]}/sunrise.png", 320, 200, lambda x, y: (255, int(120 + 100 * y), int(60 + 120 * x)))
png(f"{sys.argv[1]}/sea.png", 320, 200, lambda x, y: (int(20 + 40 * x), int(110 + 90 * y), int(170 + 70 * (1 - y))))
png(f"{sys.argv[1]}/forest.png", 200, 320, lambda x, y: (int(30 + 60 * x), int(90 + 120 * (1 - y)), int(50 + 40 * y)))
PY
on_tty() { SHELL=/bin/bash script -qefc "$(printf '%q ' "$@")" /dev/null </dev/null | tr -d '\r'; }
export OMACLOUD_SECRET_ACCESS_KEY=secret-one
on_tty "$OMACLOUD" --config "$work/a.toml" init --folder "$work/A" --provider s3 --endpoint "$endpoint" \
  --region us-east-1 --bucket files --access-key-id key-one --device a >/dev/null
"$OMACLOUD" --config "$work/a.toml" --styled sync >/dev/null 2>&1
# something deleted since, for Recently Deleted
rm "$work/A/Work/Reports/sales.csv" "$work/A/Photos/forest.png"
"$OMACLOUD" --config "$work/a.toml" --styled sync >/dev/null 2>&1
printed=$(on_tty "$OMACLOUD" --config "$work/a.toml" drive-code | grep '^omacloud-drive-')

# a computer of the account, running: what the page uploads, it takes in
"$OMACLOUD" --config "$work/a.toml" watch --poll 2 >"$work/watch.log" 2>&1 &
pids+=($!)
(cd "$work/site" && python3 -m http.server "$port" --bind 127.0.0.1 >/dev/null 2>&1) &
pids+=($!)
cat <<END
Open http://127.0.0.1:$port and paste this, what \`omacloud drive-code\` prints:

$printed

Or follow a drive link, what \`omacloud drive-link\` shows (good for five minutes):

END
# the link's command waits for Enter, so it keeps a terminal of its own going
OMACLOUD_DRIVE_URL=http://127.0.0.1:$port/ SHELL=/bin/bash script -qefc \
  "$(printf '%q ' "$OMACLOUD" --config "$work/a.toml" drive-link)" /dev/null < <(sleep 290) | stdbuf -oL tr -d '\r' | grep --line-buffered '#link=' &
pids+=($!)
echo
echo "(a throwaway account; Ctrl-C ends it)"
wait
