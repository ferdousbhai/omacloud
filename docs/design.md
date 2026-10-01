# How OneCloud works

OneCloud keeps Desktop, Documents and Pictures (and any folder of home you
add) in sync on every Omarchy computer, with Omarchy settings, package lists
and ssh and gpg keys alongside, every version kept, end to end encrypted.
This page describes what protects your data and what each party can and
can't do. The source files named here hold the details.

## Two ways to run it

- **Self-hosted** (free): everything lives in your own S3 bucket (Hetzner,
  R2, B2, MinIO), coordination included. Nobody sits in between: your
  computers talk to your bucket and to nothing else.
- **Hosted** (from 1.0): the OneCloud service stores the data and
  coordinates. It sees only ciphertext and signed records, and the client
  verifies everything it relays.

## Storage: plain restic

Files live in a restic repository, written through
[rustic_core](https://github.com/rustic-rs/rustic_core) (`repo.rs`). Restic
encrypts contents, file names and directory structure (AES-256-CTR with
Poly1305-AES), deduplicates, and keeps every version as a snapshot. Two
patches (`vendor/rustic_core/ONECLOUD.md`) pad pack files to Padmé sizes, so
an object's size says little about the files in it, and let a sync rewrite
one subtree of a snapshot without walking the whole folder.

Because it's plain restic, you can always leave: `onecloud export` prints the
repository location and password, and restic or rustic restores everything
without OneCloud.

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
  computer and to the root (`epoch.rs`). Removing a computer starts a new
  *epoch*: a new repository with a new key, sealed only to the remaining
  members, with every snapshot copied over so history stays readable. The
  removed computer keeps only what it had already seen.
- **Secrets bundle.** ssh and gpg keys never sync as files. `onecloud secrets
  save` seals them to the root (`secrets.rs`), so only the recovery code
  opens them: no computer, not even a member, can read another computer's
  keys.

## Coordination: signed history

Computers agree on the latest version through a chain of *heads*
(`head.rs`). Each head names a snapshot, links to the previous one by hash,
records the device chain position its signer saw, and is signed by the
computer that pushed it. Every computer remembers the last head it verified,
so whoever relays heads (the service, or your bucket) can order them but
can't roll history back, withhold it, fork it or forge it without the
clients noticing.

- **Self-hosted** (`bucket.rs`): the heads, device chain and epoch records
  are objects in your bucket. Appending is a create-only write
  (`If-None-Match: *`), which the bucket refuses if another computer took
  that position first. S3, R2, MinIO and Hetzner support it on unversioned
  buckets; a bucket with object lock keeps coordination in a second, plain
  bucket (`--coordination-bucket`). Computers poll for changes.
- **Hosted** (`http.rs`, `service/`): a Cloudflare Durable Object per account
  orders the records and pushes change notices. It checks every write
  against the same rules the client applies, and the client still verifies
  every read. Storage goes through presigned URLs (`storage.rs`), so a
  removed computer is cut off from storage at once.

## Bucket keys (self-hosted)

On a self-hosted account every computer holds the bucket's key, kept in the
desktop keyring rather than a file. A lost computer is cut off by changing
that key (`bucket_key.rs`): one computer publishes the new key sealed to the
current members and the root, the others switch on their next sync and say
so, and once all have, you delete the old key at your provider. A computer
that was away through the whole change catches up with `onecloud bucket
set-key --here-only`.

With delete protection on the bucket (object lock, R2 bucket locks), sync
works as usual, and old epochs are deleted once their retention ends.

## Settings

Omarchy's dots manifest lists which files of home are shared between
machines and which stay local (`settings.rs`). OneCloud syncs the shared
ones, inside the same encrypted history, with three-way merges and a backup
of anything it replaces. A change made on two machines that doesn't merge is
held for you to choose (Keep Mine or Keep Theirs). Settings sync stands down
when another dotfile manager owns those files.

## What each party can see

| | self-hosted | hosted |
|---|---|---|
| your bucket provider | encrypted objects, their sizes and times | (not involved) |
| the OneCloud service | nothing | encrypted objects, signed records, your computers' public keys, storage used |
| a removed computer | what it had synced before removal, until the bucket key changes | what it had synced before removal |

Nobody but your computers and your recovery code can read file contents,
names, settings or keys.

## Not yet

- Key transparency for device keys; until then, approvals compare
  fingerprints out of band.
- Direct computer-to-computer sync on a local network.
- Online-only files fetched on open (#7).
- An outside review of this design (#18).
