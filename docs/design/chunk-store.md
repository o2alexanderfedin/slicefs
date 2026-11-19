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

- Entries of 58 bytes: the full 33-byte ID, pack u64, offset u64, stored
  length u32, raw length u32, flags u8. A tombstone is an entry with the
  tombstone flag: its pack field names the pack whose entry it hides.
- The entries are sorted by (tag, digest) and are in 4 KiB pages (70
  entries at most). Each page has an entry count and a CRC32C.
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
- All numbers are for a warm page cache. No cold-cache number was taken:
  on macOS that needs `sudo purge` by hand, and on Linux a privileged
  container.

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
- All numbers are for a warm page cache. No cold-cache number was taken.

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
group. The live set of the mark and the touched sets still grow with the
number of objects.

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

The FSKit Boost round trip (`scripts/macos-mount-test/boost-test.sh`) is a
manual run, and nothing was mounted for these numbers. Add its numbers here
when that run is done: time per step, `fsync` and `write` time per call, and
the store file count after that copy.

To repeat: run `scripts/macos-mount-test/fetch-boost.sh <DIR> 1_85_0`, then
`scripts/pack-measure/seed-measure.sh <BINARY> <DIR> 3` for each build (see
`scripts/pack-measure/README.md`).
