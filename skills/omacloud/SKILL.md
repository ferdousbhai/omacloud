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
omacloud status --json
omacloud folders list
omacloud devices list
omacloud settings status
omacloud bucket status
```

Only `status` has `--json`; do not assume a global JSON flag. Consult
`omacloud <command> --help`. Other inspection commands may contact the repository;
report errors and offline state instead of implying everything is current.

## Setup (only when requested)

Agree on hosted storage versus the owner's bucket/Dropbox, device name, folders,
and whether to sync settings. `omacloud init --hosted` opens Google sign-in for
Omacloud storage; the owner completes it. Hosted storage is invitation-only in
the current docs. Bring-your-own storage needs the provider's documented options;
read the README/provider docs rather than inventing credentials or endpoints.

The owner must retain the recovery code privately. Init and recovery operations
can print secrets: have the owner run them in their terminal, not captured agent
tool output. Join codes include the bucket key. Never run `join-code`, `export`,
recovery-card/share creation, or commands printing secrets merely to inspect status.
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
separate destination first; do not overwrite an existing file without authority.

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
