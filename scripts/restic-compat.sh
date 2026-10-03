#!/usr/bin/env bash
# Leaving without us works: two devices sync through omacloud, then upstream
# restic, knowing only the exported password, checks the repository and
# restores a folder identical to the devices'.
#
#   scripts/restic-compat.sh [path/to/omacloud]
#
# RESTIC may name another restic command, e.g. a docker wrapper.
set -euo pipefail

OMACLOUD=${1:-target/debug/omacloud}
RESTIC=${RESTIC:-restic}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
oc() { local dev=$1; shift; "$OMACLOUD" --config "$work/$dev.toml" "$@"; }

oc a init --folder "$work/A" --repo "$work/repo" --coordinator "$work/coord" --device a >"$work/init.txt" 2>/dev/null
password=$(oc a export 2>/dev/null | sed -n 's/^password: *//p')
recovery=$(sed -n 's/^recovery code: *//p' "$work/init.txt")
[ -n "$recovery" ] || { echo "no recovery code in init output"; exit 1; }
# b joins with the recovery code
OMACLOUD_RECOVERY_CODE=$recovery \
  oc b init --folder "$work/B" --repo "$work/repo" --coordinator "$work/coord" --device b >/dev/null 2>&1

mkdir -p "$work/A/docs/deep" "$work/B/src" "$work/A/empty/folder"
echo "from a" >"$work/A/docs/deep/a.txt"
head -c 3000000 /dev/urandom >"$work/A/blob.bin"
ln -s docs/deep "$work/A/link"
echo "from b" >"$work/B/src/b.rs"
chmod 755 "$work/B/src/b.rs"

oc a sync >/dev/null 2>&1
oc b sync >/dev/null 2>&1
oc a sync >/dev/null 2>&1
diff -r --no-dereference "$work/A" "$work/B"

restic_restore() { # repository password target
  RESTIC_PASSWORD=$2 $RESTIC -r "$1" --no-lock check --read-data
  RESTIC_PASSWORD=$2 $RESTIC -r "$1" --no-lock restore latest --target "$3"
  diff -r --no-dereference "$work/A" "$3"
  [ "$(stat -c %a "$3/src/b.rs")" = 755 ]
}
restic_restore "$work/repo" "$password" "$work/restored"

# after a key rotation: the export names the new repository and password,
# and the old repository is gone
oc a rotate >/dev/null 2>&1
echo "after rotation" >"$work/A/docs/rotated.txt"
oc a sync >/dev/null 2>&1
oc b sync >/dev/null 2>&1
diff -r --no-dereference "$work/A" "$work/B"
[ ! -e "$work/repo" ] || { echo "old repository still there"; exit 1; }
repo2=$(oc b export 2>/dev/null | sed -n 's/^repository: *//p')
password2=$(oc b export 2>/dev/null | sed -n 's/^password: *//p')
[ "$repo2" != "$work/repo" ] && [ "$password2" != "$password" ]
restic_restore "$repo2" "$password2" "$work/restored2"
echo "restic compatibility: ok"
