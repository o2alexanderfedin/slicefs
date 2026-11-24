---
phase: 21-worker-pool
plan: 04
subsystem: measurement
status: complete
tags: [pool-05, s7, step-limit, before-runs, vmhwm]

requires:
  - phase: 21-worker-pool
    provides: "21-01: pool::DEFAULT_WORKERS_MAX (8)"
  - phase: 20-dirty-budget-and-rebase-flusher
    provides: "s7_budget_baseline flusher form, CAP_MIB 320, the 20-01 toolchain line"
provides:
  - "S7_THREADS (1..=64, default 4) in s7_budget_baseline; the S7 line has threads=<n> and cap_checked=<true|false>"
  - "The VmHWM cap assert of the flusher form applies only when the writer count is at most pool::DEFAULT_WORKERS_MAX"
  - "run.sh harness mode passes -e S7_THREADS"
  - "runs/s7-before-limit.log: 12 S7 flusher-form runs at 1, 2, 8 and 16 writers before the step limit"
affects: [21-05, 21-06, 21-08]

estimate:
  tokens: 50000
  tasks: 2
actuals:
  tokens: 4800    # chars/4 over the diff: stress_harness.rs and run.sh (about 600), the committed log (about 4200)
  tasks: 2
  commits: 2

plan_head_before: 964e13e1ca817763b49d2ef7fcf724c3321339c7
plan_head_after: ef26ea276038ccb256649ae15faa78ee8a3c3ab5
commits: 2

tech-stack:
  added: []
  patterns:
    - "One cap_checked value gates both the printed field and the assert"
    - "Each Docker run writes to a file first; the log gets the output, then exit=<status> at column 0"

key-files:
  created:
    - .planning/phases/21-worker-pool/runs/s7-before-limit.log
  modified:
    - crates/slicefs-cli/tests/stress_harness.rs
    - scripts/docker-linux-test/run.sh

decisions:
  - "cap_checked = flusher form AND writers <= DEFAULT_WORKERS_MAX; it does not depend on VmHWM (macOS has no VmHWM and keeps its own 'cap not checked' line)"
  - "The committed log is the raw log less the cargo warning blocks, the [rebase] lines and the [FUSE] overlay lines; the header gives the counts"

metrics:
  duration: "about 46 min"
  completed: 2026-11-23
---

# Phase 21 Plan 04: S7 writer-count knob and the S7 numbers before the step limit Summary

`s7_budget_baseline` now reads its writer count from `S7_THREADS` and asserts the 320 MiB VmHWM cap only at 1 to 8 writers (`pool::DEFAULT_WORKERS_MAX`). Twelve Docker Linux runs before the step limit show: 1 and 2 writers are well under the cap, 8 writers are above it in all 3 runs (419 to 441 MiB), and 16 writers reach 688 to 730 MiB. Peak memory grows by about 35 MiB for each writer above 2.

## What was built

- **Knob (`stress_harness.rs`).** `const THREADS: usize = 4` is replaced by `let threads = env_num("S7_THREADS", 4, 1, 64)`. `let cap_checked = flusher_on && threads <= slicefs_cli::pool::DEFAULT_WORKERS_MAX;` gates the VmHWM assert and is printed as `cap_checked=` after `flusher=` on the S7 line. `within_cap=` stays on the line at every count. The doc comment and the `#[ignore]` text name `S7_THREADS`. `CAP_MIB` did not change.
- **run.sh.** The `harness)` mode passes `-e S7_THREADS`, and the usage comment names it. `bash -n` is clean.

## Red and green runs (Task 1)

Short local runs (macOS, debug, `S7_FILE_MIB=4 S7_WRITE_MIB=2 S7_COMMIT_SECS=1 S7_FLUSHER=1`, `timeout 900`):

| Tree | S7_THREADS | S7 line | Verify grep |
|------|-----------|---------|-------------|
| HEAD 964e13e (red) | 2 | `S7 threads=4 ...` (knob not read; no `cap_checked`) | 0 matches: fails |
| HEAD 964e13e (red) | 9 | `S7 threads=4 ...` | 0 matches: fails |
| new code (green) | 2 | `S7 threads=2 ... cap_checked=true ...` | match |
| new code (green) | 9 | `S7 threads=9 ... cap_checked=false ...` | match |
| new code (green) | 8 | `S7 threads=8 ... cap_checked=true ...` (the edge of the bar, extra run) | match |

On macOS there is no VmHWM, so the runs with `cap_checked=true` also print `S7 cap not checked: no VmHWM on this system`. All five runs exited 0.

`cargo test --workspace` exited 0 before the code commit 2dc170d (72 `test result: ok` lines, 0 `FAILED`). Acceptance counts: `S7_THREADS` 3 lines in stress_harness.rs and 2 in run.sh, `DEFAULT_WORKERS_MAX` 2, `const THREADS: usize` 0.

## Docker before-runs (Task 2)

`runs/s7-before-limit.log`: one container, harness form (no mount), tar stream of the committed clean tree at 2dc170d, volume `slicefs-target-p21`, release build, default sizes (1024 MiB file and 128 MiB mix per writer, commit every 30 s), `S7_FLUSHER=1`. The `rustc -V` line is `rustc 1.100.0 (0ef1bc386 2026-11-09)`, the same as the `RUSTC = ...` line of the 20-01 notes (`same-toolchain`). `nproc` 32. `df -h /tmp`: 555 GiB free before the runs. Load in the container 4.6 to 7.9 before each run; host load 20 before and 17 after. No host cargo job ran during the container. The verify gave `runs=3` for 1, 2, 8 and 16 writers, 12 `exit=` lines, 0 mismatch lines and `same-toolchain`.

| # | threads | vmhwm_mib | within_cap | exit | throttle_timeouts | throttle_wait_max_ms | flusher_steps | flusher_drops | slowest_write_ms |
|---|---------|-----------|------------|------|-------------------|----------------------|---------------|---------------|------------------|
| 1 | 8 | 419 | false | 101 (cap) | 496 | 274 | 201 | 21 | 665.3 |
| 2 | 16 | 700 | false | 0 | 1595 | 492 | 356 | 48 | 6488.6 |
| 3 | 1 | 97 | true | 0 | 0 | 0 | 0 | 0 | 9386.9 |
| 4 | 2 | 208 | true | 0 | 0 | 200 | 59 | 1 | 497.3 |
| 5 | 8 | 441 | false | 101 (cap) | 783 | 396 | 181 | 27 | 994.1 |
| 6 | 16 | 730 | false | 0 | 2088 | 361 | 362 | 44 | 6538.9 |
| 7 | 1 | 95 | true | 0 | 0 | 0 | 0 | 0 | 734.0 |
| 8 | 2 | 198 | true | 0 | 2 | 201 | 53 | 1 | 135.3 |
| 9 | 8 | 423 | false | 101 (cap) | 361 | 450 | 214 | 16 | 997.2 |
| 10 | 16 | 688 | false | 0 | 1777 | 462 | 382 | 48 | 4601.2 |
| 11 | 1 | 90 | true | 0 | 0 | 0 | 0 | 0 | 9605.0 |
| 12 | 2 | 216 | true | 0 | 5 | 201 | 55 | 4 | 649.1 |

The three 8-writer runs failed on the cap assert after the S7 line (`VmHWM <v> MiB is above the cap 320 MiB`). This is a result, as the plan says; the runs went on. The 16-writer runs exit 0 because the cap is not asserted above 8 writers. No run had a data mismatch.

Per writer count (VmHWM range, median):

| threads | VmHWM MiB | median | source |
|---------|-----------|--------|--------|
| 1 | 90-97 | 95 | this log |
| 2 | 198-216 | 208 | this log |
| 4 | 262-340 | 295 | runs/s7-throttle-200ms.log (7 runs on 9861aa5; the S7 code path has only comment changes since then) |
| 8 | 419-441 | 423 | this log |
| 16 | 688-730 | 700 | this log |

Other values: `rss_after_writes_mib` equals VmHWM at 8 and 16 writers, so the peak is in the write phase. `max_dirty_mib` is 32 at 1 writer and 64 to 66 at 2 or more. One 8-writer run takes about 100 to 120 s, one 16-writer run about 215 to 280 s.

## Trace default

S7 runs in process and calls the handlers directly, with no FUSE session. The per-request FUSE trace lines (now off by default; `SLICEFS_FUSE_TRACE=1` turns them on) do not apply to these runs. The plan text does not assume them.

## Deviations from Plan

### Auto-fixed Issues

None.

### Other notes

- **Filtered log.** The raw log was 1.8 MB (10403 lines): 8312 `[rebase]` debug lines, 1752 lines of cargo warning blocks (the same blocks in each run) and 81 `[FUSE] ... switches to overlay` lines. The committed log (16.7 KB) removes only these three kinds. Its third line states the filter and the counts. All S7 lines, `=== ` lines, load lines, panic texts, `test result` lines and `exit=` lines are as printed.
- **Extra local run at 8 writers.** The plan verify covers 2 and 9 writers. One more short run at 8 shows `cap_checked=true` at the edge of the bar.
- **Docker form.** The harness form of the 21-01 method (tar stream, no bind mount), not `run.sh harness`: `run.sh` uses the bind mount and prints no `rustc -V` line. The container had a name, so a `timeout` kill could not leave it running.
- **Build before the runs.** The container built the test binary with `--no-run` before the first run, so no run includes build time.
- POOL-05 is not marked complete: plan 21-09 owns the marks.

## Known Stubs

None.

## Threat Flags

None. T-21-27: tar stream of the committed tree, the 20-01 `rustc -V` line, default sizes. T-21-28: the assert reads `DEFAULT_WORKERS_MAX`; the local runs show `cap_checked=true` at 2 and 8 and `false` at 9.

## Open points for a decision

- **8 writers are above the cap before the limit (419-441 MiB).** This is the gap that plan 21-05 (step limit) must close: the bar is hard at 8 writers. The growth is about 35 MiB per writer above 2, and the throttle timeouts grow with the writer count (0-5 at 2 writers, 361-783 at 8, 1595-2088 at 16). The 21-06 judgement uses these runs as the "before" side.
- **The harness's own memory grows with the writer count.** Each writer has a 1 MiB version model (one u32 per 4 KiB block), a 1 MiB write buffer, and a 1 MiB read buffer in the check phase: about 3 MiB per writer, 24 MiB at 8 writers. This counts against the 320 MiB bar. It is small next to the 35 MiB per writer that the run shows, so no change was made.
- **1 writer: one very slow write in 2 of 3 runs (9.4 s and 9.6 s).** At 1 writer the dirty total stays at 32 MiB, the flusher does no step (`flusher_steps=0`) and nothing is throttled, so the writer pays its own save steps. The slowest write then includes a long step. The run ends in about 20-30 s. This is outside the memory bar, but it is a latency value that the step limit (21-05) should not make worse; 21-06 can compare it.
- **16 writers: slowest write 4.6 to 6.5 s** and VmHWM 688-730 MiB. Above the default cap of 8, so recorded and reported to the owner, not a stop.

## Self-Check: PASSED

- FOUND: crates/slicefs-cli/tests/stress_harness.rs (S7_THREADS, DEFAULT_WORKERS_MAX)
- FOUND: scripts/docker-linux-test/run.sh (S7_THREADS)
- FOUND: .planning/phases/21-worker-pool/runs/s7-before-limit.log
- FOUND: 2dc170d, ef26ea2 (ancestors of HEAD)
