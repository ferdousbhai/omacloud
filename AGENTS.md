# Omacloud

End-to-end encrypted sync of files, Omarchy settings, package lists and keys
across Omarchy computers, on plain restic. User docs: README.md; design and
threat model: docs/design.md; build, test and release: CONTRIBUTING.md.

## Layout

| path | what |
|---|---|
| `crates/omacloud-core` | sync engine (`sync.rs`), settings sync (`settings.rs`, `agents.rs`, `merged.rs`, `chromium/`, `wifi.rs`), signed heads (`head.rs`), device chain (`devices.rs`, `account.rs`), repository keys and rotation (`epoch.rs`), coordination (`Coordinator`, and `DirCoordinator` for development and tests, in `head.rs`), secrets bundle (`secrets.rs`), Shamir split (`shamir.rs`). Integration tests in `tests/` |
| `crates/omacloud` | the `omacloud` CLI and `watch` daemon (`main.rs`: every subcommand; `output.rs`: the shared output contract, vendored from icloud-for-omarchy; `fail.rs`: error codes and exit codes; `alerts.rs`, `power.rs`). CLI tests in `tests/cli.rs` |
| `crates/omacloud-app` | GTK4/libadwaita app; it shells out to the CLI with `--json` and reads the envelope's `data` (`status` above all) or `error.code`, so keep that output stable; `init` and `join-code` it runs on a pseudo-terminal of its own |
| `vendor/rustic_core` | patched rustic_core 0.13 (pack padding, `splice_tree`), wired in through `[patch.crates-io]`; see its `OMACLOUD.md` |
| `server/` | Cloudflare Worker (TypeScript, `cf` CLI): omacloud.computer's public pages (home, privacy, terms, `/llms.txt`); the Omacloud network's API is to come in `server/src/net/`. D1 schema in `server/migrations/`. See server/README.md |
| `scripts/` | the end to end check against upstream restic (`restic-compat.sh`), release scripts, and `tests/*.py` for installer and release tooling |
| `pkgbuild/`, `packaging/omarchy-pkgs/` | Arch PKGBUILD and the Omarchy recipe: change them together |
| `contrib/` | systemd user unit and desktop entry |

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
`scripts/restic-compat.sh` needs restic (or a docker wrapper); see CONTRIBUTING.md.

## Conventions

- Rust edition 2024, workspace dependencies in the root `Cargo.toml`; no
  new dependencies without need.
- Comments and docs are plain, short prose saying why; match it.
- Every command returns a `Response` (data, summary, text, ids/count) or
  fails with a `CliError` (`fail.rs`: code, exit 1-5, remediation in the
  hint); piped output is the JSON envelope, so scripts parsing text pass
  `--styled`. Commands that print a secret (a recovery code, join code,
  shares, export's password) check `output::to_terminal` (stdin and
  stdout a terminal; `combine` stdout only) first and refuse otherwise;
  tests and scripts run them under a pty (`script`), the app on its own.
  `watch` keeps its exit codes (2 stopped, 75 upgraded): the systemd unit's
  `RestartPreventExitStatus=2` depends on them.
- Secrets never go on a command line: they come from env vars
  (`OMACLOUD_RECOVERY_CODE`, `OMACLOUD_JOIN_CODE`) or a no-echo prompt
  (`read_secret` in `crates/omacloud/src/main.rs`), asked only in text mode
  with stdin and stdout on a terminal.
- Worker code is TypeScript with tabs, run directly by node
  (`--experimental-strip-types`), so imports use `.ts` extensions.

## Gotchas

- `OMACLOUD_CONFIG` points the CLI at a test account and `OMACLOUD_BIN`
  points the app at a local build. Don't touch the real `~/.config/omacloud`
  when testing.
- Storage is between options: the Omacloud network is being built (plan in
  docs/design.md), and until then `omacloud init` needs `--repo <path>
  --coordinator <dir>`; plain `init` and `join-code` return `unsupported`.
- Coordination appends must stay create-only (a taken position fails);
  `DirCoordinator` does it with a hard link.
- `server/deploy.sh` applies D1 migrations before deploying; code assumes
  they're applied. Don't deploy or release from an agent session.
- Kill test processes with `pkill -x` or by PID, never `pkill -f`; guard
  `rm -rf "${dir:?}"/...`.
- Builds are reproducible from 0.0.4; release flow is in CONTRIBUTING.md.
