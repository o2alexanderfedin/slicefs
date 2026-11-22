---
phase: 19-fine-grained-locks
plan: 06
subsystem: concurrency
tags: [lock-01, lock-05, d-02, d-13, overlay, rebase, generation, hard-cap]

requires:
  - phase: 19-fine-grained-locks
    provides: "19-05 FileCell per inode, CellGuard dirty sync, keep_budget on the writer's own cell"
provides:
  - "Overlay generation (process-wide counter), copy_step, step_ranges, discard on the same selection as take"
  - "DictMetadataStore::prepare_manifest (hash + put, no lock) and set_manifest_digest (memory)"
  - "Three-phase rebase step: Phase A copy under the cell lock, Phase B splice + manifest node + hook with no cell lock, Phase C publish if the generation did not move"
  - "CellState::rebasing (one step in its I/O phase per cell), CellGuard::unlocked"
  - "rebase_cell: skip while a step is in flight, locked retry after a drop, hard cap limit + 2 * REBASE_STEP"
  - "test_dirty_cost(ino); crates/slicefs-cli/tests/lock_free_io.rs (5 tests)"
affects: [19-07, 19-08, 19-09, 19-10, 19-11, 20]

estimate:
  tokens: 120000
  tasks: 2
actuals:
  tokens: 11200
  tasks: 2
  commits: 2

plan_head_before: dd925c814c7abbd7674ade6f06ccbfa6cf391aa8
plan_head_after: 9811c32ee2d68ae860b8325cabe7edee3c20e6c8
commits: 2

tech-stack:
  added: []
  patterns:
    - "Copy under the lock, I/O with no lock, publish only if the generation did not move"
    - "A guard that can release its lock for a closure and lock again (CellGuard::unlocked); the caller checks retired after it"
    - "One selection function (Overlay::select) behind take, copy_step, step_ranges and discard, so they cannot differ"

key-files:
  created:
    - crates/slicefs-cli/tests/lock_free_io.rs
  modified:
    - crates/slicefs-cli/src/overlay.rs
    - crates/slicefs-cli/src/filesystem.rs
    - crates/metadata/src/store.rs

key-decisions:
  - "Overlay generations come from one process-wide counter, so an overlay made after a truncate to 0 cannot repeat an old generation"
  - "A dropped step is tried again at once in the same write, with the cell locked"
  - "A writer past the hard cap stores a locked step even while another step is in flight (that step is then dropped)"
  - "The last release (finalize) keeps all three phases in one cell-lock hold, because the release retires the cell in that hold; for usize::MAX the splice reads the live overlay, with no copy"

patterns-established:
  - "After CellGuard::unlocked, check retired before you use the state"

requirements-completed: []

duration: 25min
completed: 2026-11-22
status: complete
---

# Phase 19 Plan 06: Overlay rebase step out of the file lock Summary

**A rebase step copies its extents under the cell lock. It splices, hashes and puts them with no cell lock held. It publishes only if the overlay generation did not change. A parked rebase now blocks neither a read nor a write of the same file. Before this plan, both timed out after 5 s.**

## Performance

- **Duration:** about 25 min
- **Completed:** 2026-11-22
- **Tasks:** 2
- **Files:** 4 (3 modified, 1 created)

## Accomplishments

- `Overlay` (Task 1): a private generation, read with `generation()`. `put`, `write`, `truncate`, `take`, `discard` and `set_sweep` move it. `new` and `empty_like` take a new value. `copy_step(free)` copies what `take(free)` would take, with no change. `step_ranges(free)` gives the same pieces as `(start, len)` with no copy. `discard(pieces)` removes exactly those pieces and moves the sweep point as `take` does. All four use one private `select`. `take` is now `select` and `cut`.
- `store.rs` (Task 1): `prepare_manifest(blocks)` stores the manifest node and returns its digest, with no lock. `set_manifest_digest(ino, digest)` changes the map under its short lock. `set_manifest` calls the two in sequence.
- `rebase_step` (Task 1) is now three helpers:
  - `step_plan` (Phase A, memory only) copies the pieces and reads the generation.
  - `step_io` (Phase B) runs `content::splice`, `prepare_manifest`, the counters and the rebase hook.
  - `step_publish` (Phase C) does the generation check, the manifest digest swap, and the refcounts from `last_committed_root` as it is at publish time. Then it calls `discard`, makes the next overlay and calls `stamp_content`.
  - `rebase_unlocked` runs Phase B inside `CellGuard::unlocked`. The locked form (`rebase_step`) runs all three in one hold.
  - On a Phase B error nothing is put back. The live overlay kept every byte.
- `rebase_cell` (Task 2): at most one step per cell is in its I/O phase (`CellState::rebasing`). A writer that finds the flag set skips its step, unless the file cost is past `dirty_limit + 2 * REBASE_STEP`. In that case it stores a locked step. If a result is dropped, the same write tries again at once with the cell locked. `free_dirty` (the budget step of `keep_budget`, D-13) goes through `rebase_cell`.

## Task Commits

1. **Task 1: Overlay generation, copy_step and discard, three-phase rebase step** - `3a32c50` (feat)
2. **Task 2: One rebase per cell, locked retry after a drop, hard cap, budget step through the protocol** - `9811c32` (feat)

**Plan metadata:** in the final docs commit.

## TDD record (red before green, no red commit)

- `reader_does_not_wait_for_rebase_io` on the Plan 05 code (`dd925c8`): **timed out**. The test printed "the read waited for the parked rebase: Timeout" after 5 s. `writer_does_not_wait_for_rebase_io` failed in the same way ("the write waited for the parked rebase: Timeout"). Both passed after Task 1 (0.06 s for the binary).
- Task 2 tests on the Task 1 tree:
  - `a_dropped_step_is_retried_with_the_cell_locked`: **red**. The message was "the dropped attempt and the locked retry" (`rebase_count` moved by 1, not 2). It was green after `rebase_cell`.
  - `a_second_writer_skips_its_step_while_one_is_in_flight`: green before the fix. Task 1 has the `rebasing` skip already.
  - `dirty_cost_stays_under_the_hard_cap`: green before the fix. A fully dirty 2 MiB file costs about the cap, so this test cannot show a cap break (see Deviations). After Task 2 the run did 6 rebase steps for 800 writes.

## Verification

- `cargo test -p slicefs-cli --lib overlay::`: 8 passed, among them `every_change_moves_gen_and_a_copy_does_not` and `a_copy_then_discard_at_one_gen_equals_take`.
- `cargo test -p slicefs-cli --test lock_free_io`: 5 passed (0.24 s).
- `cargo test -p slicefs-cli --test gc_online_tests --test rebase_step_tests`: 15 and 4 passed. `online_gc_during_a_rebase_keeps_every_chunk` and `online_gc_inside_a_rebase_step_keeps_every_chunk` passed unchanged.
- Tracer gate after Task 1: `lock_free_io` and `overlay::` ran again on the committed tree and passed.
- Serialize-off one-file run (`STRESS_SERIALIZE=0 STRESS_MODE=one-file STRESS_SECS=3 STRESS_THREADS=4 STRESS_FILE_MIB=4`): exit 0. The result was `max_inflight=4`, `ops=2303 dirty_total=0`, `op=write n=1332 p99_us=25083.9`, `op=read n=2213 p99_us=665.6`, with no "mismatches" line.
- Full suite command from 19-VALIDATION.md, on the final tree: exit 0. It printed 66 `test result: ok` lines, with no `FAILED` and no `lock rank violation`.
- `cargo test -p slicefs-cli --features lock-stats --test lock_stats_report`: 1 passed.
- Acceptance greps: `fn copy_step` 1, `fn discard` 1, `fn prepare_manifest` 1, `fn reader_does_not_wait_for_rebase_io` 1, `fn dirty_cost_stays_under_the_hard_cap` 1, `rebasing` in filesystem.rs 6.

## Decisions Made

See `key-decisions`. All of them can be changed later.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 - Blocking] `gen` is a reserved word in Rust 2024**
- **Found during:** Task 1
- **Issue:** The crate uses edition 2024, where `gen` is reserved. The plan named the field `gen` and the getter `gen()`.
- **Fix:** The field is `generation` and the getter is `generation()`. The unit test names still contain "gen", as the verify step asks.
- **Commit:** `3a32c50`

**2. [Rule 1 - Bug] A generation could repeat after a truncate to 0**
- **Found during:** Task 1 design
- **Issue:** A truncate to 0 replaces the whole `OpenFileState`. A later `to_overlay` makes a new `Overlay`. With a counter per overlay, its generation could again equal the one that a parked Phase A read. A stale step could then publish over newer bytes (T-19-19).
- **Fix:** All overlays take their generation from one process-wide `AtomicU64`. A new overlay, also the one after a publish, starts past every older generation. This gives the plan's "continues from the old gen + 1" rule, so no setter is necessary. Phase C also drops the result if the mode is no longer `Overlay` or the cell is retired.
- **Commit:** `3a32c50`

### Plan facts

- **Retry timing:** "after one dropped attempt, the next attempt runs under the lock" is done in the same write call, at once. A per-cell flag for a later write was not necessary for this rule.
- **Hard cap while a step is in flight:** a writer past the cap does not skip. It stores a locked step, and the step in flight is then dropped. Without this, a long Phase B could leave the cap with no check.
- **`rebase_overlay` (finalize):** it uses the same three helpers, but in one cell-lock hold. `leave` retires the cell in that hold. If the lock were released, an `open` could add a write handle to a cell that is about to retire. Plan 07 makes finalize stepwise. For `usize::MAX` the splice reads the live overlay, so there is no second copy of the overlay (#98). It is still one step, which `gc_online_tests` expects (`before + 1` at release).
- **Hard cap test strength:** with a 2 MiB file, the plan's numbers (limit 256 KiB, 4 KiB blocks) make a fully dirty overlay about 2.03 MiB, below the 2.25 MiB cap. So the test was green before Task 2. A 2 ms sleep in the rebase hook makes the I/O phase longer, so concurrent writes skip and drop steps. The test keeps the plan's sizes.
- **`rebase_count`:** a dropped attempt counts too (its Phase B ran). The retry test expects `+2`.
- **Commit gate for Task 1:** the workspace run before the Task 1 commit used the Task 1 test file. The Task 2 tests were kept aside until that commit.

**Total deviations:** 2 auto-fixes (1 Rule 3, 1 Rule 1) and 6 notes. **Impact:** all plan truths hold.

## For later plans

- Plan 07: still under the cell lock: the stream `push` in `test_write`, `to_overlay` (`finish` of the stream clone, `has_content`, `get_manifest`), `settle_pending` (`fill_stream`), `flush_state` (`end_padded`) and `finalize_write` (with `rebase_overlay` in one hold). `CellGuard::unlocked`, `step_plan`, `step_io` and `step_publish` can be used again there.
- `rebase_hook` now fires with the cell unlocked in `rebase_unlocked`. It fires with the cell locked in `rebase_step` (finalize, the locked retry, and the hard-cap step). A test hook that parks must park only once, or it can hold the cell lock.
- `keep_budget` stops when a step frees nothing. A skip (another step in flight) or a drop therefore ends the budget loop for that write.
- The coarse rule drops a result on any change. Phase 20 can publish when the changes since Phase A do not touch the copied ranges (RESEARCH Pattern 1, optional).

## Issues Encountered

None.

## Known Stubs

None.

## Threat Flags

None. There is no new endpoint, file kind or trust boundary.
- T-19-SC: no new crate.
- T-19-19: publish only at the same generation. Every change moves it (unit test per method), generations never repeat, and nothing is put back.
- T-19-20: one step per cell, a locked retry after a drop, and the hard cap (`dirty_cost_stays_under_the_hard_cap`).
- T-19-21: refcounts use `last_committed_root` as read in Phase C.
- T-19-22: Phase B runs on the request thread inside its gate hold. The GC-in-hook tests pass unchanged.

## Self-Check: PASSED

- `crates/slicefs-cli/tests/lock_free_io.rs` and this SUMMARY: FOUND
- Commits `3a32c50` and `9811c32`: FOUND on HEAD (`git rev-list --count dd925c8..HEAD` prints 2)
