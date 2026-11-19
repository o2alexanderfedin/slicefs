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

### Splice (#98)

`chunk_store::file::splice` stores a changed file as a new root without a
read of the whole file. The inputs are the old root, the length `keep` of
the old bytes that stay, the changed ranges, the new length and a source of
the new bytes. The overlay of an open file (see the write-log note) gives
these inputs at a rebase.

- Restart: FastCDC starts again at the largest old cut strictly below the
  first changed byte. An old cut exactly at the first changed byte is not
  safe: a cut at a byte depends on that byte, so a write that starts there
  can move it. With "at or below", the oracle test gave a wrong key for
  about half of the overwrites and for all appends.
- Resync: after each new chunk end, the splice goes back to copy mode if the
  end is past the changed bytes and is a usable old cut (an old cut below
  `keep`, or the old end when the size and the tail stay). From there, the
  old entries are copied by ID.
- Whole-node reuse: in copy mode, a whole old manifest node that ends before
  the next change is pushed as one entry one level up, so its entries are
  not read one by one.
- Exact zero shortcut: a 32 KiB range that is known to be zeros (past
  `keep`, or old zero chunks) and has no change is pushed as the zero chunk
  without a read or a hash.
- The output key is always the key of a full `FileWriter` write of the same
  bytes (oracle tests). Chunking stays deterministic: the same bytes give
  the same chunks whatever the write history, so there is no forced cut. On long repeating non-zero data, FastCDC finds no old cut to
  resync at, so a rebase can cut again up to the end of the file. The CPU
  then follows the file size, but the memory stays fixed: one piece of at
  most 2 x `CHUNK_MAX` and one decoded node per tree level.

Small splices (debug build, tests in `file.rs` and `write_path_tests.rs`):

| Change | Bytes cut again | Other |
|---|---|---|
| 4 KiB overwrite in the middle of a 32 MiB random file (splice only) | 35,343 | 1,602 entries and 3 nodes reused, 5 new objects |
| 4 KiB write at 16 MiB + 1234 in a 32 MiB random file (rebase of an open file) | 38,355 | 5 new objects, 124,310 more store bytes |
| Extend 1 MiB to 4 GiB with no open handle | 35,628 | 0.14 s (39 s before the splice) |
| Shrink 4 GiB back to 1 MiB | 2,860 | the same key as before |

Docker, Linux libfuse (OrbStack on macOS, release build, the machine was
shared, load average 55 to 93). `scripts/docker-linux-test/run.sh bigwrite`
copies a random file in, then writes 512 MiB of seeded random 1 MiB and
4 KiB blocks into it, then compares it, and again after a remount. Per-op
log, GC off, dirty limit 32 MiB. VmHWM is the peak RSS of the daemon in MiB.

| Run | File | VmHWM after cp | after random write | after cmp | Slowest write | Store bytes |
|---|---|---|---|---|---|---|
| Before (full-file buffer) | 1 GiB | 81 | 6224 | 6224 | 17,295 ms | 1,912,157,485 |
| Overlay, measure only | 1 GiB | 30 | 82 | 117 | 5,418 ms | 2,666,083,776 |
| Overlay, cap 256 MiB | 1 GiB | 30 | 79 | 114 | 5,870 ms | 2,672,373,748 |
| Overlay, cap 256 MiB | 4 GiB | 82 | 161 | 161 | 7,253 ms | 6,168,209,263 |
| Overlay, dirty limit 16 MiB | 1 GiB | 31 | 97 | 97 | 7,778 ms | 2,755,445,492 |
| Overlay, dirty limit 16 MiB | 4 GiB | 84 | 146 | 146 | 5,726 ms | 6,364,447,131 |

- The cap is 256 MiB for both sizes, from a fixed rule on the 1 GiB
  measure-only run (117 MiB is below 170 MiB). All cmp steps passed, and
  each size switched to an overlay once.
- The 4 GiB run uses 47 MiB more than the 1 GiB run. The copy step alone
  has 52 MiB more (82 against 30), so the difference comes before the
  random writes. The probable cause is the copy path and the chunk index of
  the larger file. This is not checked.
- The store holds more bytes than before: each rebase stores new chunks, and
  GC is off in this test, so the old chunks stay. This is not checked.
- The slowest write is above 2 s in every run. With a 16 MiB dirty limit,
  the slowest write of the run was 7,778 ms against 7,253 ms (it was smaller
  at 4 GiB and larger at 1 GiB), so the dirty limit stays 32 MiB. The cause is not
  checked (the load of the machine changed from run to run).

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
  index/               on-disk index (#85), next to packs/
    MANIFEST           live runs, high-water mark, counts
    0000000000000007.run   sorted run (immutable)
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

### Index on disk (#85)

Until #85, the index was a `HashMap` from `ObjectId` to `Loc` in RAM,
filled at open from every `.idx` file: about 100 to 150 bytes of RAM for
each object, and an open time that grew with the object count. The
measured values are in "Index baseline (#66)". The on-disk index replaces
it. The packs and the `.idx` files do not change: they stay the source of
truth, and the index is derived data that can be built again from them.

The index is in the directory `index/` next to `packs/`. It has two parts:
sorted run files on disk, and a memtable in RAM for the newest entries.

Run file (`NNNNNNNNNNNNNNNN.run`, written one time, never changed):

- An entry holds the full ID, the pack, the offset, the stored length,
  the raw length and the flags. A tombstone is an entry with the
  tombstone flag: its pack field names the pack whose entry it hides.
- The entries are sorted by (tag, digest) and are in 4 KiB pages. A page
  holds one tag only. Its 12-byte header has the entry count, the tag, a
  "shared" count, a CRC32C (over the header and the entries), and the byte
  widths of the pack, offset, stored and raw fields of this page (run
  format version 2, see "Index size"). All entries of a page have the same
  size, so a search can go to entry `i` directly. An entry is: the digest
  bytes after the shared bytes (the leading bytes that all keys of the page
  have in common, at most 8; they are the first bytes of the page fence),
  the four fields in the widths of the page, and the flags. The raw field
  takes no byte in a page where each raw length is the stored length.
  The index still holds the full 33-byte ID of each object: no byte of the
  ID is dropped, only bytes that the page header or the fence already give.
- A fence table: one 8-byte key for each page (digest bytes 0..8 of its
  first entry), and a tag table (first page and page count of each tag).
- A blocked Bloom filter over the digests of all entries, 10 bits for each
  key (about 1 % false positives, about 1.25 bytes for each entry). The
  probe positions come from the digest bytes, so the filter does no extra
  hashing.
- A footer with the counts, the offset, length and CRC32C of each table and
  of the filter, and a CRC32C of the footer.

MANIFEST: the list of the live runs (newest first), the runs that it made
obsolete, the high-water mark (hwm), the object counts of the store and a
rebuild flag. It is written with `write_durable` (temporary file, `fsync`,
rename, directory `fsync`). This write is the commit point of every flush,
merge and GC install. The hwm is a (pack, offset): every pack record below
it is in a durable run (or hidden by a durable tombstone).

Memtable: 16 shards by the first bits of the digest, each with its own
lock. New entries (puts, GC copies, the records that the open replays) go
there. The memtable is written to a new run when it has 262,144 entries
(`Options::index_memtable_limit`), and at a GC install.

Open: reads the MANIFEST, the footer, the tag table, the fence table and
the filter of each run (the filter and fences stay in RAM), and maps each
run file read-only. Then it replays into the memtable only the records at
or above the hwm: from the `.idx` of such a pack, or from a scan of the
pack. Packs below the hwm are not opened. Thus the open time grows with
the filter bytes and with the records above the hwm, not with all entries.

Lookup (`contains`, `get`, and the dedup check of `put`): the memtable,
then each run from the newest to the oldest. For a run, the filter (in
RAM) says "not here" or "maybe". For "maybe", a search of the fence table
finds the page, and one page is read from the mapping. The newest entry
wins, and a tombstone stops the search. The page read is done outside the
store lock. `put` and `read` then take the lock, and a generation counter
shows whether a flush, merge or GC install changed the index in between;
if so, they look up again under the lock. (`contains` gives a
point-in-time answer.)

Merges: size-tiered, on one background thread of the store. When 4 runs
of a similar size are next to each other in the list, they are merged into
one. The merge reads its inputs with no store lock and takes the lock only
for the install (one MANIFEST write). The input files are removed only
after that MANIFEST is durable. A merge keeps the tombstones, except when
its output becomes the oldest run.

Idle merge (#111): when the index has had no new entry and no flush, merge
or GC install for 5 s (`Options::index_idle_merge_after`), and it has more
than 2 runs, the same thread merges the newest runs into one. The window
is the longest run of newest runs (at least 2) whose oldest run has at
most 4 times the entries of the newer runs of the window
(`IDLE_MAX_REWRITE`): the largest run is rewritten only together with at
least a quarter of its size of other data. It uses the same write and
install steps as the other merges. See "Idle merge (#111)" for the measurements.

Read-only open (`slicefs stats`, scrub, the format check): it writes
nothing, and keeps the replayed records in an in-RAM memtable. If a run
that the MANIFEST lists is missing (a merge of a mount can replace the
MANIFEST and the runs between two reads), it reads the MANIFEST one more
time.

Damage: a run or MANIFEST that fails its CRC or its size checks, a listed
run file that is missing, or the rebuild flag makes the next writable open
remove the index and build it again from the packs (a read-only open builds
it in RAM). A read error that is not damage (for example `EIO`) fails the
open, so that one transient error does not remove the index. When a record
that the index points to cannot be read, the store checks the run page
with its CRC; a damaged page sets the rebuild flag.

### Index baseline (#66)

Method: the tool `benchmarks/index-bench` (see
`benchmarks/index-bench/README.md`). Object `i` is the 8 bytes of `i`, so
zstd does not run. The store uses the default `Options` (64 MiB packs). Each
store is made one time and closed, so every pack has an `.idx`. Each open
runs in a new process. The page cache is warm: one warm-up open runs
before the measured opens. The Open and Recovery values are the median of
3 runs. The create and lookup steps run one time for each size.

macOS: Intel Xeon W-3245 (16 cores, 32 threads), 192 GiB RAM, macOS 26.2
(x86_64), APFS on an internal PCIe SSD, commit `321a443`, warm cache.

Linux: Docker in the OrbStack VM on the same host, kernel
7.2.12-orbstack-00394-gbecdd6130068, btrfs on the Docker volume, same
commit, warm cache (the container is not privileged, so the page cache
cannot be dropped). The VM had 12.6 GB of memory available (`MemAvailable`
12342996 kB).

100M was not run on Linux: the Docker VM had 12.6 GB of memory available,
and the run needs about 25 GB.

All timings were measured under load. On macOS, other work on the same
host kept the load average between about 80 and 140 during the runs, and
the swap use of the host went from 7.5 GB to 9.2 GB during the 100M run.
The wall times are thus noisy. The CPU times are more stable. The Linux
load values in the table are the load of the VM. The load of the host was
57.09 36.47 34.13 before the Linux run and 68.70 57.27 45.00 after it.

| OS | Objects | Load before (1, 5, 15 min) | Load after (1, 5, 15 min) |
|---|---|---|---|
| macOS | 1M | 138.96 106.46 99.40 | 129.94 109.14 100.78 |
| macOS | 10M | 122.01 112.08 103.35 | 125.97 125.58 113.82 |
| macOS | 100M | 125.97 125.58 113.82 | 79.14 80.73 92.59 |
| Linux | 1M | 2.52 1.99 1.53 | 2.78 2.09 1.57 |
| Linux | 10M | 2.78 2.09 1.57 | 3.75 3.13 2.21 |

Store:

| OS | Objects | Packs | Disk B/object | Create puts/s | Creator peak RSS MB |
|---|---|---|---|---|---|
| macOS | 1M | 1 | 104 | 68240.129 | 497.734 |
| macOS | 10M | 9 | 104 | 64026.099 | 2803.016 |
| macOS | 100M | 81 | 104 | 72219.59 | 16323.326 |
| Linux | 1M | 1 | 104 | 132072.399 | 386.064 |
| Linux | 10M | 9 | 104 | 144879.559 | 2027.95 |

Open:

| OS | Objects | Open s (median) | Open s (min-max) | Open CPU s (median) | Peak RSS B/object (median) | Steady RSS B/object (median) | Cache |
|---|---|---|---|---|---|---|---|
| macOS | 1M | 0.964 | 0.953-1.007 | 0.591 | 390.185 | 390.185 | warm |
| macOS | 10M | 12.844 | 10.981-16.072 | 5.881 | 256.312 | 195.792 | warm |
| macOS | 100M | 141.241 | 134.217-151.394 | 62.239 | 154.232 | 105.533 | warm |
| Linux | 1M | 0.595 | 0.559-0.619 | 0.593 | 301.974 | 153.371 | warm |
| Linux | 10M | 10.364 | 8.465-12.179 | 10.198 | 192.716 | 123.204 | warm |

Lookups (one thread):

| OS | Objects | contains hit ns | contains miss ns | put dedup-hit ns | contains hit/s | contains miss/s |
|---|---|---|---|---|---|---|
| macOS | 1M | 1304.791 | 727.474 | 1284.086 | 766405.984 | 1374619.105 |
| macOS | 10M | 700.501 | 323.994 | 945.17 | 1427549.444 | 3086474.614 |
| macOS | 100M | 819.545 | 419.842 | 974.533 | 1220188.607 | 2381846.106 |
| Linux | 1M | 695.432 | 266.265 | 796.291 | 1437955.483 | 3755661.853 |
| Linux | 10M | 886.801 | 305.328 | 931.746 | 1127648.216 | 3275162.098 |

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
| Linux | 1M | one | rw | 0/1 | 54 | 1000000 | 0.653 | 0.058 | 931.035 | 17241379.31 | -0.031 | n/a |
| Linux | 1M | one | ro | 0/1 | 54 | 1000000 | 0.569 | -0.026 | n/a | n/a | -0.026 | n/a |
| Linux | 1M | all | rw | 0/1 | 54 | 1000000 | 0.567 | -0.028 | n/a | n/a | -0.057 | n/a |
| Linux | 1M | all | ro | 0/1 | 54 | 1000000 | 0.64 | 0.045 | 1200 | 22222222.222 | 0.046 | 1174 |
| Linux | 10M | one | rw | 1/0 | 67.109 | 1242757 | 11.806 | 1.442 | 46.539 | 861828.71 | 1.512 | 44 |
| Linux | 10M | one | ro | 1/0 | 67.109 | 1242757 | 8.888 | -1.476 | n/a | n/a | -1.334 | n/a |
| Linux | 10M | all | rw | 8/1 | 540 | 10000000 | 39.569 | 29.205 | 18.49 | 342407.122 | 27.715 | 19 |
| Linux | 10M | all | ro | 8/1 | 540 | 10000000 | 11.688 | 1.324 | 407.855 | 7552870.091 | 1.458 | 370 |

Notes:

- Open time grows linearly with the object count: on macOS, about 0.6 µs
  of CPU time for each object (0.59 s, 5.9 s and 62 s of CPU at 1M, 10M
  and 100M). The wall time is about two times the CPU time because of the
  load.
- Peak RSS is higher than steady RSS, because the old and the new table are
  in memory together during the last resize. #85 must compare both values.
- On macOS, the RSS after open includes memory that the open freed: the
  smaller tables of the earlier resizes, the `.idx` buffers and the decoded
  entry lists. Thus at 1M the peak and the steady value are the same
  (390 B/object), and the steady values are more than the live table
  (153, 122 and 98 B/object). On Linux, the steady values (153 and
  123 B/object) are the same as the live table. A test program that fills
  the same map on macOS shows this: 153 B/object at 1M when the map is
  sized before the
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
- In the Linux VM, the CPU time is almost the same as the wall time, and
  it changes from run to run (the open CPU time at 10M is 8.4 to 12.1 s in
  3 runs). Thus the Linux CPU columns are noisy too. Only the "all rw" row
  at 10M (27.7 s) is clearly above this spread.
- The 1M store has one pack, so its "one" and "all" rows are the
  active-pack path (CRC check only, no hash check, no `.idx` write). Its
  extra CPU time is less than 0.07 s of a 0.6 s open, which is in the
  range of the noise. Thus the 1M scan rates are not useful.
- The "ro" rows show the cost of the scan: on macOS at 10M and 100M, the "all ro"
  rows give 235 to 283 MB/s of CPU (4.3M to 5.2M objects for each CPU
  second). The "rw" rows also write a new `.idx` for each sealed pack. This
  adds about 1.2 s of CPU time for each sealed pack, so the "rw" rows give
  only 46 to 48 MB/s. The "one ro" rows at 10M and 100M (0.18 s and
  -0.74 s) and the "one rw" row at 100M (2.2 s; its 3 runs are 63.0 to
  66.7 s of CPU) are in the range of the noise.
- The recovery open does not load the `.idx` files of the packs that it
  scans. Thus the scan rate is a little too high, most in the "all" rows.
- "Create puts/s" is the wall time of one create run under load. The CPU
  time of create gives 117k, 125k and 122k puts for each CPU second on
  macOS (8.51 s, 80.1 s and 822.6 s at 1M, 10M and 100M), and 137k and
  152k on Linux (7.32 s and 65.9 s at 1M and 10M). Phase 17 compares its
  write speed with these values.
- The lookup values come from one run. Three more lookup runs on the macOS
  1M store, at a lower load (load average about 50), gave 430 to 756 ns
  for a `contains` hit, 219 to 413 ns for a miss, and 776 to 929 ns for a
  dedup-hit `put`. Thus the macOS 1M row in the table is slow because of
  the load.
- All numbers are for a warm page cache. No cold-cache number was taken
  for this (in-memory) index. For the on-disk index (#85) with a cold
  page cache, see "Index with a cold page cache".

### Index results (#85)

Method: the same tool (`benchmarks/index-bench`, see
`benchmarks/index-bench/README.md`) and the same method as "Index baseline
(#66)": 8-byte objects, default `Options` (64 MiB packs), one new process
for each step, one warm-up open, then 3 measured opens (median), one
create and one lookup run. Each store is new (a new directory, never a #66
store). The `chunk-store` backend now waits for the background merges of
the index before each close, so each open finds a settled store. "Disk
B/object" now counts the whole store (packs, `.idx` files and `index/`);
for the #66 stores that was the same as `packs/` alone. The RAM is split
into anonymous pages (heap) and file-backed pages (the mapped run files):
`RssAnon` and `RssFile` on Linux, and on macOS the `internal` and
`external` values of `task_info(TASK_VM_INFO)` (resident anonymous pages,
not the compressed ones; resident pages of mapped files). Peak RSS
(`ru_maxrss`) has no such split.

macOS: the same host as #66: Intel Xeon W-3245 (16 cores, 32 threads),
192 GiB RAM, macOS 26.2 (x86_64), APFS on an internal PCIe SSD, commit
`c1c20d7`, warm cache.

Linux: Docker in the OrbStack VM on the same host
(`scripts/docker-linux-test/run.sh index-bench 1000000 10000000`), kernel
7.2.12-orbstack-00394-gbecdd6130068, btrfs on the Docker volume, same
commit, warm cache. The VM had 11.2 GB of memory available
(`MemAvailable` 11205952 kB). 100M was not run on Linux, as for #66. The
gc step was run only on macOS: the Docker mode runs only `run`.

The host was loaded by other work, but less than during the #66 runs: the
load average was about 13 to 19 during the macOS runs (80 to 140 for
#66). Thus the wall times of #66 are slower partly because of the load.
The CPU times compare better.

| OS | Objects | Load before (1, 5, 15 min) | Load after (1, 5, 15 min) |
|---|---|---|---|
| macOS | 1M | 18.07 19.36 18.93 | 17.62 19.14 18.86 |
| macOS | 10M | 13.69 17.90 18.42 | 14.07 17.07 18.06 |
| macOS | 100M | 14.07 17.07 18.06 | 12.83 13.35 15.20 |
| Linux | 1M | 2.47 1.22 1.11 | 2.77 1.37 1.16 |
| Linux | 10M | 2.77 1.37 1.16 | 3.07 1.67 1.27 |

The Linux load values are the load of the VM. The load of the host was
8.12 6.68 7.98 before the Linux run and 9.11 7.44 8.14 after it.

In the tables, a column named "#66" gives the baseline value of the
column before it, from "Index baseline (#66)".

Store:

| OS | Objects | Packs | Disk B/object | #66 | Index B/object | Create puts/s | #66 | Create puts per CPU s | #66 | Creator peak RSS MB | #66 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| macOS | 1M | 1 | 151.1 | 104 | 47.1 | 124,120 | 68,240 | 141,700 | 117,000 | 322 | 498 |
| macOS | 10M | 9 | 163.6 | 104 | 59.6 | 129,574 | 64,026 | 138,000 | 125,000 | 856 | 2,803 |
| macOS | 100M | 81 | 163.8 | 104 | 59.8 | 113,722 | 72,220 | 117,800 | 122,000 | 3,875 | 16,323 |
| Linux | 1M | 1 | 151.1 | 104 | 47.1 | 239,909 | 132,072 | 251,600 | 137,000 | 256 | 386 |
| Linux | 10M | 9 | 163.6 | 104 | 59.6 | 233,179 | 144,880 | 208,100 | 152,000 | 810 | 2,028 |

Open (median of 3):

| OS | Objects | Open s | #66 | Open CPU s | #66 | Steady RSS B/object | #66 | Steady anon B/object | Steady file B/object | Peak RSS B/object | #66 | Index runs | Memtable entries after open |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| macOS | 1M | 0.626 | 0.964 | 0.625 | 0.591 | 182.0 | 390.2 | 153.8 | 22.0 | 182.0 | 390.2 | 3 | 213,568 |
| macOS | 10M | 0.060 | 12.844 | 0.060 | 5.881 | 4.56 | 195.8 | 2.19 | 0.90 | 4.56 | 256.3 | 5 | 38,528 |
| macOS | 100M | 0.492 | 141.241 | 0.492 | 62.239 | 4.02 | 105.5 | 3.29 | 0.50 | 4.02 | 154.2 | 9 | 123,136 |
| Linux | 1M | 0.506 | 0.595 | 0.504 | 0.593 | 77.6 | 153.4 | 31.2 | 46.4 | 149.7 | 302.0 | 3 | 213,568 |
| Linux | 10M | 0.039 | 10.364 | 0.039 | 10.198 | 12.6 | 123.2 | 2.38 | 10.27 | 12.7 | 192.7 | 5 | 38,528 |

Lookups (one thread, 1M hits and 1M misses, one run):

| OS | Objects | contains hit ns | #66 | contains miss ns | #66 | put dedup-hit ns | #66 | put dedup-hit/s | #66 |
|---|---|---|---|---|---|---|---|---|---|
| macOS | 1M | 1,056 | 1,305 | 701 | 727 | 1,280 | 1,284 | 781,289 | 778,764 |
| macOS | 10M | 1,385 | 701 | 544 | 324 | 1,374 | 945 | 727,986 | 1,058,016 |
| macOS | 100M | 3,183 | 820 | 1,171 | 420 | 2,051 | 975 | 487,632 | 1,026,133 |
| Linux | 1M | 858 | 695 | 501 | 266 | 1,125 | 796 | 888,726 | 1,255,822 |
| Linux | 10M | 1,268 | 887 | 573 | 305 | 1,392 | 932 | 718,514 | 1,073,254 |

Recovery (`recover` moves the `.idx` files away and opens; median of 3).
These rows are not like for like with the #66 recovery rows: the new
index does not read the `.idx` of a pack below the high-water mark, so
only the packs at or above it are scanned ("Covered packs" are the others).
A recovery open is thus about the same as a normal open, and "Extra CPU s"
is in the range of the noise, so no scan rate is given.

| OS | Objects | Scope | Mode | Dropped packs | Covered packs | Scanned objects | Recovery open s | Extra CPU s |
|---|---|---|---|---|---|---|---|---|
| macOS | 1M | one | rw | 1 | 0 | 1,000,000 | 0.324 | -0.323 |
| macOS | 1M | all | rw | 1 | 0 | 1,000,000 | 0.400 | -0.248 |
| macOS | 1M | all | ro | 1 | 0 | 1,000,000 | 0.282 | -0.343 |
| macOS | 10M | one | rw | 1 | 1 | 0 | 0.055 | -0.005 |
| macOS | 10M | all | rw | 9 | 8 | 57,944 | 0.127 | -0.003 |
| macOS | 10M | all | ro | 9 | 8 | 57,944 | 0.048 | -0.012 |
| macOS | 100M | one | rw | 1 | 1 | 0 | 0.412 | -0.080 |
| macOS | 100M | all | rw | 81 | 80 | 579,440 | 0.356 | -0.165 |
| macOS | 100M | all | ro | 81 | 80 | 579,440 | 0.335 | -0.157 |
| Linux | 1M | one | rw | 1 | 0 | 1,000,000 | 0.261 | -0.267 |
| Linux | 1M | all | rw | 1 | 0 | 1,000,000 | 0.254 | -0.277 |
| Linux | 1M | all | ro | 1 | 0 | 1,000,000 | 0.236 | -0.268 |
| Linux | 10M | one | rw | 1 | 1 | 0 | 0.038 | -0.001 |
| Linux | 10M | all | rw | 9 | 8 | 57,944 | 0.046 | 0.004 |
| Linux | 10M | all | ro | 9 | 8 | 57,944 | 0.044 | 0.005 |

The "one ro" rows are left out; they are like the "one rw" rows. At 1M the
store has one pack, and the recovery open is faster than a normal open
(about 0.25 s against 0.5 to 0.6 s of CPU). A probable cause, not
measured: the normal open decodes the whole `.idx` of that pack (1M
entries) to replay the 213,568 records above the high-water mark, and the
recovery open scans the pack (CRC check only) instead.

GC (macOS only; `index-bench gc` on a new store made for it, 3 of 4
objects live, one run). Object `i` is dead when `(i / R) mod 4` is 0. With
R = 1024 the dead objects lie in runs of 1,024 (55 KiB of records), so a
mostly live 64 MiB pack gets holes. #66 had no GC step, so there is no
baseline. The gc step ran with `ulimit -n 256` (the default soft limit of
open files of launchd); the shell limit was 1048576. The 1-minute load
average before the three GC runs was 11.45, 6.70 and 7.23.

| OS | Objects | Live share | Dead run R | GC wall s | Lock hold max ms | Lock hold total ms | Removed objects | Packs punched | Packs compacted | Packs deleted | gc process peak RSS MB |
|---|---|---|---|---|---|---|---|---|---|---|---|
| macOS | 10M | 3/4 | 1,024 | 60.6 | 108.3 | 336.4 | 2,387,518 of 2,500,608 | 8 | 1 | 0 | 1,416 |
| macOS | 100M | 3/4 | 1,024 | 745.5 | 104.5 | 384.1 | 24,073,153 of 25,000,192 | 81 | 0 | 0 | 7,427 |
| macOS | 10M | 3/4 | 1 | 34.0 | 111.9 | 345.7 | 14,486 of 2,500,000 | 0 | 1 | 0 | n/a |

The longest hold of the store lock is about the same at 10M and at 100M
(108 and 105 ms), and the sum of all holds grows only a little (336 and
384 ms), because the passes over the index run outside the store lock.
The wall time grows with the object count (about 6 to 7.5 µs for each
object). The longest hold is probably a memtable flush or
the install of a group (the tool does not show which one).
A punch removes only the dead records in whole 4 KiB blocks, so some dead
objects at the ends of each dead run stay (they are counted as kept). With
R = 1 (every fourth object dead, one by one), no 64 MiB pack can get a
hole: only the small last pack of the store was compacted, and 99.4 % of
the dead objects stayed. The peak RSS of the gc process includes the live
set of the mark (75M IDs at 100M).

At 100M the GC changed 81 packs in 3 groups of at most 32 packs. Before
this change, each pending GC run kept its file open until the
install, so a GC over more packs with dead objects than the limit of open
files stopped with `EMFILE` and changed nothing. A test store of 330
packs (every fourth object dead) showed this at `ulimit -n 256`; after
the change, the same GC completes at 256 and at 128. A GC now holds at
most 32 pending run files open, plus one merged run for each group done,
until the background merges take them into larger runs. This was shown
up to 330 packs.

Notes:

- RAM per object, steady anonymous memory after open: 2.19 B at 10M and
  3.29 B at 100M on macOS, 2.38 B at 10M on Linux. The target was "a few
  bytes": it is met. The #66 index needed 106 to 196 B (macOS) and 123 to
  153 B (Linux).
- The marginal RAM per object, (steady RSS at 100M - steady RSS at 10M) /
  90M, is 3.41 B of anonymous memory (3.95 B of all RSS). This value is
  higher than the true cost for each object. The only part that grows
  with the object count is the filters and the fences: 1.36 B for each
  object at both sizes (13.6 MB at 10M, 136.3 MB at 100M). The rest is
  fixed: the memtable (up to 262,144 entries) and the replay of the
  records at or above the high-water mark, which reads the whole `.idx` of
  that pack. At 100M that pack had 579,440 entries and the memtable 123,136
  entries; at 10M 57,944 and 38,528. Thus the fixed part was 192 MB at
  100M and 8 MB at 10M. It is bounded by one pack and the memtable limit,
  not by the object count.
- At 1M, the steady anonymous RSS is 154 B for each object on macOS and
  31 B on Linux. That is the fixed part above, divided by only 1M objects
  (the one pack of the store has 1M entries, and 213,568 records are
  above the high-water mark). On macOS, memory that the open frees stays
  in the RSS (see the #66 notes), so the value is higher there.
- The file-backed RSS is the mapped run files. After the open it is small
  (0.5 to 0.9 B for each object on macOS at 10M and 100M; 10.3 B on Linux
  at 10M, where the kernel maps more pages around each touched page). The
  lookups touch the run pages: the file-backed RSS grew by 57 B (10M) and
  32 B (100M) for each object during the lookup step on macOS, and the
  anonymous RSS did not grow. These pages are page cache, which the kernel
  can drop.
- The open reads only the filters, the fences and the tables of the runs,
  and replays the records above the high-water mark. Its time does not
  grow with the entries: 0.06 s at 10M and 0.49 s at 100M on macOS (12.8 s
  and 141 s for #66), 0.04 s at 10M on Linux (10.4 s). The 100M open is
  slower than the 10M open mostly because its replay is larger (see
  above); the filters are about 1.25 B for each object.
- Lookups are slower than with the map in RAM at 10M and 100M (at 1M on
  macOS they are not): a hit costs 0.9 to 1.4 µs at 1M and 10M and
  3.2 µs at 100M (0.7 to 1.3 µs for #66; 3.9 times slower at 100M), a
  miss 0.5 to 1.2 µs (0.3 to 0.7 µs). A miss checks the memtable and the
  filter of each run (5 runs at 10M, 9 at 100M). A hit also reads one page
  of the mapping. A probable cause of the slow hits, not measured: each
  lookup step is a new process, so the first touch of each page is a page
  fault, and at 100M the lookups mapped 3.2 GB of the 6 GB of runs (the
  file-backed RSS grew by 31.5 B for each object). A long-running mount
  takes that fault only at the first touch of each page. The dedup-hit
  `put` is 1.1 to 2.1 µs (0.8 to 1.3 µs for #66).
- Disk: the index adds 59.8 B for each object at 100M (58-byte entries,
  the filter, the fences, page padding). The store needs 164 B for each
  object in place of 104 B.
- Create: 114k to 130k puts/s of wall time on macOS and 233k to 240k on
  Linux, about 2 times the #66 values (which were measured at a higher
  load). For each CPU second: 118k to 142k on macOS (117k to 125k for
  #66), 208k to 252k on Linux (137k to 152k). The CPU time of create now
  includes the background merges and the wait for them before the close;
  on Linux at 10M it is higher than the wall time (48.1 s and 42.9 s),
  because the merges run on a second thread. The creator peak RSS is 3.9 GB
  at 100M (16.3 GB for #66). `ru_maxrss` counts the mapped run pages that
  the dedup checks of the puts touch, so this value does not show the
  anonymous memory alone (a sample of a 10M create had 164 MB of mapped
  file pages in 675 MB of RSS).
- Recovery: see the note above the Recovery table. The #66 scan rates do
  not apply to the new index.
- All numbers are for a warm page cache. For a cold page cache (Linux
  only), see "Index with a cold page cache".
- The index size was made smaller after these runs: see "Index size".

### Index size

The run files of #85 used about 60 B for each object (58-byte entries with
fixed-size fields, 70 for each page). Three ways to make them smaller were
measured. The object ID stays the full 33-byte BLAKE3-256 ID in every
content identity; option A shortens it only inside the local index.

- A: keep only 16 digest bytes of the ID in the run entries, and confirm
  a hit against the full ID (a read of the 33-byte ID at the start of the
  record in the pack). On a mismatch, the search would go on.
- B: per-page field widths. Each page holds the byte widths of its pack,
  offset, stored and raw fields (the smallest width that holds each value
  of the page; 0 to 8 bytes, so no pack number, offset or length has a
  limit). The raw field takes no byte in a page where each raw length is
  the stored length. The tag moves to the page header.
- C: prefix compression in a page. The leading digest bytes that all keys
  of a page share (at most 8) are not stored; they are the first bytes of
  the page fence, which is in RAM and checked at open.

Kept: B and C (run format version 2). A was measured with a throwaway
spike only and is not kept (see below).

Method: `benchmarks/index-bench` (`create`, then `lookup`), macOS on the
host of "Index results (#85)", base commit `a9a51ce`, B and C at
`6b51539`. Each variant had its own new store. The lookups ran 3 times
for each variant, interleaved (base, B, B+C, A, then again), with a
1-minute load average of 7 to 8; the table gives the median of the 3
first-pass times (each lookup is a new process, so the first pass takes
the page faults of the mapped runs). The objects are 8 bytes, so the
stored lengths are 1 byte and the raw field is empty: the bench favours B.
The unit test `a_run_of_real_places_needs_about_41_bytes_for_each_entry`
(`run.rs`) gives the value for real places: 1,000 packs of 64 MiB, chunks
of 2 to 32 KiB, half of them compressed.

Disk (index bytes for each object; `index_bytes_per_object` of `create`):

| Variant | 1M | 10M | Store B/object at 10M | Bytes for each entry in the pages (bench) | Real places (unit test, 200k entries) |
|---|---|---|---|---|---|
| #85 (base) | 47.1 | 59.6 | 163.6 | 58.5 | 58.5 |
| B | 31.7 | 40.6 | 144.6 | about 39.5 | not measured |
| B + C (kept) | 31.0 | 39.5 | 143.5 | about 38.5 | 42.4 (43.7 with filter and tables) |
| A + B + C (spike) | 18.3 | 23.4 | 127.4 | about 22 | about 27 (estimate) |

At 1M, about 213,000 entries are still in the memtable (above the
high-water mark) and are not in the runs, so the 1M values are lower.

Lookups (ns, median of 3 first passes; the second, warm pass gave the
same order):

| Variant | Objects | contains hit | contains miss | put dedup-hit |
|---|---|---|---|---|
| #85 (base) | 1M | 797 | 449 | 1,134 |
| B | 1M | 731 | 446 | 1,131 |
| B + C | 1M | 754 | 458 | 1,140 |
| A + B + C | 1M | 2,080 | 459 | 2,566 |
| #85 (base) | 10M | 1,295 | 496 | 1,358 |
| B | 10M | 1,065 | 485 | 1,238 |
| B + C | 10M | 1,153 | 497 | 1,299 |

Create of 10M objects, base and B + C, twice each, interleaved (base, B +
C, base, B + C; 1-minute load 5 to 8). The wall times vary much with the
host load; the CPU time and the merge bytes compare better:

| Variant | CPU s | Wall s | Merge bytes written |
|---|---|---|---|
| #85 (base) | 85.9, 69.3 | 174.2, 112.6 | 1,067 MB |
| B + C | 73.0, 66.3 | 155.6, 68.7 | 706 MB |

Results:

- B + C: the index uses 20 B less for each object at 10M (39.5 against
  59.6 B, -34 %); the store uses 143.5 B in place of 163.6 B (-12 %).
  With real places, an entry needs about 42 B (58.5 B before). The
  lookup times stay in the noise of the base: a hit reads one page as
  before, and there is no extra read. B gives almost all of the gain; C
  adds about 1 B for each entry (the pages of a run of 2 to 10M entries
  share 1 to 2 leading bytes), at no measured cost. The merges write
  34 % fewer bytes, and the create needs no more CPU time.
- A gives 16 B more for each object (23.4 B at 10M), but:
  - Each hit of `contains` and of the dedup check of `put` needs one more
    read (the record ID in the pack) and the pack handle, which is taken
    under the store lock: +1.3 µs for a hit at 1M (2,080 against 754 ns),
    +1.4 µs for a dedup hit. (`read` checks the record ID already, so it
    needs no extra read.)
  - The merges and the GC need the full IDs of the run entries: the merge
    must keep two objects with the same 16-byte prefix apart, and GC asks
    the live set with the full ID of each entry (`Snapshot::iter_all`).
    With prefixes only, each of these would need a read of the record or
    of the `.idx` for every entry, or a GC by prefix, and a tombstone would
    have to name the record it hides, not only its pack. The spike shows
    the problem: its merges wrote zero-padded IDs into the Bloom filters,
    so at 10M most hits were not found (its 10M lookups are not valid and
    are not given).
  - The change would reach `lib.rs` (GC, put, contains), not only the
    index files.
  Thus A is not kept. It can come back if the disk size of the index
  becomes the limit; it then needs a filter on the prefix, tombstones
  with the full place, and a GC that is correct with prefix matches.

### Idle merge (#111)

Problem: a lookup checks the filter of each run, and the size-tiered
merges keep 3 runs at 1M objects, 5 at 10M and 9 at 100M.

Policy: the idle merge (see "Index on disk (#85)"). When the store is idle
(no new index entry and no flush, merge or GC install for 5 s) and the
index has more than 2 runs (`IDLE_TARGET_RUNS`), the merge thread merges
the newest runs into one. The window is the longest prefix of the run list
(at least 2 runs) whose oldest run has at most 4 times the entries of the
newer runs in the window (`IDLE_MAX_REWRITE`). The constants are in
`index.rs`; the delay is `Options::index_idle_merge_after`. The idle merge
uses the same write and install steps as the size-tiered merges, so it
has the same crash and GC safety. An idle merge that has started runs to
its end; puts during it go to the memtable, and the merges of their
flushes wait until it ends.

Why this policy: a model of the merges (sizes in memtable flushes, the
size-tiered merges after each flush, then an idle merge after each burst of
B flushes) compares three rules. These are model values, not measured on
a store.

- "Merge every run, but leave out the largest if the merge would rewrite
  more than 4 times the new data": with B = 1 (an idle pause after each
  flush, the worst case) it keeps at most 3 runs, but the merges write
  23.9 bytes for each byte of index at 100M (381 flushes), because the
  second run is rewritten at each pause.
- "Grow the window from the newest run while the window has 1/4 of the next
  run": 7.9 bytes and at most 5 runs at B = 1, but it stops at the first
  large run. The size-tiered merges often leave a small run before two
  large ones (for example 1, 16, 16 flushes at 33 flushes), and then it
  merges nothing: after one long write load and one pause, 3 to 7 runs
  stay for 111 of the flush counts from 1 to 400.
- The chosen rule (longest prefix whose oldest run passes): 9.1 bytes and
  at most 5 runs at B = 1. After one long write load and one pause it
  leaves 1 run for 311 of the flush counts from 1 to 400, 2 runs for 86 and
  3 runs for 3. A store at 33 flushes (1M objects, memtable limit 30,303)
  went from 3 runs to 1 on a real store.

The size-tiered merges alone write 3.5 bytes for each index byte at 100M
and keep up to 12 runs. After one long write load and one pause, every
rule merges every run into one (4.5 bytes for each index byte at 100M, of
which 1.0 is the idle merge). With the chosen rule at B = 1 and 100M, a
ratio of 2 gives 6.8 bytes and up to 6 runs, and a ratio of 8 gives 13.2
bytes and up to 4 runs.

Bench: `index-bench lookup` now times a second pass over the same sample
in the same process (`*_2` keys). The first pass takes the page faults of
the mapped runs, and the second pass does not. The new step `settle` opens
the store with the default options, waits for the idle merge, and reports
the runs and the merge bytes. All other steps open with no idle merge.

Method: macOS, the host of "Index results (#85)", commit of this change,
release build, warm cache. For each size: `create` (the size-tiered merges
only), 3 `lookup` runs, `settle`, 3 `lookup` runs, all on the same store.
Each lookup run is a new process. The values are the median of the 3 runs.
The load average (1 min) was 10.5 to 13.0. These runs used the second
rule of the list above; for these run lists (3 and 5 runs) both rules
merge every run, so the chosen rule gives the same result.

Runs and merge bytes ("Merge B / index B" is the bytes that the merges
wrote, divided by the size of the index after the create):

| Objects | Runs after create | Merge B / index B in create | Runs after idle | Merge B / index B of the idle merge | Settle s (with the 5 s delay) |
|---|---|---|---|---|---|
| 1M | 3 | 0 | 1 | 1.00 | 6.6 |
| 10M | 5 | 1.79 | 1 | 1.00 | 17.6 |

Lookups (ns; 1M hits, 1M misses and 1M dedup-hit puts in each pass):

| Objects | Runs | Hit, pass 1 | Hit, pass 2 | Miss, pass 1 | Miss, pass 2 | Put dedup-hit, pass 1 | Put dedup-hit, pass 2 |
|---|---|---|---|---|---|---|---|
| 1M | 3 (as created) | 875 | 847 | 546 | 533 | 1,196 | 1,164 |
| 1M | 1 (after idle) | 789 | 755 | 444 | 438 | 1,072 | 1,074 |
| 10M | 5 (as created) | 1,219 | 960 | 473 | 479 | 1,205 | 1,223 |
| 10M | 1 (after idle) | 983 | 751 | 295 | 295 | 1,005 | 996 |

Notes:

- At 10M the warm hit goes from 960 to 751 ns (-22 %) and the miss from
  479 to 295 ns (-38 %); the dedup-hit put from 1,223 to 996 ns. The
  values with one run are near the #66 baseline (701 ns hit, 324 ns miss
  at 10M) and near the values of the spike in #111 (0.80 µs, 0.31 µs).
- The page faults of the first pass cost about 240 to 260 ns for each hit
  at 10M (1,219 against 960 ns, 983 against 751 ns), with 5 runs and with
  1 run. They do not depend on the run count. The misses read no page, so
  they have no such cost. At 1M the store is small and the difference is
  about 30 ns.
- At 1M about 213,568 entries stay in the memtable after the open (the
  records above the high-water mark), so a part of the lookups ends in the
  memtable.
- The idle merge rewrote the index one time (1.00 byte for each byte of
  index). With the size-tiered merges of the create, the merges wrote 2.79
  bytes for each byte of index at 10M. At 100M the index is about 6 GB,
  so one idle merge after a long write load writes about 6 GB; the run at
  100M was not made.
- The run count at 100M is not measured. By the model above it is 1 after
  a long write load and a pause (381 flushes), and at most 5 under a load
  with a pause after each flush.

### Index with a cold page cache

The numbers of #66, #85 and #111 are for a warm page cache. This section
measures the on-disk index (#85) after a drop of the page cache: the open
and the lookups.

Method: Linux only, Docker in the OrbStack VM on the host of "Index results
(#85)" (`scripts/docker-linux-test/run.sh index-bench-cold 1000000
10000000`), kernel 7.2.12-orbstack-00394-gbecdd6130068, btrfs on the Docker
volume, commit `a9a51ce` plus the `--drop-caches` and `--only` flags of
`index-bench`, release build. The VM had 12.9 GB of memory available
(`MemAvailable`). The container is privileged, so root can write
`/proc/sys/vm/drop_caches`. Each store is new. For each size: `create`,
`settle` (the idle merge, #111: 3 runs to 1 at 1M, 5 runs to 1 at 10M),
one warm-up open, 3 warm opens, one warm lookup, 3 cold opens, one cold
lookup, then 3 cold lookups of a small sample (seeds 1 to 3) for each phase
(hit, miss, put), each in a new process. A cold step calls `sync` and
writes `3` to `drop_caches` just before the timed part: `open` before the
open, `lookup` after the open and before the first pass. The load average
of the VM was about 0.8 to 2.3. The load of the macOS host was 27.6 before
the first run and 29.3 before the last run.

A third store, "1M, no tail", has 1M objects made with `--memtable-limit
250000`: 4 whole flushes, so nothing stays above the high-water mark and
the open puts no record in the memtable (see the 1M note below). It was
measured with the same steps (by hand, with the same binary).

What a cold step can and cannot show:

- The drop does not remove pages that a process has mapped. So in one
  process the put phase finds the run pages that the hit phase read; a
  cold put needs its own process (`--only put`). For the same reason, a
  lookup after an open that has read the run pages through the mapping is
  not cold (the default 1M store, see below).
- With the default sample (1M lookups) the first pass reads almost every
  page of the run (142k pages at 10M), so its mean mixes cold and warm
  reads. The small sample (K = N/1000: 1,000 at 1M, 10,000 at 10M) reads
  mostly pages that no lookup before it read, so it gives the cost of one
  cold lookup.
- The page reads are the major faults (`ru_majflt`) and the bytes of
  `read_bytes` in `/proc/self/io` of each phase.
- The disk is an SSD: the VM disk is a file on the internal PCIe SSD of
  the host. No HDD is available here, so no HDD number was taken.
- The drop empties the cache of the VM, not the cache of macOS. The VM
  disk is a file on macOS, and the host has 192 GiB of RAM, so a read that
  misses the cache of the VM can still come from the RAM of the host. The
  cost of one page read here is about 14 to 27 µs (see the notes). That is
  less than a usual random 4 KiB read of an SSD, so some or all of these
  reads probably came from the cache of the host. `sudo purge` on macOS
  was not used (no sudo), so the numbers are "cold in the VM", a lower
  bound of a cold disk.
- macOS: no cold run. `purge` needs sudo, and `index-bench` refuses
  `--drop-caches` on macOS.
- 100M: not run. The store is about 16.4 GB and the VM has about 12.9 GB
  of free memory, so the page cache cannot hold it, and the warm numbers
  would not be warm. It needs a host with about 25 GB of free RAM.

Open (median of 3; "Read MB" is `read_bytes` of the open; "Mapped MB" is
the growth of the mapped file pages, `RssFile`, in the first cold open):

| Store | Runs (MB) | Memtable after open | Warm open s | Cold open s | Cold read MB | Cold major faults | Mapped MB |
|---|---|---|---|---|---|---|---|
| 1M | 1 (47) | 213,568 | 0.740 | 0.826 | 98.4 | 5 | 46.3 |
| 1M, no tail | 1 (60) | 0 | 0.831 | 0.840 | 52.6 | 3 | 0.5 |
| 10M | 1 (596) | 38,528 | 0.042 | 0.060 | 19.4 | 415 | 2.3 |

Lookups, default sample (1M hits, 1M misses, 1M dedup-hit puts, one
process; ns per call; "pass 2" is the second pass over the same sample):

| Store | Cache | Hit, pass 1 | Hit, pass 2 | Miss, pass 1 | Put dedup-hit, pass 1 | Hit major faults | Hit read MB |
|---|---|---|---|---|---|---|---|
| 1M | warm | 1,349 | 1,372 | 791 | 1,803 | 0 | 0 |
| 1M | after a drop, but the run is still mapped by the open (not cold) | 1,337 | 1,375 | 878 | 1,756 | 4 | 0.3 |
| 1M, no tail | warm | 679 | 686 | 166 | 889 | 0 | 0 |
| 1M, no tail | cold | 973 | 795 | 202 | 1,005 (warm: after the hits) | 21 | 59.9 |
| 10M | warm | 1,678 | 1,524 | 518 | 1,923 | 0 | 0 |
| 10M | cold | 5,253 | 1,453 | 529 | 1,720 (warm: after the hits) | 141,769 | 580.7 |

Lookups, small sample (one phase for each process; cold first pass; the
median of 3 seeds; "Warm" is the second pass of the same process; page
reads per call = major faults / K):

| Store | K | Phase | Cold ns | Warm ns | Page reads per call | Read KiB per call |
|---|---|---|---|---|---|---|
| 1M, no tail | 1,000 | hit | 27,699 | 815 | 0.021 | 58,512 |
| 1M, no tail | 1,000 | miss | 13,423 | 172 | 0.008 | 29,284 |
| 1M, no tail | 1,000 | put dedup-hit | 27,932 | 992 | 0.021 | 58,512 |
| 10M | 10,000 | hit | 18,130 | 1,588 | 0.96 | 3.8 |
| 10M | 10,000 | miss | 987 | 560 | 0.012 | 0.05 |
| 10M | 10,000 | put dedup-hit | 23,779 | 1,874 | 0.92 | 3.7 |

The default 1M store has no cold row in this table: its open has already
read the run (see the notes), so its "cold" small sample read no page
(0.002 page reads per call; hit 2,420 ns against 1,715 ns warm).

The 3 cold values of the 10M small sample: hit 13,973, 25,859 and 18,130
ns; put 38,492, 23,779 and 14,383 ns. They vary much; the host was loaded
(see above).

Notes:

- 10M: a cold hit reads about one 4 KiB page of the run (0.92 to 0.96
  major faults per hit, 4,096 bytes per fault; the run is larger than
  256 MiB, so it is mapped with `MADV_RANDOM` and has no readahead). It
  costs about 18 µs against 1.6 µs warm, so one page read costs about 17
  µs here (14 to 25 µs over the seeds). The dedup-hit put is the same plus
  the hash. A miss reads a page only for a false positive of the filter
  (about 1 %), so a cold miss costs about 1 µs, near the warm value.
- 10M, default sample: the first pass reads all 141,769 pages of the run
  (581 MB) and costs 5.25 µs for each hit against 1.45 µs in pass 2. That
  is about 27 µs for each page read. After that the whole run is in the
  cache, so the puts of the same pass are warm.
- 10M open: a cold open reads the filter (12.5 MB), the fence and tag
  tables, the MANIFEST, the `.idx` of the newest pack (2.9 MB) and about
  400 run pages: 19.4 MB, 18 ms more than warm. The run pages come from
  the replay of the 38,528 records above the high-water mark (see the 1M
  note): 415 faults are about the false-positive rate of the filter
  (1.1 %) of 38,528 lookups.
- 1M (default store): the open reads the whole run. The store has one
  pack, and its 213,568 records above the high-water mark (1,000,000 - 3
  × 262,144 flushed entries) are put in the memtable at open. The replay
  looks up each ID in the index (`replay` in `lib.rs`), and the run is
  smaller than 256 MiB, so it has the default readahead: the mapped file
  pages of the open grow by 46 MB (the run is 47 MB). The cold open reads
  98 MB: the run and the 50 MB `.idx` of the pack. So the cold cost of this
  store is in the open (+0.09 s), and its lookups after the open are warm.
  A cold lookup of this store cannot be measured in the lookup step.
- 1M, no tail: the open reads only the `.idx` (52.6 MB) and maps 0.5 MB.
  Here the readahead of a run below 256 MiB shows: the first cold faults
  read the whole 60 MB run in about 21 faults (about 2.9 MB for each
  fault). 1,000 cold hits take 27.7 ms in total, about the time to read
  the run; after that every lookup is warm. 1,000 cold misses read about
  30 MB in 8 faults (the false positives of the filter), 13.4 ms. So below
  `MADV_RANDOM_MIN` the first cold lookups pay for the whole run at once;
  the cost does not grow with the number of lookups.
- The warm hit of "1M, no tail" (679 ns) is half of the default 1M store
  (1,349 ns). The default store answers part of its lookups from the
  memtable with 213,568 entries; this was not studied further.
- The Linux warm values of the default stores are slower than the macOS
  values of #111 (10M, 1 run: 751 ns hit, 295 ns miss). The macOS host had
  a load of about 28 during these runs.

### Dedup

Before an object is written, its ID is looked up in the index. If the
object is there, it is not written again (writes are idempotent; no lock is
needed for correctness).

#### Locality cache (#112)

The runs are sorted by digest, so the chunks of one file are spread over
all run pages, and each dedup hit reads one page. A file written again
usually has its chunks in one pack, in the same order, and the `.idx` of
that pack holds them together. So `put` checks a small cache first
(`crates/chunk-store/src/locality.rs`, `Options::locality_cache`, on by
default): after 4 index hits in a row in one pack, it reads that pack's
`.idx` and keeps its entries. One random dedup hit never reads an `.idx`.

- Bound: at most 16 packs and 65,536 entries (a pack with more entries is
  never cached). The table is at most about 9.1 MiB.
- GC safety: a `.idx` is not the index. A GC writes its tombstones first
  and can leave an `.idx` stale (a skipped punch, a failed removal, a
  crash). So the cache takes only packs that this process sealed (their
  `.idx` is exact) and that no GC has selected since. Under the store
  lock, before its install, the GC removes the packs it changes or deletes
  from the trusted set and from the cache, and moves an epoch so that a
  fill running at that time is dropped. A cache hit takes the same path as
  an index hit: it is touched for a running GC and checked again under the
  lock (`Index::gen`). A failed index lookup in `put` writes the object
  again, so an older copy can stay in a trusted pack that a later GC does
  not select; after such a failure no pack sealed so far is trusted.
- Limit: packs of an earlier process are never cached, because nothing on
  disk shows whether a GC left their `.idx` stale. A re-write of data from
  an earlier mount gets no gain until a durable marker (for example a pack
  header flag set before the GC install) exists.

Results (2026-11-19, macOS, warm cache, `index-bench rewrite`, 8-byte
objects, `--pack-limit 442368` so a pack has about 8K entries as with
8 KiB chunks; the store is made in the same process; median of the runs,
ns per `put`):

| Objects | Packs | in-order re-write, cache off | cache on | random dedup hits, off | on |
|---|---|---|---|---|---|
| 1M (3 runs) | 123 | 1,293 | 833 | 1,497 | 1,264 |
| 10M (2 runs) | 1,221 | 1,350 / 1,377 | 558 / 568 | 1,555 / 1,496 | 1,574 / 1,523 |

In order, 99.95 % of the dedup checks hit the cache (0.0005 index lookups
for each put; 1 `.idx` read per 8K puts). Random hits never fill and cost
the same within the noise (the machine had other load; the 1M random
numbers vary by 30 %). The bench hashes 8-byte objects; with 8 KiB chunks
the hash is a larger part of each put, so read the gain as about 0.8 µs
saved for each put at 10M, not as a percentage. The bench is one thread,
so it does not show the cost of the cache mutex under many writers. With the default 64 MiB packs and 8-byte objects a
pack has 1.2M entries and is never cached.

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

The on-disk index (#85) adds these rules:

- Before a run that points into the active pack is written (a flush, a GC
  install), the active pack is synced (and the directory, if a pack was
  made since the last directory sync). So a durable run never points to
  bytes that are not durable.
- A run file is synced and its name made durable before a MANIFEST lists
  it. The MANIFEST is written with `write_durable`; this write is the
  commit point. A crash before it leaves the old MANIFEST, and the new run
  file is an orphan.
- A writable open removes every run file that the MANIFEST does not list
  (orphans of a failed flush, merge or GC) and the obsolete runs that it
  names. The inputs of a merge are removed only after the MANIFEST without
  them is durable; if a crash comes before their removal, the next
  writable open removes them.
- The memtable is not written at close. The next open replays the records
  at or above the hwm from the packs, so nothing is lost.
- A damaged index (see "Index on disk (#85)") is built again from the
  packs.

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

The index files (#85) go through the same trait, as two more file kinds:
`Run` (a sorted run, `NNNNNNNNNNNNNNNN.run`) and `Manifest` (one file,
`MANIFEST`). `FileIo` keeps them in the directory `index/` next to
`packs/`. A run file is written like a pack (`create_run`, writes, `sync`,
then a directory sync) and read with `open_run`; `map_run` maps it
read-only for lookups (`FileIo`; `MemIo` has no mapping, so its lookups
use `read_at` and its read faults reach them). The MANIFEST is written
only with `write_durable`. Thus the crash and fault tests of `MemIo` cover
the index files too.

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

GC with the on-disk index (#85). `gc_finish` keeps no map of all objects,
and it holds the store lock only for short steps:

1. Start (store lock): a snapshot of the index (a copy of the memtable and
   the list of runs). From now on, a second touched set records the
   objects that are put or read.
2. Pass 1 (no store lock): one merged pass over the snapshot gives, for
   each pack, its bytes, its live bytes and its entries. Then the same
   rules as above choose: delete, compact, punch or keep.
3. Pass 2 (no store lock), one selected pack at a time: the entries come
   from the pack's `.idx` (or a scan of the pack). An entry is current
   only if the snapshot points to this pack and offset. The dead entries
   go as tombstones (ID, pack) into a pending run file that no MANIFEST
   lists yet. Live records of a pack to compact are read with no lock and
   appended in batches of 1,024 under the lock.
4. Install (store lock): the plan of a pack that holds an object put or
   read during the GC is dropped (that pack does not change). The active
   pack and the directory are synced. Then one MANIFEST write lists the
   pending runs as the newest runs, with the memtable (it can hold a
   victim's put) in a run under them, and moves the hwm to the end of the
   active pack.
5. Only after that MANIFEST is durable, with no store lock, one pack at a
   time: delete the pack (and its `.idx`), or punch it (its remaining
   entries are read again from the index). Then the runs of the install are
   merged into one run.

So the tombstones and the copies are durable before any pack is punched or
deleted, and no lookup (also not the dedup check of `put`) finds an object
in a removed pack. Steps 3 to 5 run for groups of at most 32 packs
(`GC_GROUP_PACKS`): each pending run keeps its file open until the merge
after its install, so a GC over many packs must not hold one file for
each pack (with more packs than the limit of open files, it would stop
with `EMFILE`). A GC thus holds at most 32 pending run files open, plus
one merged run for each group done, until the background merges take
those into larger runs. The packs of objects that are touched during an
earlier group stay unchanged in the later groups too, and the touched set
records again after each install.

A read error or a damaged run page in the passes or at the install ends
the GC with the error: the pending runs of that group are removed, and
that group and the later ones change nothing (only harmless duplicate
copies stay). A damaged page also sets the rebuild flag. The groups
before it are done; each one is a complete GC of its packs. An index read
error is never taken as "not current".

Merges keep the tombstones; only a merge whose output becomes the oldest
run drops them (no older entry is left for them to hide).

If the removal of a pack fails after the install, its tombstones are
durable and the pack stays. A later GC finds the pack with no current
entries and removes it. But if the index is built again from the packs
before that (after damage, or with the rebuild flag), the rebuild finds
the objects of that pack and makes them live again. This is safe: their
bytes are valid, and the next GC removes them again.

The RAM of a GC: the totals of each pack, the entries of one pack, the
memtable copy, and the filters and fences of the pending runs of one
group.

The mark (#113) is a Bloom filter of the live IDs, not a set of them:
10 bits (1.25 bytes) for each object of the store
(`ChunkStore::live_filter`). A false positive only keeps a dead object; a
Bloom filter has no false negatives, so no live object is removed. The
mark walk never asks the filter (a false "seen" would hide a live
subtree). The objects put or read before `gc_finish` (also the nodes that
the mark reads) are exact up to `Options::gc_touched_exact` IDs, then they
move into a Bloom filter of the same size. The objects put or read during
`gc_finish` stay an exact set, because each one is looked up to find its
pack; it grows with the distinct objects touched during `gc_finish`, not
with the store.

`index-bench gc` (`--dead-run 1024`, 3 of 4 objects live, macOS):

| Objects | Peak RSS, ID set | Peak RSS, filter | Filter | False positives | Dead left, ID set | Dead left, filter |
|---|---|---|---|---|---|---|
| 1M | 472 MB | 441 MB | 1.25 MB | 933 (0.37 %) | 27,692 | 82,376 |
| 10M | 1,429 MB | 807 MB | 12.5 MB | 9,314 (0.37 %) | 113,090 | 699,087 |

The dead objects that stay are more than the false positives: the bench
objects have records of 54 bytes, and a punched pack frees only whole
4 KiB blocks, so one false positive keeps about 75 dead records. A chunk of
file content (4 to 32 KiB) fills at least one block, so there we expect one
false positive to keep about one object (not measured). Each filter (the
mark and a spilled touched set) has its own random salt, mixed into the
probe positions, so the false positives of one GC are not those of the
next one, and a dead object does not stay forever.

### Full GC

`slicefs gc --full <store>` (offline), or `full` in `gc.request` (a
mounted store), runs a GC that frees every dead object. It uses
`gc_begin_full`, `metadata::gc::live_objects_exact` and `gc_finish_full`:

- Exact mark in bounded RAM. The mark (and the objects put or read before
  `gc_finish_full`, also the nodes that the mark reads) go to an external
  sort: a buffer of `Options::gc_sort_chunk_ids` IDs (default 4 MiB of
  IDs, 127,100) is sorted, deduplicated and written to a temp file. Then
  merges of at most `Options::gc_merge_fan_in` files (default 32; the
  same open-file limit as `GC_GROUP_PACKS`), each read through a 64 KiB
  buffer, run pass after pass until one sorted file of distinct IDs is
  left. RAM: never the object count. During a mark on a mount two
  buffers fill at once (the mark and the touched set, 4 MiB each); the
  merges need 32 x 64 KiB; the dead entries (below) have their own buffer
  of the same record count (41 bytes each, about 5 MiB).
- Pass 1 is a merge join of the index (in ID order) with that file: no
  false positive. Each dead entry goes as (pack, ID), pack first in big
  endian, to a second sorted file. Pass 2 reads the dead IDs of one pack
  at a time from it, in pack order.
- No holes: every pack with a dead record is compacted (or deleted if it
  has no live record). So is a pack with dead bytes that the index no
  longer lists: a punched pack (its holes and the parts of the removed
  records outside whole blocks), or a pack whose `.idx` has more records
  than current entries (an older copy of an object). This check reads the
  header and the `.idx` of each pack that is otherwise kept. The rest (install, order, groups, error handling,
  touched objects during `gc_finish_full`) is the GC above.
- Temp files are in `Options::gc_tmp_dir` (`<store>/gc-tmp`), removed at
  the end, also after an error; a writable open removes what a crash left.
  They need about 33 bytes per live object plus 41 bytes per dead one.
- `GcStats::dead_left` counts the dead objects that the GC did not remove
  (packs kept because an object of theirs was put or read during the GC,
  or whose entry list did not match the index): with no writes during
  the GC it is 0. It needs no extra pass.

Both modes print the counts, the pack counts, and the disk usage of all
store files before and after (apparent and allocated bytes), offline and
in the `gc.result` line.

Peak heap of the chunk-store part of a full GC (`exact_mark` filled
directly, then `gc_finish_full`) above the baseline (`gc_full_memory_tests`,
1 in 2 objects live, 32 KiB packs, 2,048 IDs a sort chunk, fan-in 8):

| Objects | 12,000 | 24,000 | 48,000 | 96,000 |
|---|---|---|---|---|
| Peak heap | 530 KB | 630 KB | 753 KB | 822 KB |

The growth flattens; an exact set of the live IDs alone would add 33 bytes
for each live object (594 KB from 12,000 to 48,000 objects). The run
filters and fences of the index grow by about 2.7 bytes for each object
of the index (98 KB from 12,000 to 48,000), as without GC. The metadata walk
(`metadata::gc::mark`) still holds one key for each node and file root
(as the normal GC does).

### GC pacing

An online GC gives way to writes, depending on the free space of the file
system under the store (`statvfs`, available / total blocks; an error or
zero blocks counts as plenty). Code: `slicefs-cli/src/gc_pacing.rs`; all
thresholds and sleeps are constants there, to be tuned (#121).

| Free space | Mode | At each safe point |
|---|---|---|
| more than 20 % | yield | while writes came in the last 100 ms: sleep 50 ms, look again |
| 5 % to 20 % | throttle | once per 100 ms of GC work: sleep up to 50 ms, in proportion to the write rate (full at 1,000 requests/s); never a full stop |
| less than 5 % | full speed | no wait; the GC thread also starts a GC that changes made due at once, without `--gc-interval` or the duty gap (still one at a time; an interval of 0 still means only on request) |

- Write signal: a counter of write, create and setattr requests
  (`SliceFsFilesystem::note_write`, bumped in the FUSE handlers). The
  pacer only reads it, and counts only the changes after it was made.
- Safe points: in the mark (`metadata::gc::mark`) before each root and each
  file; in the sweep before each group of `GC_GROUP_PACKS` packs. No store
  lock, no install, no commit gate is held there. The chunk store sees only
  the `GcPacer` trait (`gc_finish_paced`, `gc_finish_full_paced`), not FUSE.
- Caps: the waits of the mark add up to at most 60 s, those of the sweep to
  at most 30 s; then that phase runs at full speed. So a GC finishes also
  under endless writes. The sweep cap matters most: the exact touched set
  of the sweep grows with the writes during it. The mark does not abandon
  at its cap: it deletes nothing, the objects put or read meanwhile go to
  the touched set, and a retry under endless writes could never finish.
- Low space starts GCs at once only while they free something: after a
  GC that freed less than 1 MiB (`LOW_SPACE_MIN_FREED`), the next waits
  the duty gap (at least 10 s), so GCs that free nothing do not loop.
- A failed GC backs off: the next automatic GC waits 1 s, 2 s, 4 s, ...
  after it, up to `--gc-interval` (5 min if it is 0); a success resets
  this. A request from `slicefs gc` runs at once.
- A wait ends at once when the mount shuts down, and when the free space
  falls out of the yield mode (read again at most once a second).
- The duty gap uses the GC time without the waits. `gc.result` and the log
  line show the time waited.
- Offline `slicefs gc` never waits (no writes).

## Read path

Manifest → `ObjectId` → index → `pread` → zstd decode if flagged → bytes. A
small LRU cache of decoded chunks serves reads that do not align with chunks.

## Write path

The current streaming writer keeps its role; only the chunker and the object
store change: bytes of a file → FastCDC → for each chunk, the write hash → dedup
check → append to the active pack → manifest entry. Out-of-order and sparse
writes switch the handle to an overlay (#98): the base is a root in the
manifest, and a rebase stores base plus extents through the splice.

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

Store files after the FSKit Boost round trip (`scripts/macos-mount-test/boost-test.sh`,
macOS CI run 44484947311, 2026-11-19, commit c921a11, Boost 1.85 tar and tree
copied through an FSKit mount): 48 (packs/ 41, index/ 4, segments/ 1). The run
passed (`RESULT fskit: 0 failure(s)`, `gc-freed: ok`). The time per step and
the `fsync` and `write` time per call on FSKit are not measured yet.

Time per step in that run (Boost 1.85: one tar and a tree of 77,655
files; GitHub macOS runner, release build):

| Step | Time | Note |
|---|---|---|
| tar copy to / from / cmp | 10 / 6 / 16 s | |
| tree copy to the mount | 2,183 s | about 28 ms per file |
| tree copy from the mount | 495 s | about 6.4 ms per file |
| tree diff / diff after remount | 73 / 383 s | |
| tar cmp after remount | 17 s | |
| delete all (`rm -rf`) | 2,332 s | about 30 ms per file |
| online GC after the delete | 0.2 s | removed 81,290 objects (199,859,608 bytes) |

The per-file times are the step time divided by the file count. Most of the
tree steps is the FSKit cost of each file operation (see the top of this
document), not the store.

To repeat: run `scripts/macos-mount-test/fetch-boost.sh <DIR> 1_85_0`, then
`scripts/pack-measure/seed-measure.sh <BINARY> <DIR> 3` for each build (see
`scripts/pack-measure/README.md`).
