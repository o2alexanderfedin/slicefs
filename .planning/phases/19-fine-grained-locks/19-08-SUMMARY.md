---
phase: 19-fine-grained-locks
plan: 08
subsystem: concurrency
status: complete
tags: [lock-04, lock-05, d-08, d-11, d-12, d-17, write-log, o_append, posix]

requires:
  - phase: 19-fine-grained-locks
    provides: "19-04 log_append / log_write_out in the store; 19-05 Handle { ino, write, append }; 19-06/19-07 cell protocol (stream_step Phase C, overlay write in one hold, flush_cell Phase C, stepwise finalize)"
provides:
  - "fs.log_append (encode + queue, None while replaying) and fs.log_write_out; log_record = append + write-out"
  - "write_logged: the Write record (inode of the cell, effective offset) is appended in the cell-lock hold that applies the write"
  - "Truncate appended in the open's truncate hold; Release only by the release that retired the cell; Flush only when its Phase C installed"
  - "O_APPEND: Handle.append from open/create flags, handle_write_flags with the request flags, EOF under the cell lock per loop turn, EFBIG on EOF + len"
  - "test_write_at (returns the effective offset)"
  - "Tests: parallel_overlapping_writes_replay_to_the_same_content, two_write_handles_release_replays, visibility_across_ro_rw_append_handles, all_or_none_reads, appends_never_overlap and 6 more"
affects: [19-09, 19-10, 19-11, 20]

estimate:
  tokens: 120000
  tasks: 3
actuals:
  tokens: 10436
  tasks: 3
  commits: 3

plan_head_before: b50db77f2b62eef9cd7e60bc74ae59ec941e105a
plan_head_after: 80b42c395615921ee51aad534c3474df2f6663d5
commits: 3

tech-stack:
  added: []
  patterns:
    - "Logged fs entry: `*_logged(.., time: Option<(i64, u32)>) -> (Result, Option<Lsn>)`; the record is appended in the apply's hold, the handler calls log_write_out after the call and before the reply"
    - "The fs method reports whether the apply happened (retired, Phase C installed); the handler no longer looks the handle up to decide"
    - "An append takes EOF at the top of every write-loop turn, so a dropped publish retries with the new EOF"

key-files:
  created: []
  modified:
    - crates/slicefs-cli/src/filesystem.rs
    - crates/slicefs-cli/src/handlers.rs
    - crates/slicefs-cli/src/fuse_callbacks.rs
    - crates/slicefs-cli/tests/multi_handle_posix.rs
    - crates/slicefs-cli/tests/write_log_replay_tests.rs

key-decisions:
  - "A Release record is appended only by the release that moved the cell to retired; a finalize stopped by a new writer logs none (the later last release does)"
  - "A Flush record is appended only when flush_cell reached Phase C (it installed); an empty, clean or overlay flush logs none"
  - "Replay never appends: a Create logged with O_APPEND gives a replay handle that writes at the logged offsets"
  - "A write that applied and then failed (rebase or budget step) keeps its record; the LSN is returned beside the error and written out"

patterns-established:
  - "Write-path records are appended under the cell lock of their apply and written out after it (D-08)"

requirements-completed: []

duration: 45min
completed: 2026-11-22
---

# Phase 19 Plan 08: Write-path logging under the cell lock and O_APPEND Summary

**Write, Truncate, Release and Flush records are appended in the cell-lock hold of their apply and written out before the reply. `O_APPEND` handles write at the end of file taken under the cell lock. Tests show the POSIX.1 2.9.7 rules with RO, RW and `O_APPEND` handles.**

## Performance

- **Duration:** about 45 min
- **Started:** 2026-11-22T07:08:38Z
- **Completed:** 2026-11-22T07:48:11Z
- **Tasks:** 3
- **Files modified:** 5

## Accomplishments

- **D-08 (write path):**
  - `fs.log_append` and `fs.log_write_out` wrap the store's split from Plan 04. `log_append` returns `None` while the log is replayed.
  - `write_logged` appends the `Write` record at the three apply sites, each in the hold that applies the write:
    - the overlay write;
    - Phase C of the in-order `stream_step`, before `drain_lined_up` can unlock;
    - the pending insert.
  - Gap fills, dropped steps and `to_overlay` log nothing. The inode comes from the cell, so the after-apply lookup in `handle_write` is gone (RESEARCH dependency 15).
- **The other write-path records:**
  - `Truncate` is appended after `setattr_size_open(0)` in `open_write`, in the same hold.
  - `Release` is appended only when `release_write` moved the cell to `retired`.
  - `Flush` is appended only when `flush_cell` returned `Ok(true)`, that is, Phase C installed.
  - The handlers write each record out after the call.
- **D-12 / D-17 (`O_APPEND`):**
  - `Handle.append` is set at open (`open_logged`) and at create (`test_create_full`).
  - `handle_write_flags` passes the request flags. The FUSE `write` callback gives it the request's `OpenFlags`, and `handle_write` is a wrapper with flags 0.
  - An append ignores the request offset. At the top of each loop turn, under the cell lock, `append_eof` takes the state's `byte_count` (if it has bytes or is an overlay), else the committed inode size. The turn then checks EOF + len against `MAX_FILE_SIZE` (EFBIG).
  - The `Write` record carries the effective offset. `test_write_at` returns it.
- **D-11 (tests):** visibility with RO, RW and append handles, all-or-none reads during rebases, and K x N appends that never overlap and that replay to the same file.

## Task Commits

1. **Task 1: Write records appended inside the cell lock** - `cb6a3a0` (feat)
2. **Task 2: O_APPEND, write flags; Truncate, Release and Flush in the apply hold** - `dab9d10` (feat)
3. **Task 3: POSIX.1 2.9.7 cases with RO, RW and O_APPEND handles** - `80b42c3` (test)

## TDD evidence

- **Task 1 (tracer):** both tests passed on their first run, and the tracer gate re-ran the Task 1 verify (25 passed).
  - Mutation check: `handle_write` was changed back to "apply, `yield_now`, then log through `ino_of_fh`". `parallel_overlapping_writes_replay_to_the_same_content` was then red in 1 of 3 runs ("left/right" dump mismatch at `check_with`).
  - Only the last write to each of the 3 blocks decides the content, so one run can miss an inversion. For this reason the test does 4 rounds (0.75 s).
- **Task 2:** the tests were written first and run against the Task 1 code.
  - Compile errors: `handle_write_flags` and `test_write_at` did not exist yet.
  - `two_write_handles_release_replays` was **red**: 1 `Release` record after A's release, expected 0.
  - `a_file_created_with_o_append_replays_its_overwrites` was **red**: content `XXcond;`, expected `XXrst;second;`.
  - `an_o_trunc_open_beside_a_write_handle_replays` was green before the fix, as expected: the old code also logged the `Truncate`, only later.
  - All passed after the fix.
  - Mutation check: without `!replaying` in the append rule, the O_APPEND create test was **red** (replayed size 15, live size 13).
- **Task 3:** all three tests passed on their first run. They test the code of Task 2 and Plans 06-07. Two mutation checks:
  - The append keeps the request offset: `appends_never_overlap` is **red** (size 1000610, expected 204800).
  - `handle_read` reads in two halves with a `yield_now` between them: `all_or_none_reads` is **red** ("a mix of writes" in both reader threads).

## Verification

- `cargo test -p slicefs-cli --test write_log_replay_tests`: 28 passed (about 28 s). These include `parallel_overlapping_writes_replay_to_the_same_content` (4 rounds, 0.75 s), `a_failed_write_appends_no_record`, `two_write_handles_release_replays`, `an_o_trunc_open_beside_a_write_handle_replays` and `a_file_created_with_o_append_replays_its_overwrites`.
- `cargo test -p slicefs-cli --test multi_handle_posix`: 13 passed.
  - The tests take 1.5-1.7 s. The warm debug binary takes 3.3 s and 5.8 s wall time in two runs, inside the 6 s budget. The first run of a new binary took 22 s, which is the macOS start-up cost of a new executable.
  - The new tests are `append_handle_ignores_the_request_offset`, `append_starts_at_the_committed_size_and_stops_at_the_max`, `a_write_request_with_o_append_appends`, `visibility_across_ro_rw_append_handles`, `all_or_none_reads` and `appends_never_overlap`.
- `cargo test --workspace` exited 0 before each task commit (66 `test result: ok` lines, no `FAILED`, no `lock rank violation`). `cargo check -p slicefs-cli --all-targets --features lock-stats` exited 0 after the Task 2 lock-site changes.
- The full suite command from 19-VALIDATION.md, on the Task 3 tree: `FULL_EXIT=0`, 66 `test result: ok` lines, no `FAILED`, no `error`, no `lock rank violation`.
- `cargo test -p slicefs-cli --features lock-stats --test lock_stats_report`: 1 passed (`LSR_EXIT=0`).
- Acceptance greps:
  - `fn log_append` 1 (filesystem.rs), `if let Some(ino) = fs.ino_of_fh(fh)` 0 (handlers.rs), `fn parallel_overlapping_writes_replay_to_the_same_content` 1;
  - `fn handle_write_flags` 1, `fn test_write_at` 1, `handle_write_flags` in fuse_callbacks.rs 2, `l.log(fs,` 11 (was 15);
  - `fn appends_never_overlap` 1, `fn all_or_none_reads` 1, `fn visibility_across_ro_rw_append_handles` 1.
- Requirements: LOCK-04 and LOCK-05 are not marked complete, because 19-10 (LOCK-04) and 19-11 (LOCK-01, LOCK-04, LOCK-05) still list them.

## Decisions Made

See `key-decisions`. All of them can be changed later.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Bug] Replay of a file created with O_APPEND would append**
- **Found during:** Task 2 design
- **Issue:** `Op::Create` carries the create flags, and replay passes them to `handle_create`. With `O_APPEND` set, the single replay handle of the inode would be an append handle. It would then ignore the logged offsets, also those of writes from other, non-append handles.
- **Fix:** The append rule in `write_in_cell` is `(handle flag || request flag) && !replaying`. The records carry the effective offsets.
- **Test:** `a_file_created_with_o_append_replays_its_overwrites` (red without the guard, see TDD evidence).
- **Commit:** `dab9d10`

**2. [Rule 1 - Bug] A write that applied and then failed would lose its record's write-out**
- **Found during:** Task 1 design
- **Issue:** The plan's example signature `Result<(u32, Option<Lsn>), i32>` drops the LSN when the write applied and a later rebase or budget step returned an error.
- **Fix:** `write_logged` returns `(Result<(u32, u64), i32>, Option<Lsn>)`, and the handler writes the LSN out in both cases.
- **Commit:** `cb6a3a0`

### Plan facts

- **EFBIG test:** the plan's case "an append past MAX_FILE_SIZE returns EFBIG" uses a stream that setattr extended to `MAX_FILE_SIZE - 10` (only the size is recorded). The first draft also wrote 10 bytes at that end. The run hung, because a fresh stream fills the gap up to the offset with streamed zeros (1 TiB). This was already the behavior for any write far past the stream end. The test now checks only the EFBIG case and truncates back to 0 before the release, so the finalize pads no zeros.
- **Release in the hold of the retire:** the record is appended in the last hold of the finalize, so writes that were logged during the finalize's I/O phase can come before it. Replay gives the same content: those writes go to the replay handle, and the `Release` then finalizes it.
- **Gate:** the FUSE callbacks hold the shared commit gate across the call and the write-out, as Plan 04 requires. The handler tests call the handlers with no gate, as before.

**Total deviations:** 2 auto-fixes (Rule 1) and 3 notes. **Impact:** all plan truths hold.

## For later plans

- Plan 10: the namespace and setattr/xattr records still log after the apply (`l.log(fs, ..)`, 11 sites). Use the same shape: a `*_logged` fs method that appends in the apply's hold and returns the LSN, then `log_write_out` in the handler.
- `Logged::log` (`log_record`) stays for those sites. `log_record` is now `log_append` plus `log_write_out`.
- `flush_cell` returns `Ok(true)` when Phase C ran. `finalize_steps` and the commit relog ignore the value.
- `open_handle` takes `append`. `test_create` opens with `append = false`, and `test_create_full` sets it from the flags.

## Issues Encountered

- The first run of the EFBIG case streamed zeros toward 1 TiB (see Plan facts). The process was stopped and the test changed.

## Known Stubs

None.

## Threat Flags

None. There is no new endpoint, file kind or trust boundary.
- T-19-SC: no new crate.
- T-19-27: `checked_add` of EOF + len against `MAX_FILE_SIZE` (EFBIG), with nothing allocated by offset or size; `append_starts_at_the_committed_size_and_stops_at_the_max`.
- T-19-28: the request offset is ignored for an append, and EOF is taken under the cell lock; `appends_never_overlap` passes wrong offsets.
- T-19-29: records are appended in the apply's hold, once per applied write; `parallel_overlapping_writes_replay_to_the_same_content`, `two_write_handles_release_replays`.
- T-19-30: the record is built in the locked section from the cell's inode; there is no after-apply lookup.

## Self-Check: PASSED

- `crates/slicefs-cli/tests/multi_handle_posix.rs`, `crates/slicefs-cli/tests/write_log_replay_tests.rs` and this SUMMARY: FOUND
- Commits `cb6a3a0`, `dab9d10` and `80b42c3`: FOUND on HEAD (`git rev-list --count b50db77..80b42c3` prints 3)
