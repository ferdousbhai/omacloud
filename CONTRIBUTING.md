# Contributing

The layout is in the README, the design in [docs/design.md](docs/design.md),
and the work ahead in the issues.

## Before a commit

```sh
cargo fmt --all                                        # CI checks it
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
(cd service && pnpm check && pnpm test)                # when service/ changed
```

The GTK app needs `gtk4` and `libadwaita` (CI installs `libgtk-4-dev
libadwaita-1-dev`). After changing a signed format in Rust, regenerate the
service's fixtures so `service/src/proto.ts` is checked against it:

```sh
ONECLOUD_FIXTURES=$PWD/service/test/fixtures.json cargo test -p onecloud-core --test fixtures
```

## End to end scripts

- `scripts/s3-sync.sh` and `scripts/bucket-key.sh` run against SeaweedFS:
  `docker run -d -p 8333:8333 -v <s3.json>:/etc/s3.json chrislusf/seaweedfs
  server -s3 -s3.config=/etc/s3.json -dir=/data`. Bucket names need at least
  three characters. SeaweedFS supports the create-only writes coordination
  needs (`If-None-Match: *`), and `bucket-key.sh` starts its own container,
  with keys made through its admin shell so one can be deleted mid-test.
- `scripts/restic-compat.sh` needs restic on PATH; a wrapper around the
  `restic/restic` image with paths mounted 1:1 works.
- `scripts/coordinator-e2e.sh` runs `service/` under `wrangler dev` with
  SeaweedFS as its bucket.
- Where `/tmp` is small, put `CARGO_TARGET_DIR` and SeaweedFS data under
  `~/.cache` (SeaweedFS preallocates volumes).

## The app without a desktop

Run `gtk4-broadwayd :7`, start `onecloud-app <page>` with
`GDK_BACKEND=broadway BROADWAY_DISPLAY=:7`, and open `http://localhost:8087`
in a browser kept open for a few seconds (the page renders after it
connects). Clicks through Broadway are unreliable; `onecloud-app devices`
opens a page directly. `ONECLOUD_CONFIG` and `ONECLOUD_BIN` point the app at
a test account and a local build.

## Secrets

Secrets never go on a command line, where other processes can read them:
`ONECLOUD_RECOVERY_CODE`, `ONECLOUD_JOIN_CODE`, `ONECLOUD_INVITE` and
`ONECLOUD_SECRET_ACCESS_KEY` come from the environment, or a prompt that
doesn't echo. A self-hosted computer's bucket key lives in the desktop
keyring; scripts that make test accounts clear their keyring entries on exit
(`ONECLOUD_KEYRING=0` keeps keys in the config file instead).

## Shell hygiene

- Kill test processes by exact name (`pkill -x`) or PID; a `pkill -f`
  pattern can match the shell running it.
- Guard `rm -rf` paths built from variables: `"${dir:?}"/...`.

## Releases

`scripts/release.sh <version>` (from a clean `master`, with the version in
`Cargo.toml` and `CHANGELOG.md`) tags, builds and signs the package and the
`[onecloud]` repository database, publishes them with `install.sh` as a
GitHub release, then `scripts/verify-release.sh` installs it in a clean Arch
container and takes the release back down if that fails. Signing needs the
package-signing key whose fingerprint `install.sh` pins.
