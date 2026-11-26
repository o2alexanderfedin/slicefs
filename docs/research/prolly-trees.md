# Prolly trees for the metadata maps (#19)

Plan 24-05, run on 2026-11-26. The data is in
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

**Recommendation: later**

- A prolly map for `inode_data` does much less work per small commit than
  the flat map. For one changed file (scatter, K = 1): 3.3 KB serialized in
  place of 3,448.3 KB, 0.6 CPU ms in place of 6.1, and 2.6 KB new stored in
  place of 15.4 KB. The peak RSS of the process is 26.6 MiB in place of
  107.8 MiB.
- But at the Boost scale (84,104 inodes) the gain on the whole commit is
  small. The estimated whole commit with a prolly `inode_data` is 56.4 ms
  in place of 61.7 ms (91.4 %), and 35.2 CPU ms in place of 40.8 (86.5 %).
  That is about 5 ms per commit, below the 10 ms limit of the rubric. At
  K = 1,000 scattered files and at the version step, prolly is slower than
  flat. A Linux Docker run, where a sync is cheap, gives the same answer:
  4.4 to 6.6 ms (14.3 to 21.4 %) less per commit for K up to 10.
- The gain grows with the map. With 4,000,000 entries one changed value
  costs flat 229.9 ms (208.7 CPU ms, 164,000 KB serialized, 589.5 MiB peak
  RSS), and prolly 21.8 ms (0.8 CPU ms, 10.7 KB, 94.2 MiB). By the scale
  rows, one flat map passes 100 ms per commit at about 1.7 million entries.
- Disk: over the 80 small commits and the version step, the flat store grew
  by 31.2 MiB and the prolly store by 17.5 MiB (apparent size, before GC).
- File manifests are already content-cut trees, and every metadata map node
  is already a FastCDC-cut value. A commit stores only the changed chunks of
  a map. The cost that is left is the full serialization, the hashing and the
  full in-RAM maps.
- Adoption is a store format change and an owner decision (Q-1). This report
  does not take it.

## Setup

- Machine (from `prolly-machine.txt`): macOS (Darwin 25.2.0), x86_64,
  Intel Xeon W-3245 at 3.20 GHz, 32 CPUs, 192 GiB of memory, APFS. Commit
  `50965cb` (the Task 1 commit of this plan; the code is the merged tree of
  plans 24-01 to 24-04). `rustc 1.95.0-nightly`, release build.
- Load: the 1-minute load was 5.78 before and 7.79 after the run
  (`load_before=5.78 11.19 15.18`, `load_after=7.79 9.58 13.85`). The run
  started after the 1-minute load came down to 8 or less. No cargo, rustc or
  other SliceFS job ran. Jobs of other projects used about 3 CPUs at the
  start and about 6 CPUs at the end of the run (of 32). So the machine was
  not fully quiet. Byte, object and node counts do not change with load;
  times can.
- Start 2026-11-26T20:40:32Z, end 20:42:59Z: 147 s for 3 runs.
- Inputs: Boost 1_84_0 (the seed tree; `inode_data_entries=84104` in the
  `info` line of `prolly-shape.txt`) and Boost 1_85_0 (the version step:
  18,991 files changed, a count that includes the files that the edit
  script changed before; 1,588 files created and 734
  deleted; 150 directories created and 28 deleted; 85,083 entries in the
  final map).
- Command lines:

  ```sh
  cargo build --release -p prolly-bench
  WORK="$M/results/prolly-2026-11-26" BIN=target/release/prolly-bench \
    timeout 5400 bash scripts/prolly-bench/bench.sh "$M/data" 3
  ```

  with the defaults `BOOST_VERS="1_84_0 1_85_0"`, `KS="1 10 100 1000"`,
  `COMMITS=10`, `PATTERNS="scatter cluster"`, `SEED=24` and
  `SCALE_NS="100000 1000000 4000000"`. `M` is the measure dir outside the
  repo.
- Runs: 3, no fallback. 738 run rows and 30 median rows. The flat and the
  prolly digest checks gave `ok=1` in all 3 runs.
- Before the long run, a quick pass on the same data (`RUNS=1 KS=1
  COMMITS=1 PATTERNS=scatter SCALE_NS=100000`, 76 s) checked the tools. Its
  files are `runs/prolly-quick-*`. Its numbers are not used here.
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
  `inode_data` change of a commit; it is smaller than K.
- Version step (the last commit): the 1_85_0 tree as one commit, matched by
  content key. A file with the same content key stays; a changed file gets
  the new content; new files and directories are made; names that are only
  in the store are removed.

That is 82 commits per mode and run. The columns that matter:

- `total_ms`: wall time of the commit, sync included. `begin_ms` of `whole`
  is the clone of every map.
- CPU ms: user plus system CPU time of the commit (`getrusage`).
- `serialized_bytes`: bytes serialized and hashed by the commit.
- `new_objects` and `new_stored_bytes`: new objects and new stored bytes in
  the chunk store (a `ChunkStore::stats()` delta; a deduplicated put adds 0).
- `nodes_read` and `nodes_written`: prolly nodes read and written by
  `apply`.
- `peak_rss_bytes`: peak RSS of the process (`/usr/bin/time -l`).
- The on-disk size of each store at the start and at the end.

The prototype has no node cache (D-17). `nodes_read` shows the read cost
without one: every `apply` reads its paths from the store.

The `scale` rows are synthetic: N sorted entries of the same shape (8-byte
key, 33-byte value) for N = 100,000, 1,000,000 and 4,000,000. Each row is a
build, then one changed middle value (`apply1`), in a new store and
process.

How to read the wall times: each commit ends with one `cs.sync()`, which is
`F_FULLFSYNC` on macOS. On this machine the wall time of a commit minus its
CPU time is about 20 ms in most commits and about 40 ms in some. Computed
from the run rows (the median of the 30 rows of each group): the gap is 20.3
to 21.4 ms in 19 of the 24 small-commit groups, and 23.5 to 40.4 ms in the
other five (`whole` scatter and cluster K = 1,000, `flat` and `prolly`
scatter K = 1,000, and `prolly` scatter K = 100). The gap is about the
same in the three modes. So for small commits the wall time is mostly the
sync, and a difference of one sync (about 20 ms) between two medians comes
from the disk, not from the work of the map. The CPU time, the serialized bytes
and the scale rows show the work. The decision below uses them first (a
reading of this plan, recorded in the plan summary). On Linux (see "Linux
Docker run") the gap is about 2 to 3 ms.

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
| setup | 0 | NA | 95.5 | 13.9 | 41.1 | - | - | - | - |
| bulk | 0 | 84,105 | - | - | - | 56.0 | 30.1 | 89.0 | 64.0 |
| scatter | 1 | 1 | 61.7 | 13.4 | 40.8 | 26.9 | 6.1 | 21.6 | 0.6 |
| scatter | 10 | 6 | 59.6 | 11.7 | 38.6 | 27.1 | 6.2 | 22.4 | 1.3 |
| scatter | 100 | 54 | 63.2 | 9.3 | 42.0 | 30.3 | 9.3 | 45.7 | 5.8 |
| scatter | 1000 | 503 | 110.4 | 11.0 | 73.5 | 46.1 | 21.2 | 55.1 | 31.7 |
| cluster | 1 | 1 | 56.5 | 8.3 | 34.9 | 26.5 | 5.7 | 20.8 | 0.5 |
| cluster | 10 | 10 | 55.2 | 7.8 | 33.9 | 26.3 | 5.7 | 20.9 | 0.6 |
| cluster | 100 | 54 | 56.4 | 7.8 | 34.6 | 26.2 | 5.8 | 21.1 | 0.6 |
| cluster | 1000 | 504 | 67.7 | 8.0 | 40.9 | 26.7 | 6.1 | 21.9 | 1.2 |
| version | 0 | 21,521 | 148.5 | 14.5 | 101.1 | 52.1 | 27.2 | 104.1 | 79.9 |

The whole commit costs about 55 to 63 ms (34 to 42 CPU ms) for up to 100
changed files. Of that, `begin_commit` (the clone of every map) is 7.8 to
13.4 ms. For `inode_data` alone, flat costs about 6 CPU ms per commit up to
K = 10 and in every cluster row (9.3 at scatter K = 100), because it
serializes and hashes the full map every time. Prolly costs
0.5 to 1.3 CPU ms up to K = 10 and for every cluster commit, and its cost
grows with the number of touched leaves: 5.8 CPU ms at scatter K = 100 and
31.7 CPU ms at scatter K = 1,000 (more than flat's 21.2). The version step
(21,521 changed entries) costs prolly 79.9 CPU ms and flat 27.2: a large
batch is cheaper as a full rewrite. The prolly wall time at scatter K = 100
(45.7 ms) is high because most syncs of that group took about 40 ms, not 20
(median wall minus CPU 40.4 ms); its CPU time is lower than flat's. The 40 ms
does not grow with the nodes written: prolly scatter K = 1,000 writes 401
nodes and its median gap is 23.5 ms. The cause was not measured.

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
| version | 0 | 10,465.5 | 1,784 | 5,495.9 | 3,488.4 | 312 | 2,861.1 | 3,757.9 | 991 | 2,918.1 | 991 | 991 |

Serialized and hashed per commit is not the same as new stored per commit
(D-24). The whole commit serializes and hashes about 10.3 MB on every
commit, whatever K is, but stores only 34.2 to 36.3 KB for one changed file: the
FastCDC cut of `store_io` keeps every unchanged chunk. For `inode_data`,
flat serializes 3,448 KB per commit and prolly 3.3 KB for one change. New
stored bytes are small in both designs, and prolly stores less in every
row: 2.6 KB against 15.4 KB at K = 1, 188.4 KB against 487.6 KB at scatter
K = 100. Write amplification of prolly: one change writes one path of 3
nodes (depth 3) and reads 6; 54 scattered changes write 64 nodes. Flat
writes the changed FastCDC chunks and the manifest leaf of the map again
(2 objects at K = 1, 48 at scatter K = 100).

### Estimated whole commit with a prolly inode_data (estimate, not measured)

| pattern | K | whole ms | flat ms | prolly ms | estimate ms (whole - flat + prolly) | estimate / whole | whole CPU ms | estimate CPU ms (whole - flat + prolly) | estimate CPU / whole CPU |
|---|---|---|---|---|---|---|---|---|---|
| scatter | 1 | 61.7 | 26.9 | 21.6 | 56.4 | 91.4 % | 40.8 | 35.2 | 86.5 % |
| scatter | 10 | 59.6 | 27.1 | 22.4 | 54.9 | 92.1 % | 38.6 | 33.7 | 87.4 % |
| scatter | 100 | 63.2 | 30.3 | 45.7 | 78.6 | 124.5 % | 42.0 | 38.5 | 91.7 % |
| scatter | 1000 | 110.4 | 46.1 | 55.1 | 119.5 | 108.2 % | 73.5 | 84.0 | 114.3 % |
| cluster | 1 | 56.5 | 26.5 | 20.8 | 50.9 | 90.0 % | 34.9 | 29.7 | 84.9 % |
| cluster | 10 | 55.2 | 26.3 | 20.9 | 49.8 | 90.2 % | 33.9 | 28.8 | 84.8 % |
| cluster | 100 | 56.4 | 26.2 | 21.1 | 51.3 | 91.0 % | 34.6 | 29.4 | 85.0 % |
| cluster | 1000 | 67.7 | 26.7 | 21.9 | 62.9 | 92.9 % | 40.9 | 36.0 | 88.0 % |
| version | 0 | 148.5 | 52.1 | 104.1 | 200.5 | 135.0 % | 101.1 | 153.8 | 152.1 % |

This is an estimate: the real commit does not run with a prolly map. If
`inode_data` were a prolly map, a small commit at the Boost scale would be
about 5 ms faster: 7 to 10 % of the wall time and 12 to 15 % of the CPU
time. The scatter K = 100 wall row (124.5 %) is the sync effect of the
table above; by CPU that row is 91.7 %. For scatter K = 1,000 and for the
version step the estimate is worse than today. A prolly map for all four
O(N) maps would gain more (not measured), but the clone in `begin_commit`
and the fixed costs of a commit stay.

### Peak memory

| mode | peak RSS of the process (MiB) | ru_maxrss after the store load (MiB) | ru_maxrss after the last commit (MiB) |
|---|---|---|---|
| whole | 390.0 | 103.4 | 390.0 |
| flat | 107.8 | - | 107.8 |
| prolly | 26.6 | - | 26.2 |

The whole process reaches 103.4 MiB after the store load and 390.0 MiB at
the end. Its high-water mark grows with every commit (123.3 MiB after the
setup commit and 328.6 MiB in the cluster K = 1,000 row, from the
`maxrss_bytes` column), so it is not only the maps; the parts were not
measured. The flat process, which holds only `inode_data` and its chunk
store, reaches 107.8 MiB; the prolly process 26.6 MiB, with no map in RAM.
On Linux the same processes peak at 147.8, 23.3 and 13.8 MiB (see "Linux
Docker run"). The cause of the higher macOS numbers was not measured.

### On-disk size

| mode | start apparent MiB | end apparent MiB | growth apparent MiB | start allocated MiB | end allocated MiB | growth allocated MiB | files at end |
|---|---|---|---|---|---|---|---|
| whole | 283.2 | 443.3 | 160.1 | 283.2 | 444.2 | 161.0 | 19 |
| flat | 2.7 | 33.9 | 31.2 | 2.7 | 34.1 | 31.4 | 1 |
| prolly | 2.9 | 20.4 | 17.5 | 2.9 | 21.3 | 18.4 | 1 |

After the 80 small commits and the version step (81 commits after the bulk
build), with no GC, the prolly store
of `inode_data` grew by 17.5 MiB and the flat store by 31.2 MiB (44 % less).
The start sizes are close (2.9 against 2.7 MiB): a prolly node holds 41
bytes per entry plus the length fields, and its 33-byte digests do not
compress. The growth of `whole` includes the 4 KiB content of the content
edits and the creates, and the content of the version step.

### Scale

| n | op | flat ms | flat CPU ms | flat serialized KB | flat peak RSS MiB | prolly ms | prolly CPU ms | prolly serialized KB | prolly new objects | prolly peak RSS MiB |
|---|---|---|---|---|---|---|---|---|---|---|
| 100,000 | build | 60.8 | 35.2 | 4,100.0 | 24.5 | 103.8 | 78.9 | 4,573.0 | 1,218 | 7.9 |
| 100,000 | apply1 | 29.0 | 6.0 | 4,100.0 | 24.5 | 21.4 | 0.7 | 7.6 | 3 | 7.9 |
| 1,000,000 | build | 464.5 | 321.5 | 41,000.0 | 162.1 | 1,009.2 | 798.3 | 45,734.6 | 12,245 | 30.6 |
| 1,000,000 | apply1 | 62.9 | 42.2 | 41,000.0 | 162.1 | 22.3 | 0.8 | 10.9 | 4 | 30.6 |
| 4,000,000 | build | 1,817.3 | 1,308.1 | 164,000.0 | 589.5 | 3,672.7 | 3,082.1 | 182,940.7 | 49,012 | 94.2 |
| 4,000,000 | apply1 | 229.9 | 208.7 | 164,000.0 | 589.5 | 21.8 | 0.8 | 10.7 | 4 | 94.2 |

The cost of one small change grows linearly with N for flat (6.0, 42.2 and
208.7 CPU ms), and stays near 0.8 CPU ms for prolly (one path of 3 or 4
nodes). Flat's peak RSS also grows with N (24.5 to 589.5 MiB); prolly's grows
less (7.9 to 94.2 MiB; on Linux 7.4 to 21.6 MiB with the same 49,012 new
objects at 4,000,000 entries; what uses the memory was not measured). A bulk build is 1.7 to 2.2 times as slow for prolly
as a full flat intern. By linear interpolation of the flat `apply1` wall times
between the 1,000,000 and 4,000,000 rows, one flat map passes 100 ms per
commit at about 1.67 million entries (by CPU time alone, at about 2.04
million). The whole commit interns three maps of this size and clones five,
so it passes 100 ms at a smaller inode count (not measured).

### Tree shape

| field | value |
|---|---|
| inode_data entries after the seed | 84,104 |
| final tree: depth | 3 |
| final tree: nodes | 1,042 |
| final tree: leaves | 1,026 |
| final tree: leaf bytes min | 549 |
| final tree: leaf bytes mean | 3,740 |
| final tree: leaf bytes max | 6,804 |
| final tree: internal nodes | 16 |
| final tree: internal bytes mean | 3,327 |
| boundary hash ns per key (median of runs) | 94.3 |

The final tree (85,083 entries) has depth 3, 1,026 leaves and 16 internal
nodes. The node size spread (#19): leaves are 549 to 6,804 bytes with a mean
of 3,740, as the Weibull rule predicts (mean about 0.906 x 4,096 plus the
header). No node is near `NODE_MAX` (16,384). The boundary hash costs 94.3
ns per key (D-12), so a bulk build of 84,104 keys spends about 8 ms in it.

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
  the same work tree (commit `e16962c`, same code as `50965cb`).
- Start 2026-11-26T20:44:32Z, end 20:47:09Z, 3 runs, no fallback, 6 digest
  checks `ok=1`. The load in the VM was 2.40 before and 1.17 after. No host
  load was read during this run; the last host reading, at the end of the
  macOS run (20:42:59Z), was 7.79, with jobs of other projects running. So
  the host was not quiet.

The byte, object and node counts are the same as on macOS, or close. In the
prolly rows the entries, serialized bytes, new objects and nodes read and
written are equal, and the stored bytes differ by at most 62 bytes (zstd of
other digest bytes). The flat and whole rows differ a little more, because
file times go into the inode digests and FastCDC cuts the flat map by
content. The times differ: on Linux a sync costs about 2 to 3 ms (median
wall minus CPU over the small commits: 3.0, 2.6 and 2.2 ms for whole, flat
and prolly), so the wall time shows the work. The small-commit estimate is
4.4 to 6.6 ms less per commit (14.3 to 21.4 %) for K up to 10, and worse
than today at scatter K = 1,000 and at the version step. In the scale rows
flat's `apply1` is 110.1 ms at 1,000,000 entries and 285.0 ms at 4,000,000;
prolly's is 2.9 to 4.7 ms. Peak RSS is much lower than on macOS for the
same work.

<details>
<summary>Linux tables</summary>

#### Linux: Commit time and CPU

| pattern | K | entries changed | whole ms | whole begin ms | whole CPU ms | flat ms | flat CPU ms | prolly ms | prolly CPU ms |
|---|---|---|---|---|---|---|---|---|---|
| setup | 0 | NA | 40.7 | 9.6 | 35.5 | - | - | - | - |
| bulk | 0 | 84,105 | - | - | - | 62.5 | 36.4 | 76.8 | 70.4 |
| scatter | 1 | 1 | 30.7 | 6.1 | 27.3 | 8.6 | 6.2 | 2.5 | 0.3 |
| scatter | 10 | 6 | 30.9 | 6.0 | 28.3 | 11.0 | 7.3 | 4.4 | 2.2 |
| scatter | 100 | 54 | 38.7 | 6.1 | 35.5 | 14.2 | 10.5 | 11.3 | 7.7 |
| scatter | 1000 | 503 | 76.9 | 6.3 | 70.7 | 29.7 | 27.4 | 46.2 | 43.6 |
| cluster | 1 | 1 | 29.9 | 6.2 | 27.7 | 8.2 | 6.1 | 2.7 | 1.1 |
| cluster | 10 | 10 | 30.6 | 6.2 | 28.2 | 8.3 | 6.1 | 3.9 | 1.3 |
| cluster | 100 | 54 | 31.9 | 6.4 | 29.5 | 8.9 | 6.2 | 3.3 | 1.4 |
| cluster | 1000 | 504 | 39.4 | 6.5 | 36.1 | 8.1 | 6.6 | 4.3 | 2.0 |
| version | 0 | 21,521 | 150.9 | 29.2 | 156.0 | 36.9 | 33.1 | 138.3 | 111.0 |

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
| version | 0 | 10,465.5 | 1,801 | 5,493.5 | 3,488.4 | 328 | 2,865.6 | 3,757.9 | 991 | 2,918.1 | 991 | 991 |

#### Linux: Estimated whole commit with a prolly inode_data (estimate, not measured)

| pattern | K | whole ms | flat ms | prolly ms | estimate ms (whole - flat + prolly) | estimate / whole | whole CPU ms | estimate CPU ms (whole - flat + prolly) | estimate CPU / whole CPU |
|---|---|---|---|---|---|---|---|---|---|
| scatter | 1 | 30.7 | 8.6 | 2.5 | 24.6 | 80.1 % | 27.3 | 21.3 | 78.3 % |
| scatter | 10 | 30.9 | 11.0 | 4.4 | 24.3 | 78.6 % | 28.3 | 23.2 | 81.9 % |
| scatter | 100 | 38.7 | 14.2 | 11.3 | 35.8 | 92.6 % | 35.5 | 32.7 | 92.1 % |
| scatter | 1000 | 76.9 | 29.7 | 46.2 | 93.4 | 121.5 % | 70.7 | 86.9 | 123.0 % |
| cluster | 1 | 29.9 | 8.2 | 2.7 | 24.4 | 81.7 % | 27.7 | 22.7 | 82.1 % |
| cluster | 10 | 30.6 | 8.3 | 3.9 | 26.2 | 85.7 % | 28.2 | 23.5 | 83.1 % |
| cluster | 100 | 31.9 | 8.9 | 3.3 | 26.3 | 82.6 % | 29.5 | 24.7 | 83.8 % |
| cluster | 1000 | 39.4 | 8.1 | 4.3 | 35.6 | 90.4 % | 36.1 | 31.6 | 87.3 % |
| version | 0 | 150.9 | 36.9 | 138.3 | 252.2 | 167.2 % | 156.0 | 233.8 | 149.9 % |

#### Linux: Peak memory

| mode | peak RSS of the process (MiB) | ru_maxrss after the store load (MiB) | ru_maxrss after the last commit (MiB) |
|---|---|---|---|
| whole | 147.8 | 64.8 | 147.8 |
| flat | 23.3 | - | 23.3 |
| prolly | 13.8 | - | 13.8 |

#### Linux: On-disk size

| mode | start apparent MiB | end apparent MiB | growth apparent MiB | start allocated MiB | end allocated MiB | growth allocated MiB | files at end |
|---|---|---|---|---|---|---|---|
| whole | 283.2 | 443.6 | 160.4 | 283.2 | 443.6 | 160.4 | 19 |
| flat | 2.7 | 34.4 | 31.7 | 2.7 | 34.4 | 31.7 | 1 |
| prolly | 2.9 | 20.4 | 17.5 | 2.9 | 20.4 | 17.5 | 1 |

#### Linux: Scale

| n | op | flat ms | flat CPU ms | flat serialized KB | flat peak RSS MiB | prolly ms | prolly CPU ms | prolly serialized KB | prolly new objects | prolly peak RSS MiB |
|---|---|---|---|---|---|---|---|---|---|---|
| 100,000 | build | 62.6 | 54.2 | 4,100.0 | 19.6 | 94.7 | 87.7 | 4,573.0 | 1,218 | 7.4 |
| 100,000 | apply1 | 7.6 | 6.0 | 4,100.0 | 19.6 | 3.3 | 1.3 | 7.6 | 3 | 7.4 |
| 1,000,000 | build | 498.2 | 506.7 | 41,000.0 | 135.8 | 922.0 | 911.5 | 45,734.6 | 12,245 | 15.9 |
| 1,000,000 | apply1 | 110.1 | 85.9 | 41,000.0 | 135.8 | 4.7 | 1.8 | 10.9 | 4 | 15.9 |
| 4,000,000 | build | 1,863.8 | 1,975.6 | 164,000.0 | 503.0 | 3,408.7 | 3,513.4 | 182,940.7 | 49,012 | 21.6 |
| 4,000,000 | apply1 | 285.0 | 288.8 | 164,000.0 | 503.0 | 2.9 | 1.5 | 10.7 | 4 | 21.6 |

#### Linux: Tree shape

| field | value |
|---|---|
| inode_data entries after the seed | 84,104 |
| final tree: depth | 3 |
| final tree: nodes | 1,042 |
| final tree: leaves | 1,026 |
| final tree: leaf bytes min | 549 |
| final tree: leaf bytes mean | 3,740 |
| final tree: leaf bytes max | 6,804 |
| final tree: internal nodes | 16 |
| final tree: internal bytes mean | 3,327 |
| boundary hash ns per key (median of runs) | 76.9 |

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

Measured node size spread on Boost: see "Tree shape" (leaves 549 to 6,804
bytes, mean 3,740).

## Node storage

Each node is one chunk-store object (`ChunkStore::put`, read back with the
hash-checked `ChunkStore::read`). So a node is deduplicated, compressed and
packed like a file chunk, and its ID comes from the store hasher (BLAKE3-256
by default). Nodes are not cut again by FastCDC: the prolly rule already
makes them content-defined and small. A child pointer is the child's
`ObjectId`. The root of a map is one `ObjectId`, so a snapshot of the map is
one ID. The node bytes (magic `SPT1`, level, entry count, length-prefixed
entries) are private to `crates/prolly`. They are not a store format: no
root record, segment record or header line refers to them, and
`docs/design/store-format.md` does not change (D-03, D-14, D-15).

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
`chunk-store` and `blake3` (about 2,100 lines, 670 of them tests), and no
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

Cost: the prototype is about 2,100 lines with tests for one generic map,
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
- **Large batches.** A large batch (the version step, scatter K = 1,000) is
  slower as a prolly `apply` than as a full flat rewrite. An adopted design
  would need a switch to a bulk rebuild for large batches.
- **Read cost without a cache.** Each `apply` reads its paths from the store
  (no node cache, D-17): 6 nodes for one change. A real adoption needs a
  bounded cache of the top levels.

## Owner questions

These are one-way doors. This phase does not decide them (D-08).

- **Q-1:** Adopt prolly maps for the store? It is a format change (a new
  format number and root record, GC and rehash changes). If yes, in which
  milestone? Open. The recommendation below is "later".
- **Q-2:** If adopted: one prolly map per metadata map, or one combined
  namespace map (keys such as `(kind, inode, name)`)? This changes the root
  record and the GC mark design. Open. Suggestion: decide it with Q-1, after
  the redundant `inode_map` or `inode_data` is gone.
- **Q-3:** If adopted: do the boundary rule and its constants become part of
  the frozen store format? They decide every node ID, so a later change
  rewrites every map. Open. Suggestion: yes if adopted, but with a
  fixed-point table in place of the float test, and with a version byte for
  the rule.

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

The numbers that decided it:

- Not `drop`: for K up to 100, prolly uses less CPU than flat in every row
  (0.5 to 5.8 CPU ms against 5.7 to 9.3), and in the 1M and 4M scale rows
  (0.8 against 42.2 and 208.7 CPU ms; 22.3 and 21.8 ms against 62.9 and
  229.9 ms wall). Its new stored bytes are lower in every row (at most 0.47 x
  flat's for K up to 100). Its median wall time is lower in 5 of the 6 rows
  for K up to 100. The sixth (scatter K = 100, 45.7 against 30.3 ms) is the
  sync effect: its sync took about 40 ms in place of 20, and its CPU time is
  lower (5.8 against 9.3 ms). On Linux, where a sync is about 2 ms, prolly's
  wall time is lower in all 6 rows.
- Not `adopt`: in the rows where prolly helps (K up to 10, and cluster up to
  K = 1,000), the estimated whole-commit gain at the Boost scale is 4.7 to
  5.6 ms (7.1 to 10.0 % of the wall time) and 4.9 to 5.6 CPU ms (12.0 to
  15.2 % of the CPU time). On Linux it is 3.8 to 6.6 ms (9.6 to 21.4 %). So
  it is below 10 ms on both, and below 20 % on macOS. At scatter K = 1,000
  and at the version step the estimate is worse than today.
- The gain is real for large stores. By the scale rows, one flat map passes
  100 ms per commit at about 1.67 million entries on macOS (about 0.91
  million on Linux, where the 1M row is already 110.1 ms), while prolly stays
  under 2 CPU ms and near one sync of wall time.

What would change the choice:

- Stores with more than about one million inodes as a target, or a commit
  interval much shorter than today's, would make the O(N) serialization the
  main cost. Then a new run of this benchmark at that scale (the scale rows,
  or a seeded tree of that size) should decide Q-1.
- A smaller first step exists: `inode_map` and `inode_data` hold the same
  entries, so one of the two can go. That removes about a third of the
  serialized bytes per commit for all K. It changes the root record (one map
  key less), so it is also a format change and an owner decision, but a much
  smaller one than prolly maps.
- If snapshot diff or sync between stores becomes a goal, the subtree skip
  of prolly roots adds a reason that this benchmark does not measure.

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
