# How Omacloud works

Omacloud keeps Desktop, Documents and Pictures (and any folder of home you
add) in sync on every Omarchy computer, with Omarchy settings, package lists
and ssh and gpg keys alongside, every version kept, end to end encrypted.
This page describes what protects your data and what each party can and
can't do. The source files named here hold the details.

## Where it's stored

Everything lives in two places: a restic repository with the files, and the
records that keep your computers in step. Both are on the Omacloud network
(below): the objects on other Omarchy computers, the records at the Worker
on omacloud.computer, sealed. For development and tests, `omacloud init
--repo <path> --coordinator <dir>` keeps both in directories instead, and
`OMACLOUD_NETWORK=dir:<path>` keeps the Worker's part of a network in a
directory. Either way the storage sees only encrypted objects, and nothing
below depends on trusting it.

## Storage: plain restic

Files live in a restic repository, written through
[rustic_core](https://github.com/rustic-rs/rustic_core) (`repo.rs`). Restic
encrypts contents, file names and directory structure (AES-256-CTR with
Poly1305-AES), deduplicates, and keeps each version as a snapshot, for 30
days: once a day the member with the lowest device key forgets older ones
(`Engine::tidy`), keeping the one each member last synced (recorded in the
sealed `bases` records) and the one the window opens on, and prunes with
rustic's lock-free two phases (packs younger than a day are left alone,
packs marked for deletion wait a week, so data a computer takes up again
in the meantime is kept). Two
patches (`vendor/rustic_core/OMACLOUD.md`) pad pack files to Padmé sizes, so
an object's size says little about the files in it, and let a sync rewrite
one subtree of a snapshot without walking the whole folder.

Because it's plain restic, you can always leave: `omacloud export --to
<folder>` copies the repository to a folder and prints its password, and
restic or rustic restores everything without Omacloud. `omacloud gateway`
serves the repository on the network as S3 on loopback while it runs, for
restic to read in place.

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

On the network, the Worker at omacloud.computer orders these records
(`omacloud-net`'s `coord.rs` and `records.rs`): a taken position stays
taken, which is the create-only append. Every body is sealed with a key
derived from the recovery code's root, under an account id derived from it
too; join codes carry that key and the network's address, so a join code
opens the records but not the files. Writes need a bearer capability
derived from the epoch's repository password (the Worker keeps only its
hash); a new epoch replaces it, which shuts a removed computer out.
Joining with the recovery code opens the epoch key as the root first, then
writes with that epoch's capability. Join requests are the one write that
needs no capability. Clients still verify every signature and the chain, so
the Worker is trusted for ordering only. Its API is in
[network-api.md](network-api.md).

## The Omacloud network

The crate `omacloud-net` makes the network look like an S3 bucket to the
sync engine. Nothing in it is trusted for anything but availability.

- **Holders.** Objects live on Omarchy computers that run Omacloud, each
  giving some disk space while its daemon runs (20 GiB unless told
  otherwise, `omacloud node give`); `omacloud node run` runs one with no
  account, as seeds do. The owner's computer picks holders from the public
  node directory (`place.rs`): two copies per object (or six shards, below), on different
  networks (ASN and /24 or /48 as the Worker saw them), one of them always
  on (95% uptime or more) when there is one, never a node of the
  account's own computers (each records its node id, sealed, among the
  account's records). New nodes are on probation and come last: they hold
  copies only when established nodes run out, as on a new network. Among
  the rest the draw favours steady uptime and a long record (it grows over
  three months), so the holders that stay get more to keep and earn more:
  a node key is free to make, but a node that walks away to start over
  gives up its history.
- **Shards** (`erasure.rs`). An object of 1 MiB or more (restic's data
  packs) is coded 4+2: its four slices and two parity shards, each on its
  own holder on its own network, any four of which rebuild it. That keeps
  1.5 times its size and survives two holders going, where two copies keep
  twice it and survive one. A range is read from the slices it falls in;
  a slice that doesn't come is rebuilt from four shards asked at once.
  While the network has fewer than six independent nodes, large objects
  are kept as two copies too.
- **What a holder keeps** (`node.rs`): a blob only if its bytes hash to the
  id it was stored under. Anyone may read any blob, as it's ciphertext. A
  first lease is 7 days, free. A renewal keeps a blob up to three months
  on credit and pays for the time it was already kept: a holder is paid
  for what it served, as the owner's audits found it, never ahead, so
  there's nothing to take and run with. Asked with too little, a holder
  says what's owed; the owner pays it unless it's more than every byte for
  every month since the last renewal, which marks the holder a cheat. A
  blob that was paid for gets 180 days' grace after its lease before it's
  dropped, so deletes are slow by design. A holder never learns whose
  blobs it has.
- **Scrambling** (`blob.rs`). Each holder gets its own copy, the restic
  object XORed with ChaCha20 keyed per epoch, its nonce from the object key
  and the holder's id, so no two holders store the same bytes and a holder
  that dropped its copy can't answer from another's. It adds uniqueness,
  not secrecy: the object is already restic ciphertext.
- **Reads and audits** are BLAKE3-verified ranges (iroh-blobs), checked
  against the blob's id as they arrive.
- **The index** (`seal.rs`, `registry.rs`) says which holders keep each
  object. The Worker keeps it under entry ids that are keyed hashes of the
  object keys, each entry sealed (XChaCha20 with a keyed BLAKE3 MAC, bound
  to its entry id), in a scope per epoch, so it can't read entries or move
  one under another name unnoticed.
- **Keys per epoch.** The S3 access key the engine uses is the epoch's
  network key, made from the repository password (`repo.rs`'s
  `network_key`). The gateway (`gateway.rs`), on 127.0.0.1 only, is
  stateless: from that key it derives the bucket's scope and the keys that
  scramble and seal. A rotation (a new epoch) gets a new bucket with keys
  the removed computer never saw.
- **Credits** (`credit.rs`). Storage is paid in blind-signed tokens (RFC
  9474), each a GiB kept for a month. The Worker issues and redeems them
  and can't link a token to whoever it gave it to. A holder checks tokens
  against the Worker's public key, trades them for fresh ones of its own,
  and spends those on its own account's storage: giving space earns what
  storing costs. A computer short of tokens asks for a small starter grant.
  There's no money in it, by design: credits are only earned by giving
  space, never bought or sold.
- **Upkeep** (`upkeep.rs`, run by the daemon). Every 15 minutes a node
  tells the directory it's up and trades in the tokens it took. Every 6
  hours the account's computer reads back 32 random ranges of its copies,
  monthly pays each holder for the month it kept them (from a wallet in its
  data directory), and gives new copies to objects whose holder failed an
  audit or left, fewest copies first. A holder that failed is remembered
  for 30 days and given no new copy meanwhile: kept on this computer,
  reported nowhere, so nobody can get an honest node shunned. Audits are ordinary reads: a holder can't
  tell one from a restore, and nothing about them goes to the Worker.
- **Transport.** Computers talk over iroh. A computer reaches holders from
  a throwaway key made for each run, so a holder can't tie the blobs it's
  given to the node key that computer serves under. Port mapping and n0's
  discovery are off. On omacloud.computer's network every computer uses
  Omacloud's relay (relay1.omacloud.computer, on the first seed), and
  reaches holders through it only, so a holder never learns its address;
  nodes take direct connections too. Other networks set their own relays
  (`[network] relays`).
- **Shared files** (`share.rs`). `omacloud share` seals the file under a
  random 256-bit secret and keeps it in a bucket of its own, whose keys
  come from that secret alone, like an epoch's from its network key. The
  link is `<network>/s#<secret>`: the Worker's page at `/s` never gets the
  part after `#`. A share lasts its first lease, 7 days, unless the
  sharing computer's daemon renews it (`share --days`), keeping its secret
  in a private file beside the wallet, and paying as for its own copies. Holders and the Worker see a bucket like any other,
  unlinked to the account that made it.
- **What stays central.** The Worker keeps the node directory, the sealed
  index, the sealed coordination records, and issues credits. There are no
  accounts, emails or sign-ins: only ids and keys the computers make.
- **Outliving the Worker** (`mirror.rs`). Each of an account's computers
  keeps, as the Worker keeps them, the sealed records and index entries it
  reads and writes, and the node directory. Reads fall back to that copy
  while the Worker is out of reach. A listing that drops most of what's
  kept reads as loss, not deletes, so a Worker that lost everything doesn't
  wipe the copy. When a computer finds the records older than it already
  saw, its daemon puts the copy back (`omacloud republish` by hand): the
  account's records, made writable with the capability its own key gives,
  then the index of the current epoch's bucket and of live shares. Only
  what's missing is written, and logs stay create-only, so it can't
  overwrite anything.

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

With the development setup, whoever can read the shared directories sees
encrypted objects, their sizes (padded) and when they're written. On the
network:

- **The Worker** sees random account ids, encrypted coordination records,
  the number of index entries and when they change, node ids with their
  network and uptime, credit tokens it can't link to who spent them, and
  IP addresses and timing. It never links a node to an account, and can't
  read object names, placements or heads.
- **Holders** see scrambled blobs, their sizes and when they're fetched or
  checked, and a node key that changes every run. With relays set, a
  computer reaches holders through relays only, so they don't learn its
  address; with none (as until seed relays run), it connects directly and
  they do.
- **Relays and seeds** see who connects to whom and how much, by address;
  seeds are holders too. Omacloud will run them while the network is small.
- **Cloudflare** sits in front of the Worker and sees what it sees.
- **A removed computer** keeps what it had synced before removal, and with
  a new epoch can't read what comes after or write coordination records.

The remaining leaks are addresses and timing at the Worker and the relays,
and at holders when there are no relays.

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

On the network:

- One seed and one relay, at one provider (Hetzner, Helsinki): while the
  network is small, most copies have one always-on home, on one network.
  Seeds at other providers would remove that single point (#33).
- OHTTP or Tor for requests to the Worker (#39), a transparency log
  (#40), and a DHT for finding nodes when the Worker is down (#41); each
  computer already keeps its account's records and index (above).

Elsewhere:

- Key transparency for device keys (#40); until then, approvals compare
  fingerprints out of band.
- Direct computer-to-computer sync on a local network (#41).
- Online-only files you can edit in place (#7): `omacloud mount` shows
  every file read-only, fetched on open, but a folder is either kept here
  in full or skipped, not freed file by file.
- Sharing a folder with other people (#8); a shared link (#15) is one
  file, read-only, for 7 days.
- Reaching files from a phone or a browser (#35).
- An outside review of this design (#18).
