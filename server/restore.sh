#!/usr/bin/env bash
# Put an account of Omacloud storage back as it was at a time in the last 14
# days, from the trash (src/trash.ts), with cf signed in.
#
#   server/restore.sh <account id or email> <time>   ask for it: the scheduled
#                                                    job does it, a batch every
#                                                    15 minutes
#   server/restore.sh status                         the latest restores (one
#                                                    failing 5 runs in a row
#                                                    is given up on, and the
#                                                    admin emailed)
#
# The time is ISO-8601 with its zone (2026-10-07T09:30:00Z, or +02:00) or
# unix seconds. Everything deleted or overwritten since then is put back as
# it was; what's been made since stays. The account's trash isn't purged
# until it's done. If whoever did the damage may still hold a key, close the
# account first (disabled = 1, see README.md) and open it again after.
#
# Arguments are checked here and reach D1 as bound parameters, never as SQL.
# OMACLOUD_CF runs another cf (a test's stand-in).
set -euo pipefail
cd "$(dirname "$0")"

cf() { ${OMACLOUD_CF:-npx cf} "$@"; }
db=$(sed -n 's/^[[:space:]]*id: "\([0-9a-f-]\{36\}\)",$/\1/p' cloudflare.config.ts)
[ -n "$db" ] || { echo "no D1 database id in cloudflare.config.ts" >&2; exit 1; }
usage() { echo "usage: server/restore.sh <account id or email> <time> | status" >&2; exit 1; }
[ $# -gt 0 ] || usage

if [ "$1" = status ]; then
  [ $# -eq 1 ] || usage
  exec ${OMACLOUD_CF:-npx cf} d1 query "$db" --sql "SELECT r.id, r.account, a.email,
    datetime(r.at, 'unixepoch') AS back_to, datetime(r.requested, 'unixepoch') AS asked,
    r.restored, coalesce(datetime(r.done, 'unixepoch'), 'not yet') AS done,
    CASE WHEN r.failed IS NOT NULL THEN 'failed ' || datetime(r.failed, 'unixepoch')
      WHEN r.attempts > 0 THEN r.attempts || ' runs failed' ELSE '' END AS trouble
    FROM restores r LEFT JOIN accounts a ON a.id = r.account ORDER BY r.id DESC LIMIT 20"
fi
[ $# -eq 2 ] || usage

who=$(tr '[:upper:]' '[:lower:]' <<<"$1")
local_re="^[a-z0-9.!#\$%&'*+/=?^_\`{|}~-]{1,64}@[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?(\.[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?)+\$"
if ! [[ $who =~ ^u[0-9a-f]{16}$ ]] && { [ "${#who}" -gt 254 ] || ! [[ $who =~ $local_re ]]; }; then
  echo "not an account id or an email address: $1" >&2
  exit 1
fi

# seconds since 1970, from either form; within the trash's 14 days, and past
at=$(python3 -c '
import sys, time, datetime
t = sys.argv[1]
if t.isdigit() and len(t) <= 11:
    s = int(t)
else:
    try:
        d = datetime.datetime.fromisoformat(t.replace("Z", "+00:00"))
    except ValueError:
        sys.exit("not a time: " + t + " (ISO-8601 with its zone, or unix seconds)")
    if d.tzinfo is None:
        sys.exit("give the time its zone: " + t + "Z, say, for UTC")
    s = int(d.timestamp())
now = int(time.time())
if s > now:
    sys.exit("that time hasn'"'"'t come yet")
if s < now - 14 * 86400 + 3600:
    sys.exit("the trash keeps 14 days: pick a time since " +
             datetime.datetime.fromtimestamp(now - 14 * 86400 + 3600, datetime.UTC).strftime("%Y-%m-%dT%H:%M:%SZ"))
print(s, datetime.datetime.fromtimestamp(s, datetime.UTC).strftime("%Y-%m-%dT%H:%M:%SZ"))' "$2")
read -r at when <<<"$at"

# one account, by id or (any case of) email; none or several: the insert
# fails on its NOT NULL
batch=$(python3 -c '
import json, sys
print(json.dumps([{"sql": "INSERT INTO restores (account, at, requested) VALUES ("
    "(SELECT CASE WHEN count(*) = 1 THEN max(id) END FROM accounts WHERE id = ?1 OR lower(email) = ?1), ?2, unixepoch())",
    "params": [sys.argv[1], int(sys.argv[2])]}]))' "$who" "$at")
cf d1 query "$db" --batch "$batch" >/dev/null ||
  { echo "no restore asked for: $1 is no single account" >&2; exit 1; }
echo "restoring $1 to $when: the scheduled job starts within 15 minutes"
echo "(server/restore.sh status says when it's done)"
