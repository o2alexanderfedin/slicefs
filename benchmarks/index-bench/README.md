# index-bench

Measures the index of the chunk store: open time, RAM per object, lookup speed, the time to recover a pack that has no `.idx` file, and (step `gc`) the store-lock hold of a GC. It was written for the in-memory index (#66). The results of that index are the baseline in `docs/design/chunk-store.md`, section "Index baseline (#66)". The results of the on-disk index (#85) are in the section "Index results (#85)".

The tool uses only the public `ChunkStore` API. It makes stores of many small synthetic objects: object `i` is the 8 bytes of `i` (a little-endian u64). These objects are smaller than the zstd limit, so zstd does not run. The store uses the default `Options` (64 MiB packs).

The tool does not change the store format and does not mount anything. It works on pack files only.

## Run

```sh
cargo build --release -p index-bench
B=target/release/index-bench

# D is a directory OUTSIDE the repo, on a disk with enough free space.
$B run --dir "$D/1000000"   --objects 1000000
$B run --dir "$D/10000000"  --objects 10000000
$B run --dir "$D/100000000" --objects 100000000
```

- `run` makes the store when `D/N` has no store, and then measures it. The tool makes each size one time. Later runs use the same store again and measure it again.
- Do not put `D` in the repo. The stores are large (see "Disk and memory") and must not be committed.
- `run` prints the output of each step and then four summary tables in markdown (see "Output"). It never prints the `D` path, so you can copy the tables into public docs.
- Copy the binary out of the target directory before a long run if other builds can use that target directory. `run` starts its own binary again for each step.

## Steps

Each step is a subcommand. Every step takes `--dir D` and `--backend NAME` (default `chunk-store`).

| Step | Flags | What it does |
|---|---|---|
| `create` | `--objects N`, `--pack-limit BYTES` and `--memtable-limit ENTRIES` (both only for tests) | Makes a store of N objects in an empty directory, waits until the background merges of the index stop, closes it (every pack is sealed and has an `.idx`), and writes the marker file `D/index-bench.txt`. `--memtable-limit` sets `Options::index_memtable_limit`, so that a small store has several index runs. |
| `open` | `--cache warm\|cold` | Opens the store one time and measures the time and the RSS (resident set size). `--cache` is only a label in the output. It does not change the cache. |
| `lookup` | `--hits K`, `--misses K`, `--seed S` | Opens the store and times `contains` for K IDs in the store (hits) and K IDs not in the store (misses), and `put` of K objects that are in the store (the dedup-hit path, with the hash). K is 1,000,000 by default, or N if N is smaller. The seed is 66 by default. It times two passes over the same sample in one process: the first pass takes the page faults of the mapped index runs, the second pass is warm. |
| `settle` | `--timeout-s S` (default 900) | Opens the store with the default background work of the index, so the idle merge (#111) runs after the idle delay, waits at most S seconds until it is done, and reports the runs before and after and the bytes that the merges wrote. Fails if the index did not settle in time. |
| `recover` | `--packs 1\|all`, `--read-only` | Moves the `.idx` of one pack or of all packs to `D/bench-saved`, times an open that must scan those packs, and then restores the `.idx` files. |
| `gc` | `--live-every K` (default 4), `--dead-run R` (default 1) | Opens the store and runs one complete GC (`gc_begin`, then `gc_finish`). Object `i` is dead when `(i / R) mod K` is 0, so (K-1)/K of the objects stay live, and the dead objects lie in runs of R objects. Reports what the GC removed, its wall time, and the longest and the total hold of the store lock (`ChunkStore::last_gc_timing`). |
| `run` | `--objects N`, `--reps R` (default 3), `--pack-limit`, `--memtable-limit`, `--hits`, `--misses`, `--drop-caches` | Runs create (if necessary), one warm-up open, R opens, lookup, R recovery opens for each scope (one, all) and mode (rw, ro), and a last open as a check. Then it prints the summary tables with the median of the R runs. |

Every step except `create` refuses a directory without the marker file. Thus the tool cannot change a real store.

The `gc` step is not part of `run`, because it removes objects. Use a store that you make only for it:

```sh
$B create --dir "$D/gc-10000000" --objects 10000000
$B gc --dir "$D/gc-10000000" --dead-run 1024
```

Before the GC, the step adds `gc_done=1` to the marker. After that, every step refuses the store, because it no longer has the N objects of `create`.

In a full 64 MiB pack where at least half of the bytes stay live, the GC punches holes only over whole 4 KiB blocks of dead records. A record of an 8-byte object has 54 bytes, so with the default `--dead-run 1` (every fourth object dead) such a pack gets no hole and the GC removes nothing. Use `--dead-run 1024` to measure a GC that punches. A small pack (less than 1/8 of the pack limit of the open, which is the default 64 MiB) is compacted instead, so on a test store with `--pack-limit 65536` the GC removes every dead object.

`run` starts a new process for each step. The reason: the peak RSS (`ru_maxrss` from `getrusage`) of a process only goes up, and the `create` process already holds the full index. Thus each RSS value must come from a new process.

The `open` step and the other steps exit immediately after they print their output. They do not drop the store, because the drop of a large map takes seconds and is not part of the measurement.

## Output

Each step prints `key=value` lines, one key on each line, in a fixed order. The keys are stable: new keys come after the old keys of a step, and no old key changes its name or place.

| Step | Key | Meaning |
|---|---|---|
| all | `step`, `backend` | The step and the backend |
| create | `objects`, `packs` | Objects and pack files in the new store |
| create | `disk_bytes`, `disk_bytes_per_object` | Size of all files of the store (packs, `.idx` files and the index directory `D/index`), in total and for each object. The marker file and `D/bench-saved` are not counted. A #66 store has no index directory, so for it this is the same as the size of `D/packs`. |
| create | `wall_s`, `cpu_s`, `puts_per_s` | Wall time and CPU time (user + system) of all puts and the close; puts per second of wall time |
| create | `max_rss_bytes` | Peak RSS of the create process |
| create | `index_bytes`, `index_bytes_per_object` | Size of the files in `D/index` (run files and the MANIFEST; 0 if there is no such directory), in total and for each object |
| open | `cache` | The `--cache` label (`warm` by default) |
| open | `objects`, `packs` | Objects and packs that the open found |
| open | `wall_s`, `cpu_s` | Wall time and CPU time of the open |
| open | `max_rss_before_bytes`, `max_rss_after_bytes` | Peak RSS before and after the open |
| open | `rss_before_bytes`, `rss_after_bytes` | Current RSS before and after the open (`task_info` on macOS, `/proc/self/statm` on Linux) |
| open | `peak_bytes_per_object` | (peak RSS after - peak RSS before) / objects |
| open | `steady_bytes_per_object` | (current RSS after - current RSS before) / objects |
| open | `index_runs`, `index_filter_bytes`, `index_fence_bytes` | Run files of the index, and the bytes of their Bloom filters and fence tables (from `ChunkStore::index_info`) |
| open | `index_memtable_entries`, `hwm_pack` | Entries in the memtable after the open (the records above the high-water mark), and the pack of the high-water mark |
| open | `rss_anon_before_bytes`, `rss_anon_after_bytes` | Anonymous resident memory (heap, stacks) before and after the open: `RssAnon` of `/proc/self/status` on Linux, the `internal` value of `task_info(TASK_VM_INFO)` on macOS |
| open | `rss_file_before_bytes`, `rss_file_after_bytes` | Resident pages of mapped files (the mapped index runs, the binary) before and after the open: `RssFile` on Linux, the `external` value of `task_info(TASK_VM_INFO)` on macOS |
| open | `steady_anon_bytes_per_object`, `steady_file_bytes_per_object` | The growth of each part during the open / objects |
| create | `index_runs` | Run files of the index after the write load (before the close) |
| create | `merge_bytes`, `merge_bytes_per_index_byte` | Bytes that the index merges wrote during the create, in total and for each byte of `index_bytes` |
| lookup | `objects`, `hits`, `misses` | Objects in the store and the sample sizes |
| lookup | `contains_hit_ns`, `contains_hit_per_s` | Time of one `contains` that finds the ID, and calls per second |
| lookup | `contains_miss_ns`, `contains_miss_per_s` | The same for IDs that are not in the store |
| lookup | `put_hit_ns`, `put_hit_per_s` | Time of one `put` of an object that is in the store, and calls per second |
| lookup | `rss_anon_before_lookups_bytes`, `rss_anon_after_lookups_bytes`, `rss_file_before_lookups_bytes`, `rss_file_after_lookups_bytes` | The two parts of the RSS before and after the timed lookups. The lookups read run pages through the mapping, so the file part grows here. |
| lookup | `lookups_anon_bytes_per_object`, `lookups_file_bytes_per_object` | The growth of each part during the lookups / objects |
| lookup | `contains_hit_ns_2`, `contains_miss_ns_2`, `put_hit_ns_2` | The same times in the second (warm) pass |
| lookup | `index_runs` | Run files of the index during the lookups |
| settle | `objects`, `settled`, `wait_s` | Objects in the store; 1 if the index settled; time of the open and the wait |
| settle | `index_runs_before`, `index_runs_after` | Run files after the open and after the idle merge |
| settle | `index_bytes_before`, `index_bytes_after` | Size of the files in `D/index` before and after |
| settle | `merge_bytes`, `merge_bytes_per_index_byte` | Bytes that the merges wrote, in total and for each byte of `index_bytes_before` |
| recover | `scope`, `mode` | `one` or `all`; `rw` (normal open) or `ro` (read-only open) |
| recover | `packs_dropped`, `sealed_packs`, `active_packs` | Packs without an `.idx` for this open, and how many of them are sealed packs and the last (active) pack |
| recover | `scanned_bytes`, `scanned_objects` | Size and objects of the dropped packs that the open scans: the packs at or above the high-water mark (all dropped packs if the backend has no mark) |
| recover | `wall_s`, `cpu_s` | Wall time and CPU time of the recovery open |
| recover | `objects` | Objects after the open (must be N) |
| recover | `idx_identical` | `yes` if the open wrote new `.idx` files and each one is byte-identical to the saved file; `n/a` if the open wrote no `.idx` |
| recover | `covered_packs` | Dropped packs below the high-water mark: the index runs hold their entries, so the open does not scan them |
| recover | `hwm_pack` | The pack of the high-water mark, read with a read-only open (not timed) before the `.idx` files move; `n/a` if the backend has none |
| gc | `objects`, `live_every`, `dead_run`, `dead_objects` | N, the flags, and the objects that are not in the live set |
| gc | `live_set_s` | Time to make the live set (before the open; not part of the GC) |
| gc | `removed_objects`, `kept_objects`, `removed_bytes` | From `GcStats`. A dead object that the GC cannot remove (no whole block to punch) is counted as kept. |
| gc | `packs_punched`, `packs_compacted`, `packs_deleted` | From `GcStats` |
| gc | `wall_s` | Wall time of `gc_finish` (`GcTiming::wall`) |
| gc | `lock_hold_max_ms`, `lock_hold_total_ms` | The longest hold of the store lock in `gc_finish`, and the sum of all holds (`GcTiming`) |
| gc | `objects_after`, `packs_after` | Objects and packs after the GC |
| gc | `max_rss_bytes` | Peak RSS of the gc process (it includes the live set) |
| gc | `live_set_bytes` | Bytes of the live set: a Bloom filter of the live IDs at 10 bits for each object of the store (#113) |
| gc | `live_false_positives` | Dead objects that the filter says may be live (the GC keeps them) |
| gc | `dead_left` | Dead objects still in the store after the GC: the false positives, and the dead records that a punched pack keeps because they do not fill a whole block |

The `run` step adds lines that start with `#` (the step names) and these summary tables:

- Store: packs, disk bytes for each object, puts per second of `create`, peak RSS of the create process, index bytes for each object.
- Open: median, minimum and maximum open time, median CPU time, median peak and steady RSS for each object, the cache state, the median steady anonymous and file-backed RSS for each object, and the index runs.
- Lookups: `contains` hit and miss time, `put` dedup-hit time, and calls per second; the index runs; and the hit, miss and dedup-hit times of the second (warm) pass.
- Recovery: one row for each scope and mode. "Recovery open s" is the median time of the recovery open. "Extra s" is that time minus the median normal open time. The scan rates (MB/s and objects/s) are the scanned amount divided by "Extra s". "Covered packs" is `covered_packs`. "Extra CPU s" is the median CPU time of the recovery opens minus the median CPU time of the normal opens, and "Scan MB/s (CPU)" is the scanned MB divided by it (`n/a` if nothing was scanned or the value is not positive). A loaded host changes the CPU time less than the wall time.

Peak RSS (`ru_maxrss`) has no split into anonymous and file-backed pages.

## Cache state

By default, all numbers are for a warm page cache: `run` does one warm-up open before it measures, and the store files stay in the cache.

- Linux: `run --drop-caches` writes `3` to `/proc/sys/vm/drop_caches` before each measured open and each recovery open. This needs root in a privileged container.
- macOS: `--drop-caches` is refused. Run `sudo purge` by hand, then `$B open --dir "$D/N" --cache cold`. Run `sudo purge` again before each cold open.

Record the cache state with every number.

## Disk and memory

- Disk: the packs and `.idx` files use about 104 bytes for each object (a 54-byte record in the pack and a 50-byte entry in the `.idx`). The on-disk index (#85) adds its run files in `D/index`: about 58 bytes for each entry, plus filters and tables. 100M objects use about 16.4 GB (164 bytes for each object) in 81 packs and the index. A merge writes its output before it removes its inputs, so for a short time the index can use more.
- RAM: the measured values are in the design note. With the in-memory index (#66), the create process and an open of 100M objects needed about 15 GB each.
- The `gc` step keeps the live set in RAM: a set of (K-1)/K of the IDs.

## Recovery paths

- A pack without an `.idx` that is not the last pack is a sealed pack. The open checks each record (CRC and BLAKE3) and writes a new `.idx`. A read-only open (`--read-only`) does not write the `.idx`.
- The last pack without an `.idx` is the active pack. The open checks only the CRC of each record and writes no `.idx`.
- A store with up to about 1.24M objects has one pack. Thus its "one" row is the active-pack path.
- The tool saves each `.idx` before the open and restores it after the open. It keeps a new `.idx` only if it is byte-identical to the saved file.
- With the on-disk index (#85), an open does not read the `.idx` of a pack below the high-water mark: the index runs hold its entries. Such a pack is not scanned and gets no new `.idx`, and `recover` counts it in `covered_packs`, not in `scanned_bytes`. Only the packs at or above the mark (usually the last one or two) are scanned. Thus a recovery open of the new index is about the same as a normal open, and its rows do not compare like for like with the #66 recovery rows, where every dropped pack was scanned.

## Linux

```sh
scripts/docker-linux-test/run.sh index-bench 1000000 10000000
```

This runs the tool in the Docker image of `scripts/docker-linux-test`, without a mount and without the FUSE device. The stores are on the Docker volume `slicefs-index-bench`. The mode prints the free memory of the VM and the free disk of the volume first, and the load average before and after each size. When you do not need the stores, remove the volume:

```sh
docker volume rm slicefs-index-bench
```

## Other backends

All store calls go through the `Backend` and `Store` traits in `src/backend.rs`. A new backend goes there; the steps do not change. `Store::index_keys` and `Store::gc` have defaults (no keys; gc not supported), so a backend without an index or a GC still works with every other step. The `chunk-store` backend waits for its background merges (`ChunkStore::wait_index_idle`) before each close, so each measured open finds a settled store. Every step except `settle` opens the chunk store with no idle merge (`Options::index_idle_merge_after = None`), so a step measures the runs as the step before left them; `settle` opens with the default options and waits with `ChunkStore::wait_index_settled`.
