# Omacloud

Your files and settings on every Omarchy computer, end to end encrypted.
The folders you choose (Desktop, Documents, Pictures, Music, any other in
home) sync in place, as iCloud does; Omarchy settings, AI agent settings,
Chromium's bookmarks and extensions, Wi-Fi networks, package lists and ssh
and gpg keys follow; earlier versions and deleted files come back.

Everything is encrypted on your computer before it leaves it: the storage
only ever holds what it can't read. Underneath it's plain restic, so you can
always restore without Omacloud. [docs/design.md](docs/design.md) explains
how it protects your data.

Storage is your own: your Dropbox, or an S3 bucket at Cloudflare R2, Hetzner or any
other provider with create-only writes ([docs/providers.md](docs/providers.md)).
You pay your provider, if anything; Omacloud runs no storage and charges
nothing. Your family and friends each get an account of their own, in their own
Dropbox or a bucket you set up for them ([below](#family-and-friends)).

## Install

On Omarchy (x86_64 or aarch64):

```sh
curl -fsSL https://omacloud.computer/install.sh | sudo bash
```

Where Omarchy's own `[omarchy]` package repository carries Omacloud, the
installer installs it from there, as do Install > Service > Omacloud in the
Omarchy menu (where it's listed) and `omarchy pkg add omacloud`. Otherwise
it adds Omacloud's signed package repository for your architecture (key
fingerprint `5213 0299 581D AD68 5226 900C A89C A1A6 A1E7 4251`) and
installs from that. Either way `omarchy update` keeps it current.

If you installed from Omacloud's repository and Omarchy's comes to carry
Omacloud, run the install command again: it switches you over and removes
the repository and Omarchy hook it added. pacman takes a package from the
first repository that has it, and `[omarchy]` comes first, so Omacloud's
own would go unused. New versions then reach you once Omarchy publishes
them, which can be a day or more after a release here.

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

The simplest way is your own Dropbox: on the first computer, click Set Up
With Dropbox in the Omacloud app, or run this and allow Omacloud in your browser:

```sh
omacloud init --provider dropbox
```

Or use a bucket: make a private one and a key that reaches it at your provider
([docs/providers.md](docs/providers.md) has the steps), then:

```sh
omacloud init --provider r2 --account <cloudflare account id> \
  --bucket <name> --access-key-id <id>
omacloud init --provider hetzner --location fsn1 --bucket <name> --access-key-id <id>
omacloud init --provider s3 --endpoint <url> --bucket <name> --access-key-id <id>
```

It asks for the key's secret (or takes it from `OMACLOUD_SECRET_ACCESS_KEY`).
Either way it checks that the storage can hold an account, and shows your **recovery code**
once: write it down and keep it offline. With it you can add a computer when
no other is at hand and recover everything; without it and without a
computer, nobody can.

To add another computer, run `omacloud join-code` on one that's set up (or
the app) and give the code to the new one privately: it holds the
key to your storage, though it can't open your files. On the new computer,
with the code in `OMACLOUD_JOIN_CODE` (or in the app):

```sh
read -rs OMACLOUD_JOIN_CODE && export OMACLOUD_JOIN_CODE
omacloud init                          # asks to join, and shows this computer's fingerprint
omacloud devices approve <fingerprint> # on a computer already in, after comparing fingerprints
```

With no other computer at hand, the recovery code gets a computer in without
approval: `omacloud init` with `--provider` and the rest as above, and the code in
`OMACLOUD_RECOVERY_CODE`.

Secrets never go on the command line: the recovery code, the join code and
the key's secret come from `OMACLOUD_RECOVERY_CODE`, `OMACLOUD_JOIN_CODE` and
`OMACLOUD_SECRET_ACCESS_KEY`, which `read -rs` fills without echo or shell
history.

Without a bucket, `omacloud init --repo <path> --coordinator <directory>`
keeps the repository and the records that keep computers in step in two
directories you share yourself, for development.

## Family and friends

Each person gets an account of their own. On Dropbox they run `omacloud init --provider
dropbox` on their computer and sign in to their own Dropbox, with no invite needed. With
buckets, each has a bucket of their own, and you can do the setting up. Sharing one
account wouldn't do: every computer of an account reads all its files and Wi-Fi keys.
Separate storage also means a computer that's lost or infected can't reach anyone else's backups.

1. At your provider, make a bucket for them and a key limited to it.
2. Run `omacloud invite` with that bucket, as for `init` above. It checks the
   bucket and prints a code; pass it privately.
3. They run `omacloud init` with the code in `OMACLOUD_JOIN_CODE`, or paste it
   into the app. They get a recovery code only they hold.

You pay for their storage, and neither of you can read the other's files. A
recovery code you keep for them is the one thing that would change that, so
let them keep it, or split it (below). Using the same invite again on another
of their computers asks to join their account, as a join code does.

On Hetzner, a key reaches every bucket of its project, so give each person a
project of their own ([docs/providers.md](docs/providers.md)).

## What syncs

- **Folders:** you choose, as on iCloud: setup shows switches for Desktop,
  Documents, Pictures, Music, Videos and Downloads with their sizes
  (Desktop, Documents and Pictures start on), and Overview has the same switches
  later: the folders every Omarchy computer has under Omarchy folders, and
  folders of your own under Omacloud Drive, where Add a folder from your home has
  a switch for every other folder in your home. Make a new folder in your home and a notification offers to
  sync it: click it. Switching one
  off on a computer keeps its files there and stops syncing it there only.
  Folders match by what they are, so a German computer's `Dokumente` is
  another's `Documents`. From the terminal: `omacloud folders add Music` (or its path, `~/Music`),
  `omacloud folders skip Pictures`, `omacloud folders exclude Documents/Notes` for
  a subfolder. Change these at any time: what comes back loses nothing. Skipping
  keeps the cloud's copy; `omacloud folders remove Documents` takes it out, and no
  computer deletes its files.
- **Omacloud Drive, every file, kept here or not:** `omacloud mount` (or Browse
  under Omacloud Drive in the app's Overview) shows all your folders in `~/Omacloud`, read-only,
  including those skipped on this computer; each file is downloaded when
  it's opened, as iCloud's online-only files are. So a small laptop can
  skip Pictures and still open any photo. It needs `fuse3` (Omarchy has
  it); Ctrl-C or `fusermount3 -u ~/Omacloud` unmounts it, and the app
  keeps it running as the `omacloud-mount` user unit.
- **Not this file:** anything named like `name.nosync` stays on its
  computer, as on iCloud.
- **Omarchy settings:** the shared files in Omarchy's dots manifest
  (bindings, look and feel, terminals, `.bashrc`), never what's about the
  machine, like `monitors.lua`, `input.lua` (touchpad and keyboard) or the bar's
  `shell.json`, and your shell's: `.bash_aliases`, `.inputrc`,
  git's settings, fish's `config.fish` and functions, Neovim, mise,
  lazygit, mpv and fastfetch (not `.bash_profile`, `.profile` or fish's
  `conf.d`, where installers add lines for one machine), herdr's
  config, the night light's hours and Omarchy's About and screensaver text.
- **Web apps:** the ones you installed through Omarchy (Install > Web App), with
  their icons: added or removed on one computer, they are on the others. Other
  programs' launchers stay.
- **Theme:** the Omarchy theme you chose. Changed on one computer, it's set on
  the others, as choosing it in the menu does; one not installed there waits.
- **AI agents:** what you wrote or chose for them: the settings, instructions
  (`CLAUDE.md`, `AGENTS.md`), skills (theirs and `~/.agents/skills`), commands
  and themes of Claude Code, Codex, Gemini, opencode, pi, Grok and Copilot.
  Never their sign-ins, sessions, history, caches or databases: any credential,
  auth or token file is refused by name. API keys written into these settings
  files (a provider's `apiKey`, an MCP server's env or headers) sync with them,
  end-to-end encrypted; every computer of yours can read them. In skill folders
  only text files up to 2 MiB sync, without `node_modules`, `.git` or caches.
  What another program put there stays with the computer it's installed on:
  a file that says it's managed by a program, a skill a product installed
  (its folder holds a `.managed-by-<program>` file), and hooks that run
  something of that computer. A hook travels when what it runs does: its
  command names no file (`keep-going hook claude`), or only files that sync
  here, like a script of yours in `~/.claude/hooks` (write its path with `~`
  or `$HOME` if your user name differs between computers). One that runs a
  program's install or a checkout stays. That holds for Claude Code, Gemini,
  Antigravity, Codex and Cursor; Grok's and Copilot's hook files stay. A file or folder linked in from elsewhere
  (a `CLAUDE.md` pointing at your notes, say) is left as it is, and `omacloud
  settings` lists it. Codex's trusted projects and pi's version note and device
  id stay with each computer while the rest of those files syncs. Edits on two machines
  merge; when they can't, the app asks which to keep.
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
- **Packages:** each computer's package list, so `omacloud packages restore
  --from laptop` installs what the laptop has and this one doesn't.
- **Keys:** `omacloud secrets save` (or Keys in the app) seals `~/.ssh`,
  `~/.gnupg` and token files to your recovery code. They travel with the
  rest, but only the recovery code opens them.

`omacloud settings` (and the app's Omarchy page) shows each of these and
how it stands: in sync, waiting for Chromium to close, extensions to
install, edits settled lately, a change to choose, or not available here
and why.

A fresh machine catches up in one go:

```sh
omacloud restore-machine --from laptop --yes  # settings, files, laptop's packages
omacloud secrets restore --from laptop --yes  # ssh and gpg keys: asks for the recovery code
```

**Computers that aren't the same** can share an account. What's about you
follows you; what's about the machine stays on it. Joining an account gives both
computers the union of their files: one held by only one of them reaches the
other, identical files stay one file, and two different files of the same name
are both kept, the older as `name.sync-conflict-<date>-<time>-<device>.ext`.
Settings that already differ stay as each computer has them until you choose
(`omacloud settings`). From then on a delete on one computer deletes on the others,
and stays in Recently Deleted for 30 days; name a file `name.nosync`, or switch its
folder off, to keep it on one computer.

## Versions and leaving

Earlier versions and deleted files stay for 30 days, the least of what
iCloud, Dropbox and Google Drive keep, which keeps what your account
stores small: `omacloud versions notes.md` lists
a file's versions, and `omacloud restore notes.md --at 3` writes version 3
beside you as `notes.md.v3` (`--to` elsewhere, the file itself included). `omacloud deleted`
lists the files deleted in the last 30 days with the
version to restore, and the app's Recently Deleted, under Overview, puts
one back where it was.

## In a browser

[Omacloud Drive](https://omacloud.computer/drive/) opens your files from any browser: it reads them
from your bucket and decrypts them there, so the site sees neither a file nor a key. `omacloud
drive-code` prints the one-line code you paste to sign in. Keep the code private: it holds the key
to your storage and the password to your files. The bucket needs one rule first
([docs/providers.md](docs/providers.md#reading-in-a-browser)); Dropbox accounts aren't there yet.

Leaving needs nothing from Omacloud: `omacloud export` prints the command that
has plain restic or rustic restore everything from your storage, with the
repository's password (and the bucket's key; Dropbox goes through rclone), and `omacloud export --to <folder>` copies the
repository to a folder first.

## A lost computer

Remove it under Devices and choose It's Lost or Stolen (or `omacloud devices revoke
<fingerprint> --lost`, which asks first; `--yes` where nobody can be asked). It can't
sync anymore, and everything moves to a new encryption key only the remaining computers
get. On Dropbox that's all: Dropbox asks you to allow Omacloud again, and the old token
is revoked by itself once your other computers have switched.

A bucket's key takes two more steps, as the removed computer still holds it:

1. Make a new key at your provider, and run `omacloud bucket set-key
   --access-key-id <id>`. It reaches the other computers sealed to them.
2. When `omacloud bucket status` says every computer switched, delete the old
   key at your provider. The removed computer can't reach the bucket from then
   on.

Until then it could still delete what's there, as any computer with the key can: [docs/providers.md](docs/providers.md#delete-protection)
says how to make that harmless.

## Keeping the recovery code

Losing the recovery code with every computer locks you out. Keep a copy
with someone you trust, or split it among several: `omacloud recovery split
--threshold 3 --shares 5` makes five shares, any three of which rebuild it
(`omacloud recovery combine`).

## Agent skill

[skills/omacloud/SKILL.md](skills/omacloud/SKILL.md) teaches AI agents to install,
set up, and use omacloud. It is built into the CLI:

```sh
omacloud skill install   # ~/.agents/skills/omacloud, linked into ~/.claude/skills
omacloud skill remove
```

A new release refreshes the installed copy automatically; only a copy omacloud
wrote (marked `.managed-by-omacloud`) is ever changed.

## Scripting

Every command prints text on a terminal and a JSON envelope when piped:
`{"ok":true,"data":...,"summary":"..."}`, or
`{"ok":false,"error":{"code":"...","message":"...","hint":"..."}}` on stdout
when it fails (text mode: `error:` and `hint:` lines on stderr). `--json`
forces the envelope, `--styled` text, `--quiet` prints only `data`,
`--ids-only` one id per line (device fingerprints, version numbers, deleted
paths, folder, package and secrets device names) and `--count` the number of results; given
together, ids-only wins over count, count over quiet, quiet over json, json
over styled.

```sh
omacloud status --quiet | jq .synced_head
omacloud devices --ids-only
```

| exit | meaning |
|---|---|
| 0 | done |
| 1 | anything else failed |
| 2 | invalid usage or request: a bucket that can't hold an account, a destructive command (`devices revoke`, `rotate`, `restore` over a file) without `--yes` where nobody can be asked, a secret asked for where no terminal shows it, an ambiguous fingerprint, already set up |
| 3 | not set up (or set up on storage Omacloud no longer uses), not approved, settings sync off, or a bucket key, recovery code, join code or shares refused |
| 4 | not found: a device, a version, a package list, a secrets bundle |
| 5 | out of reach: the storage, the network, or (for `restore-machine`, alongside its result) systemd |

Commands only ask (a secret, a confirmation) in text mode with stdin and
stdout on a terminal; elsewhere secrets come from the `OMACLOUD_*`
environment variables and confirmations
from `--yes`.

What prints a secret prints it only to your terminal, in text mode with
stdin and stdout on it, and refuses (exit 2) as JSON or into a pipe: `init`
when it creates an account (the recovery code), `join-code`, `invite`, `drive-code`, `export`,
and `recovery split` and `combine`
(`combine` reads its shares from stdin, so only its stdout must be a
terminal). Joining with a join code or the recovery code prints no secret
and works anywhere. This guards against accidents, such as a secret landing
in a script's or an agent's captured output; it is no wall against a program
that gives a command a whole terminal (see docs/design.md).

`omacloud watch`, which the systemd unit runs, keeps its own exit codes: 2
when this computer was removed, the account can't be verified, or the
computer was set up on storage Omacloud no longer uses (the unit doesn't
restart into that), 75 after an upgrade, a change to settings sync or a new bucket key (it
restarts into that), 1 for anything else.

## Configuration

```toml
# ~/.config/omacloud/config.toml (written by `omacloud init`, readable by you
# alone: it holds the key to your storage)
[limits]
# bandwidth = "5MiB"          # per second, if you set one; see below
# connections = 4             # parallel requests to the storage, if set
pause_on_battery_below = 20   # percent while discharging; 0 never pauses

[repo]                        # where the files are, with the bucket's key
repository = "opendal:s3"
```

`notifications = false` turns off the desktop notifications (a computer
asking to join, a setting that needs a choice, a new folder in your home to sync,
this computer removed, your storage refusing its key).

Ignore rules use gitignore syntax, from three places: built-in defaults
(editor swap files, desktop junk), `~/.config/omacloud/ignore` for this
computer only, and `.omacloudignore` at the folder root: syncing folders of
home (the default) that's `~/.omacloudignore`, for this computer too;
syncing one whole folder, it syncs, so every computer follows it. Ignored paths are neither pushed nor pulled.

The daemon runs at nice 10 with idle I/O priority. The bandwidth cap
holds on average across all transfers: each one, large or small, waits for
its turn at that rate, and after a quiet spell up to a second's worth goes
out at once. So it bites on large transfers such as a first sync, not on
everyday edits.

## Layout

| path | what |
|---|---|
| `crates/omacloud-core` | sync engine (`sync.rs`), coordination in the bucket and its key (`bucket.rs`, `bucket_key.rs`), settings (`settings.rs`; Chromium and Wi-Fi as merged documents: `merged.rs`, `chromium/`, `wifi.rs`), signed heads (`head.rs`), device chain (`devices.rs`, `account.rs`), repository keys and rotation (`epoch.rs`), secrets bundle (`secrets.rs`), Shamir split (`shamir.rs`) |
| `crates/omacloud` | CLI and `watch` daemon |
| `crates/omacloud-app` | the Omacloud app (GTK4, libadwaita), driving the CLI |
| `vendor/rustic_core` | rustic_core 0.13 plus pack padding and `splice_tree` (upstream PRs in `vendor/upstream-patches`) |
| `scripts/` | end to end checks (an account in a bucket, upstream restic; see CONTRIBUTING.md) and the release scripts |
| `server/` | the Cloudflare Worker behind omacloud.computer: the public pages (see its README) |
| `contrib/` | the systemd user unit and the desktop entry |
| `pkgbuild/PKGBUILD` | Arch package, built from the committed checkout |
| `install.sh` | the installer each release ships |

[CONTRIBUTING.md](CONTRIBUTING.md) covers building, testing and releases;
the issues hold the work ahead.

## Optional Recall search

With [Recall](https://ferdousbhai.com/recall) installed, **Open Recall**
under Overview searches your files by meaning: the folders Recall's own
settings select, which Omacloud doesn't choose for it. Recall's model
weights and partial downloads don't sync. Without Recall nothing changes,
and Omacloud never installs or contacts it otherwise.
