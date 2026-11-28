#!/usr/bin/env bash
# Set one of the Worker's secrets (README.md lists them) without it passing
# through a command line or the shell's history: asked for with no echo, or
# read from stdin when that isn't a terminal.
#
#   server/set-secret.sh STRIPE_SECRET_KEY
set -euo pipefail
cd "$(dirname "$0")"
name=${1:?which secret}
if [ -t 0 ]; then
  read -rsp "$name: " value
  echo
else
  value=$(cat)
fi
[ -n "$value" ] || { echo "nothing given" >&2; exit 1; }
body=$(mktemp)
trap 'rm -f "$body"' EXIT
NAME=$name VALUE=$value python3 -c 'import json, os, sys
json.dump({"name": os.environ["NAME"], "type": "secret_text", "text": os.environ["VALUE"]}, open(sys.argv[1], "w"))' "$body"
npx cf workers secrets update "$name" --worker omacloud --body "@$body" >/dev/null
echo "$name is set"
