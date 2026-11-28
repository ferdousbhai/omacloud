# Omacloud

End-to-end encrypted sync of files, Omarchy settings, package lists and keys
across Omarchy computers, on plain restic, in each person's own Dropbox or S3 bucket.
User docs: README.md; storage: docs/providers.md; design and threat model:
docs/design.md; what's in the storage, record by record: docs/format.md;
build, test and release: CONTRIBUTING.md.

## Layout

| path | what |
|---|---|
| `crates/omacloud-core` | sync engine (`sync.rs`), settings sync (`settings.rs`, `agents.rs`, `merged.rs`, `chromium/`, `wifi.rs`), signed heads (`head.rs`), the on-demand view of a head (`browse.rs`), device chain (`devices.rs`, `account.rs`), repository keys and rotation (`epoch.rs`), coordination (`Coordinator`, and `DirCoordinator` for development and tests, in `head.rs`; `BucketCoordinator` in `bucket.rs`), the bucket's key (`bucket_key.rs`), secrets bundle (`secrets.rs`), Shamir split (`shamir.rs`). Integration tests in `tests/` |
| `crates/omacloud` | the `omacloud` CLI and `watch` daemon (`main.rs`: every subcommand; `dropbox.rs`: allowing Omacloud in Dropbox; `mount.rs`: the read-only FUSE view `mount` serves; `output.rs`: the shared output contract, vendored from icloud-for-omarchy; `fail.rs`: error codes and kinds; `agent_skill.rs`: installs `plugin/skills/omacloud` for coding agents; `alerts.rs`, `bar.rs`, `power.rs`; `uploads.rs`: the daemon taking in what a browser uploaded; `cloudflare.rs` and `plan.rs`: where a connector for agents goes, the person's own Cloudflare account or, with a subscription, omacloud.computer). CLI tests in `tests/cli.rs` |
| `crates/omacloud-app` | GTK4 app; its look, icons, dialogs, rows and tabs are [omui](https://github.com/ferdousbhai/omui)'s, a git dependency pinned by revision (no libadwaita: the two cannot share a process). `dev/preview.sh` runs it in a nested Hyprland on `dev/fake-omacloud`'s made-up account, and `dev/qa.sh` drives a build with feature `qa` through `dev/qa/*.jsonl` on that account (in `bin/check`). It shells out to the CLI with `--json` and reads the envelope's `data` (`status` above all) from stdout or `error.code` from stderr's last line, so keep that output stable; `init` and `join-code` it runs on a pseudo-terminal of its own. Every string it shows goes through omui's `tr`/`trn` and is listed in `src/i18n/en.json` (a test checks); the CLI's failures are shown in the window's words by `code` (`Failure::known`), else in the CLI's English. `omacloud open [page]` execs it, installed as `/usr/lib/omacloud/omacloud-app`: the one name, `omacloud`, is the command, the desktop entry, the icon and the window's Wayland app id; the GApplication id stays `com.ferdousbhai.Omacloud` |
| `crates/omacloud-web`, `web/` | Omacloud in a browser: the page (`web/`) fetches from the person's own storage (`s3.js` with presigned URLs, `dropbox.js` through Dropbox's API) and hands the bytes to `omacloud-web`, compiled to WebAssembly, to decrypt, to open a web link (`link.rs`) and to sign and seal an upload (`upload.rs`); that crate never does I/O and must build for `wasm32-unknown-unknown`, so no rustic_core there. What a browser uploads waits in an inbox a computer's daemon takes in (`crates/omacloud-core/src/inbox.rs`). `scripts/web-files.sh` runs it all on a throwaway account. See web/README.md |
| `connector/` | the connector for agents that aren't on a person's computers: a Cloudflare Worker (plain JavaScript modules, no dependencies) that `omacloud agents connect` puts in the person's own Cloudflare account (`crates/omacloud/src/cloudflare.rs`) or, with a subscription, has omacloud.computer run (`server/src/hosting.ts`), compiled into the CLI with `web/`'s `reader.js`, `s3.js` and `dropbox.js`, which it shares. `files.js` reads the account, `mcp.js` is the protocol and the tools, `oauth.js` the sign-in, `worker.js` the entry. Unit tests in `server/test/connector.test.ts`; `scripts/connector.sh` runs it end to end on stand-ins. See docs/cloud-agents.md |
| `vendor/rustic_core`, `vendor/rustic_backend`, `vendor/opendal-service-dropbox` | patched rustic_core 0.13 (pack padding, `splice_tree`), rustic_backend (S3 and Dropbox) and OpenDAL's Dropbox service (create-only writes, refresh without an app secret), wired in through `[patch.crates-io]`; see their `OMACLOUD.md` |
| `server/` | Cloudflare Worker (TypeScript, `cf` CLI): omacloud.computer. It opens on Omacloud in a browser at `/files/`, the page in `web/` served as static assets under a strict policy (`server/build-files.sh` builds it into `server/public/files`, which isn't committed); serves privacy, terms, `/llms.txt` and `/install.sh`; and does two things for accounts that ask (`paid.ts`, in D1): keeps what a passkey sealed, free, so it signs in on any device; and for subscribers (Stripe) runs their connectors for agents through `hosting.ts`. No account depends on it, and it holds no key it can open. See server/README.md |
| `scripts/` | end to end checks (`s3-sync.sh`: an account in a bucket, on `fake-s3.py`; `s3-choices.sh`: daemons following changing files and choices; `restic-compat.sh`: upstream restic; `bar-widget.sh`: the bar widget following the daemon's line; `connector.sh`: the connector for agents, on stand-ins for the storage and for Cloudflare; `dropbox-sync.sh`: a real Dropbox, which needs someone at a browser, so not in `bin/check`), release scripts, and `tests/*.py` for installer and release tooling |
| `pkgbuild/`, `packaging/omarchy-pkgs/` | Arch PKGBUILD and the Omarchy recipe: change them together |
| `contrib/` | systemd user unit, desktop entry and icon (`omacloud.desktop`, `omacloud.svg`), and the Omarchy bar widget (`omarchy-plugin/`, which watches the line the daemon writes through `crates/omacloud/src/bar.rs`; its words are in `omarchy-plugin/i18n/en.json`, the daemon's lines among them, keyed `daemon.<line>`) |

## Commands

```sh
bin/check                                # every check below; .githooks/pre-push runs it before a push
git config core.hooksPath .githooks      # once per clone (NO_CHECK=1 skips the hook)
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cd server && npm ci && npm run check     # Worker types + unit tests (node --test)
```

The app crate needs `libgtk-4-dev` (Arch: `gtk4`);
`cargo build -p omacloud` builds the CLI alone. Packaging checks:
`python3 scripts/tests/<name>.py` (all run by `bin/check`, which GitHub CI runs on every push, on Arch).
`scripts/s3-sync.sh [path/to/omacloud]` (in `bin/check`) runs accounts through a
stand-in S3; with `S3_*` set it proves a real provider. `scripts/restic-compat.sh` needs restic
(or a docker wrapper; `NO_RESTIC=1 bin/check` leaves it out), and `scripts/bar-widget.sh` quickshell and
qmllint; see CONTRIBUTING.md.

## Conventions

- Rust edition 2024, workspace dependencies in the root `Cargo.toml`; no
  new dependencies without need.
- Comments and docs are plain, short prose saying why; match it.
- Every command returns a `Response` (data, summary, text, ids/count) or
  fails with a `CliError` (`fail.rs`: code, kind, remediation in the hint).
  Exit codes are Omarchy's, 0 done and 1 failed; stdout holds only a result,
  and a failure goes to stderr (the error object on one line when piped), so
  the app and scripts read errors there. Piped output is the JSON envelope,
  so scripts parsing text pass `--styled`. Commands that print a secret (a recovery code, join code,
  shares, an invite, a web code or link, export's password, a connector's passphrase) check `output::to_terminal` (stdin and
  stdout a terminal; `combine` stdout only) first and refuse otherwise;
  tests and scripts run them under a pty (`script`), the app on its own.
  `watch` keeps its exit codes (2 stopped, 75 upgraded): the systemd unit's
  `RestartPreventExitStatus=2` depends on them.
- Secrets never go on a command line: they come from env vars
  (`OMACLOUD_RECOVERY_CODE`, `OMACLOUD_JOIN_CODE`, `OMACLOUD_SECRET_ACCESS_KEY`, `OMACLOUD_DROPBOX_REFRESH_TOKEN`, `OMACLOUD_DROPBOX_AUTH_CODE`, `OMACLOUD_CLOUDFLARE_TOKEN`) or a no-echo prompt
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
- Deploy or release only when asked to; follow CONTRIBUTING.md (`scripts/release-ci.sh <version>`).
- Kill test processes with `pkill -x` or by PID, never `pkill -f`; guard
  `rm -rf "${dir:?}"/...`.
- Builds are reproducible from 0.0.4; release flow is in CONTRIBUTING.md.

## Cloudflare CLI

Use the `cf` CLI alone for Cloudflare work in this repository: deploys, builds and
their logs, secrets, D1, R2, Tunnels and network routes. No Wrangler, as a command,
a config file or a dependency. Find commands with
`cf cli search "<what you want to do>"`, then check `<command> --help`.
