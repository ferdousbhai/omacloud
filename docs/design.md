# How Omacloud works

Omacloud keeps Desktop, Documents and Pictures (and any folder of home you
add) in sync on every Omarchy computer, with Omarchy settings, package lists
and ssh and gpg keys alongside, every version kept, end to end encrypted.
This page describes what protects your data and what each party can and
can't do. The source files named here hold the details.

## Where it's stored

Everything lives in one S3 bucket: the files, and the records that keep
your computers in step. It is either your own, rented from a provider
(Hetzner, R2, B2, MinIO), and then your computers talk to it and to nothing
else; or a folder of Omacloud storage, reached through the gateway at
storage.omacloud.computer (`server/`) with the key your computer got when
you signed in with Google. The gateway checks that key, keeps each account
to its own folder of the bucket behind it, and enforces the account's
quota. Either way the storage sees only encrypted objects, and nothing
below depends on trusting it.

## Storage: plain restic

Files live in a restic repository, written through
[rustic_core](https://github.com/rustic-rs/rustic_core) (`repo.rs`). Restic
encrypts contents, file names and directory structure (AES-256-CTR with
Poly1305-AES), deduplicates, and keeps every version as a snapshot. Two
patches (`vendor/rustic_core/OMACLOUD.md`) pad pack files to Padmé sizes, so
an object's size says little about the files in it, and let a sync rewrite
one subtree of a snapshot without walking the whole folder.

Because it's plain restic, you can always leave: `omacloud export` prints the
repository location and password, and restic or rustic restores everything
without Omacloud.

## Keys

- **Recovery code.** 28 characters, shown once when the account is created.
  It derives the account's root key (`devices.rs`), which never lives on a
  computer. With it you can add a computer when no other is at hand, and
  recover the account. It can be split among people you trust, any `k` of
  `n` shares rebuilding it (Shamir's secret sharing, `shamir.rs`).
- **Trusted contact.** With Omacloud storage, the recovery code is split in
  two (`contact.rs`): Omacloud keeps a random pad, and a person you trust
  keeps a card, the code plus the pad character by character (a one-time
  pad over the code's alphabet, with four check characters). Either half
  alone is uniformly random. The pad goes only to a Google sign-in of the
  account, never to a storage key, so a computer holding a key (a removed
  one, say) can't fetch or replace it. A new card replaces the pad, which
  voids earlier cards. Omacloud together with the card could rebuild the
  code; neither can alone.
- **Device keys.** Each computer has its own signing key. The account's
  computers form a signed, hash-chained device chain: the root signs the
  first computer, and after that a computer counts only if the root or an
  existing member signed it in. A new computer is approved by comparing the
  fingerprint it shows with the one on the approving computer.
- **Repository keys.** The repository password is sealed to each member
  computer and to the root (`epoch.rs`). Removing a computer starts a new
  *epoch*: a new repository with a new key, sealed only to the remaining
  members, with every snapshot copied over so history stays readable. The
  removed computer keeps only what it had already seen.
- **Secrets bundle.** ssh and gpg keys never sync as files. `omacloud secrets
  save` seals them to the root (`secrets.rs`), so only the recovery code
  opens them: no computer, not even a member, can read another computer's
  keys.

## Coordination: signed history

Computers agree on the latest version through a chain of *heads*
(`head.rs`). Each head names a snapshot, links to the previous one by hash,
records the device chain position its signer saw, and is signed by the
computer that pushed it. Every computer remembers the last head it verified,
so whoever can write to the bucket can't roll history back, withhold it,
fork it or forge it without the other computers noticing.

The heads, device chain and epoch records are objects in your bucket
(`bucket.rs`). Appending is a create-only write (`If-None-Match: *`), which
the bucket refuses if another computer took that position first. S3, R2,
MinIO and Hetzner support it on buckets without versioning; a bucket with
object lock keeps these records in a second, plain bucket
(`--coordination-bucket`). Every write also rewrites a small `changed`
object; an idle computer checks that every few seconds, one request, and
syncs when it moves, with a full sync every ten minutes regardless.

## Bucket keys

Every computer holds the bucket's key, kept in the
desktop keyring rather than a file. With your own bucket, a lost computer is
cut off by changing that key (`bucket_key.rs`): one computer publishes the new key sealed to the
current members and the root, the others switch on their next sync and say
so, and once all have, you delete the old key at your provider. A computer
that was away through the whole change catches up with `omacloud bucket
set-key --here-only`.

With Omacloud storage, Omacloud issues the keys, the new one too: a
computer asks for it with its current key (`server/src/keys.ts`) and
publishes it as above, and once every computer has switched, one of them
retires the account's other keys, a removed computer's among them. A
removed computer could ask for keys or retire others in the meantime, but
whatever it makes is retired with the rest, and a computer whose key was
retired signs in with Google again (`bucket set-key --here-only`): at worst
it makes the account's computers sign in again. What any key deletes or
overwrites meanwhile is kept: the gateway first copies it to a trash
outside every account's folder, kept 14 days (`server/src/trash.ts`), from
which Omacloud can put back what was deleted or overwritten after a given
time. A single delete or put then goes ahead only if the object is still
the version copied (`If-Match`), where the bucket checks that; one that
turns the check down gets the request again without it. A batch delete, a
finished multipart upload, and any delete or put on a bucket without the
check, can lose a version another computer writes in the moment between
the copy and the delete (milliseconds).
The trash holds at most the account's quota: past that, deletes and
overwrites are refused until some of it expires, rather than anything
leaving it early.

With delete protection on the bucket (object lock, R2 bucket locks), sync
works as usual, and old epochs are deleted once their retention ends.

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

- **Your storage provider** sees encrypted objects, their sizes (padded) and
  when they're written.
- **Omacloud storage**, when you use it, is that provider: it sees the same,
  plus your Google account's id and email. With your own bucket, the
  Omacloud project sees nothing: there is no Omacloud server in the way.
- **A trusted contact** holds a card that is random without the pad
  Omacloud keeps, and Omacloud's pad is random without the card.
- **A removed computer** keeps what it had synced before removal, and can
  reach the bucket until its key changes.

Nobody but your computers and your recovery code can read file contents,
names, settings or keys.

## Not yet

- Key transparency for device keys; until then, approvals compare
  fingerprints out of band.
- Direct computer-to-computer sync on a local network.
- Online-only files fetched on open (#7).
- An outside review of this design (#18).
