# Vendored rustic_core

crates.io rustic_core 0.13.0 plus the two patches in `spike/upstream`:

- `0001-pack-padding.patch`: `pack_padding` repo config option (Padmé padded packs)
- `0002-tree-splice.patch`: `Repository::splice_tree`, `TreeEdit`, `Repository::save_snapshot`

Applied with `patch -p3`; test hunks skipped (the crates.io package has no
tests). Drop this copy once both land upstream and a release includes them.
