---
phase: 20-dirty-budget-and-rebase-flusher
plan: 02
subsystem: concurrency
status: complete
tags: [lock-06, lock-02, flusher, throttle, tracer]

requires:
  - phase: 20-dirty-budget-and-rebase-flusher
    plan: 01
    provides: "pay_budget (own cell only), no_take_out.rs, CAP_MIB = 320 (uses W = FLUSH_STEP = 4 MiB)"
provides:
  - "SliceFsFilesystem::throttle_write -> Admit { pay }: the write admission before any lock"
  - "flusher.rs: run_flusher (loop) and flush_pass (one pass, the test entry), PassStats"
  - "SliceFsFilesystem::flush_step (gate shared, cell lock, free_dirty, release): the flusher step helper"
  - "FlushCtl (own Arc): flags stop/kicked/paused, condvars wake and freed, running, counters"
  - "FLUSH_STEP = 4 MiB; THROTTLE_WAIT 5 s, THROTTLE_SLICE 50 ms, FLUSH_PASS 100 ms"
  - "write_logged_admitted and handlers::handle_write_admitted; pay_budget runs only when admit.pay"
  - "test hooks: test_pause_flusher, test_set_throttle_wait, test_set_flush_pass, test_throttled_now, test_flusher_running, test_flusher_stats"
  - "tests/flusher_tests.rs helpers: block-version Model, throttled_write, wait_until, watchdog, StopGuard"
affects: [20-03, 20-04, 20-05, 20-06, 20-07]

estimate:
  tokens: 130000
  tasks: 2
actuals:
  tokens: 8865      # chars/4 over the added lines of the diff 5497e4d..ca74000
  tasks: 2
  commits: 2

plan_head_before: 5497e4dd4d86ba7355a123c3b90f821a7b13e7af
plan_head_after: ca740000871697fc0595473c978f86cee763dcee
commits: 2

tech-stack:
  added: []
  patterns:
    - "Admission before the gate: throttle_write returns an Admit value that goes down to write_in_cell"
    - "Flusher step = one gate_shared hold around one free_dirty on one locked cell; skip, never wait, on a step token or a writer at the cap"
    - "Lost-wakeup rule: the total falls in the guard drop, then the flusher takes and releases the flags lock and notifies; the writer checks the total under that lock"
    - "Test stop guard: on a panic it lifts the budget so a throttled writer leaves at its next slice"

key-files:
  created:
    - crates/slicefs-cli/src/flusher.rs
    - crates/slicefs-cli/tests/flusher_tests.rs
  modified:
    - crates/slicefs-cli/src/filesystem.rs
    - crates/slicefs-cli/src/handlers.rs
    - crates/slicefs-cli/src/lib.rs
    - crates/slicefs-cli/tests/no_take_out.rs

decisions:
  - "Planner values kept: FLUSH_STEP 4 MiB, THROTTLE_WAIT 5 s, THROTTLE_SLICE 50 ms, FLUSH_PASS 100 ms, background threshold budget / 2"
  - "The flusher step helper is SliceFsFilesystem::flush_step in filesystem.rs (it calls free_dirty); flusher.rs holds the pick and the loop"
  - "FileCell gets a stream: AtomicBool, set in CellGuard::sync_dirty, so the pick can put overlays before streams with no cell lock"
  - "handle_write_flags now calls handle_write_admitted with pay: true (same behavior, no copy of the body)"

metrics:
  duration: "about 50 min"
  completed: 2026-11-23
---

# Phase 20 Plan 02: Throttle and flusher tracer Summary

A writer over the shared dirty budget now waits in `throttle_write` before it takes any lock. A flusher pass frees memory with one `free_dirty` step of another file per shared gate hold, and the writer then goes on and does not pay. After the wait limit the writer writes and pays with steps of its own file. A write that finds the total at or above half the budget wakes the flusher at once.

## Tasks

| Task | Name | Commit | Files |
|---|---|---|---|
| 1 | Throttle before the gate, flusher pass with one step per gate hold, writer goes on without paying | 98ca73e | filesystem.rs, flusher.rs (new), lib.rs, handlers.rs, tests/flusher_tests.rs (new) |
| 2 | Pay after the wait limit; kick at half the budget; no cell removal in flusher.rs | ca74000 | filesystem.rs, tests/flusher_tests.rs, tests/no_take_out.rs |

## What was built

- `Admit { pay }`. `write_logged` calls `write_logged_admitted(..., Admit { pay: true })`, so replay, `test_write`, `handle_write` and `handle_write_flags` keep the Phase 19 path. In `write_in_cell`, `pay_budget` runs only when `admit.pay`. The own limit step and the hard-cap gate do not change.
- `throttle_write`: below the budget, `pay: false`. With no flusher running, `pay: true` at once. Else the writer waits on `FlushCtl::freed` in slices of 50 ms until the total is below the budget (`pay: false`) or the deadline passed (`pay: true`, a timeout is counted). A drop guard lowers `throttled_now` on every exit. A total at or above half the budget calls `kick_flusher` (no lock when no flusher runs).
- `run_flusher`: `require_gate(true)`, sets `running`, a drop guard clears it. Loop: wait on `wake` for the pass interval unless kicked or stopped (the first wait comes before the first pass); stop returns; `kicked` is cleared each turn; paused waits again; else `flush_pass`.
- `flush_pass`: while the total is at or above half the budget, pick the unskipped candidate with the largest `FileCell::dirty`, overlays before streams. `flush_step` takes `gate_shared()`, locks the cell, skips it if it is retired, transient, has a step token or a writer at the cap, else calls `free_dirty(&mut g, FLUSH_STEP)` and compares `dirty_cost` before and after under the lock. It drops the cell guard, then the gate. Then the pass calls `notify_freed()` (take and release the flags lock, `freed.notify_all()`). A step that freed nothing (skipped, dropped, failed) puts the cell in the skip set for the rest of the pass. An error is printed after the locks are released.
- `stop_flusher`: sets `stop` under the flags lock and notifies `wake`. As the plan says, plan 20-03 adds the writer wake, the join and the lock assert. The 20-02 throttle loop leaves only on "below the budget" or the deadline, and the running guard wakes nobody. These are the facts that the 20-03 red runs depend on.
- A dirty cell with no write handle is an ordinary candidate. `flush_step` never retires or removes a cell.

## Red and green runs

Each new test failed before its code, at an assert with a limit (no abort). There is no separate red commit (see Deviations).

Task 1, on the stubs (`throttle_write` returned `Admit { pay: true }` at once; `flush_pass` did nothing; the `run_flusher` loop was real):

| Test | Failed at |
|---|---|
| `a_writer_over_the_budget_waits_for_the_flusher` | line 219, `the writer waits in the throttle` (`wait_until` 5 s) |
| `the_flusher_lowers_the_total` | line 250, `the pass lowered the total below half the budget: PassStats { steps: 0, ... }` |
| `a_pass_ends_when_no_step_frees` | line 278, `the dropped steps were counted: PassStats { steps: 0, published: 0, drops: 0, ... }` |

Task 2, on the Task 1 tree (98ca73e plus the two new tests):

| Test | Failed at |
|---|---|
| `a_throttled_writer_writes_after_the_limit` | line 316, `the writer paid with a step of its own file` (the elapsed and timeouts asserts passed first; Task 1 counts timeouts) |
| `a_write_past_half_the_budget_kicks_the_flusher` | line 350, `the second write kicked the flusher` (`wait_until` 5 s) |

Guard mutation (not committed): a comment line `// fs.cells.remove(&cell)` added to the end of `flusher.rs` made `the_flusher_takes_no_cell_out` fail with `flusher.rs removes a cell at lines [136]`. The file was restored from a copy. `git diff` of `flusher.rs` was empty after it, and `grep -c 'cells.remove(' crates/slicefs-cli/src/flusher.rs` gives 0.

Green:
- Task 1 tree: `cargo test --workspace` exited 0 (69 `test result: ok` lines, 0 failed). After that run, only the doc comment of `flush_pass` in `flusher.rs` changed (a comment, no code); `flusher_tests` ran green again on that text before the 98ca73e commit. The other Task 1 verify set (`dirty_budget_tests`, `rebase_step_tests`, `write_log_replay_tests`, `lock_free_io`, `no_take_out`) passed unchanged.
- Tracer gate: the three Task 1 tests ran green again on the Task 1 tree (the Task 2 red run shows them `ok`).
- Task 2 tree (ca74000): `cargo test --workspace` exited 0 before the commit (69 `test result: ok` lines, 0 failed). `cargo test -p slicefs-cli --test flusher_tests --test no_take_out`: 5 and 4 passed (flusher_tests in 0.53 s). `stress_harness`: 6 passed, warm 9.05 s and 9.00 s (limit 10 s).
- Final tree (the docs commit, code the same as ca74000), each command with its own exit code and no pipe: `cargo check --workspace --all-targets` 0; `cargo check --workspace --all-targets --features slicefs-cli/lock-stats` 0; `cargo check -p slicefs-cli --all-targets --features deadlock-detect` 0 (no warning or error in slicefs-cli in all three); `cargo test -p slicefs-cli --features deadlock-detect --test stress_harness serialize_off_target` 0 (1 test passed). `cargo test -p slicefs-cli --features deadlock-detect --test flusher_tests`: 5 passed on ca74000.

## Acceptance checks

- `grep -c 'pub fn throttle_write' crates/slicefs-cli/src/filesystem.rs` = 1
- `grep -c 'pub fn flush_pass' crates/slicefs-cli/src/flusher.rs` = 1
- `free_dirty` in `flusher.rs` = 1 (in the doc of `flush_pass`). The call itself is in the step helper `SliceFsFilesystem::flush_step` in `filesystem.rs`.
- `grep -c 'pub fn handle_write_admitted' crates/slicefs-cli/src/handlers.rs` = 1
- `grep -c 'fn a_throttled_writer_writes_after_the_limit' crates/slicefs-cli/tests/flusher_tests.rs` = 1
- `grep -c 'cells.remove(' crates/slicefs-cli/src/flusher.rs` = 0

## Deviations from Plan

### Auto-fixed Issues

None.

### Other deviations

**1. No separate RED commit.** The project rule is to commit only when `cargo test --workspace` passes, so each task has one commit with the tests and the code. The red runs are recorded above (the same as in 20-01).

**2. Mode atomic on `FileCell`.** The plan asks the pick to put overlays before streams and to read atomics only. No atomic for the mode existed, so `FileCell::stream: AtomicBool` was added and is set in `CellGuard::sync_dirty`, with the cell locked.

**3. `handle_write_flags` delegates.** It now calls `handle_write_admitted(..., Admit { pay: true })`. The behavior is the same; the body is not copied.

**4. Test stop guard lifts the budget on a panic.** `StopGuard` calls `stop_flusher()`. When the thread panics, it also sets the budget to `usize::MAX`, so a throttled writer leaves at its next 50 ms slice and the scope join does not wait for the 30 s limit. The throttle loop itself has no stop exit in this plan (plan 20-03 adds it).

**5. `PassStats::steps` is the attempt count.** "At most one attempt per dirty cell" is checked as `st.steps <= 2` for two dirty files. One flusher attempt can make up to `REBASE_TRIES` (2) unlocked rebase tries inside `rebase_cell`; those are not flusher attempts.

## Open points

- **Stop exit of the throttle wait (project rule).** The rule says every new wait exits at stop or unmount. In this plan the throttle wait leaves only when the total is below the budget or at its deadline (at most the wait limit, 5 s by default). This is the plan's split: 20-03-PLAN line 100 says so, and its red runs need it. Plan 20-03 adds the stop exit and the writer wake. No mount starts a flusher before 20-03, so no daemon has this wait now.
- **No test tells `pay: false` from `pay: true` on the admitted path (for a decision).** In `a_writer_over_the_budget_waits_for_the_flusher` the total is already below the budget when the write runs, so `pay_budget` would also do nothing with `pay: true`, and `rebase == 0` holds in both cases. The test is as the plan wrote it. A test where a write is admitted with `pay: false` while the total is still above the budget would need a new hook; I did not add a sixth test.
- **State tool.** `state.advance-plan` declined with `plan_position_unreadable`: the "Current Position" section of STATE.md has `Plan: 20-02 complete (2 of 8); next 20-03`, which the tool does not parse. I set the position line by hand (as 20-01 did). The next executor will get the same message unless that section gets a `Current Plan: N` / `Total Plans in Phase: M` line.
- A pass goes on while each step frees bytes. With writers that add bytes as fast as the flusher frees them, a pass can last long (it checks `stop` before each step, so it ends at a stop). Plan 20-05 adds the hot rule, which removes busy files from the pick below the budget.
- `max_wait_ms` is counted, but no test of this plan reads it. Plan 20-07 reads it (`throttle_wait_max_ms`).

## Threat model check

- T-20-SC: no new crate.
- T-20-04: the wait has a deadline from the wait limit, in 50 ms slices; after the limit the writer writes and pays (`a_throttled_writer_writes_after_the_limit`).
- T-20-05: a cell that freed nothing is skipped for the rest of the pass (`a_pass_ends_when_no_step_frees`: published 0, drops at least 1, steps at most 2, the pass returned within 5 s).
- T-20-06: `flush_step` skips a cell with a step token or a writer at the cap; it never calls `wait_step`. Plan 20-04 tests it.
- T-20-07: the step is `free_dirty` (the Phase 19 generation check); every test compares the files with block-version models with the handles open and after release.

## Known Stubs

None. `stop_flusher` without the writer wake and the join is the plan's scope split (plan 20-03), not a stub.

## Self-Check: PASSED

- FOUND: crates/slicefs-cli/src/flusher.rs, crates/slicefs-cli/tests/flusher_tests.rs
- FOUND: 98ca73e and ca74000 in HEAD
