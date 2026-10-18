#!/usr/bin/env bash
# Deploy the omacloud.computer Worker with cf: type check, apply new D1
# migrations (the code expects them), then deploy. It has no secrets.
set -euo pipefail
cd "$(dirname "$0")"
[ $# -eq 0 ] || { echo "usage: server/deploy.sh" >&2; exit 1; }

npx tsc --noEmit
# the schema first: the new code expects it, and the old code ignores what's added
npx cf d1 migrations apply "$(sed -n 's/.*id: "\([0-9a-f-]\{36\}\)".*/\1/p' cloudflare.config.ts)"
npx cf deploy
