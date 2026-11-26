---
phase: 19-fine-grained-locks
plan: 07
subsystem: concurrency
tags: [lock-01, lock-05, d-02, d-03, stream, fsync, finalize, truncate, online-gc]

requires:
  - phase: 19-fine-grained-locks
    provides: "19-06 three-phase rebase step, Overlay generation, prepare_manifest / set_manifest_digest, CellGuard::unlocked"
provides:
  - "Stream generation in OpenFileState (process-wide counter, overlay::next_gen)"
  - "stream_step: clone the stream under the cell lock, push with no cell lock, swap in only at the same generation"
  - "to_overlay, settle and the pending drain through the protocol; reads settle unlocked"
  - "flush_cell: fsync/flush publish in snapshot order (CellState::last_published_gen), refcounts at publish time"
  - "Stepwise finalize at the last release (FINAL_STEP, 8 MiB); retire only when clean and no write handle is open"
  - "Closed-file truncate in a transient cell, locked before it goes into the map"
  - "test_set_push_hook, test_set_final_step"
affects: [19-08, 19-09, 19-10, 19-11, 20]

estimate:
  tokens: 140000
  tasks: 3
actuals:
  tokens: 20526
  tasks: 3
  commits: 5

plan_head_before: d4ebb2c9c310061aac9c43178a9182f058e96da5
plan_head_after: 81e4d1eeec9d58231d474eae853a3ccdb33a0a52
commits: 5

tech-stack:
  added: []
  patterns:
    - "One stream step = clone, push unlocked, publish at the same generation; after a drop the rest of the request runs locked"
    - "Publish order by snapshot generation: a flush installs only if last_published_gen is older"
    - "A transient cell is locked before it is in the map, so nobody else can use it first"

key-files:
  created: []
  modified:
    - crates/slicefs-cli/src/filesystem.rs
    - crates/slicefs-cli/src/overlay.rs
    - crates/slicefs-cli/tests/lock_free_io.rs
    - crates/slicefs-cli/tests/gc_online_tests.rs

key-decisions:
  - "Stream generations use the same process-wide counter as overlays, so a truncate to 0 (a new state) never repeats one"
  - "Pending writes are streamed one entry per step (one cloned entry at a time), never a copy of the whole pending map"
  - "A flush does not move the stream generation; it skips its install if last_published_gen is not older than its snapshot, and every other manifest install (overlay switch, truncate to 0) sets last_published_gen too"
  - "FINAL_STEP is 8 MiB (Claude's discretion); finalize retries a dropped rebase step once locked, then goes on unlocked"
  - "A size change of an open stream (setattr) runs its settle and overlay switch with the cell locked (rare path)"
  - "leave() always finalizes when the last write handle goes (the finalize flag is gone)"

patterns-established:
  - "After any CellGuard::unlocked: check retired, then the mode, then the generation"

requirements-completed: []

duration: 95min
completed: 2026-11-22
status: complete
---

# Phase 19 Plan 07: Streaming, read, fsync, finalize and truncate through the cell protocol Summary

**Every content path now follows copy, then I/O with no cell lock, then publish only at the same generation. A parked streaming push no longer blocks a read. Two fsyncs install in snapshot order. The last release stores an overlay in bounded steps. A closed-file truncate runs in a transient cell.**

## Performance

- **Duration:** about 95 min
- **Completed:** 2026-11-22
- **Tasks:** 3
- **Files:** 4 modified

## Accomplishments

- **Task 1 (streaming, overlay switch, settles, reads):**
  - `OpenFileState::generation` takes a new value from `overlay::next_gen` at each change: a push, a pending insert, a fill, an extension by setattr, the overlay switch. A truncate to 0 makes a new state, so it also gets a new value.
  - `stream_step` does the three phases for one push:
    - Phase A: clone the stream (at most about 64 KiB).
    - Phase B: push zeros and bytes with no cell lock. The push hook runs here.
    - Phase C: swap the clone in only at the same generation. It also removes the pending entry that the bytes came from.
  - `test_write` routes these through `stream_step`: the in-order push, the pending entries that then line up, and the zero fill of a sparse jump. The first-write `has_content` check runs unlocked, followed by a generation check. After one dropped step, the rest of the write runs locked.
  - `to_overlay` ends the clone (or reads the committed root) and stores the manifest node in Phase B. Phase C installs only if the generation and `last_committed_root` did not move.
  - `settle` streams pending writes one entry per step. Reads and setattr use it. The read path then copies the overlay view or clones the stream, and reads with no lock, as before.
  - `install_root` was removed (no callers).
- **Task 2 (fsync, finalize, closed truncate):**
  - `flush_cell` (from `flush_handle`, and locked from `commit_cut_begin`) ends the snapshot and stores its manifest node unlocked. Phase C skips the install if `last_published_gen >= snapshot gen` or the mode is no longer a stream. The refcounts come from `last_committed_root` as it is at publish time. `cas_committed` becomes true only if the generation still matches.
  - `finalize` / `finalize_steps` work in a loop until the content is clean, the cell is retired, or a write handle opens again (then the cell lives on). Each pass does one of:
    - a settle;
    - a rebase step of at most `final_step` (unlocked; one locked retry after a drop, or if another step is in flight);
    - a flush (once more locked if the stream changed);
    - the empty-file manifest rule.
  - When the content is clean and no write handle is open, the cell is retired and removed in the same hold. `test_destroy` uses the locked form. `finalize_write`, `rebase_overlay`, `settle_pending`, `fill_stream` and `push_zeros` are gone.
  - `test_setattr_size` case B makes a `FileCell`, locks it, then calls `CellMap::insert_new`. The splice (`setattr_size_closed`) runs under that lock, and then the cell is retired and removed. Another open or size change of the inode waits for it, then finds a retired cell and looks the inode up again. A live cell (also one whose finalize is in progress) gets case A.
- **Task 3:** `many_files_no_cross_file_wait` and `reads_during_rewrite_and_gc_never_fail`.

## Task Commits

1. **Task 1: streaming writes, overlay switch, settles and reads do their I/O unlocked** - `8d10747` (feat)
2. **Task 2: fsync publishes in order, stepwise finalize, transient cell for closed truncate** - `eae0402` (feat)
3. **Task 3: no cross-file wait, reads stay safe during rewrites and online GC** - `cc1f24e` (test)
4. **Tests that pin the stream generation check and the truncate install rule (after the review)** - `81e4d1e` (test)

**Plan metadata:** `47ede52` (first docs commit) and the docs commit after `81e4d1e`. `commits: 5` counts `d4ebb2c..81e4d1e`, which includes `47ede52`.

## TDD record (red before green, no red commit)

- **Task 1 (tracer):** `test_set_push_hook` was added first. It fired inside today's locked push.
  - `reader_does_not_wait_for_streaming_push` was **red**: "the read waited for the parked push: Timeout" after 5 s.
  - `a_read_settles_pending_writes` was **red**: "one unlocked step per pending write, left: 0".
  - `a_streaming_push_that_loses_the_generation_check_retries` passed on the old code, but only after the hook's 10 s park timeout, because the second write waited for the cell lock.
  - All 9 `lock_free_io` tests passed after the change (0.22 s).
  - Tracer gate: `lock_free_io` and the streaming, write_path and fsync suites ran again on the committed tree and passed.
- **Task 2:**
  - `fsyncs_publish_in_order`, with `flush_cell` written first without the `last_published_gen` check: **red**, "the newer root stays" (the manifest held fsync 1's older root). Green after the check.
  - `the_last_release_finalizes_in_steps` and `a_writer_that_opens_during_the_finalize_keeps_the_cell` were run once with the old release shape restored for a moment (one locked `usize::MAX` step). Both were **red**: "more than one step: 1" and "the open and write waited for the finalize: Timeout". Both passed with the real code.
  - `a_closed_file_truncate_runs_in_a_transient_cell` was written together with the code. Its park point (the rebase hook after the closed splice) is new in this plan, so the old code could not reach it.
- **Mutation checks after the review (commit `81e4d1e`):**
  - Removing the generation compare in Phase C of `stream_step` left all 14 tests green. The old retry test let the second write only add a pending entry, so a stale swap gave the same bytes.
  - The retry test now does a conflicting in-order write: 128 KiB at the parked write's offset. With the compare removed it is **red**, "both writes with the handles open" (the stale clone drops the tail of the second write). With the compare it is green, and the retry goes to an overlay on top of the second write.
  - Removing the `last_published_gen` update at a truncate to 0 also left all 14 tests green. The new `a_parked_fsync_does_not_install_over_a_truncate` is **red** without it ("the truncate wins": the parked fsync put the old root back) and green with it.
- **Task 3:** both tests were green on their first run. They test the code of Tasks 1-2 and Plan 06, and showed no fault that needed a fix. One run of the GC test did 24 rewrites, 98 reads, 3 online GCs and 6 rebase steps in 2.2 s. After the review the test also adds up `removed_objects` of the sweeps and asserts that it is above 0. Two runs removed 171 and 208 objects, so the sweeps deleted the objects of old roots while the reads ran.

## Verification

- `cargo test -p slicefs-cli --test lock_free_io`: 15 passed (0.33 s). Among them are `reader_does_not_wait_for_streaming_push`, `fsyncs_publish_in_order` and `many_files_no_cross_file_wait`.
- `cargo test -p slicefs-cli --test gc_online_tests`: 16 passed. `reads_during_rewrite_and_gc_never_fail` takes 2.2-2.4 s. The two GC-in-hook tests passed unchanged.
- `streaming_tests` 25, `write_path_tests` 24, `fsync_tests` 7, `rebase_step_tests` 4, `multi_handle_posix` 7, `dirty_budget_tests` 1: all passed.
- Serialize-off one-file run (`STRESS_SERIALIZE=0 STRESS_MODE=one-file STRESS_SECS=3 STRESS_THREADS=4 STRESS_FILE_MIB=4`): `test result: ok. 1 passed` with no "mismatches" line. The exit code was not captured (the shell has no `PIPESTATUS`); the test result line is the evidence. The run reported:
  - `max_inflight=4`, `ops=2325 dirty_total=0`;
  - `op=read n=2242 p99_us=858.6`, `op=write n=1338 p99_us=40828.9`, `op=fsync n=83 p99_us=123666.4`.
- By-hand many-files run (`STRESS_MODE=many-files`, same env): exit 0, `test result: ok`, no "mismatches" line. The run reported:
  - `max_inflight=4`, `ops=1351 dirty_total=0`;
  - `read n=850`, `write n=549`, `fsync n=82`, `create n=196`, `rename n=122`, `unlink n=124`.
  - This run showed no namespace race. Plan 09 still owns that area.
- `cargo test --workspace` exited 0 before each task commit and before `81e4d1e` (66 `test result: ok` lines, no `FAILED`).
- Full suite command from 19-VALIDATION.md, on `cc1f24e` (`81e4d1e` changes only two test files, and the workspace run before it was green): exit 0, 66 `test result: ok` lines, no `FAILED`, no `error`, no `lock rank violation`.
- `cargo test -p slicefs-cli --features lock-stats --test lock_stats_report`: 1 passed.
- Acceptance greps:
  - `fn test_set_push_hook` 1, `fn reader_does_not_wait_for_streaming_push` 1;
  - `last_published_gen` 7, `FINAL_STEP` 3;
  - `fn fsyncs_publish_in_order` 1, `fn many_files_no_cross_file_wait` 1, `fn reads_during_rewrite_and_gc_never_fail` 1.

## Decisions Made

See `key-decisions`. All of them can be changed later.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Bug] A parked fsync could install over a truncate or an overlay switch**
- **Found during:** Task 2 design
- **Issue:** "Skip if a newer snapshot already published" covers only fsync against fsync. Two other cases need it too:
  - After a truncate to 0, a parked older snapshot would bring back the old content. Release would not correct it, because `byte_count` is 0.
  - After `to_overlay`, the parked snapshot would break the rule that the manifest names the overlay base.
- **Fix:** The truncate to 0 and the overlay switch also set `last_published_gen`, to the new generation. Phase C of a flush also needs the mode to still be `Streaming`.
- **Commit:** `eae0402`; test `a_parked_fsync_does_not_install_over_a_truncate` in `81e4d1e`

**2. [Rule 1 - Bug] A finalize could retire a cell that has a new writer**
- **Found during:** Task 2 review
- **Issue:** As first written, `finalize` retired the cell after any step error, also when a write handle had opened during the finalize. That handle would then get EBADF.
- **Fix:** With a new writer present, the cell lives on, also after an error.
- **Commit:** `eae0402`

**3. [Rule 1 - Bug] The `!finalize` path of `leave` could drop the work of a finalize in flight**
- **Found during:** Task 2 design
- **Issue:** With a stepwise finalize, `write_handles == 0` no longer means that the content is stored. Suppose a failed `O_TRUNC` open drops the count to 0 while another release is in a step's I/O phase. The old `leave(.., false)` would retire the cell and lose those dirty bytes.
- **Fix:** `leave` always finalizes, and the flag is gone. A failed truncate leaves a fresh state, and its finalize only applies the empty-file manifest rule.
- **Commit:** `eae0402`

### Plan facts

- **setattr of an open stream:** `setattr_size_open` runs its settle and its overlay switch with the cell locked. It is a rare size change of an open stream. The plan's D-02 list names writes, reads, flush and finalize. If the I/O ran unlocked there, a finalize could retire the cell in the middle of the size change.
- **Settle errors:** the old `settle_pending` printed an error and went on with a partial stream. `settle` returns EIO, so a read or a release does not use a stream that is missing pending bytes. In a write, an error in the drain of lined-up pending entries is only printed: the write itself is in, and the entries stay pending.
- **Push hook scope:** it fires in the unlocked Phase B of a stream step and of a flush snapshot. It never fires in a locked retry, so a park cannot hold the cell.
- **Rebase hook scope:** it now also fires after the splice of a closed-file truncate, with the transient cell locked. The truncate test parks there.
- **`rebase_step` with `usize::MAX`:** no caller passes it now. The no-copy branch and its doc stay. The finalize uses `final_step` (default `FINAL_STEP`, `test_set_final_step` in tests).
- **Truncate test:** "the open waited" is checked with a 300 ms `recv_timeout` that must time out while the truncate is parked. Then the open must finish within 5 s after the release.
- **GC test commits:** the GC thread takes the gate exclusively for `commit_cut`. `gc_online` takes the gate itself, so it is not wrapped (the std RwLock would deadlock). The dirty limit is 256 KiB, so rebases publish new roots during the GCs.

**Total deviations:** 3 auto-fixes (Rule 1) and 7 notes. **Impact:** all plan truths hold.

## For later plans

- Plans 08 and 10: `test_write` has more than one cell-lock hold (stream steps, the overlay switch, the rebase). Log the `Write` record in the hold that applies the write. That is Phase C of its own `stream_step`, the pending insert, or the overlay write. It is not after a later unlock.
- `with_cell` cannot run a protocol step (it gives no guard). New code that needs one locks the cell with `lock_cell` and checks `retired`.
- A cell with `write_handles == 0` can be live (a finalize in its I/O phase). `open_write`, `test_setattr_size` and `test_read` treat it as live. `retired` is the only "gone" signal.
- Stream steps run at most one queued pending entry per step. A reorder window full of small entries costs one stream clone per entry at settle time.
- Phase 20 (flusher): `finalize_steps`, `flush_cell` and `settle` can serve as the background steps. Any of them called from another thread must still run inside a shared gate hold (RESEARCH Pitfall 3).

## Issues Encountered

None.

## Known Stubs

None.

## Threat Flags

None. There is no new endpoint, file kind or trust boundary.
- T-19-SC: no new crate.
- T-19-23: `last_published_gen` check and refcounts in Phase C under the cell lock; `fsyncs_publish_in_order`, `a_parked_fsync_does_not_install_over_a_truncate`.
- T-19-24: reads and every Phase B stay inside the request's shared gate hold; `reads_during_rewrite_and_gc_never_fail`.
- T-19-25: finalize steps of at most `FINAL_STEP`; a stream clone is at most about 64 KiB; one pending entry is copied per step.
- T-19-26: retire only when clean and `write_handles == 0`, in one cell-lock hold with the map removal; `a_writer_that_opens_during_the_finalize_keeps_the_cell`.

## Self-Check: PASSED

- `crates/slicefs-cli/tests/lock_free_io.rs` (15 tests), `crates/slicefs-cli/tests/gc_online_tests.rs` and this SUMMARY: FOUND
- Commits `8d10747`, `eae0402`, `cc1f24e`, `47ede52` and `81e4d1e`: FOUND on HEAD (`git rev-list --count d4ebb2c..81e4d1e` prints 5)
