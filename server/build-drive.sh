#!/usr/bin/env bash
# Put Omacloud Drive's page where the Worker serves it from (public/drive):
# the page's files from ../web, and the reader built to WebAssembly from
# ../crates/omacloud-web. Needs Rust's wasm32-unknown-unknown target.
set -euo pipefail
cd "$(dirname "$0")"
export PATH="$HOME/.cargo/bin:$PATH"
(cd .. && cargo build --locked --release -q -p omacloud-web --target wasm32-unknown-unknown)
out=public/drive
rm -rf "${out:?}"
mkdir -p "$out"
cp ../web/index.html ../web/style.css ../web/*.js "$out/"
cp "../${CARGO_TARGET_DIR:-target}/wasm32-unknown-unknown/release/omacloud_web.wasm" "$out/"
