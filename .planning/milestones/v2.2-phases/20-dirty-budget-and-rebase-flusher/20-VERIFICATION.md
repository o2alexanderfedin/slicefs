---
phase: 20-dirty-budget-and-rebase-flusher
verified: 2026-11-23T14:51:21Z
status: passed
score: 5/5 must-haves verified
covered_files:
  - .planning/phases/20-dirty-budget-and-rebase-flusher/20-01-PLAN.md
  - .planning/phases/20-dirty-budget-and-rebase-flusher/20-01-SUMMARY.md
  - .planning/phases/20-dirty-budget-and-rebase-flusher/20-02-PLAN.md
  - .planning/phases/20-dirty-budget-and-rebase-flusher/20-02-SUMMARY.md
  - .planning/phases/20-dirty-budget-and-rebase-flusher/20-03-PLAN.md
  - .planning/phases/20-dirty-budget-and-rebase-flusher/20-03-SUMMARY.md
  - .planning/phases/20-dirty-budget-and-rebase-flusher/20-04-PLAN.md
  - .planning/phases/20-dirty-budget-and-rebase-flusher/20-04-SUMMARY.md
  - .planning/phases/20-dirty-budget-and-rebase-flusher/20-05-PLAN.md
  - .planning/phases/20-dirty-budget-and-rebase-flusher/20-05-SUMMARY.md
  - .planning/phases/20-dirty-budget-and-rebase-flusher/20-06-PLAN.md
  - .planning/phases/20-dirty-budget-and-rebase-flusher/20-06-SUMMARY.md
  - .planning/phases/20-dirty-budget-and-rebase-flusher/20-07-PLAN.md
  - .planning/phases/20-dirty-budget-and-rebase-flusher/20-07-SUMMARY.md
  - .planning/phases/20-dirty-budget-and-rebase-flusher/20-08-PLAN.md
  - .planning/phases/20-dirty-budget-and-rebase-flusher/20-08-SUMMARY.md
  - .planning/phases/20-dirty-budget-and-rebase-flusher/runs/s7-flusher.log
  - .planning/phases/20-dirty-budget-and-rebase-flusher/runs/s7-wave0-NOTES.md
  - .planning/phases/20-dirty-budget-and-rebase-flusher/runs/smoke-20-08.log
  - crates/slicefs-cli/src/cli.rs
  - crates/slicefs-cli/src/commit_timer.rs
  - crates/slicefs-cli/src/content.rs
  - crates/slicefs-cli/src/filesystem.rs
  - crates/slicefs-cli/src/flusher.rs
  - crates/slicefs-cli/src/fuse_callbacks.rs
  - crates/slicefs-cli/src/handlers.rs
  - crates/slicefs-cli/src/lib.rs
  - crates/slicefs-cli/src/main.rs
  - crates/slicefs-cli/src/mount.rs
  - crates/slicefs-cli/tests/flusher_tests.rs
  - crates/slicefs-cli/tests/no_take_out.rs
  - crates/slicefs-cli/tests/stress_harness.rs
  - scripts/docker-linux-test/run.sh
covered_digest: "v4:sha256:0252a722d3c58edbcac7bdc7e906c1550988a929975f00a33c51a91a30f54aee"
behavior_unverified: 0
overrides_applied: 0
human_verification:
  - test: "Run the Docker Linux mount smoke (scripts/docker-linux-test mt-linux.sh, as in runs/smoke-20-08.log) on HEAD 23c61be or later, with a clean tree and the same rustc."
    expected: "All steps ok, including unmount and daemon-exit; exit = 0; the daemon exits within 10 s of the unmount."
    why_human: "The smoke log is on cbb1182. After it, b912aed changed shutdown() and the commit timer loop, and 7ff8c3c changed how main.rs builds the DirtyConfig. Both run in every mount and unmount. Unit tests of these changes pass on HEAD, but no mount ran on HEAD. A Docker mount is out of scope for this verifier."
---

# Phase 20: Dirty budget and rebase flusher - Verification Report

**Phase goal:** With several writers, daemon memory stays under the set limit, a writer over the limit waits before it takes any lock, and a commit never deadlocks with throttled writers (#21).
**Verified:** 2026-11-23T14:51:21Z
**Status:** human_needed
**Re-verification:** No. This is the first verification.

## Result in short

- The code gives all four ROADMAP criteria. The tests of criteria 1 to 3 pass on HEAD. Criterion 4 is covered by the Docker S7 runs on efc1e9b (see the judgment below).
- One plan truth is not covered on HEAD: the Docker smoke mount "on the final tree". The smoke ran on cbb1182, before the unmount and CLI fixes. This is one human item, not a gap.
- No gap was found.

## Goal Achievement

### Observable Truths (ROADMAP success criteria)

| # | Truth | Status | Evidence |
|---|-------|--------|----------|
| 1 | No request takes another file's state out of `open_files`; the dirty-budget path uses per-file cost counters and the flusher; a test in the suite finds no take-out site, including `keep_budget` | VERIFIED | `filesystem.rs` has exactly 4 `self.cells.remove(` sites, in `finalize`, `finalize_locked`, `reclaim_inode` and `setattr_size_logged`. Each one removes the cell of the inode that the function itself works on: `finalize` and `finalize_locked` at the last release of that file; `reclaim_inode(ino)` for the deleted inode, only when it has no write handle; `setattr_size_logged` a transient cell that it inserted itself for a truncate of a closed file. None of them removes the cell of another file that a request still uses. `flusher.rs` has no removal. `keep_budget` is gone; `pay_budget` steps only the locked cell `g` of the writer. The token `open_files` is not in `filesystem.rs`, `handlers.rs` or `fuse_callbacks.rs`. `no_take_out.rs`: 4 passed on HEAD (`only_four_functions_remove_a_cell`, `the_budget_step_is_own_file_only`, `the_flusher_takes_no_cell_out`, `header_parser_finds_names`). Limit: the check matches the text `self.cells.remove(` only (review IN-03, accepted). The debug rank rule (a second `Rank::Cell` lock panics) is the dynamic half. |
| 2 | A background flusher lowers the shared dirty budget, a writer over the limit sleeps before it takes any lock, and every wait has a time limit | VERIFIED | `fuse_callbacks.rs:437` calls `throttle_write()` before `gate_shared()` (line 440). `throttle_write` has `debug_assert!(sync::rank::held().is_empty())`. `mount.rs:384-386` starts the flusher after the log replay, not on a snapshot mount. `flush_pass` takes the gate shared for one `flush_step` only and skips a cell with a step token or a writer at the cap. New waits, from the added lines of `9091b7f..HEAD` in `src`: the throttle wait (`freed.wait_for`, deadline `THROTTLE_WAIT` 5 s, slices of 50 ms, exits at stop, flusher end and a stuck pass); the flusher sleep (`wake.wait_for`, `FLUSH_PASS` 100 ms, wakes at stop); the join in `stop_flusher` (no own timeout; bound is one flusher step, which has `STORE_WAIT` 10 s and `STEP_WAIT_MAX` 120 s). The other new `lock()` calls are short flag locks. This check counts lock acquisitions apart from waits: the `gate_shared()` in `flush_step` is a new acquisition with no time limit of its own, but the exclusive hold of a commit is only the cut (`commit_cut_begin`), so it ends. The other `join()` and `sleep()` lines are in tests. Tests that pass on HEAD: `a_writer_over_the_budget_waits_for_the_flusher`, `the_flusher_lowers_the_total`, `a_throttled_writer_writes_after_the_limit`, `a_throttled_writer_does_not_wait_when_the_flusher_cannot_free`, `a_throttle_with_a_lock_held_panics`, `a_stop_with_a_lock_held_panics`, `stop_wakes_throttled_writers`, `a_flusher_panic_wakes_throttled_writers`, `dirty_total_stays_bounded_with_parallel_writers`. |
| 3 | A test with all workers throttled while a commit waits finishes without deadlock | VERIFIED | `all_writers_throttled_while_a_commit_waits_flusher_paused` and `all_writers_throttled_while_a_commit_waits_flusher_in_a_step` (`flusher_tests.rs:888`, `:923`). Each has a 60 s abort watchdog. Four writers wait in the throttle (`test_throttled_now() == 4`), a `timed_commit` is queued for the exclusive gate, the commit returns within 10 s, all writes are in, and the content is checked with the handles open, after release and after a replay of the log. Also `shutdown_with_a_parked_flusher_step_and_a_queued_commit_ends`. All pass on HEAD in debug and with `--features deadlock-detect`. |
| 4 | A Docker run with parallel random writers on large files keeps peak RSS (VmHWM) under one fixed cap | VERIFIED (judgment on a measurement of an older tree) | `runs/s7-flusher.log`: three `S7_FLUSHER=1` release runs in Docker Linux on efc1e9b (clean tree, rustc 1.100.0), 4 random writers on 1 GiB files, model check ok, `vmhwm_mib` 257, 278, 287 against `CAP_MIB = 320`. One fixed cap, from git: the only commit that set `const CAP_MIB` in `stress_harness.rs` in this phase is e4bc8d3 (20-01, before any flusher code); `s7-wave0-NOTES.md` has only that commit; `runs/s7-wave0.log` gives P_old 248, 260, 254 and P0 267, 247, 263 (maximum 267). `stress_harness.rs` asserts `VmHWM <= CAP_MIB`. See the judgment below. |
| 5 | (plan truth, no ROADMAP criterion) A Docker smoke mount on the final tree passes, and the daemon exits within 10 s after the unmount (20-03, 20-08) | UNCERTAIN (human item) | `runs/smoke-20-08.log` has all steps ok, `unmount: ok`, `daemon-exit: ok`, but on cbb1182. After it, b912aed changed `shutdown()` and the commit timer loop, and 7ff8c3c changed the `DirtyConfig` default. Their unit tests pass on HEAD (`no_timed_commit_after_shutdown`, `the_timer_does_not_commit_after_shutdown`, `a_small_budget_alone_gives_a_limit_of_the_budget`). |

**Score:** 4/5 truths verified: all four ROADMAP criteria; truth 5 is a human item (0 present, behavior-unverified).

### Judgment: does the S7 evidence still cover criterion 4?

Yes, as a judgment. It is not a measurement of HEAD.

The runs are on efc1e9b. After them came 4d8ac3d and e44d57d (empty-pass kick), 79f84e5 (WR-01, stuck pass ends the wait), c0d10d0 (WR-02 back-off, WR-03 test), 7ff8c3c (WR-04 CLI default), b912aed and db225bb (IN-01 commit timer). The test is: can one of these change the path that the S7 load takes at the budget?

- `stress_harness.rs` did not change after efc1e9b. `CAP_MIB` and the assert are the measured code.
- The three stat lines have `flusher_errors=0`, `flusher_drops=0`, `flusher_skips=0`, `throttle_timeouts=0`, and steps equal published (128, 120, 118).
- The back-off (c0d10d0) starts only after a failed step. There were none.
- A stuck pass (79f84e5) needs a pass with no published step. At the budget every dirty cell is a candidate, and with no skip, drop or error every tried step published. So this exit did not occur in this load. If it occurs, the writer pays with steps of its own file. That is the P0 path, which 20-01 measured at 267 MiB at most, under the cap.
- The empty-pass fix stops only a kick below the budget. `max_dirty_mib=65` shows that the peak was at the budget, where a write always kicks and the throttle holds the total. The fix removes passes that found no candidate (98.6 % to 99.7 % of passes in these runs).
- The budget, `FLUSH_STEP`, `REBASE_STEP` and the dirty limit did not change. The CLI default change applies only to a budget below 32M.
- On HEAD, `dirty_total_stays_bounded_with_parallel_writers` passes: the dirty total of 4 parallel writers stays under the budget plus one write per writer plus one step.

Margin: the worst run is 287 MiB against 320 MiB, so 33 MiB. The three runs span 30 MiB (257 to 287). The margin is about one spread. P1 is 20 MiB above P0 (267). The 20-08 summary names one `MALLOC_ARENA_MAX=2` flusher run as an open check. A new S7 run on HEAD would make this a measurement and not a judgment. It is not required by this verification.

### Plan must-haves (evidence below the criteria)

| Plan | Truth (short) | Status | Evidence |
|------|---------------|--------|----------|
| 20-01 | S7 serialize knob and VmHWM; six P_old/P0 runs; CAP_MIB by a rule written before the runs; allow-list test; `pay_budget` own file only | VERIFIED | `runs/s7-wave0.log`, `s7-wave0-NOTES.md` (`CAP_MIB = 320`), `stress_harness.rs:1990`, `no_take_out.rs`. |
| 20-02 | Throttle before the gate; flusher pass with one step per gate hold; pay after the wait limit; kick at half the budget; Phase 19 path for replay and `handle_write_flags` | VERIFIED | `throttle_write`, `flush_pass`, `flush_step`, `handle_write_admitted`; `handle_write_flags` calls it with `Admit { pay: true }`; `oplog.rs:334` replay uses `handle_write`. Tests listed under criterion 2. |
| 20-03 | Stop and panic wake writers; flusher with the mount; throttle in FUSE `write`; `shutdown` order | VERIFIED | `fuse_callbacks.rs:65` calls `shutdown()`; `shutdown` marks shut down, calls `stop_flusher`, then takes the gate. Tests `stop_wakes_throttled_writers`, `a_flusher_panic_wakes_throttled_writers`, `shutdown_with_a_parked_flusher_step_and_a_queued_commit_ends`, two flusher unit tests in `flusher.rs`. |
| 20-03 | A Docker Linux mount with the flusher unmounts, and the daemon exits within 10 s | UNCERTAIN (human item) | `runs/smoke-20-08.log` shows `unmount: ok`, `daemon-exit: ok`, but on cbb1182. `shutdown()` and the commit timer changed after it (b912aed). |
| 20-04 | All writers throttled while a commit waits (two forms); no wait for a step token | VERIFIED | Criterion 3 tests and `the_flusher_skips_a_cell_with_a_step_in_flight`. |
| 20-05 | Idle, expire, hot rule, quiet first, per-file limit trigger, test clock | VERIFIED | `flush_pass` code; tests `an_idle_file_is_flushed`, `an_expired_file_is_flushed`, `the_flusher_steps_the_quiet_file_first`, `a_hot_file_is_left_to_its_own_writer`, `a_quiet_file_over_its_limit_is_flushed`, `at_the_budget_a_hot_file_is_stepped`. |
| 20-06 | Four mount options with checks; values reach the file system before the replay | VERIFIED | `cli.rs:106-123`, `Cmd::dirty_config`; `mount.rs:269` `dirty.check()` before any side effect; `mount.rs:362` `set_dirty_config` before the replay. `a_small_budget_alone_gives_a_limit_of_the_budget` passes on HEAD. |
| 20-07 | Dirty bound with 4 writers; S7 flusher form; three Docker runs under CAP_MIB; constant not changed | VERIFIED | See criterion 4. |
| 20-08 | Research doc `## Phase 20 result`; README options; ARCHITECTURE text; requirement marks; public-text rules | VERIFIED | `docs/research/multithreaded-request-handling.md:325`; `README.md:106-117`; `.planning/research/ARCHITECTURE.md:22`, `:109`; `REQUIREMENTS.md` LOCK-02 and LOCK-06 marked; added public text follows the 20-08 text rules (no home path, no forbidden words). |
| 20-08 | Phase gate green, including a Docker smoke mount on the final tree | UNCERTAIN (human item) | Workspace and deadlock-detect gates were green per the review fix status. The smoke is on cbb1182, not on the final tree. |

### Required Artifacts

| Artifact | Expected | Status | Details |
|----------|----------|--------|---------|
| `crates/slicefs-cli/src/flusher.rs` | flusher loop, pass, spawn | VERIFIED | `run_flusher`, `spawn_flusher` (Weak), `flush_pass`, `Running` drop guard. Wired from `mount.rs` through `start_flusher`. |
| `crates/slicefs-cli/src/filesystem.rs` | throttle, pay, stop, shutdown, flush step, config | VERIFIED | `throttle_write`, `pay_budget`, `stop_flusher`, `shutdown`, `flush_step`, `DirtyConfig`, `set_dirty_config`. |
| `crates/slicefs-cli/src/handlers.rs` | `handle_write_admitted` | VERIFIED | Called from FUSE `write`. |
| `crates/slicefs-cli/tests/no_take_out.rs` | take-out check | VERIFIED | 4 tests pass. |
| `crates/slicefs-cli/tests/flusher_tests.rs` | criteria 2 and 3 tests | VERIFIED | 26 tests pass. |
| `crates/slicefs-cli/tests/stress_harness.rs` | S7 flusher form, cap assert | VERIFIED | `S7_FLUSHER`, `CAP_MIB = 320`, assert on VmHWM. |
| `runs/s7-flusher.log` | P1 runs | VERIFIED | Three runs, exit 0, on efc1e9b. |

### Key Link Verification

| From | To | Via | Status |
|------|----|-----|--------|
| FUSE `write` | `throttle_write` | call before `gate_shared()` | WIRED (`fuse_callbacks.rs:437`, `:440`) |
| FUSE `destroy` | `shutdown` | stop and join the flusher before the gate | WIRED (`fuse_callbacks.rs:65`) |
| `mount.rs` | `start_flusher` | after the replay, not on a snapshot mount | WIRED (`mount.rs:384-386`; stop at `:482`) |
| `handle_write_admitted` | `pay_budget` | `admit.pay` | WIRED (`write_in_cell`: `if admit.pay`) |
| flusher | `free_dirty` | `flush_step`, one gate hold | WIRED |
| `main.rs` / `cli.rs` | `run_mount` | `DirtyConfig`, `check()` first | WIRED |
| `stress_harness.rs` | `s7-wave0-NOTES.md` | `CAP_MIB = 320` in both | WIRED |

### Behavioral Spot-Checks (run on HEAD 23c61be)

| Behavior | Command | Result | Status |
|----------|---------|--------|--------|
| Take-out check and flusher tests | `cargo test -p slicefs-cli --test flusher_tests --test no_take_out` | flusher_tests 26 passed (7.47 s); no_take_out 4 passed | PASS |
| Flusher tests with the deadlock detector | `cargo test -p slicefs-cli --features deadlock-detect --test flusher_tests` | 26 passed (7.07 s) | PASS |
| Review fixes IN-01 and WR-04 | `cargo test -p slicefs-cli --lib -- no_timed_commit_after_shutdown the_timer_does_not_commit_after_shutdown a_small_budget_alone_gives_a_limit_of_the_budget` | 3 passed | PASS |
| Docker S7 and Docker smoke | not run (out of scope for this verifier) | logs on efc1e9b and cbb1182 | SKIP |

### Probe Execution

No phase probe scripts (`scripts/*/tests/probe-*.sh`) are declared. Not applicable.

### Requirements Coverage

| Requirement | Source plans | Description | Status | Evidence |
|-------------|--------------|-------------|--------|----------|
| LOCK-02 | 20-01, 20-08 | Per-file state stays in its table behind its own lock; no request takes another file's state out of `open_files`, `keep_budget` included | SATISFIED | Criterion 1. |
| LOCK-06 | 20-02 to 20-08 | Shared dirty budget safe with N writers; flusher; writer sleeps before any lock; every wait has a time limit; all-throttled-while-commit test; peak RSS under the set limit | SATISFIED | Criteria 2, 3 and 4. The RSS part rests on the judgment above. |

No orphaned requirement: REQUIREMENTS.md maps only LOCK-02 and LOCK-06 to Phase 20.

### Phase 19 items that name Phase 20 as owner

| Item | Needed by a Phase 20 criterion? | Verdict |
|------|-------------------------------|---------|
| Span probe of the long `file_cell` holds (3.6-13 ms) | No. It is a measurement. It does not hold dirty memory and adds no wait. | Properly deferred. |
| The wait of other writers behind a writer at the hard cap (performance effect) | No. The wait goes through `wait_step`, which has `STEP_WAIT` slices and `STEP_WAIT_MAX` 120 s, so criterion 2 holds. Only its cost is not measured. | Properly deferred. |
| Retry of a cell kept by a failed last release | No. The flusher steps such a cell like any other (`flush_step`), so its dirty data leaves the total. Only the retire is not done. CONTEXT made the retry optional ("if it is cheap"). A failed step now backs off (c0d10d0). | Properly deferred. |

All three still say "Owner: Phase 20" in `.planning/phases/19-fine-grained-locks/deferred-items.md`, and line 18 of that file (the flusher and throttle item) is still in "Open" although Phase 20 delivered it. They need a new owner phase (WARNING, not a gap).

### Anti-Patterns Found

| File | Line | Pattern | Severity | Impact |
|------|------|---------|----------|--------|
| changed source files | - | `TBD`, `FIXME`, `XXX`, `TODO`, `todo!`, `unimplemented!` in added lines | none found | - |
| `tests/no_take_out.rs` | 58 | removal check matches one literal text | Info | Review IN-03, accepted. Another binding of the map could pass the check. The debug rank rule covers a second cell lock. |
| `.planning/phases/19-fine-grained-locks/deferred-items.md` | 16, 18, 22, 23 | items say "Owner: Phase 20" and stay open | Warning | No owner after this phase. |
| `stress_harness` default run | - | time near or above the 10 s plan limit under host load | Info | In `deferred-items.md`; not a test assert. |
| `.planning/ROADMAP.md` | 189 | Phase 20 box not ticked | Info | For the orchestrator at phase close. |
| `.planning/research/ARCHITECTURE.md` | 100 | still says that the take-out pattern is in `filesystem.rs` | Info | Stale text against the 20-08 truth on ARCHITECTURE text; lines 22 and 109 are correct. |

### Human Verification Required

### 1. Docker smoke mount on HEAD

**Test:** Run the Docker Linux mount smoke (`mt-linux.sh`, the same script as `runs/smoke-20-08.log`) on HEAD 23c61be or later, on a clean tree, with rustc 1.100.0.
**Expected:** All steps `ok`, including `unmount` and `daemon-exit`; `exit = 0`; the daemon exits within 10 s.
**Why human:** The only mount evidence is on cbb1182. After it, b912aed changed `shutdown()` and the commit timer loop, and 7ff8c3c changed the `DirtyConfig` default in the CLI. Both run in each mount and unmount. Their unit tests pass on HEAD, but a mount was not run. This verifier does not run Docker.

Optional, not required: three `S7_FLUSHER=1` runs on HEAD (and one with `MALLOC_ARENA_MAX=2`) would turn the criterion 4 judgment into a measurement and show the cause of the 20 MiB between P0 and P1.

### Gaps Summary

No gap. The four ROADMAP criteria and LOCK-02 and LOCK-06 have evidence in the code and in tests that pass on HEAD. Criterion 4 rests on Docker runs of efc1e9b; the later commits do not reach the path of that load at the budget, so the evidence still covers it. The only open point is the Docker smoke mount on the final tree, which a person must run.

---

_Verified: 2026-11-23T14:51:21Z_
_Verifier: Claude (gsd-verifier)_

## Human item closed (orchestrator, 2026-11-23)

The Docker smoke mount ran on d061ba2 (the same source code as the final Phase 20 code; later commits change only planning text). `rustc -V` in the image equals the `RUSTC` line of the Wave 0 notes. All file steps ok, `unmount: ok`, `daemon-exit: ok`, exit 0. Log: `runs/smoke-final.log`. Status changed from human_needed to passed.
