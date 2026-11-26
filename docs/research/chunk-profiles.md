# Chunk profiles: five FastCDC profiles on Boost (#67)

Plan 22-04, run on 2026-11-26. The data is in
`.planning/phases/22-chunk-profiles/runs/chunk-profiles-results.csv` (one row
per run, input and profile, and one `median` row per input and profile),
`chunk-profiles-edits.csv` (one row per edit) and `chunk-profiles-machine.txt`.
Every number in the tables below is a median row value of that CSV, or the
median of the three runs in the edits CSV. The derived numbers (ratios and
percents) are computed from those medians by the formulas given with them.

## Summary

- Recommended default for a later release: **8/32/128 KiB**. On the Boost tar
  it stores 15.0 % less than 4/8/32 KiB, with 74.7 % fewer objects (so 74.7 %
  fewer index entries) and a 76.8 % shorter cold open. Random 4 KiB reads get
  slower: p50 +36.8 %, p99 +30.2 %. The next step, 16/64/256 KiB, saves only
  3.3 more points of stored bytes on the tar (-18.3 %), but random reads p50
  get +131.1 % slower.
- On the two Boost source trees (154,456 small files) the profile matters
  little: 8/32/128 KiB stores 4.2 % less, with 15.1 % fewer objects.
- The code default stays 4/8/32 KiB in v2.2; a change of the default is an owner decision after this report.

Larger chunks dedup a little less (trees: 1.78x at 4/8/32 KiB, 1.66x at
32/128/512 KiB), but zstd compresses a larger object better (trees: 3.51x
to 3.95x). The compression gain is larger than the dedup loss, so every
larger profile stores less than 4/8/32 KiB on both inputs.

## Setup

- Machine (from `chunk-profiles-machine.txt`): macOS (Darwin 25.2.0),
  x86_64, Intel Xeon W-3245 at 3.20 GHz, 32 CPUs, 192 GiB of memory, APFS.
  Commit `3894905` (the docs commit of this plan; the code is the merged
  tree of plans 22-01 to 22-03). `rustc` is `unknown` in machine.txt,
  because the run shell did not have the Rust tools in its PATH; the release
  binary was built before the run with rustc 1.95.0-nightly.
- Load: test binaries of another project ran at the start (about six busy
  CPUs in `ps`); no cargo, rustc or other SliceFS job ran. The 1-minute load was
  8.00 before and 9.39 after the run (machine.txt: `8.00 8.98 10.79` and
  `9.39 9.19 10.27`). Byte and object counts do not change with load: they
  were the same in all three runs. Times can: treat them as indicative.
- Time: 10:32:32Z to 10:38:21Z UTC, 5 min 49 s of wall time for 3 runs of
  two inputs and five profiles.
- Inputs: `trees` is Boost 1_84_0, then 1_85_0, into one store, so the
  store dedups across versions (1,504,269,678 bytes in 154,456 files).
  `tar` is the Boost 1_85_0 tar alone (823,613,440 bytes, 1 file). Both
  come from `scripts/macos-mount-test/fetch-boost.sh`.
- Runs: 3. Fallback: none (the full run fit in the time limit).
- Command lines (`M` is the measure root, outside the repo):

```sh
M=${MEASURE_ROOT:-$HOME/slicefs-measure}
timeout 1200 scripts/macos-mount-test/fetch-boost.sh "$M/data" 1_84_0
timeout 1200 cargo build --release -p chunk-store --example chunk_profile_bench
OUT="$M/results/chunk-profiles-2026-11-26"
WORK="$OUT" BIN=target/release/examples/chunk_profile_bench \
  timeout 5400 bash scripts/chunk-profiles/bench.sh "$M/data" 3 \
  > "$OUT.stdout.csv" 2> "$OUT.progress.log"
```

The script defaults were used: `PROFILES` (the five profiles),
`INPUTS="trees tar"`, `BOOST_VERS="1_84_0 1_85_0"`, `SEQ_BYTES` and
`EDIT_BYTES` 64 MiB, `READS=10000`, `EDIT_SIZE=4096`.

Limits of this run:

- The page cache is warm: the script reads each input once before the
  runs, and it does not drop the cache. Reads and cold opens hit the page
  cache, not the disk.
- The work is in process, through the chunk-store API. Reads go through
  `FileReader`, with no FUSE and no mount. A read through a mount adds a
  fixed cost per request on top, which makes the relative read cost of a
  large profile smaller; this run does not measure it.
- The tool stores file content and manifest nodes only. In a real store,
  the metadata values (`metadata::store_io`) are cut with the store profile
  too (D-14). That adds to the rewrite cost of an edit (one more changed
  value, cut with the same profile), and it is not in these numbers.
  Manifest node boundaries (`LEAF_ENTRIES`, `NODE_MIN`, `is_cut` on IDs) do
  not depend on the profile.
- Index: no store reached 262,144 index entries (the memtable limit), so
  every row has `index_runs` 0, and its cold open replays the pack `.idx`
  files into the memtable (`index_memtable_entries` = objects). So the cold
  open and peak RSS here measure the replay path. A store with runs on
  disk loads its index another way (see `scripts/chunk-profiles/README.md`).
- One tar only: the tar input has almost no dedup (1.03x at 4/8/32 KiB). The
  cross-version dedup is in `trees` only.

## Results

MB is 10^6 bytes. CPU s is user plus system time of the ingest process.
Apparent and allocated bytes are the store files (packs and index).

### trees (Boost 1_84_0 then 1_85_0, one store)

| Profile (KiB) | Apparent MB | Allocated MB | Stored MB | Objects | Index runs | Memtable entries | Cold open ms | Open peak RSS MiB | Ingest MB/s | CPU s |
|---|---|---|---|---|---|---|---|---|---|---|
| 4/8/32 | 262.3 | 264.7 | 241.2 | 220,311 | 0 | 220,311 | 111.2 | 42.4 | 102.3 | 13.69 |
| 8/16/64 | 252.9 | 254.6 | 234.1 | 195,817 | 0 | 195,817 | 100.4 | 41.4 | 95.6 | 14.62 |
| 8/32/128 | 249.0 | 251.2 | 231.0 | 187,101 | 0 | 187,101 | 95.8 | 41.9 | 97.8 | 14.46 |
| 16/64/256 | 247.3 | 249.0 | 229.8 | 182,527 | 0 | 182,527 | 93.9 | 38.9 | 97.2 | 14.57 |
| 32/128/512 | 246.4 | 247.8 | 229.1 | 180,470 | 0 | 180,470 | 92.6 | 39.3 | 99.2 | 14.28 |

### tar (Boost 1_85_0)

| Profile (KiB) | Apparent MB | Allocated MB | Stored MB | Objects | Index runs | Memtable entries | Cold open ms | Open peak RSS MiB | Ingest MB/s | CPU s |
|---|---|---|---|---|---|---|---|---|---|---|
| 4/8/32 | 200.0 | 201.1 | 194.2 | 60,997 | 0 | 60,997 | 28.7 | 14.2 | 180.3 | 3.82 |
| 8/16/64 | 178.2 | 179.2 | 175.4 | 29,445 | 0 | 29,445 | 13.2 | 6.7 | 215.6 | 3.07 |
| 8/32/128 | 166.6 | 168.1 | 165.1 | 15,458 | 0 | 15,458 | 6.7 | 4.3 | 239.3 | 2.80 |
| 16/64/256 | 159.3 | 159.8 | 158.6 | 7,614 | 0 | 7,614 | 3.1 | 2.9 | 250.9 | 2.67 |
| 32/128/512 | 152.7 | 153.1 | 152.4 | 3,469 | 0 | 3,469 | 1.5 | 2.8 | 250.4 | 2.69 |

Reads and edits of the tar. Reads are 4 KiB: sequential over the first
64 MiB, and 10,000 random reads. The edits are nine 4 KiB edits of the
first 64 MiB (the edit base), through `splice`; the columns are the sums
over the nine edits.

| Profile (KiB) | Seq p50 us | Seq p99 us | Rand p50 us | Rand p99 us | Edit objects | Edit stored KB | Edit rechunked MB |
|---|---|---|---|---|---|---|---|
| 4/8/32 | 0.3 | 33.4 | 52.7 | 168.3 | 29 | 211.1 | 201.5 |
| 8/16/64 | 0.2 | 50.7 | 60.4 | 140.9 | 30 | 313.4 | 201.6 |
| 8/32/128 | 0.2 | 75.8 | 72.1 | 219.2 | 27 | 335.1 | 201.8 |
| 16/64/256 | 0.2 | 114.0 | 121.8 | 320.3 | 18 | 359.8 | 202.2 |
| 32/128/512 | 0.2 | 166.1 | 257.0 | 576.3 | 18 | 328.3 | 203.2 |

`Edit rechunked MB` is CPU work, not space: an insert or delete at the
start or middle makes `splice` cut the rest of the edit base again, but
FastCDC finds the old cuts again and those chunks dedup. `Edit objects`
and `Edit stored KB` are the space cost.

### Per edit (tar, median of 3 runs: new objects / new stored KB)

| Edit | 4/8/32 | 8/16/64 | 8/32/128 | 16/64/256 | 32/128/512 |
|---|---|---|---|---|---|
| modify start | 3 / 14.0 | 3 / 12.0 | 3 / 51.1 | 2 / 47.3 | 2 / 36.8 |
| modify middle | 4 / 42.7 | 6 / 89.4 | 3 / 48.9 | 2 / 47.1 | 2 / 55.6 |
| modify end | 3 / 17.9 | 3 / 14.1 | 3 / 16.0 | 2 / 30.0 | 2 / 21.4 |
| insert start | 3 / 14.2 | 3 / 12.1 | 3 / 51.2 | 2 / 47.4 | 2 / 36.9 |
| insert middle | 4 / 44.0 | 3 / 78.4 | 3 / 49.5 | 2 / 47.5 | 2 / 56.0 |
| insert end | 3 / 17.9 | 3 / 14.1 | 3 / 16.1 | 2 / 30.0 | 2 / 21.4 |
| delete start | 3 / 9.4 | 3 / 10.2 | 3 / 46.3 | 2 / 42.4 | 2 / 31.9 |
| delete middle | 3 / 37.4 | 3 / 73.2 | 3 / 44.3 | 2 / 42.2 | 2 / 50.9 |
| delete end | 3 / 13.7 | 3 / 9.8 | 3 / 11.7 | 2 / 25.9 | 2 / 17.3 |

The new objects of an edit are the changed chunks plus the new manifest
nodes. The stored bytes follow the size and the compression of the changed
chunks, so they do not grow in step with the chunk size: a changed chunk of
a large profile compresses better.

## Trade-offs

Formulas: dedup = `in_bytes / raw_bytes`; zstd = `raw_bytes / stored_bytes`;
in/apparent = `in_bytes / apparent_bytes`; a change in percent =
`(value of the profile / value of 4/8/32 - 1) x 100`, from the median rows.

Dedup and compression:

| Profile (KiB) | trees dedup | trees zstd | trees in/apparent | tar zstd | tar in/apparent |
|---|---|---|---|---|---|
| 4/8/32 | 1.78x | 3.51x | 5.73x | 4.13x | 4.12x |
| 8/16/64 | 1.73x | 3.71x | 5.95x | 4.63x | 4.62x |
| 8/32/128 | 1.71x | 3.82x | 6.04x | 4.94x | 4.94x |
| 16/64/256 | 1.68x | 3.89x | 6.08x | 5.17x | 5.17x |
| 32/128/512 | 1.66x | 3.95x | 6.11x | 5.40x | 5.39x |

Change against 4/8/32 KiB:

| Profile (KiB) | trees stored | trees objects | trees cold open | tar stored | tar objects | tar cold open | tar open RSS | tar ingest MB/s | rand p50 | rand p99 | edit stored |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 8/16/64 | -3.0 % | -11.1 % | -9.7 % | -9.7 % | -51.7 % | -53.8 % | -53.0 % | +19.6 % | +14.6 % | -16.3 % | +48.5 % |
| 8/32/128 | -4.2 % | -15.1 % | -13.9 % | -15.0 % | -74.7 % | -76.8 % | -69.5 % | +32.7 % | +36.8 % | +30.2 % | +58.7 % |
| 16/64/256 | -4.7 % | -17.2 % | -15.6 % | -18.3 % | -87.5 % | -89.1 % | -79.3 % | +39.2 % | +131.1 % | +90.3 % | +70.4 % |
| 32/128/512 | -5.0 % | -18.1 % | -16.8 % | -21.5 % | -94.3 % | -94.9 % | -80.6 % | +38.9 % | +387.7 % | +242.4 % | +55.5 % |

What the numbers say:

- Stored size and objects: every larger profile stores less and has fewer
  objects. On small files (trees) the gain is small, because most files
  are one chunk under any profile. On a large file (tar) the object count
  falls about in step with the average chunk size.
- Index RAM and cold open follow the object count (every row replays the
  `.idx` files): the tar open goes from 28.7 ms to 6.7 ms at 8/32/128 KiB.
  On the trees, the open peak RSS changes little (42.4 to 39.3 MiB).
- Ingest: on the tar, larger profiles ingest faster (180.3 to 250.9 MB/s)
  with less CPU (3.82 to 2.67 s), because there are fewer objects to hash,
  index and write. On the trees, the ingest speed is about the same for
  all profiles (95.6 to 102.3 MB/s; the run-to-run spread of 4/8/32 KiB
  was 76.3 to 103.5 MB/s), and the CPU time is a little higher for the
  larger profiles (13.69 to 14.62 s).
- Read latency: a random 4 KiB read decodes the whole chunk, so it gets
  slower with the chunk size. The p50 grows slowly up to 8/32/128 KiB
  (52.7 to 72.1 us) and fast after it (121.8 us at 16/64/256 KiB, 257.0 us
  at 32/128/512 KiB). The p99 of 8/16/64 KiB is lower than that of
  4/8/32 KiB, but its runs spread from 140.3 to 175.5 us, so this is inside
  the noise. Sequential p50 is a hit in the chunk that the reader holds, for all profiles; the sequential
  p99 grows with the chunk size (33.4 to 166.1 us), once per chunk.
- Rewrite after small edits: the nine 4 KiB edits add 211.1 KB at
  4/8/32 KiB and 313.4 to 359.8 KB at the larger profiles (+48.5 % to
  +70.4 %). The objects per edit stay at 2 to 6. The CPU work of a splice
  (`Edit rechunked MB`) is the same for all profiles.

## Recommendation

From these numbers, **8/32/128 KiB** is the best candidate for a later
default:

- It takes most of the gain of the large profiles on large files: on the
  tar, 74.7 % fewer objects and index entries (16/64/256 KiB: 87.5 %),
  15.0 % less stored (16/64/256 KiB: 18.3 %), a 76.8 % shorter cold open,
  and 32.7 % faster ingest.
- Its read cost is moderate: random 4 KiB reads p50 +36.8 % and p99
  +30.2 %. At 16/64/256 KiB the p50 is +131.1 %, and at 32/128/512 KiB
  +387.7 %, for only a few more points of stored bytes.
- Its edit cost is +58.7 % of new stored bytes (335.1 KB in place of
  211.1 KB for nine 4 KiB edits), about the same as the other larger
  profiles (+48.5 % to +70.4 %).
- On small files it is neutral to slightly better (trees: -4.2 % stored,
  -15.1 % objects).

8/16/64 KiB is the low-risk alternative: half the objects on large files
(-51.7 %), -9.7 % stored, and random reads within the noise of 4/8/32 KiB
(p50 +14.6 %).

The code default stays 4/8/32 KiB in v2.2, as the owner decided (D-01).
This report does not change it.

What would change the choice:

- A read test through a mount (FUSE) and with a cold cache. If the fixed
  cost per request hides the decode cost, a larger profile (16/64/256 KiB)
  is cheaper than it looks here. If small random reads of large files are
  the main load, 8/16/64 KiB or 4/8/32 KiB stays better.
- A store with index runs on disk (more than 262,144 entries): the cold
  open and the index RAM then depend on another path, and the object count
  can matter more or less.
- The metadata values (D-14) and the prolly-tree node policy of Phase 24:
  the combined benchmark there adds the metadata rewrite cost per edit,
  which this tool does not measure.
- Workloads with many small in-place edits of large files (databases,
  VM images): the edit cost grows by half or more with every larger
  profile.

## Reproduce

```sh
scripts/macos-mount-test/fetch-boost.sh DIR 1_84_0 1_85_0
cargo build --release -p chunk-store --example chunk_profile_bench
WORK=WORKDIR BIN=target/release/examples/chunk_profile_bench \
  timeout 5400 bash scripts/chunk-profiles/bench.sh DIR 3 > results.csv
```

The inputs, the columns, the edits and the notes on the index are in
`scripts/chunk-profiles/README.md`. The data of this run is in
`.planning/phases/22-chunk-profiles/runs/chunk-profiles-results.csv`,
`chunk-profiles-edits.csv` and `chunk-profiles-machine.txt`.
