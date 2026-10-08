#!/usr/bin/env bash
# Cut a release from this machine: tag it, build the package, sign it and the
# [omacloud] pacman repository database with the local package-signing key,
# and publish them as one GitHub release. Computers that ran install.sh get it
# through `omarchy update`.
#
#   scripts/release.sh 0.0.1 /path/to/other-architecture.pkg.tar.zst
#
# The tag is v<version>; the PKGBUILD takes its version from it. CHANGELOG.md
# and the workspace version must already say <version>. The release's assets
# are the package, the repository database, the signing key and install.sh.
# Once published, scripts/verify-release.sh installs it in a clean Arch
# container; if that fails, the release and tag are taken back down.
set -euo pipefail
cd "$(dirname "$0")/.."
repo=omacloud
gh_repo=ferdousbhai/omacloud

version=${1:-}
shift || true
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "Usage: scripts/release.sh <major.minor.patch> <other-architecture.pkg.tar.zst>" >&2; exit 1; }
tag=v$version
# Build natively here; provide the package built from the same release
# source on the other architecture. Both are signed with the local key.
(( $# == 1 )) || { echo "Supply a native package for the other architecture." >&2; exit 1; }
other_package=$(realpath "$1")
[[ -f $other_package ]] || { echo "Package not found: $other_package" >&2; exit 1; }
[[ $other_package != "$PWD/dist/"* ]] || { echo "Keep the supplied package outside dist (which is rebuilt)." >&2; exit 1; }
# ARM verification needs an Arch Linux ARM container with pacman and
# its distribution keyring installed; there is no official multiarch archlinux Docker image.
[[ -n ${ARM_VERIFY_IMAGE:-} ]] || { echo "Set ARM_VERIFY_IMAGE to your Arch Linux ARM verification image." >&2; exit 1; }

case "$(uname -m)" in
  x86_64) other_architecture=aarch64 ;;
  aarch64) other_architecture=x86_64 ;;
  *) echo "Release builds require x86_64 or aarch64." >&2; exit 1 ;;
esac
validate_package() {
  local package=$1 architecture=$2 metadata field
  [[ $(basename "$package") == "$repo-$version-1-$architecture.pkg.tar.zst" ]] \
    || { echo "Expected $repo-$version-1-$architecture.pkg.tar.zst, got $package." >&2; return 1; }
  metadata=$(bsdtar -xOf "$package" .PKGINFO) || return 1
  for field in "pkgname = $repo" "pkgver = $version-1" "arch = $architecture"; do
    grep -qxF "$field" <<< "$metadata" \
      || { echo "$package: expected $field" >&2; return 1; }
  done
}
validate_package "$other_package" "$other_architecture" || exit 1
source_manifest=$(dirname "$other_package")/source-commit.txt
if [[ -f $source_manifest ]]; then
  [[ $(cat "$source_manifest") == "$(git rev-parse HEAD)" ]] \
    || { echo "The supplied CI package was built from a different commit." >&2; exit 1; }
fi

[[ -z $(git status --porcelain) ]] || { echo "Commit or stash your changes first." >&2; exit 1; }
[[ $(git branch --show-current) == master ]] || { echo "Release from master." >&2; exit 1; }
! git rev-parse -q --verify "refs/tags/$tag" >/dev/null || { echo "$tag already exists." >&2; exit 1; }
grep -q "^version = \"$version\"" Cargo.toml || { echo "Cargo.toml's version isn't $version." >&2; exit 1; }
grep -q "^## $version" CHANGELOG.md || { echo "CHANGELOG.md has no $version section." >&2; exit 1; }

# install.sh pins the fingerprint users trust; the key that signs must be it.
fingerprint=$(sed -n 's/^SIGNING_KEY_FINGERPRINT=//p' install.sh)
gpg --batch --list-secret-keys "$fingerprint" >/dev/null 2>&1 \
  || { echo "The secret key $fingerprint pinned in install.sh is not in this keyring." >&2; exit 1; }
export GPGKEY=$fingerprint

cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace >/dev/null 2>&1 || { cargo test --workspace 2>&1 | grep -E 'FAILED|panicked|^error' >&2; echo "Tests failed; not releasing." >&2; exit 1; }

git tag -a "$tag" -m "Omacloud $version"
undo_tag() { git tag -d "$tag" >/dev/null 2>&1 || true; }

rm -rf dist
mkdir dist
(cd pkgbuild && PKGDEST="$PWD/../dist" PKGEXT=.pkg.tar.zst makepkg --force) \
  || { git checkout -q -- pkgbuild/PKGBUILD; echo "Building the package failed; not releasing." >&2; undo_tag; exit 1; }
# makepkg writes the computed pkgver back into the PKGBUILD; keep the committed one.
git checkout -q -- pkgbuild/PKGBUILD
cp "$other_package" dist/ || { undo_tag; exit 1; }

(
  cd dist
  for architecture in x86_64 aarch64; do
    package="$repo-$version-1-$architecture.pkg.tar.zst"
    [[ -f $package ]] || { echo "Missing $package; both architectures are required." >&2; exit 1; }
    validate_package "$package" "$architecture" || exit 1
    gpg --batch --yes --local-user "$fingerprint" --detach-sign "$package" || exit 1
    database=$repo
    [[ $architecture == x86_64 ]] || database=$repo-$architecture
    repo-add --sign --verify "$database.db.tar.gz" "$package" || exit 1
    # GitHub assets cannot hold the symlinks repo-add creates.
    for name in db files; do
      rm -f "$database.$name" "$database.$name.sig"
      cp "$database.$name.tar.gz" "$database.$name" || exit 1
      cp "$database.$name.tar.gz.sig" "$database.$name.sig" || exit 1
    done
    gpg --batch --armor --export-filter keep-uid="uid =~ Omacloud" --export "$fingerprint" >"$database-signing-key.asc" || exit 1
  done
  cp ../install.sh install.sh || exit 1
) || { echo "Preparing the repositories failed; not releasing." >&2; undo_tag; exit 1; }

git push -q origin "$tag"
notes=$(awk -v v="## $version" '$0 ~ "^"v {on=1; next} /^## / && on {exit} on' CHANGELOG.md)
notes+=$'\n\nInstall on Omarchy, then updates arrive through `omarchy update`:\n\n'
notes+='```'$'\n''curl -fsSL https://github.com/ferdousbhai/omacloud/releases/latest/download/install.sh | sudo bash'$'\n''```'
notes_file=$(mktemp)
trap 'rm -f "$notes_file"' EXIT
printf '%s\n' "$notes" > "$notes_file"
gh release create "$tag" dist/* --repo "$gh_repo" --title "Omacloud $version" --notes-file "$notes_file" --latest
echo "Published $tag: https://github.com/$gh_repo/releases/tag/$tag"

# Infrastructure/installer failures retain the release for investigation.
for architecture in x86_64 aarch64; do
  result=0
  scripts/verify-release.sh "$version" "$architecture" || result=$?
  if (( result == 1 )); then
    echo "Rolling back $tag after a confirmed package defect." >&2
    gh release delete "$tag" --repo "$gh_repo" --yes
    git push -q origin --delete "$tag" || true
    undo_tag
    exit 1
  elif (( result != 0 )); then
    echo "Verification incomplete; retaining $tag for investigation." >&2
    exit "$result"
  fi
done
