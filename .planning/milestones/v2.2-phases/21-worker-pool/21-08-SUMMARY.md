---
phase: 21-worker-pool
plan: 08
subsystem: bench
status: complete
tags: [pool-05, bench, docker, vmhwm, lock-stats, macos-fskit]

requires:
  - phase: 21-worker-pool
    provides: "--workers and the Workers line (21-01), the pool line at unmount (21-01/21-02), SLICEFS_FUSE_TRACE off by default (Amendment 2), alloc_tune and the steps line (21-05), CAP(16) = 480 (21-06), the Docker method (21-07)"
provides:
  - "scripts/docker-linux-test/pool-bench.sh and run.sh pool-bench: the Linux pool bench (write, read, random write, small-file metadata) with VmHWM per mount"
  - "scripts/macos-mount-test/pool-bench.sh and .github/workflows/pool-bench-macos.yml (FSKit, 1 and 2 workers; dispatch or push to ci-pool/**)"
  - "lock_row lines at unmount in a lock-stats build; the 6 metadata map locks of DictMetadataStore are lock-stats sites"
  - "runs/pool-bench.log, runs/pool-bench-quick.log, runs/pool-bench-lockstats.log, runs/pool-bench-NOTES.md (verdict=PASS, mem_bar=pass)"
affects: [21-09]

estimate:
  tokens: 90000
  tasks: 3
actuals:
  tokens: 26300    # chars/4 over the added lines since ea76303 (91465 chars) plus this SUMMARY (13779 chars)
  tasks: 3
  commits: 4

plan_head_before: ea763033afa0fdcc3f2961bc636c3e55d5797fbf
plan_head_after: f6149bb781705afd3a400360065096c35da1c564
commits: 4

tech-stack:
  added: []
  patterns:
    - "One mount per bench phase group, so a lock-stats build gives lock rows of one phase (the metadata mount M)"
    - "The daemon pid is the `$!` of `env -u ... VAR=v slicefs mount ... &`, checked with /proc/<pid>/comm before VmHWM is read"
    - "Random writes from a seeded RNG, checked later by replaying the seed (1 MiB buffers, no whole file in memory)"

key-files:
  created:
    - scripts/docker-linux-test/pool-bench.sh
    - scripts/macos-mount-test/pool-bench.sh
    - .github/workflows/pool-bench-macos.yml
    - .planning/phases/21-worker-pool/runs/pool-bench-quick.log
    - .planning/phases/21-worker-pool/runs/pool-bench.log
    - .planning/phases/21-worker-pool/runs/pool-bench-lockstats.log
    - .planning/phases/21-worker-pool/runs/pool-bench-NOTES.md
  modified:
    - scripts/docker-linux-test/run.sh
    - crates/slicefs-cli/src/mount.rs
    - crates/chunk-store/src/lock_stats.rs
    - crates/metadata/src/store.rs
    - crates/slicefs-cli/tests/lock_stats_report.rs

decisions:
  - "The metadata phase runs in its own mount (M) in every run, so the lock rows of the lock-stats runs belong to that phase only"
  - "The random-write phase ends with an fsync before the close, like the write phase (conv=fsync), and a 4th mount (V) checks its content"

metrics:
  duration: "about 50 min"
  completed: 2026-11-24
---

# Phase 21 Plan 08: POOL-05 bench Summary

The Linux Docker bench with 16 clients on different files gives **verdict=PASS**. The median read throughput at 4 workers is 2.69 times the value at 1 worker. The write phase does not scale (1.26 times). The store lock waits are consistent with this, but the cause is not isolated. The daemon VmHWM stays at or under 178 MiB at 1 to 8 workers (**mem_bar=pass**), and it reaches 223 MiB at 16 workers. All 22 runs have 0 content mismatches.

## Tasks

| Task | Name | Commit | Files |
| ---- | ---- | ------ | ----- |
| 1 | Linux bench script, run.sh mode, lock rows at unmount; macOS script and workflow | 7853542 (code), 3af763e (quick log) | pool-bench.sh (2), run.sh, mount.rs, lock_stats.rs, store.rs, lock_stats_report.rs, pool-bench-macos.yml, runs/pool-bench-quick.log |
| 2 | The Linux bench runs (16 clients, 1 to 16 workers, 3 rounds) | 76a1974 | runs/pool-bench.log |
| 3 | Notes and verdict; lock-stats runs at 4 and 8 workers | f6149bb | runs/pool-bench-NOTES.md, runs/pool-bench-lockstats.log |

**Task 1 code commit (the last code commit of the phase): `7853542bd6c5a6d3bf3d0e07dac7064aa4181e51`.**

## Result for the owner

The metric is the median aggregate MB/s of 3 runs, of the write phase **or** of the read phase, at `--workers 4` against `--workers 1`, with the default trace setting (off). The read phase passes the bar. The write phase alone does not.

- ratio_write_4_1=1.26
- ratio_read_4_1=2.69
- verdict=PASS
- mem_bar=pass, mem16_max_mib=223

Medians of the default runs, and the largest VmHWM (the larger value of mount A, the write mount, and mount B, the read and random-write mount):

| workers | write MB/s | read MB/s | rand-write MB/s | VmHWM max MiB | create files/s | stat ops/s | read ops/s | unlink ops/s |
|---|---|---|---|---|---|---|---|---|
| 1 | 134.9 | 345.8 | 115.2 | 124 | 5000.0 | 57971.0 | 11527.4 | 27972.0 |
| 2 | 178.8 | 550.8 | 128.3 | 140 | 5839.4 | 61538.5 | 12158.1 | 31496.1 |
| 4 | 169.5 | 931.8 | 145.6 | 152 | 6578.9 | 60606.1 | 16260.2 | 41666.7 |
| 8 | 165.6 | 1304.5 | 151.4 | 178 | 6600.7 | 58823.5 | 16326.5 | 40816.3 |
| 16 | 149.1 | 1190.7 | 131.4 | 223 | 5571.0 | 55555.6 | 16000.0 | 42553.2 |

- **Trace-on cost** (`SLICEFS_FUSE_TRACE=1`, medians, on/off): write 1.02 and 1.08 (no cost); read 0.84 at 1 worker and 0.66 at 4; metadata 0.23 to 0.49. With the lines on, the create rate is the same at 1 and 4 workers (2430 and 2477 files/s), so the stderr lock serializes the workers.
- **Lock sites, metadata phase** (the wait p99 in us):
  - At 4 workers: `store` 49.6, `wal_writer` 37.9 and `inode_map` 29.6. All other sites are under 1.
  - At 8 workers: `store` 161.7, `inode_map` 120.6, `wal_writer` 113.6 and `readers` 97.6.
  - `inode_map` is the only metadata map lock with a real wait. At 8 workers one `stripe` hold of 68 ms was seen once (its p99 wait is 0).
  - In the write phase `store` waits the most (p99 90.9 us at 4 workers and 249.0 us at 8; max 13.5 ms). This is the single pack appender. It is consistent with writes that stop rising after 2 workers, but the bench does not isolate it from fsync or disk limits.
- **Arena run** (16 workers, `MALLOC_ARENA_MAX=2`): 9 to 13 MiB lower VmHWM than the default runs at 16, with similar throughput.
- **Default-cap rule** (recorded only): at 16 workers no phase reaches 1.1 times the 8-worker median (0.84 to 1.04). VmHWM at 16 (223) is under CAP(16) = 480. So the rule is not met, and the cap of 8 stays.
- **Stat step:** about 2200 of the 4000 stats per run reach the daemon as getattr. The kernel attribute cache answers the rest, so the stat rate is partly a kernel rate.

## Verification

- Task 1: `bash -n` passes for both scripts. Ruby YAML gives `branches=["ci-pool/**"]`, `dispatch=true` and `runs_on=macos-26`. `cargo check -p slicefs-cli --features lock-stats` finishes with no error. The public-text gate (added lines since 9157444) gives 0. The quick log has 2 bench lines `n=1|2 round=1 trace=off ... mismatches=0`, 0 mismatch lines and `same-toolchain`.
- Task 2:
  - The default runs: `off=3` at each of 1, 2, 4, 8 and 16 workers.
  - The trace-on pair: 6 runs. The arena run: 1.
  - 0 mismatch lines, `same-toolchain`, 22 `bench n=` lines.
- Task 3:
  - The awk ratios (on the trace=off rows) equal the notes: 1.26 and 2.69, `verdict=PASS`.
  - The awk memory lines equal the notes: `mem_bar=pass` and `mem16_max_mib=223`.
  - The notes have 0 home paths and 0 legal-terms words.
  - The lock-stats log is present.
- `cargo test --workspace` on macOS exited 0 before the code commit (1527 passed, 0 failed, 12 ignored; 22 min). With `--features lock-stats`, these tests pass on the committed code: `lock_stats_report` (with the 5 new labels asserted), `lock_wait_rows`, and the chunk-store and metadata tests (498 passed, 0 failed).

## Orchestrator after this plan

1. The verdict is PASS and mem_bar is pass, so no follow-up issue is needed for POOL-05. Optional, owner report only: the write phase does not scale (1.26 times; the store lock waits are consistent with this), and the default-cap rule is not met at 16 workers.
2. Do this step before step 3. The macOS script has never run, so a fix to it would be a new code commit, and then 7853542 would no longer be the last code commit. Push the current commit to a branch named `ci-pool/<name>` (for example `ci-pool/21-08`). This starts `pool-bench-macos.yml` (FSKit, 1 and 2 workers, 3 runs each). Save its log (artifact `pool-bench-macos`, file `pool-bench.log`) as `.planning/phases/21-worker-pool/runs/pool-bench-macos.log` for plan 21-09. The workflow is not on the default branch, so dispatch works only after a merge. The push trigger works now. The script prints `run dir: <path>`, which is a home path on the runner: scrub it (and any other home path) before you save the log. Both scripts have mode 100755 in git (`git ls-files -s`).
3. The Task 1 code commit `7853542bd6c5a6d3bf3d0e07dac7064aa4181e51` is the last code commit of the phase. Plan 21-09 changes only docs, unless its gate finds a bug. Push it and run CI for that sha, the same as the 21-07 steps:
   - the ci.yml Linux job, with `Workers: 4 (set)` in its three steps;
   - the ci.yml macOS job;
   - macos-mounts.yml on a `ci/<name>` branch, with `Workers: 1 (default)`.

   Then, before plan 21-09 Task 2, write `.planning/phases/21-worker-pool/runs/ci-evidence.md` with the line `sha = 7853542bd6c5a6d3bf3d0e07dac7064aa4181e51`, the run IDs, the conclusions and the Workers lines found. The later commits of this plan (3af763e, 76a1974, f6149bb) change only `.planning/`. ci.yml may run on the branch head, and macos-mounts.yml ignores `.planning/**`. The 21-09 check reads `git log -1 -- crates vendor scripts .github Cargo.toml Cargo.lock`, which gives 7853542.

## Deviations from Plan

**1. [Amendment 2] Trace labels in the verify commands.** The plan's verify greps and its ratio awk select ` trace=on ` for the default runs. They were written before Amendment 2. The default runs are now `trace=off`, and the pair is `trace=on`. I ran the checks with on and off swapped. The literal awk on the trace-on rows gives write 1.32 and read 2.10, which is also a PASS. The notes state this.

**2. [Rule 2] Metadata phase per the owner request, in its own mount.** Task 1 text says "create 250 files and close them". The owner request adds stat, read and unlink. The bench line has `meta_fps` (create) plus `meta_stat_ops`, `meta_read_ops` and `meta_unlink_ops`. The phase runs in its own mount M, after mount B, so the lock rows of a lock-stats build cover only this phase. VmHWM of M (`vmhwm_m_mib`) is printed too. It is at most 45 MiB, so the memory bar on A and B is not changed by it.

**3. [Rule 2] Content checks beyond `cmp`.** The random writes come from a seeded RNG. A 4th mount (V, cold cache) replays the seed and checks every block (`randcheck_mbs`, `vmhwm_v_mib`). The metadata read step checks the content of each 4 KiB file. A mismatch in either check stops the script, like a `cmp` mismatch. All runs passed.

**4. Random writes end with an fsync.** The random-write processes call fsync before the close, like the write phase (`conv=fsync`). So the rate includes durability, and the 2 write rates can be compared.

**5. [Rule 2, owner request] New lock-stats sites.** The 6 map locks of `DictMetadataStore` were not sites: `inode_map`, `inode_data`, `dir_data`, `manifest_data`, `xattr_data` and `refcounts`. They are now `.timed(Site::...)` in both constructors. The 6 new `Site` variants come after the old ones, and the test `lock_stats_report` asserts 5 of them (`xattr_data` has samples too, but this test does not assert it). This changes `crates/chunk-store/src/lock_stats.rs`, `crates/metadata/src/store.rs` and `crates/slicefs-cli/tests/lock_stats_report.rs`, which are not in `files_modified`. In a normal build `.timed` only stores the site in the lock metadata. Nothing is timed.

**6. Lock-stats runs at 4 and 8 always ran (owner request).** The plan has them only on a MISS. I ran them in a separate container after the full set, with the lock-stats build in `/target/lockstats`. They are in `runs/pool-bench-lockstats.log`, and their throughput is not in the tables.

**7. Store volume.** The bench store and the source files are on the Docker volume `slicefs-pool-bench` (`POOL_BENCH_DIR=/pb`). With `--rm`, this is the only way to keep a store after a mismatch. The volume holds no data after a run that passes. The mount point is under `/tmp` in the container.

**8. Pre-check on the working tree.** Before the code commit, one container ran the quick set on the working tree, plus `POOL_BENCH_EXTRA=1` (a trace-on run and an arena run) and a lock-stats run. This proved every path of the script. After it, one change was made: the request counts are printed only for trace-on runs. That log is not committed. The committed quick log comes from the committed clean tree.

**9. Two system notes.** `load` in the bench lines is the load of the OrbStack VM, from `/proc/loadavg` in the container (0.00 to 7.06). The host macOS load (15.7 to 18.0) is in the log header. The full set ran on a clean tree at 3af763e (the same code as 7853542, plus the quick log).

## Threat model

- T-21-SC: no new crate.
- T-21-22: the bar and the metric are in the plan, and they did not change. The awk computes the ratios and the memory lines again from the log, and the values equal the notes.
- T-21-23: every run checks the content with `cmp`, the random-write replay and the 4 KiB read check. There were 0 mismatches.
- T-21-24: the order changes over 3 rounds, and every run line has its load. The tree came by tar stream from the committed tree. The toolchain is the same as in the 20-01 notes.

## Known Stubs

None.

## Self-Check: PASSED

- FOUND: scripts/docker-linux-test/pool-bench.sh, scripts/macos-mount-test/pool-bench.sh, .github/workflows/pool-bench-macos.yml, runs/pool-bench-quick.log, runs/pool-bench.log, runs/pool-bench-lockstats.log, runs/pool-bench-NOTES.md
- FOUND: commits 7853542, 3af763e, 76a1974, f6149bb in HEAD
- commits: 4 = `git rev-list --count ea76303..f6149bb` (the docs commit of this SUMMARY comes after)
