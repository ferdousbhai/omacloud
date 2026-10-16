#!/usr/bin/env bash
# Sign and publish native CI packages from a regular terminal. The private
# key stays in the local GPG keyring; GitHub verifies installation afterward.
set -euo pipefail
cd "$(dirname "$0")/.."
repository=ferdousbhai/omacloud
version=${1:?Usage: scripts/release-ci.sh <version> [successful-ci-run-id]}
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "Invalid version." >&2; exit 1; }
for command in gh gpg repo-add bsdtar python3; do
  command -v "$command" >/dev/null || { echo "$command is required." >&2; exit 1; }
done
# GitHub over a flaky connection: a read that's safe to repeat gets a
# deadline and up to five tries. Publishing itself runs once.
retry() { # seconds command...
  local seconds=$1 attempt
  shift
  for attempt in 1 2 3 4 5; do
    timeout "$seconds" "$@" && return
    ((attempt < 5)) || break
    echo "GitHub didn't answer; trying again ($attempt of 4)" >&2
    sleep "$attempt"
  done
  return 1
}
source_commit=$(retry 60 gh api "repos/$repository/commits/master" --jq .sha)
# CI runs only when a release starts it: build master's commit now unless a run is named.
run=${2:-}
if [[ -z $run ]]; then
  gh workflow run ci.yml --repo "$repository" --ref master
  for _ in $(seq 1 30); do
    run=$(retry 60 gh run list --repo "$repository" --workflow ci.yml --event workflow_dispatch --commit "$source_commit" \
      --limit 1 --json databaseId --jq '.[0].databaseId // empty')
    [[ -n $run ]] && break
    sleep 5
  done
fi
[[ $run =~ ^[0-9]+$ ]] || { echo "No CI run started for master; run this command again." >&2; exit 1; }
echo "Waiting for CI run $run"
gh run watch "$run" --repo "$repository" --exit-status >/dev/null || true
run_info=$(retry 60 gh run view "$run" --repo "$repository" --json event,headBranch,headSha,status,conclusion --jq '[.event,.headBranch,.headSha,.status,.conclusion] | @tsv')
[[ $run_info == $'workflow_dispatch\tmaster\t'"$source_commit"$'\tcompleted\tsuccess' ]] \
  || { echo "The run must be a successful CI build of the current master commit." >&2; exit 1; }

mkdir -p dist
staging=$(mktemp -d "$PWD/dist/release-$version.XXXXXX")
assets=$staging/assets
mkdir "$assets"
echo "Preparing release assets in $assets"
for path in Cargo.toml CHANGELOG.md install.sh; do
  content=$(retry 60 gh api "repos/$repository/contents/$path?ref=$source_commit" --jq .content)
  base64 --decode <<< "$content" > "$staging/$path"
done
grep -qxF "version = \"$version\"" "$staging/Cargo.toml" \
  || { echo "The requested version does not match master." >&2; exit 1; }
grep -q "^## $version\([ (]\|$\)" "$staging/CHANGELOG.md" \
  || { echo "The changelog does not contain $version." >&2; exit 1; }
fingerprint=$(sed -n 's/^SIGNING_KEY_FINGERPRINT=//p' "$staging/install.sh")
[[ $fingerprint =~ ^[0-9A-F]{40}$ ]] || { echo "Invalid pinned fingerprint." >&2; exit 1; }
gpg --batch --list-secret-keys "$fingerprint" >/dev/null 2>&1 \
  || { echo "The pinned signing key is unavailable in the local keyring." >&2; exit 1; }
export GPGKEY=$fingerprint

# Validate both artifacts before signing either one.
for architecture in x86_64 aarch64; do
  artifact=$staging/$architecture
  # a try cut short leaves part of the artifact; each starts from nothing
  retry 600 bash -c 'rm -rf "${1:?}" && exec gh run download "$2" --repo "$3" --name "$4" --dir "$1"' \
    _ "$artifact" "$run" "$repository" "omacloud-$architecture"
  [[ $(cat "$artifact/source-commit.txt") == "$source_commit" ]] \
    || { echo "$architecture was built from a different commit." >&2; exit 1; }
  package="omacloud-$version-1-$architecture.pkg.tar.zst"
  metadata=$(bsdtar -xOf "$artifact/$package" .PKGINFO)
  for field in 'pkgname = omacloud' "pkgver = $version-1" "arch = $architecture"; do
    grep -qxF "$field" <<< "$metadata" || { echo "$package: expected $field" >&2; exit 1; }
  done
  cp "$artifact/$package" "$assets/"
done

for architecture in x86_64 aarch64; do
  package=$assets/omacloud-$version-1-$architecture.pkg.tar.zst
  gpg --local-user "$fingerprint" --detach-sign "$package"
  gpg --verify "$package.sig" "$package"
  database=omacloud
  [[ $architecture == x86_64 ]] || database=omacloud-aarch64
  (cd "$assets" && repo-add --sign --verify "$database.db.tar.gz" "$(basename "$package")")
  # GitHub assets need regular files instead of repo-add's symlinks.
  for name in db files; do
    python3 - "$assets/$database.$name" <<'PY'
from pathlib import Path
import sys
path = Path(sys.argv[1])
for target in (path, Path(str(path) + '.sig')):
    contents = target.read_bytes()
    target.unlink()
    target.write_bytes(contents)
PY
  done
  gpg --batch --armor --export "$fingerprint" > "$assets/$database-signing-key.asc"
done
cp "$staging/install.sh" "$assets/install.sh"
awk -v v="## $version" '$0 ~ "^"v"([ (]|$)" {on=1; next} /^## / && on {exit} on' "$staging/CHANGELOG.md" > "$staging/notes.md"
cat >> "$staging/notes.md" <<'NOTES'

Install on x86_64 or aarch64 Omarchy:

```sh
curl -fsSL https://github.com/ferdousbhai/omacloud/releases/latest/download/install.sh | sudo bash
```
NOTES
# Recheck master after download/signing so a concurrent change cannot be
# mistaken for the tested source. No release exists until this point.
[[ $(retry 60 gh api "repos/$repository/commits/master" --jq .sha) == "$source_commit" ]] \
  || { echo "Master moved; use its new successful CI build before publishing." >&2; exit 1; }
gh release create "v$version" "$assets/"* --repo "$repository" --target "$source_commit" \
  --title "Omacloud $version" --notes-file "$staging/notes.md" --latest
echo "Published v$version; watch the Release verification workflow."
echo "Confirmed package defects remove the release and tag; infrastructure failures retain them."
# Omarchy's repository builds from the tag's tarball; carry the release there.
# The release stands either way, so a failure here only warns.
bash scripts/omarchy-pkgs.sh "$version" \
  || echo "Warning: the Omarchy recipe was not updated; run scripts/omarchy-pkgs.sh $version again." >&2
