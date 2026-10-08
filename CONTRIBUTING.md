# Contributing

The layout is in the README, the design in [docs/design.md](docs/design.md),
and the work ahead in the issues.

## Before a commit

```sh
cargo fmt --all                                        # CI checks it
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The GTK app needs `gtk4` and `libadwaita` (CI installs `libgtk-4-dev
libadwaita-1-dev`). After changing `server/`, run `npm run check` there
(types and unit tests) and `scripts/gateway-sync.sh`; deploy with
`server/deploy.sh`.

## End to end scripts

- `scripts/s3-sync.sh` and `scripts/bucket-key.sh` run against SeaweedFS:
  `docker run -d -p 8333:8333 -v <s3.json>:/etc/s3.json chrislusf/seaweedfs
  server -s3 -s3.config=/etc/s3.json -dir=/data`. Bucket names need at least
  three characters. SeaweedFS supports the create-only writes coordination
  needs (`If-None-Match: *`), and `bucket-key.sh` starts its own container,
  with keys made through its admin shell so one can be deleted mid-test.
- `scripts/gateway-sync.sh` runs Omacloud storage (`server/`) locally with
  wrangler, SeaweedFS behind it (started in docker) and a stand-in for
  Google: computers sync through it, sign in, and can't reach each other's
  accounts. It needs node and python3.
- `scripts/restic-compat.sh` needs restic on PATH; a wrapper around the
  `restic/restic` image with paths mounted 1:1 works.
- Where `/tmp` is small, put `CARGO_TARGET_DIR` and SeaweedFS data under
  `~/.cache` (SeaweedFS preallocates volumes).

## The app without a desktop

Run `gtk4-broadwayd :7`, start `omacloud-app <page>` with
`GDK_BACKEND=broadway BROADWAY_DISPLAY=:7`, and open `http://localhost:8087`
in a browser kept open for a few seconds (the page renders after it
connects). Clicks through Broadway are unreliable; `omacloud-app devices`
opens a page directly. `OMACLOUD_CONFIG` and `OMACLOUD_BIN` point the app at
a test account and a local build.

## Secrets

Secrets never go on a command line, where other processes can read them:
`OMACLOUD_RECOVERY_CODE`, `OMACLOUD_JOIN_CODE`,
`OMACLOUD_SECRET_ACCESS_KEY`, `OMACLOUD_DROPBOX_CLIENT_SECRET` and
`OMACLOUD_DROPBOX_AUTH_CODE` come from the environment, or a prompt that
doesn't echo. A computer's bucket key lives in the desktop
keyring; scripts that make test accounts clear their keyring entries on exit
(`OMACLOUD_KEYRING=0` keeps keys in the config file instead).

## Shell hygiene

- Kill test processes by exact name (`pkill -x`) or PID; a `pkill -f`
  pattern can match the shell running it.
- Guard `rm -rf` paths built from variables: `"${dir:?}"/...`.

## Packaging checks

`python3 scripts/tests/installer.py` checks architecture selection,
repository migration, and repeated installation with isolated files and
commands. `python3 scripts/tests/release.py` checks that each repository
contains only its architecture and that metadata, signing, or database
failures stop publication. It needs `bsdtar` (libarchive-tools on Ubuntu).
`python3 scripts/tests/verify-release.py` exercises the container verification
commands, including wrong runtime/package architectures and embedded builder
paths. CI runs these checks and builds/tests the application on a native
ARM runner.

## Releases

From a regular terminal with the existing signing key imported, publish
packages from the latest successful CI push build of master:

```sh
scripts/release-ci.sh 0.0.14
```

This path needs gh, GPG, repo-add, bsdtar, and Python; it does not need local
Docker access or a Rust rebuild. It downloads both native packages, checks
their source manifests and metadata, signs and verifies their signatures,
and publishes the repository databases and installer at the tested commit.
The signing key remains local. The Release verification workflow installs
the signed release on native x86_64 and ARM runners; if either fails, it
removes the release and tag. Assets remain under dist for inspection.
A specific successful master run can be selected with a second argument.
`python3 scripts/tests/release-ci.py` checks provenance, signing failures,
and master changing while the release is being prepared.

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
ARM_VERIFY_IMAGE=<your-arch-linux-arm-image> \
  scripts/release.sh <version> /path/to/omacloud-<version>-1-<other-architecture>.pkg.tar.zst
```

The script checks both packages' names, versions and architectures, signs
both using the key pinned in `install.sh`, uses zstd package compression on both builders, and creates separate signed
`omacloud` and `omacloud-aarch64` databases. It publishes these with the
installer and the signing key under both repository names.

Verification installs each architecture's package in a clean container,
checks its package architecture and runs the CLI. Set `ARM_VERIFY_IMAGE` to
an Arch Linux ARM image with pacman and archlinuxarm-keyring installed.
For example, import the official [generic AArch64 root filesystem](https://archlinuxarm.org/platforms/armv8/generic):

```sh
curl -fL https://ca.us.mirror.archlinuxarm.org/os/ArchLinuxARM-aarch64-latest.tar.gz -o /tmp/omacloud-arch-arm.tar.gz
docker import --platform linux/arm64 /tmp/omacloud-arch-arm.tar.gz omacloud-arch-arm
```

Then use `ARM_VERIFY_IMAGE=omacloud-arch-arm`. Docker needs native execution
or configured emulation for both platforms. If either installation fails,
the release and tag are taken back down.

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
