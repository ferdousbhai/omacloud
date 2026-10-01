#!/usr/bin/env bash
# Prove a published release installs the way users get it: a clean Arch
# container runs the latest release's install.sh and must end up with
# onecloud <version>, and the CLI must run. scripts/release.sh runs this
# after publishing and takes the release down if it fails.
#
#   scripts/verify-release.sh 0.0.1
set -euo pipefail

version=${1:?Usage: scripts/verify-release.sh <version>}
command -v docker >/dev/null || { echo "docker is required to verify a release." >&2; exit 1; }
installer=https://github.com/ferdousbhai/onecloud/releases/latest/download/install.sh

# GitHub's "latest" redirect can lag a new release by a little; try for a while.
for attempt in 1 2 3 4 5 6; do
  installed="$(docker run --rm -e INSTALLER="$installer" archlinux:base-devel bash -euo pipefail -c '
    pacman-key --init >/dev/null 2>&1 || true
    pacman -Sy --noconfirm --needed curl gnupg >/dev/null 2>&1
    curl -fsSL "$INSTALLER" | bash >/dev/null 2>&1
    onecloud --version >/dev/null
    pacman -Q onecloud' 2>/dev/null || true)"
  if [[ $installed == "onecloud $version-"* ]]; then
    echo "Verified: install.sh installs $installed"
    exit 0
  fi
  echo "Attempt $attempt: got '${installed:-nothing}', wanted onecloud $version; retrying in 20s" >&2
  sleep 20
done
echo "install.sh does not install onecloud $version." >&2
exit 1
