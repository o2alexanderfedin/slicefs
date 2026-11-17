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
- Object ID: BLAKE3 of the uncompressed bytes, 224 bits (28 bytes, the size
  of the `Digest224` that the metadata already uses, so the metadata types
  do not change; collision strength 112 bits, the same as the SHA-224 used
  now). Dedup does not depend on compression.
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

Both kinds are content-addressed by BLAKE3-224 (28 bytes, type `ObjectId`).
A chunk and a node with the same bytes are the same object; the kind is not
part of the ID.

### File manifest

A file is an ordered list of `(ObjectId, length u64)` entries (36 bytes
each). The file offset of an entry is the sum of the lengths before it.

- Up to 1,024 entries: the list is one manifest node.
- More entries: the list is split into leaf nodes of 1,024 entries, and an
  index node holds `(ObjectId of the leaf, byte length of the leaf range)`.
  The tree has as many levels as it needs. A read finds its range with a
  binary search on the lengths at each level.
- The size of the file is in the inode. An empty file has no manifest.

The other metadata (directories, inode map, xattrs) keeps its current
logical layout. Only the storage of its nodes moves from `State::push_all`
into node objects. Roots and snapshots stay 28 bytes in the `RootUpdate` and
`Snapshot` records.

## Pack files

```
store/
  packs/
    00000001.pack      sealed (immutable)
    00000001.idx       index of a sealed pack
    00000002.pack      active (append-only)
  segments/            metadata log (unchanged)
```

Pack header: magic `SFPK`, format version u16, pack number u64.

Record:

| Field | Size |
|---|---|
| ObjectId | 28 |
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
52 bytes per object, so 52 MB for one million objects. At open, it is filled
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
data thus costs about one header per chunk (45 bytes for 8 KiB, 0.5 %).

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

A failed sync is final. On Linux, the kernel can mark the pages clean after
a failed `fsync`, so a retry can succeed without the data. After any failed
sync, the store refuses every write and sync until it is opened again. The
open scan then finds the true durable state.

## Storage strategies

The store does every file operation through the `PackIo` trait
(`pack_io.rs`): list, read, durable write, remove, create, open, and
directory sync. The open handles are `PackFile` (the active pack) and
`PackRead` (reads). `ChunkStore::open_with` takes the strategy.

- `FileIo`: real files with `pread` and `pwrite`. `ChunkStore::open` uses it.
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

Mark: the same as today, from the current root and each snapshot, through
the metadata nodes and the manifests, into a set of live `ObjectId`s.

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
store change: bytes of a file → FastCDC → for each chunk, BLAKE3 → dedup
check → append to the active pack → manifest entry. Out-of-order and sparse
writes keep the current handling until #20 part 2.

## Plan (one PR each)

1. `chunk-store` crate: pack writer and reader, index, compression, crash
   scan, with unit and crash tests. No user of it yet.
2. File data: FastCDC + BLAKE3 + packs for file content and manifests. New
   store format version; old stores are rejected.
3. Metadata nodes into packs; roots become `ObjectId`.
4. GC: pack deletion and compaction, offline and online.
5. Remove blockset and the `data-id` submodule.

Each step keeps all tests green, and steps 2 and 4 run the macOS Boost round
trip.

## Measurements to take

- `slicefs seed` throughput and on-disk size (Boost tree and tar, random
  data): before (blockset) and after each of steps 2 to 4.
- Boost round trip on FSKit: time per step, store size, `fsync` and `write`
  time per call.
- Number of files in the store after a Boost copy (target: tens, not
  hundreds of thousands).
