# Metadata maps

The metadata of a store has six maps: `inode_map`, `inode_data`,
`dir_data`, `manifest_data`, `xattr_data` and `refcounts`
(`crates/metadata/src/store.rs`). Each is a `BTreeMap` in RAM. On each
commit, `begin_commit` clones every map, and `finish_commit` serializes each
map as one flat node (an 8-byte count and fixed entries of 41 bytes) and
stores it with `store_io::put_value`. That function cuts the node with
FastCDC under a manifest tree, so a commit stores only the changed chunks
of a map, but it serializes and hashes every map in full: about 10.3 MB per
commit at 84,104 inodes (Boost 1_84_0). File manifests are already
content-cut trees (`crates/chunk-store/src/file.rs`): nodes are cut by
chunk ID, entries carry subtree byte lengths, and an edit rewrites one path.

## Metadata maps: prolly-tree decision

Status: recommendation of Phase 24 (#19), 2026-11-26. Adoption is an owner
decision (Q-1); it is not taken.

Decision: **later**. The maps stay flat for now.

Reasons (macOS, Boost 1_84_0 and 1_85_0, median of 3 runs; a Linux Docker
run gives the same answer):

- A prolly `inode_data` does much less work per small commit than the flat
  map: for one changed file, 3.3 KB serialized in place of 3,448.3 KB, 0.6
  CPU ms in place of 6.1, and 2.6 KB new stored in place of 15.4 KB.
- But at the Boost scale the estimated gain on the whole commit is only
  about 5 ms (56.4 ms in place of 61.7 ms, 91.4 %). That is below the 10 ms
  limit of the rubric (on Linux too) and below its 20 % limit on macOS. For
  1,000 scattered changes and for the
  version step, prolly is slower than a full flat rewrite.
- The gain grows with the store: with 4,000,000 entries, one change costs
  flat 229.9 ms and prolly 21.8 ms. One flat map passes 100 ms per commit at
  about 1.7 million entries (about 0.9 million on Linux).
- New stored bytes and disk growth are lower with prolly (17.5 MiB in place
  of 31.2 MiB after 82 commits), but the current design already stores only
  the changed chunks.
- A cheaper first step exists: `inode_map` and `inode_data` hold the same
  entries, so one of them can go.

The full report, with the method, the tables, the boundary rule and the
crate survey: [Prolly trees for the metadata maps](../research/prolly-trees.md).

What adoption would change: the root record, all six maps, the GC mark,
rehash, snapshots and the format number (a store format change).

Owner questions (open, one-way doors):

- Q-1: adopt prolly maps, and in which milestone? Recommendation: later,
  when stores of about a million inodes or more are a target.
- Q-2: one prolly map per metadata map, or one combined namespace map?
- Q-3: do the boundary rule and its constants become part of the frozen
  store format?

The prototype is the crate `crates/prolly`. Nothing in the product uses it.
`docs/design/store-format.md` does not change.
