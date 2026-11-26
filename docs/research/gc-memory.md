# GC and rehash memory as a function of the file count

Question: does `slicefs rehash` (and the full GC that it runs at its step 6)
have bounded memory? The rule is: per-file memory is never O(file size), and
1 GiB is huge.

Short answer: no. The rewrite of the rehash engine stays below the GC step,
and its dry run has a flat heap. The GC mark is not bounded: it keeps about
385 bytes per file in RAM with one snapshot, and about 117 bytes more per
file for each extra snapshot. With one snapshot, the GC heap goes above
1 GiB at about 2.8 million files. The earlier estimate of 33 bytes per file
was too low by a factor of 10 or more.

Update (#207): the mark is now bounded. The GC heap no longer grows
with the snapshot count. From 100k to 1M files it grows by 21 to 35 MB
(measured), all outside the mark: mostly the Bloom filters of the index
runs (about 15 bytes per tiny file, by design), which adds about 135 MB
more at 10M files, plus the per-pack `.idx` part (at most about 150 MB).
See "After the fix" at the end.

## How it was measured

`scripts/gc-memory/run.sh [N ...]` (Linux, Docker, no FUSE mount). For each
N, it writes N tiny files (each has different content of 8 to 14 bytes,
1000 files per directory) and imports them with `slicefs seed`. Then it
makes 1 snapshot (`GCMEM_SNAPS` sets the count; a snapshot of the same root)
and runs, in this order: `slicefs gc`, `slicefs gc --full`,
`slicefs rehash --to sha256 --dry-run`, `slicefs rehash --to sha256`.

- Peak RSS: `/usr/bin/time` (maximum resident set). This includes the
  file pages of the mapped index runs.
- Anon: the highest `RssAnon` of a sample every 50 ms (RSS without file
  pages). A sample can miss a short peak, so this is a lower limit. For the
  real rehash, the anon peak came after the `rehash: gc` line in every row.
- Heap peak: `GCMEM_HEAPTRACK=1` also runs the commands under heaptrack
  (glibc malloc, release build with line tables). Heaptrack shows the bytes
  in live heap blocks at the peak and the code line of each block. Its
  units are MB (10^6 bytes).
- Machine: OrbStack Linux VM, 32 CPUs, 16 GiB RAM. A tiny file is 3 values
  (inode node, manifest node, content) and about 6 stored objects.

## Results

Peak RSS and anon in MiB. Heap in MB (heaptrack).

| Files | Snapshots | Store MiB | gc RSS | gc --full RSS | dry-run RSS | rehash RSS (anon) | rehash wall | heap gc --full | heap rehash |
|---|---|---|---|---|---|---|---|---|---|
| 10,000 | 1 | 9 | 36 | 38 | 11 | 48 (41) | 1.3 s | - | - |
| 100,000 | 1 | 109 | 153 | 233 | 76 | 260 (214) | 11 s | 75 | - |
| 1,000,000 | 1 | 1,110 | 562 | 559 | 258 | 832 (399) | 117 s | 385 | - |
| 1,000,000 | 4 | 1,110 | 859 | 907 | 258 | 861 (615) (1) | 68 s (1) | 735 | 764 |

(1) In the 4-snapshot row, the timed rehash ran on a store that already had
the target hash (the heaptrack run converted it first). It keeps every value
as it is, but it still runs the full GC. The heaptrack column shows the real
conversion.

Other numbers:

- Dry-run heap: 75 MB at 100k files and 95 MB at 1M files. All of it comes
  from the store open (see "Per pack" below), not from the walk. The
  dry-run RSS grows (11, 76, 258 MiB), but its anon part stays small
  (66 MiB at 1M files), so the rest is very likely file pages of the
  mapped index runs.
- At 1M files, about half of the rehash RSS is not anonymous memory
  (832 MiB RSS, at least 399 MiB anon).
- Two runs of the 1M row gave the same numbers within 3 %.
- `slicefs seed` peaks at 1.2 GiB for 1M files. That is the in-RAM metadata
  store of seed. It is not part of this question.

## Fit and extrapolation

At 100k files and below, the peak comes from a per-pack part (see below). At
1M files, the GC mark is the largest part. These are the heap peaks:

- 1 snapshot (2 roots): 385 MB for 1M files = about 385 bytes per file.
- 4 snapshots (5 roots): 735 MB for 1M files = about 735 bytes per file.
- So the heap is about N x (150 + 117 x R) bytes, with R = 1 + snapshots.
- RSS of the real rehash (1 snapshot): (832 - 260) MiB / 900k files = about
  670 bytes per file. About half of this is file pages (see above).

| Files | Heap, 1 snapshot | Heap, 4 snapshots | Rehash RSS, 1 snapshot |
|---|---|---|---|
| 1M (measured) | 0.36 GiB | 0.68 GiB | 0.81 GiB |
| 10M | about 3.6 GiB | about 6.8 GiB | about 6.4 GiB |
| 100M | about 36 GiB | about 68 GiB | about 62 GiB |

The cost depends only on the file and directory count, not on file size: a
big file adds one content key, the same as a tiny one. So the rule
"never O(file size)" holds. But "bounded" does not hold. The heap goes above
1 GiB at about 2.8 million files (1 snapshot) or 1.5 million files
(4 snapshots); the rehash RSS goes above 1 GiB at about 1.3 million files.

## Where the memory comes from

All lines are at the head of `develop` (`a882251`). Heaptrack at 1M files
showed these blocks at the peak (sizes in MB):

1. **The key list of the mark** (`metadata::gc::mark`, gc/mod.rs:112-123).
   It is one `Vec<ObjectKey>` (33 bytes per key) for all roots. It is
   sorted and deduplicated only after every root is in it. Each root adds,
   for each file, its inode node key, its manifest node key and its content
   key (3 keys = 99 bytes), and each directory adds 2 keys. A snapshot of
   the same tree adds the same keys again before the dedup. The `Vec` grows
   by doubling, so its capacity is up to 2 times the keys:
   264 MB at 1M files and 2 roots, 529 MB at 5 roots (gc/mod.rs:119-120).
2. **The per-root lists of `referenced`** (store.rs:1580-1601). For each
   root, they hold the node keys (66 MB at 1M files) and the content keys
   (35 MB) before they go into the list of (1).
3. **Whole map nodes in RAM** (`load_u64_digest_map`, store.rs:1647-1680).
   The inode, directory and manifest maps of a root are each read whole
   (41 bytes per entry) and copied into a `BTreeMap` (66 MB of tree nodes
   at 1M files).
4. **Per pack** (lib.rs:1525 at open, lib.rs:2735 `pack_entries` in the
   GC). The `.idx` file of a pack is read whole and decoded into a
   `Vec<(ObjectId, Loc)>`: 30 MB plus 43 MB for a pack of 600k tiny
   objects. This part has a limit (the pack limit of 64 MiB), but with tiny
   objects one pack can hold more than 1 million objects, so it can be
   about 150 MB. This is the peak of all commands below about 200k files.
5. **Small parts that grow with the object count**: the Bloom filter of
   each index run (about 2.5 bytes per object, 15 MB for 6M objects) and
   the Bloom filter of the normal GC (1.25 bytes per object). These are
   small and by design.

RSS is higher than the heap. One reason is the file pages of the mapped
index runs. Another likely reason (not tested) is that freed heap blocks
stay in the process: the offline commands do not call
`alloc_tune::tune_at_start` (only `mount` does).

The help of `gc --full` says "bounded RAM". That is true only for the sorted
mark of the object IDs, not for the key list (1) in front of it.

## A bounded version (sketch; items 1, 2 and 4 are done, see "After the fix")

The parts for this are already in the code: the external sorter
(`chunk_store::extsort`, used by `ExactMark`) and the windowed record
readers of the rehash engine.

1. **Stream the keys of the walk to an external sort.** Push every key from
   the walk into an `extsort::Sorter` (33-byte records) and not into a
   `Vec`. Then read the sorted, deduplicated keys back one at a time, and
   for each one run `for_each_file_object` into the mark (the exact mark or
   the Bloom filter). RAM: one sort chunk (4 MiB) and the merge buffers
   (32 x 64 KiB). Disk: 33 bytes per key per root in temp files, about
   2 GB for 10M files and 2 roots. Time: one extra sequential write and read
   of that file, a few seconds per GB.
2. **Read the map nodes in windows.** Replace `load_u64_digest_map` in the
   GC walk with a reader that gives one window of records at a time (as the
   rehash engine does for the same nodes). Nothing then holds a whole map
   or a `BTreeMap`.
3. **Decode `.idx` files in windows** at open and in `pack_entries`, so the
   per-pack part is a fixed buffer and not about 120 bytes per object of
   the pack. This is less urgent, because the pack limit already bounds it.
4. **Call `tune_at_start`** in the offline commands. This is a small change,
   and it is expected (not tested) to bring RSS closer to the heap.

Rough cost: items 1 and 2 are 2 to 4 days with tests (one counting-allocator
test at two file counts, as in `gc_full_memory_tests.rs`, but for the
metadata walk). Item 3 is 1 to 2 days. Item 4 is less than an hour. After
1 and 2, the GC and rehash heap is expected to stop growing with the file
count: the per-pack part (up to about 150 MB) stays until item 3, then a few
MB of buffers. The extra cost is temp disk space and one more sequential
pass over the key file (not measured).

The mount uses the same `mark` function for its online GC, so the same fix
also bounds the GC memory of a mount with many files.

## Recommendation

Do not accept the RAM of the GC step as "bounded". Its heap is about 385
bytes per file with one snapshot, and about 117 bytes more per file for each
extra snapshot. It goes above 1 GiB at about 2.8 million files (1 snapshot)
or 1.5 million files (4 snapshots), and the RSS of a rehash goes above 1 GiB
at about 1.3 million files. Make a separate issue for a bounded mark (items 1 and 2
above, with item 4 as a quick fix). Also correct the "bounded RAM" text of
`gc --full` and the "33-byte key per node and file" line in
`docs/design/chunk-store.md` until that issue is done. For the rehash
engine itself, the data agrees with HASH-05: the dry-run heap is flat
(75 to 95 MB, all at the store open), and the rewrite of a real run peaks
below its GC step. The rewrite was not measured alone.

## After the fix (#207)

The fix does items 1, 2 and 4 of the sketch above. Item 3 (the `.idx`
decode in windows) is not done; see below. The mark walk now gives every
key to an external sort in `<store>/gc-tmp` and reads the sorted keys
back one at a time, and it reads the map and manifest nodes in 1 MiB
windows. The offline commands call `alloc_tune::tune_at_start`.

Same script, same machine: `scripts/gc-memory/run.sh 100000 1000000`
with `GCMEM_HEAPTRACK=1`, once with `GCMEM_SNAPS=1` and once with
`GCMEM_SNAPS=4`. Heap peak in MB (heaptrack). The "before" values are
from the tables above.

| Files | Snapshots | heap gc | heap gc --full (before) | heap dry-run | heap rehash (before) |
|---|---|---|---|---|---|
| 100,000 | 1 | 74 | 75 (75) | 74 | 204 |
| 1,000,000 | 1 | 96 | 109 (385) | 95 | 258 |
| 100,000 | 4 | 75 | 75 | 75 | 204 |
| 1,000,000 | 4 | 101 | 109 (735) | 95 | 258 (764) |

Peak RSS in MiB, anon in parentheses. "Before" is from the tables above
(no 100k row with 4 snapshots was measured before).

| Files | Snapshots | Run | gc | gc --full | dry-run | rehash | rehash wall |
|---|---|---|---|---|---|---|---|
| 100,000 | 1 | before | 153 | 233 | 76 | 260 (214) | 11 s |
| 100,000 | 1 | after | 125 (98) | 200 (174) | 75 (5) | 104 (68) | 4.3 s |
| 1,000,000 | 1 | before | 562 | 559 | 258 | 832 (399) | 117 s |
| 1,000,000 | 1 | after | 387 (172) | 494 (279) | 255 (19) | 337 (103) | 51 s |
| 100,000 | 4 | after | 125 (101) | 176 (152) | 76 (5) | 100 (47) | 6.0 s |
| 1,000,000 | 4 | before | 859 | 907 | 258 | 861 (615) | 68 s |
| 1,000,000 | 4 | after | 386 (171) | 462 (246) | 256 (77) | 337 (101) | 69 s |

(The rehash rows of this table ran on a store that heaptrack had
converted first; see note (1) above. The heaptrack column is the real
conversion.)

Acceptance:

- **Snapshots.** At 1M files, the heap with 4 snapshots is the heap with
  1 snapshot within 5 MB in every command (before: +350 MB for
  `gc --full`). So the heap does not grow with the snapshot count.
- **Files.** From 100k to 1M files (10 times the files), the heap of
  `gc` and `gc --full` grows by 21 to 35 MB, and the heap of a rehash by
  54 MB (before: +310 MB for `gc --full` with 1 snapshot). Margin as
  measured: the heap at 1M files is at most 35 MB (GC) or 55 MB (rehash)
  above the heap at 100k files. This rest is not fixed: the Bloom filters
  of the index runs (item 2 below) grow with the object count, about
  15 bytes per tiny file, so about 135 MB more at 10M files.
- At the peak of every command, no allocation of the metadata walk is in
  the top sites (heaptrack): the mark keeps only its fixed buffers.

What is left, and why it grows a little from 100k to 1M files (sites at
the heap peak):

1. **Per pack: the `.idx` read and decode** (`pack_io.rs:427` and
   `decode_index`, at the open and in `pack_entries`): 73 MB at 100k
   files, 93 MB at 1M files. The packs of the 1M store are fuller (more
   tiny objects per 64 MiB pack). This part has the limit of one pack,
   up to about 150 MB with tiny objects.
2. **The Bloom filters of the index runs** (`run.rs:769`): 0.9 MB at 100k
   files, 15 MB at 1M files (about 2.5 bytes per object). By design.
3. **The sweep of a GC that frees much** (rehash, step 6): the plans of
   one group of up to 32 packs (`gc_prepare`, 151 MB, the same at both
   sizes) and the dead sets of the group (36 MB), and the copy of the
   memtable (`Index::snapshot`, 6 to 20 MB, bounded by the memtable
   limit). These are bounded by the group size and the pack limit, not by
   the file count.

Item 3 of the sketch is not done: the callers need the entries of a
pack as a list (the open replay, the active pack, `gc_prepare` and the
compaction keep the entries of a pack), and the CRC of an `.idx` covers
the whole file. A windowed decode alone does not remove that list, so it
is not a small change, and the pack limit already bounds this part.

The RSS stays above the heap: the file pages of the mapped index runs
(RSS minus anon), and anon memory that the allocator keeps (anon minus
heap, for example 172 MiB anon for 96 MB heap in `gc` at 1M files). The
RSS went down by 12 to 60 % from before.

Disk: the sort of the keys takes about 33 bytes per key per root in
`<store>/gc-tmp` (about 100 bytes per tiny file per root), removed at the
end. The wall time of `gc --full` at 1M files and 1 snapshot is 73 s.

Costs of the change for every GC (also the online GC of a mount): the
temp disk space above must be free before the GC frees anything, so a GC
on a full disk can fail (then it deletes nothing). A stop request is not
checked during the merge of the sorted keys (sequential disk I/O, a few
seconds per GB).
