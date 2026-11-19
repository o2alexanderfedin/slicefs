Results of the on-disk chunk index (PR #109).

The chunk store now keeps its index on disk: sorted run files of (ObjectId, Loc) in 4 KiB pages, a memtable in RAM for new entries, and a MANIFEST. For each run, RAM holds one 8-byte fence for each page and one Bloom filter (10 bits for each key). Size-tiered merges run on a background thread. The pack files and the `.idx` files did not change. They stay the source of truth, and the index can be built again from them. Design: `docs/design/chunk-store.md`, section "Index on disk (#85)" (https://github.com/o2alexanderfedin/slicefs/blob/develop/docs/design/chunk-store.md#index-on-disk-85).

Method: `benchmarks/index-bench` (the #66 tool) and the #66 method, commit c1c20d7. macOS on the same host as #66 (Xeon W-3245, 192 GiB RAM, macOS 26.2, APFS on an internal SSD). Warm page cache. The open values are the median of 3 opens; the create and lookup steps ran one time. The host load was lower than for #66 (load average 13 to 19, against 80 to 140), so the CPU times compare better than the wall times.

Each cell is "new / #66":

| Objects | Steady RSS B/object | Open s | Open CPU s | Hit ns | Miss ns | Put dedup-hit ns | Create puts per CPU s | Disk B/object |
|---|---|---|---|---|---|---|---|---|
| 1M | 182.0 / 390.2 | 0.626 / 0.964 | 0.625 / 0.591 | 1,056 / 1,305 | 701 / 727 | 1,280 / 1,284 | 141,700 / 117,000 | 151.1 / 104 |
| 10M | 4.56 / 195.8 | 0.060 / 12.844 | 0.060 / 5.881 | 1,385 / 701 | 544 / 324 | 1,374 / 945 | 138,000 / 125,000 | 163.6 / 104 |
| 100M | 4.02 / 105.5 | 0.492 / 141.241 | 0.492 / 62.239 | 3,183 / 820 | 1,171 / 420 | 2,051 / 975 | 117,800 / 122,000 | 163.8 / 104 |

- RAM: after open, the steady anonymous RAM is 2.19 B for each object at 10M and 3.29 B at 100M (target: a few bytes). Only the filters and the fences grow with the object count: 1.36 B for each object. The rest is fixed: the memtable (at most 262,144 entries) and the replay of the pack at the high-water mark (192 MB at 100M). At 1M, this fixed part gives most of the 182 B for each object.
- Open: it reads the MANIFEST, the run footers, the tables and the filters, and the records above the high-water mark. It does not read the run entries.
- Lookups are slower than with the map in RAM at 10M and 100M. At 100M a hit takes 3.2 µs against 0.8 µs (3.9 times). A lookup checks the filter of each run (3 runs at 1M, 5 at 10M, 9 at 100M), and a hit reads one page of a mapped run file. A probable cause of the slow hits at 100M is page faults on the run pages, because each lookup step is a new process. This cause is not measured. A separate measurement at 10M (macOS, one process, the second lookup pass) gave a hit of 1.03 µs and a miss of 0.50 µs with 5 runs, and 0.80 µs and 0.31 µs after a merge into 1 run (#66: 0.70 µs and 0.32 µs). Follow-up: #111.
- A hit reads one 4 KiB page, and a miss usually reads none (test `hits_read_one_page_and_misses_read_almost_none`).
- Disk: the index adds about 60 B for each object. The store needs 164 B for each object, against 104 B.
- Create: 118k to 142k puts per CPU second, against 117k to 125k for #66. The wall-time rate is about 2 times the #66 rate, but the lower host load is part of that.
- Recovery: these rows are not like for like with #66. The new index does not read the `.idx` of a pack below the high-water mark, so a recovery open scans only the packs at or above it. Thus the #66 scan rates do not apply.
- Crash tests (in process, no mount): a crash during a run flush or a merge loses no object and gives no wrong Loc (`crates/chunk-store/tests/index_crash_tests.rs`).
- GC tests: after online GC, no lookup and no put dedup check returns a removed object, also after a reopen and after merges (`crates/chunk-store/tests/gc_tests.rs`, `crates/chunk-store/tests/gc_fault_tests.rs`, `crates/chunk-store/src/race_tests.rs`).
- GC lock: GC makes its decisions from a snapshot of the runs, with no store lock held. With 3 of 4 objects live, the longest hold of the store lock was 108 ms at 10M and 105 ms at 100M objects. GC works on groups of at most 32 packs. A read error or a damaged page in a GC pass stops the GC before the install of the current group, so no pack of that group changes; the groups that are done stay done. A damaged page requests a rebuild of the index.
- Not measured: a cold page cache, and an HDD.
- All tables, Linux included: `docs/design/chunk-store.md`, section "Index results (#85)" (https://github.com/o2alexanderfedin/slicefs/blob/develop/docs/design/chunk-store.md#index-results-85).

Follow-up issues:

- #111: fewer index runs for each lookup.
- #112: a locality cache for the dedup check.
- #113: the RAM of the GC mark set and of the touched sets.

The acceptance items of this issue are met, so this issue is closed.
