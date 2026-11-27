---
phase: 18-spikes-baseline-and-stress-harness
plan: 06
subsystem: testing
tags: [spike-s3, spike-s5, spike-s6, spike-s7, stress-harness, lock-stats, docker]
requires: ["18-02", "18-03", "18-05"]
provides:
  - "crates/slicefs-cli/tests/stress_harness.rs: s3_store_lock, s5_gate_fairness, s6_small_fsync, s7_budget_baseline (all ignored)"
  - "scripts/docker-linux-test/run.sh harness mode: one ignored harness test in release in Docker, no mount, HARNESS_FEATURES passed to cargo"
  - "runs/s3-store-lock-{macos,linux}.log, runs/s5-gate-{macos,linux}.log, runs/s6-inproc-macos.log, runs/s7-budget-linux.log"
affects: [18-07, 19, 20, 21]
tech-stack:
  added: []
  patterns:
    - "One spike test per process by exact name; lock_stats::reset() before measuring"
    - "Throughput from a feature-off pass, lock rows from a feature-on pass"
    - "File bytes never held: expected content is a function of (seed, file, block, version) or a stamped base buffer"
key-files:
  created:
    - .planning/phases/18-spikes-baseline-and-stress-harness/runs/s3-store-lock-macos.log
    - .planning/phases/18-spikes-baseline-and-stress-harness/runs/s3-store-lock-linux.log
    - .planning/phases/18-spikes-baseline-and-stress-harness/runs/s5-gate-macos.log
    - .planning/phases/18-spikes-baseline-and-stress-harness/runs/s5-gate-linux.log
    - .planning/phases/18-spikes-baseline-and-stress-harness/runs/s6-inproc-macos.log
    - .planning/phases/18-spikes-baseline-and-stress-harness/runs/s7-budget-linux.log
  modified:
    - crates/slicefs-cli/tests/stress_harness.rs
    - scripts/docker-linux-test/run.sh
key-decisions:
  - "S3 MB/s is bytes of a phase over the longest time any thread spent inside the wrapped calls; the buffer fill and the compare are not counted. MB means MiB."
  - "S7 runs a commit thread at the daemon default of 30 s (S7_COMMIT_SECS), because the plan did not say and the daemon commits."
  - "The S7 cap is reported (within_cap) and not asserted; the data check is asserted."
requirements-completed: [SPIKE-02]
duration: ~75 min
completed: 2026-11-21
status: complete
actuals:
  tokens: 21000
  tasks: 3
  commits: 5
plan_head_before: 8f288fe071212c8b096b116629e87c2227098906
plan_head_after: 6f4290292dc7e2d97a9e439776936b0f44c05509
commits: 5
---

# Phase 18 Plan 06: S3, S5, S6 and S7 in-process spikes Summary

Four ignored in-process spike tests were added to the stress harness and run one at a time on the platforms the spikes need; the six raw logs are saved for Plan 07. The suite is unchanged: 3 passed, 6 ignored.

## Tasks

| Task | Name | Commit |
|------|------|--------|
| 1 (tracer) | S3 test, run.sh `harness` mode, S3 logs on macOS and Linux | d24da49 (code, all four tests), 0fc1ae6 (logs) |
| 2 | S5 test run, logs on macOS and Linux | d0ab89d |
| 3 | S6 (macOS, in process) and S7 (Linux) logs | f6dcce6 |
| - | Comment fix so the ignored-test count is 6 | 6f42902 |

## Results (raw, for Plan 07)

Linux ran in Docker (OrbStack, kernel 7.2.12, 32 CPUs, rustc 1.100.0, store on overlayfs). macOS ran in process (rustc 1.95.0-nightly). No mount on either.

### S3: N threads, own file each, serialize off (safe subset, dirty limit 4 MiB per file)

No mismatches on either platform with serialize off, so A2 (the safe subset is safe) held and no `S3_SERIALIZE=1` rerun was needed.

| N | macOS write MB/s | macOS read MB/s | Linux write MB/s | Linux read MB/s |
|---|------------------|-----------------|------------------|-----------------|
| 1 | 79.7 | 689.8 | 117.9 | 854.3 |
| 2 | 102.4 | 786.2 | 160.4 | 918.5 |
| 4 | 110.3 | 766.8 | 132.9 | 319.5 |
| 8 | 112.1 | 783.3 | 120.4 | 762.1 |

Writes do not scale past N = 2 on either platform. The Linux read at N = 4 (319.5) is a one-run dip; the lock pass at N = 4 was not checked for it.

Lock rows at N = 8 (lock-stats pass):

| Site | macOS wait p50 / p99 (us) | Linux wait p50 / p99 (us) |
|------|---------------------------|---------------------------|
| store | 0 / 1831.9 | 0 / 3381.2 |
| wal | 0 / 2601.0 | 0 / 3741.7 |
| readers | 8994.8 / 12746.8 | 8716.3 / 12492.8 |
| open_files | 0 / 0 | 0 / 0 |
| io, wal_writer | 0 / 0 | 0 / 0 |

`readers` is the site that makes threads wait most (p50 about 9 ms at N = 8, hold p50 about 1.3 ms): the read phase queues on it. `store` and `wal` wait only in the tail. `gate_wait` stays below 1 us at p99.

### S5: gate fairness, 78,000 files, commit every 1 s, 4 holders (50, 200, 500 ms), 30 s

| Platform | commits | commit gate wait p50 / max ms | commit hold p50 / max ms | commit total p50 / max ms | new-reader wait max while commit active ms | idle max ms | longest hold ms |
|----------|---------|-------------------------------|--------------------------|---------------------------|--------------------------------------------|-------------|-----------------|
| macOS | 19 | 386.7 / 501.0 | 10.8 / 14.1 | 563.6 / 675.3 | 508.3 | 0.0 | 510.0 |
| Linux | 19 | 450.1 / 484.4 | 9.2 / 146.5 | 524.0 / 713.6 | 597.7 | 0.0 | 505.8 |

A new shared holder waits about as long as the longest hold while a commit waits for the gate (508 ms vs 510 ms on macOS; 598 ms vs 506 ms on Linux). This is the writer-preferring `RwLock` behaviour from the std source; the max of the `commit_active` waits is the figure to use (p50 and p99 of that tag include time after the gate is released). The hold of the cut itself is small (p50 about 10 ms).

### S6 in process on macOS (2,000 files of 4 KiB, timed fsync, no FUSE)

| WAL | Pass | fsyncs/s | fsync p50 ms | p99 ms | max ms |
|-----|------|----------|--------------|--------|--------|
| per-op | 1 | 48.1 | 20.00 | 36.80 | 65.40 |
| flush-on-fsync | 1 | 44.3 | 20.91 | 41.75 | 64.65 |
| periodic | 1 | 45.0 | 20.81 | 41.91 | 115.61 |
| per-op | 2 | 47.1 | 20.25 | 36.96 | 73.33 |
| flush-on-fsync | 2 | 43.1 | 21.02 | 41.71 | 136.84 |
| periodic | 2 | 44.7 | 20.91 | 41.48 | 67.96 |

The three strategies are within about 10% of each other; the fsync cost (about 20 ms) dominates.

### S7 baseline on Linux (serialized; 4 writers on 1 GiB files, 128 MiB of the bigwrite mix each)

`peak_rss_mib=234` against the 256 MiB cap (`within_cap=true`), `slowest_write_ms=347.1`, `rebase_steps=449`, `max_dirty_mib=64` (the dirty budget was reached), RSS at start 42 MiB and after the writes 214 MiB, 3 commits (hold max 171.8 ms). The data check passed. The margin to the cap is 22 MiB. `peak_rss_mib` is the whole test process, so it includes the model (4 MiB) and the check buffers; it is not the same instrument as the daemon VmHWM in the S2 mount runs (112 MiB for one 1 GiB file). Flusher lag and the pause curve need Phase 20.

## Deviations from Plan

**1. [Process] All four tests were committed in the Task 1 commit.** They were written and compiled together, and splitting hunks of one file by task would have added work with no value. The Task 2 and Task 3 commits carry their logs.

**2. [Rule 2 - Missing functionality] S7 has a commit thread.** The plan did not say. The daemon commits every 30 s (this changes what is retained), so `commit_loop` got an interval parameter (`commit_loop_every`; `commit_loop` is unchanged for existing callers) and S7 uses 30 s (`S7_COMMIT_SECS`).

**3. [Rule 1 - Bug] Comment fix.** A comment contained the text of the ignore attribute, so `grep -c` printed 7 instead of 6; reworded (6f42902).

**4. S3 throughput definition.** Plan said "MB/s"; the number is MiB/s over the longest per-thread time inside the wrapped calls, so the buffer fill (a 1 MiB copy plus 256 stamps) is not in it. Stated in the log headers.

## Known Stubs

None.

## Threat Flags

None. No new network endpoint, auth path or schema; the new tests are ignored and read only numeric env vars with explicit ranges (T-18-20).

## Notes for Plan 07

- S3: the lock pass shows `readers` as the waiting site at N = 8; `store` waits only in the tail. MB/s come from the feature-off pass only (T-18-25).
- S5 numbers were taken once per platform; the `RwLock` behaviour depends on the std version (macOS 1.95.0-nightly, Linux 1.100.0).
- S6 on macOS has no mount: it is SliceFS cost plus device sync only.
- S7 is a serialized baseline; do not read it as the Phase 20 result.

## Self-Check: PASSED

- All six logs exist; `/Users/` count is 0 in each; the gate greps print 4, 4, 1, 1 (S3), 1, 1 (S5), 6 (S6), 1 (S7).
- Commits d24da49, 0fc1ae6, d0ab89d, f6dcce6 and 6f42902 are on the branch.
- `cargo test -p slicefs-cli --test stress_harness`: 3 passed, 6 ignored.
