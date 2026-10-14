#!/usr/bin/env bash
# Build a release candidate natively in an Arch container. Tags made here
# stay inside the container; the output is unsigned until release.sh signs it.
set -euo pipefail
cd "$(dirname "$0")/.."

architecture=${1:?Usage: scripts/build-package.sh <x86_64|aarch64> <version> [output-directory]}
version=${2:?Usage: scripts/build-package.sh <x86_64|aarch64> <version> [output-directory]}
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "Invalid version: $version" >&2; exit 1; }
case "$architecture" in
  x86_64) platform=linux/amd64; image=archlinux:base-devel ;;
  aarch64) platform=linux/arm64; image=${ARM_VERIFY_IMAGE:?Set ARM_VERIFY_IMAGE to an Arch Linux ARM container image} ;;
  *) echo "Unsupported architecture: $architecture" >&2; exit 1 ;;
esac
grep -qxF "version = \"$version\"" Cargo.toml || { echo "Cargo.toml's version isn't $version." >&2; exit 1; }
[[ -z $(git status --porcelain) ]] || { echo "Commit or stash your changes first; containers build committed source." >&2; exit 1; }
output=$(realpath -m "${3:-dist/$architecture}")
# The container is discarded. A cache left only on the host never reaches pacman or Cargo.
cache=$(realpath -m "${PACKAGE_CACHE:-$PWD/target/package-cache/$architecture}")
mkdir -p "$output" "$cache/pacman" "$cache/cargo"

docker run --rm --platform "$platform" \
  -e BUILD_ARCH="$architecture" -e BUILD_VERSION="$version" \
  -e OUTPUT_UID="$(id -u)" -e OUTPUT_GID="$(id -g)" \
  -e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/cargo/target \
  -v "$PWD:/source:ro" -v "$output:/output" \
  -v "$cache/pacman:/var/cache/pacman/pkg" \
  -v "$cache/cargo:/cargo" \
  "$image" bash -euo pipefail -c '
    [[ $(uname -m) == "$BUILD_ARCH" ]]
    # the Docker seccomp profile can reject pacman Landlock setup. This
    # setting applies only to the disposable build container.
    sed -i "/^\[options\]$/a DisableSandbox" /etc/pacman.conf
    pacman-key --init
    if [[ $BUILD_ARCH == aarch64 ]]; then
      pacman-key --populate archlinuxarm
    else
      pacman-key --populate archlinux
    fi
    pacman -Syu --noconfirm --needed base-devel cargo git gtk4 libadwaita libsecret
    git config --global --add safe.directory /source
    git config --global --add safe.directory /source/.git
    git clone --no-hardlinks /source /build
    source_commit=$(git -C /source rev-parse HEAD)
    [[ $(git -C /build rev-parse HEAD) == "$source_commit" ]]
    # pkgver() reads the version from the tag, so the commit being built gets
    # it here. After a release, CI builds commits past the release tag;
    # this clone is discarded, and source-commit.txt names what was built.
    git -C /build tag -f "v$BUILD_VERSION" >/dev/null
    if ! id "$OUTPUT_UID" >/dev/null 2>&1; then
      getent group "$OUTPUT_GID" >/dev/null || groupadd -g "$OUTPUT_GID" builder
      useradd -u "$OUTPUT_UID" -g "$OUTPUT_GID" -m builder
    fi
    builder=$(id -nu "$OUTPUT_UID")
    mkdir -p /cargo/target
    chown -R "$builder:$OUTPUT_GID" /build /output /cargo
    su "$builder" -c "export CARGO_HOME=/cargo CARGO_TARGET_DIR=/cargo/target; cd /build/pkgbuild && PKGDEST=/output PKGEXT=.pkg.tar.zst makepkg --force"
    package=/output/omacloud-$BUILD_VERSION-1-$BUILD_ARCH.pkg.tar.zst
    metadata=$(bsdtar -xOf "$package" .PKGINFO)
    grep -qxF "arch = $BUILD_ARCH" <<< "$metadata"
    grep -qxF "pkgver = $BUILD_VERSION-1" <<< "$metadata"
    [[ $(/cargo/target/release/omacloud --version) == "omacloud $BUILD_VERSION" ]]
    if grep -aq "/home/" /cargo/target/release/omacloud /cargo/target/release/omacloud-app; then
      echo "Builder home paths found in the binaries." >&2
      exit 1
    fi
    printf "%s\n" "$source_commit" > /output/source-commit.txt
    chown -R "$OUTPUT_UID:$OUTPUT_GID" /output /cargo /var/cache/pacman/pkg
  '
