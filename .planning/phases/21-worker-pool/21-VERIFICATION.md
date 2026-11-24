---
phase: 21-worker-pool
verified: 2026-11-24T17:08:40Z
status: passed
score: 5/5 must-haves verified
covered_files: [".github/workflows/ci.yml", ".github/workflows/pool-bench-macos.yml", ".planning/phases/21-worker-pool/21-01-PLAN.md", ".planning/phases/21-worker-pool/21-01-SUMMARY.md", ".planning/phases/21-worker-pool/21-02-PLAN.md", ".planning/phases/21-worker-pool/21-02-SUMMARY.md", ".planning/phases/21-worker-pool/21-03-PLAN.md", ".planning/phases/21-worker-pool/21-03-SUMMARY.md", ".planning/phases/21-worker-pool/21-04-PLAN.md", ".planning/phases/21-worker-pool/21-04-SUMMARY.md", ".planning/phases/21-worker-pool/21-05-PLAN.md", ".planning/phases/21-worker-pool/21-05-SUMMARY.md", ".planning/phases/21-worker-pool/21-06-PLAN.md", ".planning/phases/21-worker-pool/21-06-SUMMARY.md", ".planning/phases/21-worker-pool/21-07-PLAN.md", ".planning/phases/21-worker-pool/21-07-SUMMARY.md", ".planning/phases/21-worker-pool/21-08-PLAN.md", ".planning/phases/21-worker-pool/21-08-SUMMARY.md", ".planning/phases/21-worker-pool/21-09-PLAN.md", ".planning/phases/21-worker-pool/21-09-SUMMARY.md", "README.md", "crates/chunk-store/src/lib.rs", "crates/chunk-store/src/lock_stats.rs", "crates/chunk-store/tests/retry_tests.rs", "crates/metadata/src/segment/writer.rs", "crates/metadata/src/store.rs", "crates/slicefs-cli/src/alloc_tune.rs", "crates/slicefs-cli/src/cli.rs", "crates/slicefs-cli/src/filesystem.rs", "crates/slicefs-cli/src/flusher.rs", "crates/slicefs-cli/src/fuse_callbacks.rs", "crates/slicefs-cli/src/fuset_watch.rs", "crates/slicefs-cli/src/handlers.rs", "crates/slicefs-cli/src/lib.rs", "crates/slicefs-cli/src/main.rs", "crates/slicefs-cli/src/mount.rs", "crates/slicefs-cli/src/pool.rs", "crates/slicefs-cli/src/snapshot.rs", "crates/slicefs-cli/tests/alloc_tune_tests.rs", "crates/slicefs-cli/tests/common/mod.rs", "crates/slicefs-cli/tests/coverage_boost_tests.rs", "crates/slicefs-cli/tests/e2e_filesystem_tests.rs", "crates/slicefs-cli/tests/flusher_tests.rs", "crates/slicefs-cli/tests/fsync_tests.rs", "crates/slicefs-cli/tests/lock_stats_report.rs", "crates/slicefs-cli/tests/pool_panic_tests.rs", "crates/slicefs-cli/tests/pool_tests.rs", "crates/slicefs-cli/tests/reply_after_locks.rs", "crates/slicefs-cli/tests/store_busy_tests.rs", "crates/slicefs-cli/tests/streaming_tests.rs", "crates/slicefs-cli/tests/stress_harness.rs", "crates/slicefs-cli/tests/write_log_replay_tests.rs", "crates/slicefs-cli/tests/write_path_tests.rs", "docs/design/chunk-store.md", "docs/design/write-log.md", "docs/research/multithreaded-request-handling.md", "scripts/docker-linux-test/dedup-probe.sh", "scripts/docker-linux-test/mt-linux.sh", "scripts/docker-linux-test/panic-replay.sh", "scripts/docker-linux-test/pjdfstest.sh", "scripts/docker-linux-test/pool-bench.sh", "scripts/docker-linux-test/run.sh", "scripts/docker-linux-test/s4-waiting.sh", "scripts/docker-linux-test/verify-phases.sh", "scripts/macos-mount-test/boost-test.sh", "scripts/macos-mount-test/large-copy.sh", "scripts/macos-mount-test/pool-bench.sh"]
covered_digest: "v4:sha256:4ce4224cb9e7f7d732346af4a0ecc4fdf6405f21ee1c76fbd504e987eb7131d3"
behavior_unverified: 0
overrides_applied: 0
human_verification: []
---

# Phase 21: Worker pool - Verification Report

**Phase Goal:** FUSE requests run on a pool of worker threads, with the number of workers set by a mount option, on Linux and on macOS FSKit (#22)
**Verified:** 2026-11-24
**Status:** passed
**Re-verification:** No (initial verification)

## Evidence base and its trees

The last code commit is `7853542bd6c5a6d3bf3d0e07dac7064aa4181e51`. The command `git log -1 --format=%H -- crates vendor scripts .github Cargo.toml Cargo.lock` returns this SHA. `git diff 7853542 HEAD` outside `.planning/` changes only `README.md` and `docs/research/multithreaded-request-handling.md`.

| Evidence | Tree | Weight |
|---|---|---|
| CI run 45421387746 (ci.yml, PR #173): Linux Docker job and macOS job | fdceef0. This has the same code as 7853542; the diff touches only `.planning/`. | Primary (final code) |
| CI run 45421361354 (macos-mounts.yml, FSKit) | 7853542 | Primary (final code) |
| CI run 45421265175 (pool-bench-macos.yml) | 7853542 | Primary (final code) |
| `runs/gate-21-09.log` (mt-linux + verify-phases, 4 workers) | 15b32f1. The code is the same as 7853542. | Primary (final code) |
| `runs/pool-bench.log` and NOTES | Code of 7853542 (a tar of 3af763e) | Primary (final code) |
| `runs/docker-{all,verify,pjdfstest}-w4.log` | 1ca0f2c (21-07, before #170 and 21-08) | Supporting only |
| `runs/s7-step-limit.log` and NOTES | 839713b (21-05 allocator setting and the #170 fix) | Supporting only. These are hand runs and not CI. |

The CI conclusions were read with `gh run view` (read-only): all three runs are `success`. In the Linux job log, the mount lines are 30 `workers-line:` lines, and all 30 say `Workers: 4 (set)`. The other 3 matches are the echoed `grep -q` check of each step. The test totals are Linux 1569 passed, 0 failed, and macOS 1527 passed, 0 failed. The named POOL test lines (for example `a_worker_panic_aborts_and_the_remount_replays ... ok`) are in the macOS job log only, because the Linux job prints only the summary lines.

## Goal Achievement

### Observable Truths (ROADMAP Success Criteria)

| # | Truth | Status | Evidence |
|---|---|---|---|
| 1 | A mount option sets the number of workers. With 1 worker the handler runs inline as today. The macOS default stays 1 unless SPIKE-03 shows that requests overlap. | VERIFIED | `cli.rs:128-129`: `--workers` with `RangedU64ValueParser::range(1..)`. 0 is refused and there is no upper bound. The tests `workers_flag_parses`, `workers_zero_is_refused` and `workers_above_eight_is_accepted` pass in CI. `main.rs:33` → `run_mount(..., workers)`. `mount.rs:303-310` prints `Workers: N (set\|default)`. `mount.rs:519`: a `Pool` is made only for `n_workers >= 2`. In `MountedFs::run` (`fuse_callbacks.rs`), `None => job()` runs the job on the reader thread. The test `run_without_a_pool_runs_on_the_caller` passes. `pool::default_workers()` returns 1 on macOS, else `clamp(cpus/2, 1, 8)`. The tests `default_workers_is_one_on_macos` and `workers_for_cpus_halves_and_clamps` pass. The FSKit CI log shows `Workers: 1 (default)` 4 times. **SPIKE-03 reading:** S1 did show overlap on FUSE-T 1.2.9 (2 requests in flight, 5 replies out of order). The macOS default of 1 still complies, for three reasons. (a) The criterion permits a raise; it does not require one. (b) The S1 "Decides" line in the research doc and the 21-CONTEXT owner decision both tie a raise to a measured FSKit throughput gain. (c) The macOS bench (`ci-evidence.md`, CI 45421265175) shows no gain: the write median is 25.2 MB/s at 1 worker and 37.3 at 2, and the read median is 265.6 at 1 worker and 165.7 at 2, on a noisy 3-CPU runner. |
| 2 | Unmount and SIGTERM drain the queue and join the workers before `destroy()`, and replies are sent after all locks are released. | VERIFIED | **Reading:** the drain is the first step inside the `destroy` callback, before `shutdown()` (the Phase 20 flusher stop, then the gate). The vendored fuser calls `destroy()` only after all reader threads have joined (`vendor/fuser/src/session.rs:330`). The `destroy` code in `fuse_callbacks.rs` runs `pool.drain_and_join(DRAIN_WAIT)` first and aborts at the limit; then comes `self.shutdown()`. The test `destroy_drains_before_shutdown` checks that a queued job runs before the shutdown with the flusher still running; it passes in CI. The tests `drain_runs_every_queued_job` and `drain_limit_reports_a_stuck_worker` are in `pool_tests.rs`. SIGTERM, SIGINT and SIGHUP make `signals::install` do a soft unmount, then the session ends and `destroy` runs. With 4 workers, `gate-21-09.log` has `SIGTERM-exits/no-mount-lock/write-kept: ok` (and the same for SIGINT and SIGHUP). The drained `pool: workers=4 jobs=163 ... jobs_per_worker=38,44,39,42` line comes from destroy. **Replies after locks:** `serve()` is the only gate site apart from `init` (the test `the_gate_is_taken_only_in_serve_and_init`). All 31 request callbacks go through `serve` (the test `every_request_callback_uses_serve`; `write` goes through `write_job`). I checked the 31 `serve(` closures myself: none of them touches `reply`, so every reply is sent after the gate guard drops. A debug assert in `serve` checks that no ranked lock is held at the reply (the test `serve_panics_when_a_lock_is_held_at_the_reply`). `Pool::submit` also asserts that no lock is held (the test `a_submit_with_a_lock_held_panics`). |
| 3 | A worker panic aborts the daemon. The next mount replays the write log, and no request is served after a half-done update. | VERIFIED | `pool::install_abort_hook()` (called in `mount.rs:509` before the mount) is a panic hook. It calls `process::abort()` on threads named `fuser-*` or `slicefs-worker-*`, before the unwind, so the locks stay held and no reply or destroy commit happens. The vendored fuser names its reader threads `fuser-{i}` (`session.rs:294`). Child-process tests in `pool_panic_tests.rs` check SIGABRT, no `REPLY B`, the replayed fsynced file A, and that B is absent, for a worker and for the reader. They also check that a full queue aborts and that a panic on another thread does not abort. All pass in CI. The live check `panic-replay.sh` in `gate-21-09.log` and in the CI Linux job shows `panic-aborts-w4/w1`, `fsynced-file-replayed-w4/w1` and `half-done-entry-absent-w4/w1: ok`, and `RESULT panic-replay: PASS`. |
| 4 | All tests and pjdfstest pass with 4 workers on Linux (Docker CI), and the tests pass on macOS FSKit CI with the macOS default. | VERIFIED | `ci.yml` sets the Linux job env `SLICEFS_WORKERS: "4"`. Each of its 3 steps fails without `Workers: 4 (set)` (the red run is `runs/workers-check-red.log`). In CI 45421387746 (fdceef0, the same code as 7853542), the Linux job is a success: 1569 tests passed and 0 failed, `RESULT verify-phases: PASS`, `RESULT panic-replay: PASS`, pjdfstest `Result: PASS`, and all 30 mount Workers lines are `4 (set)`. The macOS cargo test job is a success with 1527 passed. The FSKit run macos-mounts 45421361354 on 7853542 is a success: `RESULT fskit: 0 failure(s)`, `RESULT large-copy: PASS` and `Workers: 1 (default)`. |
| 5 | Parallel I/O on different files scales with the number of workers on Linux, and the measured result is recorded. | VERIFIED | The bar was set in 21-08-PLAN before the runs (the plan was last changed in 964e13e on 2026-11-23, before the bench commits). The bar: the median of 3 runs of the **write or the read phase** at 4 workers must be at least 1.5x the median at 1 worker, with 0 mismatches. I recomputed this from `runs/pool-bench.log` (22 bench lines, `mismatches=0` in all 22). Default trace=off: `ratio_write_4_1=1.26`, `ratio_read_4_1=2.69`. The plan's literal pre-registered form (trace=on rows) gives write 1.32 and read 2.10. **The pass rests on the read phase. Writes do not scale past 2 workers** (134.9, 178.8, 169.5, 165.6 MB/s at 1, 2, 4 and 8 workers). This is recorded in the notes, the README, the research doc and the deferred list. The memory bar also holds: the largest daemon VmHWM at 1 to 8 workers is 178 MiB, under 320. |

**Score:** 5/5 truths verified (0 present, behavior-unverified)

### Plan must-haves (merged)

The plan truths of 21-01 to 21-09 restate or refine the five criteria above. The ones that add detail were checked:

- 21-01: the thread names are `slicefs-worker-<k>`. The queue is bounded by `max(2n,4)` slots and by `QUEUE_BYTES` 8 MiB. Every pool wait has a time limit of 200 s (`SUBMIT_WAIT`/`DRAIN_WAIT`, after #170), then an abort. The idle worker wait is the exception: it ends at close, which is documented in the deferred list. VERIFIED.
- 21-02: the live checks pass at the container default of 8 workers (`runs/verify-21-02.log`: `Workers: 8 (default)`, `RESULT verify-phases: PASS`; tree ef8c991, supporting only). In the `write` callback, the inline path borrows the payload and the pool path copies it with `data.to_vec()`. The trace default was later flipped by the [Trace default] decision: off by default, and `SLICEFS_FUSE_TRACE=1` turns it on. The code (`trace_on()`) and the README match the later decision. VERIFIED.
- 21-04 and 21-06: the S7 cap is asserted for 1 to 8 writers (`cap_checked=true`). R1=pass: 16 of 16 runs at 1, 2, 4 and 8 writers are within 320 MiB (Pm(8)=290). R2 and R3 (16 writers, 412 to 426 MiB) were recorded and do not block. VERIFIED. The `verify.artifacts` tool fails one 21-06 artifact: the frontmatter still asks for `save_steps_max=` in `runs/s7-step-limit.log`. The step limit was not built. The owner replaced it with the 21-05 allocator setting (21-CONTEXT [Allocator], 2026-11-23). The in-plan Amendment of 21-06 (2026-11-24) says that it wins on a conflict, drops `save_steps_max=`, and puts the check `grep -c mmap_threshold=131072` >= 19 in its place. I ran that check: 19 of 19 S7 lines. So the artifact passes the amended plan, and the tool failure comes from stale frontmatter. This is not counted as an override (see "Owner confirmation" below).
- 21-05: the threshold test `tune_at_start_fixes_the_mmap_threshold` exists and is not `#[ignore]` (Linux glibc only, so CI shows it only in the Linux total). `alloc_tune::tune_at_start()` is the first statement of `run_mount` (`mount.rs:298`). The workspace has no other `mallopt`, `mallinfo` or `malloc_*` call (grep). VERIFIED.
- 21-07: the red run prints `workers-line: FAIL` and `red-exit=1` (`runs/workers-check-red.log`). 21-09: the research doc has `## Phase 21 result` (line 369). The added README and research text (diff 7853542..HEAD) has no user or home path and no legal word (grep count 0).
- 21-07, 21-08 and 21-09: the CI wiring, the bench, the macOS bench workflow, the README `--workers` section (`README.md:121-130`), the research result and the deferred list are all present. VERIFIED.

### Required Artifacts

`verify.artifacts` was run on each plan: 21-01 3/3, 21-02 3/3, 21-03 3/3, 21-04 2/2, 21-05 2/2, 21-06 1/2 (the stale `save_steps_max=` pattern; the amended check passes, see above), 21-07 2/2, 21-08 3/3, 21-09 3/3.

| Artifact | Status | Details |
|---|---|---|
| `crates/slicefs-cli/src/pool.rs` | VERIFIED | Real bounded queue, workers, drain/join, abort hook, stats; wired in mount.rs and fuse_callbacks.rs |
| `crates/slicefs-cli/src/fuse_callbacks.rs` | VERIFIED | `MountedFs{fs,pool}`, `run`, `serve`, 31 callbacks dispatched, drain in destroy |
| `crates/slicefs-cli/src/mount.rs` | VERIFIED | worker count, Workers/Allocator lines, abort hook, pool only for n>=2 |
| `crates/slicefs-cli/src/cli.rs` | VERIFIED | `--workers` (1..) |
| `crates/slicefs-cli/src/alloc_tune.rs` | VERIFIED | mallopt at start (Linux glibc), stats; called from run_mount and S7 |
| tests: `pool_tests.rs`, `pool_panic_tests.rs`, `reply_after_locks.rs`, `alloc_tune_tests.rs` | VERIFIED | Run in CI and pass |

### Key Link Verification

`verify.key-links` passes for all 9 plans (16/16). Manual checks:

| From | To | Via | Status |
|---|---|---|---|
| `main.rs` | `run_mount` | the `workers` argument | WIRED |
| `run_mount` | `Pool::new` / `MountedFs::new` | `n_workers >= 2` | WIRED |
| fuser reader | `MountedFs::run` → `Pool::submit` | 30 callbacks through `self.run`, and `write` through `pool.submit` | WIRED |
| fuser session end | `MountedFs::destroy` → `drain_and_join` → `shutdown` | `session.rs:330` | WIRED |
| fuser config | one reader | `Config::default()`, `n_threads` not set (default 1, `session.rs:257`) | WIRED |
| CI Linux job | mount scripts | `SLICEFS_WORKERS` → `--workers` and the Workers-line check | WIRED |

### Behavioral Spot-Checks

Per the instructions, I did not run the cargo suite. The CI logs of the final code and the committed logs are the evidence.

| Behavior | Source | Result | Status |
|---|---|---|---|
| POOL-05 ratios equal the notes | awk on `runs/pool-bench.log` | write 1.26 / read 2.69 (trace=off); 1.32 / 2.10 (trace=on) | PASS |
| CI runs green on the final code | `gh run view` (read-only) | 3 of 3 success | PASS |
| Linux CI uses 4 workers only | grep in the CI log | 30 `workers-line:` lines, all `Workers: 4 (set)` | PASS |
| 21-06 amended check | `grep -c mmap_threshold=131072 runs/s7-step-limit.log` | 19 (19 S7 lines) | PASS |
| The last code commit | `git log -1 -- crates vendor scripts .github Cargo.*` | 7853542 | PASS |

### Probe Execution

Step 7c: the phase does not declare a `scripts/*/tests/probe-*.sh`. Not applicable.

### Requirements Coverage

| Requirement | Plans | Status | Evidence |
|---|---|---|---|
| POOL-01 | 21-01, 21-02 | SATISFIED | Truth 1 and the one-reader config |
| POOL-02 | 21-01, 21-02 | SATISFIED | Truth 2 |
| POOL-03 | 21-03 | SATISFIED | Truth 3 |
| POOL-04 | 21-07, 21-08 | SATISFIED | Truth 4 (CI on the final code) |
| POOL-05 | 21-04, 21-05, 21-06, 21-08 | SATISFIED | Truth 5. Also R1=pass and mem_bar=pass. |

No orphaned requirements: REQUIREMENTS.md maps only POOL-01 to POOL-05 to Phase 21.

### Decision Coverage

`check.decision-coverage-verify` result: skipped. "No trackable decisions in CONTEXT.md." (The CONTEXT has decisions, but not in the tracked format.) I checked the main decisions by hand: [Worker count option], [Pool], [Shutdown], [Panic], [Locks], [Tests], [Bench], [Scale bar], [Allocator], [Trace default], [Metadata bench] and [Stats]. Each one is present in the code or in the logs.

### Test Quality Audit

| Test file | Linked req | Active | Skipped | Circular | Assertion level | Verdict |
|---|---|---|---|---|---|---|
| `pool_tests.rs` | POOL-01, POOL-02 | 9 | 0 | no | Behavioral (parallelism, bounds, drain, stats) | OK |
| `pool_panic_tests.rs` | POOL-03 | 4 | 1 (`panic_child`: the child entry, `#[ignore]` by design and run by the parent) | no | Behavioral (SIGABRT, replay content) | OK |
| `reply_after_locks.rs` | POOL-02 | 2 | 0 | no | Source scan | OK |
| `alloc_tune_tests.rs` | POOL-05 memory ([Allocator]) | 2 (cfg-gated: the threshold test is Linux glibc only) | 0 | no | Value (a 1 MiB block after a 4 MiB free is mmapped) | OK |
| `fuse_callbacks.rs` unit tests | POOL-01, POOL-02 | 3 | 0 | no | Behavioral (ordering at destroy, debug assert) | OK |
| `stress_harness.rs` `s7_budget_baseline` | POOL-05 memory bar | 0 in CI | 1 (`#[ignore]`, a long hand run) | no | Value (VmHWM <= 320) | WARNING: the 320 MiB S7 bar has evidence only from hand runs (`runs/s7-step-limit.log`), not from CI |

Disabled tests that are the only proof of a requirement: 0. Circular tests: 0.

### Anti-Patterns Found

No `TBD`, `FIXME` or `XXX` in any file that the phase changed. No `TODO`, `HACK` or `PLACEHOLDER` in pool.rs, mount.rs, fuse_callbacks.rs, alloc_tune.rs or cli.rs.

| File | Line | Pattern | Severity | Impact |
|---|---|---|---|---|
| `crates/slicefs-cli/src/pool.rs` | 25-31 | Stale doc: `SUBMIT_WAIT` names "the 200 ms step-slot wait", but no step slot exists | Info | The value (200 s) is still above the real sum (about 190.2 s). Already in the deferred list. |
| Phase 19 deferred races | n/a | A closed-file size change racing an unlink or a setattr, and no ancestor check for a cross-directory rename | Warning | Before Phase 21 every mount had one request thread, so these races could not happen. The Linux default is now 2 to 8 workers (1 on a host with fewer than 4 CPUs), so the races are now possible by default. pjdfstest passes with 4 workers. This is in the Phase 19 and 21 deferred lists. Not a Phase 21 criterion, but the owner should weigh it. |
| `fuse_callbacks.rs` getlk/setlk docs | n/a | Doc says "single-threaded" | Info | Deferred list. The lock calls do nothing, so the behavior is correct. |

### Human Verification Required

N/A. This is an internal concurrency phase. Each behavior-dependent truth (the drain order, the abort before the unwind, the reply after the locks, inline at 1 worker) has a passing behavioral test or a live check in CI on the final code.

### Owner confirmation (not an override)

The verifier does not accept overrides on its own. The 21-06 artifact passes on the plan's own Amendment, so no override is needed. If the owner wants the record formal, add this to the frontmatter:

```yaml
overrides:
  - must_have: "21-06 artifact: runs/s7-step-limit.log contains save_steps_max="
    reason: "Step limit replaced by the 21-05 allocator setting (21-CONTEXT [Allocator]); the 21-06 Amendment checks mmap_threshold=131072 (19 of 19); R1=pass"
    accepted_by: "{owner}"
    accepted_at: "{ISO timestamp}"
```

### Gaps Summary

There are no gaps. All five criteria are delivered in the code and backed by CI on the last code commit (7853542). The owner should note the following; none of them blocks:

- POOL-05 passes on reads only (2.69x). Writes reach 1.26x and do not rise after 2 workers. The likely cause is the single pack appender (`store` lock), but the bench does not isolate it.
- The S7 memory bar passes at 1 to 8 writers (Pm(8) = 290 MiB, 30 MiB under the cap). 16 writers reach 412 to 426 MiB. This is above the default cap of 8 and is recorded only.
- The Linux default is now above 1 worker. This makes the Phase 19 deferred race windows possible by default.
- The S7 320 MiB bar has evidence only from hand runs, not from CI.

---

_Verified: 2026-11-24_
_Verifier: Claude (gsd-verifier)_
