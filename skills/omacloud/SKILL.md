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
curl -fsSL https://github.com/ferdousbhai/omacloud/releases/latest/download/install.sh | sudo bash
```

If sudo needs a password, have the owner run it in a terminal. Installation does
not authorize joining an account, syncing files, or restoring settings.

```bash
omacloud status          # data.set_up false: not set up yet
omacloud folders list
omacloud devices list
omacloud settings status
omacloud bucket status
```

Piped (as in agent tools), every command prints a JSON envelope
`{"ok":true,"data":...,"summary":"..."}`; `--quiet` prints only `data`,
`--ids-only` ids, `--count` a number, `--styled` text. Failures are
`{"ok":false,"error":{"code","message","hint"?}}` on stdout: follow the hint.
Exit codes: 1 other failure, 2 usage (incl. a missing `--yes` or a secret
command off a terminal), 3 not set up/approved or a refused key/code, 4 not
found, 5 storage, network or keyring out of reach. Inspection may contact the
repository; report errors and offline state instead of implying everything is
current. Consult `omacloud <command> --help`.

## Setup (only when requested)

Agree on hosted storage versus the owner's bucket/Dropbox, device name, folders,
and whether to sync settings. `omacloud init --hosted` opens Google sign-in for
Omacloud storage; the owner completes it. Hosted storage is invitation-only in
the current docs. Bring-your-own storage needs the provider's documented options;
read the README/provider docs rather than inventing credentials or endpoints.

The owner must retain the recovery code privately. Commands that print a
secret (`init` creating an account or recovering with a card, `join-code`,
`export`, `recovery contact|split|combine`) refuse with `terminal_required`
off a terminal: have the owner run them in their own terminal. Never work
around that refusal (no pseudo-terminals, no `script`), and never run them
merely to inspect status. Joining with a join code or recovery code prints none.
Never put recovery codes, join codes, passwords, or secret keys in chat or shell
arguments. The CLI accepts no-echo prompts or documented `OMACLOUD_*` environment
variables; prefer the owner's terminal for those flows.

A joining device needs owner approval against the fingerprint shown on that
device. Do not approve an unknown device based on its name alone.

## Requested sync and restore

```bash
omacloud sync
omacloud versions 'notes.md'
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

Machine migration: `packages restore --from NAME` previews package installation;
`--yes` runs pacman. `restore-machine --from NAME` is broader: settings, files,
packages, and daemon startup. It is not a harmless preview. Secret restoration
and device revocation/key rotation likewise need explicit scope; inspect their
help and the README first. Never run server deploy/release scripts for user setup.
