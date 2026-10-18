# Omacloud

Your files and settings on every Omarchy computer, end to end encrypted.
Desktop, Documents and Pictures sync in place, as iCloud does; Omarchy
settings, package lists and ssh and gpg keys follow; every version is kept.

Everything is encrypted on your computer before it leaves it: the storage
only ever holds what it can't read. Underneath it's plain restic, so you can
always restore without Omacloud. [docs/design.md](docs/design.md) explains
how it protects your data.

**Between storages.** Omacloud is moving to one storage of its own, the
Omacloud network: encrypted objects held by other Omarchy computers, with no
accounts and no sign-in. It is being built. The earlier storage choices are
gone (see [CHANGELOG.md](CHANGELOG.md)), so for now only the development
setup below works, with the files and the records that keep computers in
step in directories you share yourself.

## Install

On Omarchy (x86_64 or aarch64):

```sh
curl -fsSL https://github.com/ferdousbhai/omacloud/releases/latest/download/install.sh | sudo bash
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

Until the Omacloud network is here, `omacloud init` on its own says so
(`unsupported`), and so do `omacloud join-code` and `init --join-code`. For
development, give it a directory for the restic repository and one for the
records that keep computers in step; both can be on a shared or synced
filesystem:

```sh
omacloud init --repo <path> --coordinator <directory>   # first computer: shows the recovery code
omacloud init --repo <path> --coordinator <directory>   # another computer, same two: asks to join
omacloud devices approve <fingerprint>                  # on the first, after comparing fingerprints
```

The first computer shows your **recovery code** once: write it down and
keep it offline. With it you can add a computer when no other is at hand
(`OMACLOUD_RECOVERY_CODE` set when joining) and recover everything;
without it and without a computer, nobody can.

Secrets never go on the command line: the recovery code (and, with the
network, the join code) come from a prompt that doesn't echo, or from
`OMACLOUD_RECOVERY_CODE` and `OMACLOUD_JOIN_CODE`.

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

Remove it under Devices (or `omacloud devices revoke <fingerprint>`, which
asks first; `--yes` where nobody can be asked). It can't sync anymore, but
it still holds the repository key it had: `omacloud rotate --yes` (or
`devices revoke --rotate`) moves everything to a new encryption key that
only the remaining computers get, so it can't read anything written after.

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
`--ids-only` one id per line (device fingerprints, version numbers, folder,
package and secrets device names) and `--count` the number of results; given
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
| 2 | invalid usage or request: a destructive command (`devices revoke`, `rotate`, `restore` over a file) without `--yes` where nobody can be asked, a secret asked for where no terminal shows it, an ambiguous fingerprint, already set up |
| 3 | not set up, not approved, settings sync off, or a key, recovery code, join code or shares refused |
| 4 | not found: a device, a version, a package list, a secrets bundle |
| 5 | out of reach: the storage, the network, or (for `restore-machine`, alongside its result) systemd |

Commands only ask (a secret, a confirmation) in text mode with stdin and
stdout on a terminal; elsewhere secrets come from the `OMACLOUD_*`
environment variables and confirmations from `--yes`.

What prints a secret prints it only to your terminal, in text mode with
stdin and stdout on it, and refuses (exit 2) as JSON or into a pipe: `init`
when it creates an account (the recovery code), `join-code`, `export`, and
`recovery split` and `combine`
(`combine` reads its shares from stdin, so only its stdout must be a
terminal). Joining with a join code or the recovery code prints no secret
and works anywhere. This guards against accidents, such as a secret landing
in a script's or an agent's captured output; it is no wall against a program
that gives a command a whole terminal (see docs/design.md).

`omacloud watch`, which the systemd unit runs, keeps its own exit codes: 2
when this computer was removed or the account can't be verified (the unit
doesn't restart into that), 75 after an upgrade (it restarts into the new
binary), 1 for anything else.

## Configuration

```toml
# ~/.config/omacloud/config.toml (written by `omacloud init`)
[limits]
bandwidth = "5MiB"            # per second; see below
connections = 4               # parallel requests to the storage
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
| `crates/omacloud-core` | sync engine (`sync.rs`), settings (`settings.rs`; Chromium and Wi-Fi as merged documents: `merged.rs`, `chromium/`, `wifi.rs`), signed heads (`head.rs`), device chain (`devices.rs`, `account.rs`), repository keys and rotation (`epoch.rs`), secrets bundle (`secrets.rs`), Shamir split (`shamir.rs`) |
| `crates/omacloud` | CLI and `watch` daemon |
| `crates/omacloud-app` | the Omacloud app (GTK4, libadwaita), driving the CLI |
| `vendor/rustic_core` | rustic_core 0.13 plus pack padding and `splice_tree` (upstream PRs in `vendor/upstream-patches`) |
| `scripts/` | an end to end check against upstream restic (see CONTRIBUTING.md) and the release scripts |
| `server/` | the Cloudflare Worker behind omacloud.computer: the public pages now, the Omacloud network's coordination later (see its README) |
| `contrib/` | the systemd user unit and the desktop entry |
| `pkgbuild/PKGBUILD` | Arch package, built from the committed checkout |
| `install.sh` | the installer each release ships |

[CONTRIBUTING.md](CONTRIBUTING.md) covers building, testing and releases;
the issues hold the work ahead.

## Optional Recall search

The Sync page offers **Open Recall** for semantic file search. Recall searches only roots selected in its own settings; Omacloud does not select or index roots automatically. EmbeddingGemma GGUF weights and partial downloads are excluded from sync by default; explicit ignore-file overrides remain possible.

Recall is an optional extra. Normal operation never contacts it. Missing, incompatible, busy, or warming-up Recall leaves opening and browsing a sync folder available. Requests run in the background with a bounded wait. This project does not install Recall, download embedding weights, or start its own model runtime. When enabled, the owner-local Recall service owns one shared model cache and inference worker; records stay on this machine.
