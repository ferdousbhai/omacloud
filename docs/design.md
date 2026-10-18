# How Omacloud works

Omacloud keeps Desktop, Documents and Pictures (and any folder of home you
add) in sync on every Omarchy computer, with Omarchy settings, package lists
and ssh and gpg keys alongside, every version kept, end to end encrypted.
This page describes what protects your data and what each party can and
can't do. The source files named here hold the details.

## Where it's stored

Everything lives in two places: a restic repository with the files, and the
records that keep your computers in step. Omacloud is between storages:
the earlier storage choices (see `CHANGELOG.md`) are gone, and the Omacloud
network that replaces them is being built (below). Until it's here, both
live in directories you give `omacloud init` (`--repo`, `--coordinator`),
for development and tests. Either way the storage sees only encrypted
objects, and nothing below depends on trusting it.

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
so whoever can write to the storage can't roll history back, withhold it,
fork it or forge it without the other computers noticing.

The heads, device chain and epoch records are append-only logs
(`head.rs`'s `Coordinator`). Appending is create-only: the write is refused
if another computer took that position first. For development they are
files in a shared directory (`DirCoordinator`), each hard linked into place
so a taken name fails. A coordinator may keep a change marker that every
write moves; an idle computer checks it every few seconds and syncs when it
moves, with a full sync every ten minutes regardless.

With the network, the Worker at omacloud.computer will order these records
instead: a conflicting insert is the create-only append. It will hold them
encrypted with a key derived from the epoch secret, under a random account
id, and accept writes only with a bearer capability also derived from the
epoch secret (it keeps only its hash). A new epoch replaces the capability,
which shuts a removed computer out. Clients still verify every signature
and the chain, so the Worker is trusted for ordering only.

## The Omacloud network (in progress)

None of this ships yet; it is the design being built.

- **Holders.** Objects live on Omarchy computers that run Omacloud, each
  giving some disk space. Every node is untrusted. Each object is kept on
  two holders, picked by the owner's computer from a public node directory:
  on different networks (ASN and /24 or /48), never the account's own
  computers, at least one with high uptime. New nodes count only as extra
  copies while on probation.
- **Scrambling.** Each holder gets its own copy, the restic object XORed
  with a keystream from the epoch secret and the holder's id, so no two
  holders store the same bytes and only the account's computers can make
  or undo one. A read is checked against the object's hash after
  unscrambling.
- **Gateway.** Each Omacloud process serves a small S3 subset on localhost,
  so restic writes and reads as before. An encrypted index, kept by the
  Worker, says which holders have each object; the Worker can't read it.
- **Credits.** Storage is paid in anonymous credits: blind-signed tokens
  the Worker issues and redeems, so it can't link what an account spends
  to the holder that earns it. Giving space earns what storing costs.
- **Leases, audits, repair.** Owners renew each holder monthly with
  payment; a blob that isn't renewed is dropped after a grace period, so
  deletes are slow by design. Owners check random ranges of what holders
  keep, stop paying one that fails, and place its objects again, as they
  do for a holder gone for days.
- **What stays central.** The Worker orders coordination records, keeps the
  node directory and the encrypted index, and issues credits. There are no
  accounts, emails or sign-ins: only ids and keys the computers make.
  Omacloud runs seed nodes and relays while the network is small.
- **Transport.** Computers talk over iroh. Traffic between accounts goes
  through relays, so a holder doesn't learn an owner's address; an
  account's own computers may connect directly.

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

Today, with the development setup, whoever can read the shared
directories sees encrypted objects, their sizes (padded) and when they're
written. With the network, as designed:

- **The Worker** sees random account ids, encrypted coordination records,
  the number of index entries and when they change, node ids with their
  network and uptime, credit tokens it can't link to who spent them, and
  IP addresses and timing. It never links a node to an account, and can't
  read object names, placements or heads.
- **Holders** see scrambled blobs, their sizes and when they're fetched or
  checked, and a relay's address rather than the owner's.
- **Relays and seeds** see who connects to whom and how much, by address;
  seeds are holders too.
- **Cloudflare** sits in front of the Worker and sees what it sees.
- **A removed computer** keeps what it had synced before removal, and with
  a new epoch can't read what comes after or write coordination records.

The remaining leaks are addresses and timing at the Worker and the relays.

Nobody but your computers and your recovery code can read file contents,
names, settings or keys.

- **Programs you run, AI agents included,** can run `omacloud` as you. The
  commands that print a secret (the recovery code, a join code,
  shares, the repository password) print it only in text
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

- Key transparency for device keys; until then, approvals compare
  fingerprints out of band.
- Direct computer-to-computer sync on a local network.
- Online-only files fetched on open (#7).
- An outside review of this design (#18).
