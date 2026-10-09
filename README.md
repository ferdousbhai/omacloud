# Omacloud

Your files and settings on every Omarchy computer, end to end encrypted.
Desktop, Documents and Pictures sync in place, as iCloud does; Omarchy
settings, package lists and ssh and gpg keys follow; every version is kept.

Sign in with Google and your files are kept on Omacloud storage
([omacloud.computer](https://omacloud.computer), in the EU, by invitation for
now), or keep them in a storage bucket you rent yourself from a provider
such as Hetzner, Cloudflare R2 or Backblaze B2, or use your own Dropbox app.
Either way everything is
encrypted on your computer before it leaves it: the storage only ever holds
what it can't read. Underneath it's plain restic, so you can always restore
without Omacloud. [docs/design.md](docs/design.md) explains how it protects
your data.

## Install

On Omarchy (x86_64 or aarch64):

```sh
curl -fsSL https://github.com/ferdousbhai/omacloud/releases/latest/download/install.sh | sudo bash
```

It adds the signed package repository for your architecture (key fingerprint
`5213 0299 581D AD68 5226 900C A89C A1A6 A1E7 4251`) and installs Omacloud;
from then on `omarchy update` keeps it current.

If Omarchy's own `[omarchy]` repository already carries Omacloud, the
installer installs it from there instead and adds no repository of its own,
removing one an earlier run added. pacman takes a package from the first
repository that has it, and `[omarchy]` comes first, so a second copy of
Omacloud's repository would go unused. New versions then reach you once
Omarchy publishes them, which can be a day or more after a release here.

Releases use separate signed package databases for x86_64 (`[omacloud]`)
and aarch64 (`[omacloud-aarch64]`). The installer selects the native
architecture and, on ARM, removes the incompatible repository left by
older installers. This requires a release containing both architectures;
releases through 0.0.13 contain x86_64 packages only.

To build locally on either architecture, run as your desktop user with
Rust/Cargo and the base-devel tools installed:

```sh
git clone https://github.com/ferdousbhai/omacloud.git
cd omacloud/pkgbuild
makepkg -si
```

Locally built packages must be rebuilt from an updated checkout to receive
new versions until you install the signed repository.

## Set up

Open **Omacloud** from the app launcher (Super + Space).

**Your first computer:** click Sign In with Google to use Omacloud storage,
and you're done but for writing down your recovery code (below). Or, to use
your own storage, pick your provider and follow the steps the app shows. With Hetzner that's one page: click Get a Key, generate
credentials there, and paste both keys back; Omacloud makes its own private
bucket. With R2 or B2, make a bucket and a key just for it, and paste the
bucket name and both keys. Then click Create Account. The app
then shows your **recovery code** once: write it down and keep it offline.
With Dropbox, create an App folder scoped app, enable the four file read and
write scopes, then enter its app key and secret and authorize it from setup.
With it you can add a computer when no other is at hand and recover
everything; without it and without a computer, nobody can. [docs/providers.md](docs/providers.md) has the
steps and notes for each storage provider.

**Each other computer:** install Omacloud, then on a computer that already
has it, open Devices and click Show Join Code. Paste the code into the new
computer's app, and approve the new computer by its fingerprint. The join
code holds your bucket's key, so pass it only between your own computers.

With Omacloud storage, signing in on another computer works too: it asks
to join, and you approve it by its fingerprint as above.

From the command line, the same is:

```sh
omacloud init --hosted                              # Omacloud storage: sign in with Google
omacloud init --repo opendal:s3 --opt endpoint=https://fsn1.your-objectstorage.com \
  --opt region=fsn1 --opt bucket=<bucket> --opt access_key_id=<key id>   # asks for the secret key
omacloud init --repo opendal:dropbox --opt client_id=<app key>  # asks for the app secret and authorization code
omacloud join-code                                  # on a computer you have
omacloud init --join-code <code>                    # on the new one (or OMACLOUD_JOIN_CODE)
omacloud devices approve <fingerprint>              # back on the first
```

Secrets never go on the command line: the secret key, recovery code and
join code come from a prompt that doesn't echo, or from
`OMACLOUD_SECRET_ACCESS_KEY`, `OMACLOUD_DROPBOX_CLIENT_SECRET`,
`OMACLOUD_DROPBOX_AUTH_CODE`, `OMACLOUD_RECOVERY_CODE` and
`OMACLOUD_JOIN_CODE`. Storage credentials are kept in the desktop keyring,
or in a private config file when a keyring is unavailable.

## What syncs

- **Folders:** you choose, as on iCloud: setup shows switches for Desktop,
  Documents, Pictures, Music, Videos and Downloads with their sizes
  (Documents and Pictures start on), and Overview has the same switches
  later, plus Add Folder for any other folder in your home. Switching one
  off on a computer keeps its files there and stops syncing it there only.
  Folders match by what they are, so a German computer's `Dokumente` is
  another's `Documents`. From the terminal: `omacloud folders add Music`,
  `omacloud folders skip Pictures`.
- **Not this file:** anything named like `name.nosync` stays on its
  computer, as on iCloud.
- **Omarchy settings:** the shared files in Omarchy's dots manifest
  (bindings, look and feel, terminals, `.bashrc`), never machine-local ones
  like `monitors.lua`, and your shell's: `.bash_aliases`, `.inputrc`,
  git's settings, fish's `config.fish` and functions, Neovim, mise,
  lazygit, mpv and fastfetch (not `.bash_profile`, `.profile` or fish's
  `conf.d`, where installers add lines for one machine), and herdr's
  config.
- **AI agents:** the settings, instructions (`CLAUDE.md`, `AGENTS.md`),
  skills, hooks and themes of Claude Code, Codex, Gemini, opencode,
  Cursor, pi, Grok and Copilot. Never their sign-ins, sessions, history,
  caches or databases: any credential, auth or token file is refused by
  name. API keys written into these settings files (a provider's
  `apiKey`, an MCP server's env or headers) sync with them, end-to-end
  encrypted; every computer of yours can read them. In skill and hook
  folders only text files up to 2 MiB sync, without `node_modules`,
  `.git` or caches. A file or folder linked in from elsewhere (a
  `CLAUDE.md` pointing at your notes, say) is left as it is, and
  `omacloud settings` lists it. Codex's trusted projects and pi's version
  note stay with each computer while the rest of those files syncs. Edits on
  two machines merge; when they can't, the app asks which to keep.
- **Chromium:** bookmarks, search engines, browser settings and the
  extensions you added from the Web Store, for each profile (matched by
  name). Edits on two computers merge; the same bookmark or setting
  changed on both keeps one version, and every computer shows which for a
  week. Changes from another computer are written in once Chromium is
  closed. An extension from
  another computer installs at Chromium's next start; for profiles other
  than the first, the app offers it to install. History, passwords,
  cookies, open tabs and extensions' own data stay on each computer, as do
  the home page, startup pages and default search engine, which Chromium
  guards against change.
- **Wi-Fi:** saved networks and their keys, through NetworkManager, without
  root. A network saved with different keys on two computers keeps each
  one's key, until the key is changed on one of them: that new key then
  goes to all. Enterprise (802.1X) and WEP networks stay put, and with iwd
  Wi-Fi doesn't sync. Like everything else, the keys are encrypted before they
  leave the computer, and every computer of yours can read them.

`omacloud settings` (and the app's Omarchy page) shows each of these and
how it stands: in sync, waiting for Chromium to close, extensions to
install, edits settled lately, a change to choose, or not available here
and why.
- **Packages:** each computer's package list, so `omacloud packages restore
  --from laptop` installs what the laptop has and this one doesn't.
- **Keys:** `omacloud secrets save` (or Keys in the app) seals `~/.ssh`,
  `~/.gnupg` and token files to your recovery code. They travel with the
  rest, but only the recovery code opens them.

A fresh machine catches up in one go:

```sh
omacloud restore-machine --from laptop    # settings, files, laptop's packages
omacloud secrets restore --from laptop    # ssh and gpg keys: asks for the recovery code
```

## Versions and leaving

Every change is kept: `omacloud versions notes.md` lists a file's versions,
and `omacloud restore notes.md --at 3` brings one back. `omacloud export`
prints the repository location and password, so plain restic or rustic
restores everything without Omacloud; `export --to <dir>` copies it
somewhere of your own first.

## A lost computer

Remove it under Devices (or `omacloud devices revoke <fingerprint>`). It
can't sync anymore, but it still holds a key to your storage, so the app
goes on to change that key.

With Dropbox, authorize your app again when the device is removed. The other
computers switch to the new token; Omacloud then revokes the old one. Keep
the computer that started the change available until `omacloud bucket status`
shows the old token is revoked.

With Omacloud storage that takes no more: your other computers switch to a
new key Omacloud makes, and once all have, the old keys are retired and the
removed computer's key no longer reaches your storage (`omacloud bucket
set-key` from the terminal). Signing in with Google still gets any computer
a new key, so anyone who can sign in to your Google account (on the lost
computer, say) can reach your storage again: sign it out of Google too.
Whatever is deleted or replaced on Omacloud storage, by a removed computer
or anyone else, can be undone for 14 days: ask at
privacy@omacloud.computer, saying when it started. Everything deleted or
replaced since then is put back; files added since stay (they do no harm),
and a computer already approved may show on Devices as asking to join
again (don't approve one you removed). Once as much as your quota
has been deleted in 14 days, more deleting and replacing is refused (a sync
says `TooMuchDeleted`) until some of it expires.
With your own bucket, make a new key at your provider and enter it; every
other computer switches on its next sync, Devices shows when all have, and
then you delete the old key at your provider.

A computer that was away through the whole change catches up with
`omacloud bucket set-key --access-key-id <id> --here-only`, or, with
Omacloud storage, `omacloud bucket set-key --here-only`, which signs in
again. `omacloud rotate` also moves everything to a new encryption key.

With your own bucket and delete protection on it (object lock), sync works
as usual and old data is removed once its retention ends. Keep the bucket
without object lock, or give Omacloud a second, plain bucket for keeping
your computers in step (`--coordination-bucket`, or the switch in the app):
some providers can't do the create-only writes it needs on a locked
bucket.

## A trusted contact

Losing the recovery code with every computer locks you out. With Omacloud
storage, someone you trust can keep a recovery card that gets you back in:
under Recovery in the app, enter your recovery code and click Create
Recovery Card (or `omacloud recovery contact`), sign in with Google, then
print the card and give it to them. The card works only together with your Google sign-in,
so on its own it opens nothing, and Omacloud can't use its half without
the card. Making a new card replaces the last one, and Remove Contact
(`--remove`) voids it.

To recover, on a new computer enter the card in setup, beside Sign In with
Google (or `omacloud init --hosted --contact-card`). The computer joins
your account and shows your recovery code again.

With storage of your own, keep a copy of the recovery code with someone you
trust, or split it among several: `omacloud recovery split --threshold 3
--shares 5` makes five shares, any three of which rebuild it (`omacloud
recovery combine`).

## Configuration

```toml
# ~/.config/omacloud/config.toml (written by `omacloud init`)
[limits]
bandwidth = "5MiB"            # per second; see below
connections = 4               # parallel requests to the bucket
pause_on_battery_below = 20   # percent while discharging; 0 never pauses
```

`notifications = false` turns off the desktop notifications (a computer
asking to join, a setting that needs a choice, this computer removed).

Ignore rules use gitignore syntax, from three places: built-in defaults
(editor swap files, desktop junk), `~/.config/omacloud/ignore` for this
computer only, and `.omacloudignore` at the folder root, which syncs so every
computer follows it. Ignored paths are neither pushed nor pulled.

The daemon runs at nice 10 with idle I/O priority. The bandwidth cap is
opendal's throttle with a 256 MiB burst (a single pack upload must fit in the
burst), so it bites on large transfers such as a first sync, not on everyday
edits.

## Layout

| path | what |
|---|---|
| `crates/omacloud-core` | sync engine (`sync.rs`), settings (`settings.rs`; Chromium and Wi-Fi as merged documents: `merged.rs`, `chromium/`, `wifi.rs`), signed heads (`head.rs`), device chain (`devices.rs`, `account.rs`), repository keys and rotation (`epoch.rs`), coordination in your bucket (`bucket.rs`, `bucket_key.rs`) |
| `crates/omacloud` | CLI and `watch` daemon |
| `crates/omacloud-app` | the Omacloud app (GTK4, libadwaita), driving the CLI |
| `vendor/rustic_core` | rustic_core 0.13 plus pack padding and `splice_tree` (upstream PRs in `vendor/upstream-patches`) |
| `scripts/` | end to end checks (see CONTRIBUTING.md) and the release scripts |
| `server/` | Omacloud storage: the Cloudflare Worker behind Sign In with Google (see its README) |
| `contrib/` | the systemd user unit and the desktop entry |
| `pkgbuild/PKGBUILD` | Arch package, built from the committed checkout |
| `install.sh` | the installer each release ships |

[CONTRIBUTING.md](CONTRIBUTING.md) covers building, testing and releases;
the issues hold the work ahead.
