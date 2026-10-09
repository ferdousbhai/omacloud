#!/usr/bin/env bash
# Carry a published release to Omarchy's package repository. Renders the
# recipe in packaging/omarchy-pkgs/omacloud (as of tag v<version>) for that
# version and the sha256 of GitHub's tag tarball, then:
#
# - while omacom/omarchy-pkgs#778 is open, pushes it to the pull request's
#   branch (omacloud on ferdousbhai/omarchy-pkgs) when it changed;
# - once #778 is merged, Omarchy's sync-upstream bot bumps pkgver and
#   sha256sums for each tag by itself, so this opens a pull request only
#   when the recipe changed in some other way.
#
#   scripts/omarchy-pkgs.sh <version> [--dry-run]
#
# --dry-run renders and compares, and says what it would push or open.
# scripts/release-ci.sh runs this after publishing; it can be rerun safely.
# If Release verification removes the release, run it for the version that
# is then the latest, so the open pull request builds that again.
set -euo pipefail
cd "$(dirname "$0")/.."
repository=ferdousbhai/omacloud
upstream=omacom/omarchy-pkgs
fork=ferdousbhai/omarchy-pkgs
pull_request=778
recipe=packaging/omarchy-pkgs/omacloud
usage='Usage: scripts/omarchy-pkgs.sh <version> [--dry-run]'
version=${1:?$usage}
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "Invalid version." >&2; exit 1; }
dry_run=0
case ${2:-} in
  '') ;;
  --dry-run) dry_run=1 ;;
  *) echo "$usage" >&2; exit 1 ;;
esac
tag=v$version

# The rendered recipe, kept under dist for inspection.
render=$PWD/dist/omarchy-pkgs-$version/omacloud
rm -rf "${render:?}"
mkdir -p "$render/.omarchy"
for path in PKGBUILD .omarchy/package.json; do
  gh api "repos/$repository/contents/$recipe/$path?ref=$tag" --jq .content | base64 --decode > "$render/$path"
done
sum=$(curl -fsSL "https://github.com/$repository/archive/refs/tags/$tag.tar.gz" | sha256sum | cut -d' ' -f1)
[[ $sum =~ ^[0-9a-f]{64}$ ]] || { echo "Could not hash the $tag tarball." >&2; exit 1; }
sed -i "s/^pkgver=.*/pkgver=$version/; s/^pkgrel=.*/pkgrel=1/; s/^sha256sums=.*/sha256sums=('$sum')/" "$render/PKGBUILD"
grep -qxF "pkgver=$version" "$render/PKGBUILD" && grep -qxF "sha256sums=('$sum')" "$render/PKGBUILD" \
  || { echo "Could not render the recipe for $version." >&2; exit 1; }
echo "Rendered the Omarchy recipe for $version in $render"

state=$(gh pr view "$pull_request" --repo "$upstream" --json state --jq .state)
case $state in
  OPEN) source_repository=$fork; branch=omacloud ;;
  MERGED) source_repository=$upstream; branch= ;;  # its default branch
  *)
    echo "$upstream#$pull_request is ${state:-unknown}, so there is nothing to update." >&2
    echo "Next: carry $render to pkgbuilds/omacloud in $upstream by hand." >&2
    exit 1
    ;;
esac

work=$(mktemp -d)
trap 'rm -rf "${work:?}"' EXIT
git clone -q --filter=blob:none ${branch:+--branch "$branch"} "https://github.com/$source_repository.git" "$work/pkgs"
cd "$work/pkgs"
base=$(git rev-parse --abbrev-ref HEAD)
rm -rf pkgbuilds/omacloud
cp -a "$render" pkgbuilds/omacloud
git add -A pkgbuilds/omacloud
commit() {
  git -c user.name="${GIT_AUTHOR_NAME:-Ferdous Bhai}" \
      -c user.email="${GIT_AUTHOR_EMAIL:-23114244+ferdousbhai@users.noreply.github.com}" \
      commit -q -m "omacloud $version"
}

if [[ $state == OPEN ]]; then
  if git diff --cached --quiet; then
    echo "$upstream#$pull_request already builds omacloud $version."
  elif ((dry_run)); then
    git diff --cached --stat
    echo "Dry run: would push this to $fork $branch, updating $upstream#$pull_request."
  else
    commit
    git push -q origin "HEAD:refs/heads/$branch"
    echo "Pushed omacloud $version to $fork $branch."
  fi
  echo "Next: nothing here; $upstream#$pull_request waits for Omarchy's review."
  exit 0
fi

# Merged: pkgver, pkgrel and sha256sums follow each tag through Omarchy's bot.
if git diff --cached --quiet -I '^(pkgver|pkgrel|sha256sums)='; then
  echo "Omarchy's recipe differs from ours only in what its bot updates."
  echo "Next: nothing; Omarchy's sync-upstream bot picks up $tag (fast ring, after min_release_age)."
  exit 0
fi
git diff --cached
new_branch=omacloud-$version
if ((dry_run)); then
  echo "Dry run: would push this to $fork $new_branch and open a pull request against $upstream $base."
  exit 0
fi
git switch -q -c "$new_branch"
commit
git push -q --force "https://github.com/$fork.git" "HEAD:refs/heads/$new_branch"
url=$(gh pr list --repo "$upstream" --head "$new_branch" --state open --json url --jq '.[0].url // empty')
if [[ -z $url ]]; then
  url=$(gh pr create --repo "$upstream" --base "$base" --head "${fork%%/*}:$new_branch" \
    --title "omacloud $version" \
    --body "Updates the omacloud recipe to $version, with changes beyond pkgver and sha256sums: the recipe now matches https://github.com/$repository/tree/$tag/$recipe.")
fi
echo "Opened $url: the recipe changed beyond pkgver and sha256sums."
echo "Next: follow $url until Omarchy merges it."
