---
phase: 20-dirty-budget-and-rebase-flusher
plan: 05
subsystem: concurrency
status: complete
tags: [lock-06, flusher, idle, expire, hot-rule, tracer]

requires:
  - phase: 20-dirty-budget-and-rebase-flusher
    plan: 04
    provides: "the criterion 3 tests; the_flusher_skips_a_cell_with_a_step_in_flight and its guard mutation"
provides:
  - "FileCell::last_write and FileCell::dirty_since (atomics, flusher clock ms, 0 = none)"
  - "A flusher clock per file system: FlushCtl::now_ms (process epoch + test offset), test_advance_clock"
  - "FlushCtl::idle_ms (5 s), expire_ms (30 s), hot_ms (300 ms); hooks test_set_dirty_idle, test_set_dirty_expire, test_set_hot_window"
  - "flush_pass: budget-full mode with the hot rule, the quiet-first pick and the per-file limit trigger; then the idle and expire mode"
  - "SliceFsFilesystem::dirty_limit_now; FlushTarget::last_write and dirty_since"
affects: [20-06, 20-07]

estimate:
  tokens: 110000
  tasks: 2
actuals:
  tokens: 4429      # chars/4 over the added lines of the diff ad1d31d..63be447
  tasks: 2
  commits: 2

plan_head_before: ad1d31d75fd1da7c99a0c546b89f2be9c84079b6
plan_head_after: 63be44712bee57c906a2a40a94e1c913dee33cfe
commits: 2

tech-stack:
  added: []
  patterns:
    - "Test clock per file system: tests that run in parallel in one process do not move the clock of each other"
    - "One candidate filter per budget-full step, with the total read again: not tried, (at the budget or quiet), (at half the budget or at the own limit)"
    - "A cell that the hot rule filters out is not a step and not a skip"

key-files:
  created: []
  modified:
    - crates/slicefs-cli/src/filesystem.rs
    - crates/slicefs-cli/src/flusher.rs
    - crates/slicefs-cli/tests/flusher_tests.rs

decisions:
  - "Planner choices kept: hot window 300 ms; one FLUSH_STEP step per idle or expired cell per pass; a stream with pending writes is switched only in the idle or expire mode, or in the budget-full mode when no overlay candidate is left"
  - "The clock offset is per file system (a FlushCtl field), not a process-wide static; the epoch (OnceLock<Instant>) is process-wide"
  - "The idle and expire mode never steps a hot cell; at or above the budget the budget-full mode already allows hot cells"
  - "Budget-full sort key: overlays first, then quiet cells, then the largest dirty cost"

metrics:
  duration: "about 39 min (08:35 to 09:13 UTC; two workspace runs)"
  completed: 2026-11-23
---

# Phase 20 Plan 05: Flusher policy (idle, maximum age, buffer full, hot rule) Summary

The flusher now acts on the three triggers of the owner's rule. It stores the dirty data of a file with no write for the idle time (5 s), and of a file whose oldest dirty data reached the maximum age (30 s). When the buffer is full, it steps the largest quiet file first. Below the budget it never steps a hot file (a write in the last 300 ms). The writer of a hot file keeps it under the hard cap with its own limit steps. At or above the budget a hot file can be stepped, because its writers wait in the throttle and the step publishes. A quiet file at or above its own limit is stepped also when the total is below half the budget.

## Tasks

| Task | Name | Commit | Files |
|---|---|---|---|
| 1 | Per-cell write and dirty times, a test clock, the idle and expire triggers | 139a837 | filesystem.rs, flusher.rs, tests/flusher_tests.rs |
| 2 | Hot rule, largest quiet file first, the per-file limit trigger | 63be447 | filesystem.rs, flusher.rs, tests/flusher_tests.rs |

## What was built

- **Clock.** `FlushCtl::now_ms()` gives the ms since a process epoch (`OnceLock<Instant>`), plus 1, plus a test offset. The value is always at least 1, because 0 means "none" in the cell times. The offset is a field of `FlushCtl`, so each file system has its own offset. `test_advance_clock(ms)` adds to it. In production it is 0. Every age uses `saturating_sub`, because a writer can store `last_write` after the flusher read the clock.
- **Cell times.** `write_in_cell` stores `FileCell::last_write` once, right after the apply loop when the write is Ok. This is before the own limit step and the hard-cap loop. `CellGuard::sync_dirty` stores `dirty_since` when the cost goes from 0 to above 0, and stores 0 when the cost goes to 0 (the Linux `dirtied_when` rule). It reads the clock only at that change. `CellGuard` gets a reference to the flush control block in `lock_cell`, its only construction site.
- **Limits.** `FlushCtl::idle_ms` (default 5000), `expire_ms` (default 30000) and `hot_ms` (default 300), with the hooks `test_set_dirty_idle`, `test_set_dirty_expire` and `test_set_hot_window`. Plan 20-06 sets the idle and expire values from the mount options.
- **`flush_pass`, budget-full mode.** Before each step the pass reads the total, the budget, the limit and the clock again. A candidate is not in the skip set of the pass, is quiet or the total is at or above the budget, and the total is at or above half the budget or the cell is at or above its own limit. The pick is the maximum of (overlay, quiet, dirty cost). With no candidate the mode ends. A stop ends the whole pass.
- **`flush_pass`, idle and expire mode.** Then, for each dirty cell that is not in the skip set and is not hot: one step if its last write is at least the idle time old, or if `dirty_since` is not 0 and is at least the maximum age old. The step helper `step()` is shared by both modes and counts the result as before.
- **Doc comment.** The doc of `flush_pass` says why the rule is "never" and not "prefer quiet". At the default values the per-file limit is half the budget, so one busy file at its limit keeps the budget-full mode on. Its steps would be dropped, and while the flusher holds the step token the writer's own limit step is skipped.

## Red and green runs

Each new test failed before its code, at an assert, with no watchdog abort. No red tree was committed (project rule).

Task 1, on the stubs (clock, the two times and the idle and expire hooks stored values that `flush_pass` did not read):

| Test | Failed at |
|---|---|
| `an_idle_file_is_flushed` | line 899, `F's cost is 0 after the idle time` (left 525312, `PassStats` all 0) |
| `an_expired_file_is_flushed` | line 921, `a pass lowers F's cost to 0 at the age of 35 s` (left 65664, `PassStats` all 0) |

Task 2, on the Task 1 tree (139a837) plus the `test_set_hot_window` hook as a stub (stored, not read):

| Test | Failed at |
|---|---|
| `the_flusher_steps_the_quiet_file_first` | line 968, `H's cost is the same` (left 0: largest first stepped H) |
| `a_hot_file_is_left_to_its_own_writer` | line 996, `5 passes made 0 steps on the hot file` (left 1) |
| `a_quiet_file_over_its_limit_is_flushed` | line 1044, `Q's cost 3151872 is 2 MiB or less` (`PassStats` all 0: no limit trigger) |
| `at_the_budget_a_hot_file_is_stepped` | passed, as the plan says |

Guard mutations (not committed). Each source file was copied to a scratch directory before the mutation and restored from the copy after it. `cmp` showed the restored file equal to the copy.

| Test | Guard mutation | Failed at |
|---|---|---|
| `at_the_budget_a_hot_file_is_stepped` | the candidate filter `at_budget \|\| !hot(t, now)` became `!hot(t, now)` (no at-budget exception) | line 1065, `H1's cost fell` (`PassStats` all 0), 0.92 s |
| `the_flusher_skips_a_cell_with_a_step_in_flight` (the 20-04 mutation, run again) | `flush_step` calls `g.wait_step()` when the step token is set, before its skip checks | line 867, `the pass returns within 5 s: Timeout`, 5.49 s |

Green:
- Task 1 tree: `cargo test -p slicefs-cli --test flusher_tests -- --nocapture`: 16 passed. `cargo test --workspace` exited 0 (69 `test result: ok` lines, 0 failed) before the 139a837 commit. Tracer gate: the Task 1 verify ran again on the committed tree: 16 passed, both new tests `ok`.
- Task 2 tree: `cargo test -p slicefs-cli --test flusher_tests`: 20 passed in 3.55 s; the lines for `the_flusher_lowers_the_total`, `the_flusher_skips_a_cell_with_a_step_in_flight` and the four new tests each say `ok`.
- `cargo test -p slicefs-cli --test dirty_budget_tests --test rebase_step_tests --test lock_free_io --test memory_bound_tests --test stress_harness`: exit 0 (1, 4, 33, 1 and 6 passed). In that run `stress_harness` finished in 10.25 s, right after the other test binaries. Two warm runs of `stress_harness` alone: 9.81 s and 9.38 s, at a load average of about 19. In the two workspace runs it finished in 10.45 s (Task 1 tree) and 9.84 s (Task 2 tree). The harness starts no flusher; the only new work on its write path is one clock read per write.
- `cargo test --workspace` exited 0 (69 `test result: ok` lines, 0 failed) before the 63be447 commit.

## Acceptance checks

- `grep -c 'fn test_advance_clock' crates/slicefs-cli/src/filesystem.rs` = 1
- `grep -c 'dirty_since' crates/slicefs-cli/src/filesystem.rs` = 6
- `grep -c 'fn an_expired_file_is_flushed' crates/slicefs-cli/tests/flusher_tests.rs` = 1
- `grep -c 'fn test_set_hot_window' crates/slicefs-cli/src/filesystem.rs` = 1
- `grep -c 'fn a_hot_file_is_left_to_its_own_writer' crates/slicefs-cli/tests/flusher_tests.rs` = 1
- `grep -c 'fn the_flusher_steps_the_quiet_file_first' crates/slicefs-cli/tests/flusher_tests.rs` = 1
- `stress_harness.rs` did not change.

## Deviations from Plan

### Auto-fixed Issues

None.

### Other deviations

**1. No separate RED commit.** The project rule is to commit only when `cargo test --workspace` passes. Each task has one commit with its tests and its code. The red runs are recorded above.

**2. The clock offset is per file system.** The plan says "a test offset in an atomic". The atomic is a field of `FlushCtl`, not a process-wide static, and `test_advance_clock` is a method of the file system. Reason: the tests of `flusher_tests` run in parallel in one process. With one static offset, the 120 s move of one test can fall between the fill and the pass of another test.

**3. More time limits set to 1 h in the tests.** The plan names only some of them. In each new test, every time limit that the test does not use is 1 h (as the plan rule says). In `a_quiet_file_over_its_limit_is_flushed` the expire time is also 1 h: the 120 s clock move passes the default 30 s, and the red run on the Task 1 tree would then be green. The hot window is 60 s in `a_hot_file_is_left_to_its_own_writer` and `at_the_budget_a_hot_file_is_stepped`, so a loaded machine cannot end the hot state during the test.

**4. The three older one-pass tests.** `the_flusher_lowers_the_total`, `a_pass_ends_when_no_step_frees` and `the_flusher_skips_a_cell_with_a_step_in_flight` now set idle and expire to 1 h and move the clock by 1 s (past the hot window) before the pass. In the skip test this occurs after the park is reached. The idle and expire limits are 1 h so that only the budget-full mode acts; `the_flusher_lowers_the_total` asserts that B's cost does not change.

**5. The hot-window hook landed in Task 2.** The plan lists it with the Task 1 stubs. It was added as a stub at the start of Task 2, before the Task 2 red run, so the Task 1 commit has no hook that nothing reads.

**6. A stop now ends the whole pass.** Before, a stop only ended the budget-full loop. With the second mode, the pass returns at a stop, so the idle and expire mode does not run after it.

## Open points

- **`stress_harness` time.** Five runs on this plan's code: 10.45 s (in the Task 1 workspace run), 9.84 s (in the Task 2 workspace run), 10.25 s (in the Task 2 verify set, after four other test binaries), and 9.81 s and 9.38 s (alone, warm, at a load average of about 19). Two runs were above the 10 s limit of the plan, and both ran together with other test binaries. The harness starts no flusher; the only new work on its write path is one clock read per write. The 20-02 times (9.05 s and 9.00 s) came from another session at a different load, so these runs do not show whether the load or the change caused the difference. The margin is small.
- **Empty passes from the kick (for a decision).** The plan says that with no quiet candidate "the flusher sleeps until the next pass". `throttle_write` kicks the flusher on each write that finds the total at or above half the budget. At the default values one busy file at its limit is such a case, so each of its writes starts one pass that finds no candidate (two scans of the cell map and some takes of the flags lock). Before this plan the kick led to a step; now it leads to nothing. This is cheap and correct, but the S7 run of 20-07 will show a high `passes` count. Options: accept it, or do not kick when the last pass found no candidate.
- **Requirements.** LOCK-06 is in the plan frontmatter, but it is not marked complete (orchestrator rule). REQUIREMENTS.md did not change.
- **Commit timer at unmount.** This stays deferred (`deferred-items.md`). This plan did not change it.
- **A test of the stream switch in the idle or expire mode.** The idle and expire mode can switch a stream with pending writes to an overlay (through `free_dirty`). No test of this plan covers that path; the plan does not ask for one.

## Threat model check

- T-20-SC: no new crate.
- T-20-15: the hot rule below the budget; `a_hot_file_is_left_to_its_own_writer` checks 0 steps and 0 drops over 5 passes and one expired pass, then that the write that passes the limit runs its own step and that H stays at or below the hard cap.
- T-20-16: the idle and expire triggers; `an_idle_file_is_flushed` and `an_expired_file_is_flushed` on the test clock.
- T-20-17: the step is `free_dirty` with the Phase 19 generation check; every new test compares each file with its block-version model with the handle open and after release.

## Known Stubs

None.

## Self-Check: PASSED

- FOUND: crates/slicefs-cli/src/filesystem.rs (`fn test_advance_clock`, `fn test_set_hot_window`), crates/slicefs-cli/src/flusher.rs (`dirty_since`), crates/slicefs-cli/tests/flusher_tests.rs (the six new tests)
- FOUND: 139a837 and 63be447 in HEAD
- `commits: 2` is measured with `git rev-list --count` from `plan_head_before`. The docs commit (this file, STATE.md, ROADMAP.md) comes after it, as in 20-02 to 20-04.
