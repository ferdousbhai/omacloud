#!/usr/bin/env bash
# Copy the Worker's D1 database off Cloudflare: every table, as JSON, into
# <dir>/<date>/ (default ~/omacloud-backups), private to you.
#
#   server/backup.sh [dir]
#
# Cloudflare also keeps the database restorable to any minute of the last
# 30 days (7 on the free plan): `cf d1 time-travel restore`. This copy is
# for losing the Cloudflare account itself. Accounts' records and index
# need neither: their computers keep them and put them back
# (`omacloud republish`); what only lives here is the node directory, which
# heartbeats rebuild, and the set of spent credits.
set -euo pipefail
cd "$(dirname "$0")"
out=${1:-$HOME/omacloud-backups}/$(date -u +%Y-%m-%dT%H%MZ)
db=$(sed -n 's/.*id: "\([0-9a-f-]\{36\}\)".*/\1/p' cloudflare.config.ts)
umask 077
mkdir -p "$out"
query() { npx cf d1 query "$db" --sql "$1"; }
tables=$(query "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE '\_cf\_%' ESCAPE '\' AND name <> 'sqlite_sequence'" \
  | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>console.log(JSON.parse(s)[0].results.map(r=>r.name).join("\n")))')
rows() { node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{for(const x of JSON.parse(s)[0].results)console.log(JSON.stringify(x))})'; }
for t in $tables; do
  # in pages, ordered by the primary key, as one answer is limited in size
  key=$(query "SELECT group_concat(name, ',') AS k FROM (SELECT name FROM pragma_table_info('$t') WHERE pk > 0 ORDER BY pk)" \
    | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>console.log(JSON.parse(s)[0].results[0].k||"rowid"))')
  : >"$out/$t.jsonl"
  for ((offset = 0; ; offset += 2000)); do
    page=$(query "SELECT * FROM \"$t\" ORDER BY $key LIMIT 2000 OFFSET $offset" | rows)
    [ -z "$page" ] && break
    printf '%s\n' "$page" >>"$out/$t.jsonl"
  done
  echo "$t: $(wc -l <"$out/$t.jsonl") rows"
done
echo "copied to $out"
