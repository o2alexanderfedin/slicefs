---
phase: 20-dirty-budget-and-rebase-flusher
plan: 07
subsystem: concurrency
status: complete
tags: [lock-06, flusher, throttle, rss-cap, s7, docker]

requires:
  - phase: 20-dirty-budget-and-rebase-flusher
    plan: 01
    provides: "CAP_MIB = 320, the S7 VmHWM field, runs/s7-wave0-NOTES.md (P_old, P0, RUSTC)"
  - phase: 20-dirty-budget-and-rebase-flusher
    plan: 06
    provides: "the flusher, the throttle, handle_write_admitted, test_flusher_stats"
provides:
  - "dirty_total_stays_bounded_with_parallel_writers (flusher_tests.rs) and its helper throttled_write_sampled"
  - "S7_FLUSHER=1: the flusher form of s7_budget_baseline; it asserts VmHWM <= CAP_MIB on Linux"
  - "runs/s7-flusher.log: three Docker release runs of the flusher form (P1)"
affects: [20-08]

estimate:
  tokens: 90000
  tasks: 3
actuals:
  tokens: 31100     # chars/4 over the added lines of abf7011..a6cf1eb; the run log is about 29100 of it, code about 2000
  tasks: 3
  commits: 3

plan_head_before: abf7011f530d948910e7affcc377d7dd3d6bb310
plan_head_after: a6cf1eb5910f12cae49f06bcf228797af6930512
commits: 3

tech-stack:
  added: []
  patterns:
    - "Samples in the gate hold: a closure runs just before and just after the write handler"
    - "A drop guard stops the flusher of a scoped test, so a panic does not leave the scope join waiting"

key-files:
  created:
    - .planning/phases/20-dirty-budget-and-rebase-flusher/runs/s7-flusher.log
  modified:
    - crates/slicefs-cli/tests/flusher_tests.rs
    - crates/slicefs-cli/tests/stress_harness.rs
    - scripts/docker-linux-test/run.sh

decisions:
  - "The S7 cap assert uses VmHWM (/proc/self/status), not the getrusage peak (orchestrator)"
  - "The S7 line takes the largest throttle wait from the flusher stats (max_wait_ms), with no new histogram"

metrics:
  duration: "about 55 min (two workspace runs, one Docker probe, three Docker runs)"
  completed: 2026-11-23
---

# Phase 20 Plan 07: Dirty bound and the RSS cap with the flusher Summary

A new short test shows that the throttle and the flusher keep the shared dirty total under budget + 4 x (write + 64) + one rebase step with 4 parallel writers. The S7 run now has a flusher form (`S7_FLUSHER=1`). Three Docker Linux release runs of that form passed their model check with VmHWM 257, 278 and 287 MiB. The cap is 320 MiB and did not change. ROADMAP criterion 4 holds on these numbers.

## Tasks

| Task | Name | Commit | Files |
|---|---|---|---|
| 1 | Short test of the dirty bound with 4 parallel writers and the flusher | 78cab9f | tests/flusher_tests.rs |
| 2 | S7 flusher form (`S7_FLUSHER=1`) with the cap assert | efc1e9b | tests/stress_harness.rs, scripts/docker-linux-test/run.sh |
| 3 | Docker P1 runs of the flusher form against the fixed cap | a6cf1eb | runs/s7-flusher.log |

## Task 1

- `throttled_write_sampled(fs, fh, off, data, before, after)` runs `before` and `after` in the gate hold, just before and just after the handler. `throttled_write` now calls it with empty closures.
- `dirty_total_stays_bounded_with_parallel_writers`: 4 files of 8 MiB, limit 2 MiB, 28 dirty pieces per file (about 0.9 MiB) with the default budget. Then the flusher runs (pass 20 ms, not paused), the throttle wait limit is 10 s, and the budget becomes 4 MiB. Each writer makes 200 writes of 32 KiB at random slots of the 64 KiB grid (xorshift, no new crate). In the gate hold it reads its own cost before and after the handler, and the total after it (`fetch_max` into `AtomicUsize`).
- Asserts: every write is `Ok(32768)`; the largest growth of the own cost of one write is at or below 32 KiB + `EXTENT_OVERHEAD` (32832); the largest sampled total is at or below 4 MiB + 4 x 32832 + `REBASE_STEP` = 5374208; each file matches its block-version model with the handle open and after release.

Green runs (local, macOS, debug), seven runs:

| Run | max_total | bound | max_own_growth | passes | steps / published | throttled | timeouts | max_wait_ms |
|---|---|---|---|---|---|---|---|---|
| 1 | 4268160 | 5374208 | 32832 | 487 | 14 / 14 | 56 | 0 | 93 |
| 2 | 4235328 | 5374208 | 32832 | 487 | 14 / 14 | 56 | 0 | 122 |
| 3 | 4268160 | 5374208 | 32832 | 524 | 14 / 14 | 55 | 0 | 188 |
| 4 | 4268160 | 5374208 | 32832 | 469 | 14 / 14 | 55 | 0 | 146 |
| 5 | 4268160 | 5374208 | 32832 | 520 | 14 / 14 | 54 | 0 | 133 |
| 6 | 4268160 | 5374208 | 32832 | 472 | 14 / 14 | 55 | 0 | 83 |
| 7 | 4268160 | 5374208 | 32832 | 473 | 14 / 14 | 56 | 0 | 97 |

The largest sampled total is 4268160 bytes (4.07 MiB), about 1.06 MiB under the bound. The test takes about 2 s. The whole `flusher_tests` binary: 22 passed in 4.45 s.

Guard mutation (red run, not committed): `throttle_write` returned `Admit { pay: false }` at once, after the kick and before any wait. `src/filesystem.rs` was copied to a scratch file first and restored from it after the run; `cmp` showed no difference. The run failed at the bound assert, with the flusher running (not paused), in 0.94 s:

`the largest sampled total 9751104 is above the bound 5374208: FlusherStats { passes: 1, steps: 1, published: 1, drops: 0, skips: 0, errors: 0, throttled: 0, timeouts: 0, stop_exits: 0, max_wait_ms: 0 }`

So the form that went red is the plan's first form (throttle off, flusher on). The second form (also pause the flusher) was not necessary.

## Task 2

- `S7_FLUSHER` is read with `env_num` (default 0, range 0..=1). When it is 1, serialize is off whatever `S7_SERIALIZE` says. `flusher::run_flusher(&ctx.fs)` runs in the scope, and the test waits (up to 10 s) until the flusher runs before the writers start. A drop guard (`StopFlusher`) calls `stop_flusher()` after the check phase, and also when the scope ends on a panic.
- Each write calls `ctx.fs.throttle_write()` first (no lock held), then `ctx.gated(...)` with `handle_write_admitted`. The `max_dirty` sample stays in the closure. The default form calls `handle_write_admitted` with `Admit { pay: true }` and flags 0; this is what `handle_write` does, so the default form has the same behavior.
- New fields on the `S7 ` line: `flusher=0|1`, `commit_gate_wait_max_ms=`, `flusher_passes=`, `flusher_steps=`, `flusher_published=`, `flusher_drops=`, `flusher_skips=`, `flusher_errors=`, `throttled=`, `throttle_timeouts=`, `throttle_wait_max_ms=`. The largest throttle wait comes from `test_flusher_stats().max_wait_ms`. The throttle measures it, and `slowest_write_ms` does not include it.
- The S7 line is printed before the asserts. Then the mismatch assert, then (flusher form only) `VmHWM <= CAP_MIB` with both numbers in the message. With no VmHWM (not Linux) the test prints `S7 cap not checked: no VmHWM on this system`.
- `CAP_MIB` is still 320 (`const CAP_MIB: u64 = 320`, the same as `CAP_MIB = 320` in the 20-01 notes). The workload did not change.
- `run.sh` `harness` mode passes `-e S7_FLUSHER`, and the usage comment names `S7_FLUSHER=1 for the flusher form`.

Local check (macOS, debug, `S7_FILE_MIB=8 S7_WRITE_MIB=4 S7_COMMIT_SECS=1 S7_FLUSHER=1`): an `S7 ` line with `flusher=1` and `vmhwm_mib=na`, then `S7 cap not checked`, test ok in 3.73 s.

Default harness time: 9.86 s in the Task 2 workspace run, and 9.65 s in the Task 1 workspace run. Two warm runs alone took 11.27 s and 10.71 s at a host load average of about 75. The default tests do not run `s7_budget_baseline` (it is ignored), so this plan does not change their code path. The time above 10 s comes from the load.

## Task 3

- Image: `docker build` from the same Dockerfile (orchestrator correction). `rustc -V` in the image is `rustc 1.100.0 (0ef1bc386 2026-11-09)`, the same as the `RUSTC =` line of `runs/s7-wave0-NOTES.md`. Image ID of the three measured runs, for reference only: `sha256:cdecaa062837b5d1c249becf3670bc203bffe9023e9975a75623c3e679501da0` (the probe run built again and got another ID; the containerd store gives a new ID for each build).
- Tree: `git archive HEAD` of `efc1e9b` (porcelain 0 lines) as a tar stream into `docker run -i`, target volume `slicefs-target-p20`, release, `timeout 5400`. Kernel `Linux 7.2.12-orbstack x86_64`, 32 CPUs. Host load average 69.30 / 56.64 / 58.34 at the start and 29.55 / 49.00 / 55.18 at the end. VM load (1 min) 4.6 to 4.9 before each run.
- Before the measurement, one small probe run (`S7_FILE_MIB=8 S7_WRITE_MIB=4 S7_COMMIT_SECS=1`) went into a scratch log. It gave `vmhwm_mib=60`, test ok, and it built the release binary. It is not in `runs/`.

| Run | vmhwm_mib | peak_rss_mib | rss_after_writes_mib | max_dirty_mib | slowest_write_ms | rebase_steps | commits | commit_gate_wait_max_ms | passes | steps / published | drops | throttled | timeouts | throttle_wait_max_ms | time |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 257 | 257 | 234 | 65 | 321.1 | 179 | 3 | 692.6 | 9238 | 128 / 128 | 0 | 428 | 0 | 1015 | 83.10 s |
| 2 | 278 | 272 | 246 | 65 | 2485.1 | 138 | 2 | 12.4 | 36605 | 120 / 120 | 0 | 437 | 0 | 525 | 59.58 s |
| 3 | 287 | 281 | 253 | 65 | 1238.4 | 130 | 2 | 105.8 | 37761 | 118 / 118 | 0 | 411 | 0 | 282 | 42.25 s |

All three runs: `test result: ok`, no mismatch, no errno, `flusher_skips=0`, `flusher_errors=0`.

Numbers for criterion 4:
- P_old = 260 MiB, P0 = 267 MiB (20-01, VmHWM max).
- P1 = 287 MiB (VmHWM max of the three flusher runs).
- CAP_MIB = 320 MiB. Margin = 320 - 287 = 33 MiB.
- The plan's awk on `runs/s7-wave0.log` prints `runs=6 modes=2 pm=267 spread=20 r=40 cap=320`. `runs/s7-wave0.log` and the notes did not change since `e4bc8d3b9ac1a85ec2d9455d1f7eefc0bdf0c034`, the 20-01 Task 3 sha.

Notes on the numbers:
- P1 is 20 MiB above P0. VmHWM was higher in each later run (257, 278, 287), and `rss_after_writes_mib` too (234, 246, 253). Each run is a new `cargo test` process, so nothing accumulates from one run to the next. The host load fell from about 69 to about 29 during the three runs; this is a fact, not a known cause. The P0 runs had 207 to 215 MiB after the writes. The flusher form keeps about 30 to 40 MiB more after the write phase. The dirty total is the same (`max_dirty_mib=65`), so this is not dirty data. A possible cause is malloc arenas of the flusher thread (the 20-01 arena run showed that arenas are a part of the overhead). This plan did not measure it.
- `flusher_drops=0` in all runs, next to 118 to 128 steps. So the hot rule holds under this load (research Pitfall 1 did not occur).
- `throttle_timeouts=0`: no writer reached the 5 s wait limit. The largest throttle wait was 1015 ms.
- `max_dirty_mib=65`: the sampled total went about 1 MiB above the 64 MiB budget. The 1 MiB fill writes and the bound of Task 1 permit this.

## Empty flusher passes (deferred item from 20-05)

The pass count is high: 9238, 36605 and 37761 passes in 42 to 83 s, for 118 to 128 steps. That is 98.6 % (run 1) to 99.7 % (runs 2 and 3) empty passes, or up to about 900 passes per second. The local bound test shows the same shape (about 480 passes for 14 steps in about 1 s). Each write that finds the total at or above half the budget kicks the flusher, and with hot files the pass finds no candidate. The runs did not measure CPU time per pass. The memory and latency numbers above are good, so the cost does not show in them. This is for a decision (see Open points).

## Deviations from Plan

### Auto-fixed Issues

None.

### Other deviations

**1. No separate RED commit.** The project rule is to commit only when `cargo test --workspace` passes. The Task 1 red run is the guard mutation, recorded above; it was not committed.

**2. Docker image built again; image ID not compared.** The plan says not to run `docker build` and to compare the image ID with the `IMAGE_ID` line. The orchestrator correction replaced this check. The image was built from the same Dockerfile. `rustc -V` in the image equals the `RUSTC =` line of the 20-01 notes. The check `test "RUSTC = $(grep -m1 '^rustc ' runs/s7-flusher.log)" = "$(grep -E '^RUSTC = ' runs/s7-wave0-NOTES.md)"` printed `same-toolchain`. Only the rustc half of the plan's second Task 3 verify command was run. The image ID is in the log for reference only.

**3. The S7 line has two more flusher fields than the plan lists.** `flusher_skips=` and `flusher_errors=` are on the line too. They come from the same stats copy and cost nothing. A step error would otherwise not show.

**4. The S7 line "baseline only: flusher lag and pause curve need Phase 20" was removed.** It is not true now that Phase 20 has the flusher form.

**5. Extra Docker probe run** (small sizes, scratch log), as in 20-01. It is not part of P1.

**6. Default harness time above 10 s in two warm runs alone** (11.27 s and 10.71 s at a load average of about 75). In both workspace runs it was under 10 s (9.65 s and 9.86 s). The plan changes only the ignored S7 test.

## Open points

- **Empty flusher passes (decision).** Up to about 37800 passes for about 120 steps in one S7 run. Options: accept it (passes are cheap and the numbers are good), or do not kick the flusher when the last pass found no candidate (the option in deferred-items.md). A CPU measurement of the flusher thread would settle it.
- **P1 is above P0 by 20 MiB, and the margin to the cap is 33 MiB.** The three P1 values span 30 MiB (257 to 287); the 20-01 rule used a spread of 20 MiB. So the margin is about one spread. It is under the cap, but the owner may want one `MALLOC_ARENA_MAX=2` flusher run to see if the extra memory is arenas.
- **Harness time.** The Task 2 check fails on a warm default harness run above 10 s. Two runs alone gave 11.27 s and 10.71 s at a load average of about 75. The workspace run on the committed Task 2 tree gave 9.86 s. Check again at phase close on a quiet machine (as deferred-items.md says).
- **Requirements.** LOCK-06 is in the plan frontmatter. It is not marked complete (orchestrator rule); REQUIREMENTS.md did not change.

## Threat model check

- T-20-SC: no new crate (the random slots use an inline xorshift).
- T-20-21: the throttle and the flusher bound the total; the bound test is red on its guard mutation; three Docker runs are under the fixed cap.
- T-20-22: `CAP_MIB` is 320, the same as the 20-01 notes; the wave-0 log and notes did not change since `e4bc8d3`.
- T-20-23: tar stream (`git archive`) of the committed, clean tree `efc1e9b`; the commit, `uname -a` and `rustc -V` are in the log.

## Known Stubs

None.

## Self-Check: PASSED

- FOUND: tests/flusher_tests.rs, tests/stress_harness.rs, runs/s7-flusher.log, this file
- FOUND: 78cab9f, efc1e9b and a6cf1eb in HEAD
- `commits: 3` is measured with `git rev-list --count` from `plan_head_before`. The docs commit (this file, STATE.md, ROADMAP.md) comes after it.
