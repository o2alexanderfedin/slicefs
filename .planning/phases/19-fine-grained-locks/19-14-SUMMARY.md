---
phase: 19-fine-grained-locks
plan: 14
subsystem: concurrency
status: complete
tags: [lock-05, lock-01, step-token, hard-cap, gap-closure, d-02, g3, g5]

requires:
  - phase: 19-fine-grained-locks
    provides: "19-06/19-07 three-phase steps (clone, unlocked I/O, publish at the same generation); 19-11 Finding 5 (locked fallbacks); 19-12 SyncWait::wait_timeout and STORE_WAIT; spike 19g (eprintln in a cell hold)"
provides:
  - "CellState::stepping: a per-cell step token (SyncWait); CellGuard::run_step / wait_step (STEP_WAIT 15 s, then EIO)"
  - "Stream writes wait for the token with the cell unlocked; every stream step of a write runs unlocked; dropped steps retry unlocked (STEP_TRIES 16, then EIO)"
  - "test_read composes pending stream writes from memory; a read runs no stream step and never waits for the token"
  - "Overlay hard-cap gate on the apply (Overlay::cost_after); writers at the cap wait or step with the cell unlocked"
  - "rebase_cell: up to REBASE_TRIES (2) unlocked attempts, no locked step"
  - "test hooks test_drop_steps(n), test_drop_every(k)"
  - "Overlay-switch and rebase trace lines print with no cell lock held (CellGuard notes for the locked finalize form)"
affects: [19-15, 19-16, 20]

estimate:
  tokens: 140000
  tasks: 2
actuals:
  tokens: 16083
  tasks: 2
  commits: 2

plan_head_before: 90c389a03700c28d2cad0123c2081727f5558824
plan_head_after: 24bf98e6da6accfce4b40ab27d086dc1e15de8d7
commits: 2

tech-stack:
  added: []
  patterns:
    - "Step token: one I/O step per cell in flight; writers wait for it with the cell released (bounded); readers and flushes never touch it"
    - "Gate before apply: an overlay write applies only if its predicted cost stays under the hard cap"
    - "Debug lines leave the hold: format in the hold, print after the guard drops (CellGuard notes, SwitchNote)"

key-files:
  created: []
  modified:
    - crates/slicefs-cli/src/filesystem.rs
    - crates/slicefs-cli/src/overlay.rs
    - crates/slicefs-cli/tests/lock_free_io.rs

key-decisions:
  - "G3 (write and read paths): one step token per cell (CellState::stepping) replaces the rebase flag; stream writers wait for it with the cell unlocked (STEP_WAIT 15 s, above STORE_WAIT 10 s), readers never do"
  - "A dropped stream step retries unlocked, at most STEP_TRIES (16) in a row, then EIO; a write's rebase step tries REBASE_TRIES (2) unlocked attempts, then leaves the work to the next write, the release or a budget step"
  - "Reads compose stream prefix, zeros and pending parts from memory (no settle); memory O(request)"
  - "hard_cap() = limit + limit / GROW_DIV + 2 * REBASE_STEP (was limit + 2 * REBASE_STEP): the 1/8 regrowth of an extent near the limit passes the old cap in a plain single-writer run at the 32 MiB production limit"
  - "The cap gate predicts the cost of the write exactly (Overlay::cost_after); a post-apply loop still steps or waits if the cost is above the cap"
  - "G5: the overlay-switch line prints in write_logged; the rebase trace prints in the unlocked closure, or from CellGuard notes after the locked finalize hold"

patterns-established:
  - "Locked forms of a step are separate named functions (stream_step_locked, settle_locked, to_overlay_locked, rebase_step) and only the 19-15 callers (open-stream size change, finalize, destroy) use them"
  - "Only token steps consult the forced-drop hooks; locked forms never do"

requirements-completed: []

duration: about 5 h 20 min wall time (includes a rate-limit stop and test runs on a heavily loaded machine)
completed: 2026-11-22
---

# Phase 19 Plan 14: Step token, unlocked retries, composed reads and the hard-cap gate Summary

**On the write and read paths no chunk, hash or store put runs with a cell locked. A per-cell step token allows one I/O step in flight; other writers wait for it with the cell released, and readers never wait. Dropped steps retry unlocked. Reads compose pending stream writes from memory. An overlay writer at the hard cap waits or steps with no cell lock, and the cap gates the apply.**

## Performance

- **Duration:** about 5 h 20 min wall time, from 2026-11-22T15:43Z to 21:05Z. This includes a rate-limit stop, and workspace runs of 15 to 20 min each on a machine at load average 30-40.
- **Tasks:** 2
- **Files:** 3 modified

## Accomplishments

- **Task 1 (tracer): step token, stream writes, reads.**
  - `CellState::stepping: Option<SyncWait>` replaces the rebase flag.
    - `CellGuard::run_step` sets a fresh token, runs the I/O with the cell unlocked, locks again, then clears and signals the token. The closure returns errors as values, so no path skips the clear.
    - `CellGuard::wait_step` waits for the token with the cell unlocked, `STEP_WAIT` (15 s) per step, then EIO.
  - Stream mode of `write_in_cell`:
    - At the top of each loop turn, a stream writer waits for a token in flight.
    - The first-write content check, the gap fill, the in-order push, the lined-up drain and the overlay switch each run as one unlocked step under the token.
    - A dropped step makes the loop turn again, with the I/O still unlocked. After `STEP_TRIES` (16) dropped steps in a row the write returns EIO, and prints the reason after the hold.
    - The `Write` record is still appended in the Phase C hold that publishes the push. A failure in the drain after that does not fail the write.
  - `test_read` does no settle. Under the cell lock it copies the overlay view. For a stream it copies the clone (only if the range starts in the streamed prefix), the size, the streamed end, and the parts of pending entries inside the range. With no lock it composes stream bytes, zeros up to the size, and the pending parts on top.
  - `free_dirty` switches a stream with pending writes only as an unlocked step under the token. If a step is in flight, or the switch drops, it frees nothing.
  - The test hooks `test_drop_steps(n)` and `test_drop_every(k)` drop the results of unlocked steps (stream steps, overlay switches, rebase steps). Flushes are never dropped.
  - Locked forms stay only as separately named functions for the 19-15 callers: `settle_locked` / `to_overlay_locked` (the size change of an open stream), `stream_step_locked` (finalize after a drop, destroy) and `rebase_step` (finalize, destroy).
- **Task 2 (TDD): overlay hard cap, rebase retries, debug lines.**
  - The overlay branch gates the apply. If the cost is above 0 and `Overlay::cost_after(offset, len)` is above `hard_cap()`, the writer acts with the cell released, then looks again:
    - with a step in flight, it waits for that step (`wait_step`);
    - otherwise it runs its own unlocked rebase step that frees at least the excess, and at least `REBASE_STEP` (`cap_step`).
  - After the apply, a loop steps or waits the same way while the cost is above the cap. It is the safety net for "a write never returns above the cap", if the prediction and the allocator differ.
  - `rebase_cell` skips while a step is in flight. Otherwise it runs up to `REBASE_TRIES` (2) unlocked attempts. If both drop it returns Ok, and the next write, the release or a budget step tries again. It has no locked step.
  - G5: the overlay-switch line is built in the hold as a `SwitchNote` and printed by `write_logged` after the cell guard is gone. `step_io` returns its trace line. `rebase_unlocked` prints that line inside its unlocked closure. The locked `rebase_step` returns it; finalize pushes it to `CellGuard::notes`, which `Drop` prints after the cell lock is released.

## Task Commits

1. **Task 1: step token; stream writes retry unlocked; reads compose pending writes** (tracer): `6c607a3`
2. **Task 2: overlay hard cap waits with no cell lock; dropped rebase steps retry unlocked** (TDD): `24bf98e`

## TDD and red-run evidence

Each new test first ran against the tree before its fix. Each test was committed with its fix, so no commit is red.

- **Task 1** (against 90c389a, with only the inert hook setters added):
  - `a_second_stream_write_waits_for_the_push_without_the_cell`: `B waited for the parked push: Ok(Ok(65536))`. On the old code B returned at once through a pending insert, and A then switched to an overlay.
  - `a_forced_drop_of_a_push_retries_unlocked`: `the retry after the dropped push reached its push: Timeout`. The hook was inert, so no retry ran.
  - `a_read_of_pending_writes_runs_no_stream_step`: `no stream step in a read, left: 2, right: 0`. The old read settled both pending writes.
- **Task 2** (against the Task 1 commit `6c607a3`):
  - `a_dropped_step_is_retried_unlocked`: `the read waited for the retry: Timeout`. The old retry was locked.
  - `a_writer_at_the_hard_cap_waits_without_the_cell`: `the read waited for the writer at the cap: Timeout`. This is the read assertion, as the plan asks: the old locked step past the cap parked with the cell held.
  - `a_dropped_step_below_the_cap_retries_later`: `cost 0 after two dropped steps`. The old locked fallback published at once.
- **Tracer gate (Task 1):** both verify commands ran again after the commit and passed: `lock_free_io` 16 passed, with both named tests ok; streaming, write_path, multi_handle_posix, fsync and write_log_replay all green.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Bug] The hard cap could not hold under plain single-writer growth**
- **Found during:** Task 2 (`rebase_step_tests::a_growing_extent_past_the_limit_is_rebased_in_parts` at the 32 MiB production limit: one write re-chunked 2 281 865 bytes, above the test's 2 MiB bound).
- **Issue:** `Overlay::cut` shrinks an extent after a step. The next in-order write then regrows it by 1/`GROW_DIV` of its length. A temporary trace in the gate showed the cost before the write at 31 921 245 bytes (below the limit) and the predicted cost at 35 910 872, above the old cap of 35 651 584 (limit + 2 MiB). The old code did not check the cap with no step in flight, so this transient was never enforced. The baseline's peak cost sampled after each write's step was 32 739 976, below the old cap. The now-enforced gate ran an extra step of more than 2 MiB.
- **Fix:** `hard_cap()` = `limit + limit / GROW_DIV + 2 * REBASE_STEP`. The must-have truth text says "limit + 2 * REBASE_STEP". The per-file memory bound still holds, with limit/8 more margin (4 MiB at the 32 MiB limit). The two new tests use the same formula. `dirty_cost_stays_under_the_hard_cap` and the 2 MiB bound in `rebase_step_tests` are unchanged and green.
- **Files modified:** `crates/slicefs-cli/src/filesystem.rs`, `crates/slicefs-cli/tests/lock_free_io.rs`
- **Commit:** `24bf98e`

**2. [Rule 2 - Correctness] Exact cost prediction for the cap gate (`Overlay::cost_after`)**
- **Issue:** with the plan's gate (cost + bytes + `EXTENT_OVERHEAD`), extent regrowth often passes the cap after the apply. The writer then steps or waits with the synced cost above the cap, and `dirty_cost_stays_under_the_hard_cap` and the new hard-cap test sample that value.
- **Fix:** `Overlay::cost_after(off, len)` uses the merge and growth rule of `put` with no change (O(log n + hits)). The gate uses it. The post-apply loop stays as a safety net. A unit test `cost_after_predicts_the_cost_of_a_write` checks the prediction against eight writes.
- **Files modified:** `crates/slicefs-cli/src/overlay.rs` (not in the plan's `files_modified`)
- **Commit:** `24bf98e`

**3. Finalize settle keeps a locked fallback, without `unlocked &=`**
- The plan lets finalize keep locked forms until 19-15. The old `settle(g, unlocked)` was split up, so finalize now settles one pending entry per loop turn. It uses an unlocked token step when no token is set. After a drop, or with a token set, it uses `stream_step_locked`, as before. 19-15 removes this.

**4. No `gc_online_tests` count changed**
- `before + 1` (one rebase at release) and the wait for `before + 4` still hold. No change was needed.

## Not covered by a test

- `test_drop_every(k)` (19-15 uses it) and the EIO exit after `STEP_TRIES` dropped steps have no test in this plan.
- `a_forced_drop_of_a_push_retries_unlocked` and `a_dropped_step_is_retried_unlocked` require a read to end within 300 ms, as the plan asks. On a slow CI machine these assertions can fail with no bug.

## Locked paths that stay (19-15)

- `finalize_steps`: the locked `stream_step_locked` and `rebase_step` after a drop or with a step in flight, and the locked flush retry.
- `setattr_size_open`: `settle_locked` and `to_overlay_locked` with the cell held. The size change does not wait for the token yet, so it can drop a step in flight.
- `test_destroy`, which runs no request, and `commit_cut_begin`'s locked `flush_cell` under the exclusive gate.

## User-visible change

- A write that waits more than 15 s for another writer's step on the same file returns EIO. So does a write with 16 dropped steps in a row; in this plan only a forced drop (a test hook), the retire of the cell, one fsync install, or a storm of open-stream size changes can cause that. Before this plan these paths retried with the cell locked and had no limit.
- Reads of a stream with out-of-order writes no longer stream them. The pending writes stay in memory until a later write or the release streams them.
- **Owner choice: a large switch now pays one large step.** With the production constants, the pending writes of a stream (up to `REORDER_WINDOW`, 64 MiB) can switch into an overlay that is above the hard cap (38 MiB at the 32 MiB limit). The write that switches then runs its gate step for the whole excess. A probe (59 x 1 MiB pending writes, then an overlapping write; a temporary test, not committed) showed one step with `free=22281511`: a 22 MB copy in the hold (a memory copy) and a 161 ms splice with the cell unlocked. Under the old rule each write freed at most max(added, 1 MiB) and the cost came down over many writes. The per-file memory bound holds. A possible change is to limit each gate step to `final_step` and let the writer step again. This plan does not make that change. Two comments are now stale: "after a switch it comes down with each write smaller than `REBASE_STEP`" in `write_in_cell`, and "The real peak of one handle is ..." in the `DIRTY_LIMIT` doc.

## Threat model notes

- **T-19-52:** every Phase C keeps the generation check. The token stops concurrent stream changes. `a_forced_drop_of_a_push_retries_unlocked` and `a_dropped_step_is_retried_unlocked` show that a dropped result is not installed: the content matches the model, and the newer bytes win.
- **T-19-53:** the cap gate stops the apply until the predicted cost is under the cap. `a_writer_at_the_hard_cap_waits_without_the_cell` samples the cost after each write and while the writer waits. `dirty_cost_stays_under_the_hard_cap` stays green.
- **T-19-54:** every token wait has `STEP_WAIT` (15 s), then EIO.
- **T-19-55:** `a_read_of_pending_writes_runs_no_stream_step` compares reads across gaps with the content after release.
- **T-19-56:** only `run_step` sets and clears the token, on every return path.

## Verification

- `cargo test -p slicefs-cli --test lock_free_io -- --nocapture`: 17 passed, including all five new tests named in the plan and the two Task 1 tests. Five more runs in a row were also green, 0.55-0.58 s each.
- Task 1 suites: fsync (7), multi_handle_posix (13), streaming (25), write_log_replay (35), write_path (24). All green.
- Task 2 suites: dirty_budget (1), gc_online (16), memory_bound (1), rebase_step (4). All green.
- `cargo test --workspace` exited 0 before each task commit, with 67 `test result: ok` lines each time and no FAILED.
- `cargo test -p slicefs-cli --features lock-stats --test lock_wait_rows`: 2 passed.
- Full suite command of 19-VALIDATION (`cargo test --workspace` plus the three `cargo check` runs): exit 0, 68 `test result: ok` lines (including `lock_stats_report`). `cargo test -p slicefs-cli --features lock-stats --test lock_stats_report`: exit 0. `cargo check -p slicefs-cli --all-targets --features deadlock-detect`: exit 0.
- `stress_harness` (nothing added to it). Machine load average was 30-40 from other projects. The baseline ran from a scratch worktree at `90c389a`, alternated with this tree:
  - After Task 1: baseline 9.62 / 9.64 / 10.04 s; this tree 10.13 / 9.65 / 9.66 s.
  - After Task 2: baseline 10.04 / 10.03 / 10.32 s; this tree 9.78 / 10.34 / 10.19 s.
  - The spread is the same, so the plan adds no time. The "under 10 s warm" signal is **not met today** on this loaded machine, for the baseline too.
- Acceptance greps on the final tree:
  - Task 1: `unlocked = false` in write_in_cell 0; `unlocked &=` 0; `self.settle(` in test_read 0; `rebasing` 0; `fn test_drop_steps` 1.
  - Task 2: `self.rebase_step(` in rebase_cell 0; `switches to overlay` in write_in_cell 0; `switches to overlay` in the file 1; the `[rebase]` eprintln in step_io 0; `fn a_writer_at_the_hard_cap_waits_without_the_cell` 1.

## Self-Check: PASSED

- `crates/slicefs-cli/src/filesystem.rs`, `crates/slicefs-cli/src/overlay.rs`, `crates/slicefs-cli/tests/lock_free_io.rs` and this SUMMARY exist.
- Commits `6c607a3` and `24bf98e` are on HEAD.
