#!/bin/bash
# Install Omacloud on Omarchy from its signed package repository, and keep it
# updating with the system:
#
#   curl -fsSL https://github.com/ferdousbhai/omacloud/releases/latest/download/install.sh | sudo bash
#
# Every step is idempotent, so re-running is safe. It trusts the
# package-signing key (checked against the fingerprint pinned below), adds
# the [omacloud] repository, installs an Omarchy hook that restores it after
# `omarchy refresh pacman` rewrites /etc/pacman.conf, and installs omacloud.
# From then on `omarchy update` brings new versions.
set -euo pipefail

REPO=omacloud
RELEASES=https://github.com/ferdousbhai/omacloud/releases/latest/download
SIGNING_KEY_FINGERPRINT=52130299581DAD685226900CA89CA1A6A1E74251

# --- add_signed_repo (shared) ---
# Trust a project's package-signing key (checked against the pinned
# fingerprint), add its signed pacman repository, and keep the repository
# across `omarchy refresh pacman`, which rewrites /etc/pacman.conf from
# Omarchy's defaults and then runs the user's pre-refresh-pacman hooks.
# Works as root (`sudo bash`) or as a desktop user (sudo inside). This text
# is identical in every installer that uses it, and each repository's test
# pins its hash: change it here and in its twins together.
add_signed_repo() {
  local name="$1" release="$2" fingerprint="$3"
  local conf="/etc/pacman.d/$name.conf" include="Include = /etc/pacman.d/$name.conf"
  local sudo='' key user home hook_dir
  (( EUID == 0 )) || sudo=sudo
  key="$(mktemp)"
  if ! curl -fsSL "$release/$name-signing-key.asc" -o "$key"; then
    rm -f "$key"
    echo "Could not download the package-signing key from $release." >&2
    return 1
  fi
  if ! gpg --batch --with-colons --show-keys "$key" 2>/dev/null | grep -q "^fpr:*:$fingerprint:"; then
    rm -f "$key"
    echo "The downloaded key does not match the pinned fingerprint $fingerprint; nothing was changed." >&2
    return 1
  fi
  $sudo pacman-key --add "$key"
  $sudo pacman-key --lsign-key "$fingerprint"
  rm -f "$key"
  printf '[%s]\nSigLevel = Required DatabaseRequired\nServer = %s\n' "$name" "$release" | $sudo tee "$conf" >/dev/null
  grep -qxF "$include" /etc/pacman.conf || printf '\n%s\n' "$include" | $sudo tee -a /etc/pacman.conf >/dev/null
  user="${SUDO_USER:-${USER:-$(id -un)}}"
  home="$(getent passwd "$user" | cut -d: -f6)"
  if [[ -n $home && -d $home/.config/omarchy ]]; then
    hook_dir="$home/.config/omarchy/hooks/pre-refresh-pacman.d"
    install -d -o "$user" -g "$(id -gn "$user")" "$hook_dir"
    printf '%s\n' '#!/bin/bash' \
      "# Restore the [$name] repository after Omarchy rewrote /etc/pacman.conf." \
      "grep -qxF '$include' /etc/pacman.conf || printf '\\n%s\\n' '$include' | sudo tee -a /etc/pacman.conf >/dev/null" \
      > "$hook_dir/$name"
    chown "$user" "$hook_dir/$name"
    chmod 755 "$hook_dir/$name"
  fi
  $sudo pacman -Sy
}
# --- end add_signed_repo ---

if ! command -v pacman >/dev/null; then
  echo "pacman not found: this installer is for Omarchy." >&2
  exit 1
fi
# GitHub release assets share one flat directory, so each architecture has
# its own database name. Keep the original x86_64 name for existing installs.
case "$(uname -m)" in
  x86_64) ;;
  aarch64) REPO=omacloud-aarch64 ;;
  *)
    echo "Unsupported architecture: $(uname -m). Omacloud supports x86_64 and aarch64; nothing was changed." >&2
    exit 1
    ;;
esac
if [[ ! $SIGNING_KEY_FINGERPRINT =~ ^[0-9A-F]{40}$ ]]; then
  echo "This copy of install.sh has no signing key pinned; nothing was changed." >&2
  exit 1
fi

# Before its rename this was OneCloud, from an [onecloud] repository with
# its own pacman.conf Include and Omarchy hook: those go, and so does the
# old package (its setup doesn't carry over; set up again in the app).
remove_onecloud() {
  local sudo='' conf=/etc/pacman.d/onecloud.conf include rest user home
  (( EUID == 0 )) || sudo=sudo
  include="Include = $conf"
  if pacman -Q onecloud >/dev/null 2>&1; then
    echo "Removing OneCloud, the earlier name"
    $sudo pacman -Rns --noconfirm onecloud
  fi
  if grep -qxF "$include" /etc/pacman.conf; then
    rest="$(grep -vxF "$include" /etc/pacman.conf)"
    printf '%s\n' "$rest" | $sudo tee /etc/pacman.conf >/dev/null
  fi
  $sudo rm -f "$conf"
  user="${SUDO_USER:-${USER:-$(id -un)}}"
  home="$(getent passwd "$user" | cut -d: -f6)"
  [[ -n $home ]] && rm -f "$home/.config/omarchy/hooks/pre-refresh-pacman.d/onecloud"
  return 0
}
# first: syncing would fail on the old repository, which no longer exists
remove_onecloud

# Earlier installers added the x86_64 repository even on ARM. Leaving it
# enabled would keep exposing incompatible packages during system updates.
if [[ $REPO == omacloud-aarch64 ]]; then
  remove_x86_repo() {
    local sudo='' include='Include = /etc/pacman.d/omacloud.conf' rest user home
    (( EUID == 0 )) || sudo=sudo
    if grep -qxF "$include" /etc/pacman.conf; then
      rest="$(grep -vxF "$include" /etc/pacman.conf || true)"
      printf '%s\n' "$rest" | $sudo tee /etc/pacman.conf >/dev/null
    fi
    $sudo rm -f /etc/pacman.d/omacloud.conf
    user="${SUDO_USER:-${USER:-$(id -un)}}"
    home="$(getent passwd "$user" | cut -d: -f6)"
    [[ -z $home ]] || $sudo rm -f "$home/.config/omarchy/hooks/pre-refresh-pacman.d/omacloud"
  }
  remove_x86_repo
fi

echo "Adding the [$REPO] repository"
# The shared helper downloads a key named after its repository, so releases
# publish the same pinned key under both repository names.
add_signed_repo "$REPO" "$RELEASES" "$SIGNING_KEY_FINGERPRINT"

echo "Installing omacloud"
if command -v omarchy-pkg-add >/dev/null; then
  # Omarchy's pacman hook aborts a direct `pacman -Syu` (system upgrades go
  # through `omarchy update`), so install the way Omarchy installs its own
  # apps; the next `omarchy update` brings everything current.
  omarchy-pkg-add omacloud
else
  # Upgrade and install in one transaction: installing from freshly synced
  # databases without upgrading is Arch's unsupported partial upgrade.
  if (( EUID == 0 )); then
    pacman -Syu --needed --noconfirm omacloud
  else
    sudo pacman -Syu --needed --noconfirm omacloud
  fi
fi

cat <<EOT

Done. Open "Omacloud" from the app launcher (Super + Space) to set up this
computer: your own storage bucket, a join code from one of your computers,
or, optionally, Omacloud storage with a Google sign-in (by invitation for
now). Updates arrive with the rest of the system through:
omarchy update
EOT
