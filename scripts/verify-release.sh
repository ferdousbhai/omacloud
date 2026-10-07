#!/usr/bin/env bash
# Prove a published release installs the way users get it: a clean Arch
# container runs the latest release's install.sh and must end up with
# omacloud <version>, the CLI must run, and the binaries must carry no
# home directory paths. scripts/release.sh runs this
# after publishing and takes the release down if it fails.
#
#   scripts/verify-release.sh 0.0.1
set -euo pipefail

version=${1:?Usage: scripts/verify-release.sh <version> [x86_64|aarch64]}
architecture=${2:-x86_64}
case "$architecture" in
  x86_64) platform=linux/amd64; container_image=archlinux:base-devel ;;
  aarch64) platform=linux/arm64; container_image=${ARM_VERIFY_IMAGE:?Set ARM_VERIFY_IMAGE to an Arch Linux ARM image with pacman and archlinuxarm-keyring} ;;
  *) echo "Unsupported architecture: $architecture" >&2; exit 1 ;;
esac
command -v docker >/dev/null || { echo "docker is required to verify a release." >&2; exit 1; }
installer=https://github.com/ferdousbhai/omacloud/releases/latest/download/install.sh

# GitHub's "latest" redirect can lag a new release by a little; try for a while.
for attempt in 1 2 3 4 5 6; do
  installed="$(docker run --rm --platform "$platform" -e EXPECTED_ARCH="$architecture" -e INSTALLER="$installer" "$container_image" bash -euo pipefail -c '
    [[ $(uname -m) == "$EXPECTED_ARCH" ]]
    # Keep pacman downloads compatible with Docker's seccomp profile.
    sed -i "/^\[options\]$/a DisableSandbox" /etc/pacman.conf
    pacman-key --init >/dev/null 2>&1 || true
    if [[ $EXPECTED_ARCH == aarch64 ]]; then
      pacman-key --populate archlinuxarm >/dev/null 2>&1
    fi
    pacman -Syu --noconfirm --needed curl gnupg >/dev/null 2>&1
    curl -fsSL "$INSTALLER" | bash >/dev/null 2>&1
    omacloud --version >/dev/null
    [[ $(pacman -Qi omacloud | sed -n '"'"'s/^Architecture *: //p'"'"') == "$EXPECTED_ARCH" ]]
    # no builder paths (and so no username) in the binaries
    if grep -aq "/home/" /usr/bin/omacloud /usr/bin/omacloud-app; then exit 1; fi
    pacman -Q omacloud' 2>/dev/null || true)"
  if [[ $installed == "omacloud $version-"* ]]; then
    echo "Verified ($architecture): install.sh installs $installed"
    exit 0
  fi
  echo "Attempt $attempt: got '${installed:-nothing}', wanted omacloud $version; retrying in 20s" >&2
  sleep 20
done
echo "install.sh does not install omacloud $version." >&2
exit 1
