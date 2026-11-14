# Omacloud

End-to-end encrypted sync of files, Omarchy settings, package lists and keys
across Omarchy computers, on plain restic, in each person's own Dropbox or S3 bucket.
User docs: README.md; storage: docs/providers.md; design and threat model:
docs/design.md; build, test and release: CONTRIBUTING.md.

## Layout

| path | what |
|---|---|
| `crates/omacloud-core` | sync engine (`sync.rs`), settings sync (`settings.rs`, `agents.rs`, `merged.rs`, `chromium/`, `wifi.rs`), signed heads (`head.rs`), the on-demand view of a head (`browse.rs`), device chain (`devices.rs`, `account.rs`), repository keys and rotation (`epoch.rs`), coordination (`Coordinator`, and `DirCoordinator` for development and tests, in `head.rs`; `BucketCoordinator` in `bucket.rs`), the bucket's key (`bucket_key.rs`), secrets bundle (`secrets.rs`), Shamir split (`shamir.rs`). Integration tests in `tests/` |
| `crates/omacloud` | the `omacloud` CLI and `watch` daemon (`main.rs`: every subcommand; `dropbox.rs`: allowing Omacloud in Dropbox; `mount.rs`: the read-only FUSE view `mount` serves; `output.rs`: the shared output contract, vendored from icloud-for-omarchy; `fail.rs`: error codes and exit codes; `agent_skill.rs`: installs `skills/omacloud` for coding agents; `alerts.rs`, `bar.rs`, `power.rs`; `uploads.rs`: the daemon taking in what a browser uploaded). CLI tests in `tests/cli.rs` |
| `crates/omacloud-app` | GTK4/libadwaita app; it shells out to the CLI with `--json` and reads the envelope's `data` (`status` above all) or `error.code`, so keep that output stable; `init` and `join-code` it runs on a pseudo-terminal of its own |
| `crates/omacloud-web`, `web/` | web access, a first slice: the page (`web/`) fetches from the person's own bucket with presigned URLs and hands the bytes to `omacloud-web`, compiled to WebAssembly, to decrypt; that crate never does I/O and must build for `wasm32-unknown-unknown`, so no rustic_core there. `scripts/web-files.sh` runs it on a throwaway account. See web/README.md |
| `vendor/rustic_core`, `vendor/rustic_backend`, `vendor/opendal-service-dropbox` | patched rustic_core 0.13 (pack padding, `splice_tree`), rustic_backend (S3 and Dropbox) and OpenDAL's Dropbox service (create-only writes, refresh without an app secret), wired in through `[patch.crates-io]`; see their `OMACLOUD.md` |
| `server/` | Cloudflare Worker (TypeScript, `cf` CLI): omacloud.computer's public pages (home, privacy, terms, `/llms.txt`, `/install.sh`) and Omacloud Drive at `/drive/`, the page in `web/` served as static assets under a strict policy (`server/build-drive.sh` builds it into `server/public/drive`, which isn't committed). No account depends on it. See server/README.md |
| `scripts/` | end to end checks (`s3-sync.sh`: an account in a bucket, on `fake-s3.py`; `s3-choices.sh`: daemons following changing files and choices; `restic-compat.sh`: upstream restic; `dropbox-sync.sh`: a real Dropbox, which needs someone at a browser, so not in `bin/check`), release scripts, and `tests/*.py` for installer and release tooling |
| `pkgbuild/`, `packaging/omarchy-pkgs/` | Arch PKGBUILD and the Omarchy recipe: change them together |
| `contrib/` | systemd user unit, desktop entry, and the Omarchy bar widget (`omarchy-plugin/`, which watches the line the daemon writes through `crates/omacloud/src/bar.rs`) |

## Commands

```sh
bin/check                                # every check below; .githooks/pre-push runs it before a push
git config core.hooksPath .githooks      # once per clone (NO_CHECK=1 skips the hook)
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cd server && npm ci && npm run check     # Worker types + unit tests (node --test)
```

The app crate needs `libgtk-4-dev libadwaita-1-dev` (Arch: `gtk4 libadwaita`);
`cargo build -p omacloud` builds the CLI alone. Packaging checks:
`python3 scripts/tests/<name>.py` (all run by `bin/check`; GitHub CI runs only for a release).
`scripts/s3-sync.sh [path/to/omacloud]` (in `bin/check`) runs accounts through a
stand-in S3; with `S3_*` set it proves a real provider. `scripts/restic-compat.sh` needs restic
(or a docker wrapper); see CONTRIBUTING.md.

## Conventions

- Rust edition 2024, workspace dependencies in the root `Cargo.toml`; no
  new dependencies without need.
- Comments and docs are plain, short prose saying why; match it.
- Every command returns a `Response` (data, summary, text, ids/count) or
  fails with a `CliError` (`fail.rs`: code, exit 1-5, remediation in the
  hint); piped output is the JSON envelope, so scripts parsing text pass
  `--styled`. Commands that print a secret (a recovery code, join code,
  shares, an invite, a drive code or link, export's password) check `output::to_terminal` (stdin and
  stdout a terminal; `combine` stdout only) first and refuse otherwise;
  tests and scripts run them under a pty (`script`), the app on its own.
  `watch` keeps its exit codes (2 stopped, 75 upgraded): the systemd unit's
  `RestartPreventExitStatus=2` depends on them.
- Secrets never go on a command line: they come from env vars
  (`OMACLOUD_RECOVERY_CODE`, `OMACLOUD_JOIN_CODE`, `OMACLOUD_SECRET_ACCESS_KEY`, `OMACLOUD_DROPBOX_REFRESH_TOKEN`, `OMACLOUD_DROPBOX_AUTH_CODE`) or a no-echo prompt
  (`read_secret` in `crates/omacloud/src/main.rs`), asked only in text mode
  with stdin and stdout on a terminal.
- Worker code is TypeScript with tabs, run directly by node
  (`--experimental-strip-types`), so imports use `.ts` extensions.

## Gotchas

- `OMACLOUD_CONFIG` points the CLI at a test account and `OMACLOUD_BIN`
  points the app at a local build. Don't touch the real `~/.config/omacloud`
  when testing.
- A scratch `HOME` doesn't isolate settings sync: Wi-Fi comes from
  NetworkManager, so a test account with settings on uploads this computer's
  saved networks and their keys. Keep settings off on real storage.
- Test with `init --repo <path> --coordinator <dir>`, or `scripts/s3-sync.sh`;
  never against a real bucket of the user's.
- Coordination appends must stay create-only (a taken position fails);
  `DirCoordinator` does it with a hard link, `BucketCoordinator` with
  `If-None-Match: *`. A write that only says where a device stands must not
  move the `changed` marker, or each sync wakes the rest in turn.
- Don't deploy or release from an agent session.
- Kill test processes with `pkill -x` or by PID, never `pkill -f`; guard
  `rm -rf "${dir:?}"/...`.
- Builds are reproducible from 0.0.4; release flow is in CONTRIBUTING.md.
