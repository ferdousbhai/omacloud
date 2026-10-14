# Omacloud

End-to-end encrypted sync of files, Omarchy settings, package lists and keys
across Omarchy computers, on plain restic. User docs: README.md; design and
threat model: docs/design.md; build, test and release: CONTRIBUTING.md.

## Layout

| path | what |
|---|---|
| `crates/omacloud-core` | sync engine (`sync.rs`), settings sync (`settings.rs`, `agents.rs`, `merged.rs`, `chromium/`, `wifi.rs`), signed heads (`head.rs`), device chain (`devices.rs`, `account.rs`), repository keys and rotation (`epoch.rs`), coordination in the bucket (`bucket.rs`, `bucket_key.rs`), secrets bundle (`secrets.rs`), Shamir split (`shamir.rs`), Dropbox (`dropbox.rs`). Integration tests in `tests/` |
| `crates/omacloud` | the `omacloud` CLI and `watch` daemon (`main.rs`: every subcommand; `hosted.rs`: Google sign-in and Omacloud storage; `keyring.rs`, `alerts.rs`, `power.rs`, `dropbox.rs`) |
| `crates/omacloud-app` | GTK4/libadwaita app; it shells out to the CLI and reads `omacloud status --json`, so keep that output stable |
| `vendor/rustic_core` | patched rustic_core 0.13 (pack padding, `splice_tree`), wired in through `[patch.crates-io]`; see its `OMACLOUD.md` |
| `server/` | Cloudflare Worker (TypeScript, `cf` CLI): omacloud.computer (home, privacy, terms, `/llms.txt`, Google sign-in, waitlist, `/api/*`) and storage.omacloud.computer (S3 gateway, chosen by Host). D1 schema in `server/migrations/`. See server/README.md |
| `scripts/` | end to end checks (`s3-sync.sh`, `bucket-key.sh`, `gateway-sync.sh`, `restic-compat.sh`), release scripts, and `tests/*.py` for installer and release tooling |
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
`python3 scripts/tests/<name>.py` (see `.github/workflows/ci.yml`). End to end
scripts need docker (SeaweedFS) and are described in CONTRIBUTING.md.

## Conventions

- Rust edition 2024, workspace dependencies in the root `Cargo.toml`; no
  new dependencies without need.
- Comments and docs are plain, short prose saying why; match it.
- Secrets never go on a command line: they come from env vars
  (`OMACLOUD_RECOVERY_CODE`, `OMACLOUD_JOIN_CODE`,
  `OMACLOUD_SECRET_ACCESS_KEY`, `OMACLOUD_DROPBOX_CLIENT_SECRET`,
  `OMACLOUD_DROPBOX_AUTH_CODE`, `OMACLOUD_CONTACT_CARD`) or a no-echo prompt
  (`read_secret` in `crates/omacloud/src/main.rs`).
- Worker code is TypeScript with tabs, run directly by node
  (`--experimental-strip-types`), so imports use `.ts` extensions.

## Gotchas

- `OMACLOUD_CONFIG` points the CLI at a test account, `OMACLOUD_BIN` points
  the app at a local build, and `OMACLOUD_KEYRING=0` keeps keys in the config
  file instead of the desktop keyring. Don't touch the real
  `~/.config/omacloud` when testing.
- Coordination needs create-only writes (`If-None-Match: *`); SeaweedFS has
  them, and some providers lack them on object-locked buckets.
- The Worker tells sign-in from storage by Host, so local runs drop the
  custom domains (`--mode test` in `cloudflare.config.ts`;
  `scripts/gateway-sync.sh` writes its own wrangler config).
- `server/deploy.sh` applies D1 migrations before deploying; code assumes
  they're applied. Don't deploy or release from an agent session.
- Kill test processes with `pkill -x` or by PID, never `pkill -f`; guard
  `rm -rf "${dir:?}"/...`.
- Builds are reproducible from 0.0.4; release flow is in CONTRIBUTING.md.
