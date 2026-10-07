#!/usr/bin/env bash
# Omacloud storage's waitlist and invitations, in its D1 database (with cf
# signed in).
#
#   server/invite.sh list          who's waiting (confirmed only), oldest first
#   server/invite.sh <email>...    invite them: the scheduled job emails each
#                                  within 15 minutes, and they leave the waitlist
#
# Inviting an address again emails it again. Addresses must look like email
# addresses (lower-cased here) and reach D1 as bound parameters, never as SQL.
# OMACLOUD_CF runs another cf (a test's stand-in).
set -euo pipefail
cd "$(dirname "$0")"

cf() { ${OMACLOUD_CF:-npx cf} "$@"; }
db=$(sed -n 's/^[[:space:]]*id: "\([0-9a-f-]\{36\}\)",$/\1/p' cloudflare.config.ts)
[ -n "$db" ] || { echo "no D1 database id in cloudflare.config.ts" >&2; exit 1; }
usage() { echo "usage: server/invite.sh list | <email>..." >&2; exit 1; }
[ $# -gt 0 ] || usage

if [ "$1" = list ]; then
  [ $# -eq 1 ] || usage
  exec ${OMACLOUD_CF:-npx cf} d1 query "$db" --sql "SELECT email, datetime(confirmed, 'unixepoch') AS waiting_since,
    CASE WHEN google_sub IS NULL THEN 'website' ELSE 'sign-in' END AS how
    FROM waitlist WHERE confirmed IS NOT NULL ORDER BY confirmed, email"
fi

# as the Worker checks them (src/waitlist.ts normalEmail)
local_re="^[a-z0-9.!#\$%&'*+/=?^_\`{|}~-]{1,64}\$"
domain_re='^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?(\.[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?)+$'
emails=()
for arg in "$@"; do
  e=$(tr '[:upper:]' '[:lower:]' <<<"$arg")
  local_part=${e%@*} domain=${e##*@}
  if [ "${#e}" -gt 254 ] || [ "$local_part@$domain" != "$e" ] || [[ $local_part == *@* ]] ||
    ! [[ $local_part =~ $local_re ]] || ! [[ $domain =~ $domain_re ]] ||
    [[ $local_part == .* || $local_part == *. || $local_part == *..* ]]; then
    echo "not an email address: $arg" >&2
    exit 1
  fi
  emails+=("$e")
done

# each address: invited (again), and off the waitlist
batch=$(python3 -c '
import json, sys
print(json.dumps([s for e in sys.argv[1:] for s in (
    {"sql": "INSERT INTO invites (email, created) VALUES (?, unixepoch()) ON CONFLICT (email) DO UPDATE SET notified = NULL, notify_tries = 0, notify_after = 0", "params": [e]},
    {"sql": "DELETE FROM waitlist WHERE email = ?", "params": [e]},
)]))' "${emails[@]}")
cf d1 query "$db" --batch "$batch" >/dev/null
printf 'invited %s: the email goes within 15 minutes\n' "${emails[@]}"
