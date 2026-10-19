---
name: omacloud
description: Use Omacloud through Bash for encrypted file/settings sync, device status, file versions and restores, and machine migration; install or set it up when requested. Use for Omacloud tasks, not general cloud documents or calendars.
---

# Omacloud

Run `omacloud` through the harness's Bash/shell tool. It syncs files and Omarchy
settings across the owner's computers using end-to-end encrypted restic storage.
[Source and full docs](https://github.com/ferdousbhai/omacloud).

## Install and inspect

Check `command -v omacloud`. If missing and needed for the requested task,
explain that the official installer installs the package from Omarchy's repo
when available, otherwise adds Omacloud's signed repo, then install within that
scope (Omarchy x86_64/aarch64):

```bash
curl -fsSL https://omacloud.computer/install.sh | sudo bash
```

If sudo needs a password, have the owner run it in a terminal. Installation does
not authorize joining an account, syncing files, or restoring settings.

```bash
omacloud status          # data.set_up false: not set up yet
omacloud folders list
omacloud devices list
omacloud settings status
```

Piped (as in agent tools), every command prints a JSON envelope
`{"ok":true,"data":...,"summary":"..."}`; `--quiet` prints only `data`,
`--ids-only` ids, `--count` a number, `--styled` text. Failures are
`{"ok":false,"error":{"code","message","hint"?}}` on stdout: follow the hint.
Exit codes: 1 other failure, 2 usage (incl. a missing `--yes` or a secret
command off a terminal), 3 not set up/approved or a refused key/code, 4 not
found, 5 storage or network out of reach. Inspection may contact the
repository; report errors and offline state instead of implying everything is
current. Consult `omacloud <command> --help`.

## Setup (only when requested)

Storage is the Omacloud network: other Omarchy computers hold the encrypted
objects. Agree on device name, folders, and whether to sync settings first.

- New account: the owner runs `omacloud init` in their own terminal; it shows
  the recovery code once.
- Another computer: the owner runs `omacloud join-code` on a computer already
  in; the new one runs `omacloud init` with the code in `OMACLOUD_JOIN_CODE`
  and asks to join; the owner approves with `omacloud devices approve
  <fingerprint>` after comparing fingerprints. With `OMACLOUD_RECOVERY_CODE`
  instead, `init` gets in without approval.
- `OMACLOUD_NETWORK` picks the network (default omacloud.computer);
  `OMACLOUD_NETWORK=dir:<path>` keeps a whole network in a directory, and
  `init --repo <path> --coordinator <directory>` uses plain directories:
  both for development and tests only. Point test accounts at
  `OMACLOUD_CONFIG`, never the owner's real `~/.config/omacloud`.

While `omacloud watch` runs, the computer gives disk space to the network
(20G by default). `omacloud node status` shows it; `omacloud node give
<size>` changes it (0 gives none) after a daemon restart, only with the
owner's say. `omacloud node run` runs a node with no account; don't start it
unasked.

The owner must retain the recovery code privately. Commands that print a
secret (`init` creating an account, `join-code`, `export`, `gateway`,
`recovery split|combine`) refuse with `terminal_required`
off a terminal: have the owner run them in their own terminal. Never work
around that refusal (no pseudo-terminals, no `script`), and never run them
yourself, even where your shell has a terminal and nothing refuses. Joining with a join code or recovery code prints none.
Never put recovery codes, join codes, or passwords in chat or shell
arguments. The CLI accepts no-echo prompts or documented `OMACLOUD_*` environment
variables; prefer the owner's terminal for those flows.

A joining device needs owner approval against the fingerprint shown on that
device. Do not approve an unknown device based on its name alone.

## Requested sync and restore

```bash
omacloud sync
omacloud versions 'notes.md'
omacloud deleted                # deleted lately, with the version to restore
omacloud mount                  # every file read-only in ~/Omacloud, fetched on open; runs until Ctrl-C
omacloud share 'photo.jpg'      # a link that opens the file for 7 days (prints to a terminal only; --copy to the clipboard)
omacloud restore 'notes.md' --at 3 --to './notes-restored.md'
```

Sync is bidirectional and can change local and remote files. Do not start
`omacloud watch` or enable its service for an inspection request. Restore to a
separate destination first; `restore` over an existing file, `devices revoke`
and `rotate` need `--yes`, which needs the owner's authority.

Folder controls: `folders add Music`, `folders skip Pictures`, `folders unskip
Pictures`. Adding affects the shared folder set; skipping leaves this device's
files in place and stops syncing them here. Explain scope before changing it.

Settings: `settings on|off`, `settings resolve PATH --keep local|remote`.
Changes can affect desktop settings, browser configuration, Wi-Fi, and agent
settings across computers. Existing settings can contain API keys. Do not sync
or settle conflicts without the owner's requested scope; never blindly prefer
remote settings. Follow the applicable desktop configuration skill when editing
Omarchy config directly.

Leaving: `omacloud export --to <folder>` copies the repository to a folder
plain restic reads and prints its password; `omacloud gateway` serves the
network as S3 on loopback until stopped. Both print a secret: the owner runs
them.

Machine migration: `packages restore --from NAME` previews package installation;
`--yes` runs pacman. `restore-machine --from NAME` is broader: settings, files,
packages, and daemon startup. It is not a harmless preview. Secret restoration
and device revocation/key rotation likewise need explicit scope; inspect their
help and the README first. Never run server deploy/release scripts for user setup.
