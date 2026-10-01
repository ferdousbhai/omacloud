#!/usr/bin/env bash
# Cut a release from this machine: tag it, build the package, sign it and the
# [onecloud] pacman repository database with the local package-signing key,
# and publish them as one GitHub release. Computers that ran install.sh get it
# through `omarchy update`.
#
#   scripts/release.sh 0.0.1
#
# The tag is v<version>; the PKGBUILD takes its version from it. CHANGELOG.md
# and the workspace version must already say <version>. The release's assets
# are the package, the repository database, the signing key and install.sh.
# Once published, scripts/verify-release.sh installs it in a clean Arch
# container; if that fails, the release and tag are taken back down.
set -euo pipefail
cd "$(dirname "$0")/.."
repo=onecloud
gh_repo=ferdousbhai/onecloud

version=${1:-}
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "Usage: scripts/release.sh <major.minor.patch>" >&2; exit 1; }
tag=v$version

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

git tag -a "$tag" -m "OneCloud $version"
undo_tag() { git tag -d "$tag" >/dev/null 2>&1 || true; }

rm -rf dist
mkdir dist
(cd pkgbuild && PKGDEST="$PWD/../dist" makepkg --force --sign) \
  || { echo "Building the package failed; not releasing." >&2; undo_tag; exit 1; }
# makepkg writes the computed pkgver back into the PKGBUILD; keep the committed one.
git checkout -q -- pkgbuild/PKGBUILD
ls dist/"$repo-$version"-*.pkg.tar.zst >/dev/null \
  || { echo "The package isn't version $version." >&2; undo_tag; exit 1; }

(
  cd dist
  repo-add --sign --verify "$repo.db.tar.gz" ./*.pkg.tar.zst
  # repo-add leaves the names pacman asks for (onecloud.db, .files and their
  # .sig) as symlinks, which a GitHub release cannot hold: copy them.
  for name in db files; do
    rm -f "$repo.$name" "$repo.$name.sig"
    cp "$repo.$name.tar.gz" "$repo.$name"
    cp "$repo.$name.tar.gz.sig" "$repo.$name.sig"
  done
  gpg --batch --armor --export "$fingerprint" >"$repo-signing-key.asc"
  cp ../install.sh install.sh
)

git push -q origin "$tag"
notes=$(awk -v v="## $version" '$0 ~ "^"v {on=1; next} /^## / && on {exit} on' CHANGELOG.md)
notes+=$'\n\nInstall on Omarchy, then updates arrive through `omarchy update`:\n\n'
notes+='```'$'\n''curl -fsSL https://github.com/ferdousbhai/onecloud/releases/latest/download/install.sh | sudo bash'$'\n''```'
gh release create "$tag" dist/* --repo "$gh_repo" --title "OneCloud $version" --notes "$notes" --latest
echo "Published $tag: https://github.com/$gh_repo/releases/tag/$tag"

# A release is shipped only once its installer installs it. If it doesn't,
# take the release down so "latest" never points at a dud.
if ! scripts/verify-release.sh "$version"; then
  echo "Rolling back $tag." >&2
  gh release delete "$tag" --repo "$gh_repo" --yes
  git push -q origin --delete "$tag" || true
  undo_tag
  exit 1
fi
