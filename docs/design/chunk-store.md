# Chunk store: FastCDC + BLAKE3 in pack files (#15, #8, #10, #4)

Status: design. It replaces the blockset store (`data-id`, one file per
object in `vt0/`). There is no migration: the store format can change
without backward compatibility (no deployments yet). A store in the old
format is rejected at open with a clear message.

## Why

Measured on the macOS CI round trip (Boost 1.85, 77,655 files, FSKit):

- `write` takes 4.6 ms and `fsync` 3.35 ms per call in SliceFS. A 28 KB file
  needs about 7 objects of about 4 KB. Each object is a temporary file, a
  write, a rename, and at `fsync` an `fsync` of each file. Together these
  calls take about one third of the copy time; FSKit takes the rest.
- One file per object: APFS uses 4 KiB units, so a 4,206-byte object uses
  8 KiB (+50 % on disk). `vt0/` holds more than 100,000 files.
- blockset chunks at 3.4 MB/s of CPU. FastCDC 8 KiB + BLAKE3 does 530 to
  910 MB/s, and it stores less data on all three Boost series (#17).
- Objects are not compressed (#10). On random data the format uses about
  1.55 bytes for each byte of file data.

## Goals

- Chunking: FastCDC v2020, average 8 KiB (min 4 KiB, max 32 KiB). The minimum is one
  4 KiB disk block; min 2 KiB stored less than 1 % less data on source trees and
  tars, with 5-9 % more chunks.
- Object ID: a hash tag byte, then the hash of the uncompressed bytes, the
  full 256 bits (33 bytes in all, the size of the `ObjectKey` that the
  metadata uses; collision strength 128 bits). The default hash is BLAKE3.
  The store is meant to be global and decentralized, so the IDs keep all
  bits of the hash and name the hash they use (see "Object IDs and the
  hash"). Dedup does not depend on compression.
- Objects go into append-only pack files: one `fsync` for each pack and
  commit, not one for each object.
- Compression: zstd, per object, only when it makes the object smaller.
- Crash safety with the write log (`docs/design/write-log.md`): a root never
  refers to an object that is not durable.
- Online GC deletes and compacts packs.

Not in scope: overlays and re-chunk at commit (#20 part 2), fine-grained
locks (#21), and prolly trees for metadata (#19). The format leaves room for
them.

## Objects

| Kind | Content | Made by |
|---|---|---|
| chunk | file data, one FastCDC chunk | file writes |
| node | serialized metadata: manifest, directory, inode map, xattr, inode | commit |

Both kinds are content-addressed by a tagged hash (33 bytes, type `ObjectId`).
A chunk and a node with the same bytes are the same object; the kind is not
part of the ID.

### File manifest

A file is an ordered list of `(ObjectId, length u64)` entries (41 bytes
each). The file offset of an entry is the sum of the lengths before it.

- The list is split into nodes by content, like the chunks: a node ends
  after an entry whose ID matches a bit pattern (1 entry in 512), with at
  least 64 and at most 4,096 entries. An index node holds `(ObjectId of the
  node, byte length of its range)`, cut the same way. The tree has as many
  levels as it needs. A read finds its range with a binary search on the
  lengths at each level.
- Why by content: with a fixed count per node, a chunk added or removed
  near the start moves every later entry, so every later node is stored
  again (about 0.5 % of the file size). With content cuts, the nodes line up
  again after the next cut. In a test with 200,000 chunks (379 nodes), one
  entry added or removed at the start gave 2 new nodes: the first leaf and
  the root.
- The size of the file is in the inode. An empty file has no manifest.

The other metadata (directories, inode map, xattrs) keeps its current
logical layout. Only the storage of its nodes moved from `State::push_all`
into the chunk store (`metadata::store_io`, step 3). A node is stored like
a file: FastCDC chunks under a manifest. The maps (inode, directory,
manifest, xattr, refcount) are written whole at each commit, so the chunks
matter: after a change of one file in 5,000, the next commit stored 12 KB,
against 1.36 MB for the first commit (`tests/node_crash_tests.rs`). Roots and
snapshots are 33-byte keys in the `RootUpdate` and `Snapshot` records.

The metadata and the file content share one `ChunkStore` instance: a commit
syncs the active pack once, then logs the root. A directory entry key is
BLAKE3-256 of the name (no object is stored for it). The store format is 6;
a store of format 5 (224-bit object IDs) or 4 (metadata in `vt0/`) is
refused.

## Object IDs and the hash

```
ObjectId = tag (1 byte) | digest (32 bytes)
```

- The tag names the hash: `0x1e` is BLAKE3-256 (the multihash code of
  BLAKE3). Every hash gives 32 bytes; there are no variable-length digests.
  The metadata key (`ObjectKey`) holds the same 33 bytes, so manifest
  entries and roots name their hash too, and a peer can verify the content
  that a key refers to.
- A hash is an `ObjectHasher` (`chunk_store::hash`: name, tag, digest). The
  build knows a fixed list of them (`hasher_by_name`, `hasher_by_tag`).
- Write hash: the store makes the IDs of new objects with one hasher,
  injected at open (`Options::hasher`, default BLAKE3-256), as the zstd
  level is. The store records it in its format marker, as a second line
  `hash blake3-256` (a marker without that line means blake3-256).
  `slicefs mount --hash <name>` and `slicefs seed --hash <name>` change it:
  the new name is recorded and new objects use it. Without `--hash`, the
  recorded hash stays. An unknown name is refused with the list of the
  known ones.
- Verification: each object is verified with the hasher of its own tag
  (reads, `scrub`, the GC mark, the open scan of a pack without its index).
  So after a change of the write hash, the old objects stay readable and
  verifiable. A tag that the build does not know is an error (a damaged or
  newer store), never a panic.
- The same bytes under two hashes are two objects: there is no dedup across
  hashes. A conversion of a whole store to a new hash is future work
  (#62, `slicefs rehash`).
- Keys that are not content addresses do not change with the write hash: a
  directory entry key is always BLAKE3-256 of the name, so lookups keep
  working for the life of the store.
- The cut of the manifest nodes (`is_cut`) looks at the last 4 digest bytes,
  never at the tag.

## Pack files

```
store/
  packs/
    00000001.pack      sealed (immutable)
    00000001.idx       index of a sealed pack
    00000002.pack      active (append-only)
  segments/            metadata log (unchanged)
```

Pack header: magic `SFPK`, format version u16 (2: tagged 33-byte IDs; a
pack or index of another version is refused at open), pack number u64.

Record:

| Field | Size |
|---|---|
| ObjectId (tag + digest) | 33 |
| flags (bit 0: zstd) | u8 |
| raw length | u32 LE |
| stored length | u32 LE |
| payload | stored length |
| CRC32C over the fields above and the payload | u32 LE |

- One active pack at a time. A pack is sealed when it reaches 64 MiB, and at
  unmount.
- At seal: `fsync` the pack, write `NNNNNNNN.idx` (sorted
  `(ObjectId, offset, stored length, raw length, flags)`, CRC at the end),
  `fsync` the index and the directory.
- Records are written with `write` on a buffered file handle. Readers use
  `pread`; a sealed pack needs no lock.

### Index in memory

A map `ObjectId → (pack, offset, stored length, raw length, flags)`: about
57 bytes per object, so 57 MB for one million objects. At open, it is filled
from the `.idx` files of the sealed packs and from a scan of the active
pack. A later change can use a memory-mapped sorted index if stores get very
large.

### Dedup

Before an object is written, its ID is looked up in the index. If the
object is there, it is not written again (writes are idempotent; no lock is
needed for correctness).

## Compression

zstd level 3, per object. The compressed form is kept only if it is at least
1/8 smaller; otherwise the raw bytes are stored (flag bit 0 = 0). Random
data thus costs about one record header and CRC per chunk (46 bytes for
8 KiB, 0.6 %).

## Durability and crash safety

The commit order from the write log stays, with packs in place of object
files:

1. Write the new chunks and nodes into the active pack.
2. `fsync` the active pack (and any pack sealed since the last commit, with
   its index).
3. Append `RootUpdate` to the metadata log and sync it.

So a root never refers to an object that is not durable.

`fsync` of a file: today it makes the file's objects durable. With packs it
syncs the active pack once (all files written since the last sync are
covered), then the write log. A second `fsync` with no new data returns
without a disk flush.

At open:

- Sealed pack without `.idx` (crash during seal): scan the pack, and write
  the index.
- Active pack: scan records until the first record with a bad CRC or a short
  read, and cut the pack there. The objects after that point were not
  synced, so no root refers to them. Write-log replay writes them again.

A failed sync is retried, but not as a plain second `fsync`. On Linux, the
kernel can mark the pages clean after a failed `fsync`, so a plain retry can
succeed without the data. The decorator `RetryingIo` (`retry.rs`) keeps
every write since the last good sync of a pack. When a sync fails, it writes
them again and then syncs, after 10 ms, 100 ms and 500 ms. A durable write
of a whole file is retried as a whole. The mount injects it over `FileIo`
(`content::storage()`); the store does not know about it.

If the sync still fails after the retries, it is final: the store refuses
every write and sync until it is opened again. The open scan then finds the
true durable state.

## Storage strategies

The store does every file operation through the `PackIo` trait
(`pack_io.rs`): list, read, durable write, remove, create, open, and
directory sync. The open handles are `PackFile` (the active pack) and
`PackRead` (reads). `ChunkStore::open_with` takes the strategy.

- `FileIo`: real files with `pread` and `pwrite`. `ChunkStore::open` uses it
  inside `RetryingIo`.
- `MemIo` (`mem.rs`): an in-memory mock for tests. It can inject `ENOSPC`,
  `EIO` on reads, and failed syncs, and `crash()` keeps only the durable
  bytes.
- Other read or write paths (for example `mmap` reads, see
  `docs/research/arena-spikes.md`) can be added as new strategies.

The strategies do not behave the same in every case. For example, with
`mmap` a disk error is `SIGBUS`, not `EIO`. So the contract tests in
`tests/io_tests.rs` run on each strategy. Known differences must be written
down next to the strategy.

## GC

Mark (`metadata::gc::live_objects`): from the current root and each
snapshot, through the metadata nodes and the manifests, into one set of live
`ObjectId`s: the objects of every node and of every file content.

`gc_begin` seals the active pack, so every object so far is in a sealed
pack. Sweep (`gc_finish`), for each sealed pack up to that point:

- No live objects: delete the pack and its index.
- Less than half of its bytes live, or smaller than 1/8 of the pack limit:
  copy the live records (as they are, no re-compression) into the active
  pack, sync it, update the index, then delete the old pack.
- Otherwise, if it has dead records: punch holes (`F_PUNCHHOLE` on macOS,
  `fallocate(PUNCH_HOLE)` on Linux) over the whole 4 KiB blocks of the gaps
  between live records. Spike D measured GC write amplification 1.02 with
  punching against about 1.6 with compaction only
  (`docs/research/arena-spikes.md`).

Punch order, so that a crash never leaves an index entry over zeros:

1. Drop the dead records in the punched ranges from the index, and write
   the pack's `.idx` durably without them. A dead record with no whole block
   stays in the index; a later GC punches it when a neighbour dies.
2. Set the "punched" bit in the pack header and sync the pack.
3. Punch.

A punched pack is never the active pack and is never cut at open. If its
index is lost, the open resyncs it: it takes a record only where the CRC is
good and the bytes hash to the ID, and skips holes and damage. Every sealed
pack without a valid index is resynced this way; only the active pack is
cut at its first bad record.

Not every file system can punch holes (for example exFAT). The storage
strategy says so (`PackIo::can_punch`; `FileIo` probes it once with a small
file), and the store asks once, at open. Without holes, GC keeps the dead
records of a mostly live pack in the index, so that a later GC still counts
their bytes and compacts the pack when it is mostly dead. The mount prints
which way GC frees space. The mount (`content::open`) chooses the strategy
and injects it into the store.

The small-pack rule merges the packs that each `gc_begin` seals; it rewrites
at most 1/8 of the pack limit per GC.

Online GC: from `gc_begin` to `gc_finish`, the store records every object
that is put or read, and keeps them even if the mark did not find them. (A
put of bytes that are already stored returns at once; without this record,
the sweep could delete an object that a new file uses.) A pack that a reader has
open stays readable after its file is deleted (POSIX), so readers need no
lock against the GC; they only need the index entry to change atomically.

## Read path

Manifest → `ObjectId` → index → `pread` → zstd decode if flagged → bytes. A
small LRU cache of decoded chunks serves reads that do not align with chunks.

## Write path

The current streaming writer keeps its role; only the chunker and the object
store change: bytes of a file → FastCDC → for each chunk, the write hash → dedup
check → append to the active pack → manifest entry. Out-of-order and sparse
writes keep the current handling until #20 part 2.

## Plan (one PR each)

1. `chunk-store` crate: pack writer and reader, index, compression, crash
   scan, with unit and crash tests. No user of it yet.
2. File data: FastCDC + BLAKE3 + packs for file content and manifests. New
   store format version; old stores are rejected.
3. Metadata nodes into packs (done; roots are `ObjectKey`, the same 33
   bytes as an `ObjectId`).
4. GC: pack deletion and compaction, offline and online.
5. Remove blockset and the `data-id` submodule (done).

Each step keeps all tests green, and steps 2 and 4 run the macOS Boost round
trip.

## Measurements to take

- `slicefs seed` throughput and on-disk size (Boost tree and tar, random
  data): before (blockset) and after each of steps 2 to 4.
- Boost round trip on FSKit: time per step, store size, `fsync` and `write`
  time per call.
- Number of files in the store after a Boost copy (target: tens, not
  hundreds of thousands).

### Results (2026-11-18)

Machine: Intel(R) Xeon(R) W-3245 CPU @ 3.20GHz, 16 physical cores, 192 GiB
RAM, macOS 26.2, APFS on an SSD.

Builds, all with rustc 1.95.0-nightly (9e79395f9 2026-02-10) and no local
patch:

- c2f82fa: before packs. Each object is one file in `vt0/`, written by
  blockset. This is the merge of #44.
- 7016bb6: file content in packs, metadata nodes still in `vt0/`. This is the
  first parent of the #56 merge e038e9f.
- c447551: develop. All objects are in packs.

Method: `slicefs seed` of each input into a new store, with a warm page cache.
Each number is the median of 3 runs. Seconds are wall time from
`/usr/bin/time -l`. Peak RSS is its maximum resident set size. Sizes come from
`find`, `stat` and `du` on the store dir. MB = 10^6 bytes. Other work ran on
the machine during this run: the 1-minute load average was 52 to 137 on 32
logical cores. Thus the times are only an indication, and the 3 runs of one
input differ by up to 32 %. Load does not change the sizes and the file
counts.

Boost 1_85_0 tree: 77,655 files, 757.5 MB.

| build | seconds | MB/s | apparent MB | allocated MB | files | peak RSS MB |
|---|---|---|---|---|---|---|
| c2f82fa | 634.64 | 1.2 | 910.9 | 2,194.2 | 426,739 (vt0/ 426,738, segments/ 1) | 257.3 |
| 7016bb6 | 189.32 | 4.0 | 255.3 | 936.3 | 170,376 (vt0/ 170,366, packs/ 8, segments/ 1, other 1) | 365.8 |
| c447551 | 26.74 | 28.3 | 288.6 | 290.0 | 10 (packs/ 8, segments/ 1, other 1) | 533.2 |

Boost 1_85_0 tar: 1 file, 823.6 MB.

| build | seconds | MB/s | apparent MB | allocated MB | files | peak RSS MB |
|---|---|---|---|---|---|---|
| c2f82fa | 617.04 | 1.3 | 925.5 | 1,381.1 | 217,099 (vt0/ 217,098, segments/ 1) | 889.1 |
| 7016bb6 | 12.28 | 67.1 | 199.2 | 200.7 | 21 (vt0/ 13, packs/ 6, segments/ 1, other 1) | 1,818.0 |
| c447551 | 9.22 | 89.3 | 200.1 | 218.2 | 8 (packs/ 6, segments/ 1, other 1) | 1,810.8 |

Random data: 1 file, 1,073.7 MB.

| build | seconds | MB/s | apparent MB | allocated MB | files | peak RSS MB |
|---|---|---|---|---|---|---|
| c2f82fa | 854.61 | 1.3 | 1,670.5 | 2,514.3 | 318,112 (vt0/ 318,111, segments/ 1) | 1,150.1 |
| 7016bb6 | 14.17 | 75.8 | 1,085.2 | 1,085.3 | 49 (vt0/ 13, packs/ 34, segments/ 1, other 1) | 2,409.5 |
| c447551 | 10.49 | 102.4 | 1,086.6 | 1,086.7 | 36 (packs/ 34, segments/ 1, other 1) | 2,411.5 |

Store files after a seed of the Boost tree: c2f82fa 426,739, 7016bb6 170,376,
c447551 10. The current build meets the target "tens, not hundreds of
thousands": its store has 10 files.

Allocation cost (#8): for the Boost tree, the c2f82fa store allocates 2.41
bytes on disk for each byte of store data (2,194.2 MB for 910.9 MB). The
c447551 store allocates 1.00 bytes for each byte (290.0 MB for 288.6 MB).

The FSKit Boost round trip (`scripts/macos-mount-test/boost-test.sh`) is a
manual run, and nothing was mounted for these numbers. Add its numbers here
when that run is done: time per step, `fsync` and `write` time per call, and
the store file count after that copy.

To repeat: run `scripts/macos-mount-test/fetch-boost.sh <DIR> 1_85_0`, then
`scripts/pack-measure/seed-measure.sh <BINARY> <DIR> 3` for each build (see
`scripts/pack-measure/README.md`).
