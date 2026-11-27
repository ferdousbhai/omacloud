#!/usr/bin/env bash
# The connector for agents (connector/) as `omacloud agents connect` uploads
# it: its modules, the storage and reader modules it shares with the browser
# page (web/), and the reader built to WebAssembly, side by side in one folder.
# Needs Rust's wasm32-unknown-unknown target.
#
#   scripts/build-connector.sh <folder>
set -euo pipefail
cd "$(dirname "$0")/.."
out=${1:?where to put the connector}
cargo build --locked --release -q -p omacloud-web --target wasm32-unknown-unknown
mkdir -p "$out"
rm -f "${out:?}"/*.js "${out:?}"/*.wasm
cp connector/*.js web/reader.js web/s3.js web/dropbox.js "$out/"
# side by side here, where connector/ names them in web/
sed -i 's|from "\.\./web/|from "./|' "$out"/*.js
cp "${CARGO_TARGET_DIR:-target}/wasm32-unknown-unknown/release/omacloud_web.wasm" "$out/"
