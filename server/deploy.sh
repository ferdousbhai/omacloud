#!/usr/bin/env bash
# Deploy the Omacloud storage Worker with cf.
#
#   server/deploy.sh --first     the first deploy: asks for the bucket's key and
#                                the Google OAuth client, and makes the master key
#   server/deploy.sh             the code and settings; secrets stay as they are
#   server/deploy.sh --secrets   also asks for the bucket's key and the Google
#                                OAuth client again
#
# With --first or --secrets, `--key-id ID` gives the bucket key's access
# key id (not a secret) and `--google client_secret_....json` (the file
# Google offers when the OAuth client is made) the Google client, instead
# of asking for them.
#
# Secrets are typed at a prompt that doesn't show them and reach cf through a
# pipe, never a file or a command line. The master key, which every account's
# key derives from, is made by --first only, and never replaced: a new one
# would void every account's key.
set -euo pipefail
cd "$(dirname "$0")"

secrets=()
ask() { # NAME prompt
  local value
  read -rsp "$2 (not shown): " value </dev/tty
  echo >/dev/tty
  [ -n "$value" ] || { echo "nothing entered" >&2; exit 1; }
  secrets+=("$1" "$value")
}
usage() { echo "usage: server/deploy.sh [--first | --secrets] [--key-id ID] [--google client_secret.json]" >&2; exit 1; }
mode= key_id= google=
while [ $# -gt 0 ]; do
  case $1 in
    --first | --secrets) mode=$1 ;;
    --key-id) key_id=${2:?$(usage)}; shift ;;
    --google) google=${2:?$(usage)}; shift ;;
    *) usage ;;
  esac
  shift
done
# one field of Google's client file, never printed
google_field() { python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["web"][sys.argv[2]])' "$google" "$1"; }
case $mode in
  --first)
    if npx cf workers secrets list --worker omacloud 2>/dev/null | grep -q '"MASTER_KEY"'; then
      echo "omacloud already has a master key: deploy without --first" >&2
      exit 1
    fi
    secrets+=(MASTER_KEY "$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')") ;;&
  --first | --secrets)
    if [ -n "$key_id" ]; then
      secrets+=(UPSTREAM_KEY_ID "$key_id")
    else
      ask UPSTREAM_KEY_ID "Hetzner Object Storage access key"
    fi
    ask UPSTREAM_SECRET "Hetzner Object Storage secret key"
    if [ -n "$google" ]; then
      secrets+=(GOOGLE_CLIENT_ID "$(google_field client_id)" GOOGLE_SECRET "$(google_field client_secret)")
    else
      ask GOOGLE_CLIENT_ID "Google OAuth client ID"
      ask GOOGLE_SECRET "Google OAuth client secret"
    fi ;;
  *) [ -z "$google$key_id" ] || { echo "--key-id and --google go with --first or --secrets" >&2; exit 1; } ;;
esac

npx tsc --noEmit
if [ ${#secrets[@]} -eq 0 ]; then
  npx cf deploy
else
  # the secrets as JSON, through a pipe
  npx cf deploy --secrets-file <(python3 -c '
import json, sys
a = sys.stdin.read().split("\0")[:-1]
print(json.dumps(dict(zip(a[::2], a[1::2]))))' < <(printf '%s\0' "${secrets[@]}"))
fi
