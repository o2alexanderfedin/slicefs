# Chunk profile benchmark

`bench.sh` compares the FastCDC chunk profiles of a store (#67, PROF-03).
For each run, input and profile, it records:

- physical bytes after dedup and zstd: store apparent and allocated bytes (packs and index), stored bytes, object count
- index RAM and cold open: index runs, run entries, memtable entries, filter and fence bytes, open time and peak RSS of a new process
- ingest throughput: MB/s, and CPU user and system time
- read latency: sequential and random 4 KiB reads, p50 and p99
- the bytes and objects that nine small edits add

The work runs in process, on the chunk-store API, in the example
`crates/chunk-store/examples/chunk_profile_bench.rs`. Nothing mounts, so the
script runs on macOS and on Linux. It runs by hand only. It is never part of
`cargo test` (`cargo test` only builds the example).

| Script | What |
|---|---|
| `bench.sh <data-dir> [runs]` | For each run (default 3), input and profile: ingest into a new store, open it cold, probe it; print one CSV row per step and one median row per input and profile |

The profiles are 4/8/32, 8/16/64, 8/32/128, 16/64/256 and 32/128/512 KiB
(min/avg/max). 4/8/32 KiB is the default profile of a store.

## Inputs

| Input | Source |
|---|---|
| `trees` | Every Boost tree `DIR/x/boost_<v>/` of `BOOST_VERS` (default 1_84_0 then 1_85_0), into one store, so the store dedups across versions |
| `tar` | The Boost tar of the last version, `DIR/boost_<v>.tar` |
| `rand` | One random file `DIR/in-rand-<RAND_BYTES>/random.bin`, made once (not in the default `INPUTS`) |
| `sparse` | One file `DIR/in-sparse-<SPARSE_BYTES>/sparse.bin` of zeros with 64 KiB of random bytes at every 16 MiB, made once (not in the default `INPUTS`) |

Get the Boost data with the mount-test script:

```sh
scripts/macos-mount-test/fetch-boost.sh DIR 1_84_0 1_85_0
```

A missing tree or tar stops the script (exit 2) with this command.

## Run

```sh
scripts/chunk-profiles/bench.sh DIR > results.csv
WORK=WORKDIR RUNS=5 PROFILES="4/8/32 16/64/256" scripts/chunk-profiles/bench.sh DIR
SMOKE=1 RUNS=1 scripts/chunk-profiles/bench.sh
```

On Linux the same lines work. The smoke in Linux Docker (no FUSE) is

```sh
scripts/docker-linux-test/run.sh chunk-profiles-smoke
```

It builds the example in release in the container and runs `bench.sh` with
`SMOKE=1 RUNS=1 PROFILES="4/8/32 16/64/256"`. Smoke run in Linux Docker
(plan 22-03): rows and edits as on macOS (8 CSV rows, 18 edit rows, the same
object counts and stored bytes). The image has no GNU `time`, so
`open_peak_rss_bytes` is `NA` there. A full run in Docker needs the data dir
in the container (a `-v DIR:/data:ro` volume, then `bench.sh /data`).

| Env | Default | What |
|---|---|---|
| `WORK` | a new temp dir | The store (`WORK/store`, removed after each step), step logs, `results.csv`, `edits.csv`, `machine.txt` |
| `BIN` | build it | The `chunk_profile_bench` binary. When unset, the script runs `cargo build --release -p chunk-store --example chunk_profile_bench` |
| `PROFILES` | the five profiles | Profiles `MIN/AVG/MAX` in KiB |
| `INPUTS` | `trees tar` | Inputs, in this order (also `rand`, `sparse`) |
| `BOOST_VERS` | `1_84_0 1_85_0` | Versions of `trees`; `tar` is the last one |
| `RAND_BYTES` | `1073741824` | Size of the `rand` file |
| `SPARSE_BYTES` | `1073741824` | Size of the `sparse` file |
| `SEQ_BYTES` | `67108864` | Sequential 4 KiB reads cover the first `SEQ_BYTES` of the file |
| `READS` | `10000` | Random 4 KiB reads |
| `EDIT_BYTES` | `67108864` | The edit base is the first `EDIT_BYTES` of the file |
| `EDIT_SIZE` | `4096` | Bytes of each edit |
| `RUNS` | `3` | Runs, when the second argument is not given |
| `SMOKE` | `0` | `1`: small synthetic inputs, a short probe (see below) |

stdout gets only CSV. Progress goes to stderr. The same rows also go to
`WORK/results.csv`, which the script makes new at each start. The loop order
is run, then input, then profile, so the profiles alternate inside each run.
After the runs, one `run=median` row per input and profile follows. In it,
each number is the `median_low` of that column over the runs, so it is always
a value from a real run. A column that is `NA` in every run stays `NA`.

The script removes only `WORK/store`, which it makes, and it refuses an empty
`WORK` or `/`. The script stops with a non-zero exit code when a step fails or
an input is missing.

## Smoke mode

`SMOKE=1` makes small inputs in `WORK/smoke-data` (or in the data dir, when
one is given) with `chunk_profile_bench gen`: two trees of 200 files each
(`x/boost_0_0_1`, then `x/boost_0_0_2` from the same seed, with other bytes
in 1 KiB of every tenth file) and an 8 MiB random file `boost_0_0_2.tar`. It
sets `BOOST_VERS="0_0_1 0_0_2"`, `EDIT_BYTES=4194304`, `SEQ_BYTES=4194304` and
`READS=500`, unless they are set. With `RUNS=1` and two profiles it gives
8 CSV rows and 18 edit rows. It checks that the script works; its numbers
say nothing about Boost.

## results.csv columns

| Column | Unit | What |
|---|---|---|
| `profile` | KiB | `MIN/AVG/MAX` |
| `input` | | `trees`, `tar`, `rand` or `sparse` |
| `run` | | 1 to RUNS, or `median` |
| `in_bytes` | bytes | Sum of the input file sizes |
| `in_files` | files | Input file count (regular files; symlinks are skipped) |
| `ingest_secs` | s | Wall time of the ingest: store open, all files through `FileWriter`, `close` |
| `ingest_mb_s` | MB/s | `in_bytes / ingest_secs / 10^6` |
| `cpu_user_s` | s | User CPU time of the ingest process (bash `time`) |
| `cpu_sys_s` | s | System CPU time of the ingest process (bash `time`) |
| `objects` | objects | Distinct objects in the store after the ingest (chunks and manifest nodes) |
| `raw_bytes` | bytes | Bytes of the distinct objects before zstd |
| `stored_bytes` | bytes | Bytes of the object payloads in the packs (after zstd) |
| `apparent_bytes` | bytes | Sum of the store file sizes (packs and index) |
| `alloc_bytes` | bytes | Allocated blocks of the store files x 512 |
| `index_runs` | runs | Index runs on disk at the cold open |
| `index_run_entries` | entries | Entries in those runs |
| `index_memtable_entries` | entries | Entries in the memtable after the cold open (replayed from the pack `.idx` files) |
| `index_filter_bytes` | bytes | Bloom filter bytes of the runs, in RAM |
| `index_fence_bytes` | bytes | Bytes of the fence tables of the runs, in RAM |
| `open_ms` | ms | Cold open: `ChunkStore::open` (read only) plus `index_info`, in a new process |
| `open_peak_rss_bytes` | bytes | Peak RSS of the open process: `/usr/bin/time -l` (macOS) or `-v` (GNU time on Linux); `NA` without one of them |
| `seq_p50_us`, `seq_p99_us` | us | p50 and p99 of sequential 4 KiB reads from offset 0 up to `SEQ_BYTES` |
| `rand_p50_us`, `rand_p99_us` | us | p50 and p99 of `READS` random 4 KiB reads (seeded) |
| `edit_objects` | objects | Sum over the nine edits of the new objects |
| `edit_stored_bytes` | bytes | Sum over the nine edits of the new stored bytes |
| `edit_rechunked_bytes` | bytes | Sum over the nine edits of the bytes that `splice` cut again with FastCDC |

MB is 10^6 bytes. The seven read and edit columns are `NA` for `trees` (the
probe runs only on the single-file inputs `tar`, `rand` and `sparse`). The
percentiles are nearest-rank, in microseconds with one decimal.

`edits.csv` has one row per edit: `profile,input,run,kind,where,objects,stored_bytes,rechunked_bytes`.
`machine.txt` has `os`, `arch`, `cpu`, `cpus`, `mem_bytes`, `fs` (the file
system of `WORK`), `rustc`, `commit`, `page_cache`, `load_before`,
`load_after`, `start_utc` and `end_utc`. It has no path, host name or user
name.

## Edits

The probe first writes the first `L = min(file length, EDIT_BYTES)` bytes of
the file as their own content: the edit base. Then it makes nine edits, each
from the base, through `splice`, with `k = EDIT_SIZE` new random bytes:

| Edit | start (p = 0) | middle (p = L/2) | end |
|---|---|---|---|
| `modify` | bytes [0, k) change | bytes [p, p+k) change | the last k bytes change |
| `insert` | k bytes go in at 0 | k bytes go in at p | k bytes are added after the end |
| `delete` | bytes [0, k) go out | bytes [p, p+k) go out | the last k bytes go out (truncate) |

For each edit, the row takes the change of the store object count and stored
bytes, and `rechunked` of the splice. Then the probe reads the edited window
(up to 32 KiB on each side of p) back and compares it with the expected
bytes. A mismatch stops the run, so a broken path never gives numbers. The
edit base keeps an edit near the start from re-chunking the whole tar nine
times per run.

## Notes

- Index RAM and cold open depend on how the index is loaded. `ChunkStore::close`
  does not flush the memtable, and a run is written only when the memtable is
  full (`index_memtable_limit`, 262,144 entries). So a store with fewer index
  entries than that has no run, and its cold open replays the pack `.idx`
  files into the memtable. Read `index_runs` and `index_memtable_entries`
  with `open_ms` and `open_peak_rss_bytes`: they show which mechanism each
  row measures.
- The page cache is warm: the script reads each input once before the runs,
  and it does not drop the cache. Drop it by hand for a cold run.
- Read latency is measured in process through `FileReader`, with no FUSE.
- In a real store, the metadata values are cut with the store profile too
  (D-14). This tool stores file content and manifest nodes only, so its edit
  numbers have no metadata part.
- Memory of the tool is bounded: files stream in 1 MiB pieces, and an edit
  reads its base through a `FileReader`, with no copy of the file.
- Run time of a full run: plan 22-04 measures it and writes it in
  `docs/research/chunk-profiles.md`.
