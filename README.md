# OneCloud

Your files and settings on every Omarchy computer, end to end encrypted, in
plain restic format. Desktop, Documents and Pictures sync in place as iCloud
does; settings (through Omarchy's dots manifest), packages and keys follow;
every version is kept. [docs/design.md](docs/design.md) explains how it
protects your data; the issues hold the work ahead.

Version 0.0.1 is self-hosted: everything lives in your own S3 bucket
(Hetzner, R2, B2, MinIO), coordination included, and nobody sits in between.
A hosted service comes with 1.0 (https://onecloud.computer).

How it holds together: every change is a snapshot, and the order of
snapshots is a signed, hash chained list of heads; which devices may sign is
itself a signed chain rooted in the account's recovery code. Whoever
coordinates can't roll back, fork or forge history, or slip in a device of
its own. Repository keys reach devices sealed to their device keys, and
`onecloud rotate` moves everything to a new key so removed devices lose
access.

## Install

From a checkout, on Arch or Omarchy: `cd pkgbuild && makepkg -si`. It
installs `onecloud` and a systemd user unit that keeps the folder in sync
for every user who ran `onecloud init`. A signed package repository will
follow, as for icloud-notes.

`onecloud-app` is the app for all of this: devices and join requests,
settings sync, secrets and recovery shares. The daemon also notifies you
when a device asks to join, a setting needs a choice, or this device was
removed; "Open" goes to the right page (`notifications = false` in the
config turns them off).

## A new machine

```sh
onecloud init --recovery-code <code>      # or: --account <id>, then approve it
onecloud restore-machine --from laptop    # settings, files, laptop's packages
onecloud secrets restore --from laptop    # ssh and gpg keys: asks for the recovery code
```

`init` defaults to the onecloud service with storage through it and the
hostname as the device name. What syncs follows iCloud: your Desktop,
Documents and Pictures folders, in place, on every machine (their names come
from the XDG user dirs, so a German machine's `Dokumente` is another
machine's `Documents`). `onecloud folders add Music` syncs another folder of
home everywhere; `onecloud folders skip Pictures` keeps a machine out of
one. `init --folder <dir>` syncs a single folder instead.

## Try it

```sh
cargo build --release
oc=target/release/onecloud

# device 1: creates the repository and the account, and shows the account
# id, the recovery code and the repository password once
$oc --config /tmp/a.toml init --folder ~/Sync --repo /srv/onecloud/repo \
    --coordinator http://127.0.0.1:8787 --device laptop

# device 2: asks to join the account (no password needed)
$oc --config /tmp/b.toml init --folder ~/Sync2 --repo /srv/onecloud/repo \
    --coordinator http://127.0.0.1:8787 --account <account id> --device desktop

# device 1: compare the fingerprint device 2 printed, then approve it
$oc --config /tmp/a.toml devices
$oc --config /tmp/a.toml devices approve 3f2a-91c0
# (or join device 2 with ONECLOUD_RECOVERY_CODE=... instead of approving)

$oc --config /tmp/a.toml watch      # keep in sync (wakes on other devices' pushes)
$oc --config /tmp/a.toml versions notes.md
$oc --config /tmp/a.toml restore notes.md --at 3
$oc --config /tmp/a.toml devices revoke 3f2a-91c0 --rotate  # new repository key
$oc --config /tmp/a.toml export     # password and command for plain restic
$oc --config /tmp/a.toml export --to ~/onecloud-copy   # copy out, then restore anywhere
```

`--repo onecloud` stores the repository with the service (the managed
tiers): the device never holds bucket credentials and moves data through
presigned URLs. `export --to` copies it somewhere of your own first.

Two ways to run it:

- **Hosted**: storage and coordination by the onecloud service.
- **Self-hosted** (free): your own bucket holds everything, coordination
  included, and nobody sits in between. `init --coordinator bucket` with your bucket's
  `--repo`; other devices join with `onecloud init --join-code <code>`,
  the code from `onecloud join-code` on a device you have (it holds the
  bucket's keys: pass it privately). Devices poll for changes instead of
  being told. To cut off a lost or removed computer, make a new key at your
  provider and run `onecloud bucket set-key --access-key-id <id>`: every
  other computer switches on its next sync, `onecloud bucket status` shows
  when all have, and then you delete the old key there. With object lock on the data bucket, keep coordination in a
  second, plain one (`--coordination-bucket`): Hetzner does create-only
  writes only on buckets without versioning.

Any S3 endpoint works (Hetzner, R2, B2, MinIO): `--repo opendal:s3 --opt
endpoint=... --opt bucket=... --opt access_key_id=...`, and the secret key is
asked for (or comes from `ONECLOUD_SECRET_ACCESS_KEY`, never the command
line); or the setup page of `onecloud-app`. The
bucket's key goes to the desktop keyring, not the config file (without a
keyring, or with `ONECLOUD_KEYRING=0`, the config file keeps it, readable
only by you). With delete
protection on the bucket (object lock, R2 bucket locks), sync works as usual;
old key epochs are removed once their retention ends.

`onecloud settings on` also syncs Omarchy settings: the shared files in
the dots manifest (bindings, look and feel, terminals, `.bashrc` and so
on), never machine-local ones like `monitors.lua`. Each device's package list
travels too: `onecloud packages restore --from laptop` installs what the
laptop has and this machine doesn't. `onecloud secrets save` seals
`~/.ssh`, `~/.gnupg` and token files (or what `~/.config/onecloud/secrets`
lists) to the account's recovery code: they travel with the rest, but no
device can open them, and `secrets restore` asks for the code and shows what
it will write. Edits to the same file
on two machines merge; if they can't, this machine's version stays and
`onecloud settings` shows what to resolve.

The service deploys as `onecloud-coordinator` with 1.0. Until then the only
live one is the pre-rename `omacloud-coordinator`
(`https://omacloud-coordinator.ferdousbd.workers.dev`), on the old formats
and holding test data only.
During the beta, creating an account there needs an invite code
(`--invite` or `ONECLOUD_INVITE`); joining an existing account doesn't.
Share a folder with another onecloud user:

```sh
onecloud share create photos --folder ~/Photos/trip   # prints the id to send
onecloud share join <id> --name trip --folder ~/trip  # they run this, and send you their fingerprint
onecloud share approve photos <fingerprint>           # after comparing it
onecloud share remove photos <fingerprint> --rotate   # later: out, with a new key
```

Each shared folder is its own repository with its own key and members,
synced by the same daemon; removing someone with `--rotate` locks them out
of everything written after.

Losing the recovery code with every device locks you out, so
`onecloud recovery split --threshold 3 --shares 5` can split it among people
you trust; any three of them rebuild it with `onecloud recovery combine`.

`onecloud delete-account --confirm <account id>` deletes the account and
its stored data.

```sh
$oc init --folder ~/Sync --repo onecloud --coordinator https://… --device laptop
```

Without `--config`, config lives in `~/.config/onecloud/config.toml` and
secrets and state in `~/.local/share/onecloud/` (mode 0600). To run the
daemon at login without the package, copy `contrib/onecloud.service` to
`~/.config/systemd/user/` and point `ExecStart` at your binary.

## Configuration

```toml
# ~/.config/onecloud/config.toml (written by `onecloud init`)
[limits]
bandwidth = "5MiB"            # per second, S3 repositories; see below
connections = 4               # parallel requests to S3
pause_on_battery_below = 20   # percent while discharging; 0 never pauses
```

Ignore rules use gitignore syntax, from three places: built-in defaults
(editor swap files, desktop junk), `~/.config/onecloud/ignore` for this
device only, and `.onecloudignore` at the folder root, which syncs so every
device follows it. Ignored paths are neither pushed nor pulled.

The daemon runs at nice 10 with idle I/O priority. The bandwidth cap is
opendal's throttle with a 256 MiB burst (a single pack upload must fit in the
burst), so it bites on large transfers such as a first sync, not on everyday
edits.

## Layout

| path | what |
|---|---|
| `crates/onecloud-core` | sync engine (`sync.rs`), signed heads and coordinator (`head.rs`), device chain (`devices.rs`, `account.rs`), repository keys and rotation (`epoch.rs`) |
| `crates/onecloud` | CLI and `watch` daemon |
| `service/` | the coordinator service and account page (Cloudflare Worker, TypeScript; see `service/README.md`) |
| `vendor/rustic_core` | rustic_core 0.13 plus pack padding and `splice_tree` (upstream PRs in `spike/upstream`) |
| `scripts/restic-compat.sh` | CI: sync two devices, restore with upstream restic, compare |
| `scripts/s3-sync.sh` | CI: two devices through an S3 repository (SeaweedFS in CI) |
| `scripts/bucket-key.sh` | CI: a bucket key change while one computer is offline; it catches up with `--here-only` (runs its own SeaweedFS) |
| `scripts/coordinator-e2e.sh` | the engine and CLI against the coordinator service (`service/` under `wrangler dev`, SeaweedFS as the bucket), managed storage included |
| `contrib/onecloud.service` | systemd user unit for `onecloud watch` |
| `pkgbuild/PKGBUILD` | Arch package, built from the committed checkout |
| `spike/` | phase 0 spikes and measurements |

## Tests

```sh
cargo test --workspace            # head chain and two device engine tests
scripts/restic-compat.sh          # needs restic on PATH (or RESTIC=...)
(cd service && pnpm install && pnpm test)   # the coordinator service in workerd
```

## Not yet

The issues hold what's next: the hosted service and billing (milestone
`1.0`), sharing on self-hosted accounts (#8), online-only files (#7), and an
outside security review (#18).

