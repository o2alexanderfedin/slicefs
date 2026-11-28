# Prolly trees for the metadata maps (#19)

Plan 24-05, run again on 2026-11-26 after the fixes of the code review
(`.planning/phases/24-prolly-evaluation/24-REVIEW.md`, section "Fixes"): a
new edit pattern with real inserts and deletes (`insdel`), the disk sizes
with the pack index, the same timed regions in both map modes, and checks
of the prototype. Every number here is from the new run. The data is in
`.planning/phases/24-prolly-evaluation/runs/`: `prolly-results.csv` (one row
per commit, mode and run, and one `median` row per mode, pattern and K),
`prolly-scale.csv`, `prolly-disk.csv`, `prolly-edits.csv`,
`prolly-batches.csv`, `prolly-shape.txt` and `prolly-machine.txt`. Every
table number is a median row value of `prolly-results.csv`,
`prolly-scale.csv` or `prolly-disk.csv`, with three exceptions: the tree
shape and `maxrss_after_load` come from `prolly-shape.txt` (median of the
runs), the "Estimated whole commit" columns are computed from median values
by the formula in the column name, and the CPU columns are (user + system)
x 1000 of the median row. The script
`.planning/phases/24-prolly-evaluation/runs/report-tables.py` prints every
table from the CSV files.

## Summary

**Since Phase 26 the metadata maps are prolly trees** (#215). The product
run of plan 26-13 gives `verdict pmap07=pass`: at 1,000,000 files a
one-change commit takes 6.8 ms in place of 528.6 ms (1.6 in place of 515.1
CPU ms, 18 in place of 14,181 nodes), and all nine Boost rows for K up to
100 are better than flat. Two recorded rows are worse (K = 1,000 with
scatter and with real inserts and deletes); they are accepted, see
"Phase 26: the product run". The rest of this summary is the Phase 24
evaluation, before adoption.

**Phase 24 recommendation: later** (the same as before the fixes; see
"Recommendation" for the one Linux row that is not better)

- A prolly map for `inode_data` does much less work per small commit than
  the flat map. For one changed file (scatter, K = 1): 3.3 KB serialized in
  place of 3,448.3 KB, 0.6 CPU ms in place of 6.6, and 2.6 KB new stored in
  place of 15.4 KB. The peak RSS of the process is 33.7 MiB in place of
  175.4 MiB.
- The `scatter` and `cluster` commits change mostly values: their creates
  and deletes cancel inside the commit, so K = 1,000 changes about 500
  values. The new `insdel` commits insert and delete K keys in the middle of
  the map, so node boundaries move. Prolly stays better up to K = 100 on
  macOS (10.1 against 12.5 CPU ms, 30.8 against 34.5 ms at K = 100), but
  the margin is small at K = 100, and at K = 1,000 prolly is slower (46.3
  against 24.2 CPU ms).
- At the Boost scale (84,104 inodes) the gain on the whole commit is small.
  The estimated whole commit with a prolly `inode_data` is 58.4 ms in place
  of 63.9 ms (91.4 %), and 36.8 CPU ms in place of 42.9 (85.8 %). In the
  rows where prolly helps the gain is 3.7 to 8.3 ms per commit, below the
  10 ms limit of the rubric. At K = 1,000 (scatter and insdel) and at the
  version step, prolly is slower than flat. A Linux Docker run, where a
  sync is cheap, gives 3.4 to 6.5 ms (9.8 to 23.2 %) less per commit for K
  up to 10, and at insdel K = 100 prolly is a little slower than flat there
  (16.7 against 15.4 ms, 13.4 against 12.7 CPU ms).
- The gain grows with the map. With 4,000,000 entries one changed value
  costs flat 242.1 ms (221.2 CPU ms, 164,000 KB serialized, 591.5 MiB peak
  RSS), and prolly 22.0 ms (0.9 CPU ms, 10.7 KB, 85.7 MiB). By the scale
  rows, one flat map passes 100 ms per commit at about 1.6 million entries.
- Disk, pack index included: over the 121 commits after the bulk step (80
  small commits, the version step and 40 `insdel` commits), the flat store
  grew by 64.4 MiB and the prolly store by 38.4 MiB (apparent size, before
  GC).
- File manifests are already content-cut trees, and every metadata map node
  is already a FastCDC-cut value. A commit stores only the changed chunks of
  a map. The cost that is left is the full serialization, the hashing and the
  full in-RAM maps.
- Adoption is a store format change and an owner decision (Q-1). This report
  does not take it.

## Setup

- Machine (from `prolly-machine.txt`): macOS (Darwin 25.2.0), x86_64,
  Intel Xeon W-3245 at 3.20 GHz, 32 CPUs, 192 GiB of memory, APFS. Commit
  `9a26452` (the last fix commit of the review). `rustc 1.95.0-nightly`,
  release build. The `rustc` line of `prolly-machine.txt` was filled in
  after the run from the toolchain that built the binary: the wrapper that
  waited for a quiet machine started `bench.sh` without `rustc` in its
  `PATH`, so the script wrote `unknown`.
- Load: the run was to start when the 1-minute load came down to 8 or
  less, with no `slicefs-gcmem` container (the Docker runs of another
  agent). The wait was limited to 30 minutes. In that time the load stayed
  at 13 to 18 (jobs of other projects, and short Docker containers of
  another project), so the wait expired and the run started at a load of
  13.30 (`load_before=13.30 14.65 16.45`, `load_after=14.25 14.62 16.11`).
  `docker ps` showed no `slicefs-gcmem` container at the start and at the
  end, and no cargo job of this work ran beside the benchmark. Other jobs
  were not counted; the 1-minute load average of 13 to 15 (of 32 CPUs)
  is about twice that of the first run. Byte, object and node counts do
  not change with load; times can.
- Start 2026-11-26T22:18:56Z, end 22:21:47Z: 171 s for 3 runs.
- Inputs: Boost 1_84_0 (the seed tree; `inode_data_entries=84104` in the
  `info` line of `prolly-shape.txt`) and Boost 1_85_0 (the version step:
  18,991 files changed, a count that includes the files that the edit
  script changed before; 1,588 files created and 734
  deleted; 150 directories created and 28 deleted; 85,083 entries after
  it). After the `insdel` commits the final map has 84,173 entries.
- Command lines:

  ```sh
  cargo build --release -p prolly-bench
  WORK="$M/results/prolly-2026-11-26" BIN=target/release/prolly-bench \
    timeout 5400 bash scripts/prolly-bench/bench.sh "$M/data" 3
  ```

  with the defaults `BOOST_VERS="1_84_0 1_85_0"`, `KS="1 10 100 1000"`,
  `COMMITS=10`, `PATTERNS="scatter cluster insdel"`, `SEED=24` and
  `SCALE_NS="100000 1000000 4000000"`. `M` is the measure dir outside the
  repo.
- Runs: 3, no fallback. 1,098 run rows and 42 median rows. The flat and the
  prolly digest checks gave `ok=1` in all 3 runs.
- The edit script before the `insdel` commits is the same as in the first
  run of plan 24-05: in every one of those rows (seq 0 to 81) the entries,
  serialized bytes, new objects, new raw bytes and nodes read and written
  are equal to the first run, on macOS and on Linux. Only the times changed.
- The files `runs/prolly-quick-*` are the quick pass of the first run (the
  code before the fixes). Their numbers are not used here.
- Unchanged by this phase: FastCDC 4/8/32 KiB, BLAKE3-256 IDs and the 1 TiB
  cap (D-04). The store format does not change.

## Method

Three modes run on the same edit script. Each mode runs in its own process
and its own store:

- **`whole`** is the real commit of `DictMetadataStore` with all six maps:
  `begin_commit` (it clones every map) and `finish_commit` (it interns every
  map in full, writes the root record and syncs). This is the cost that a
  user sees.
- **`flat`** is the current design for one map. It holds `inode_data` in RAM
  and interns it in full on each commit, with a copy of
  `intern_u64_digest_map` through `store_io::put_value`, then `cs.sync()`.
- **`prolly`** is the prototype for the same map. It bulk-builds the first
  map, then does one `prolly::apply` of the changes and one `cs.sync()` per
  commit.
- In both map modes the input is outside the timed region: `flat` fills its
  map before the timer, and the prolly bulk build reads `base.bin` in pieces
  outside the timer and times only the pushes, `finish` and the sync. The
  same holds for the synthetic values of the scale rows.

Why three processes, and not one map inside the real commit: the commit
internals of `metadata` are private, and this phase does not change
`metadata`. Also `inode_map` and `inode_data` hold the same entries
(`crates/metadata/src/store.rs` lines 1112-1113), so FastCDC cuts both into
mostly the same chunks. A stats delta of `inode_data` inside the real commit
thus reads near 0. So `flat` and `prolly` have their own stores with only
this map. They replay the real `inode_data` change of each `whole` commit
(the `batches` step), and each ends with a digest check against the real
final map.

The target map is `inode_data` (D-09). It is the largest map (with
`inode_map`), every file change touches it, and its values have a fixed
width (41-byte entries), so "boundaries depend only on the keys" is exact
for it. It is one of the four maps with one entry per inode or file
(`inode_map`, `inode_data`, `dir_data`, `manifest_data`), and about a third
of the bytes that a commit serializes. So a gain of `prolly` over `flat` for
this map is not the gain of the whole commit. The table "Estimated whole
commit" gives `whole - flat + prolly`: an estimate, not a measurement.

Edit script (all from seed 24, the same in every run):

- Setup commit (seq 0): makes a `bench` directory.
- Small commits: for each pattern (`scatter`, `cluster`) and each K (1, 10,
  100, 1,000), 10 commits of K edits. Edit i of commit seq has the kind
  (i + seq) mod 4: a metadata change (mtime and ctime), a content change
  (new 4 KiB content), a create (a new file in `bench`), or a delete (the
  oldest file that the script made). `scatter` picks each file at random.
  `cluster` picks one start per commit and takes the next files in inode
  order. New inode numbers only grow, so creates go to the right edge of
  the map in both patterns. The "entries changed" column is the measured
  `inode_data` change of a commit. It is about K / 2: a delete takes the
  oldest file that the script made, and that is mostly a file of the same
  commit, so most creates and deletes cancel (`prolly-batches.csv`: 502
  puts and 2 deletes for K = 1,000). So these commits change mostly values,
  plus a few entries at the right edge. With fixed-width values a changed
  value moves no node boundary, which is the best case for prolly.
- Version step: the 1_85_0 tree as one commit, matched by content key. A
  file with the same content key stays; a changed file gets the new
  content; new files and directories are made; names that are only in the
  store are removed.
- `insdel` commits (after the version step, so the commits before them are
  the same as without them): for each K, 10 commits of K edits. Edit i of
  commit seq is a delete when (i + seq) mod 2 is 0, else an insert. A
  delete removes a random file of a random directory of the tree. An insert
  makes a file that an earlier commit deleted again, at its freed inode
  number, with its name and content and an mtime one second later
  (`create_inode_at`); with no such file the edit is a delete. So every edit
  inserts or deletes one key in the middle of the committed map, and the
  `inode_data` change is exactly K entries. For K = 1 the commits are one
  delete or one insert in turn; for K of 10 and more, 9 of the 10 commits
  of each K are half inserts and half deletes, and the first is mostly
  deletes. Inserts
  and deletes in the middle move node boundaries, so prolly rewrites nodes
  up to the next resync. The product never uses a freed inode number
  again, so for `inode_data` the inserts are a stand-in: they show the cost
  for a map whose keys can appear in the middle.

That is 122 commits per mode and run. The columns that matter:

- `total_ms`: wall time of the commit, sync included. `begin_ms` of `whole`
  is the clone of every map.
- CPU ms: user plus system CPU time of the commit (`getrusage`).
- `serialized_bytes`: bytes serialized and hashed by the commit.
- `new_objects` and `new_stored_bytes`: new objects and new stored bytes in
  the chunk store (a `ChunkStore::stats()` delta; a deduplicated put adds 0).
- `nodes_read` and `nodes_written`: prolly nodes read and written by
  `apply`.
- `peak_rss_bytes`: peak RSS of the process (`/usr/bin/time -l`).
- The on-disk size of each store at the start and at the end, each after a
  close of the store, so the pack index is counted in all three modes.

The prototype has no node cache (D-17). `nodes_read` shows the read cost
without one: every `apply` reads its paths from the store.

The `scale` rows are synthetic: N sorted entries of the same shape (8-byte
key, 33-byte value) for N = 100,000, 1,000,000 and 4,000,000. Each row is a
build, then one changed middle value (`apply1`), in a new store and
process.

How to read the wall times: each commit ends with one `cs.sync()`, which is
`F_FULLFSYNC` on macOS. On this machine the wall time of a commit minus its
CPU time is about 20 ms. Computed from the run rows (the median of the 30
rows of each group): the gap is 19.9 to 21.1 ms in 28 of the 36
small-commit groups, and 23.3 to 25.9 ms in the other eight (the K = 1,000
groups of `whole`, of `flat` and `prolly` scatter and insdel, and `whole`
insdel K = 100). The gap is about the same in the three modes. In this run
no group had the 40 ms syncs of the first run. So for small commits the
wall time is mostly the sync, and a difference of one sync between two
medians comes from the disk, not from the work of the map. The CPU time,
the serialized bytes and the scale rows show the work. The decision below
uses them first (a reading of plan 24-05, recorded in its summary). On
Linux (see "Linux Docker run") the gap is about 2 to 5 ms.

Limits:

- The page cache is warm (not dropped).
- In process, with no FUSE and no mount.
- Sizes are before GC: dead nodes and chunks stay in the packs.
- The scale rows are synthetic, not Boost.
- One machine and one file system (APFS) for the main result. A Linux
  Docker run is in "Linux Docker run" below.

## Results

### Commit time and CPU

| pattern | K | entries changed | whole ms | whole begin ms | whole CPU ms | flat ms | flat CPU ms | prolly ms | prolly CPU ms |
|---|---|---|---|---|---|---|---|---|---|
| setup | 0 | NA | 88.7 | 14.7 | 45.8 | - | - | - | - |
| bulk | 0 | 84,105 | - | - | - | 58.9 | 32.6 | 118.0 | 66.7 |
| scatter | 1 | 1 | 63.9 | 13.5 | 42.9 | 26.1 | 6.6 | 20.6 | 0.6 |
| scatter | 10 | 6 | 61.8 | 12.2 | 40.9 | 28.2 | 6.8 | 21.8 | 1.4 |
| scatter | 100 | 54 | 66.2 | 10.4 | 44.6 | 31.0 | 10.4 | 26.6 | 6.0 |
| scatter | 1000 | 503 | 100.4 | 10.9 | 74.4 | 46.1 | 22.7 | 57.7 | 34.2 |
| cluster | 1 | 1 | 56.7 | 8.4 | 36.0 | 26.6 | 6.1 | 20.7 | 0.5 |
| cluster | 10 | 10 | 57.3 | 8.3 | 36.6 | 26.4 | 6.1 | 20.8 | 0.7 |
| cluster | 100 | 54 | 57.1 | 8.1 | 36.2 | 26.6 | 6.3 | 20.8 | 0.7 |
| cluster | 1000 | 504 | 67.3 | 8.4 | 42.3 | 26.8 | 6.3 | 21.6 | 1.3 |
| insdel | 1 | 1 | 59.0 | 10.5 | 38.7 | 28.6 | 6.6 | 20.9 | 0.6 |
| insdel | 10 | 10 | 59.6 | 8.5 | 39.1 | 30.4 | 7.3 | 22.1 | 1.9 |
| insdel | 100 | 100 | 77.1 | 8.5 | 51.8 | 34.5 | 12.5 | 30.8 | 10.1 |
| insdel | 1000 | 1,000 | 127.5 | 10.7 | 101.2 | 49.5 | 24.2 | 69.5 | 46.3 |
| version | 0 | 21,521 | 151.3 | 13.8 | 105.1 | 51.2 | 29.0 | 117.1 | 92.1 |

The whole commit costs about 57 to 66 ms (36 to 45 CPU ms) for up to 100
changed files of `scatter` and `cluster` and for up to 10 of `insdel`
(77.1 ms and 51.8 CPU ms at insdel K = 100). Of that, `begin_commit` (the
clone of every map) is 8.1 to 13.5 ms. For `inode_data` alone, flat costs
about 6 to 7 CPU ms per commit up to K = 10 and in every cluster row (10.4
at scatter K = 100 and 12.5 at insdel K = 100), because it serializes and
hashes the full map every time. Prolly costs 0.5 to 1.9 CPU ms up to K = 10
and for every cluster commit, and its cost grows with the number of touched
leaves: 6.0 CPU ms at scatter K = 100 and 34.2 at scatter K = 1,000 (more
than flat's 22.7). The `insdel` commits cost prolly more for the same K,
because each edit is a real insert or delete (K entries changed, not about
K / 2) and moves boundaries: 10.1 CPU ms at K = 100 (flat 12.5) and 46.3 at
K = 1,000 (flat 24.2). The version step (21,521 changed entries) costs
prolly 92.1 CPU ms and flat 29.0: a large batch is cheaper as a full
rewrite. In this run every prolly wall time for K up to 100 is lower than
flat's.

### Bytes per commit

| pattern | K | whole serialized KB | whole new objects | whole new stored KB | flat serialized KB | flat new objects | flat new stored KB | prolly serialized KB | prolly new objects | prolly new stored KB | prolly nodes read | prolly nodes written |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| setup | 0 | 10,345.2 | 18 | 57.7 | - | - | - | - | - | - | - | - |
| bulk | 0 | - | - | - | 3,448.3 | 323 | 2,832.5 | 3,846.2 | 1,026 | 2,988.9 | 0 | 1,026 |
| scatter | 1 | 10,345.2 | 10 | 36.3 | 3,448.3 | 2 | 15.4 | 3.3 | 3 | 2.6 | 6 | 3 |
| scatter | 10 | 10,345.2 | 26 | 108.1 | 3,448.3 | 6 | 60.2 | 38.3 | 11 | 28.1 | 18 | 12 |
| scatter | 100 | 10,345.4 | 185 | 773.1 | 3,448.4 | 48 | 487.6 | 246.2 | 64 | 188.4 | 80 | 64 |
| scatter | 1000 | 10,345.4 | 1,415 | 3,950.6 | 3,448.4 | 244 | 2,281.3 | 1,592.5 | 401 | 1,232.7 | 417 | 401 |
| cluster | 1 | 10,345.4 | 11 | 34.2 | 3,448.4 | 2 | 17.7 | 3.4 | 3 | 2.7 | 6 | 3 |
| cluster | 10 | 10,345.4 | 29 | 61.9 | 3,448.4 | 3 | 20.6 | 11.8 | 5 | 9.0 | 9 | 5 |
| cluster | 100 | 10,345.4 | 120 | 70.4 | 3,448.4 | 4 | 28.3 | 13.0 | 6 | 10.1 | 10 | 6 |
| cluster | 1000 | 10,345.4 | 1,023 | 154.9 | 3,448.4 | 5 | 43.4 | 34.9 | 11 | 26.9 | 15 | 11 |
| insdel | 1 | 10,465.4 | 7 | 31.0 | 3,488.4 | 2 | 18.6 | 9.1 | 3 | 6.8 | 6 | 3 |
| insdel | 10 | 10,464.2 | 50 | 245.9 | 3,488.0 | 11 | 113.3 | 68.7 | 18 | 51.7 | 26 | 18 |
| insdel | 100 | 10,454.4 | 359 | 1,644.4 | 3,484.7 | 82 | 861.9 | 467.2 | 112 | 345.2 | 133 | 117 |
| insdel | 1000 | 10,353.5 | 2,396 | 5,094.5 | 3,451.1 | 258 | 2,424.0 | 2,205.4 | 555 | 1,685.3 | 580 | 564 |
| version | 0 | 10,465.5 | 1,784 | 5,495.9 | 3,488.4 | 312 | 2,861.1 | 3,757.9 | 991 | 2,918.1 | 991 | 991 |

Serialized and hashed per commit is not the same as new stored per commit
(D-24). The whole commit serializes and hashes about 10.3 to 10.5 MB on
every commit, whatever K is, but stores only 31.0 to 36.3 KB for one
changed file: the FastCDC cut of `store_io` keeps every unchanged chunk.
For `inode_data`, flat serializes 3,448 to 3,488 KB per commit and prolly
3.3 to 9.1 KB for one change. New stored bytes are small in both designs,
and prolly stores less in every small-commit row: 2.6 KB against 15.4 KB at
scatter K = 1, 188.4 KB against 487.6 KB at scatter K = 100, and 345.2 KB
against 861.9 KB at insdel K = 100 (at the version step 2,918.1 against
2,861.1 KB, 1.02 x). Write amplification of prolly: one change writes one
path of 3 nodes (depth 3) and reads 6; 54 scattered value changes write 64
nodes; 100 inserts and deletes write 117 and read 133; 1,000 inserts and
deletes write 564 (401 for scatter K = 1,000). Flat writes the changed
FastCDC chunks and the manifest leaf of the map again (2 objects at K = 1,
48 at scatter K = 100, 82 at insdel K = 100).

### Estimated whole commit with a prolly inode_data (estimate, not measured)

| pattern | K | whole ms | flat ms | prolly ms | estimate ms (whole - flat + prolly) | estimate / whole | whole CPU ms | estimate CPU ms (whole - flat + prolly) | estimate CPU / whole CPU |
|---|---|---|---|---|---|---|---|---|---|
| scatter | 1 | 63.9 | 26.1 | 20.6 | 58.4 | 91.4 % | 42.9 | 36.8 | 85.8 % |
| scatter | 10 | 61.8 | 28.2 | 21.8 | 55.4 | 89.7 % | 40.9 | 35.5 | 86.7 % |
| scatter | 100 | 66.2 | 31.0 | 26.6 | 61.8 | 93.4 % | 44.6 | 40.2 | 90.2 % |
| scatter | 1000 | 100.4 | 46.1 | 57.7 | 112.0 | 111.5 % | 74.4 | 85.9 | 115.6 % |
| cluster | 1 | 56.7 | 26.6 | 20.7 | 50.8 | 89.7 % | 36.0 | 30.4 | 84.4 % |
| cluster | 10 | 57.3 | 26.4 | 20.8 | 51.8 | 90.3 % | 36.6 | 31.1 | 85.0 % |
| cluster | 100 | 57.1 | 26.6 | 20.8 | 51.3 | 89.9 % | 36.2 | 30.7 | 84.7 % |
| cluster | 1000 | 67.3 | 26.8 | 21.6 | 62.1 | 92.2 % | 42.3 | 37.3 | 88.2 % |
| insdel | 1 | 59.0 | 28.6 | 20.9 | 51.3 | 87.0 % | 38.7 | 32.7 | 84.4 % |
| insdel | 10 | 59.6 | 30.4 | 22.1 | 51.3 | 86.0 % | 39.1 | 33.7 | 86.2 % |
| insdel | 100 | 77.1 | 34.5 | 30.8 | 73.4 | 95.2 % | 51.8 | 49.4 | 95.4 % |
| insdel | 1000 | 127.5 | 49.5 | 69.5 | 147.6 | 115.7 % | 101.2 | 123.3 | 121.8 % |
| version | 0 | 151.3 | 51.2 | 117.1 | 217.2 | 143.5 % | 105.1 | 168.3 | 160.1 % |

This is an estimate: the real commit does not run with a prolly map. If
`inode_data` were a prolly map, a small commit at the Boost scale would be
about 4 to 8 ms faster: 4.8 to 14.0 % of the wall time and 4.6 to 15.6 %
of the CPU time (3.7 to 8.3 ms and 2.4 to 6.1 CPU ms in the rows where
prolly helps). The smallest gain is at insdel K = 100 (95.2 % of the wall
time, 95.4 % of the CPU time). For K = 1,000 of scatter and insdel and for
the version step the estimate is worse than today. A prolly map for all
four O(N) maps would gain more (not measured), but the clone in
`begin_commit` and the fixed costs of a commit stay.

### Peak memory

| mode | peak RSS of the process (MiB) | ru_maxrss after the store load (MiB) | ru_maxrss after the last commit (MiB) |
|---|---|---|---|
| whole | 478.2 | 104.0 | 478.2 |
| flat | 175.4 | - | 175.3 |
| prolly | 33.7 | - | 32.9 |

The whole process reaches 104.0 MiB after the store load and 478.2 MiB at
the end. Its high-water mark grows with the commits (from the
`maxrss_bytes` column: 133.8 MiB after the setup commit, 421.9 MiB after
the version step, and 468.9 MiB as the median of the `insdel` K = 1,000
rows), so it is not only the maps; the parts were not measured. The flat
process, which holds only `inode_data` and its chunk store, reaches 175.4
MiB (115.7 MiB after the version step); the prolly process 33.7 MiB (26.3
MiB after the version step), with no map in RAM. The last column is the
`maxrss_bytes` of the last commit of each run (median of the runs). These peaks are higher than in the first run
(390.0, 107.8 and 26.6 MiB) because the run now has 40 more commits, and
the macOS high-water marks grow with the commits. On Linux the same
processes peak at 153.2, 22.6 and 14.0 MiB, and the flat and prolly peaks
do not grow in the `insdel` commits (see "Linux Docker run"). The cause of
the higher macOS numbers was not measured.

### On-disk size

| mode | start apparent MiB | end apparent MiB | growth apparent MiB | start allocated MiB | end allocated MiB | growth allocated MiB | files at end |
|---|---|---|---|---|---|---|---|
| whole | 283.2 | 513.5 | 230.3 | 283.2 | 514.2 | 231.0 | 21 |
| flat | 2.7 | 67.1 | 64.4 | 2.7 | 67.7 | 64.9 | 6 |
| prolly | 2.9 | 41.3 | 38.4 | 2.9 | 41.7 | 38.7 | 4 |

All sizes are taken after a close of the store, so the pack index is in
all three modes (in the first run the flat and prolly sizes did not count
it). After the 121 commits after the bulk build (80 small commits, the
version step and 40 `insdel` commits), with no GC, the prolly store of
`inode_data` grew by 38.4 MiB and the flat store by 64.4 MiB (40 % less).
The start sizes are close (2.9 against 2.7 MiB): a prolly node holds 41
bytes per entry plus the length fields, and its 33-byte digests do not
compress. The growth of `whole` includes the 4 KiB content of the content
edits and the creates, and the content of the version step.

### Scale

| n | op | flat ms | flat CPU ms | flat serialized KB | flat peak RSS MiB | prolly ms | prolly CPU ms | prolly serialized KB | prolly new objects | prolly peak RSS MiB |
|---|---|---|---|---|---|---|---|---|---|---|
| 100,000 | build | 61.1 | 36.9 | 4,100.0 | 32.5 | 106.1 | 80.8 | 4,573.0 | 1,218 | 8.5 |
| 100,000 | apply1 | 29.4 | 9.3 | 4,100.0 | 32.5 | 20.6 | 0.7 | 7.6 | 3 | 8.5 |
| 1,000,000 | build | 462.6 | 341.2 | 41,000.0 | 164.1 | 977.8 | 791.0 | 45,734.6 | 12,245 | 36.7 |
| 1,000,000 | apply1 | 64.4 | 44.1 | 41,000.0 | 164.1 | 44.6 | 0.9 | 10.9 | 4 | 36.7 |
| 4,000,000 | build | 1,894.7 | 1,382.8 | 164,000.0 | 591.5 | 3,591.7 | 3,054.1 | 182,940.7 | 49,012 | 85.7 |
| 4,000,000 | apply1 | 242.1 | 221.2 | 164,000.0 | 591.5 | 22.0 | 0.9 | 10.7 | 4 | 85.7 |

The cost of one small change grows linearly with N for flat (9.3, 44.1 and
221.2 CPU ms), and stays near 0.8 CPU ms for prolly (one path of 3 or 4
nodes). The prolly wall time of 44.6 ms at 1,000,000 entries is the sync
effect of "Method" (0.9 CPU ms). Flat's peak RSS also grows with N (32.5
to 591.5 MiB); prolly's grows less (8.5 to 85.7 MiB; on Linux 7.9 to 22.4
MiB with the same 49,012 new objects at 4,000,000 entries; what uses the
memory was not measured). A bulk build is 1.7 to 2.1 times as slow for
prolly as a full flat intern; in this run the synthetic values are made
outside the timer in both modes. By linear interpolation of the flat
`apply1` wall times between the 1,000,000 and 4,000,000 rows, one flat map
passes 100 ms per commit at about 1.60 million entries (by CPU time alone,
at about 1.95 million). The whole commit interns three maps of this size
and clones five, so it passes 100 ms at a smaller inode count (not
measured).

### Tree shape

| field | value |
|---|---|
| inode_data entries after the seed | 84,104 |
| final tree: depth | 3 |
| final tree: nodes | 1,035 |
| final tree: leaves | 1,018 |
| final tree: leaf bytes min | 684 |
| final tree: leaf bytes mean | 3,729 |
| final tree: leaf bytes max | 6,804 |
| final tree: internal nodes | 17 |
| final tree: internal bytes mean | 3,111 |
| boundary hash ns per key (median of runs) | 99.5 |

The final tree (84,173 entries, after the `insdel` commits) has depth 3,
1,018 leaves and 17 internal nodes. The node size spread (#19): leaves are
684 to 6,804 bytes with a mean of 3,729, as the Weibull rule predicts (mean
about 0.906 x 4,096 plus the header). No node is near `NODE_MAX` (16,384).
The boundary hash costs 99.5 ns per key (D-12), so a bulk build of 84,104
keys spends about 8 ms in it.

### Linux Docker run

The same benchmark ran once more in the Linux Docker image of
`scripts/docker-linux-test` (a copy of its `prolly-bench-smoke` case with
the Boost data as a read-only volume and GNU `time` added, so that peak RSS
is measured). Data: `runs/prolly-linux-results.csv`, `prolly-linux-scale.csv`,
`prolly-linux-disk.csv`, `prolly-linux-shape.txt` and
`prolly-linux-machine.txt`, with the same rules as above (`--stem
prolly-linux` of the tables script).

- Machine (from `prolly-linux-machine.txt`): Linux 7.2 (OrbStack VM) on the
  same host, 32 CPUs, 16 GiB of memory for the VM, overlay file system for
  the stores, `rustc 1.100.0`, release build. `commit=unknown`, because git
  cannot read the read-only source mount in the container; the source was
  the same work tree at commit `9a26452`.
- Start 2026-11-26T22:22:19Z, end 22:25:15Z, 3 runs, no fallback, 6 digest
  checks `ok=1`. The load in the VM was 0.05 before and 1.06 after. The
  host load was 13.80 before and 15.19 after (jobs of other projects), so
  the host was not quiet. It ran right after the macOS run, not at the same
  time.

The byte, object and node counts are the same as on macOS, or close. In the
prolly rows the entries, serialized bytes, new objects and nodes read and
written are equal, and the stored bytes differ by at most 88 bytes (zstd of
other digest bytes). The flat and whole rows differ a little more, because
file times go into the inode digests and FastCDC cuts the flat map by
content. The times differ: on Linux a sync costs about 2 to 5 ms (median
wall minus CPU per small-commit group: 1.9 to 4.7 ms; the median over the
groups is 2.6, 2.4 and 2.8 ms for whole, flat and prolly), so the wall time
shows the work. The small-commit estimate is 3.4 to 6.5 ms less per commit
(9.8 to 23.2 %) for K up to 10. At insdel K = 100 prolly is a little slower
than flat (16.7 against 15.4 ms, 13.4 against 12.7 CPU ms; estimate 102.6
%), and the estimate is worse than today at K = 1,000 (scatter and insdel)
and at the version step. In the scale rows flat's `apply1` is 66.9 ms at
1,000,000 entries and 243.7 ms at 4,000,000 (the first run had 110.1 and
285.0 ms); prolly's is 2.8 to 5.2 ms. By the scale rows one flat map passes
100 ms per commit at about 1.56 million entries on Linux. Peak RSS is much
lower than on macOS for the same work.

<details>
<summary>Linux tables</summary>

#### Linux: Commit time and CPU

| pattern | K | entries changed | whole ms | whole begin ms | whole CPU ms | flat ms | flat CPU ms | prolly ms | prolly CPU ms |
|---|---|---|---|---|---|---|---|---|---|
| setup | 0 | NA | 69.3 | 8.0 | 39.9 | - | - | - | - |
| bulk | 0 | 84,105 | - | - | - | 44.5 | 35.2 | 75.6 | 69.1 |
| scatter | 1 | 1 | 27.9 | 5.8 | 25.4 | 9.0 | 6.1 | 2.5 | 0.3 |
| scatter | 10 | 6 | 29.4 | 5.8 | 26.7 | 9.3 | 7.2 | 4.9 | 2.3 |
| scatter | 100 | 54 | 36.9 | 5.9 | 34.4 | 12.4 | 10.3 | 9.5 | 7.6 |
| scatter | 1000 | 503 | 77.8 | 6.0 | 69.0 | 30.3 | 25.0 | 45.8 | 43.0 |
| cluster | 1 | 1 | 28.9 | 6.0 | 26.4 | 8.4 | 6.2 | 3.6 | 1.0 |
| cluster | 10 | 10 | 29.2 | 6.0 | 26.5 | 8.9 | 5.7 | 4.0 | 1.2 |
| cluster | 100 | 54 | 31.4 | 6.1 | 27.9 | 8.4 | 6.1 | 4.1 | 1.1 |
| cluster | 1000 | 504 | 38.2 | 6.2 | 34.8 | 9.3 | 6.1 | 4.9 | 1.9 |
| insdel | 1 | 1 | 30.9 | 6.6 | 28.4 | 8.1 | 6.1 | 4.2 | 1.4 |
| insdel | 10 | 10 | 34.7 | 6.5 | 31.6 | 9.3 | 7.0 | 5.9 | 3.1 |
| insdel | 100 | 100 | 53.0 | 6.8 | 49.3 | 15.4 | 12.7 | 16.7 | 13.4 |
| insdel | 1000 | 1,000 | 113.3 | 9.2 | 110.5 | 29.8 | 26.8 | 64.5 | 61.7 |
| version | 0 | 21,521 | 153.3 | 29.1 | 151.8 | 38.7 | 30.8 | 112.2 | 108.4 |

#### Linux: Bytes per commit

| pattern | K | whole serialized KB | whole new objects | whole new stored KB | flat serialized KB | flat new objects | flat new stored KB | prolly serialized KB | prolly new objects | prolly new stored KB | prolly nodes read | prolly nodes written |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| setup | 0 | 10,345.2 | 21 | 51.4 | - | - | - | - | - | - | - | - |
| bulk | 0 | - | - | - | 3,448.3 | 321 | 2,832.6 | 3,846.2 | 1,026 | 2,988.9 | 0 | 1,026 |
| scatter | 1 | 10,345.2 | 11 | 26.5 | 3,448.3 | 3 | 9.4 | 3.3 | 3 | 2.6 | 6 | 3 |
| scatter | 10 | 10,345.2 | 30 | 116.0 | 3,448.3 | 9 | 73.3 | 38.3 | 11 | 28.2 | 18 | 12 |
| scatter | 100 | 10,345.4 | 190 | 781.1 | 3,448.4 | 52 | 498.4 | 246.2 | 64 | 188.5 | 80 | 64 |
| scatter | 1000 | 10,345.4 | 1,421 | 3,944.5 | 3,448.4 | 246 | 2,283.0 | 1,592.5 | 401 | 1,232.7 | 417 | 401 |
| cluster | 1 | 10,345.4 | 10 | 43.9 | 3,448.4 | 2 | 21.5 | 3.4 | 3 | 2.7 | 6 | 3 |
| cluster | 10 | 10,345.4 | 30 | 73.0 | 3,448.4 | 3 | 31.4 | 11.8 | 5 | 9.0 | 9 | 5 |
| cluster | 100 | 10,345.4 | 118 | 72.5 | 3,448.4 | 3 | 30.9 | 13.0 | 6 | 10.1 | 10 | 6 |
| cluster | 1000 | 10,345.4 | 1,024 | 164.1 | 3,448.4 | 6 | 54.9 | 34.9 | 11 | 26.9 | 15 | 11 |
| insdel | 1 | 10,465.4 | 9 | 24.1 | 3,488.4 | 3 | 13.5 | 9.1 | 3 | 6.8 | 6 | 3 |
| insdel | 10 | 10,464.2 | 53 | 248.3 | 3,488.0 | 13 | 108.7 | 68.7 | 18 | 51.7 | 26 | 18 |
| insdel | 100 | 10,454.4 | 363 | 1,571.5 | 3,484.7 | 86 | 792.1 | 467.2 | 112 | 345.1 | 133 | 117 |
| insdel | 1000 | 10,353.5 | 2,400 | 5,054.9 | 3,451.1 | 263 | 2,387.2 | 2,205.4 | 555 | 1,685.4 | 580 | 564 |
| version | 0 | 10,465.5 | 1,801 | 5,493.5 | 3,488.4 | 328 | 2,865.6 | 3,757.9 | 991 | 2,918.1 | 991 | 991 |

#### Linux: Estimated whole commit with a prolly inode_data (estimate, not measured)

| pattern | K | whole ms | flat ms | prolly ms | estimate ms (whole - flat + prolly) | estimate / whole | whole CPU ms | estimate CPU ms (whole - flat + prolly) | estimate CPU / whole CPU |
|---|---|---|---|---|---|---|---|---|---|
| scatter | 1 | 27.9 | 9.0 | 2.5 | 21.4 | 76.8 % | 25.4 | 19.6 | 77.3 % |
| scatter | 10 | 29.4 | 9.3 | 4.9 | 24.9 | 84.9 % | 26.7 | 21.8 | 81.8 % |
| scatter | 100 | 36.9 | 12.4 | 9.5 | 34.0 | 92.3 % | 34.4 | 31.7 | 92.1 % |
| scatter | 1000 | 77.8 | 30.3 | 45.8 | 93.3 | 120.0 % | 69.0 | 87.0 | 126.1 % |
| cluster | 1 | 28.9 | 8.4 | 3.6 | 24.0 | 83.2 % | 26.4 | 21.3 | 80.6 % |
| cluster | 10 | 29.2 | 8.9 | 4.0 | 24.3 | 83.3 % | 26.5 | 21.9 | 82.9 % |
| cluster | 100 | 31.4 | 8.4 | 4.1 | 27.1 | 86.3 % | 27.9 | 22.9 | 82.1 % |
| cluster | 1000 | 38.2 | 9.3 | 4.9 | 33.7 | 88.4 % | 34.8 | 30.5 | 87.8 % |
| insdel | 1 | 30.9 | 8.1 | 4.2 | 27.1 | 87.5 % | 28.4 | 23.7 | 83.5 % |
| insdel | 10 | 34.7 | 9.3 | 5.9 | 31.3 | 90.2 % | 31.6 | 27.7 | 87.5 % |
| insdel | 100 | 53.0 | 15.4 | 16.7 | 54.4 | 102.6 % | 49.3 | 50.0 | 101.3 % |
| insdel | 1000 | 113.3 | 29.8 | 64.5 | 148.1 | 130.6 % | 110.5 | 145.3 | 131.5 % |
| version | 0 | 153.3 | 38.7 | 112.2 | 226.8 | 147.9 % | 151.8 | 229.4 | 151.1 % |

#### Linux: Peak memory

| mode | peak RSS of the process (MiB) | ru_maxrss after the store load (MiB) | ru_maxrss after the last commit (MiB) |
|---|---|---|---|
| whole | 153.2 | 64.7 | 153.2 |
| flat | 22.6 | - | 22.6 |
| prolly | 14.0 | - | 14.0 |

#### Linux: On-disk size

| mode | start apparent MiB | end apparent MiB | growth apparent MiB | start allocated MiB | end allocated MiB | growth allocated MiB | files at end |
|---|---|---|---|---|---|---|---|
| whole | 283.2 | 512.7 | 229.5 | 283.2 | 512.7 | 229.5 | 21 |
| flat | 2.7 | 66.7 | 64.0 | 2.7 | 66.7 | 64.0 | 4 |
| prolly | 2.9 | 41.3 | 38.4 | 2.9 | 41.3 | 38.4 | 4 |

#### Linux: Scale

| n | op | flat ms | flat CPU ms | flat serialized KB | flat peak RSS MiB | prolly ms | prolly CPU ms | prolly serialized KB | prolly new objects | prolly peak RSS MiB |
|---|---|---|---|---|---|---|---|---|---|---|
| 100,000 | build | 45.6 | 36.7 | 4,100.0 | 19.5 | 88.0 | 81.3 | 4,573.0 | 1,218 | 7.9 |
| 100,000 | apply1 | 7.5 | 6.0 | 4,100.0 | 19.5 | 5.2 | 1.7 | 7.6 | 3 | 7.9 |
| 1,000,000 | build | 361.9 | 382.7 | 41,000.0 | 136.3 | 766.2 | 786.2 | 45,734.6 | 12,245 | 16.6 |
| 1,000,000 | apply1 | 66.9 | 64.3 | 41,000.0 | 136.3 | 2.8 | 1.5 | 10.9 | 4 | 16.6 |
| 4,000,000 | build | 1,325.1 | 1,420.6 | 164,000.0 | 503.3 | 3,047.6 | 3,135.3 | 182,940.7 | 49,012 | 22.4 |
| 4,000,000 | apply1 | 243.7 | 241.3 | 164,000.0 | 503.3 | 3.3 | 1.5 | 10.7 | 4 | 22.4 |

#### Linux: Tree shape

| field | value |
|---|---|
| inode_data entries after the seed | 84,104 |
| final tree: depth | 3 |
| final tree: nodes | 1,035 |
| final tree: leaves | 1,018 |
| final tree: leaf bytes min | 684 |
| final tree: leaf bytes mean | 3,729 |
| final tree: leaf bytes max | 6,804 |
| final tree: internal nodes | 17 |
| final tree: internal bytes mean | 3,111 |
| boundary hash ns per key (median of runs) | 76.8 |

</details>

## Boundary rule

The prototype uses the size-aware key splitter of Dolt
(`go/store/prolly/tree/node_splitter.go` and `chunker.go` of `dolthub/dolt`,
read at commit `d5a833f8df56e35c8fffccb48cfa5d790726aa57`). The constants
are in `crates/prolly/src/lib.rs`:

| Constant | Value | What it does |
|---|---|---|
| `NODE_MIN` | 512 B | No cut below this node size (Dolt `minChunkSize`) |
| `NODE_MAX` | 16,384 B | Hard bound of a node, header included (Dolt `maxChunkSize`) |
| `MAX_ENTRY` | 2,048 B | Largest encoded entry, so that `NODE_MAX` is a real bound |
| `MAX_KEY` | 1,024 B | Largest key, so that an internal entry is never above `MAX_ENTRY` |
| `WEIBULL_L` | 4,096 | Scale of the size CDF; the mean node is about 0.906 x L, about 3.7 KB |
| `WEIBULL_K` | 4 | Shape of the size CDF; the code writes `x*x*x*x`, never a power function |
| `FORCE_AT` | `NODE_MAX - MAX_ENTRY` = 14,336 B | Forced cut after an entry that leaves the node above this size |

After an entry that grows the node from `before` to `after` bytes:

- an internal node never ends after its first entry;
- the node ends when `after > NODE_MAX - MAX_ENTRY` (the forced cut);
- else it does not end when `after < NODE_MIN`;
- else it ends when `boundary_hash(level, key) / (2^32 - 1)` is below
  `1 - e^-((after^4 - before^4) / L^4)`. This is the conditional Weibull
  CDF of Dolt.

`boundary_hash` is the first 4 bytes (little endian) of BLAKE3 over the byte
`level + 1` and the key. It reads the key only, with a salt per level.

Two changes to Dolt:

1. The forced cut at `NODE_MAX - MAX_ENTRY`, with the limits `MAX_ENTRY` and
   `MAX_KEY`. So no node is above `NODE_MAX`. Dolt cuts after the entry that
   passes its maximum, so a Dolt node can be larger than the maximum.
2. The size counted is the encoded node size, header and length fields
   included. Dolt counts key and value bytes only. So `NODE_MAX` bounds the
   real object.

`e^-x` uses only the four basic float operations, and `after^4 - before^4`
is an exact `u64`. IEEE basic operations give the same bits on every
platform (the Rust `exp_m1` does not promise that). The test `golden_root`
pins one root ID and a table of cut probabilities. It passes on macOS and on
Linux (plan 24-04).

What the boundaries depend on (D-13): with fixed-width values, the node
boundaries are a function of the key set alone. With variable-width values,
they depend on the key set and the entry byte lengths, never on the value
bytes. An internal entry has a fixed-width child ID and count, so a level
above the leaves cuts only on the last keys of its children and their
lengths. The tests of `crates/prolly` check this: the same set built in other
orders, or by bulk build and by batches, gives the same root; a change of
value bytes with the same length moves no boundary; no node is above
`NODE_MAX`, and no node is below `NODE_MIN` except the last of a level.

Measured node size spread on Boost: see "Tree shape" (leaves 684 to 6,804
bytes, mean 3,729).

## Node storage

Each node is one chunk-store object (`ChunkStore::put`, read back with the
hash-checked `ChunkStore::read`). So a node is deduplicated, compressed and
packed like a file chunk, and its ID comes from the store hasher (BLAKE3-256
by default). Nodes are not cut again by FastCDC: the prolly rule already
makes them content-defined and small. A child pointer is the child's
`ObjectId`. The root of a map is one `ObjectId`, so a snapshot of the map is
one ID. The node format is part of store format 8 since Phase 25. It is defined in
`docs/design/store-format.md`, section "Metadata maps (store format 8)".

## Rust crate survey

Issue #19 asks if a Rust prolly-tree crate is mature enough. Result of the
survey (crates.io and docs.rs, 2026-11-26):

| Crate | Version | Last release | Node storage | Fit |
|---|---|---|---|---|
| `prolly-map` (lib name `prolly`) | 0.7.2 | 2026-08-22 | A `Store` trait with `get` and `put` by the crate's own key; the node ID is SHA-256 of the node bytes | No: `ChunkStore::put` names an object by its own hash, so a side table from the crate key to the `ObjectId` would be needed, and that is mutable state outside the content-addressed store |
| `prollytree` | 0.4.1 | 2026-08-27 | `NodeStorage` gets an in-memory node with the crate's own digest | No: the store must name the node bytes; and the shape is "a deterministic function of its contents", not of the keys only |
| `prolly` | 0.1.1 | 2022-04-08 | not examined | No: no release since 2022 |

All three have few downloads. So the prototype is an own small crate on the
chunk store, with no new dependency (D-18). `crates/prolly` depends only on
`chunk-store` and `blake3` (about 2,300 lines, 740 of them tests), and no
product crate depends on it.

## File manifests are already content-cut trees

The question "prolly trees for file content" of #19 has an answer in the
code today. A file manifest is a content-cut tree
(`crates/chunk-store/src/file.rs`, module doc and lines 32-47):

- A node ends after an entry whose chunk ID matches a bit pattern (one in
  `NODE_CUT_ONE_IN` = 512), with at least `NODE_MIN` = 64 and at most
  `LEAF_ENTRIES` = 4,096 entries.
- Each entry carries the byte length of its subtree, so a read at an offset
  finds its chunk in O(log n) node reads.
- An edit (`splice`) rewrites one path of nodes, and the key of the result is
  the key of a full write of the same bytes.

The metadata map nodes are already FastCDC-cut values too:
`store_io::put_value` (`crates/metadata/src/store_io.rs`) writes every map
node through a `FileWriter`, so each map is cut into 4/8/32 KiB chunks under
a manifest tree. Thus the text of #19 "each commit stores the full maps" no
longer holds. A commit serializes and hashes every map in full, but it
stores only the changed chunks and the changed manifest nodes (34.2 KB for
one changed file in "Bytes per commit").

## What adoption would change

Adoption is a store format change (D-28). It would change:

- **Root record:** the six map keys become prolly roots (six, or one, see
  Q-2), and the root record layout changes.
- **All six maps:** `inode_map`, `inode_data`, `dir_data`, `manifest_data`,
  `xattr_data` and `refcounts` move to prolly maps. The in-RAM `BTreeMap`s
  go away or become a bounded cache over the tree. The code that loads,
  clones and interns them changes. `inode_map` and `inode_data` hold the
  same entries today, so one of the two can go first.
- **GC mark:** the mark walks prolly nodes (internal entries to their
  children) in place of one flat node per map.
- **rehash:** the rehash of a store (to another hash) must rebuild every
  prolly tree, because a node ID is in its parent.
- **snapshots:** a snapshot keeps prolly roots. A diff of two snapshots can
  then skip equal subtrees (a later gain, not built here).
- **format number:** a new format number. No old store must stay readable
  (there are no deployments), but the new number must refuse old stores
  cleanly.

Cost: the prototype is about 2,300 lines with tests for one generic map,
with no wiring. Adoption adds the wiring in `metadata` (load, commit, the
directory and refcount paths), the GC and rehash walks, the snapshot code,
a bounded node cache, their tests, and the crash and replay tests of the
commit path. That is the work of a milestone, not of one phase.

## Risks

- **Boundary grinding.** A file-system user picks names, and by creates and
  deletes picks which inode keys exist. With a known hash, a user can make
  keys that cut often or seldom. The size limits still hold (no node above
  `NODE_MAX`, at most one small node per level below `NODE_MIN`), so the
  harm is bounded: more or larger nodes, never a broken tree. A keyed hash
  (a secret salt per store) would stop it, at the cost of node IDs that
  differ between stores (less dedup across stores).
- **The rule as a frozen format.** The cut rule and its constants decide
  every node ID. If they are in a store format, a later change of the rule
  or of a constant rewrites every map (Q-3). The float rule is exact today
  (basic operations only), but a fixed-point table is safer for a frozen
  format.
- **Single writer.** The design has one `apply` per root at a time. That
  fits the current commit lock, but a future parallel commit path would need
  a merge of two roots.
- **Large batches and moving boundaries.** A large batch (the version step,
  K = 1,000 of scatter and insdel) is slower as a prolly `apply` than as a
  full flat rewrite. Inserts and deletes in the middle of the map cost more
  than value changes, because they move boundaries: at 100 of them per
  commit prolly is only a little better than flat on macOS and a little
  worse on Linux. An adopted design would need a switch to a bulk rebuild
  for large batches.
- **Read cost without a cache.** Each `apply` reads its paths from the store
  (no node cache, D-17): 6 nodes for one change. A real adoption needs a
  bounded cache of the top levels.
- **Tree consistency.** Each node is checked alone, and each child against
  its parent entry (last key, first key above the previous sibling, count)
  when it is loaded. The check is per path: a read that does not load a bad
  node does not see it. A GC or a verify walk of an adopted design would
  load every node.

## Owner questions

These are one-way doors. Phase 24 did not decide them (D-08). Phase 25
answered Q-2 and Q-3.

- **Q-1:** Adopt prolly maps for the store? It is a format change (a new
  format number and root record, GC and rehash changes). If yes, in which
  milestone? Answered by the v2.3 milestone: adoption is issue #215. The
  recommendation below is the Phase 24 text.
- **Q-2:** If adopted: one prolly map per metadata map, or one combined
  namespace map (keys such as `(kind, inode, name)`)? This changes the root
  record and the GC mark design. **Answered in Phase 25: one prolly tree per
  map, five trees** (layout a, L 8,192, `NODE_MAX` 16,384). See
  `docs/design/store-format.md`, section "Metadata maps (store format 8)".
- **Q-3:** If adopted: do the boundary rule and its constants become part of
  the frozen store format? They decide every node ID, so a later change
  rewrites every map. **Answered in Phase 25: yes**, with a fixed-point table
  in place of the float test and a version byte for the rule. Same section.

## Recommendation

**Later.** The rubric is a reversible choice of plan 24-05:

- `drop` if prolly is not better than flat in median `total_ms` and CPU for
  K up to 100 at the Boost scale and in the 1M and 4M scale rows, or if its
  new stored bytes for K up to 100 are more than 2 x flat's;
- `later` if prolly is better but the estimated whole-commit gain at the
  Boost scale is below 20 % or below 10 ms per commit;
- `adopt` if that gain is 20 % or more, the scale rows show flat's cost
  growing with N while prolly's stays near constant, and the new stored bytes
  are not worse.

Reading of "better" (recorded in the plan summary): prolly is better when
its CPU time is lower and its median wall time is lower, or higher by no
more than the sync effect of "Method".

The numbers that decided it (the main run is macOS, as in the first run;
the rows for K up to 100 are now nine: scatter, cluster and insdel):

- Not `drop` on macOS: for K up to 100, prolly uses less CPU than flat in
  all nine rows (0.5 to 10.1 CPU ms against 6.1 to 12.5), and its median
  wall time is lower in all nine (20.6 to 30.8 ms against 26.1 to 34.5). In
  the 1M and 4M scale rows it uses 0.9 against 44.1 and 221.2 CPU ms (44.6
  and 22.0 ms against 64.4 and 242.1 ms wall; the 44.6 ms is the sync
  effect). Its new stored bytes are lower in every row for K up to 100 (at
  most 0.47 x flat's).
- On Linux, one of the nine rows is not better: at insdel K = 100 (100 real
  inserts and deletes) prolly is a little slower than flat, 16.7 against
  15.4 ms and 13.4 against 12.7 CPU ms. The other eight Linux rows are
  better in both, and so are the Linux scale rows. The rubric names no
  system; plan 24-05 applied it to the macOS run and used Linux as a check.
  Applied to the Linux run alone with "better in every row", this one row
  would give `drop`. It shows where the break-even is: with moving
  boundaries, prolly loses its lead at about 100 changed keys per commit at
  this map size, while flat's cost does not depend on K.
- Not `adopt`: in the rows where prolly helps (K up to 100 on macOS, and
  cluster up to K = 1,000), the estimated whole-commit gain at the Boost
  scale is 3.7 to 8.3 ms (4.8 to 14.0 % of the wall time) and 2.4 to 6.1 CPU
  ms (4.6 to 15.6 % of the CPU time). On Linux it is 3.4 to 6.5 ms (9.8 to
  23.2 %) for K up to 10. So it is below 10 ms on both, and below 20 % on
  macOS (scatter K = 1 on Linux, 23.2 %, is above 20 % but at 6.5 ms below
  10 ms). At K = 1,000 (scatter and insdel) and at the version step the
  estimate is worse than today.
- The gain is real for large stores. By the scale rows, one flat map passes
  100 ms per commit at about 1.6 million entries on macOS (about 1.56
  million on Linux; the first run gave 1.67 and 0.91 million), while prolly
  stays under 2 CPU ms and near one sync of wall time.

So the recommendation stays `later`. The fixes did not change it, but the
`insdel` rows make the case for prolly at the Boost scale weaker than the
first run showed: with real inserts and deletes the gain per commit is
smaller, and at 100 such changes it is gone on Linux.

What would change the choice:

- Stores with more than about one million inodes as a target, or a commit
  interval much shorter than today's, would make the O(N) serialization
  the main cost (one flat map passes 100 ms at about 1.6 million entries,
  and the whole commit, with three maps of that size, earlier; not
  measured). Then a new run of this benchmark at that scale (the scale rows,
  or a seeded tree of that size) should decide Q-1.
- A smaller first step exists: `inode_map` and `inode_data` hold the same
  entries, so one of the two can go. That removes about a third of the
  serialized bytes per commit for all K. It changes the root record (one map
  key less), so it is also a format change and an owner decision, but a much
  smaller one than prolly maps.
- If snapshot diff or sync between stores becomes a goal, the subtree skip
  of prolly roots adds a reason that this benchmark does not measure.

## Phase 26: the product run

Plan 26-13, 2026-11-28. This run measures the product code with prolly maps
against the flat maps of release `v2.3.0`, by a rule that was committed
before any measurement:
`.planning/phases/26-prolly-maps-in-commit-read-and-gc/runs/pmap07-rule.md`,
with the owner note `pmap07-owner-amendment.md` next to it. The run files
and `verdict.txt` are in `runs/pmap07/` of that phase.

- Builds: flat is tag `v2.3.0` (582a41d), prolly is the Phase 26 branch at
  3a64054. Release builds of both.
- Platform: Linux in Docker (OrbStack, x86_64, 32 CPUs), stores on a btrfs
  volume (not tmpfs). One run at a time, in the order flat, prolly, flat,
  prolly, then Boost flat and Boost prolly.
- Commit runs: 1,000,000 files (1,010,001 inodes), 11 one-change commits
  per run, two runs per build (22 samples). Boost runs: `1_84_0`, then
  `1_85_0`, 3 runs, K = 1, 10, 100 and 1,000, 10 commits per pattern and K.

### Check A: work before the sync, at 1,000,000 files

| Median per commit | flat | prolly | prolly / flat | limit |
|---|---:|---:|---:|---:|
| thread CPU (ms) | 515.083 | 1.575 | 0.003 | 0.20 |
| nodes read + written | 14,181 (0 + 14,181) | 18 (11 + 7) | 0.001 | 0.20 |

The prolly time before the sync (information only) is 1.348 ms. Check A
passes.

### Check B: total wall time, sync included

| Median per commit | flat | prolly | prolly / flat | limit |
|---|---:|---:|---:|---:|
| wall (ms) | 528.631 | 6.786 | 0.013 | below 1 |

Check B passes. The flat commit is CPU-bound: it serializes and hashes every
map. Most of the prolly commit is the sync.

### Check C: Boost rows (recorded, does not block)

Median rows of `whole`, 84,104 inodes. CPU is user plus system time; bytes
are new stored bytes.

| Pattern | K | CPU prolly / flat | wall prolly / flat | bytes prolly / flat | better | bytes ok |
|---|---:|---:|---:|---:|:---:|:---:|
| scatter | 1 | 0.063 | 0.185 | 0.569 | yes | yes |
| scatter | 10 | 0.122 | 0.277 | 0.424 | yes | yes |
| scatter | 100 | 0.458 | 0.578 | 0.307 | yes | yes |
| cluster | 1 | 0.075 | 0.180 | 0.345 | yes | yes |
| cluster | 10 | 0.113 | 0.205 | 0.443 | yes | yes |
| cluster | 100 | 0.142 | 0.219 | 0.454 | yes | yes |
| insdel | 1 | 0.052 | 0.124 | 0.326 | yes | yes |
| insdel | 10 | 0.207 | 0.260 | 0.466 | yes | yes |
| insdel | 100 | 0.731 | 0.716 | 0.361 | yes | yes |

All nine rows are better and their bytes are ok, so check C passes too. In
Phase 24 on Linux, insdel K = 100 was a little slower with prolly; in the
product run it is better (0.716 of the flat wall time).

### Verdict

The last line of `verdict.txt`, as `pmap07.py` printed it:

```
verdict pmap07=pass
```

PMAP-07 decided by checks A and B (owner amendment); check C recorded.

### Insert and delete rows, and the rows where prolly is worse

The `insdel` commits insert and delete K keys in the middle of
`inode_data`, so node boundaries move. Median rows:

| K | CPU ms flat | CPU ms prolly | ratio | wall ms flat | wall ms prolly | ratio | stored bytes flat | stored bytes prolly | ratio |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 31.2 | 1.6 | 0.052 | 35.0 | 4.3 | 0.124 | 24,080 | 7,856 | 0.326 |
| 10 | 34.3 | 7.1 | 0.207 | 39.5 | 10.3 | 0.260 | 248,269 | 115,796 | 0.466 |
| 100 | 53.9 | 39.4 | 0.731 | 61.1 | 43.8 | 0.716 | 1,571,495 | 566,578 | 0.361 |
| 1,000 | 113.2 | 153.5 | 1.356 | 116.1 | 161.6 | 1.392 | 5,054,888 | 2,777,059 | 0.549 |

The other recorded rows: scatter K = 1,000 has CPU 1.025, wall 1.074 and
bytes 0.377 (73.1 against 71.3 CPU ms, 83.2 against 77.4 ms); cluster
K = 1,000 has 0.219, 0.385 and 0.311; the version step has 0.481, 0.557 and
0.182; the setup commit has 0.103, 0.203 and 0.481.

Prolly is worse in two rows: insdel K = 1,000 and scatter K = 1,000. Both
are accepted, as the owner note allows ("an accepted cost of the scalable
layout"):

- The numbers: insdel K = 1,000 costs 40.3 CPU ms and 45.6 ms of wall time
  more per commit than flat (1.36 x and 1.39 x). Scatter K = 1,000 costs
  1.8 CPU ms and 5.8 ms more (1.03 x and 1.07 x). In both rows prolly
  stores fewer new bytes (0.55 x and 0.38 x).
- What the insdel row is: 1,000 changes in one commit, at random places in
  the tree, and each change inserts or deletes a key in the middle of
  `inode_data`. The product never uses a freed inode number again, so in
  the product new files go to the right edge of `inode_data` (and of
  `manifest_data` and `xattr_data`). Keys in the middle are normal in
  `dir_data` (a new name in an existing directory) and in `refcounts`
  (content keys). The insdel row stands for those maps.
- How often it happens: a commit runs on a timer (by default every 30 s,
  `--commit-interval`). So a K = 1,000 commit means about 1,000 files
  changed at random places in one interval. That is a bulk operation, for
  example an unpack, a copy or a remove of a tree. Such an operation also
  writes or removes the file data. The extra commit time is 45.6 ms per
  commit, about 0.05 ms per changed file.
- Scatter K = 1,000 is near parity (1.03 x CPU, 1.07 x wall, 0.38 x
  bytes). In `scatter` most creates and deletes cancel inside the commit,
  so this row is about 500 value changes at random places.
- Cost against gain: in the flat rows, insdel K = 1 costs 31.2 CPU ms and
  K = 1,000 costs 113.2 CPU ms, with the same 10.4 MB serialized per
  commit. So about 31 ms of the flat cost grows with the inode count and
  about 82 ms grows with K. At 1,000,000 files the flat one-change commit
  costs 515.1 CPU ms, so that part is about 16 times larger there. The
  prolly cost grows with K and the tree height, not with the inode count.
  So at a larger store the break-even K should move up (an inference, not
  measured here). The gain is on every small commit: at 1,000,000 files
  one change takes 6.8 ms in place of 528.6 ms.

Possible mitigations, as follow-ups only (a code change after the decision
runs would void the pre-registration, and the owner asked for no early
optimization):

- Measure where the time of an insdel K = 1,000 commit goes (boundary
  cuts, hashing, node encoding or the sync) before choosing a change.
- Apply the changes of one tree in key order in one pass, so that changes
  in the same leaf share one path read and one re-cut.
- Run the insdel rows at 1,000,000 files to find the real break-even K at
  that size.

### Cross-checks

- Early flat baseline of plan 26-01 (`runs/pmap07-early-flat.log`, the same
  test at 1,000,000 files): 533.2 ms wall, 510.0 CPU ms, 14,181 nodes. This
  run: 532.1 and 524.1 ms wall, 523.0 and 507.0 CPU ms (round 1 and 2), and
  14,181 nodes in every sample.
- Spread of the 22 samples: flat 486.5 to 654.4 ms wall, prolly 3.7 to
  9.2 ms wall and 1.3 to 2.9 CPU ms.
- The seed through the product API takes longer with prolly: 197.4 s and
  178.2 s, against 88.7 s and 93.0 s for flat. The first commit takes 1,566
  and 1,358 ms with prolly, against 1,957 and 2,494 ms for flat.
- Boost store size (apparent bytes, median, before GC): flat 296.9 MB at
  the start and 537.6 MB at the end (+240.6 MB); prolly 263.9 MB and
  413.2 MB (+149.3 MB).
- Load: each run started only after three quiet samples of the Docker VM
  (load below 3, disk writes below 30 MB/s). Jobs of other projects ran
  on the host during the runs (host load 10 to 22 of 32 CPUs). The load
  samples are in `runs/pmap07/run-order.log`.

Reproduce (the full settings are in the rule file):

```sh
scripts/prolly-bench/pmap07.sh prepare-v230 V230_DIR
scripts/prolly-bench/pmap07.sh commit V230_DIR flat 1000000 11 commit-flat-1.log
scripts/prolly-bench/pmap07.sh commit . prolly 1000000 11 commit-prolly-1.log
scripts/macos-mount-test/fetch-boost.sh DATA 1_84_0 1_85_0
scripts/prolly-bench/pmap07.sh boost V230_DIR flat DATA boost-flat
scripts/prolly-bench/pmap07.sh boost . prolly DATA boost-prolly
python3 scripts/prolly-bench/pmap07.py --flat-commit commit-flat-*.log \
  --prolly-commit commit-prolly-*.log --flat-boost boost-flat/results.csv \
  --prolly-boost boost-prolly/results.csv
```

## Reproduce

```sh
scripts/macos-mount-test/fetch-boost.sh DIR 1_84_0 1_85_0
cargo build --release -p prolly-bench
WORK=WORKDIR BIN=target/release/prolly-bench \
  timeout 5400 bash scripts/prolly-bench/bench.sh DIR 3 > results.csv
python3 .planning/phases/24-prolly-evaluation/runs/report-tables.py \
  --dir .planning/phases/24-prolly-evaluation/runs --stem prolly
```

The modes, the edit script, the environment and every column are in
`scripts/prolly-bench/README.md`. The data of this run is in
`.planning/phases/24-prolly-evaluation/runs/prolly-*.csv`,
`prolly-shape.txt` and `prolly-machine.txt`.
