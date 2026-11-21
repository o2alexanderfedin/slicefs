---
phase: 18-spikes-baseline-and-stress-harness
plan: 03
subsystem: testing
tags: [stress-harness, hdrhistogram, namespace-model, commit-thread, watchdog, slicefs-cli]
requires:
  - phase: 18-01
    provides: harness tracer (one-file mode, block-version model, serialize mode, latency report)
provides:
  - "Many-files mode: per-thread namespace model, six op kinds (read, write, fsync, create, rename, unlink) plus open and release"
  - "Commit thread running timed_commit, with commit_gate_wait, commit_hold, commit_total rows"
  - "verify_files and verify_namespace (readdir, lookup inode, size, content against the model)"
  - "model_detects_planted_bug: a write and an unlink behind the model are reported"
  - "max_inflight counter; short runs assert max_inflight == 1"
  - "Watchdog, STRESS_* env config with ranges, ignored long_run and ignored serialize_off_target"
affects: [19, 20, 21]
tech-stack:
  added: []
  patterns:
    - "Run returns an Outcome (context, models, mismatches, report) so a test can plant a bug and re-check"
    - "Watchdog: a thread with recv_timeout; dropping the sender at the end of the run stops it; a timeout prints the seed and aborts"
    - "Namespace ownership: each thread only touches names t<n>-<k> (counter never reused), so the model needs no lock"
key-files:
  created: []
  modified:
    - crates/slicefs-cli/tests/stress_harness.rs
key-decisions:
  - "Op mix (const MIX): create 15, write 25, read 20, fsync 5, rename 10, unlink 10, release 10, open 5 percent; no target means create; 32 live files cap turns create into unlink"
  - "Short-run sizes: one-file 4 threads x 150 ops, many-files 4 threads x 40 ops; fsync and release cost 20-40 ms each in a debug build, so more ops broke the 5 s budget (300/60 ops took 8 s)"
  - "The commit thread commits every 250 ms and once more after the workers join; it does not take the serialize mutex"
  - "A worker panic cannot hang the run: all workers are joined first, then the commit thread is stopped, then the panic is re-raised"
requirements-completed: []
duration: ~45 min
completed: 2026-11-21
status: complete
actuals:
  tokens: 11500
  tasks: 3
  commits: 3
plan_head_before: adf3f9c37f276dbb34ff9b771249ec1dc4571dc3
plan_head_after: 1bdf13923a96637debf82525db6559a6a6377fa8
commits: 3
---

# Phase 18 Plan 03: Full stress harness Summary

The harness now covers one file and many files with all six operation kinds, a commit thread, a model of file content and of the namespace, a planted-bug self-test, an in-flight counter, a watchdog, an ignored env-driven long run and an ignored serialize-off target.

SPIKE-02 stays unchecked in REQUIREMENTS.md: Plan 06 also lists it.

## Tasks

| Task | Name | Commit |
|------|------|--------|
| 1 (tracer) | Many-files mode with a namespace model, all op kinds and a commit thread | 547aa3f |
| 2 | Planted-bug self-test and in-flight counter | 44f65d7 |
| 3 | Watchdog, env config, ignored long run and serialize-off target | 1bdf139 |

## Results

- `cargo test -p slicefs-cli --test stress_harness` runs 3 tests (2 ignored) and finishes in 2.5 to 3.9 s in a warm debug build, also with `--features lock-stats`. The tracer gate passed: `short_` ran 2 tests, with `op=rename` and `op=commit_hold` rows.
- Both short tests pass, assert `max_inflight == 1` and dirty total below `DIRTY_BUDGET / 4`.
- Mutation check (done by hand): with the planted `handle_write` line commented out, `model_detects_planted_bug` fails ("want one mismatch, got []"); with the planted `handle_unlink` line commented out it fails ("no mismatch names t0-10: []"); with both restored it passes. Both mutations were reverted before commit.
- `long_run` (release, `STRESS_SECS=20 STRESS_THREADS=4`, both modes, serialize on) passed in 42.6 s and printed its `STRESS seed=` header. One-file: 8281 ops, `max_inflight=1`, `dirty_total=0`; write p50 92 us, p99 26.8 ms; fsync p50 28 ms; commit_total p50 258 ms, max 895 ms; one `release` took 1.1 s (the first release of the shared file after many writes). Many-files: 6282 ops; create p50 31 us, rename p50 36 us, unlink p50 42 us, fsync p50 30 ms, release p50 0.44 ms (p99 50 ms); commit_hold p50 4.7 ms, commit_total p50 196 ms.
- `serialize_off_target` (debug) fails on today's code, as planned. One-file mode, 4 threads, `max_inflight=4`: the report lists read mismatches on the shared file, for example `MISMATCH op=read block=Some(449..454) bytes match no version in 1..=1` and the same for blocks 514..523 (the bytes read are not any version the model allows; the test stops at the first failing mode, so many-files did not run in this test). For Phase 19: these are the shared-handle cases that the take-out pattern breaks (LOCK-01).
- Many-files mode with serialize off (`STRESS_MODE=many-files STRESS_SERIALIZE=0 STRESS_SECS=5 STRESS_THREADS=4`, debug build) passed once with `max_inflight=4` (one run only; each file has one owner thread, so this does not cover shared handles).

## Deviations from Plan

None - plan executed as written. Notes:
- The run-outcome refactor (Ctx owns the file system and temp dir; `run` returns an `Outcome`) was written in Task 1 instead of Task 2, because the many-files verify needed it; the in-flight counter and the planted-bug test are in Task 2 as planned.
- The first calibration (300 and 60 ops per thread) took 8 to 14 s; it was cut to 150 and 40 to meet the 5 s budget.
- The pin guard was run in a short form (same git-vs-git root comparison) before each write and commit.

## Known Stubs

None.

## Threat Flags

None. The harness only reads STRESS_* numbers (each with a range, panic names the variable) and writes into a temp dir (T-18-07, T-18-08, T-18-09: the model holds versions only).

## Self-Check: PASSED

- FOUND: crates/slicefs-cli/tests/stress_harness.rs (contains `fn short_many_files`, `fn model_detects_planted_bug`, `fn watchdog`, `process::abort`, 2 `#[ignore`)
- FOUND commits: 547aa3f, 44f65d7, 1bdf139
