# Metadata maps

The metadata of a store has five maps: `inode_data`, `dir_data`,
`manifest_data`, `xattr_data` and `refcounts`
(`crates/metadata/src/store.rs`). Each map is a prolly tree
(`crates/prolly`), one tree per map (layout a). The key grammar, the value
encodings and the node format are in
[Metadata maps (store format 8)](store-format.md). The tree protocol is the
module doc of `crates/metadata/src/pmap.rs`. This page is a summary of how
the store uses the trees.

## The five trees

- `inode_data`: inode number -> the inode record, inline (56 or 72 bytes).
- `dir_data`: directory inode number and name -> child inode number and
  type byte. One entry per name; `.` and `..` are entries of every
  directory; a hard link is one more entry with the same child.
- `manifest_data`: inode number -> the key of the file's manifest (a
  content-cut tree of chunk keys, `crates/chunk-store/src/file.rs`).
- `xattr_data`: inode number and xattr name -> tag 0 and the value bytes
  (inline), or tag 1 and the key of a spilled value. A spilled value is
  stored like file content (chunks and a manifest) and read by range.
- `refcounts`: content key -> count.

No map is held whole in RAM, and no operation loads one: mount reads the
root record, one object per tree field and the root inode; counts come from
the root record or the counts in the internal tree entries.

## Overlay and working root

Each tree has a working root (the root and a generation) and an overlay of
the pending edits (a value, or a delete), in shards by key. A read looks at
the key's overlay shard first, then at the tree through the node cache. A
write goes to the overlay only. A read-modify-write (a refcount change, an
inode change, a name link) reads the tree with no lock held, then checks the
overlay and the generation under the shard lock and retries on a change.

When one tree has `TREE_FLUSH` (16,384) dirty entries, a flush on the hot
path applies them to the working root: sort the dirty keys, apply them in one
batch, install the new root, then settle each entry whose ticket is still
current. A flush never runs while a commit is prepared, and holds no shard
lock across a node read or put.

The node cache has a byte cap: the mount option `--meta-cache-size`, default
64 MiB. It is the only RAM that grows with reads; the overlay grows with the
edits since the last flush or commit.

## Commit

1. `begin_commit` waits out a running flush on the apply lock and marks the
   commit prepared, so no flush applies after the cut.
2. The write-log cut.
3. Per tree, the dirty entries are taken (with new tickets) and the cut root
   is read. The inode allocator value is read after the takes.
4. `finish_commit` applies each batch to its cut root (only the changed
   nodes are written), puts the root record with the five new roots,
   `next_ino` and the counts, then under the apply lock installs each new
   root and settles the overlays.
5. The chunk store is synced, the file content (pre-root sync) is synced,
   and the root is logged (`RootUpdate`).

Failure rules:

- A failure before the install, or a dropped unfinished commit, requeues
  every taken entry: nothing is lost, the next commit writes it.
- After the install the edits are in the working roots: a failure of a
  later step loses nothing, and the next commit syncs and logs them.
- A crash before the root is logged leaves the last logged root; the write
  log replays the requests after it.

`last_commit_stats` gives the nodes read and written per tree for the last
commit.

## Readdir

Readdir is a range scan of the directory prefix in name byte order, merged
with the pending edits of the directory, and streamed into the reply: the
whole directory is never listed into RAM.

Each open directory handle keeps a resume cursor: the offset and the name of
each entry of its last reply. The kernel asks for the next reply at the
offset of the last entry it used, so the scan resumes just after that name
(the name followed by a 0 byte, which need not exist any more). A full
listing reads each entry once, and a name that is created or removed
between two replies does not move the other entries.

Known cost: any other offset above 0 (for example a seekdir to an older
offset) is an entry index: the scan starts at the directory start and skips
that many entries. If the directory changed since that offset was given, an entry can
be skipped or repeated. Memory per open handle is one reply of names.

## Xattrs

getxattr, setxattr and removexattr touch one entry (O(tree height) nodes);
listxattr is a range scan of the inode prefix, in name byte order. A
positioned read of a spilled value (the macOS resource fork) reads only the
chunks of its range.

Known gap: setxattr ignores `XATTR_CREATE` and `XATTR_REPLACE`. A set is a
blind put of the entry, so "create only" and "replace only" are not checked
and not atomic.

## GC mark

The mark walks each tree of the live root and of each snapshot root with a
stack of one node path per tree. The node IDs of a tree go straight to the
live set. The leaves give the values they refer to: the manifests of
`manifest_data` and the content they list, and the spilled values of
`xattr_data`. These keys go to an external sorter (a key that many roots
give is expanded once), and each sorted key is then expanded to its
objects. The leaf keys of `refcounts` are not live references. Every snapshot root is walked in full:
N snapshots cost up to N times the node count of one root. This cost is
accepted; skipping shared subtrees is later work.

Measured in plan 26-11 (debug build, macOS):

| Measure | Value |
|---------|-------|
| Tree nodes the mark visits, 3,000 files, 1 root | 89 |
| Same store, 1 root and 3 snapshots that share almost all nodes | 356 (95 distinct) |
| Peak heap of a full mark, 50,000 files | 2,086,967 bytes |
| Peak heap of a full mark, 200,000 files | 2,641,095 bytes |

The walk of 4 roots visits 4 x the nodes of one root, although they have
only 6 more distinct nodes (`snapshot_roots_cost_one_walk_each`). This
full walk per root is the accepted limit. The peak heap is measured with
sort chunks of 4,096 keys, so the keys of both stores spill to temp files
(`gc_prolly_memory_tests`): 4 x the map adds about 0.55 MB, not the map.

## Rehash

`slicefs rehash` rebuilds every tree under the target hash: a node holds
the IDs of its children, so every node ID changes. Each old tree is read in
key order with a cursor, the values that hold keys are translated (a
manifest key, a spilled xattr value), and the entries go into a
`prolly::Builder` with the store's boundary salt. The refcount rows get new
content keys, so they are sorted by the new key first.

## Known O(files) memory outside the maps

`slicefs stats` (`content_stats` in `crates/slicefs-cli/src/stats.rs`)
keeps a set of the inodes it has seen and a map by content. This memory
grows with the file count. It was there before Phase 26 and is not changed
by it.

## Metadata maps: prolly-tree decision

Status: history. This was the recommendation of Phase 24 (#19),
2026-11-26, confirmed by a second run after the fixes of the code review
(with real inserts and deletes in the middle of the map, and the disk sizes
with the pack index). The owner then adopted the prolly maps (Q-1, issue
#215); they are in the store since Phase 26.

Recommendation of Phase 24: **later**.

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
store format 8. Since Phase 26 the store keeps all five maps in its trees
(issue #215).
