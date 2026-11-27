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
`--ids-only` ids, `--count` a number, `--styled` text. A failure exits 1 and
goes to stderr, stdout staying empty, as one line
`{"ok":false,"error":{"code","kind","message","hint"?}}`: follow the hint.
`kind` is `usage` (incl. a missing `--yes` or a secret command off a
terminal), `setup` (not set up/approved or a refused key/code), `not_found`,
`unavailable` (storage or network out of reach) or `error`. Inspection may contact the
repository; report errors and offline state instead of implying everything is
current. Consult `omacloud <command> --help`.

## Jobs

Whole tasks the owner asks for in plain words, step by step. Each uses the
commands described further down; the safety rules there still hold.

**Get back a deleted or changed file.** For a deleted one, `omacloud deleted`
(`--days 90` to look further back) lists it with the version to restore; for a
changed one, `omacloud versions PATH` lists its versions, oldest first. Restore
next to it, never over it: `omacloud restore PATH --at N --to /tmp/PATH.vN`,
and show the owner. Only once they say so, put it back in place with `--to
PATH --yes`. A file is named by its path inside the synced folder.

**Find something in an older state.** `omacloud mount DIR` (an empty folder)
shows every file of the account read-only, with history, fetched as it's
opened; run it in the background, search or read there, then stop it by its
PID. Nothing there changes the account.

**Work out why a computer isn't syncing.** Run `omacloud status` and read, in
order: `set_up` false (not set up here); `daemon` false (the background sync
isn't running: `systemctl --user status omacloud`; with the owner's OK,
`systemctl --user start omacloud`); `synced_at` long ago; `requests` (computers
waiting to be approved); `settings.held` (settings waiting to be settled:
`omacloud settings status`). Then `omacloud sync` once shows the failure itself:
`not_approved` is fixed on a computer already in (`devices approve`),
`unavailable` is the network or the storage. `journalctl --user -u omacloud
-n 50` has the daemon's own account; `pausing sync (battery)` is on purpose,
unplugged and below 20% battery (by default), and ends once charging.

**Move to a new computer.** Install there. The owner runs `omacloud join-code`
on a computer already in, and on the new one runs `omacloud init` with the code
in `OMACLOUD_JOIN_CODE`, in their own terminal; on the old one they compare the
fingerprint the new one shows and run `omacloud devices approve FINGERPRINT`.
Then, on the new computer, `omacloud restore-machine --from OLD` shows what
would come over (settings, files, packages); `--yes` does it (pacman still
asks), and `--replacing` also brings what OLD kept as its own, when the new one
takes its place. Retiring the old one afterwards is `devices revoke` with the
owner's `--yes`.

**Add a family member.** Each person gets an account of their own, never a
share of the owner's. With Dropbox, they set up on their own computer with
their own Dropbox (`omacloud init --provider dropbox`, in their terminal).
With a bucket, the owner makes them a bucket and key (docs/providers.md), and
runs `omacloud invite --provider ...` with those details in their own terminal;
the family member runs `omacloud init` with that code in `OMACLOUD_JOIN_CODE`,
and keeps the recovery code it shows them alone.

**A computer is lost or stolen.** `omacloud devices list` to find it, then the
owner runs `omacloud devices revoke FINGERPRINT --lost`; the rest is in
"Removing a lost computer" below.

## Setup (only when requested)

Storage is the owner's own Dropbox (the simplest), or an S3 bucket: Cloudflare R2,
Hetzner or any other with create-only writes, or a folder on a drive or NAS of theirs
(https://github.com/ferdousbhai/omacloud/blob/master/docs/providers.md). Agree on device name, folders, and whether
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
- A folder: `omacloud init --provider folder --path <folder>` on a drive or a NAS's
  share with hard links (ext4, btrfs, NFS; `no_hard_links` otherwise), in the owner's
  terminal. Another computer joins by running the same with the folder as it sees it,
  and is approved. `unavailable` with "the account's folder isn't there" means the
  drive isn't plugged in or mounted: nothing to fix in Omacloud. No Omacloud Drive
  in a browser and no `bucket` commands there.
- `init --repo <path> --coordinator <directory>` uses plain directories, for
  development and tests only. Point test accounts at `OMACLOUD_CONFIG`, never
  the owner's real `~/.config/omacloud`.

In a browser: Omacloud Drive (omacloud.computer/drive) shows the account's files. The
owner signs in with `omacloud drive-link` (a code for a phone's camera, good for five
minutes) or Open on a Phone in the app; both print a secret, so the owner runs them.
`omacloud drive-link --close` closes an open link and prints nothing secret. A file
uploaded there reaches a computer within about a minute, if one is on.

In Omarchy's bar a cloud shows how sync stands; `omacloud bar on` and `omacloud bar off`
put it there or take it out.

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
omacloud mount /tmp/all         # every file read-only in that empty folder, fetched on open; runs until Ctrl-C
omacloud restore 'notes.md' --at 3 --to './notes-restored.md'
```

Sync is bidirectional and can change local and remote files. Do not start
`omacloud watch` or enable its service for an inspection request. Restore to a
separate destination first; `restore` over an existing file, `devices revoke`
and `rotate` need `--yes`, which needs the owner's authority.

Folder controls: `choose Music sync` (a name or a path in home, like `~/Work`;
`folders list` gives the ones not syncing yet as `more`), `choose Pictures off`,
`choose Documents/Notes off` for a subfolder. Syncing a new folder adds it to every
computer; off leaves this device's files in place and stops syncing them here, and
other devices keep syncing it. Explain scope before changing it.

Choices can change at any time, and a running `watch` follows them. A folder or
subfolder coming back loses nothing: what differs from the cloud is kept, as a
conflict copy where both have a version. `folders list --json` gives each folder's
`size` in bytes, plus `others`, `skipped`, `excluded` and `removed_from_cloud`.

`folders remove Documents --yes` takes a folder out of the cloud: no device deletes
its files, each stops syncing it, and earlier versions go within 30 days. Ask the
user first; `choose Documents sync` brings it back from what's on that device. Off
keeps the cloud's copy; only `folders remove` takes it out.

Dropbox: `init --provider dropbox` needs someone at a browser to allow it once, or a
refresh token already allowed in OMACLOUD_DROPBOX_REFRESH_TOKEN. Each person uses their
own Dropbox, so `invite` doesn't apply; `bucket set-key` needs no key id there.

Settings: `settings resolve PATH --keep local|remote|each`. What syncs per computer:
`choose` shows every computer's; `choose WHAT sync|backup|off` changes this
one's (WHAT: a folder, `settings`, a group, or a settings file; backup is for
files only: kept from this computer, not synced).
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
packages, and daemon startup; with `--replacing` it also brings what NAME
backed up only (its monitors, files it chose backup for) for a computer taking its place. It is not a harmless preview. Secret restoration
and device revocation/key rotation likewise need explicit scope; inspect their
help and the README first. Never run server deploy/release scripts for user setup.
