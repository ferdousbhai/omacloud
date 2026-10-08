#!/usr/bin/env bash
# Sign and publish native CI packages from a regular terminal. The private
# key stays in the local GPG keyring; GitHub verifies installation afterward.
set -euo pipefail
cd "$(dirname "$0")/.."
repository=ferdousbhai/omacloud
version=${1:?Usage: scripts/release-ci.sh <version> [successful-master-run-id]}
[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "Invalid version." >&2; exit 1; }
for command in gh gpg repo-add bsdtar python3; do
  command -v "$command" >/dev/null || { echo "$command is required." >&2; exit 1; }
done
source_commit=$(gh api "repos/$repository/commits/master" --jq .sha)
run=${2:-$(gh run list --repo "$repository" --workflow ci.yml --branch master --event push --commit "$source_commit" --status success --limit 1 --json databaseId --jq '.[0].databaseId // empty')}
[[ $run =~ ^[0-9]+$ ]] || { echo "Wait for CI on master to pass, then run this command again." >&2; exit 1; }
run_info=$(gh run view "$run" --repo "$repository" --json event,headBranch,headSha,status,conclusion --jq '[.event,.headBranch,.headSha,.status,.conclusion] | @tsv')
[[ $run_info == $'push\tmaster\t'"$source_commit"$'\tcompleted\tsuccess' ]] \
  || { echo "The run must be a successful push build of the current master commit." >&2; exit 1; }

mkdir -p dist
staging=$(mktemp -d "$PWD/dist/release-$version.XXXXXX")
assets=$staging/assets
mkdir "$assets"
echo "Preparing release assets in $assets"
for path in Cargo.toml CHANGELOG.md install.sh; do
  gh api "repos/$repository/contents/$path?ref=$source_commit" --jq .content |
    base64 --decode > "$staging/$path"
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
  gh run download "$run" --repo "$repository" --name "omacloud-$architecture" --dir "$artifact"
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
[[ $(gh api "repos/$repository/commits/master" --jq .sha) == "$source_commit" ]] \
  || { echo "Master moved; use its new successful CI build before publishing." >&2; exit 1; }
gh release create "v$version" "$assets/"* --repo "$repository" --target "$source_commit" \
  --title "Omacloud $version" --notes-file "$staging/notes.md" --latest
echo "Published v$version; watch the Release verification workflow."
echo "If installation fails on either architecture, that workflow removes the release and tag."
