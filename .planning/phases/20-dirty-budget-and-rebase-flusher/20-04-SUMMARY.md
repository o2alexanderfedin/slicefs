---
phase: 20-dirty-budget-and-rebase-flusher
plan: 04
subsystem: concurrency
status: complete
tags: [lock-06, flusher, throttle, deadlock, commit]

requires:
  - phase: 20-dirty-budget-and-rebase-flusher
    plan: 03
    provides: "test_gate_exclusive_waiters, the stop guard, run_flusher, flush_pass, throttle_write, flusher_tests.rs helpers"
provides:
  - "all_writers_throttled_while_a_commit_waits_flusher_paused (ROADMAP criterion 3, form A)"
  - "all_writers_throttled_while_a_commit_waits_flusher_in_a_step (ROADMAP criterion 3, form B)"
  - "the_flusher_skips_a_cell_with_a_step_in_flight (no wait for a step token in a flusher step)"
  - "flusher_tests.rs helpers: park_first_rebase (30 s park limit), throttle_setup, spawn_writers, all_writers_ok, check_after_writers"
affects: [20-05, 20-07, 20-08]

estimate:
  tokens: 80000
  tasks: 2
actuals:
  tokens: 2857      # chars/4 over the added lines of the diff 9ba2973..df5de5f
  tasks: 2
  commits: 2

plan_head_before: 9ba29739efdb7158b76a1e73f9808adf91da03f1
plan_head_after: df5de5f8bd714876e4174149fb74ec1d8f7c2b9b
commits: 2

tech-stack:
  added: []
  patterns:
    - "Guard mutation for a lock-order rule: keep a gate hold where the rule says to release it, and the test fails at its time-limited assert"
    - "Keep the go sender of a parked hook in the scope closure, so a failed assert lets the park go before the scope join"

key-files:
  created: []
  modified:
    - crates/slicefs-cli/tests/flusher_tests.rs

decisions:
  - "Task 2 files have X with 60 pieces (about 1.88 MiB) and Y with 32 pieces (1 MiB); the writer adds 256 KiB at 4 MiB"
  - "The park of the rebase hook waits at most 30 s (as in the 20-03 shutdown test), not 10 s as in lock_free_io.rs"

metrics:
  duration: "about 60 min (two workspace runs of about 13 min each)"
  completed: 2026-11-23
---

# Phase 20 Plan 04: Criterion 3, no deadlock with throttled writers Summary

Three tests in `tests/flusher_tests.rs` show that the gate, the budget and the flusher cannot make a wait cycle. A timed commit ends while all four writers wait in the throttle, with the flusher paused and with the flusher parked inside a step that holds the gate shared. A flusher pass skips a file whose step token is set and steps another file. No source file changed: the rules of plans 20-02 and 20-03 hold.

## Tasks

| Task | Name | Commit | Files |
|---|---|---|---|
| 1 | All writers throttled while a commit waits (flusher paused; flusher parked in a step) | 4d97a7a | tests/flusher_tests.rs |
| 2 | A flusher pass skips a cell with a step in flight | df5de5f | tests/flusher_tests.rs |

## What was built

- `throttle_setup`: a logged file system (`new_fs_with`, `WalConfig::PerOp`), dirty limit 2 MiB, four overlay files of 4 MiB with 35 dirty pieces each (about 1.1 MiB each, about 4.4 MiB in total), filled with the default budget. Then the flusher is paused, the pass interval is 20 ms and the throttle wait limit is 30 s. The test lowers the budget to 4 MiB after the flusher runs.
- `spawn_writers`: one scoped thread per file, 8 throttled writes each on clean slots 40 to 47. Each thread sends its results at the end. `all_writers_ok` receives them with one time limit for all.
- `check_after_writers`: the files are compared with their models with the handles open, after release, and after `drop(fs)` and `reload_and_replay`.
- Form A (`..._flusher_paused`): the four writers wait in the throttle (5 s limit). A thread runs `timed_commit`; it returns Ok within 10 s, and `test_throttled_now()` is still 4. Then the flusher is unpaused, and all writers return Ok within 10 s. The total is below the budget plus 4 x (32 KiB + `EXTENT_OVERHEAD`).
- Form B (`..._flusher_in_a_step`): a rebase hook parks its first call. The hook is set after the fill. The writers wait in the throttle and do not pay, so the first rebase is a flusher step. After the unpause the park is reached (5 s). A thread runs `timed_commit`. The test waits until `test_gate_exclusive_waiters()` is 1 (10 s) and asserts that the commit has not returned. Then it lets the park go. The commit returns Ok within 10 s, all writers return Ok within 10 s, and the flusher stats show at least one published step.
- Task 2 (`the_flusher_skips_a_cell_with_a_step_in_flight`): files of 8 MiB, limit 2 MiB. X has 60 dirty pieces in its first half (between 1.5 MiB and 2 MiB, asserted), Y has 32 pieces. The budget is then 4 MiB. A writer writes 256 KiB at 4 MiB in the clean second half of X. X passes its limit, and the writer's own step on X parks in the hook (the writer holds the gate shared and the step token of X). A helper thread runs `flusher::flush_pass`. It returns within 5 s while the writer has not returned. The stats show at least one skip, Y's cost fell, and X's cost is the same as at the park. Then the park goes, the write returns Ok, and the contents match with the handles open and after release. No flusher thread runs in this test.

## Red and green runs

The rules hold on the committed code, so each red run is a named guard mutation of the source. No mutation was committed. Each source file was copied before the mutation and restored from the copy after it. `git diff -- crates/slicefs-cli/src` was empty after each restore.

| Test | Guard mutation | Failed at | Run time |
|---|---|---|---|
| `all_writers_throttled_while_a_commit_waits_flusher_paused` | `run_flusher` takes `gate_shared()` before the pause check and keeps it while paused; it releases the hold only just before a pass | line 752, `the commit returns within 10 s: Timeout` | test 10.66 s (command 30 s with the build) |
| `all_writers_throttled_while_a_commit_waits_flusher_in_a_step` | `flush_step` gets the hold from `run_flusher` (`flush_pass_held` with an `Option<GateGuard>` that `run_flusher` owns); it takes the gate only once and never releases it, so the hold of the first step stays across the pass wait and into the next pass | line 803, `the commit returns within 10 s: Timeout` | test 10.97 s (command 38 s with the build) |
| `the_flusher_skips_a_cell_with_a_step_in_flight` | `flush_step` calls `g.wait_step()` when the step token is set, before its skip checks (the retired, transient and cap-writer skips stay) | line 851, `the pass returns within 5 s: Timeout` | test 5.64 s (command 32 s with the build) |

Each mutation run failed at its named assert. None stopped at the watchdog, and no flusher panic or lock-rank panic was in the output. In each run the stop guard (forms A and B) or the drop of the go sender (Task 2) ended the scope.

Green:
- Task 1 tree: `cargo test -p slicefs-cli --test flusher_tests all_writers_throttled_while_a_commit_waits -- --nocapture`: 2 passed in 1.74 s, no watchdog line. `cargo test -p slicefs-cli --features deadlock-detect --test flusher_tests`: 13 passed, exit 0, no "deadlock detected" or "lock rank violation". `cargo test --workspace` exited 0 (69 `test result: ok` lines, 775 s) before the 4d97a7a commit.
- Task 2 tree: `--test flusher_tests the_flusher_skips_a_cell_with_a_step_in_flight -- --nocapture`: 1 passed in 0.70 s. `--test flusher_tests --test lock_free_io --test no_take_out`: 14, 33 and 4 passed, exit 0. `--features deadlock-detect --test flusher_tests`: 14 passed, exit 0. `cargo test --workspace` exited 0 (69 `test result: ok` lines, 767 s) before the df5de5f commit.

## Acceptance checks

- `grep -c 'fn all_writers_throttled_while_a_commit_waits_flusher_paused' crates/slicefs-cli/tests/flusher_tests.rs` = 1
- `grep -c 'fn all_writers_throttled_while_a_commit_waits_flusher_in_a_step' crates/slicefs-cli/tests/flusher_tests.rs` = 1
- `grep -c 'fn the_flusher_skips_a_cell_with_a_step_in_flight' crates/slicefs-cli/tests/flusher_tests.rs` = 1
- The three guard mutation runs are recorded above. Each failed at an assert, not at the watchdog.
- `cargo test --workspace` exited 0 before each commit.

## Deviations from Plan

### Auto-fixed Issues

None.

### Other deviations

**1. No separate RED commit.** The project rule is to commit only when `cargo test --workspace` passes. The red runs are guard mutations, as the plan says, and are recorded above.

**2. Park limit 30 s.** The plan says to copy `parker_at` from `lock_free_io.rs` (10 s per park). The new helper `park_first_rebase` waits at most 30 s, as the 20-03 shutdown test does. Form B has up to 15 s of waits between the park and the go (5 s, then 10 s for the queued commit), so 10 s could let the park go early on a slow machine.

**3. Form A mutation in `run_flusher`.** `flusher_loop` has no file system reference, so the mutation copied the loop into `run_flusher` and took the gate there. It is the plan's mutation: the hold is taken before the pause check and is kept while the flusher is paused.

## Open points

- **State tool.** `state.advance-plan` cannot read the position (the same as in 20-01 to 20-03). The position line in STATE.md was set by hand.
- **Requirements.** LOCK-06 is in the plan frontmatter, but it is not marked complete (orchestrator rule). REQUIREMENTS.md did not change.
- **Commit timer at unmount.** This stays deferred (`deferred-items.md`). This plan did not change it.

## Threat model check

- T-20-SC: no new crate.
- T-20-12: the two tests of criterion 3, each with a guard mutation that failed at its time-limited assert, and a 60 s abort watchdog.
- T-20-13: `the_flusher_skips_a_cell_with_a_step_in_flight` with its guard mutation.
- T-20-14: the content checks with the handles open, after release and after a replay of the log (forms A and B).

## Known Stubs

None.

## Self-Check: PASSED

- FOUND: crates/slicefs-cli/tests/flusher_tests.rs with the three test functions
- FOUND: 4d97a7a and df5de5f in HEAD
- `commits: 2` is measured with `git rev-list --count` from `plan_head_before`. The docs commit (this file, STATE.md, ROADMAP.md) comes after it, as in 20-02 and 20-03.
