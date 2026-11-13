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

Storage is the owner's own Dropbox (the simplest), or an S3 bucket: Cloudflare R2,
Hetzner or any other with create-only writes (https://github.com/ferdousbhai/omacloud/blob/master/docs/providers.md). Agree on device name, folders, and whether
to sync settings first.

- New account: on Dropbox, the owner runs `omacloud init --provider dropbox` in their
  own terminal, allows Omacloud in the browser, and is shown the recovery code once.
  With a bucket, the owner makes a private one and a key at their provider, then
  runs `omacloud init --provider r2 --account <id> --bucket <name> --access-key-id
  <id>` (or `hetzner --location fsn1`, or `s3 --endpoint <url>`) in their own
  terminal; it asks for the secret and shows the recovery code once.
- Another computer: the owner runs `omacloud join-code` on a computer already
  in; the new one runs `omacloud init` with the code in `OMACLOUD_JOIN_CODE`
  and asks to join; the owner approves with `omacloud devices approve
  <fingerprint>` after comparing fingerprints. With `--provider ...` and
  `OMACLOUD_RECOVERY_CODE` instead, `init` gets in without approval.
- Someone else (family, a friend): the owner makes them a bucket and key, and
  runs `omacloud invite` with those details; the person runs `omacloud init` with
  the code in `OMACLOUD_JOIN_CODE`, and gets an account and recovery code of their
  own. Never put two people on one account: its computers read all of it.
- `init --repo <path> --coordinator <directory>` uses plain directories, for
  development and tests only. Point test accounts at `OMACLOUD_CONFIG`, never
  the owner's real `~/.config/omacloud`.

Removing a lost computer: `devices revoke --lost`. On Dropbox that's all: it asks to
allow Omacloud again, and the old token is revoked once every computer has switched.
With a bucket, also make a new key at the provider, run `bucket set-key`, then delete
the old key once `bucket status` says every computer switched. Each step needs the owner.

The owner must retain the recovery code privately. Commands that print a
secret (`init` creating an account, `join-code`, `invite`, `drive-code`, `drive-link`, `export`,
`recovery split|combine`) refuse with `terminal_required`
off a terminal: have the owner run them in their own terminal. Never work
around that refusal (no pseudo-terminals, no `script`), and never run them
yourself, even where your shell has a terminal and nothing refuses. Joining with a join code or recovery code prints none.
Never put recovery codes, join codes, bucket keys, or passwords in chat or shell
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
omacloud restore 'notes.md' --at 3 --to './notes-restored.md'
```

Sync is bidirectional and can change local and remote files. Do not start
`omacloud watch` or enable its service for an inspection request. Restore to a
separate destination first; `restore` over an existing file, `devices revoke`
and `rotate` need `--yes`, which needs the owner's authority.

Folder controls: `folders add Music` (a name or a path directly in home, like
`~/Work`; `folders list` gives the ones not syncing yet as `more`), `folders skip
Pictures`, `folders unskip Pictures`. Adding affects the shared folder set; skipping leaves this device's
files in place and stops syncing them here. Explain scope before changing it.

Choices can change at any time, and a running `watch` follows them. A folder or
subfolder coming back loses nothing: what differs from the cloud is kept, as a
conflict copy where both have a version. `folders list --json` gives each folder's
`size` in bytes, plus `others`, `skipped`, `excluded` and `removed_from_cloud`.

- `folders exclude Documents/Notes`, `folders include Documents/Notes`: leave a
  subfolder out here. Its files stay, and other devices keep syncing it.
- `folders remove Documents --yes`: take it out of the cloud. No device deletes its
  files, each stops syncing it, and earlier versions go within 30 days. Ask the
  user first. `folders add Documents` brings it back from what's on that device.

Skipping keeps the cloud's copy; only `folders remove` takes it out.

Dropbox: `init --provider dropbox` needs someone at a browser to allow it once, or a
refresh token already allowed in OMACLOUD_DROPBOX_REFRESH_TOKEN. Each person uses their
own Dropbox, so `invite` doesn't apply; `bucket set-key` needs no key id there.

Settings: `settings on|off`, `settings skip|unskip shell|agents|apps|theme|chromium|wifi`,
`settings resolve PATH --keep local|remote`.
Changes can affect desktop settings, browser configuration, Wi-Fi, and agent
settings across computers. Existing settings can contain API keys. Do not sync
or settle conflicts without the owner's requested scope; never blindly prefer
remote settings. Follow the applicable desktop configuration skill when editing
Omarchy config directly.

Leaving: `omacloud export` prints the plain restic command that restores from the
storage, with the repository password (and a bucket's key; Dropbox goes through rclone); `--to <folder>` copies the
repository to a folder first. It prints secrets: the owner runs it.

Machine migration: `packages restore --from NAME` previews package installation;
`--yes` runs pacman. `restore-machine --from NAME` is broader: settings, files,
packages, and daemon startup. It is not a harmless preview. Secret restoration
and device revocation/key rotation likewise need explicit scope; inspect their
help and the README first. Never run server deploy/release scripts for user setup.
