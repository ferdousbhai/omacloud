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
mkdir -p "$output"

docker run --rm --platform "$platform" \
  -e BUILD_ARCH="$architecture" -e BUILD_VERSION="$version" \
  -e OUTPUT_UID="$(id -u)" -e OUTPUT_GID="$(id -g)" \
  -v "$PWD:/source:ro" -v "$output:/output" "$image" bash -euo pipefail -c '
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
    tag=v$BUILD_VERSION
    if git -C /build rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
      [[ $(git -C /build rev-list -n1 "$tag") == "$source_commit" ]]
    else
      git -C /build tag "$tag"
    fi
    useradd -m builder
    chown -R builder:builder /build /output
    su builder -c "cd /build/pkgbuild && PKGDEST=/output PKGEXT=.pkg.tar.zst makepkg --force"
    package=/output/omacloud-$BUILD_VERSION-1-$BUILD_ARCH.pkg.tar.zst
    metadata=$(bsdtar -xOf "$package" .PKGINFO)
    grep -qxF "arch = $BUILD_ARCH" <<< "$metadata"
    grep -qxF "pkgver = $BUILD_VERSION-1" <<< "$metadata"
    [[ $(/build/pkgbuild/src/omacloud/target/release/omacloud --version) == "omacloud $BUILD_VERSION" ]]
    if grep -aq "/home/" /build/pkgbuild/src/omacloud/target/release/omacloud /build/pkgbuild/src/omacloud/target/release/omacloud-app; then
      echo "Builder home paths found in the binaries." >&2
      exit 1
    fi
    printf "%s\n" "$source_commit" > /output/source-commit.txt
    chown -R "$OUTPUT_UID:$OUTPUT_GID" /output
  '
