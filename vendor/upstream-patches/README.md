# Upstream patches for rustic_core

Two changes Omacloud carries in `vendor/rustic_core`, prepared against
rustic-rs/rustic_core `main` (647d00f, 0.13.0 plus three commits). Each is a
separate patch and PR. Not submitted: opening them is our first public
contact with the rustic project, so it waits for a go ahead.

| patch | branch | what |
|---|---|---|
| `0001-pack-padding.patch` | `pack-padding` | opt in Padmé padding of pack files |
| `0002-tree-splice.patch` | `tree-splice` | `Repository::splice_tree` and `save_snapshot` |

Apply with `git am` on a rustic_core checkout at 647d00f; both apply cleanly.

Test status (2026-09-28, `cargo test -p rustic_core`, each branch in its own
target dir):

| branch | unit | integration | new tests |
|---|---|---|---|
| main (baseline) | 171 ok | 53 ok, 5 failed | |
| pack-padding | 172 ok | 54 ok, 5 failed | `padme_matches_the_paper`, `pack_padding` pass |
| tree-splice | 171 ok | 54 ok, 5 failed | `splice` passes |

The 5 failures are identical on unmodified main: insta snapshots that
expect `error_count: 0` in the backup summary, which the output no longer
contains (drift from upstream #561). Mention it in the PRs so reviewers
don't blame the patch. One `integration::prune` case panicked once with
"index still in use" during a run with a full `/tmp` and two parallel
builds; it didn't reproduce in 6 more runs on each of main and
pack-padding, and that case doesn't enable padding.

Found while porting: prune's repacker builds its pack sizer with
`PackSizer::fixed`, which would have dropped padding on repacked packs. The
patch carries the setting through (`PackSizer::with_padding`); the
integration test covers it. An earlier prototype switched padding on by
environment variable, so it never hit this.

Before opening either, read rustic's contribution guide
(https://rustic.cli.rs/docs/contributing-to-rustic.html) and consider opening
an issue first to ask whether they want the feature.

---

## PR 1: feat: optional Padmé padding for pack files

Adds a repository config option, `pack_padding` (`--set-pack-padding`), off
by default. When on, every pack gets one random data blob before its header
is written, sized so the finished pack is exactly a Padmé size (Nikitin et
al., "Reducing Metadata Leakage from Encrypted Files and Communication with
PURBs", PETS 2019).

Why: with small, frequent backups a pack often holds one file, so the pack's
object size tells the storage provider that file's size to the byte. Padmé
limits that to O(log log n) bits for at most 12% overhead.

Format: unchanged. The padding blob is an ordinary data blob whose id is the
hash of its (random) plaintext. It is in the pack header and the index, so
every reader sees a valid pack. Verified with upstream restic 0.19.1
(`restic check --read-data` clean, `restic restore` byte identical) on a repo
of 202 padded packs.

Prune: padding blobs are unreferenced, so prune counts them as unused and
drops them on a repack; repacked packs are padded again (covered by the
test). One side effect: every padded pack is "partly used", so with the
default `max_unused` of 5% prune will repack more than it needs to. Users of
padding should set `max_unused` to 12% or more. Happy to make prune padding
aware in a follow up if you prefer that.

Config: new optional field in the config file. restic ignores unknown config
fields, like the other rustic specific ones.

Tests: `padme_matches_the_paper` (unit) and `integration::pack_padding`
(backup into a padded repo, every pack is a Padmé size, check with
read data, prune, still padded, check again).

## PR 2: feat: splice_tree, edit a tree at given paths

Adds `Repository::splice_tree(root, edits) -> TreeId` for `IndexedIds`
repositories, the `TreeEdit` enum (`Put`, `PutFile`, `Delete`), and
`Repository::save_snapshot(&SnapshotFile) -> SnapshotId`.

Why: backup cost scales with the size of the source, because every file is
stat'ed and the whole tree is rebuilt. For continuous sync, where a watcher
already knows which paths changed, that dominates. `splice_tree` reads and
rewrites only the trees on the paths from each edit to the root; everything
else keeps its id. In our measurements on a 50k file tree, one changed file
went from ~850 ms (backup with parent) to ~40 ms, including restoring it on
a second machine by diffing trees.

Behavior:
- Paths are relative to `root`; empty, absolute, `.` and `..` are rejected.
- `PutFile` chunks the reader with the repository's chunker and dedupes
  against the index. `Put` stores a node whose content or subtree is already
  in the repository.
- The path decides the node's name.
- Missing directories are created with mode 0755 (Go's `ModeDir | 0755`) and
  the current time; directories left empty by deletions are removed, unless
  the same call puts them: `Put` of a directory node without a subtree means
  "this directory exists" (an existing one keeps its contents, a new one is
  empty), which lets a caller keep empty directories.
- File contents are chunked in parallel (rayon); trees are built after.
  A first push of 50k files went from 9.2 s to 3.3 s.
- One data pack, one tree pack and one index file per call, unless they grow
  past the pack size.

`save_snapshot` exists because `save_snapshots` doesn't return ids, and a
caller building a chain of snapshots needs the id as the next parent.

Tests: `integration::splice` modifies one file, deletes another, adds one in
two new directories, checks contents and names, checks that untouched
subtrees keep their ids, saves the snapshot, runs check with read data, and
checks path validation.
