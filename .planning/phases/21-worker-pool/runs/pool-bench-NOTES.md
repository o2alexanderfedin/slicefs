# Pool bench (POOL-05): Linux Docker, 16 clients on different files

Log: `runs/pool-bench.log` (22 bench lines). Lock rows: `runs/pool-bench-lockstats.log`. Quick check of the script: `runs/pool-bench-quick.log`.

## System and method

- Code commit: `7853542bd6c5a6d3bf3d0e07dac7064aa4181e51` (the Task 1 code commit). The full set ran on a tar stream of the committed clean tree at 3af763e (the same code plus the quick log).
- RUSTC = rustc 1.100.0 (0ef1bc386 2026-11-09) (the same line as the 20-01 notes)
- `uname -r` 7.2.12-orbstack-00394-gbecdd6130068, `nproc` 32 (no CPU quota). Docker mount form, unshare, release build, the 45 s wait.
- Container load (1-minute) per run: 0.00 to 7.06. Host (macOS) load: 15.72 before, 17.99 after.
- Store and source files on the Docker volume `slicefs-pool-bench`; mount point under `/tmp` in the container.
- One run = 4 mounts of a fresh store, each with `--workers N`:
  - A: write phase. 16 parallel `dd bs=1M conv=fsync` of 64 MiB distinct random files.
  - B: read phase, cold cache. 16 parallel `dd` to `/dev/null`, then `cmp` of every file. Then the random-write phase: 16 processes, each does 64 writes of 1 MiB at random 1 MiB-aligned offsets of its own file, with a fixed seed, then fsync and close.
  - M: metadata phase. Each client works in its own directory: create 250 files of 4 KiB, stat each, read each (content check), unlink each. Ops/s per step.
  - V: content check of the random writes, cold cache.
- MB/s = 16 x 64 MiB / wall time. VmHWM is the `/proc/<daemon pid>/status` value before each unmount. The daemon pid is checked (`comm` = slicefs).
- Order: round 1 = 1 2 4 8 16, round 2 = 16 8 4 2 1, round 3 = 4 16 1 8 2. After each round there is one trace-on run (`SLICEFS_FUSE_TRACE=1`) at 1 worker and one at 4. At the end there is one run at 16 with `MALLOC_ARENA_MAX=2`.
- Trace lines are off by default (Amendment 2), so the 15 default runs are `trace=off` and the pair is `trace=on`.

Correctness: all 22 runs have 0 mismatches. This covers the `cmp` after the read phase, the content check of the random writes and the content check of the metadata reads. Every daemon exited within 30 s of its unmount.

## Medians of the 3 default runs (trace off) per worker count

| workers | write MB/s | read MB/s | rand-write MB/s | create files/s | stat ops/s | read ops/s | unlink ops/s | rand check MB/s |
|---|---|---|---|---|---|---|---|---|
| 1 | 134.9 | 345.8 | 115.2 | 5000.0 | 57971.0 | 11527.4 | 27972.0 | 217.8 |
| 2 | 178.8 | 550.8 | 128.3 | 5839.4 | 61538.5 | 12158.1 | 31496.1 | 365.2 |
| 4 | 169.5 | 931.8 | 145.6 | 6578.9 | 60606.1 | 16260.2 | 41666.7 | 504.2 |
| 8 | 165.6 | 1304.5 | 151.4 | 6600.7 | 58823.5 | 16326.5 | 40816.3 | 634.4 |
| 16 | 149.1 | 1190.7 | 131.4 | 5571.0 | 55555.6 | 16000.0 | 42553.2 | 648.5 |

Largest values of the 3 default runs:

| workers | VmHWM A (write) MiB | VmHWM B (read + rand write) MiB | VmHWM M (metadata) MiB | VmHWM V (check) MiB | max_queue_wait_ms | max_weight bytes |
|---|---|---|---|---|---|---|
| 1 | 58 | 124 | 42 | 35 | n/a (no pool) | n/a (no pool) |
| 2 | 70 | 140 | 42 | 36 | 1268 | 1048576 |
| 4 | 74 | 152 | 43 | 36 | 806 | 1048576 |
| 8 | 85 | 178 | 43 | 38 | 564 | 1048576 |
| 16 | 102 | 223 | 45 | 40 | 115 | 1048576 |

The largest queue waits are on the write mount A. The WRITE jobs of 16 clients wait behind the fsync and save work of the other jobs. The largest WRITE the pool saw is 1 MiB at every worker count. The metadata mount has a max queue wait of 2 to 3 ms.

Spread: the read phase varies a lot between runs. At 8 workers the values are 490.2, 1324.7 and 1304.5. At 16 they are 1190.7, 1365.3 and 438.5. The median removes one low run at each of these counts.

## Ratios and verdict (the bar set in 21-08-PLAN before the runs)

The metric is the median aggregate MB/s of 3 runs of the write phase or of the read phase, with the default trace setting (off), at `--workers 4` against `--workers 1`.

ratio_write_4_1=1.26
ratio_read_4_1=2.69
verdict=PASS

The read phase passes the bar (2.69 >= 1.5). The write phase alone does not pass (1.26). Every run has 0 mismatches.

The verify awk of the plan was written before Amendment 2 and selects ` trace=on `. With the amendment the default runs are ` trace=off `, so the ratios above come from the trace=off rows. The trace-on pair alone gives write 1.32 and read 2.10, which is also a PASS.

## Memory bar (320 MiB at 1, 2, 4 and 8 workers)

The largest VmHWM of mounts A and B is 178 MiB, at 8 workers. This covers every run at 1, 2, 4 and 8 workers, with trace on and off. The metadata and check mounts are lower (at most 43 and 38 MiB).

mem_bar=pass
mem16_max_mib=223

The VmHWM grows with the worker count: the largest value of mount B is 124, 140, 152, 178 and 223 MiB at 1, 2, 4, 8 and 16 workers. That is about 6.6 MiB per worker from 1 to 16.

## Trace-on pair (`SLICEFS_FUSE_TRACE=1`): the cost of the per-request lines

Medians of 3 runs each. "on/off" is the trace-on median divided by the default median.

| workers | phase | default (off) | trace on | on/off |
|---|---|---|---|---|
| 1 | write MB/s | 134.9 | 138.0 | 1.02 |
| 1 | read MB/s | 345.8 | 292.2 | 0.84 |
| 1 | rand-write MB/s | 115.2 | 109.5 | 0.95 |
| 1 | create files/s | 5000.0 | 2430.1 | 0.49 |
| 1 | stat ops/s | 57971.0 | 19512.2 | 0.34 |
| 1 | read ops/s | 11527.4 | 3996.0 | 0.35 |
| 1 | unlink ops/s | 27972.0 | 13745.7 | 0.49 |
| 4 | write MB/s | 169.5 | 182.6 | 1.08 |
| 4 | read MB/s | 931.8 | 613.2 | 0.66 |
| 4 | rand-write MB/s | 145.6 | 129.4 | 0.89 |
| 4 | create files/s | 6578.9 | 2476.8 | 0.38 |
| 4 | stat ops/s | 60606.1 | 17467.2 | 0.29 |
| 4 | read ops/s | 16260.2 | 3813.2 | 0.23 |
| 4 | unlink ops/s | 41666.7 | 12461.1 | 0.30 |

The lines cost nothing measurable on the write phase. They cost 16 % (1 worker) and 34 % (4 workers) of the read phase, and 51 % to 77 % of the metadata steps. With the lines on, the metadata rate does not grow from 1 to 4 workers (2430 and 2477 creates/s), so the stderr lock serializes the workers. This supports the [Trace default] decision (lines off by default).

In the trace-on runs, the metadata mount counts the requests per kind. Per run there were 4000 create, 4000 write, 4000 open, 4000 read, 4000 unlink, 8000 flush, 8000 release, 4000 getxattr, about 9900 lookup and about 2200 getattr. So the read and unlink steps reach the daemon for every file. The 4000 stats give only about 2200 getattr requests, because the kernel answers the rest from its attribute cache. The stat rate is therefore partly a kernel-cache rate, and it does not change with the worker count (55556 to 61539 ops/s).

## Arena run (16 workers, `MALLOC_ARENA_MAX=2`), recorded only

`bench n=16 round=1 trace=off arena=2`: write 144.3, read 1051.3, rand write 128.5 MB/s, 5256.2 creates/s. VmHWM A 93, B 210, M 44 and V 38 MiB. The default runs at 16 have a largest VmHWM of 102 (A) and 223 (B) MiB. So 2 arenas save 9 to 13 MiB at 16 workers, with similar throughput (one run).

## Rule for a later rise of the default cap of 8 (recorded only)

- Medians at 16 against 8: write 0.90, read 0.91, rand write 0.87, creates 0.84, stat 0.94, read ops 0.98, unlink 1.04. No phase reaches 1.10.
- The largest VmHWM at 16 is 223 MiB. This is under CAP(16) = 480 of plan 21-06.
- Result: the rule is not met (16 workers do not add throughput). The cap of 8 stays.

## Comparison with S3 (in process, no mount, before Phase 19)

| N | S3 Linux write MB/s | bench write MB/s | S3 Linux read MB/s | bench read MB/s |
|---|---|---|---|---|
| 1 | 117.9 | 134.9 | 854.3 | 345.8 |
| 2 | 160.4 | 178.8 | 918.5 | 550.8 |
| 4 | 132.9 | 169.5 | 319.5 | 931.8 |
| 8 | 120.4 | 165.6 | 762.1 | 1304.5 |

S3 had 1 thread per file and 1 run per N, in process. The bench goes through FUSE with 16 clients at every N, and the reads come from a cold page cache. In S3 the writes rise at 2 threads and fall after that. In the bench they also stop rising at 2 workers (178.8, then 169.5 and 165.6). In S3 the reads did not scale: the `readers` lock was held across the whole read. In the bench the reads scale to 8 workers, after the Phase 19 change to that lock.

## Lock-stats runs at 4 and 8 workers (owner request)

`runs/pool-bench-lockstats.log` has one run at 4 workers and one at 8. The build has the `lock-stats` feature and the default trace setting. The `lock_row` lines come from each unmount, so mount M gives the metadata phase alone. The six metadata map locks of `DictMetadataStore` are now sites: `inode_map`, `inode_data`, `dir_data`, `manifest_data`, `xattr_data` and `refcounts`. The recorder adds a shared mutex per lock event, so the throughput of these 2 runs is not in the tables above.

Metadata phase (mount M), the sites with the largest waits, in us:

| workers | site | count | wait p99 | wait max | hold p99 | hold max |
|---|---|---|---|---|---|---|
| 4 | store | 96922 | 49.6 | 2461.7 | 11.4 | 7217.2 |
| 4 | wal_writer | 56745 | 37.9 | 1340.4 | 9.6 | 875.0 |
| 4 | inode_map | 8162 | 29.6 | 735.7 | 15.0 | 76.7 |
| 4 | readers | 12000 | 0.3 | 707.1 | 2.4 | 591.4 |
| 4 | inode_live | 52451 | 0.0 | 741.4 | 2.3 | 151.7 |
| 8 | store | 108640 | 161.7 | 1687.6 | 20.8 | 7196.7 |
| 8 | inode_map | 8167 | 120.6 | 2996.2 | 22.4 | 245.4 |
| 8 | wal_writer | 55160 | 113.6 | 1530.9 | 12.6 | 890.4 |
| 8 | readers | 12000 | 97.6 | 2359.3 | 9.6 | 915.5 |
| 8 | stripe | 28048 | 0.0 | 47317.0 | 148.9 | 68157.4 |

- At 4 workers the waits are small: every p99 is under 50 us. At 8 workers four sites have a wait p99 of about 100 to 160 us: `store`, `inode_map`, `wal_writer` and `readers`.
- `inode_map` is the only metadata map lock with a real wait (p99 29.6 us at 4 and 120.6 us at 8). The other five map locks have a wait p99 of 0.3 us or less: `inode_data`, `dir_data`, `manifest_data`, `xattr_data` and `refcounts`.
- At 8 workers, one `stripe` hold was 68 ms and one wait was 47 ms. The p99 wait is 0.0, so this is one long hold, not a steady contention. The log does not show what held the stripe so long.
- These numbers match the flat metadata rates: create files/s is 6579 at 4 and 6601 at 8 workers.

Write phase (mount A): `store` has the largest wait. Its wait p99 is 90.9 us at 4 workers and 249.0 us at 8, and its wait max is 4.2 ms and 13.5 ms. Its hold max is 3.5 ms and 13.1 ms. All writers append to the store under this one lock (the single pack appender). This is the measured reason why the write phase stops rising at 2 workers (ratio 1.26). The read phase does not take this lock for long, and it scales.

## Notes

- The pass comes from the read phase. The write phase does not reach 1.5x. Its limit is the store lock (above), as S3 and the research foresaw ("the single pack appender remains").
- The memory bar passes with a wide margin: 178 of 320 MiB at 8 workers.
