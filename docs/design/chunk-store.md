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

A `HashMap` from `ObjectId` (33 bytes) to `Loc` (pack, offset, stored
length, raw length, flags: 25 bytes, padded to 32). One slot of the table
is 72 bytes plus one control byte. The table size goes up in powers of two
(slots = the next power of two above 8/7 of the object count), so the RAM
for each object changes with the object count. The live table is 153 bytes
for each object at 1M objects, 122 at 10M and 98 at 100M. During
the last resize, the old and the new table are in memory together: the
measured peak at 100M is 154 bytes for each object (15.4 GB, macOS). On
macOS, the RSS after open is higher than the live table (390, 196 and 106
bytes for each object at 1M, 10M and 100M), because memory that the open
frees stays in the RSS of the process. See "Index baseline (#66)".

At open, the map is filled from the `.idx` files of the sealed packs and
from a scan of the active pack. The map is not sized before the fill, so it
grows by resizes. Issue #85 replaces this map with an on-disk index. The
baseline for that change is in "Index baseline (#66)".

### Index baseline (#66)

Method: the tool `benchmarks/index-bench` (see
`benchmarks/index-bench/README.md`). Object `i` is the 8 bytes of `i`, so
zstd does not run. The store uses the default `Options` (64 MiB packs). Each
store is made one time and closed, so every pack has an `.idx`. Each open
runs in a new process. The page cache is warm: one warm-up open runs
before the measured opens. Each value is the median of 3 runs.

macOS: Intel Xeon W-3245 (16 cores, 32 threads), 192 GiB RAM, macOS 26.2
(x86_64), APFS on an internal PCIe SSD, commit `321a443`, warm cache.

All timings were measured under load: other work on the same host kept the
load average between about 80 and 140 during the runs, and the swap use of
the host went from 7.5 GB to 9.2 GB during the 100M run. The wall times
are thus noisy. The CPU times are more stable.

| OS | Objects | Load before (1, 5, 15 min) | Load after (1, 5, 15 min) |
|---|---|---|---|
| macOS | 1M | 138.96 106.46 99.40 | 129.94 109.14 100.78 |
| macOS | 10M | 122.01 112.08 103.35 | 125.97 125.58 113.82 |
| macOS | 100M | 125.97 125.58 113.82 | 79.14 80.73 92.59 |

Store:

| OS | Objects | Packs | Disk B/object | Create puts/s | Creator peak RSS MB |
|---|---|---|---|---|---|
| macOS | 1M | 1 | 104 | 68240.129 | 497.734 |
| macOS | 10M | 9 | 104 | 64026.099 | 2803.016 |
| macOS | 100M | 81 | 104 | 72219.59 | 16323.326 |

Open:

| OS | Objects | Open s (median) | Open s (min-max) | Open CPU s (median) | Peak RSS B/object (median) | Steady RSS B/object (median) | Cache |
|---|---|---|---|---|---|---|---|
| macOS | 1M | 0.964 | 0.953-1.007 | 0.591 | 390.185 | 390.185 | warm |
| macOS | 10M | 12.844 | 10.981-16.072 | 5.881 | 256.312 | 195.792 | warm |
| macOS | 100M | 141.241 | 134.217-151.394 | 62.239 | 154.232 | 105.533 | warm |

Lookups (one thread):

| OS | Objects | contains hit ns | contains miss ns | put dedup-hit ns | contains hit/s | contains miss/s |
|---|---|---|---|---|---|---|
| macOS | 1M | 1304.791 | 727.474 | 1284.086 | 766405.984 | 1374619.105 |
| macOS | 10M | 700.501 | 323.994 | 945.17 | 1427549.444 | 3086474.614 |
| macOS | 100M | 819.545 | 419.842 | 974.533 | 1220188.607 | 2381846.106 |

Recovery (the columns up to "Scan objects/s" are the output of the tool;
the last two columns are added, see the notes):

| OS | Objects | Scope | Mode | Sealed/active packs | Scanned MB | Scanned objects | Recovery open s (median) | Extra s | Scan MB/s | Scan objects/s | Extra CPU s | Scan MB/s (CPU) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| macOS | 1M | one | rw | 0/1 | 54 | 1000000 | 1.154 | 0.19 | 284.211 | 5263157.895 | 0.064 | 844 |
| macOS | 1M | one | ro | 0/1 | 54 | 1000000 | 1.296 | 0.332 | 162.651 | 3012048.193 | 0.066 | 818 |
| macOS | 1M | all | rw | 0/1 | 54 | 1000000 | 0.985 | 0.021 | 2571.429 | 47619047.619 | 0.032 | 1688 |
| macOS | 1M | all | ro | 0/1 | 54 | 1000000 | 1.317 | 0.353 | 152.975 | 2832861.19 | 0.045 | 1200 |
| macOS | 10M | one | rw | 1/0 | 67.109 | 1242757 | 13.683 | 0.839 | 79.987 | 1481235.995 | 1.387 | 48 |
| macOS | 10M | one | ro | 1/0 | 67.109 | 1242757 | 10.082 | -2.762 | n/a | n/a | 0.177 | 379 |
| macOS | 10M | all | rw | 8/1 | 540 | 10000000 | 31.755 | 18.911 | 28.555 | 528792.766 | 11.169 | 48 |
| macOS | 10M | all | ro | 8/1 | 540 | 10000000 | 11.468 | -1.376 | n/a | n/a | 1.911 | 283 |
| macOS | 100M | one | rw | 1/0 | 67.109 | 1242757 | 109.971 | -31.27 | n/a | n/a | 2.232 | 30 |
| macOS | 100M | one | ro | 1/0 | 67.109 | 1242757 | 96.013 | -45.228 | n/a | n/a | -0.739 | n/a |
| macOS | 100M | all | rw | 80/1 | 5400.001 | 100000000 | 292.017 | 150.776 | 35.815 | 663235.528 | 116.469 | 46 |
| macOS | 100M | all | ro | 80/1 | 5400.001 | 100000000 | 120.477 | -20.764 | n/a | n/a | 23.017 | 235 |

Notes:

- Open time grows linearly with the object count: about 0.6 µs of CPU time
  for each object (0.59 s, 5.9 s and 62 s of CPU at 1M, 10M and 100M). The
  wall time is about two times the CPU time because of the load.
- Peak RSS is higher than steady RSS, because the old and the new table are
  in memory together during the last resize. #85 must compare both values.
- On macOS, the RSS after open includes memory that the open freed: the
  smaller tables of the earlier resizes, the `.idx` buffers and the decoded
  entry lists. Thus at 1M the peak and the steady value are the same
  (390 B/object), and the steady values are more than the live table
  (153, 122 and 98 B/object). A test program that fills the same map on
  macOS shows this: 153 B/object at 1M when the map is sized before the
  fill, 306 B/object without that, and 390 B/object with the same
  temporary buffers as the open. With the sizing before the fill, it shows
  122 B/object at 10M.
- The RSS method was checked: at 10M, the maximum resident set size of
  `/usr/bin/time -l` and `max_rss_after_bytes` of the same run are the same
  (2564218880 bytes).
- "Extra s" is the median recovery open time minus the median normal open
  time. Under this load it is negative in many rows, because the load
  changed between the normal opens and the recovery opens, so the tool
  prints `n/a` for the scan rate. The two added columns use CPU time:
  "Extra CPU s" = median `cpu_s` of the recovery opens - median `cpu_s` of
  the normal opens, and "Scan MB/s (CPU)" = Scanned MB / "Extra CPU s". CPU
  time does not include the wait for I/O and `fsync`, so these rates are
  for each CPU second, not for each second of wall time.
- The 1M store has one pack, so its "one" and "all" rows are the
  active-pack path (CRC check only, no hash check, no `.idx` write). Its
  extra CPU time is less than 0.07 s of a 0.6 s open, which is in the
  range of the noise. Thus the 1M scan rates are not useful.
- The "ro" rows show the cost of the scan: at 10M and 100M, the "all ro"
  rows give 235 to 283 MB/s of CPU (4.3M to 5.2M objects for each CPU
  second). The "rw" rows also write a new `.idx` for each sealed pack. This
  adds about 1.2 s of CPU time for each sealed pack, so the "rw" rows give
  only 30 to 48 MB/s. The "one ro" rows at 10M and 100M are in the range
  of the noise (0.18 s and -0.74 s).
- The recovery open does not load the `.idx` files of the packs that it
  scans. Thus the scan rate is a little too high, most in the "all" rows.
- All numbers are for a warm page cache. No cold-cache number was taken:
  on macOS that needs `sudo purge` by hand.

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
