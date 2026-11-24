---
phase: 21-worker-pool
plan: 06
subsystem: measurement
status: complete
tags: [pool-05, s7, allocator, vmhwm, cap]

requires:
  - phase: 21-worker-pool
    provides: "21-05: alloc_tune::tune_at_start (mallopt M_MMAP_THRESHOLD 128 KiB) in S7; S7 line fields mmap_threshold=, heap_live_at_peak_rss_mib=, malloc_free_at_peak_rss_mib=, save_steps=, freed_bytes=; #170 fix (write_busy_retries=, flusher_busy=, store_wait_ms=)"
  - phase: 21-worker-pool
    provides: "21-04: S7_THREADS knob and the before-runs (runs/s7-before-limit.log, runs/s7-throttle-200ms.log)"
provides:
  - "runs/s7-step-limit.log: 19 S7 flusher-form runs at 1, 2, 4, 8 and 16 writers with the 21-05 allocator setting"
  - "runs/s7-step-limit-NOTES.md: R1=pass, R2=miss, R3=miss; Pm(N), Spread(N), CAP(4) = 296, CAP(8) = 336, CAP(16) = 480, CAP_MIB = 320; before and after; the harness term; memory parts at the RSS peak"
affects: [21-08, 21-09]

estimate:
  tokens: 50000
  tasks: 2
actuals:
  tokens: 10700    # chars/4 over the committed log (32281 chars) and notes (10647 chars)
  tasks: 2
  commits: 1

plan_head_before: 839713bb4bcafae52de9269463ef6d2673fa34b6
plan_head_after: 7b5a923323bac9c16ed227a22a6ceaeadc72a58b
commits: 1

tech-stack:
  added: []
  patterns:
    - "Per-run time limit (timeout 1200) inside the container, a toolchain check that stops the run before the build"

key-files:
  created:
    - .planning/phases/21-worker-pool/runs/s7-step-limit.log
    - .planning/phases/21-worker-pool/runs/s7-step-limit-NOTES.md
  modified: []

decisions:
  - "The log and notes keep the name s7-step-limit, because 21-08, 21-09 and 21-VALIDATION refer to it (the Amendment allowed this)"
  - "EIO or mismatch runs would be recorded as not a pass and the set would continue (the Amendment and the launch message). The set would stop only on a run time limit or a run with no S7 line. Neither happened"

metrics:
  duration: "about 45 min (runs 09:38 to 10:10 UTC)"
  completed: 2026-11-24
---

# Phase 21 Plan 06: S7 with the 21-05 allocator setting at 1 to 16 writers Summary

With `mallopt(M_MMAP_THRESHOLD, 128 KiB)` and no step limit, S7 stays under the 320 MiB cap at 1, 2, 4 and 8 writers in all 16 runs (largest value 290 MiB at 8 writers). 16 writers give 412 to 426 MiB. Before the setting, 16 writers gave 688 to 730 MiB.

## Result (the pass rule was written before the runs)

- **R1=pass (hard bar).** There were 16 runs at 1, 2, 4 and 8 writers. All exited 0, with 0 mismatches and VmHWM <= 320. Every run of the 19 printed `mmap_threshold=131072`. `write_busy_retries=0`, `flusher_busy=0` and `flusher_errors=0` in all 19 runs. There was no EIO.
- **R2=miss (report only).** At 16 writers the values are 412, 413 and 426 MiB. The model checks passed. The cap is not asserted above 8 writers.
- **R3=miss (report only).** Pm(16) - Pm(4) = 199 (the rule is <= 64). Pm(8) - Pm(4) = 63, which passes by 1 MiB.
- So the step limit and the step pool are not needed for the hard bar. POOL-05 is not marked by this plan: 21-09 marks it only with `R1=pass` here and the 21-08 bench evidence (`verdict=PASS`, `mem_bar=pass`).

## Pm before and after the setting

| N | Before: Pm (range) | After: Pm (range) |
|---|---|---|
| 1 | 97 (90-97) | 106 (101-106) |
| 2 | 216 (198-216) | 173 (168-173) |
| 4 | 340 (262-340) | 227 (199-227) |
| 8 | 441 (419-441) | 290 (271-290) |
| 16 | 730 (688-730) | 426 (412-426) |

CAP(4) = 296, CAP(8) = 336, CAP(16) = 480, recorded only. CAP_MIB = 320 (not changed).

## Run table

| threads | vmhwm_mib | within_cap | exit | write_busy_retries | flusher_busy | live@peak | free@peak | slowest_write_ms |
|---|---|---|---|---|---|---|---|---|
| 4 | 205 | true | 0 | 0 | 0 | 135 | 56 | 1569.7 |
| 8 | 271 | true | 0 | 0 | 0 | 163 | 80 | 1400.9 |
| 16 | 426 | false | 0 | 0 | 0 | 240 | 128 | 2315.3 |
| 1 | 106 | true | 0 | 0 | 0 | 64 | 15 | 720.4 |
| 2 | 170 | true | 0 | 0 | 0 | 118 | 44 | 335.5 |
| 4 | 217 | true | 0 | 0 | 0 | 141 | 43 | 421.8 |
| 4 | 212 | true | 0 | 0 | 0 | 123 | 75 | 495.5 |
| 8 | 281 | true | 0 | 0 | 0 | 162 | 90 | 1505.8 |
| 16 | 413 | false | 0 | 0 | 0 | 244 | 116 | 7105.2 |
| 1 | 106 | true | 0 | 0 | 0 | 79 | 4 | 1568.7 |
| 2 | 168 | true | 0 | 0 | 0 | 112 | 37 | 273.2 |
| 4 | 199 | true | 0 | 0 | 0 | 139 | 47 | 517.5 |
| 4 | 205 | true | 0 | 0 | 0 | 129 | 58 | 433.8 |
| 8 | 290 | true | 0 | 0 | 0 | 161 | 89 | 2062.5 |
| 16 | 412 | false | 0 | 0 | 0 | 206 | 137 | 7453.3 |
| 1 | 101 | true | 0 | 0 | 0 | 68 | 13 | 781.2 |
| 2 | 173 | true | 0 | 0 | 0 | 123 | 39 | 388.8 |
| 4 | 213 | true | 0 | 0 | 0 | 135 | 55 | 424.4 |
| 4 | 227 | true | 0 | 0 | 0 | 125 | 88 | 775.9 |

The full table (save_steps, freed_bytes, throttle counters, flusher steps and drops) is in `runs/s7-step-limit-NOTES.md`.

## Harness term and cause above 8 writers

- Harness term: about 3 MiB for each writer at most (the 1 MiB version model, the 1 MiB write buffer and the 1 MiB check read). Pm(N) minus 3N is 103, 167, 215, 266 and 378 MiB. The test part does not explain the rise.
- At the RSS peak, the free memory that malloc keeps at 8 writers fell from 253 MiB (diagnosis, no setting) to about 89 MiB. From 8 to 16 writers, the median live heap rises by about 78 MiB (about 10 MiB for each writer). The file-backed RSS rises by about 30 MiB (it follows the stored bytes) and the kept free memory rises by about 38 MiB. So the 16-writer miss now comes mostly from live data and mapped files, not from arena retention.

## Run method

Docker Linux, harness form, release. Volume `slicefs-target-p21`. `rustc 1.100.0 (0ef1bc386 2026-11-09)` (checked in the container before the build, and equal to the 20-01 line). Tar stream of the committed tree at 839713b. One container. Order: 3 x (4, 8, 16, 1, 2, 4), then 4. `S7_STORE_WAIT_MS` was unset. Each run had a 1200 s time limit. The committed log is the raw log (2.47 MB) less the cargo warning blocks, the [rebase] lines and the [FUSE] overlay lines (the header gives the counts). It has no home paths.

## Deviations from Plan

- **Amendment (orchestrator, 2026-11-24) applied.** The step-limit fields (`save_steps_max=`, `slot_*`, `pay_skips=`) do not exist. The check counts `mmap_threshold=131072`: 19 of 19. The precondition clause "filesystem.rs contains SAVE_STEPS" is superseded by the Amendment (no step limit was built). The other precondition parts held (21-04 and 21-05 committed, clean tree).
- **Stop rule.** The old plan text says to stop on a mismatch or an errno. The Amendment and the launch message say to record the run as not a pass. The script went on in that case, and stopped only on a run time limit or a run with no S7 line. No run hit either case, so this had no effect on the result.
- **No arena run.** R1 passed, so the `MALLOC_ARENA_MAX=2` branch did not apply.
- **No `cargo test --workspace`.** This plan made only a doc and log commit, and no code changed.

## Owner reports (R2, R3)

- 16 writers: 412 to 426 MiB, above 320 (it does not block, because 16 is above the default worker cap of 8). If the owner wants 16 writers under 320, the next options are the step limit, the byte step pool (964e13e) or a buffer reuse pool, aimed at the live heap.
- Pm(8) - Pm(4) = 63 MiB is only 1 MiB below the R3 limit.
- Slowest write: above 500 ms in 12 of 19 runs (up to 7.5 s at 16 writers; the cause of the 7 s writes is not identified, and the throttle, gate and commit maxima do not add up to it). Throttle waits reach their time limit in 10 to 45 % of waits at 4 writers and in 40 to 56 % at 8 and 16 writers. So the research Q9 delay-curve condition holds only as a borderline case at 8 and 16 writers.

## Open points

- The 1-writer Pm is 9 MiB higher than before (106 against 97). This was not examined. Page rounding of mmapped blocks is a possible cause.
- CAP(8) = 336 is above CAP_MIB 320. Pm(8) = 290 leaves 30 MiB, from only 3 runs (Spread 19 at 8 writers, 28 at 4). A later 8-writer run can miss the assert.
- A 7.5 s write at 16 writers is near the 10 s store wait. The #170 retry covers a busy store, but the margin is small.
- The daemon VmHWM at 1, 2, 4 and 8 workers (the `mem_bar` of the bench) is still to be measured in 21-08. POOL-05 needs both results.

## Known Stubs

None.

## Self-Check: PASSED

- FOUND: .planning/phases/21-worker-pool/runs/s7-step-limit.log
- FOUND: .planning/phases/21-worker-pool/runs/s7-step-limit-NOTES.md
- FOUND: commit 7b5a923 in HEAD
