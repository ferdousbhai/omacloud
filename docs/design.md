# How Omacloud works

Omacloud keeps Desktop, Documents and Pictures (and any folder of home you
add) in sync on every Omarchy computer, with Omarchy settings, package lists
and ssh and gpg keys alongside, earlier versions and deleted files kept
for 30 days, end to end encrypted.
This page describes what protects your data and what each party can and
can't do. The source files named here hold the details.

## Where it's stored

Everything lives in a bucket you choose: a restic repository with the files,
and beside it, under `omacloud-coordination/`, the records that keep your
computers in step. Any S3 store with create-only writes works
([providers.md](providers.md)); `omacloud init` checks for them before an
account depends on them (`BucketCoordinator::check`). Omacloud runs no storage
and no service an account depends on. For development and tests, `omacloud init
--repo <path> --coordinator <dir>` keeps both in directories instead. Either way
the storage sees only encrypted objects, and nothing below depends on trusting
it.

## Storage: plain restic

Files live in a restic repository, written through
[rustic_core](https://github.com/rustic-rs/rustic_core) (`repo.rs`). Restic
encrypts contents, file names and directory structure (AES-256-CTR with
Poly1305-AES), deduplicates, and keeps each version as a snapshot, for 30
days: once a day the lowest-key member among those standing on a version
from the last 30 days (the lowest of all when none is), if its daemon runs,
forgets older ones (`Engine::tidy`), keeping the one each member last synced (recorded in the
`synced` records) and the one the window opens on, and prunes with
rustic's lock-free two phases (packs younger than a day are left alone,
packs marked for deletion wait a week, so data a computer takes up again
in the meantime is kept). Two
patches (`vendor/rustic_core/OMACLOUD.md`) pad pack files to Padmé sizes, so
an object's size says little about the files in it, and let a sync rewrite
one subtree of a snapshot without walking the whole folder.

Because it's plain restic, you can always leave: `omacloud export --to
<folder>` copies the repository to a folder and prints its password, and
restic or rustic restores everything without Omacloud; without `--to` it prints
the command that restores from the bucket directly.

## Keys

- **Recovery code.** 28 characters, shown once when the account is created.
  It derives the account's root key (`devices.rs`), which never lives on a
  computer. With it you can add a computer when no other is at hand, and
  recover the account. It can be split among people you trust, any `k` of
  `n` shares rebuilding it (Shamir's secret sharing, `shamir.rs`).
- **Device keys.** Each computer has its own signing key. The account's
  computers form a signed, hash-chained device chain: the root signs the
  first computer, and after that a computer counts only if the root or an
  existing member signed it in. A new computer is approved by comparing the
  fingerprint it shows with the one on the approving computer.
- **Repository keys.** The repository password is sealed to each member
  computer and to the root (`epoch.rs`). Rotating (`omacloud rotate`, or
  `devices revoke --rotate` when removing a computer) starts a new *epoch*:
  a new repository with a new key, sealed only to the remaining members,
  with every kept snapshot copied over so history stays readable. A
  removed computer keeps only what it had already seen.
- **The bucket's key.** Every computer of an account holds it, in a config
  file only its owner can read, and a join code carries it. It reaches
  encrypted objects only. `omacloud bucket set-key` publishes a new one sealed
  to each member and the root, signed, beside the other records (`bucket_key.rs`).
  Each switches on its next sync and says so; once all have, the old key is
  deleted at the provider, which cuts off a removed computer.
- **Secrets bundle.** ssh and gpg keys never sync as files. `omacloud secrets
  save` seals them to the root (`secrets.rs`), so only the recovery code
  opens them: no computer, not even a member, can read another computer's
  keys.

## Coordination: signed history

Computers agree on the latest version through a chain of *heads*
(`head.rs`). Each head names a snapshot, links to the previous one by hash,
records the device chain position its signer saw, and is signed by the
computer that pushed it. Every computer remembers the last head it verified,
so whoever can write to the storage can't roll history back, withhold it,
fork it or forge it without the other computers noticing.

The heads, device chain and epoch records are append-only logs
(`head.rs`'s `Coordinator`), objects in the bucket (`bucket.rs`). Appending is
a create-only write (`If-None-Match: *`): the bucket refuses it if another
computer took that position first, which makes the bucket a compare and swap.
For development they are files in a shared directory (`DirCoordinator`), each
hard linked into place so a taken name fails. Every write the others need also rewrites an
object named `changed`; an idle computer reads it every ten seconds and syncs
when it moves, with a full sync every ten minutes regardless. So an idle computer
costs one read most of the time, and a small sync's worth of requests every ten
minutes.

## Accounts and people

One person is one account: a recovery code, a device chain, a repository. Every
computer of an account can read all of it, so a family isn't one account. Each
person gets a bucket and a key of their own: `omacloud invite` checks a bucket
and prints a join code that names no account, and the first computer to use it
creates that person's account, with a recovery code only they see. The person
who made the bucket holds its key and can delete what's in it, as a provider
can, but can't read it. A join code that names an account refuses a bucket
holding another.

## Settings

Omarchy's dots manifest lists which files of home are shared between
machines and which stay local (`settings.rs`). Omacloud syncs the shared
ones, inside the same encrypted history, with three-way merges and a backup
of anything it replaces. A change made on two machines that doesn't merge is
held for you to choose (Keep Mine or Keep Theirs). Settings sync stands down
when another dotfile manager owns those files. Omacloud's own entries
(shell dotfiles, AI agents' settings and skills) add a `dir/**` form for
whole folders: text files only, at most 2 MiB, never through a symlink,
`node_modules`, `.git` or a cache. Whatever the manifest says, a file named
like a credential, auth file or token never syncs as a setting; keys
written inside a synced settings file sync with it, encrypted like
everything else. A link among Omacloud's entries is skipped, and only a
link among Omarchy's own makes settings sync stand down. Codex's
`config.toml` and pi's `settings.json` sync as documents without their
machine-only parts (`agents.rs`).

Chromium's profiles and the saved Wi-Fi networks aren't files to copy, so
they sync as documents (`merged.rs`): each computer reads the app's own
store into a canonical document (bookmarks by guid, search engines by
Chromium's `sync_guid`, an allowlist of plain preferences, the set of Web
Store extensions; networks by name and security), merges it three ways
against the last synced version, and writes the result back into the app.
A merge always settles: what one side changed wins over what it left
alone, an edit beats a delete, and the same thing changed on both sides
keeps the merging computer's version and says so in a record each
computer keeps in the synced tree, so the computer whose edit gave way
shows it too. A document is written back only after the app is read
afresh: whatever changed there since the last look merges in first, and
one that can't be read right then isn't written. Chromium is written only while it's closed
(`chromium/`); preferences Chromium guards with a MAC are never touched.
Wi-Fi goes through NetworkManager as the user at the desktop
(`wifi.rs`), keys on `nmcli`'s standard input. These documents are in the
same encrypted history as everything else: the storage can't read them,
and every computer of the account can, Wi-Fi keys included.

## What each party can see

- **Your storage provider, and anyone with the bucket's key,** see encrypted
  objects, their sizes (padded), when they're written, and the addresses that
  reach them. They know whose bucket it is from your account with them. The
  records that keep computers in step are signed, not encrypted, so they also
  see your computers' names and fingerprints, and when each one pushed or asked
  to join. They can't read file contents, names, settings or keys, or forge
  history unnoticed. They can delete everything: see below.
- **Omacloud** sees nothing: it runs no service your data or your computers
  talk to.
- **A removed computer** keeps what it had synced before removal. With a new
  epoch it can't read what comes after; once the bucket's key changes and the
  old one is deleted it can't reach the bucket at all.

The remaining leaks are sizes, timing and addresses, at your provider.

## What can go wrong

Signed history means whoever holds the bucket's key can't change what you see
unnoticed; it doesn't stop them destroying it. A computer that's stolen or
infected holds the key, and could delete the bucket's objects. Against that,
the bucket needs delete protection (R2 bucket locks, object lock elsewhere;
[providers.md](providers.md#delete-protection)), which can't be set up from
here, because a key that could turn it on could turn it off. It's a step of
setting up a bucket, and the setup doesn't check for it.

One bucket is one provider. A provider losing the bucket or closing your
account takes the backup with it, though every computer keeps its synced files.
Omacloud doesn't keep a second copy elsewhere yet.

Nobody but your computers and your recovery code can read file contents,
names, settings or keys.

- **Programs you run, AI agents included,** can run `omacloud` as you. The
  commands that print a secret (the recovery code, a join code,
  shares, an invite, the repository password) print it only in text
  mode with stdin and stdout on a terminal, and refuse as JSON or into a
  pipe, so it doesn't land in a script's output, a JSON consumer or an
  agent's captured output by accident. That is a guard against accidents,
  not a boundary: a program that gives `omacloud` a whole pseudo-terminal
  (as `script(1)` does, and some agent harnesses and tmux-based agents do)
  sees the secret. Checking stdin as well turns away the many harnesses
  that give a command a terminal for its output only. Beyond that, the
  installed agent skill tells agents never to run these commands; whoever
  lets a program act as them in a terminal trusts it with what that
  terminal shows.

## Not yet

- A second copy in another bucket or at home, kept by Omacloud. Until then
  `omacloud export --to <folder>` makes one by hand.
- Key transparency for device keys (#40); until then, approvals compare
  fingerprints out of band.
- Direct computer-to-computer sync on a local network (#41).
- Online-only files you can edit in place (#7): `omacloud mount` shows
  every file read-only, fetched on open, but a folder is either kept here
  in full or skipped, not freed file by file.
- Sharing with other people (#8, #15).
- Reaching files from a phone or a browser (#35).
- An outside review of this design (#18).
