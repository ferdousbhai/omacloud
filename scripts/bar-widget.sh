#!/usr/bin/env bash
# The bar widget (contrib/omarchy-plugin) follows the line the daemon writes by the file's changes, with no timer
# that polls: it is linted, then run in a Quickshell of its own, with no display, on stand-ins for the two types it
# takes from the Omarchy shell (scripts/bar-widget/qs/Ui) and the one it reads its words through
# (scripts/bar-widget/qs/Commons), and handed lines as `omacloud watch` writes them
# (crates/omacloud/src/bar.rs). Each has to show within seconds, the first one too, written to a folder that was
# not there as the widget started; and a line left to go old has to read as a daemon gone, with nothing written.
#
#   scripts/bar-widget.sh
#
# Needs quickshell and qmllint (Arch: quickshell, qt6-declarative). The runtime directory is the test's own: the
# running shell and the real daemon's line are never read.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
plugin=$here/../contrib/omarchy-plugin
# Qt 6's, which Arch keeps out of PATH; a plain `qmllint` may be an older Qt's, with other options.
qmllint=/usr/lib/qt6/bin/qmllint
[ -x "$qmllint" ] || qmllint=qmllint6
for tool in qs "$qmllint" jq; do
  command -v "$tool" >/dev/null || { echo "bar-widget: $tool is required" >&2; exit 1; }
done

# Every warning is an error here: QML reports a property that does not exist only as the file loads.
"$qmllint" -W 0 -I "$here/bar-widget" "$plugin/Widget.qml"
jq -e '.entryPoints.barWidget == "Widget.qml"' "$plugin/manifest.json" >/dev/null
# Every catalog is a flat object of strings, and English has the tooltip the widget asks for.
for catalog in "$plugin"/i18n/*.json; do
  jq -e 'type == "object" and all(.[]; type == "string")' "$catalog" >/dev/null \
    || { echo "bar-widget: $catalog is not a flat catalog" >&2; exit 1; }
done
jq -e '.tooltip | test("%1")' "$plugin/i18n/en.json" >/dev/null
# Omarchy's own check of the manifest, where there is an Omarchy.
if command -v omarchy-plugin-validate >/dev/null; then
  omarchy-plugin-validate "$plugin" >/dev/null
fi
# A timer that repeats is a poll.
if grep -n 'repeat: *true' "$plugin/Widget.qml"; then
  echo "bar-widget: the widget polls" >&2
  exit 1
fi

# Short: Quickshell puts a socket under the runtime directory, and a socket's path has little room.
work=$(mktemp -d /tmp/omacloud-bar.XXXXXX)
shell=
cleanup() {
  [ -n "$shell" ] && kill "$shell" 2>/dev/null || true
  wait 2>/dev/null || true
  rm -rf "${work:?}"
}
trap cleanup EXIT
mkdir -p "$work/shell/Ui" "$work/shell/Commons" "$work/shell/i18n" "$work/run" "$work/home"
chmod 700 "$work/run"
cp "$plugin/Widget.qml" "$here/bar-widget/shell.qml" "$work/shell/"
cp "$plugin/i18n/"*.json "$work/shell/i18n/"
cp "$here/bar-widget/qs/Ui/"*.qml "$work/shell/Ui/"
cp "$here/bar-widget/qs/Commons/"*.qml "$work/shell/Commons/"

env -u WAYLAND_DISPLAY -u DISPLAY HOME="$work/home" XDG_RUNTIME_DIR="$work/run" XDG_CACHE_HOME="$work/home/cache" \
  XDG_CONFIG_HOME="$work/home/config" XDG_STATE_HOME="$work/home/state" XDG_DATA_HOME="$work/home/data" \
  QT_QPA_PLATFORM=offscreen qs --no-color -p "$work/shell" >"$work/log" 2>&1 &
shell=$!

# The line as the daemon writes it: whole beside the file, then moved over it.
say() { # state text age-in-seconds
  mkdir -p "$work/run/omacloud"
  printf '{"state":"%s","text":"%s","at":%s}' "$1" "$2" $(($(date +%s) - ${3:-0})) >"$work/run/omacloud/bar.json.part"
  mv "$work/run/omacloud/bar.json.part" "$work/run/omacloud/bar.json"
}
# What the widget last said has to become `standing | tooltip` within the seconds given: far less than the half
# minute a poll took.
shows() { # seconds expected
  local last=
  for _ in $(seq $(($1 * 10))); do
    last=$(sed -n 's/.*standing: //p' "$work/log" | tail -n 1)
    [ "$last" = "$2" ] && return
    kill -0 "$shell" 2>/dev/null || break
    sleep 0.1
  done
  echo "bar-widget: expected '$2', the widget says '$last'" >&2
  cat "$work/log" >&2
  exit 1
}

# No daemon has written yet, and its folder is not there: the widget makes the folder to watch for the first line.
shows 10 'stopped | Omacloud: not running'
for _ in $(seq 100); do
  [ -d "$work/run/omacloud" ] && break
  sleep 0.1
done
[ -d "$work/run/omacloud" ] || { echo "bar-widget: the widget did not make the folder it watches" >&2; exit 1; }
# so that the watch is set before the first line, as it is long before a daemon's
sleep 0.5

say synced 'up to date'
shows 5 'synced | Omacloud: up to date'
say syncing syncing
shows 5 'syncing | Omacloud: syncing'
say paused 'paused on battery'
shows 5 'paused | Omacloud: paused on battery'
say attention 'stopped: open Omacloud to see why'
shows 5 'attention | Omacloud: stopped: open Omacloud to see why'

# A daemon that was killed: its last line is two seconds from old, and nothing more is written.
say synced 'up to date' 118
shows 5 'synced | Omacloud: up to date'
shows 6 'stopped | Omacloud: not running'
# One that comes back is believed at once,
say synced 'up to date'
shows 5 'synced | Omacloud: up to date'
# and one that stops says so itself.
say stopped 'not running'
shows 5 'stopped | Omacloud: not running'
# A line that is no JSON reads as no daemon, not as the one before.
say synced 'up to date'
shows 5 'synced | Omacloud: up to date'
printf 'half a li' >"$work/run/omacloud/bar.json"
shows 5 'stopped | Omacloud: not running'

# Markup in a line is shown as text, never read as a tag by the bar's tooltip.
say attention '<img src=x>'
shows 5 'attention | Omacloud: ‹img src=x›'
# The words come from the catalog: a line it knows is shown as the catalog says, and its tooltip too, as the
# catalog changes.
jq '.["daemon.up to date"] = "in step" | .tooltip = "Omacloud says: %1"' "$plugin/i18n/en.json" \
  >"$work/shell/i18n/en.json.part"
mv "$work/shell/i18n/en.json.part" "$work/shell/i18n/en.json"
say synced 'up to date'
shows 5 'synced | Omacloud says: in step'

echo "bar-widget: passed"
