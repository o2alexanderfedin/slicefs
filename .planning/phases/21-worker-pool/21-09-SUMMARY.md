---
phase: 21-worker-pool
plan: 09
subsystem: docs
status: complete
tags: [pool-01, pool-02, pool-03, pool-04, pool-05, docs, gate]

requires:
  - phase: 21-worker-pool
    provides: "21-01..21-08: the pool, the tests, the S7 notes (R1=pass), the bench notes (verdict=PASS, mem_bar=pass), runs/pool-bench-macos.log, runs/ci-evidence.md for 7853542"
provides:
  - "docs/research/multithreaded-request-handling.md: ## Phase 21 result; step 6 row marked as shipped"
  - "README.md: ### Request workers (mount option), with SLICEFS_FUSE_TRACE=1"
  - ".planning/phases/21-worker-pool/deferred-items.md: ## Open (owner reports, later work, found and not fixed)"
  - "REQUIREMENTS.md: POOL-01 to POOL-05 Complete"
  - "runs/gate-21-09.log: Docker smoke and live checks with 4 workers"
affects: [phase-21-verification, phase-22]

estimate:
  tokens: 70000
  tasks: 2
actuals:
  tokens: 7500    # chars/4 over the added lines of the two task commits (30155 chars)
  tasks: 2
  commits: 2

plan_head_before: 15b32f116607eb335d2f1495a483cb2e27243bef
plan_head_after: 6291c933fc5151f7126ac54579da3f6cd97bb17c
commits: 2

tech-stack:
  added: []
  patterns:
    - "Gate tree by `git archive HEAD`, so edits in the working tree cannot enter the container"
    - "The gate script stops when the build fails, so no check runs on an old binary in the target volume"

key-files:
  created:
    - .planning/phases/21-worker-pool/deferred-items.md
    - .planning/phases/21-worker-pool/runs/gate-21-09.log
  modified:
    - docs/research/multithreaded-request-handling.md
    - README.md
    - .planning/REQUIREMENTS.md
    - .planning/phases/21-worker-pool/21-RESEARCH.md
    - .planning/phases/21-worker-pool/21-01-PLAN.md
    - .planning/phases/21-worker-pool/21-01-SUMMARY.md

decisions:
  - "POOL-01 to POOL-05 marked Complete, each on its evidence (table below)"
  - "The [Step limit] item is closed in the Phase 21 result: R1=pass and mem_bar=pass; the fix is the 21-05 mmap threshold, no step limit"
  - "No follow-up issue for POOL-05: verdict=PASS. The write plateau is an owner report"
  - "The old 150 s text is kept as history, with the note '200 s since the #170 fix'"

metrics:
  duration: "about 60 min"
  completed: 2026-11-24
---

# Phase 21 Plan 09: Phase 21 close Summary

The research doc has the Phase 21 result. The README documents `--workers` and `SLICEFS_FUSE_TRACE=1`, and the text matches `slicefs mount --help`. The deferred list names what stays open. POOL-01 to POOL-05 are marked Complete on their evidence. The phase gate is green: the full suite command exits 0, and the Docker smoke and live checks pass with 4 workers.

## Owner reports (first)

- **S7 at 16 writers: R2=miss and R3=miss (report only).** 16 writers give 412 to 426 MiB, above 320 MiB. Pm(16) - Pm(4) = 199 MiB (rule: at most 64). Pm(8) - Pm(4) = 63 MiB passes by 1 MiB. 16 is above the default cap of 8, so it does not block.
- **Writes do not scale.** `ratio_write_4_1=1.26`. The rate rises from 1 to 2 workers and then stays flat (178.8, 169.5, 165.6 MB/s at 2, 4, 8). The `store` lock (the single pack appender) is the likely cause. The bench does not rule out fsync or disk limits.
- **16 workers give no gain over 8.** Ratios 16/8 are 0.84 to 1.04 in all phases. The default cap of 8 stays.
- **The trace lines cost the metadata rates 51 % to 77 %** (and the read rate 16 % to 34 %). They stay off by default.

## Tasks

| Task | Name | Commit | Files |
| ---- | ---- | ------ | ----- |
| 1 | Phase 21 result, README --workers, deferred list | a5162d4 | multithreaded-request-handling.md, README.md, deferred-items.md, 21-RESEARCH.md, 21-01-PLAN.md, 21-01-SUMMARY.md |
| 2 | POOL marks on evidence; phase gate log | 6291c93 | REQUIREMENTS.md, runs/gate-21-09.log |

## POOL marks and their evidence

| Req | Mark | Evidence |
|---|---|---|
| POOL-01 | Complete | `pool_tests` (9), the cli tests `workers_*`, `workers_for_cpus_halves_and_clamps`, `default_workers_is_one_on_macos`, `run_without_a_pool_runs_on_the_caller`: green in the gate suite run. `runs/smoke-21-01.log` and `runs/smoke-21-02.log` passed (4 and 1 workers). Gate smoke: `workers-line: Workers: 4 (set)`, `daemon-exit: ok`, `pool: workers=4 jobs=163`, `smoke-exit=0`. |
| POOL-02 | Complete | `drain_runs_every_queued_job`, `drain_limit_reports_a_stuck_worker`, `destroy_drains_before_shutdown`, `reply_after_locks` (2), `serve_panics_when_a_lock_is_held_at_the_reply`: green in the gate suite run. SIGTERM, SIGINT and SIGHUP `-exits: ok` in `runs/docker-verify-w4.log` and in `runs/gate-21-09.log`. |
| POOL-03 | Complete | `pool_panic_tests` (4 passed) green in the gate suite run. The POOL-03 block (`panic-aborts`, `panic-line`, `fsynced-file-replayed`, `half-done-entry-absent`, w4 and w1) ok in `runs/verify-21-03.log` and in the gate log; `RESULT panic-replay: PASS`. |
| POOL-04 | Complete | The three local Docker logs of 21-07 passed. `runs/ci-evidence.md` has `sha = 7853542bd6c5a6d3bf3d0e07dac7064aa4181e51`, which equals `git log -1 --format=%H -- crates vendor scripts .github Cargo.toml Cargo.lock` (check prints `ci-evidence-current`, also after both commits of this plan). ci.yml Linux job success; each of its three steps greps `Workers: 4 (set)` and fails without it (ci.yml lines 31, 37, 43), so a green job is the proof. ci.yml macOS job success. macos-mounts.yml success on FSKit with `Workers: 1 (default)`. The ci.yml run was on fdceef0; `git diff --stat 7853542 fdceef0 -- crates vendor scripts .github Cargo.toml Cargo.lock` is empty. |
| POOL-05 | Complete | `verdict=PASS` (read 2.69x, write 1.26x) and `mem_bar=pass` (178 MiB max at 1 to 8 workers) in `runs/pool-bench-NOTES.md`; `R1=pass` in `runs/s7-step-limit-NOTES.md`; `runs/pool-bench-macos.log` present. R2=miss, R3=miss and `mem16_max_mib=223` do not block (owner reports above). |

No requirement stays open.

## Phase gate

- **Full suite command** (macOS host, `timeout 5400`): `suite-exit=0`. 76 `test result: ok` lines, 1554 passed, 0 failed, 12 ignored. All six commands of the chain ran: `cargo test --workspace`, the two `cargo check --workspace --all-targets` (default and `lock-stats`), `cargo check` with `deadlock-detect`, and the `deadlock-detect` runs of `stress_harness serialize_off_target` and `flusher_tests` (26 passed).
- **Docker gate** (`runs/gate-21-09.log`): `git archive` of the committed tree at 15b32f1, mount form, volume `slicefs-target-p21`, debug build, the 45 s wait, `SLICEFS_WORKERS=4`, `timeout 3600`. `build-exit=0`, `smoke-exit=0`, `verify-exit=0`, `docker-exit=0`. Plan verify: 26 `workers-line: Workers: 4 (set)`, 1 `daemon-exit: ok`, 2 `panic-aborts-w(4|1): ok`, 3 `SIG(TERM|INT|HUP)-exits: ok`, 0 `: FAIL` or `NOT MOUNTED`, `same-toolchain`. 76 `: ok`. `RESULT verify-phases: PASS`, `RESULT panic-replay: PASS`. No home path and no legal-terms word in the log.
- The gate tree 15b32f1 has the same code as the final tree: `git diff --stat 15b32f1 HEAD -- crates vendor scripts .github Cargo.toml Cargo.lock` is empty.
- `slicefs mount --help` (`cargo run -q -p slicefs-cli -- mount --help`): "Request worker threads. Default on Linux: half the CPUs, at least 1 and at most 8; default on macOS: 1. With 1, each request runs on the FUSE reader thread. At least 1; an explicit value above 8 is allowed". The README part says the same.

## README part (as committed)

```
### Request workers (mount option)

| Option | Default | Meaning |
|---|---|---|
| `--workers <N>` | Linux: half the CPUs, at least 1 and at most 8. macOS: 1. | Request worker threads. With 1, each request runs on the FUSE reader thread. |

- N must be at least 1. An explicit value above 8 is allowed.
- At start, the daemon prints the count: `Workers: <N> (set)` or `Workers: <N> (default)`.
- More workers help parallel reads of different files. In a Linux test with 16 clients on different files, reads with 4 workers were 2.7 times as fast as with 1 worker. Writes gain little after 2 workers (1.3 times with 4 workers).
- Each worker adds some memory. In the same test, the peak memory of the daemon was 124 MiB with 1 worker and 178 MiB with 8 workers. On Linux the daemon sets a fixed glibc mmap threshold (128 KiB) at start, so that freed memory goes back to the system. It prints `Allocator: mmap threshold 131072 bytes (fixed)`.
- On macOS the default stays 1 until a test shows a gain on FSKit.
- Request trace: set `SLICEFS_FUSE_TRACE=1` in the environment of `slicefs mount` to print one ENTRY line and one EXIT line on stderr for each request. It is a debug aid and is off by default (no value, or `0`). The lines slow every request, and with more than 1 worker the workers wait for each other at stderr.
```

## Deviations from Plan

**1. [Launch facts] No step limit.** The plan text names SAVE_STEPS 2, SLOT_WAIT 200 ms and the May and Must rule, and a README sentence "the step limit keeps the save work to two steps at a time". None of this exists. The Phase 21 result describes the 21-05 allocator setting (`mallopt(M_MMAP_THRESHOLD, 128 KiB)`, the `Allocator:` line, the `steps:` line), and the README gives the measured memory instead. The S7 table is "before and after the allocator setting".

**2. [Launch facts] 200 s, not 150 s.** The plan says "submit and drain 150 s". The code has 200 s (the #170 fix). All new text says 200 s. The old text in 21-RESEARCH.md (6 places), 21-01-PLAN.md (2) and 21-01-SUMMARY.md (2) keeps 150 s with the note "200 s since the #170 fix". 21-RESEARCH.md Open Question 3 also got a correction note: it said the sum covers "the 200 ms step-slot wait of plan 21-05", which does not exist. Open Question 6 got a note: the trace lines are off by default since Amendment 2 (it said "default on").

**3. [Rule 3 - Blocking] Docker build killed by low memory.** The first gate container could not build (`rustc` SIGKILL for chunk-store and metadata). The script went on, so mt-linux.sh and verify-phases.sh ran on an old binary in the target volume. That run is not valid and is not committed. Cause: the OrbStack VM had about 0.9 GiB free and no swap left. Two other OrbStack machines of the host (not of this project) hold about 13 GiB in their `/tmp` tmpfs. I did not touch them. A build with `-j 4` was also killed. The valid run used `cargo build -j 1`, and the script now stops when the build fails.

**4. [Rule 3] Gate tree by `git archive`.** The plan's tar of `git ls-files -co` reads the working tree. I edited docs while the gate ran, so the valid run used `git archive HEAD` (15b32f1, clean, the same code as the final tree).

**5. The gate ran beside the host suite.** The Docker gate and the host suite ran at the same time. Both passed, so no rerun was needed.

**6. Code text that is wrong and not fixed (no code change in this plan).** The `SUBMIT_WAIT` doc comment in `pool.rs` names "the 200 ms step-slot wait" (no step slot exists). The getlk and setlk doc comments still say "single-threaded". Both are in the deferred list. A fix would change `crates` and make the CI evidence old.

## Deferred list (`deferred-items.md`, `## Open`)

- Owner reports: S7 at 16 writers (R2, R3); writes do not scale; 16 workers give no gain; cost of the trace lines.
- Later work: macOS default above 1 (with the macOS numbers); per-inode WRITE order on FUSE-T (A6); abort on a flusher panic (Open Question 2); the Phase 19 windows with 2 or more workers; a rise of the cap of 8 by the rule; WAL group commit; the Phase 19 and 20 open items; adaptive control (#152) with the `pool:` and `steps:` counters; #170 follow-ups (fsync, release, replay and commit still give EIO on a busy store; the retry does not stop during the pool drain, because the shutdown mark comes after it).
- Found and not fixed: the stale `SUBMIT_WAIT` comment; getlk/setlk comments; the gate taken for nothing in 4 callbacks; the worker thread name cut to 15 bytes; the idle worker wait with no time limit; the abort line `0 s` at a short limit; `pool_panic_tests` time on a loaded macOS host; `freed_bytes=0` in the smoke; the 1-writer S7 peak 9 MiB higher; CAP(8) = 336 above 320 with a 30 MiB margin; the 7 s writes at 16 writers; one 68 ms `stripe` hold; the `inode_map` wait; the macOS bench log `run=1` on every `macbench` line.
- Closed: the [Step limit] item (R1=pass, mem_bar=pass); the POOL-05 bar (no follow-up issue).

## Known Stubs

None.

## Threat Flags

None. T-21-25: the marks follow the evidence checks above. T-21-26: the added-lines gate on the doc, the README and the deferred list prints 0, and on REQUIREMENTS.md prints 0. T-21-SC: no new crate.

## Self-Check: PASSED

- FOUND: docs/research/multithreaded-request-handling.md (`## Phase 21 result` 1), README.md (`### Request workers` 1), .planning/phases/21-worker-pool/deferred-items.md (`## Open` 1), .planning/phases/21-worker-pool/runs/gate-21-09.log
- FOUND: commits a5162d4 and 6291c93 in HEAD
- commits: 2 = `git rev-list --count 15b32f1..6291c93` (the docs commit of this SUMMARY comes after)
