# index-bench

Measures the in-memory index of the chunk store (#66): open time, RAM per object, lookup speed, and the time to recover a pack that has no `.idx` file. The results are the baseline for the on-disk index of #85. They are in `docs/design/chunk-store.md`, section "Index baseline (#66)".

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
| `create` | `--objects N`, `--pack-limit BYTES` (only for tests) | Makes a store of N objects in an empty directory, closes it (every pack is sealed and has an `.idx`), and writes the marker file `D/index-bench.txt`. |
| `open` | `--cache warm\|cold` | Opens the store one time and measures the time and the RSS (resident set size). `--cache` is only a label in the output. It does not change the cache. |
| `lookup` | `--hits K`, `--misses K`, `--seed S` | Opens the store and times `contains` for K IDs in the store (hits) and K IDs not in the store (misses), and `put` of K objects that are in the store (the dedup-hit path, with the hash). K is 1,000,000 by default, or N if N is smaller. The seed is 66 by default. |
| `recover` | `--packs 1\|all`, `--read-only` | Moves the `.idx` of one pack or of all packs to `D/bench-saved`, times an open that must scan those packs, and then restores the `.idx` files. |
| `run` | `--objects N`, `--reps R` (default 3), `--pack-limit`, `--hits`, `--misses`, `--drop-caches` | Runs create (if necessary), one warm-up open, R opens, lookup, R recovery opens for each scope (one, all) and mode (rw, ro), and a last open as a check. Then it prints the summary tables with the median of the R runs. |

Every step except `create` refuses a directory without the marker file. Thus the tool cannot change a real store.

`run` starts a new process for each step. The reason: the peak RSS (`ru_maxrss` from `getrusage`) of a process only goes up, and the `create` process already holds the full index. Thus each RSS value must come from a new process.

The `open` step and the other steps exit immediately after they print their output. They do not drop the store, because the drop of a large map takes seconds and is not part of the measurement.

## Output

Each step prints `key=value` lines, one key on each line, in a fixed order. The keys are stable. Phase 17 (#85) compares its results with these keys, so do not change them.

| Step | Key | Meaning |
|---|---|---|
| all | `step`, `backend` | The step and the backend |
| create | `objects`, `packs` | Objects and pack files in the new store |
| create | `disk_bytes`, `disk_bytes_per_object` | Size of the files in `D/packs` (packs and `.idx` files), in total and for each object |
| create | `wall_s`, `cpu_s`, `puts_per_s` | Wall time and CPU time (user + system) of all puts and the close; puts per second of wall time |
| create | `max_rss_bytes` | Peak RSS of the create process |
| open | `cache` | The `--cache` label (`warm` by default) |
| open | `objects`, `packs` | Objects and packs that the open found |
| open | `wall_s`, `cpu_s` | Wall time and CPU time of the open |
| open | `max_rss_before_bytes`, `max_rss_after_bytes` | Peak RSS before and after the open |
| open | `rss_before_bytes`, `rss_after_bytes` | Current RSS before and after the open (`task_info` on macOS, `/proc/self/statm` on Linux) |
| open | `peak_bytes_per_object` | (peak RSS after - peak RSS before) / objects |
| open | `steady_bytes_per_object` | (current RSS after - current RSS before) / objects |
| lookup | `objects`, `hits`, `misses` | Objects in the store and the sample sizes |
| lookup | `contains_hit_ns`, `contains_hit_per_s` | Time of one `contains` that finds the ID, and calls per second |
| lookup | `contains_miss_ns`, `contains_miss_per_s` | The same for IDs that are not in the store |
| lookup | `put_hit_ns`, `put_hit_per_s` | Time of one `put` of an object that is in the store, and calls per second |
| recover | `scope`, `mode` | `one` or `all`; `rw` (normal open) or `ro` (read-only open) |
| recover | `packs_dropped`, `sealed_packs`, `active_packs` | Packs without an `.idx` for this open, and how many of them are sealed packs and the last (active) pack |
| recover | `scanned_bytes`, `scanned_objects` | Size of the scanned packs and their objects |
| recover | `wall_s`, `cpu_s` | Wall time and CPU time of the recovery open |
| recover | `objects` | Objects after the open (must be N) |
| recover | `idx_identical` | `yes` if the open wrote new `.idx` files and each one is byte-identical to the saved file; `n/a` if the open wrote no `.idx` |

The `run` step adds lines that start with `#` (the step names) and these summary tables:

- Store: packs, disk bytes for each object, puts per second of `create`, peak RSS of the create process.
- Open: median, minimum and maximum open time, median CPU time, median peak and steady RSS for each object, and the cache state.
- Lookups: `contains` hit and miss time, `put` dedup-hit time, and calls per second.
- Recovery: one row for each scope and mode. "Recovery open s" is the median time of the recovery open. "Extra s" is that time minus the median normal open time. The scan rates (MB/s and objects/s) are the scanned amount divided by "Extra s".

## Cache state

By default, all numbers are for a warm page cache: `run` does one warm-up open before it measures, and the store files stay in the cache.

- Linux: `run --drop-caches` writes `3` to `/proc/sys/vm/drop_caches` before each measured open and each recovery open. This needs root in a privileged container.
- macOS: `--drop-caches` is refused. Run `sudo purge` by hand, then `$B open --dir "$D/N" --cache cold`. Run `sudo purge` again before each cold open.

Record the cache state with every number.

## Disk and memory

- Disk: about 104 bytes for each object (a 54-byte record in the pack and a 50-byte entry in the `.idx`). 100M objects use about 10.4 GB in about 81 packs.
- RAM: the create process of 100M objects needs about 15 GB, and an open needs about the same. The measured values are in the design note.

## Recovery paths

- A pack without an `.idx` that is not the last pack is a sealed pack. The open checks each record (CRC and BLAKE3) and writes a new `.idx`. A read-only open (`--read-only`) does not write the `.idx`.
- The last pack without an `.idx` is the active pack. The open checks only the CRC of each record and writes no `.idx`.
- A store with up to about 1.24M objects has one pack. Thus its "one" row is the active-pack path.
- The tool saves each `.idx` before the open and restores it after the open. It keeps a new `.idx` only if it is byte-identical to the saved file.

## Linux

```sh
scripts/docker-linux-test/run.sh index-bench 1000000 10000000
```

This runs the tool in the Docker image of `scripts/docker-linux-test`, without a mount and without the FUSE device. The stores are on the Docker volume `slicefs-index-bench`. The mode prints the free memory of the VM and the free disk of the volume first, and the load average before and after each size. When you do not need the stores, remove the volume:

```sh
docker volume rm slicefs-index-bench
```

## Other backends

All store calls go through the `Backend` and `Store` traits in `src/backend.rs`. Phase 17 (#85) adds its backends there. The steps and the output keys do not change.
