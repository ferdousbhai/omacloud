#!/usr/bin/env bash
# Deploy the omacloud.computer Worker with cf: type check, then deploy.
set -euo pipefail
cd "$(dirname "$0")"
npx cf workers types >/dev/null
npx tsc --noEmit
npx cf deploy
