#!/usr/bin/env bash
set -Eeuo pipefail

# The app in a nested Hyprland with a private HOME, XDG directories and D-Bus, on dev/fake-omacloud's made-up
# account: nothing of the desktop it is run from is read or shown on, and no real account or storage is reached.
#
#   dev/preview.sh <directory for the pictures> [theme-dir]
#
# `theme-dir` holds a `colors.toml` (an Omarchy theme, such as /usr/share/omarchy/themes/catppuccin-latte) and is
# copied in as the scratch desktop's current theme; without one the app shows its fallback.
#
#   OMACLOUD_PREVIEW_STEPS   what to do, in order, separated by spaces (default: "state=account open shot=overview"):
#                              state=new|gone|error|account   what the made-up `omacloud status` answers from now on
#                              open or open=<page>            start the app (again), on that page
#                              key=<name> or key=<name>*<n>   press a key as `wtype -k` names it, with an optional
#                                                             modifier (key=shift+Tab), n times
#                              type=<text>                    type it
#                              shot=<name>                    a picture, <directory>/<name>.png
#                              sleep=<seconds>
#                              clip                           note what the clipboard holds in calls.log
#                              note=<word>                    a line in calls.log, to tell the steps apart there
#   OMACLOUD_PREVIEW_BINARY  the app to run (default target/debug/omacloud-app)
#   OMACLOUD_PREVIEW_SIZE    the size of the window and of the pictures (default 1000x640)
#
# <directory>/calls.log lists every `omacloud` the app ran, in order, and <directory>/app.log what the app printed.
if (( $# < 1 || $# > 2 )); then
  echo "usage: $0 <directory> [theme-dir]" >&2
  exit 2
fi
out=$(realpath -m -- "$1")
theme=${2:-}

here=$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
binary=$(realpath -m -- "${OMACLOUD_PREVIEW_BINARY:-$here/../../../target/debug/omacloud-app}")
[[ -x $binary ]] || { echo "preview: build the app first (cargo build -p omacloud-app)" >&2; exit 1; }
for command_name in Hyprland hyprctl grim jq dbus-daemon setsid python3 wtype wl-paste; do
  command -v "$command_name" >/dev/null || { echo "preview: $command_name is required" >&2; exit 1; }
done
[[ -z $theme || -f $theme/colors.toml ]] || { echo "preview: $theme has no colors.toml" >&2; exit 1; }
parent_runtime=${XDG_RUNTIME_DIR:?preview: XDG_RUNTIME_DIR is required}
parent_display=${WAYLAND_DISPLAY:?preview: run this from a Wayland desktop}
parent_socket=$parent_runtime/$parent_display
size=${OMACLOUD_PREVIEW_SIZE:-1000x640}
[[ $size =~ ^[0-9]+x[0-9]+$ ]] || { echo "preview: OMACLOUD_PREVIEW_SIZE is <width>x<height>" >&2; exit 2; }
steps=${OMACLOUD_PREVIEW_STEPS:-state=account open shot=overview}

root=""
runtime=""
hypr_pid=""
app_pid=""
dbus_pid=""
stop() {
  # Only the PIDs this script started.
  local pid
  for pid in "$@"; do
    [[ -z $pid ]] || kill -TERM "$pid" 2>/dev/null || true
  done
  for _ in {1..30}; do
    local alive=0
    for pid in "$@"; do
      [[ -z $pid ]] || ! kill -0 "$pid" 2>/dev/null || alive=1
    done
    (( alive == 0 )) && break
    sleep 0.1
  done
  for pid in "$@"; do
    [[ -z $pid ]] || kill -KILL "$pid" 2>/dev/null || true
  done
}
cleanup() {
  local status=$?
  trap - EXIT INT TERM
  stop "$app_pid" "$hypr_pid" "$dbus_pid"
  [[ -z $root || $root != /tmp/omacloud-preview.* ]] || rm -rf -- "$root"
  [[ -z $runtime || $runtime != /tmp/omc.* ]] || rm -rf -- "$runtime"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# Wayland and Hyprland IPC use AF_UNIX paths capped at 108 bytes.
root=$(mktemp -d /tmp/omacloud-preview.XXXXXX)
runtime=$(mktemp -d /tmp/omc.XXXXXX)
chmod 700 "$root" "$runtime"
mkdir -p "$root"/{home,config,data,cache,fake,bin} "$out"

export HOME=$root/home
export XDG_CONFIG_HOME=$root/config XDG_STATE_HOME=$HOME/.local/state XDG_DATA_HOME=$root/data XDG_CACHE_HOME=$root/cache
export XDG_RUNTIME_DIR=$runtime
unset GTK_THEME

# The app's CLI is the made-up one, and the config it names does not exist.
export OMACLOUD_BIN=$here/fake-omacloud OMACLOUD_CONFIG=$root/config/omacloud/config.toml
export OMACLOUD_FAKE_STATE=$root/fake
calls=$root/fake/calls.log
: > "$calls"
# The other programs the app runs are stand-ins too, ahead of anything installed.
"$here/stand-ins" "$root/bin" "$calls"
export PATH=$root/bin:/usr/bin
mkdir -p "$HOME"/{Documents,Pictures,Work,Music}
head -c 300000 /dev/zero > "$HOME/Documents/report.pdf"

if [[ -n $theme ]]; then
  current=$XDG_STATE_HOME/omarchy/current
  mkdir -p "$current/theme"
  cp -- "$theme/colors.toml" "$current/theme/colors.toml"
  basename -- "$(realpath -- "$theme")" > "$current/theme.name"
fi

# A session bus with no service directories: nothing can be dbus-activated on it.
cat > "$root/dbus.conf" <<EOF_DBUS
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:abstract=omacloud-preview-$BASHPID</listen>
  <keep_umask/>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
EOF_DBUS
mapfile -t dbus_details < <(/usr/bin/dbus-daemon --config-file="$root/dbus.conf" --fork --print-address=1 --print-pid=1)
(( ${#dbus_details[@]} == 2 )) || { echo "preview: could not start the isolated session bus" >&2; exit 1; }
export DBUS_SESSION_BUS_ADDRESS=${dbus_details[0]}
dbus_pid=${dbus_details[1]}

printf '%s\n' \
  "hl.monitor({ output = \"\", mode = \"$size@60\", position = \"auto\", scale = 1 })" \
  'hl.config({' \
  '  general = { gaps_in = 0, gaps_out = 0, border_size = 0 },' \
  '  decoration = { rounding = 0, shadow = { enabled = false }, blur = { enabled = false } },' \
  '  animations = { enabled = false },' \
  '  misc = { disable_hyprland_logo = true, disable_splash_rendering = true },' \
  '})' \
  'hl.window_rule({ match = { class = ".*" }, monitor = "omacloud" })' > "$root/hyprland.lua"
unset HYPRLAND_INSTANCE_SIGNATURE
/usr/bin/setsid /usr/bin/env WAYLAND_DISPLAY="$parent_socket" /usr/bin/Hyprland --config "$root/hyprland.lua" > "$root/hyprland.log" 2>&1 &
hypr_pid=$!
signature=""
display=""
for _ in {1..120}; do
  kill -0 "$hypr_pid" 2>/dev/null || { echo "preview: nested Hyprland exited during startup" >&2; sed -n '1,80p' "$root/hyprland.log" >&2; exit 1; }
  for instance in "$XDG_RUNTIME_DIR"/hypr/*; do
    [[ -S $instance/.socket.sock && -f $instance/hyprland.lock ]] || continue
    candidate=$(sed -n '2p' "$instance/hyprland.lock")
    [[ -n $candidate && -S $XDG_RUNTIME_DIR/$candidate ]] || continue
    signature=$(basename "$instance")
    display=$candidate
    break 2
  done
  sleep 0.1
done
[[ -n $signature ]] || { echo "preview: nested Hyprland did not publish its sockets" >&2; exit 1; }
export HYPRLAND_INSTANCE_SIGNATURE=$signature WAYLAND_DISPLAY=$display

# The nested compositor's own output is a window on the desktop this runs from, as large as that desktop tiles it.
# The app goes on a headless output instead, whose size is the monitor rule's; the window rule sends it there.
hyprctl output create headless omacloud >/dev/null

on_output() {
  local output
  output=$(hyprctl monitors -j | jq 'first(.[] | select(.name == "omacloud") | .id)') &&
    hyprctl clients -j | jq -e --argjson output "${output:-null}" 'any(.[]; .mapped and .monitor == $output)' >/dev/null
}
open() {
  stop "$app_pid"
  GDK_BACKEND=wayland "$binary" "$@" >> "$out/app.log" 2>&1 &
  app_pid=$!
  for _ in {1..100}; do
    on_output && break
    kill -0 "$app_pid" 2>/dev/null || break
    sleep 0.1
  done
  # The first `status` answers and the pages are drawn.
  sleep 2
}
shoot() {
  sleep 1
  kill -0 "$app_pid" 2>/dev/null || { echo "preview: the app exited" >&2; sed -n '1,40p' "$out/app.log" >&2; exit 1; }
  # A picture of an output the window is not on would be blank, and look like a result.
  on_output || { echo "preview: the app's window is not on the output being captured" >&2; hyprctl clients >&2; exit 1; }
  # Hyprland's own notices (it says it was started without its wrapper) would cover the header.
  hyprctl dismissnotify >/dev/null
  /usr/bin/grim -o omacloud "$out/$1.png"
  echo "preview: $out/$1.png"
}
press() {
  local key=${1%%\**} times=1
  [[ $1 != *\** ]] || times=${1##*\*}
  for (( i = 0; i < times; i++ )); do
    if [[ $key == *+* ]]; then
      wtype -M "${key%%+*}" -k "${key#*+}" -m "${key%%+*}"
    else
      wtype -k "$key"
    fi
    sleep 0.3
  done
}

: > "$out/app.log"
for step in $steps; do
  case $step in
    state=*) printf '%s\n' "${step#state=}" > "$root/fake/state" ;;
    open) open ;;
    open=*) open "${step#open=}" ;;
    key=*) press "${step#key=}" ;;
    type=*) wtype -- "${step#type=}"; sleep 0.3 ;;
    shot=*) shoot "${step#shot=}" ;;
    sleep=*) sleep "${step#sleep=}" ;;
    clip) echo "clipboard: $(wl-paste --no-newline 2>/dev/null || true)" >> "$calls" ;;
    note=*) echo "-- ${step#note=}" >> "$calls" ;;
    *) echo "preview: unknown step $step" >&2; exit 2 ;;
  esac
done
cp -- "$calls" "$out/calls.log"
