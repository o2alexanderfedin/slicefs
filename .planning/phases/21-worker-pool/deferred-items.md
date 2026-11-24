# Phase 21 deferred items

## Open

Reports for the owner (from the measurements, no decision yet):
- **S7 at 16 writers (R2=miss, R3=miss).** 16 writers give 412 to 426 MiB, above 320 MiB. Pm(16) - Pm(4) = 199 MiB (rule: at most 64). Pm(8) - Pm(4) = 63 MiB passes by 1 MiB. Most of the rise above 8 writers is live heap (about 10 MiB for each writer) and mapped files, not malloc retention. 16 is above the default cap of 8, so this does not block. Options, if the owner wants 16 writers under 320 MiB: a step limit, the byte step pool (commit 964e13e) or a buffer reuse pool. Numbers: `runs/s7-step-limit-NOTES.md`.
- **Writes do not scale.** `ratio_write_4_1=1.26`. The write rate rises from 1 to 2 workers (134.9 to 178.8 MB/s) and then stays flat or falls (169.5 at 4, 165.6 at 8). The `store` lock (the single pack appender) has the largest wait in the write phase. This is the likely cause, but the bench does not rule out fsync or disk limits. Option: per-worker packs, after a measurement that isolates the cause.
- **16 workers give no gain over 8.** Medians at 16 against 8 are 0.84 to 1.04. The default cap of 8 stays.
- **Cost of the trace lines.** With `SLICEFS_FUSE_TRACE=1`, the metadata rates fall by 51 % to 77 %, and the read rate by 16 % (1 worker) to 34 % (4 workers). The lines are off by default.

Later work:
- **macOS default above 1.** Raise it only after a measured FSKit gain. Now (`runs/pool-bench-macos.log`, 3 CPUs, load about 50): write medians 25.2 MB/s with 1 worker and 37.3 with 2, read medians 265.6 and 165.7. The runs vary a lot, and they show no gain. Check the per-inode WRITE order first (next item).
- **Per-inode WRITE order on FUSE-T with 2 or more workers** (21-RESEARCH A6). FUSE-T can send parallel WRITEs of one file. Not measured. Check it before a macOS default above 1.
- **Abort on a flusher panic too** (21-RESEARCH Open Question 2). Only request threads abort now. A flusher panic during a publish can leave a half-done cell.
- **The Phase 19 windows with 2 or more workers.** A size change of a closed file against an unlink or a setattr of the same inode, and no ancestor check for a cross-directory rename. With 2 or more workers these can occur. On Linux, pjdfstest passed with 4 workers. FUSE-T with more than 1 worker is not measured. See `.planning/phases/19-fine-grained-locks/deferred-items.md`.
- **A rise of the Linux default cap of 8.** Only by the rule of the Phase 21 result: the median at 16 workers is at least 1.1 times the median at 8 in some phase, and the largest VmHWM at 16 is under CAP(16) = 480 MiB. Not met now.
- **WAL group commit** (step 7 of the migration; spike S6 decides).
- **Phase 19 and Phase 20 open items.** See `.planning/phases/19-fine-grained-locks/deferred-items.md` and `.planning/phases/20-dirty-budget-and-rebase-flusher/deferred-items.md`. Phase 21 did not change them.
- **Adaptive control from live statistics** (issue #152). Its inputs exist now: the `pool:` line (jobs, jobs for each worker, queue depth, largest weight, longest queue wait) and the `steps:` line (save steps, steps per second, freed bytes per step), next to the Phase 20 flusher stats.
- **Busy store outside the write path (#170 follow-up).** fsync, release, replay and commit still give EIO for a busy store. Only a write tries again.
- **Busy-store retry during the pool drain (#170 follow-up).** The retry stops at the shutdown mark, but `shutdown` runs after the drain. So a write in its retry during the drain can wait up to 60 s. The 200 s drain limit covers it.

Found in the plans and not fixed:
- **Stale doc comment of `SUBMIT_WAIT` in `pool.rs`.** It names "the 200 ms step-slot wait" in the sum 190.4 s. No step slot exists (no step limit was built). The value 200 s is correct. Fix the text with the next code change.
- **getlk and setlk doc comments** still say "SliceFS runs single-threaded" (21-02). The lock calls are no-ops, so the behavior is correct.
- **No-handler callbacks take the gate for nothing** (21-02): releasedir, access, setlk and fallocate call `serve(&fs, || ())`. A removal needs an allow-list in the scan test.
- **Worker thread name on Linux** (21-01): `slicefs-worker-<k>` is longer than the 15 bytes of a Linux thread name, so `top` and `ps` show `slicefs-worker-` for all workers.
- **Idle wait of a worker** (21-01): it has no time limit. It ends when a job comes or at the close of the queue.
- **Abort line at a short submit limit** (21-03): it gives whole seconds, so a test limit of 300 ms prints `0 s`.
- **`pool_panic_tests` on a loaded macOS host** (21-03): 23 to 60 s, from the start time of the test binary. Each run writes 3 crash reports on macOS.
- **`freed_bytes=0` for the one save step of the smoke** (21-05). Not examined. The S7 runs and `the_flusher_lowers_the_total` show that the counter works.
- **macOS `alloc_tune::stats()` path** (21-05): only the test `stats_show_a_live_heap` uses it. S7 does not run on macOS.
- **Slow 1-writer writes before the allocator setting** (21-04): 9.4 s and 9.6 s in 2 of 3 runs, cause not measured. The 21-06 runs show 0.7 to 1.6 s.
- **1-writer S7 peak 9 MiB higher with the allocator setting** (21-06): 106 MiB against 97. Not examined (possible cause: page rounding of mmapped blocks).
- **Small S7 margin at 8 writers** (21-06): CAP(8) = 336 is above CAP_MIB 320. Pm(8) = 290 leaves 30 MiB, from 3 runs. A later 8-writer run can miss the assert.
- **Slow writes at 16 writers** (21-06): 7.1 s and 7.5 s, cause not found. This is near the 10 s store wait. The slowest write is above 500 ms in 12 of 19 S7 runs. The Phase 20 delay-curve condition (research Q9) holds only as a borderline case at 8 and 16 writers.
- **One 68 ms `stripe` hold** at 8 workers in the metadata phase (21-08). The p99 wait is 0. The cause is not known.
- **`inode_map` wait** (21-08): the only metadata map lock with a real wait (p99 29.6 us at 4 workers, 120.6 us at 8). Input for #165 and the prolly-tree evaluation (Phase 24).
- **macOS bench log** (21-08): every `macbench` line prints `run=1`. The `== n=<N> run=<r>` header before each line gives the true run. Fix the script with the next code change.

## Closed in Phase 21

- **[Step limit] item.** R1=pass (S7 at or under 320 MiB at 1, 2, 4 and 8 writers) and mem_bar=pass (bench daemon VmHWM at most 178 MiB at 1, 2, 4 and 8 workers). The fix is the fixed glibc mmap threshold of plan 21-05. No step limit was built.
- **POOL-05 bar.** `verdict=PASS` (read 2.69 times at 4 workers). No follow-up issue is necessary.
