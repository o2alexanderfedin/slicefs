---
phase: 19-fine-grained-locks
plan: 15
subsystem: concurrency
status: complete
tags: [lock-05, step-token, finalize, setattr, transient-cell, forced-drop, gap-closure, d-02, g3]

requires:
  - phase: 19-fine-grained-locks
    provides: "19-14 step token (CellState::stepping, run_step, wait_step), unlocked retries, test_drop_steps / test_drop_every, hard_cap() = limit + limit/GROW_DIV + 2*REBASE_STEP; 19-07 retire and new-handle rules"
provides:
  - "finalize_steps: one unlocked step per pass under the step token (settle, rebase, flush, empty-file rule); no locked fallback"
  - "FINAL_TRIES (8): after 8 drops in a row a finalize stops consulting the drop hook (CellGuard::no_drop_hook)"
  - "finalize_locked: the locked form, only for test_destroy (no request runs)"
  - "FlushIo { Locked, Unlocked, Step } for flush_cell"
  - "A size-0 rebase root gets its empty manifest digest in Phase B; Phase C only swaps digests"
  - "setattr_size_open waits for the step token; its I/O runs as unlocked steps; STEP_TRIES then EIO; a cell retired during the wait makes the caller look up again"
  - "FileCell::transient: test_read and with_cell do not lock the cell of a closed-file size change"
  - "cap_step frees at most final_step per step (orchestrator-added)"
  - "stress_harness long_run knobs STRESS_DROP_EVERY and STRESS_DIRTY_LIMIT_KIB"
  - "test hook test_last_write_max_copy"
affects: [19-16, 20]

estimate:
  tokens: 140000
  tasks: 3
actuals:
  tokens: 19800
  tasks: 4
  commits: 4

plan_head_before: cb0d87d17e09b8d6a5cfcc3e4472f5a2984d0eee
plan_head_after: 1b52a4f495ae0e0215553148754bb6a45a42c51f
commits: 4

tech-stack:
  added: []
  patterns:
    - "Every request-path step that does I/O runs under the step token with the cell unlocked; the hold only checks the generation and swaps memory or digests"
    - "Size changes are writer-side: they wait for the token first, so they never drop a step in flight"
    - "A transient cell orders opens and size changes only; readers read the committed manifest"

key-files:
  created:
    - .planning/phases/19-fine-grained-locks/19-15-SUMMARY.md
  modified:
    - crates/slicefs-cli/src/filesystem.rs
    - crates/slicefs-cli/tests/lock_free_io.rs
    - crates/slicefs-cli/tests/lock_wait_rows.rs
    - crates/slicefs-cli/tests/stress_harness.rs

key-decisions:
  - "G3 (release part): the last release runs only unlocked steps under the step token; with no write handle only a forced drop can drop a finalize step, and after FINAL_TRIES (8) drops in a row the finalize stops consulting the hook. No give-up state."
  - "Every size change of an open cell waits for the step token (STEP_WAIT 15 s) before it changes the state; its I/O runs as unlocked steps (STEP_TRIES 16, then EIO)."
  - "Truncate to 0 of an open file: the manifest read and the empty manifest node are an unlocked step too (not in the plan; Rule 2)."
  - "Reads and getattr do not lock a transient cell; the closed-file splice stays under its own lock (the one accepted I/O under a cell lock on a request path)."
  - "Hard-cap gate steps free at most final_step each; the writer steps again until it is under the cap (orchestrator-added)."

patterns-established:
  - "Locked step forms (settle_locked, stream_step_locked, rebase_step, FlushIo::Locked) are called only from finalize_locked (destroy) and the commit cut"

requirements-completed: []

duration: about 1 h 25 min wall time (machine load average 21-30)
completed: 2026-11-22
---

# Phase 19 Plan 15: Unlocked finalize, size changes under the step token, transient-cell reads Summary

**The last release, every size change of an open file, and the hard-cap gate now do their chunk, hash, store put and flush work only in unlocked steps under the step token. Reads do not lock the transient cell of a closed-file size change. With steps forced to drop, readers wait less than 100 ms while a retry is parked for 300 ms.**

## Performance

- **Duration:** about 1 h 25 min, from about 21:25Z to 22:47Z on 2026-11-22. The machine load average was 21-30.
- **Tasks:** 3 plan tasks and 1 task that the orchestrator added.
- **Files:** 4 modified.

## Accomplishments

- **Task 1: the last release (finalize).**
  - `finalize_steps` runs one step per pass, always with the cell unlocked under the step token:
    - pending writes: one stream step;
    - a dirty overlay: one rebase step of at most `final_step`;
    - a dirty stream: `flush_cell(FlushIo::Step)`. If an fsync installed a newer snapshot first, the next pass sees the result. There is no locked retry.
    - nothing written: `empty_rule`. The manifest read and `prepare_manifest(&[])` run as a step. The hold installs the digest only if the generation and `last_committed_root` did not change.
  - If a token is set when a pass starts, the finalize waits for it with the cell released (`wait_step`), then looks again.
  - The finalize counts drops in a row. At `FINAL_TRIES` (8) it sets `CellGuard::no_drop_hook`, and `forced_drop` then gives false for the rest of that finalize.
  - The 19-07 rules stay: a new write handle stops the finalize. The cell is retired in the hold that sees it clean with no write handle. After an error the cell is retired unless a writer opened.
  - `finalize_locked` (with `destroy_steps`) is the locked form, only for `test_destroy`.
  - `flush_cell` takes `FlushIo { Locked, Unlocked, Step }`. `Locked` is used only by the commit cut (exclusive gate) and destroy.
  - `step_io` returns the empty manifest digest for a size-0 root. `step_publish` only swaps digests and refcounts (it no longer calls `clear_manifest`).
- **Task 2: size changes and the transient cell.**
  - `setattr_size_open` is a loop. Each pass first waits for the token (`wait_step`), also for the memory-only changes. Its I/O runs as unlocked steps:
    - the content check of an extension of a fresh stream;
    - for a truncate to 0, the manifest root read and the empty manifest node;
    - for a cut of a stream, the settle (one pending write per step), then `to_overlay`.
  - A dropped step is tried again: at most `STEP_TRIES` in a row, then EIO, with a note that is printed after the hold.
  - It returns `Ok(false)` if the cell was retired during the wait. `setattr_size_logged` then loops into case B (closed file), and `open_write` looks up the cell again.
  - `FileCell` has an immutable `transient: bool`, set only by `FileCell::transient` in case B of `setattr_size_logged`. `test_read` and `with_cell` (getattr) do not lock a transient cell, and read the committed manifest. The splice stays under the transient cell, so an open or a second size change still waits for it (19-07 order). The comment says why.
  - Removed: `to_overlay_locked` and `clear_manifest` (no callers).
- **Task 3: forced-drop evidence.**
  - `lock_wait_rows.rs`: `readers_do_not_wait_when_steps_drop_rows` (the write's rebase step is dropped and its unlocked retry is parked) and `release_with_drops_does_not_block_readers_rows` (the same in the finalize of the last release). Helpers: `held_hook_at`, `two_readers`, `assert_drop_rows`.
  - `lock_free_io.rs`: `forced_drops_keep_content_and_the_cap` (4 writers, 1 reader, `test_drop_every(3)`, 400 ms) and `sixteen_dropped_stream_steps_give_eio`.
  - `stress_harness.rs`: `STRESS_DROP_EVERY` (0..=1000) and `STRESS_DIRTY_LIMIT_KIB` (0..=1048576) in `Config::from_env`. `run` applies them only when they are set. The default tests do not change.
- **Orchestrator-added task: the cap-gate step size.**
  - `cap_step` frees `excess.max(REBASE_STEP).min(final_step)`. The gate loop steps again until the cost is under the hard cap. So after a switch of up to `REORDER_WINDOW` of pending writes, no hold copies more than `final_step`. A split extent head can add up to `MIN_TAKE` (64 KiB).
  - The two stale comments are fixed: the peak in the `DIRTY_LIMIT` doc, and "after a switch it comes down with each write" in `write_in_cell`.
  - New hook `test_last_write_max_copy` (the most bytes that one unlocked rebase step of this thread's last write copied in its Phase A hold). New test `a_switch_above_the_cap_steps_at_most_the_final_step`.

## Task Commits

1. **Task 1: the last release stores a file in unlocked steps only** (TDD): `263e89e`
2. **Task 2: size changes wait for the step token; reads skip the transient cell** (TDD): `6606268`
3. **Task 3: forced-drop evidence: lock rows, a concurrent run, harness knobs:** `69b0b9c`
4. **Orchestrator-added: a hard-cap gate step copies at most final_step in its hold** (TDD): `1b52a4f`

## TDD and red-run evidence

Each new test first ran against the tree before its fix, in a scratch worktree. Each test was committed with its fix, so no commit is red.

- **Task 1** (against `cb0d87d`):
  - `the_last_release_retries_dropped_steps_unlocked`: red, `the read waited for the retry of the release: Timeout`. The old fallback ran the retry with the cell locked.
  - `finalize_ends_under_forced_drops`: red, `8 dropped attempts, then the published one, left: 2, right: 9`. The old code published with a locked step after one drop.
  - `the_last_release_of_a_stream_flushes_unlocked`: **green on the old tree.** The old finalize flush was already unlocked; only the locked retry after it was removed. The test now guards the new token form.
- **Task 2** (against the Task 1 commit `263e89e`):
  - `a_size_change_of_an_open_stream_runs_its_steps_unlocked`: red, `a stream step of the size change reached its I/O phase: Timeout`. The old locked settle does not run the push hook, so the step never got to an unlocked I/O phase.
  - `a_read_does_not_wait_for_a_closed_file_truncate`: red, `the read waited for the splice: Timeout`.
  - `a_size_change_waits_for_a_finalize_step_without_the_cell`: red, `the size change waited for the finalize step: Ok(Ok(()))`. It returned at once.
  - `an_o_trunc_open_waits_for_a_finalize_step` (added, see deviations): red, `Ok(Ok((3, true)))`. The open returned at once.
- **Orchestrator-added task** (against `69b0b9c`, with the counter in place and without `.min(final_step)`): `a_switch_above_the_cap_steps_at_most_the_final_step` red, `one hold copied 1976321 bytes, more than the final step 1048576 + MIN_TAKE`. Green with the fix.
- **Task 3 mutation run** (after the Task 3 commit): `rebase_cell` was changed locally so that the retry after a dropped first attempt is the locked `rebase_step`. `readers_do_not_wait_when_steps_drop_rows` was red: `the reads did not end while the retry was parked` (the readers were blocked until the hook went on by itself after 5 s). The change was reverted with `git checkout -- crates/slicefs-cli/src/filesystem.rs`; `git status` was clean.

## Lock rows (lock-stats, this machine)

| Test | file_cell wait max | readers wait max | hold max (any site) | slowest read | rebase steps |
|---|---|---|---|---|---|
| readers_do_not_wait_when_steps_drop_rows | 4.1 us | 1.9 us | 92.5 us (file_cell) | 267.5 us | 2 |
| release_with_drops_does_not_block_readers_rows | 4.7 us | 0.4 us | 87.8 us (file_cell) | 248.0 us | 2 |

The bound is 100 ms for waits, and 300 ms (the park time) for holds. The two older row tests stayed green.

## By-hand long_run with forced drops

Command: `STRESS_SERIALIZE=0 STRESS_SECS=5 STRESS_THREADS=4 STRESS_FILE_MIB=4 STRESS_DROP_EVERY=3 STRESS_DIRTY_LIMIT_KIB=256 cargo test -p slicefs-cli --test stress_harness long_run -- --ignored --nocapture` (debug build).

- `STRESS_MODE=multi-handle`: `ops=3232 dirty_total=0`, no mismatch line, exit 0.
  - `op=read n=2480 p50_us=25.2 p99_us=20430.8 max_us=36929.5`
  - `op=write n=1313 p50_us=140.9 p99_us=41746.4 max_us=616562.7`
  - `op=fsync n=135 p50_us=21430.3 max_us=157024.3`; `op=append n=617 max_us=503316.5`
- `STRESS_MODE=one-file`: `ops=1576 dirty_total=0`, no mismatch line, exit 0.
  - `op=read n=1501 p50_us=30.8 p99_us=21037.1 max_us=95551.5`
  - `op=write n=864 p50_us=401.7 p99_us=251789.3 max_us=704643.1`
  - `op=fsync n=75 p50_us=22511.6 max_us=178389.0`

## Deviations from Plan

### Orchestrator-added task

**1. The hard-cap gate step is limited to `final_step`** (added by the orchestrator, not in the plan)
- **Issue:** 19-14 reported that the write that switches up to 64 MiB of pending writes into an overlay above the hard cap freed all of the excess in one step (22 MB copied in the cell hold in a probe).
- **Fix:** `cap_step` frees at most `final_step` per step. The writer steps again until the cost is under the cap, so "a write never returns above the cap" still holds. Memory stays bounded: one step copy of at most `final_step` + `MIN_TAKE`.
- **Test bound:** the test asserts `max copy <= final_step + MIN_TAKE`, not `<= final_step`. `Overlay::select` can take a whole extent that is up to `need + MIN_TAKE` long. The test sets `final_step` to `REBASE_STEP` (1 MiB), because the over-limit step of a write (`rebase_cell`) frees `REBASE_STEP` too. With a final step below 1 MiB that step, not the gate, would be the largest copy. In production `FINAL_STEP` is 8 MiB.
- **Files modified:** `crates/slicefs-cli/src/filesystem.rs`, `crates/slicefs-cli/tests/lock_free_io.rs`
- **Commit:** `1b52a4f`

### Auto-fixed Issues

**2. [Rule 2 - Correctness] Truncate to 0 of an open file did a store put in the hold**
- **Found during:** Task 2. `clear_manifest` read the manifest node and put the empty manifest node with the cell locked. The plan called this branch memory-only, but truth 8 and the success criteria do not allow I/O in the hold on a request path.
- **Fix:** the manifest root read and `prepare_manifest(&[])` run as one unlocked step under the token. The hold checks the generation and `last_committed_root`, then swaps the state, the refcounts and the digest. `clear_manifest` has no callers now and was removed.
- **Commit:** `6606268`

**3. [Rule 2 - Correctness] `with_cell` (getattr) also skips the transient cell**
- getattr locked the transient cell and so waited for the splice of a closed-file size change. It now reads the inode (the old or the new size).
- **Commit:** `6606268`

**4. Test changes against the plan text**
- `the_last_release_retries_dropped_steps_unlocked` parks call 2 (the first retry), not call 3. With `test_drop_steps(2)` the old code ran call 2 as a locked step and call 3 as an unlocked attempt. A park of call 3 would not be red on the old tree.
- `finalize_ends_under_forced_drops` sets the final step to 8 MiB, so the whole overlay is one step and the count is exactly 8 + 1.
- The tests that call `reload_and_replay` use a store with a write log and the request handlers (the `test_*` calls log nothing). `lock_free_io.rs` now has `mod common;`.
- Added `an_o_trunc_open_waits_for_a_finalize_step`: the O_TRUNC open now waits for the token with its stripe held. The test shows that it does not deadlock, and it also ran under `--features deadlock-detect`.
- Added `sixteen_dropped_stream_steps_give_eio` (extra scope 3).

**5. Commit content**
- The Task 2 workspace run also had the Task 3 harness knobs in the tree (they are off by default) and the uncompiled `lock_wait_rows.rs` change. Only the Task 2 files were committed.

## Extra scope 3: test_drop_every and the EIO exit

- `test_drop_every` is covered by `finalize_ends_under_forced_drops` (k = 1), `forced_drops_keep_content_and_the_cap` (k = 3), `sixteen_dropped_stream_steps_give_eio` (k = 1) and the by-hand long_run (k = 3).
- The EIO exit after `STEP_TRIES` (16) dropped stream steps is covered by `sixteen_dropped_stream_steps_give_eio`: the write returns EIO, the content does not change, and the same write then goes in.

## Paths that still do I/O with a cell locked

- `finalize_locked` / `destroy_steps` (`test_destroy`: no request runs): `settle_locked`, `stream_step_locked`, `rebase_step`, `flush_cell(FlushIo::Locked)`, `set_manifest`.
- `commit_cut_begin`: `flush_cell(FlushIo::Locked)` under the exclusive gate (no request runs).
- The closed-file splice under the transient cell (`setattr_size_closed`). No reader or writer waits for it; an open or a second size change of that inode does.
- The code says this at each of these places.

## Not covered or open

- **Open point for a decision:** the orchestrator text says "later writes or the release free the rest". This plan keeps the gate loop: the switching write still frees the whole excess down to the cap, in several steps of at most `final_step`. The other reading (one step per write, the cost stays above the cap for a while) would break "a write never returns above the cap" and the cap tests. Please confirm the loop.
- The doc comment of `held_hook` in `lock_wait_rows.rs` still says "Later calls (finalize, with the cell locked) do not park". The finalize no longer locks the cell. Only the comment is stale; it was not changed, so that no extra commit and workspace run was needed.
- `stress_harness` binary (default tests, warm, load average 21-30): 9.61 s, 9.18 s, 9.18 s (under 10 s).
- Tests that require a read to end within 300 ms can fail on a very slow CI machine with no bug (as in 19-14).
- The closed-file races with unlink and with a fields setattr stay deferred (`deferred-items.md`).

## Threat model notes

- **T-19-57:** with no write handle, only the test hook can drop a finalize step. `finalize_ends_under_forced_drops` shows the finalize ends and retires the cell. `a_size_change_waits_for_a_finalize_step_without_the_cell` shows that a size change waits for the step.
- **T-19-58:** `a_read_does_not_wait_for_a_closed_file_truncate` reads the old content during the splice and the new content after it.
- **T-19-59:** every new wait is `wait_step` (`STEP_WAIT` 15 s, then EIO). `FINAL_TRIES` bounds the forced drops of a finalize.
- **T-19-60:** the parked retry is at least 300 ms against a 100 ms bound. The hooks go on by themselves after 5 s.

## Verification

- `cargo test -p slicefs-cli --test lock_free_io -- --nocapture`: 27 passed on the final tree.
- Task 1 suites: gc_online (16), write_log_replay (35), posix_compliance (40), streaming (25), fsync (7): all green.
- Task 2 suites: write_path (24), posix_compliance (40), multi_handle_posix (13), write_log_replay (35): all green. `lock_free_io` under `--features deadlock-detect`: 24 passed.
- `cargo test -p slicefs-cli --features lock-stats --test lock_wait_rows -- --nocapture`: 4 passed.
- Full gate of Task 3 (`cargo test --workspace`, three `cargo check` runs, `cargo check --features deadlock-detect`, `serialize_off_target` under deadlock-detect): exit 0, 68 `test result: ok` lines, no FAILED, no rank violation, no deadlock line.
- `cargo test --workspace` exited 0 before each of the 4 commits (67 `test result: ok` lines each).
- Acceptance greps on the final tree:
  - `unlocked = false`: 0. `self.rebase_step(` in `finalize_steps`: 0. `set_manifest(ino` in `finalize_steps`: 0. `fn finalize_locked`: 1.
  - `transient: bool`: 2. `fn a_read_does_not_wait_for_a_closed_file_truncate`: 1.
  - `fn readers_do_not_wait_when_steps_drop_rows`: 1. `fn release_with_drops_does_not_block_readers_rows`: 1. `STRESS_DROP_EVERY` in stress_harness.rs: 3.

## Self-Check: PASSED

- The four modified source and test files and this SUMMARY exist.
- Commits `263e89e`, `6606268`, `69b0b9c` and `1b52a4f` are on HEAD.
