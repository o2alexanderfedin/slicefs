# Limits of a SliceFS store

This page lists the size and count limits of a SliceFS store and the code
that sets each one. It is the design source for the user page
`docs/user/limits.md` (milestone v2.3, issue #158).

Status: the numbers are true for store format 8 with the prolly metadata
maps of Phase 26 (#215). The five metadata maps are prolly trees; none is
held whole in RAM.

## Summary

| Item | Limit | Set by |
| --- | --- | --- |
| Size of one file | 1 TiB (2^40 bytes); a larger write or truncate fails with `EFBIG` | `MAX_FILE_SIZE` in `crates/slicefs-cli/src/filesystem.rs` |
| Inodes created in one store | 2^64 (inode numbers are `u64`, only go up, never reused) | `InodeAlloc` in `crates/metadata/src/inode_alloc.rs` |
| Files and directories in one store (design target) | 1,000,000,000 or more, no map whole in RAM | `docs/design/store-format.md`, "Metadata maps (store format 8)" |
| Entries in one directory | no limit (one tree entry per name) | `dir_data` key grammar |
| Length of one name | 255 bytes | `NAME_MAX` in `crates/metadata/src/store.rs` |
| Depth checked by a rename | 2^20 ancestor levels | `MAX_ANCESTOR_STEPS` in `crates/slicefs-cli/src/filesystem.rs` |
| Live roots per store | 1 (one mount per store at a time) | mount lock |
| Snapshots per store | no fixed limit; see "Snapshots" below | `crates/metadata/src/snapshot.rs` |
| Stores | no limit (each store is its own folder) | — |
| Prolly tree entry (key and value) | 2,048 bytes | `MAX_ENTRY` in `crates/prolly/src/lib.rs` |
| Prolly key | 1,024 bytes | `MAX_KEY` |
| Prolly node | 16 KiB when written; readers accept up to 32 KiB | `NODE_MAX`, `MAX_NODE_MAX` |
| Root record | 197 bytes | `crates/metadata/src/store.rs` |
| Store header (`FORMAT` file) | 64 KiB | `MAX_HEADER` in `crates/slicefs-cli/src/header.rs` |
| Length of one xattr name | 1 to 255 bytes | `XATTR_NAME_MAX` in `crates/metadata/src/xattr.rs` |
| Xattrs per file, and their total size | no limit; memory per call is bounded | "Xattrs" below |

## Files

- A file can be at most 1 TiB. The limit is checked before a write or a
  truncate: `offset + length` must fit in `u64` and be at most
  `MAX_FILE_SIZE`, else the call fails with `EFBIG`. A Kani proof covers the
  check on all inputs.
- The content of a file is cut into chunks with FastCDC (default 4/8/32 KiB:
  minimum, average, maximum). The chunk list of a file (its manifest) is a
  content-cut tree, so the daemon memory per file stays bounded at any file
  size (#98).
- The manifest grows with the file at about 33 bytes per chunk. With the
  default average chunk of 8 KiB, a 1 TiB file has about 4 GB of manifest
  nodes. They are read and written node by node.

## Files and directories in a store

- Inode numbers are `u64`. The allocator only goes up and never reuses a
  number, so a store can create about 1.8 x 10^19 inodes in its life.
- The design target is 1,000,000,000 files and directories or more in one
  store. No operation loads a whole metadata map into RAM: not mount, not
  stats, not GC, not scrub. Counts come from the counts in the internal tree
  entries or from the root record.
- At 1,000,000,000 entries the prolly trees are 4 to 5 levels high (layout a,
  L = 8,192, NODE_MAX = 16 KiB). A change of one entry reads and writes about
  one path of nodes per changed tree.
- Directory entries are stored by name: the key is the directory inode
  number and the entry name. One directory can have any number of entries,
  and one change in a directory touches one path of nodes. `.` and `..` are
  stored as entries. Readdir is a range scan in name byte order.
- **Readdir paging:** each open directory handle keeps a resume cursor: the
  offset and the name of each entry of its last reply. The next readdir at
  one of those offsets resumes the range scan after that name. So a full
  listing reads each entry once, and a name that is created or removed
  between two replies does not make another entry come twice or not at all.
- **Cost to note:** an offset other than 0 that is not in the last reply of
  the handle (for example a seekdir to an older offset) is an entry index:
  the scan starts at the directory start and skips that many entries. If the
  directory changed since that offset was given, an entry can be skipped or
  repeated. Memory stays bounded: one reply of names per open handle.

## Roots and snapshots

- A store has one live root. One mount at a time can use a store.
- Each snapshot is one more root, with a `u64` version number. A snapshot
  costs one root record; the tree nodes are shared with the live root until
  they change.
- **Limit to note:** the snapshot list is held whole in RAM
  (`Vec<SnapshotEntry>` in `crates/metadata/src/store.rs`) and is kept in the
  write-log segments. Thousands of snapshots are no problem. Millions would
  break the rule that no list grows without limit in RAM. A snapshot list as a
  prolly tree is later work.
- GC walks every snapshot root in full. With N snapshots the GC mark visits
  up to N times the node count of one tree; shared subtrees are not skipped
  yet (owner decision of Phase 26; skipping them is later work).
- One namespace over many stores (a global root) does not exist yet.

## Metadata entries

Store format 8 keeps five metadata maps, each a prolly tree (layout a):

| Map | Key | Value |
| --- | --- | --- |
| `inode_data` | inode number (8 bytes, big endian) | the inode record, inline (56 or 72 bytes) |
| `dir_data` | directory inode number + entry name | child inode number + file type (9 bytes) |
| `manifest_data` | inode number | 33-byte `ObjectKey` of the file's manifest |
| `xattr_data` | inode number + xattr name | tag byte + inline value or `ObjectKey` of a spilled value |
| `refcounts` | 33-byte content key | 8-byte count |

Every entry (key and value) is at most 2,048 bytes, and every key at most
1,024 bytes. A value that does not fit in one entry is not put in the tree:
the tree holds the key of an object that holds the value.

## Xattrs

Each xattr is its own entry in `xattr_data` (owner decision of Phase 26,
2026-11-27):

- Key: the inode number (8 bytes, big endian) and then the xattr name.
- Value: one tag byte, then either the value bytes (when key and value fit in
  one entry) or the `ObjectKey` of a spilled value.
- A spilled value is stored like file content: chunks and a manifest. It is
  read and written in parts, never whole in RAM.

What this gives:

- `getxattr`, `setxattr` and `removexattr` touch one entry, one path of nodes,
  for any number of xattrs on the file.
- `listxattr` is a range scan over the entries of one inode, read node by
  node.
- A read with an offset (on macOS, for example, the resource fork) reads only
  the chunks it needs.
- Memory per xattr call is bounded by that one call, not by the number or the
  total size of the file's xattrs. So SliceFS needs no limit of its own on
  xattrs for memory safety. On Linux the kernel limits one value to 64 KiB and
  one name list to 64 KiB.

This replaces the earlier design, where all xattrs of a file were one
serialized set, inline or spilled as one object. With that design a file with
many or large xattrs had to be read and written whole in RAM.

Known gap: setxattr ignores the flags `XATTR_CREATE` and `XATTR_REPLACE`. A
set always writes the entry, so "create only" and "replace only" are not
checked and not atomic.
