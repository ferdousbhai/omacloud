# OneCloud

Your files and settings on every Omarchy computer, end to end encrypted.
Desktop, Documents and Pictures sync in place, as iCloud does; Omarchy
settings, package lists and ssh and gpg keys follow; every version is kept.

Everything lives in a storage bucket you rent from a provider such as
Hetzner, Cloudflare R2 or Backblaze B2: you pay the provider for the space
you use, and OneCloud itself charges nothing. Your computers talk to your
bucket and to nothing else, and everything is encrypted before it leaves
them. Underneath it's plain restic, so you can always restore without
OneCloud. [docs/design.md](docs/design.md) explains how it protects your
data.

## Install

On Omarchy:

```sh
curl -fsSL https://github.com/ferdousbhai/onecloud/releases/latest/download/install.sh | sudo bash
```

It adds the signed `[onecloud]` package repository (key fingerprint
`5213 0299 581D AD68 5226 900C A89C A1A6 A1E7 4251`) and installs OneCloud;
from then on `omarchy update` keeps it current. To build from a checkout
instead: `cd pkgbuild && makepkg -si`.

## Set up

Open **OneCloud** from the app launcher (Super + Space).

**Your first computer:** pick your storage provider and follow the two steps
the app shows: create a private bucket, and a key that can read and write
it. Paste the bucket name and both keys, and click Create Account. The app
then shows your **recovery code** once: write it down and keep it offline.
With it you can add a computer when no other is at hand and recover
everything; without it and without a computer, nobody can. [docs/providers.md](docs/providers.md) has the
steps and notes for each storage provider.

**Each other computer:** install OneCloud, then on a computer that already
has it, open Devices and click Show Join Code. Paste the code into the new
computer's app, and approve the new computer by its fingerprint. The join
code holds your bucket's key, so pass it only between your own computers.

From the command line, the same is:

```sh
onecloud init --repo opendal:s3 --opt endpoint=https://fsn1.your-objectstorage.com \
  --opt region=fsn1 --opt bucket=<bucket> --opt access_key_id=<key id>   # asks for the secret key
onecloud join-code                                  # on a computer you have
onecloud init --join-code <code>                    # on the new one (or ONECLOUD_JOIN_CODE)
onecloud devices approve <fingerprint>              # back on the first
```

Secrets never go on the command line: the secret key, recovery code and
join code come from a prompt that doesn't echo, or from
`ONECLOUD_SECRET_ACCESS_KEY`, `ONECLOUD_RECOVERY_CODE` and
`ONECLOUD_JOIN_CODE`. The bucket's key is kept in the desktop keyring, not a
file.

## What syncs

- **Folders:** you choose, as on iCloud: setup shows switches for Desktop,
  Documents, Pictures, Music, Videos and Downloads with their sizes
  (Documents and Pictures start on), and Overview has the same switches
  later, plus Add Folder for any other folder in your home. Switching one
  off on a computer keeps its files there and stops syncing it there only.
  Folders match by what they are, so a German computer's `Dokumente` is
  another's `Documents`. From the terminal: `onecloud folders add Music`,
  `onecloud folders skip Pictures`.
- **Not this file:** anything named like `name.nosync` stays on its
  computer, as on iCloud.
- **Omarchy settings:** the shared files in Omarchy's dots manifest
  (bindings, look and feel, terminals, `.bashrc`), never machine-local ones
  like `monitors.lua`. Edits on two machines merge; when they can't, the app
  asks which to keep.
- **Packages:** each computer's package list, so `onecloud packages restore
  --from laptop` installs what the laptop has and this one doesn't.
- **Keys:** `onecloud secrets save` (or Keys in the app) seals `~/.ssh`,
  `~/.gnupg` and token files to your recovery code. They travel with the
  rest, but only the recovery code opens them.

A fresh machine catches up in one go:

```sh
onecloud restore-machine --from laptop    # settings, files, laptop's packages
onecloud secrets restore --from laptop    # ssh and gpg keys: asks for the recovery code
```

## Versions and leaving

Every change is kept: `onecloud versions notes.md` lists a file's versions,
and `onecloud restore notes.md --at 3` brings one back. `onecloud export`
prints the repository location and password, so plain restic or rustic
restores everything without OneCloud; `export --to <dir>` copies it
somewhere of your own first.

## A lost computer

Remove it under Devices (or `onecloud devices revoke <fingerprint>`). It
can't sync anymore, but it still holds your bucket's key, so the app goes on
to change it: make a new key at your provider and enter it. Every other
computer switches on its next sync, Devices shows when all have, and then
you delete the old key at your provider. A computer that was away through
the whole change catches up with `onecloud bucket set-key --access-key-id
<id> --here-only`. `onecloud rotate` also moves everything to a new
encryption key.

Losing the recovery code with every computer locks you out, so `onecloud
recovery split --threshold 3 --shares 5` (or Recovery in the app) can split
it among people you trust; any three of them rebuild it.

With delete protection on the bucket (object lock), sync works as usual and
old data is removed once its retention ends. Keep the bucket without object
lock, or give OneCloud a second, plain bucket for keeping your computers in
step (`--coordination-bucket`, or the switch in the app): some providers
can't do the create-only writes it needs on a locked bucket.

## Configuration

```toml
# ~/.config/onecloud/config.toml (written by `onecloud init`)
[limits]
bandwidth = "5MiB"            # per second; see below
connections = 4               # parallel requests to the bucket
pause_on_battery_below = 20   # percent while discharging; 0 never pauses
```

`notifications = false` turns off the desktop notifications (a computer
asking to join, a setting that needs a choice, this computer removed).

Ignore rules use gitignore syntax, from three places: built-in defaults
(editor swap files, desktop junk), `~/.config/onecloud/ignore` for this
computer only, and `.onecloudignore` at the folder root, which syncs so every
computer follows it. Ignored paths are neither pushed nor pulled.

The daemon runs at nice 10 with idle I/O priority. The bandwidth cap is
opendal's throttle with a 256 MiB burst (a single pack upload must fit in the
burst), so it bites on large transfers such as a first sync, not on everyday
edits.

## Layout

| path | what |
|---|---|
| `crates/onecloud-core` | sync engine (`sync.rs`), signed heads (`head.rs`), device chain (`devices.rs`, `account.rs`), repository keys and rotation (`epoch.rs`), coordination in your bucket (`bucket.rs`, `bucket_key.rs`) |
| `crates/onecloud` | CLI and `watch` daemon |
| `crates/onecloud-app` | the OneCloud app (GTK4, libadwaita), driving the CLI |
| `vendor/rustic_core` | rustic_core 0.13 plus pack padding and `splice_tree` (upstream PRs in `spike/upstream`) |
| `scripts/` | end to end checks (see CONTRIBUTING.md) and the release scripts |
| `contrib/` | the systemd user unit and the desktop entry |
| `pkgbuild/PKGBUILD` | Arch package, built from the committed checkout |
| `install.sh` | the installer each release ships |

[CONTRIBUTING.md](CONTRIBUTING.md) covers building, testing and releases;
the issues hold the work ahead.
