#!/usr/bin/env bash
# Deploy the omacloud.computer Worker with cf: type check, apply new D1
# migrations (the code expects them), then deploy.
#
#   server/deploy.sh           the code and settings; the secret stays as it is
#   server/deploy.sh --first   also makes the credit key (CREDIT_KEY), the RSA
#                              key that signs storage tokens. Only once: a new
#                              one voids every token out there.
#
# The key is made by node and reaches cf through a pipe, never a file or a
# command line.
set -euo pipefail
cd "$(dirname "$0")"
first=
case $# in
  0) ;;
  1) [ "$1" = --first ] || { echo "usage: server/deploy.sh [--first]" >&2; exit 1; }; first=1 ;;
  *) echo "usage: server/deploy.sh [--first]" >&2; exit 1 ;;
esac
# a listing that fails must stop --first, or a new key voids every token
if [ -n "$first" ]; then
  secrets=$(npx cf workers secrets list --worker omacloud) || exit 1
  if grep -q CREDIT_KEY <<<"$secrets"; then
    echo "omacloud already has a credit key: deploy without --first" >&2
    exit 1
  fi
fi

npx cf workers types >/dev/null
npx tsc --noEmit
# the schema first: the new code expects it, and the old code ignores what's added
npx cf d1 migrations apply "$(sed -n 's/.*id: "\([0-9a-f-]\{36\}\)".*/\1/p' cloudflare.config.ts)"
if [ -z "$first" ]; then
  npx cf deploy
else
  npx cf deploy --secrets-file <(node -e '
const { generateKeyPairSync } = require("node:crypto");
const { privateKey } = generateKeyPairSync("rsa", { modulusLength: 2048 });
process.stdout.write(JSON.stringify({ CREDIT_KEY: JSON.stringify(privateKey.export({ format: "jwk" })) }));')
fi
