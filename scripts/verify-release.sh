#!/usr/bin/env bash
# Prove a published release installs the way users get it: a clean Arch
# container runs the specified release's install.sh and must end up with
# omacloud <version>, the CLI must run, and the binaries must carry no
# home directory paths. scripts/release.sh runs this
# after publishing; only confirmed package defects trigger rollback.
#
#   scripts/verify-release.sh 0.0.1
set -euo pipefail

version=${1:?Usage: scripts/verify-release.sh <version> [x86_64|aarch64]}
architecture=${2:-x86_64}
case "$architecture" in
  x86_64) platform=linux/amd64; container_image=archlinux:base-devel ;;
  aarch64) platform=linux/arm64; container_image=${ARM_VERIFY_IMAGE:?Set ARM_VERIFY_IMAGE to an Arch Linux ARM image with pacman and archlinuxarm-keyring} ;;
  *) echo "Unsupported architecture: $architecture" >&2; exit 2 ;;
esac
command -v docker >/dev/null || { echo "docker is required to verify a release." >&2; exit 2; }
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "Invalid version." >&2; exit 2; }
release=https://github.com/ferdousbhai/omacloud/releases/download/v$version

# Exit 2 means setup/download/installation could not complete. Such failures
# may be infrastructure problems and must never trigger automatic deletion.
# Exit 1 is reserved for a defect confirmed after installation completed.
result=2
for attempt in 1 2 3; do
  result=0
  output=$(docker run --rm --platform "$platform" -e EXPECTED_ARCH="$architecture" \
    -e EXPECTED_VERSION="$version" -e RELEASE="$release" "$container_image" bash -euo pipefail -c '
    trap "exit 2" ERR
    [[ $(uname -m) == "$EXPECTED_ARCH" ]]
    sed -i "/^\[options\]$/a DisableSandbox" /etc/pacman.conf
    pacman-key --init
    if [[ $EXPECTED_ARCH == aarch64 ]]; then
      pacman-key --populate archlinuxarm
    fi
    pacman -Syu --noconfirm --needed curl gnupg
    installer=$(mktemp)
    curl -fsSL "$RELEASE/install.sh" -o "$installer"
    # Pin both the installer and its repository assets to the triggering tag.
    grep -q "^RELEASES=" "$installer"
    sed -i "s|^RELEASES=.*|RELEASES=$RELEASE|" "$installer"
    bash "$installer"
    rm -f "$installer"
    trap - ERR
    defect() { echo OMACLOUD_CONFIRMED_PACKAGE_FAILURE; exit 1; }
    omacloud --version >/dev/null || defect
    [[ $(pacman -Qi omacloud | sed -n "s/^Architecture *: //p") == "$EXPECTED_ARCH" ]] || defect
    [[ $(pacman -Q omacloud) == "omacloud $EXPECTED_VERSION-"* ]] || defect
    if grep -aq "/home/" /usr/bin/omacloud /usr/bin/omacloud-app; then defect; fi
  ' 2>&1) || result=$?
  printf '%s\n' "$output"
  if (( result == 0 )); then
    echo "Verified ($architecture): v$version installs and runs"
    exit 0
  fi
  if (( result == 1 )) && grep -qxF OMACLOUD_CONFIRMED_PACKAGE_FAILURE <<< "$output"; then
    [[ -z ${GITHUB_OUTPUT:-} ]] || echo "confirmed_failure=true" >> "$GITHUB_OUTPUT"
    echo "Confirmed package defect after installation ($architecture)." >&2
    exit 1
  fi
  echo "Verification could not complete ($architecture); attempt $attempt of 3." >&2
  (( attempt == 3 )) || sleep 20
done
echo "Infrastructure or installation failure; retain the release for investigation." >&2
exit 2
