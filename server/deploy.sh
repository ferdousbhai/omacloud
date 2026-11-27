#!/usr/bin/env bash
# Deploy the omacloud.computer Worker with cf: build the browser page,
# type check, then deploy.
set -euo pipefail
cd "$(dirname "$0")"
./build-files.sh
npx cf workers types >/dev/null
npx tsc --noEmit
npx cf deploy
