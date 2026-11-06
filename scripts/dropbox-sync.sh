#!/usr/bin/env bash
# An account on a real Dropbox: create-only writes, two computers, a race for
# the same position, many small files, a rotation, leaving with plain restic,
# and a key change that cuts off the old token.
#
#   scripts/dropbox-sync.sh [path/to/omacloud]
#
# Needs a desktop and someone at it: the browser asks to allow Omacloud twice.
# Everything stays in a new test folder of Apps/Omacloud, which goes to Dropbox's
# deleted files at the end, and both tokens are revoked. A run that fails keeps
# its token for the next, to spare allowing Omacloud again. Not in bin/check.
set -euo pipefail
trap 'echo "failed at line $LINENO: $BASH_COMMAND"' ERR

OMACLOUD=${1:-target/debug/omacloud}
OMACLOUD=$(realpath "$OMACLOUD")
work=$(mktemp -d)
root=/omacloud-test-$(date +%s)
saved=${XDG_STATE_HOME:-$HOME/.local/state}/omacloud-setup/dropbox-test-token
finished=

# the test's tokens are read from its configs, and never go on a command line
leave_nothing() {
  python3 - "$work" "$root" "$finished" <<'PY' || echo "cleaning up failed: delete $root in Apps/Omacloud, and disconnect the app if you like"
import json, sys, tomllib, urllib.parse, urllib.request
from pathlib import Path

work, root, finished = Path(sys.argv[1]), sys.argv[2], sys.argv[3]
def tokens():
    for name in ["a.toml", "a-before-key-change.toml"]:
        if (work / name).exists():
            yield tomllib.loads((work / name).read_text())["repo"]["options"]["refresh_token"]
def call(url, token, body):
    req = urllib.request.Request(url, data=body, headers={
        "authorization": f"Bearer {token}", "content-type": "application/json"})
    return urllib.request.urlopen(req, timeout=60).read()
deleted = False
for refresh in dict.fromkeys(tokens()):
    form = urllib.parse.urlencode({"grant_type": "refresh_token", "refresh_token": refresh,
                                   "client_id": "337rl43cn3fcoj8"}).encode()
    try:
        answer = urllib.request.urlopen("https://api.dropboxapi.com/oauth2/token", form, timeout=60).read()
    except urllib.error.HTTPError:
        continue  # revoked already
    access = json.loads(answer)["access_token"]
    if not deleted:
        call("https://api.dropboxapi.com/2/files/delete_v2", access, json.dumps({"path": root}).encode())
        deleted = True
    if finished:
        call("https://api.dropboxapi.com/2/auth/token/revoke", access, b"null")
print("cleaned up: test folder deleted" if deleted else "nothing to clean up")
PY
}
cleanup() {
  leave_nothing
  rm -rf "${work:?}"
}
trap cleanup EXIT

oc() { local dev=$1; shift; "$OMACLOUD" --config "$work/$dev.toml" --styled "$@"; }
on_tty() { SHELL=/bin/bash script -qefc "$(printf '%q ' "$@")" /dev/null </dev/null | tr -d '\r'; }
oc_tty() { local dev=$1; shift; on_tty "$OMACLOUD" --config "$work/$dev.toml" "$@"; }
join() {
  local fp
  fp=$(OMACLOUD_JOIN_CODE=$3 oc "$2" init --folder "$work/$4" --device "$2" 2>&1 | sed -n "s/^this device's fingerprint: //p")
  oc "$1" devices approve "$fp" >/dev/null
}
step() { echo "--- $(date +%T) $*"; }
# the link to allow Omacloud, as soon as it's printed; it holds no secret
show_links() {
  while kill -0 "$2" 2>/dev/null; do
    if grep -a -q -o 'https://www.dropbox.com[^[:space:]]*' "$1" 2>/dev/null; then
      grep -a -o 'https://www.dropbox.com[^[:space:]]*' "$1" | head -1 >&2
      return
    fi
    sleep 1
  done
}

step "allow Omacloud in the browser (first of two)"
# script writes its log as it goes, so the link shows if no browser opened
if [ -f "$saved" ]; then
  OMACLOUD_DROPBOX_REFRESH_TOKEN=$(cat "$saved")
  export OMACLOUD_DROPBOX_REFRESH_TOKEN
fi
SHELL=/bin/bash script -qefc "$(printf '%q ' "$OMACLOUD" --config "$work/a.toml" init --folder "$work/A" --provider dropbox --root "$root" --device a)" "$work/init.log" </dev/null >/dev/null &
initing=$!
show_links "$work/init.log" "$initing" &
wait "$initing" || { grep -a -i 'error\|hint\|Dropbox answered' "$work/init.log"; exit 1; }
[ "$(stat -c %a "$work/a.toml")" = 600 ] || { echo "config with the Dropbox token is readable by others"; exit 1; }
unset OMACLOUD_DROPBOX_REFRESH_TOKEN
mkdir -p "$(dirname "$saved")"
(umask 077; python3 -c 'import sys, tomllib; print(tomllib.load(open(sys.argv[1], "rb"))["repo"]["options"]["refresh_token"], end="")' "$work/a.toml" >"$saved")
oc a status >"$work/status.txt"
grep -q 'your Dropbox' "$work/status.txt" || { cut -c1-120 "$work/status.txt"; false; }
echo "an account on Dropbox, which refused a second create-only write: ok"

# a second computer, by join code
join a b "$(oc_tty a join-code | grep '^omacloud-join-')" B
mkdir -p "$work/A/docs"
echo "over dropbox" >"$work/A/docs/a.txt"
head -c 5000000 /dev/urandom >"$work/A/big.bin"
oc a sync >/dev/null
oc b sync >/dev/null
echo "back from b" >"$work/B/docs/b.txt"
rm "$work/B/big.bin"
oc b sync >/dev/null
oc a sync >/dev/null
diff -r --no-dereference "$work/A" "$work/B"
[ ! -e "$work/A/big.bin" ]
echo "two computers: ok"

# both at once: one takes the position, the other pulls and goes after it
for round in 1 2 3; do
  echo "a $round" >"$work/A/docs/race-a-$round"
  echo "b $round" >"$work/B/docs/race-b-$round"
  oc a sync >/dev/null &
  racer=$!
  oc b sync >/dev/null
  wait "$racer"
done
oc a sync >/dev/null
oc b sync >/dev/null
oc a sync >/dev/null
diff -r --no-dereference "$work/A" "$work/B"
[ "$(find "$work/A/docs" -name 'race-*' | wc -l)" = 6 ]
echo "racing for a position, three rounds: ok"

# many small files: what Dropbox limits
mkdir -p "$work/A/small"
for i in $(seq 300); do echo "file $i" >"$work/A/small/$i.txt"; done
start=$(date +%s)
oc a sync >/dev/null
pushed=$(date +%s)
oc b sync >/dev/null
pulled=$(date +%s)
diff -r --no-dereference "$work/A" "$work/B"
echo "300 small files: pushed in $((pushed - start))s, pulled in $((pulled - pushed))s: ok"

# rotation moves the repository to a new folder
oc a rotate --yes >/dev/null
echo "after rotation" >"$work/A/docs/rotated.txt"
oc a sync >/dev/null
oc b sync >/dev/null
diff -r --no-dereference "$work/A" "$work/B"
echo "rotation: ok"

# leaving: a copy on this computer, which plain restic reads
if command -v restic >/dev/null; then
  cmd=$(oc_tty a export --to "$work/copy" | sed -n '/^RESTIC_PASSWORD/,$p' | sed "s#<dir>#$work/restored#")
  bash -c "$cmd" >/dev/null
  [ "$(find "$work/restored" -name rotated.txt | wc -l)" = 1 ] || { echo "restic restored nothing"; exit 1; }
  echo "plain restic: ok"
else
  echo "plain restic: skipped, not installed"
fi

# a computer lost: removed, then a new token, then the old one is revoked once
# the others have switched. A copy of a config is all a thief would have
join a c "$(oc_tty a join-code | grep '^omacloud-join-')" C
oc a sync >/dev/null
oc b sync >/dev/null
oc c sync >/dev/null
cp "$work/b.toml" "$work/b-stolen.toml"
cp "$work/a.toml" "$work/a-before-key-change.toml"
fpb=$(oc b device-key | sed -n 's/^fingerprint //p')
oc a devices revoke "$fpb" --rotate --yes >/dev/null
step "allow Omacloud in the browser (second of two)"
oc a bucket set-key >/dev/null 2>"$work/key.log" &
changing=$!
show_links "$work/key.log" "$changing" &
wait "$changing"
oc c sync >/dev/null
rm -f "$saved"
oc a bucket status | grep -q 'the old Dropbox token is revoked' || { oc a bucket status; echo "the old token wasn't revoked"; exit 1; }
says=$("$OMACLOUD" --config "$work/b-stolen.toml" sync --json || true)
echo "$says" | grep -q '"key_refused"' || { echo "$says" | cut -c1-300; echo "the old token still works"; exit 1; }
echo "after the key change" >"$work/A/docs/rekeyed.txt"
oc a sync >/dev/null
oc c sync >/dev/null
diff -r --no-dereference "$work/A" "$work/C"
echo "lost computer: ok"

finished=1
echo "dropbox sync: ok"
