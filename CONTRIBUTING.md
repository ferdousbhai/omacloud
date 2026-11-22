# Contributing

The layout is in the README, the design in [docs/design.md](docs/design.md),
and the work ahead in the issues.

## Before a commit

`bin/check` runs everything below and the end to end and packaging
checks; the pre-push hook runs it (`git config core.hooksPath .githooks`
once per clone).

```sh
cargo fmt --all                                        # CI checks it
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
```

The GTK app needs `gtk4` (CI installs `libgtk-4-dev`); cargo fetches
[omui](https://github.com/ferdousbhai/omui), its look and widgets, at the
revision pinned in `crates/omacloud-app/Cargo.toml`. After changing `server/`, run `npm run check` there
(types and unit tests). Deploy with `server/deploy.sh`.

## Storage backend features

Builds include two OpenDAL services, S3 for buckets and Dropbox (patched, see
`vendor/opendal-service-dropbox/OMACLOUD.md`); the coordinator's tests add
its filesystem service as a dev-dependency. Local repository paths (development and tests) need none. Other OpenDAL
providers, REST and rclone are left out, to keep their protocol stacks out
of the build; the manifest-only patch that allows it is documented in
`vendor/rustic_backend/OMACLOUD.md`.

## Dropbox in development

`scripts/dropbox-sync.sh` runs an account on a real Dropbox, in a test folder of the
app's. It needs someone at a browser to allow Omacloud, so it's not in `bin/check`.

## Buckets in development

`scripts/s3-sync.sh [path/to/omacloud]`, which `bin/check` runs, takes accounts
through everything in a bucket: two computers converging, a rotation, an invite,
a second bucket for records, and a lost computer cut off by a key change. It runs
on `scripts/fake-s3.py`, a stand-in that needs nothing installed and enforces
create-only writes and retired keys, but checks no signatures: it proves how
Omacloud uses S3, not that a provider accepts it. To prove a provider, give it one:

```sh
S3_ENDPOINT=... S3_KEY=... S3_SECRET=... S3_KEY2=... S3_SECRET2=... \
S3_BUCKET=... S3_BUCKET2=... S3_BUCKET3=... S3_BUCKET4=... scripts/s3-sync.sh
```

The buckets must be empty. `crates/omacloud-core/tests/bucket.rs` runs the
coordinator on OpenDAL's filesystem service.

## End to end scripts

- `scripts/s3-sync.sh [path/to/omacloud]` (above) needs only a built CLI and
  python3; with restic on PATH it also restores from the bucket with plain
  restic, using what `export` prints.
- `scripts/restic-compat.sh` needs restic on PATH; a wrapper around the
  `restic/restic` image with paths mounted 1:1 works (name it in `RESTIC`).
  `bin/check` fails without restic, so that a run that passes has restored;
  `NO_RESTIC=1 bin/check` leaves the restore out on a machine that has none.
- `scripts/bar-widget.sh` lints the bar widget (`contrib/omarchy-plugin`) and
  runs it in a Quickshell of its own with no display, on stand-ins for the
  Omarchy shell's types (`scripts/bar-widget/`), handing it lines as the daemon
  writes them. It needs quickshell and Qt 6's qmllint.
- Where `/tmp` is small, put `CARGO_TARGET_DIR` under `~/.cache`.

## The app without touching your desktop

`crates/omacloud-app/dev/preview.sh <directory> [theme-dir]` runs the app in a
nested Hyprland with a private home and session bus, on the made-up account
`dev/fake-omacloud` answers with, and writes pictures of it: no real account,
storage or setting is read. `OMACLOUD_PREVIEW_STEPS` says what to open, which
keys to press and when to take a picture (the script's head lists the steps);
`<directory>/calls.log` then lists every `omacloud` the app ran.
`crates/omacloud-app/dev/qa.sh <app> <script> [directory]` drives an app built
with `--features qa` through one of `dev/qa/*.jsonl` (omui's scripted QA: widgets
found by role and name, clicks, keys, waits and shots) on the same made-up
account, on a broadway display of its own: no window opens. `bin/check` runs
every script; any GLib warning fails one. Release builds leave the feature off.
The README's picture is `overview.png` as `dev/qa/account.jsonl` shoots it, of
the made-up account, copied to `docs/overview.png`.
Pictures of themes and of the real compositor stay `preview.sh`'s. To run the app
by hand instead, `OMACLOUD_CONFIG` and `OMACLOUD_BIN` point it at a test account
and a local build, and `target/debug/omacloud open devices` opens a page directly
(it runs `target/debug/omacloud-app`, beside it; `OMACLOUD_APP` names another). The setup
wizard's settings switch starts on, and even with a scratch `HOME` it would
upload this computer's Wi-Fi passwords: turn it off before Continue when testing
on real storage.

## Secrets

Secrets never go on a command line, where other processes can read them:
`OMACLOUD_RECOVERY_CODE`, `OMACLOUD_JOIN_CODE` and `OMACLOUD_SECRET_ACCESS_KEY` come
from the environment, or a prompt that doesn't echo.

## Shell hygiene

- Kill test processes by exact name (`pkill -x`) or PID; a `pkill -f`
  pattern can match the shell running it.
- Guard `rm -rf` paths built from variables: `"${dir:?}"/...`.

## The browser and the bar

`scripts/web-files.sh` runs Omacloud Drive's page on a throwaway account: it
builds the reader for WebAssembly (`rustup target add wasm32-unknown-unknown`
once), starts the stand-in S3 with a rule for the page, runs a daemon for the
account, and prints a drive code and a drive link to open. Uploading there
exercises the whole way round: the page, the inbox, the daemon, the next sync,
and the page again. Dropbox has no stand-in: an account made with
`init --provider dropbox --root /some-test-folder` and the same page proves it,
with someone to allow Omacloud in a browser.

The bar widget is `contrib/omarchy-plugin`. `OMACLOUD_BAR_PLUGIN=$PWD/contrib/omarchy-plugin
target/debug/omacloud bar on` puts the checkout's copy in your own bar; it reads
`$XDG_RUNTIME_DIR/omacloud/bar.json`, which only the daemon of the default account
writes, so write a line there by hand to see each state
(`{"state":"syncing","text":"x","at":<now>}`). `omacloud bar off` takes it out again;
then delete `bar-decided` from the data directory, so the packaged daemon shows its own.

## Packaging checks

`python3 scripts/tests/installer.py` checks architecture selection,
repository migration, deferring to Omarchy's `[omarchy]` repository when it
carries omacloud, and repeated installation with isolated files and
commands. `python3 scripts/tests/release.py` checks that each repository
contains only its architecture and that metadata, signing, or database
failures stop publication. It needs `bsdtar` (libarchive-tools on Ubuntu).
`python3 scripts/tests/verify-release.py` exercises the container verification
commands, including wrong runtime/package architectures and embedded builder
paths. `python3 scripts/tests/omarchy-recipe.py` checks that the Omarchy
recipe builds the same package as `pkgbuild/PKGBUILD`, and
`python3 scripts/tests/omarchy-pkgs.py` how a release reaches omarchy-pkgs
(below). `bin/check` runs these checks locally before every push, and GitHub CI
runs `bin/check` on every push, in an Arch container. When a release starts CI,
it also tests on a native ARM runner and builds the packages.

## Releases

From a regular terminal with the existing signing key imported, publish
packages built by a CI run that the script starts on master's commit:

```sh
scripts/release-ci.sh 0.0.14
```

This path needs gh, curl, unzip, GPG, repo-add, bsdtar, and Python; it does not need local
Docker access or a Rust rebuild. It downloads both native packages, checks
their source manifests and metadata, signs and verifies their signatures,
and publishes the repository databases and installer at the tested commit.
The signing key remains local. The Release verification workflow installs
the signed release on native x86_64 and ARM runners; a confirmed defect after installation removes the release and tag.
Infrastructure and installer failures retain them for investigation. Assets remain under dist for inspection.
An existing successful CI run of master's commit can be named with a second argument.
`python3 scripts/tests/release-ci.py` checks provenance, signing failures,
and master changing while the release is being prepared.

Omarchy's `[omarchy]` repository builds Omacloud from the recipe in
`packaging/omarchy-pkgs/omacloud`, which downloads the tag's source tarball;
change it together with `pkgbuild/PKGBUILD`. After publishing,
`scripts/release-ci.sh` runs `scripts/omarchy-pkgs.sh <version>`, which
renders the recipe for the tag (pkgver, sha256 of the tarball) and, while
omacom/omarchy-pkgs#778 is open, pushes it to that pull request's branch
(`omacloud` on ferdousbhai/omarchy-pkgs). Once #778 is merged, Omarchy's
sync-upstream bot bumps pkgver and sha256sums for each tag itself, so the
script opens a pull request only when the recipe changed otherwise. It
prints the next step; a failure only warns, and it can be rerun (with
`--dry-run` to see what it would do).

### Build a package locally for a release

Release packages must be built natively on both x86_64 and aarch64 from the
same committed source, with `Cargo.toml` and `CHANGELOG.md` updated first.
CI's `packages` jobs build both architectures in Arch containers and upload
`omacloud-x86_64` and `omacloud-aarch64` artifacts. Each contains a zstd
package and `source-commit.txt`. Download the other architecture's artifact
from a successful CI run of the exact master commit you are releasing:

```sh
gh run download <run-id> --name omacloud-x86_64 --dir /tmp/omacloud-release-x86_64
```

The release script checks the source manifest when present and rejects a
package from a different commit. Locally, `scripts/build-package.sh
<architecture> <version> [output-directory]` runs the same container build;
it needs Docker and `ARM_VERIFY_IMAGE` for an ARM build. The candidate tag
is created inside the container and does not alter the checkout's tags.

To build the other architecture manually instead:
On the other builder, tag that commit locally as `v<version>` and run
`cd pkgbuild && PKGEXT=.pkg.tar.zst makepkg`; transfer its package to the releasing machine,
outside `dist/`. The release script creates its own tag on a clean `master`
checkout and builds the host package:

```sh
scripts/release.sh <version> /path/to/omacloud-<version>-1-<other-architecture>.pkg.tar.zst
```

The script checks both packages' names, versions and architectures, signs
both using the key pinned in `install.sh`, uses zstd package compression on both builders, and creates separate signed
`omacloud` and `omacloud-aarch64` databases. It publishes these with the
installer and the signing key under both repository names.

Publishing starts the Release verification workflow either way: it installs
each architecture's package in a clean container, checks its package
architecture and runs the CLI. To run `scripts/verify-release.sh` yourself,
set `ARM_VERIFY_IMAGE` to an Arch Linux ARM image with pacman and
archlinuxarm-keyring installed. For example, import the official [generic AArch64 root filesystem](https://archlinuxarm.org/platforms/armv8/generic):

```sh
curl -fL https://ca.us.mirror.archlinuxarm.org/os/ArchLinuxARM-aarch64-latest.tar.gz -o /tmp/omacloud-arch-arm.tar.gz
docker import --platform linux/arm64 /tmp/omacloud-arch-arm.tar.gz omacloud-arch-arm
```

Then use `ARM_VERIFY_IMAGE=omacloud-arch-arm`. Docker needs native execution
or configured emulation for both platforms. Verification pins the installer and repository assets to the release tag.
Only a confirmed defect after installation removes the release and tag;
infrastructure and installer failures retain them for investigation. A
release taken down this way leaves `release.sh`'s local tag: `git tag -d
v<version>` before releasing again.

## Checking a release

From 0.0.4, builds are reproducible: two builds of the same tag give
byte-identical binaries, wherever they run (the PKGBUILD maps the builder's
cargo directory to `/cargo`). Only the package's `.BUILDINFO`, which records the build
directory, and `.MTREE`, which hashes it, differ when the directory does.
Arch's `makerepropkg` rebuilds a package in a clean chroot from its
`.BUILDINFO` and compares; by hand:

```sh
git clone --branch v<version> https://github.com/ferdousbhai/omacloud && cd omacloud/pkgbuild
SOURCE_DATE_EPOCH=$(git log -1 --format=%ct) makepkg --nocheck
# then compare usr/bin/* with the released package's
```
