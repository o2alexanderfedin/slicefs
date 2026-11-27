# Metadata maps

The metadata of a store has five maps: `inode_data`, `dir_data`,
`manifest_data`, `xattr_data` and `refcounts`
(`crates/metadata/src/store.rs`). The duplicate `inode_map` was removed in
store format 8. The format of the prolly maps that replace the flat maps is
in [Metadata maps (store format 8)](store-format.md). Each map is a
`BTreeMap` in RAM. On each
commit, `begin_commit` clones every map, and `finish_commit` serializes each
map as one flat node (an 8-byte count and fixed entries of 41 bytes) and
stores it with `store_io::put_value`. That function cuts the node with
FastCDC under a manifest tree, so a commit stores only the changed chunks
of a map, but it serializes and hashes every map in full: about 10.3 MB per
commit at 84,104 inodes (Boost 1_84_0). File manifests are already
content-cut trees (`crates/chunk-store/src/file.rs`): nodes are cut by
chunk ID, entries carry subtree byte lengths, and an edit rewrites one path.

## Metadata maps: prolly-tree decision

Status: recommendation of Phase 24 (#19), 2026-11-26, confirmed by a
second run after the fixes of the code review (with real inserts and
deletes in the middle of the map, and the disk sizes with the pack index).
Adoption is an owner decision (Q-1); it is not taken.

Decision: **later**. The maps stay flat for now.

Reasons (macOS, Boost 1_84_0 and 1_85_0, median of 3 runs; a Linux Docker
run gives the same answer, with one row that is not better, see below):

- A prolly `inode_data` does much less work per small commit than the flat
  map: for one changed file, 3.3 KB serialized in place of 3,448.3 KB, 0.6
  CPU ms in place of 6.6, and 2.6 KB new stored in place of 15.4 KB.
- But at the Boost scale the estimated gain on the whole commit is only
  about 4 to 8 ms (for one changed file 58.4 ms in place of 63.9 ms,
  91.4 %). That is below the 10 ms limit of the rubric (on Linux too) and
  below its 20 % limit on macOS. For 1,000 changes and for the version
  step, prolly is slower than a full flat rewrite.
- Inserts and deletes in the middle of the map move node boundaries and
  cost prolly more than value changes: at 100 of them per commit prolly is
  only a little better than flat on macOS (10.1 against 12.5 CPU ms) and a
  little worse on Linux (13.4 against 12.7 CPU ms, 16.7 against 15.4 ms).
- The gain grows with the store: with 4,000,000 entries, one change costs
  flat 242.1 ms and prolly 22.0 ms. One flat map passes 100 ms per commit at
  about 1.6 million entries (on Linux too).
- New stored bytes and disk growth are lower with prolly (38.4 MiB in place
  of 64.4 MiB over 121 commits, pack index included), but the current
  design already stores only the changed chunks.
- A smaller first step existed: `inode_map` and `inode_data` held the same
  entries, so one of them could go. Phase 25 removed `inode_map` (store
  format 8).

The full report, with the method, the tables, the boundary rule and the
crate survey: [Prolly trees for the metadata maps](../research/prolly-trees.md).

What adoption would change: the root record, all five maps, the GC mark,
rehash, snapshots and the format number (a store format change).

Owner questions (one-way doors; Q-2 and Q-3 are answered):

- Q-1: adopt prolly maps, and in which milestone? Phase 24 recommended
  later. Answered by the v2.3 milestone: adoption is issue #215.
- Q-2: one prolly map per metadata map, or one combined namespace map?
  Answered in Phase 25: one prolly tree per map (five trees). See
  `docs/design/store-format.md`, section "Metadata maps (store format 8)".
- Q-3: do the boundary rule and its constants become part of the frozen
  store format? Answered in Phase 25: yes, with a fixed-point table in place
  of the float test and a version byte for the rule. Same section.

The crate is `crates/prolly`. Its node format, rule and constants are part of
store format 8. No product path uses it yet (issue #215).
