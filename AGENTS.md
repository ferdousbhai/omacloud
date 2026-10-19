# Omacloud

End-to-end encrypted sync of files, Omarchy settings, package lists and keys
across Omarchy computers, on plain restic. User docs: README.md; design and
threat model: docs/design.md; build, test and release: CONTRIBUTING.md.

## Layout

| path | what |
|---|---|
| `crates/omacloud-core` | sync engine (`sync.rs`), settings sync (`settings.rs`, `agents.rs`, `merged.rs`, `chromium/`, `wifi.rs`), signed heads (`head.rs`), the on-demand view of a head (`browse.rs`), device chain (`devices.rs`, `account.rs`), repository keys and rotation (`epoch.rs`), coordination (`Coordinator`, and `DirCoordinator` for development and tests, in `head.rs`), secrets bundle (`secrets.rs`), Shamir split (`shamir.rs`). Integration tests in `tests/` |
| `crates/omacloud-net` | the Omacloud network: `blob.rs` per-holder scrambling, `erasure.rs` 4+2 shards for large objects, `place.rs` holder choice, `credit.rs` blind tokens, `node.rs` a storage node, `proto.rs` its protocol, `client.rs` talking to holders, `seal.rs` epoch keys and sealed index entries, `registry.rs` directory and index, `bucket.rs` the bucket over holders, `gateway.rs` S3 on loopback, `upkeep.rs` audits, renewals, repairs, `coord.rs`/`records.rs` sealed coordination, `worker.rs` the Worker over HTTPS, `dirstore.rs` the Worker's part in a directory, `session.rs` one run's network, `mirror.rs` this computer's copy of what it keeps at the Worker, `share.rs` a file shared by link. Tests in `tests/` |
| `crates/omacloud` | the `omacloud` CLI and `watch` daemon (`main.rs`: every subcommand; `mount.rs`: the read-only FUSE view `mount` serves; `output.rs`: the shared output contract, vendored from icloud-for-omarchy; `fail.rs`: error codes and exit codes; `net.rs`: the node, heartbeats, tokens and upkeep `watch` runs; `agent_skill.rs`: installs `skills/omacloud` for coding agents; `alerts.rs`, `power.rs`). CLI tests in `tests/cli.rs` |
| `crates/omacloud-app` | GTK4/libadwaita app; it shells out to the CLI with `--json` and reads the envelope's `data` (`status` above all) or `error.code`, so keep that output stable; `init` and `join-code` it runs on a pseudo-terminal of its own |
| `vendor/rustic_core`, `vendor/rustic_backend` | patched rustic_core 0.13 (pack padding, `splice_tree`) and rustic_backend (S3 only), wired in through `[patch.crates-io]`; see their `OMACLOUD.md` |
| `server/` | Cloudflare Worker (TypeScript, `cf` CLI): omacloud.computer's public pages (home, privacy, terms, `/llms.txt`, `/s` for share links, `/install.sh`) and the network's API in `server/src/net/` (spec: docs/network-api.md). D1 schema in `server/migrations/`. See server/README.md |
| `scripts/` | end to end checks (`network.sh`: the network on one machine; `restic-compat.sh`: upstream restic), release scripts, and `tests/*.py` for installer and release tooling |
| `pkgbuild/`, `packaging/omarchy-pkgs/` | Arch PKGBUILD and the Omarchy recipe: change them together |
| `contrib/` | systemd user unit, desktop entry, and `seed-setup.sh` for a seed server (node and relay) |

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
`scripts/network.sh [path/to/omacloud]` (in `bin/check`) runs three seeds and
three computers on a `dir:` network. `scripts/restic-compat.sh` needs restic
(or a docker wrapper); see CONTRIBUTING.md.

## Conventions

- Rust edition 2024, workspace dependencies in the root `Cargo.toml`; no
  new dependencies without need.
- Comments and docs are plain, short prose saying why; match it.
- Every command returns a `Response` (data, summary, text, ids/count) or
  fails with a `CliError` (`fail.rs`: code, exit 1-5, remediation in the
  hint); piped output is the JSON envelope, so scripts parsing text pass
  `--styled`. Commands that print a secret (a recovery code, join code,
  shares, a share link, export's and gateway's password) check `output::to_terminal` (stdin and
  stdout a terminal; `combine` stdout only) first and refuse otherwise;
  tests and scripts run them under a pty (`script`), the app on its own.
  `watch` keeps its exit codes (2 stopped, 75 upgraded): the systemd unit's
  `RestartPreventExitStatus=2` depends on them.
- Secrets never go on a command line: they come from env vars
  (`OMACLOUD_RECOVERY_CODE`, `OMACLOUD_JOIN_CODE`, `OMACLOUD_LINK`) or a no-echo prompt
  (`read_secret` in `crates/omacloud/src/main.rs`), asked only in text mode
  with stdin and stdout on a terminal.
- Worker code is TypeScript with tabs, run directly by node
  (`--experimental-strip-types`), so imports use `.ts` extensions.

## Gotchas

- `OMACLOUD_CONFIG` points the CLI at a test account and `OMACLOUD_BIN`
  points the app at a local build. Don't touch the real `~/.config/omacloud`
  when testing.
- `OMACLOUD_NETWORK` picks the network's Worker (default
  https://omacloud.computer). Test with `OMACLOUD_NETWORK=dir:<path>`, a
  network whose Worker part is a directory, plus seeds from `omacloud node
  run`; never against the real Worker. `init --repo <path> --coordinator
  <dir>` still works without any network.
- Coordination appends must stay create-only (a taken position fails);
  `DirCoordinator` does it with a hard link.
- `server/deploy.sh` applies D1 migrations before deploying; code assumes
  they're applied. Don't deploy or release from an agent session.
- Kill test processes with `pkill -x` or by PID, never `pkill -f`; guard
  `rm -rf "${dir:?}"/...`.
- Builds are reproducible from 0.0.4; release flow is in CONTRIBUTING.md.
