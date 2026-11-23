---
phase: 21-worker-pool
plan: 01
subsystem: concurrency
status: complete
tags: [pool-01, pool-02, tracer, worker-pool, workers-option, drain]

requires:
  - phase: 20-dirty-budget-and-rebase-flusher
    provides: "shutdown() (flusher stop, then the gate, then test_destroy), throttle_write rank assert, the 20-01 toolchain line"
provides:
  - "crates/slicefs-cli/src/pool.rs: Pool (queue bounded in slots and bytes, workers slicefs-worker-<k>, submit, stats, drain_and_join, Drop), PoolStats with the `pool: workers=<n> jobs=` line, default_workers, workers_for_cpus, SUBMIT_WAIT, DRAIN_WAIT, QUEUE_BYTES, DEFAULT_WORKERS_MAX"
  - "slicefs mount --workers N (at least 1, no upper bound) and the `Workers: N (set|default)` line"
  - "MountedFs { fs, pool } with MountedFs::new, run (inline or pool) and the free function serve (gate only inside it, debug assert of an empty rank stack after it)"
  - "getattr through run and serve; destroy drains and joins before shutdown()"
  - "SLICEFS_WORKERS in run.sh and mt-linux.sh, with the workers-line and pool-line checks"
affects: [21-02, 21-03, 21-05, 21-07, 21-08, 21-09]

estimate:
  tokens: 110000
  tasks: 3
actuals:
  tokens: 8700    # chars/4 over the added lines of the diff (code, tests, scripts, smoke log)
  tasks: 3
  commits: 4

plan_head_before: f4d71eec3b061ce6e6b39f8e137137967a04c19d
plan_head_after: 4c70d3908ba9d2a3d333d532d604a27b008aa33c
commits: 4

tech-stack:
  added: []
  patterns:
    - "One `run` helper: no pool, the job runs on the caller; else Pool::submit with a boxed closure"
    - "One `serve` helper owns the gate hold; the reply is sent after it returns"
    - "Tests written after the code get their red runs from guard mutations (planted, not committed, reverted by a saved copy)"

key-files:
  created:
    - crates/slicefs-cli/src/pool.rs
    - crates/slicefs-cli/tests/pool_tests.rs
    - .planning/phases/21-worker-pool/runs/smoke-21-01.log
  modified:
    - crates/slicefs-cli/src/cli.rs
    - crates/slicefs-cli/src/lib.rs
    - crates/slicefs-cli/src/fuse_callbacks.rs
    - crates/slicefs-cli/src/mount.rs
    - crates/slicefs-cli/src/main.rs
    - scripts/docker-linux-test/run.sh
    - scripts/docker-linux-test/mt-linux.sh

decisions:
  - "Queue: max(2 x N, 4) slots and 8 MiB of payload; an empty queue admits one job of any weight"
  - "A worker counts its exit with a drop guard, so a worker that ends by a panic does not make the drain wait to its limit (plan 21-03 adds the abort)"
  - "drain_and_join detaches the workers when it returns Err, so the drop of the pool does not wait a second time"
  - "The queue wait counter starts at the call of submit (it includes the time that submit was blocked)"
  - "A pool spawn error becomes the mount error and goes through the stop steps after mount(), not a bare return"

metrics:
  duration: "about 70 min"
  completed: 2026-11-23
---

# Phase 21 Plan 01: Worker pool tracer Summary

`slicefs mount --workers N` now exists. With N of 2 or more, the mount builds a pool of N threads named `slicefs-worker-<k>`. The pool has a queue that is bounded in slots and in payload bytes. getattr is the first request that goes through the pool. The worker takes the commit gate only inside `serve` and sends the reply after `serve` returns. `destroy` drains the queue and joins the workers before `shutdown()`. With 1 worker there is no pool, and getattr runs on the reader thread. A Docker Linux smoke mount with `--workers 4` passed.

## What was built

- **Option.** `--workers N` in `Cmd::Mount`. The clap range parser refuses 0, and there is no upper bound. With no flag, the count is `clamp(available_parallelism / 2, 1, 8)` on Linux and 1 on macOS. `run_mount` reads the count once, prints `Workers: N (set)` or `Workers: N (default)`, and builds `Pool::new(n)` only for n >= 2.
- **Pool.** A std `Mutex` and three condvars (not empty, not full, done). The lock is held only to move a job in or out. `submit` asserts an empty rank stack in debug builds. It waits in 1 s slices, prints one warning after 10 s, and aborts after `SUBMIT_WAIT` (150 s). A worker sets `require_gate(true)` and runs jobs until the queue is closed and empty. `drain_and_join(limit)` closes the queue and runs every queued job (no job is dropped). Then it joins the workers, or returns `Err(busy)` at the limit. `Drop` is the safety net for a mount that fails before the session. The counters are atomics: queue depth now and the largest depth, jobs, jobs per worker, the largest weight, and the longest queue wait.
- **Callbacks.** `MountedFs { fs, pool }` keeps `Deref`, so the 30 callbacks that are not converted compile with no change. getattr copies `ino` and `fh`, clones the `Arc`, and calls `self.run(0, ...)`. In `destroy`, the drain comes first, then one `[ts][FUSE] destroy: pool: ...` line, then `shutdown()`. At the drain limit, the daemon prints one line and aborts (the write log replays at the next mount).
- **Scripts.** `run.sh` passes `-e SLICEFS_WORKERS`. `mt-linux.sh` mounts with `--workers $SLICEFS_WORKERS` when the variable is set. After the daemon-exit check, it prints `workers-line:` and `pool-line:`. It fails when the Workers line differs, and when N >= 2 but no pool line shows jobs above 0.

## Docker smoke (Task 1, tracer gate)

`runs/smoke-21-01.log`: a tar stream of the committed clean tree at f4856c3, the mount form, volume `slicefs-target-p21`, `SLICEFS_WORKERS=4`, and a debug build.

- `rustc 1.100.0 (0ef1bc386 2026-11-09)`: the same as the `RUSTC = ...` line of the 20-01 notes (`same-toolchain`).
- All 11 steps `ok`, `daemon-exit: ok`, `workers-line: Workers: 4 (set)`.
- `pool-line: pool: workers=4 jobs=5 max_weight=0 max_queue_wait_ms=0 queue_max=1 jobs_per_worker=1,2,1,1`.
- 0 lines with `: FAIL`. The log has no user or home path.

jobs=5 is low because only getattr goes through the pool in this plan, and the kernel caches attributes (TTL 1 s). lookup replies also carry attributes.

## Red runs (guard mutations, not committed)

Each mutation was planted, run with `timeout 600` or `timeout 900`, and reverted from a saved copy. `git diff` was clean after each revert.

| # | Mutation | Test that failed | Failing assert |
|---|----------|------------------|----------------|
| 1 | `submit` ignores the byte bound (`... && true`) | `submit_blocks_at_the_byte_bound` | `a submit over the byte bound blocks` — left `Ok(())`, right `Err(Timeout)` |
| 2 | `drain_and_join` returns `Ok(self.stats())` at once | `drain_runs_every_queued_job` (also `stats_count_...`, `drain_limit_...`) | `the drain returned after all 6 jobs` — left 0, right 6 |
| 3 | the drain clears the queue at close | `drain_runs_every_queued_job` (also `stats_count_...`) | `the drain returned after all 6 jobs` — left 1, right 6 |
| 4 | the worker does not count its jobs per worker | `stats_count_the_queue_and_the_jobs_per_worker` | sum of `jobs_per_worker` — left 0, right 5 |
| 5 | `workers_for_cpus` clamps to 16 | `workers_for_cpus_halves_and_clamps` | `32 CPUs` — left 16, right 8 |
| 6 | `destroy` drains after `shutdown()` | `destroy_drains_before_shutdown` | `the job ran once, before the shutdown` — left `[(true, false)]`, right `[(false, true)]` |

## Green runs

- `cargo test --workspace` exited 0 before each code commit: f4856c3 (Task 1), 3aaff2f (Task 2) and 4c70d39 (Task 3). Each run had 70 `test result: ok` lines and no `FAILED`.
- `pool_tests`: 9 passed (also 5 more runs at load about 17, all 9 passed each time, about 0.2 s).
- Task 3 filter: 8 passed (`workers_*` x4, `workers_for_cpus_halves_and_clamps`, `default_workers_is_one_on_macos`, `run_without_a_pool_runs_on_the_caller`, `destroy_drains_before_shutdown`).
- VALIDATION full-suite extras: `cargo check --workspace --all-targets --features slicefs-cli/lock-stats` exit 0; `cargo check -p slicefs-cli --all-targets --features deadlock-detect` exit 0; `--features deadlock-detect` `flusher_tests` 26 passed, `pool_tests` 9 passed, `stress_harness serialize_off_target` 1 passed.
- No clippy note in the new lines (the notes at cli.rs 311 and mount.rs 60 and 304 were there before this plan).

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 2 - Correctness] A worker counts its exit also when its job panics**
- **Found during:** Task 1
- **Issue:** If a job panicked, the worker thread would end with no exit count, and the drain would wait until its 150 s limit and then abort.
- **Fix:** A drop guard (`Exit`) adds 1 to the exit count and wakes the drain. It does this on a normal end and on an unwind. Plan 21-03 adds the abort hook.
- **Files modified:** crates/slicefs-cli/src/pool.rs
- **Commit:** f4856c3

**2. [Rule 1 - Bug] No second wait in `Drop` after a drain limit**
- **Found during:** Task 1
- **Issue:** `drain_and_join` takes `self`. On `Err`, the pool is dropped at the return, and `Drop` would wait `DRAIN_WAIT` again.
- **Fix:** On `Err`, the handles are detached before the return, so `Drop` sees no worker and returns.
- **Files modified:** crates/slicefs-cli/src/pool.rs
- **Commit:** f4856c3

**3. [Rule 2 - Input check] `run_mount` refuses `workers == Some(0)`**
- **Found during:** Task 1
- **Issue:** clap refuses 0, but `run_mount` is a public function.
- **Fix:** It returns the error `--workers must be at least 1` before the Workers line.
- **Files modified:** crates/slicefs-cli/src/mount.rs
- **Commit:** f4856c3

**4. [Rule 3 - Stop steps] A pool spawn error goes through the stop steps**
- **Found during:** Task 1
- **Issue:** A bare `?` before `mount(...)` would skip the flusher stop and the GC joins after it.
- **Fix:** The spawn error becomes the value of `mounted`, so the existing stop steps and `mounted?` handle it.
- **Files modified:** crates/slicefs-cli/src/mount.rs
- **Commit:** f4856c3

### Other notes

- The idle wait of a worker on "not empty" has no time limit. It ends when a job comes or at the close (unmount, or the drop of the pool). This agrees with [Waits] ("ends at unmount").
- The smoke log has one extra first line from the host (tree sha and run form). Inside the container, the tar has no `.git`, so the container header line says `at tree`.
- POOL-01 and POOL-02 are not marked complete: the plan does not say to mark them.

## Known Stubs

None. The other callbacks stay inline on the reader in this plan by design; plan 21-02 moves them.

## Threat Flags

None. No new surface outside the plan's threat model.

## Open points for a decision

- **Thread name length on Linux.** `slicefs-worker-<k>` has 16 or more bytes. The Linux thread name (comm) holds 15 bytes, so `top` and `ps` show `slicefs-worker-` for all workers. `std::thread::current().name()` keeps the full name, so the name test of the 21-03 abort hook works. A shorter name (for example `slicefs-w<k>`) would show the index in `top`, but the plan and the research fix the name `slicefs-worker-<k>`. This choice can be changed later.

## Self-Check: PASSED

- FOUND: crates/slicefs-cli/src/pool.rs
- FOUND: crates/slicefs-cli/tests/pool_tests.rs
- FOUND: .planning/phases/21-worker-pool/runs/smoke-21-01.log
- FOUND: f4856c3, 8bd1697, 3aaff2f, 4c70d39 (ancestors of HEAD)
