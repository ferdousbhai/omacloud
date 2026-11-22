#!/usr/bin/env bash
set -Eeuo pipefail

# The app's scripted QA (omui's feature `qa`) on dev/fake-omacloud's made-up account. omui's own dev/qa.sh, from the
# revision Cargo.lock pins, runs it on a broadway display of its own with a private HOME and XDG directories and no
# session bus, so no window opens anywhere and nothing of the desktop it is run from is read. The CLI and the other
# programs the app runs are stand-ins: no real account or storage is reached. The protocol is in omui's README.md.
#
#   dev/qa.sh <app built with --features qa> <script.jsonl> [directory for the shots]
#
# A script's `# state: new|gone|error|account` line is what the made-up `omacloud status` answers as the app starts
# (default: account). The file chooser and the language are omui's QA commands (`picker`, `language`); a `language`
# command's `catalog` is a path from this crate, as in omui's own scripts. Shots are written relative to the directory
# given (default: a scratch one, removed after), which also gets calls.log, every program the app ran, in order.
(( $# == 2 || $# == 3 )) || { echo "usage: $0 <app> <script.jsonl> [directory]" >&2; exit 2; }
app=$(realpath -- "$1")
script=$(realpath -- "$2")
here=$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)

omui=$(PATH="$HOME/.cargo/bin:$PATH" cargo metadata --locked --offline --format-version 1 \
  --manifest-path "$here/../Cargo.toml" | jq -r '.packages[] | select(.name == "omui") | .manifest_path')
omui_qa=$(dirname -- "$omui")/dev/qa.sh
[[ -x $omui_qa ]] || { echo "qa: no dev/qa.sh beside $omui" >&2; exit 1; }

root=$(mktemp -d /tmp/omacloud-qa.XXXXXX)
trap 'rm -rf -- "${root:?}"' EXIT
out=${3:-$root/shots}
mkdir -p "$root/fake" "$out"
out=$(realpath -- "$out")

state=$(sed -n 's/^# state: *//p' "$script" | head -n 1)
printf '%s\n' "${state:-account}" > "$root/fake/state"
: > "$root/fake/calls.log"
"$here/stand-ins" "$root/bin" "$root/fake/calls.log"

# The app runs in the directory of the shots, so a catalog's path from the crate is made whole first.
crate=$(dirname -- "$here")
sed -E "s|(\"catalog\": *\")([^/\"][^\"]*)\"|\1$crate/\2\"|g" "$script" > "$root/script.jsonl"

status=0
(
  cd "$out"
  # The config named does not exist: the made-up CLI never reads it, and the real one would find no account.
  PATH=$root/bin:/usr/bin OMACLOUD_BIN=$here/fake-omacloud OMACLOUD_CONFIG=$root/config.toml \
    OMACLOUD_FAKE_STATE=$root/fake "$omui_qa" "$app" "$root/script.jsonl"
) || status=$?
cp -- "$root/fake/calls.log" "$out/calls.log"
(( status == 0 )) || { echo "qa: programs the app ran:" >&2; sed 's/^/  /' "$root/fake/calls.log" >&2; }
exit "$status"
