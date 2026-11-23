---
phase: 19-fine-grained-locks
plan: 10
subsystem: concurrency
status: complete
tags: [lock-04, d-04, d-08, write-log, replay, namespace, crash-replay]

requires:
  - phase: 19-fine-grained-locks
    provides: "19-03 create_inode_at / create_directory_at; 19-04 log_append / log_write_out; 19-08 write-path records in the cell hold; 19-09 namespace stripes, with_inode_locked, the transient cell of a closed-file size change"
provides:
  - "LogOp callback (filesystem.rs): a namespace function calls it with the op of its record in the lock hold of its apply"
  - "*_logged fs functions: create_logged, create_full_logged, mkdir_logged, mknod_logged, symlink_logged, link_logged, unlink_logged, rmdir_logged, rename_logged, setattr_logged, setattr_size_logged, setxattr_logged, removexattr_logged; the old test_*/simulate_* names are wrappers with a no-op logger"
  - "handle_create_at, handle_mkdir_at, handle_mknod_at, handle_symlink_at (Some(ino): create at that inode number)"
  - "Logged::sink (append + keep the LSN); Logged::log is gone; every mutating handler writes out its LSN before the reply"
  - "Replay creates at the logged inode number (no advance_next_ino, no got == ino check); skips a Setattr of an inode it already reclaimed"
  - "tests/common/mod.rs: new_fs_with, reload_and_replay(_with), dump, dump_open, dump_by, segment_paths, log_len, first_differences"
  - "Harness Config.crash and crash_replay_matches_apply"
affects: [19-11, 20, 21]

estimate:
  tokens: 120000
  tasks: 3
actuals:
  tokens: 17719
  tasks: 3
  commits: 5

plan_head_before: b517f103f698ca302c84e6b5a8e64ef4bd77d7e5
plan_head_after: f1b752ca0e5fdb6c5e938f52bc814783a95d792c
commits: 5

tech-stack:
  added: []
  patterns:
    - "Namespace handler: `let l = logged(); let mut lsn = None; let r = fs.x_logged(.., &mut l.sink(fs, &mut lsn)); fs.log_write_out(lsn); reply(r)`"
    - "A fs function calls `log(op)` as the last step of its success path, before the guard of its apply drops"

key-files:
  created:
    - crates/slicefs-cli/tests/common/mod.rs
  modified:
    - crates/slicefs-cli/src/filesystem.rs
    - crates/slicefs-cli/src/handlers.rs
    - crates/slicefs-cli/src/oplog.rs
    - crates/slicefs-cli/tests/write_log_replay_tests.rs
    - crates/slicefs-cli/tests/multi_handle_posix.rs
    - crates/slicefs-cli/tests/stress_harness.rs

key-decisions:
  - "The log callback is `&mut dyn FnMut(Op)`: the fs function builds the op (it has every argument and the allocated inode), and the handler's closure adds the frozen time, appends and keeps the LSN. Replay needs nothing more, because log_append returns None while replaying."
  - "A setattr with a size logs two records: a size-only Setattr in the cell hold of the size change (the open cell or the transient cell), and then a Setattr with the other fields (size None) in the stripe-and-cell hold. The second record is logged also when all other fields are None, because that hold sets the ctime. One record in the stripe hold would let a write land between the size change and its record."
  - "create, mknod and symlink take the new inode number before the stripes and lock [parent, ino] (new_entry): a request that finds the new name and works on the inode by number waits until the create record is appended. mkdir keeps the parent stripe only (the store takes the number and adds the entry in one call; deferred)."
  - "A namespace record is appended at the end of the success path, inside the hold. A request that fails after a partial change logs nothing, as before this plan."
  - "The crash test stops the periodic commits in the last 250 ms of the run and makes no final commit (Config.crash), so the log is not empty at the crash."

patterns-established:
  - "Every record kind is appended under the locks of its apply and written out after they are released, before the reply (D-08)"

requirements-completed: []

duration: 85min
completed: 2026-11-22
---

# Phase 19 Plan 10: Namespace records under the apply locks, replay by logged inode, crash replay of the harness Summary

**Every namespace record is appended in the stripe (and cell) hold of its apply and written out before the reply. Replay creates each inode at its logged number, so creates can be logged in any inode order. After a crash, replay of the log of a serialize-off harness run gives the live state.**

## Performance

- **Duration:** about 85 min (with the soak and the create-window fix after the review)
- **Started:** 2026-11-22T09:15Z
- **Completed:** 2026-11-22T10:40Z
- **Tasks:** 3
- **Files modified:** 7 (1 created)

## Accomplishments

- **D-08 (namespace path):**
  - The create family appends its record in the parent-stripe hold. For create, this is the hold that also counts the handle and opens the cell.
  - Link, unlink, rmdir and rename append in their `lock_namespace` hold. setattr, setxattr and removexattr append in their `with_inode_locked` hold (stripe, then cell).
  - A size change of a setattr appends a size-only record in its own cell hold (see key-decisions).
  - `grep -c 'l.log(fs,' handlers.rs` is 0, `fn log(&self, fs` is 0, and `log_write_out` occurs 15 times.
- **D-08 (replay by logged inode):**
  - `handle_*_at(.., Some(ino))` uses `create_inode_at` / `create_directory_at`. Replay calls them with the logged inode.
  - `advance_next_ino` and the four `got == *ino` checks are gone from `oplog.rs`.
  - An inode that already exists makes the create fail, and replay stops with an error that names the record (T-19-38).
- **19-09 known item:** replay skips a `Setattr` of an inode it already reclaimed, as it skips xattr records. Test: `a_setattr_of_an_open_orphan_replays`.
- **19-08 known item:** `crash_replay_matches_apply` compares dumps only, so two `Release` records for one inode are accepted.
- **Shared helpers:** `tests/common/mod.rs` is used by `write_log_replay_tests`, `multi_handle_posix` and `stress_harness`, and their own copies are gone. The dump reads each file in 64 KiB pieces, never the whole file.
- **ROADMAP criterion 5 (code part):** `crash_replay_matches_apply` runs in many-files and one-file mode (4 threads, serialize off, commit thread on). It crashes, replays and gets equal dumps. Replayed logs had 218-554 records.

## Task Commits

1. **Task 1: Create logged under the parent stripe and replayed by logged inode** (tracer): `5fe95b2`
2. **Task 2: Every other namespace record under its stripes; no after-apply logging** (TDD, with the flush mtime fix): `bd1ca60`
3. **Task 3: Shared test helpers and crash replay of the harness** (TDD): `f960da3`
4. **Fix: a new file is unreachable by number until its create record is appended** (deviation 3): `f1b752c`
- Plan metadata: `6d7d2d9` (first SUMMARY), then the docs commit with this update

## TDD evidence

- **Task 1 (tracer):** `reverse_order_creates_replay` passed. The tracer gate re-ran the Task 1 verify after the commit: 30 passed. The old replay would fail this test: `advance_next_ino(7)` followed by `advance_next_ino(6)` gives 8 for the second create, not 6.
- **Task 2:** the tests were written first and run against the Task 1 code.
  - `a_setattr_of_an_open_orphan_replays` was **red**: "write log record 1 (setattr) did not replay".
  - `a_failed_namespace_op_appends_no_record` was green. The old code also logged only on success.
  - The first form of `parallel_namespace_ops_replay` (own names per thread, as the plan says) was green. It stayed green in 5 of 5 runs even with a mutation: rename and unlink logged after the apply, with a `yield_now` between. With own names, an inversion can show only in the last change of each shared directory.
  - The test now uses one shared pool of 6 names in each of two subdirectories. Under the same mutation it was **red** in 4 of 5 runs ("record 12013 (create) did not replay", "record 1435 (link) did not replay"). Without the `yield_now`, which is the exact old code shape, it was red in 1 of 5.
  - With the real code it was then **red** in 4 of 6 runs, from a live bug: the dump differed in one file's mtime. See deviation 1. After the fix it passed 12 of 12.
  - `a_utimens_during_the_close_io_is_kept` is red without the fix ("left: (1795340182, 24631000), right: (1500000000, 7)") and green with it.
- **Task 3:** `crash_replay_matches_apply` passed on its first run and in 5 more runs. Mutation checks:
  - Rename records not logged: **red** in 2 of 2 runs ("record 24 (unlink) did not replay").
  - Old create replay (`advance_next_ino` plus `got == ino`) and rename/unlink logged after the apply with a `yield_now`: green in 3 of 3 and 4 of 4 runs. The harness gives each thread its own names and its own blocks, so a wrong order between threads cannot change the final state, and reverse-order creates are rare in a 250 ms tail. These cases are covered by `reverse_order_creates_replay`, `parallel_namespace_ops_replay` and 19-08's `parallel_overlapping_writes_replay_to_the_same_content`.
- Tests and fixes were committed together (no red commit).

## Verification

- Task 1: `write_log_replay_tests` 30 passed (with `reverse_order_creates_replay`).
- Task 2: `write_log_replay_tests` 33 passed, then 34 with `a_utimens_during_the_close_io_is_kept`.
- Task 3: `stress_harness crash_replay_matches_apply -- --nocapture` passed (for example, many-files records=314, one-file records=279). `multi_handle_posix` 13 passed. `write_log_replay_tests` 34 passed.
- `stress_harness` binary: 6 passed, 5 ignored. Cargo reports 7.09 s and 7.26 s on warm runs (6.23 s before this plan), inside the budget of about 10 s.
- Before each task commit: `cargo test --workspace` exit 0, 66 `test result: ok` lines, no `FAILED`, no `lock rank violation`.
- After the plan: the full suite command of 19-VALIDATION.md `FULL_EXIT=0` (66 ok, no FAILED, no rank violation; `crash_replay_matches_apply` and `serialize_off_target` ran). `cargo test -p slicefs-cli --features lock-stats --test lock_stats_report` `LSR_EXIT=0` (1 passed).
- Acceptance greps:
  - Task 1: `advance_next_ino` in oplog.rs 0, `got == *ino` 0, `fn reverse_order_creates_replay` 1.
  - Task 2: `l.log(fs,` 0, `fn log(&self, fs` 0, `log_write_out` in handlers.rs 15.
  - Task 3: `pub fn reload_and_replay` in common 2, `fn reload_and_replay` in write_log_replay_tests 0, `mod common` 1, `fn crash_replay_matches_apply` 1.
- Soak after the review: at `6d7d2d9`, `parallel_namespace_ops_replay` was green in 39 of 40 runs (deviation 3) and `crash_replay_matches_apply` in 15 of 15. After the fix (`f1b752c`): 60 of 60 and 15 of 15.
- After the fix: `dir_link_tests` 31 passed, `write_log_replay_tests` 34 passed, the full suite command `FULL_EXIT=0` (66 ok, no FAILED, no rank violation), `lock_stats_report` `LSR_EXIT=0`.
- `grep -rn 'log_record(' crates/slicefs-cli/src` shows only the definition and the commit relog (under the exclusive gate). Every other record is appended with `log_append` in an apply hold.
- Requirements: LOCK-04 is not marked complete, because 19-11 still lists it.

## Decisions Made

See `key-decisions`. All of them can be changed later.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Bug] A utimens during the I/O phase of a close was lost in the live run**
- **Found during:** Task 2 (`parallel_namespace_ops_replay` with shared names)
- **Issue:** `flush_cell` reads `modified` before its unlocked I/O, and Phase C stamped that old value. A setattr of the mtime in the I/O phase sets `modified` and the inode's mtime. The release's flush then stamped the older write time over it. Replay applies the setattr, then the `Release`, so replay kept the explicit mtime and the live mount did not. This is in the write path (Plans 06-08), inside this plan's truth "replay gives the same mtime".
- **Fix:** Phase C stamps the current `modified` when the generation did not change, else the snapshot's.
- **Test:** `a_utimens_during_the_close_io_is_kept`. A push hook parks the release in its I/O, a setattr sets the mtime, then the release goes on. It is red without the fix.
- **Files modified:** `crates/slicefs-cli/src/filesystem.rs`
- **Commit:** `bd1ca60`

**2. [Rule 1 - Bug] Replay stopped on a Setattr of a reclaimed orphan** (19-09 known item)
- **Fix:** the skip arm for a gone inode in `replay_stream` now covers `Setattr` as well as the xattr records.
- **Test:** `a_setattr_of_an_open_orphan_replays` (red before the fix).
- **Commit:** `bd1ca60`

**3. [Rule 1 - Bug] A request could reach a new file by its name before its `Create` record**
- **Found during:** the soak after the plan (`parallel_namespace_ops_replay`, 1 of 40 runs red: "write log record 12480 (link) did not replay: Link { ino: 3357, .. }")
- **Issue:** create, mknod and symlink held only the parent's stripe. Thread A linked the new name and appended its record later in the same hold. Thread B found the name and linked the inode under the inode's stripe, so B's `Link` record came first. Replay then had no such inode yet. The same window let an open, write, setattr or xattr change come before the create. With `O_TRUNC`, the truncate ran after the hold, so a write in between was truncated in the live run but kept in replay.
- **Fix:** `new_entry` takes the inode number before the stripes (nothing can reach an inode with no name), locks `[parent, ino]` in ascending order, checks the parent and links. The create record is appended before that guard drops, and create does its `O_TRUNC` inside the hold. mkdir is not covered (see Plan facts).
- **Test:** `parallel_namespace_ops_replay` (soak counts under Verification).
- **Files modified:** `crates/slicefs-cli/src/filesystem.rs`
- **Commit:** `f1b752c`

### Plan facts

- **Setattr with a size is two records** (key-decisions). The must-have says setattr is appended "under the stripes (and cell) of its apply". The size record is appended under the cell only, because the size change itself runs under the cell only. This is the 19-09 size exception: rule 1, no stripe across a splice.
- **`parallel_namespace_ops_replay` uses shared names**, not "own name prefixes" as the plan behavior says. With own names, the test did not detect a broken log order, even with a mutation (see TDD evidence). The final checks are the plan's: a crash, `reload_and_replay`, and a dump with ino, nlinks, mtime and ctime equal to the live dump. The two directories are subdirectories, so their times are in the dump.
- **Crash test run length:** 1 s per mode, where the plan says about 1.5 s (many-files) and 1 s (one-file). `Config.secs` counts whole seconds, and the binary must stay under about 10 s. It takes 7.1-7.3 s now.
- **`Config.crash`:** the commit thread makes a final commit after the workers stop. Without this flag the log after the last root would be empty, and the test would pass with nothing to check. The test also asserts that the log is not empty.
- **"Release every open handle as a client would":** the harness workers already release every handle at the end of a run (and before an unlink), so the test adds no release step. No orphans are left.
- **`multi_handle_posix` `reload`** is now `common::reload_and_replay`. It sets a write log on the reloaded store, which the old copy did not. No test there depends on that.
- **`advance_next_ino`** is still a public method of `DictMetadataStore`. Nothing calls it now. It was left in place (smallest change).
- **mkdir and a request that finds the new directory before its `Mkdir` record** (recorded in `deferred-items.md`): deviation 3 closes this window for create, mknod and symlink, but not for mkdir, because the store takes the number and adds the entry in one call. The window is older than this plan. No test runs mkdir concurrently with requests in the new directory.
- **Closed-file size change against a fields setattr** (recorded in `deferred-items.md`): if the fields call does not see the transient cell, the two share no lock, and their records can be in the opposite order of their inode writes. This has the same root as the 19-09 deferred item.

**Total deviations:** 3 auto-fixes (Rule 1) and 9 notes (2 of them deferred). **Impact:** all plan truths hold. The setattr truth holds with the size exception from 19-09.

## For later plans

- Plan 11: `crash_replay_matches_apply` is the criterion 5 test for the #21 body. LOCK-04 can be marked complete when Plan 11 closes it.
- The harness cannot detect cross-thread order errors in the final state (own names and own blocks). Order tests are `parallel_namespace_ops_replay` (shared names) and `parallel_overlapping_writes_replay_to_the_same_content`.
- New namespace code: add a `*_logged` function that calls `log(op)` before its guard drops, and a handler in the shape of `tech-stack.patterns`.
- `log_record` (append plus write-out in one call) is still used by the commit relog, under the exclusive gate.

## Issues Encountered

- A full `cargo test --workspace` takes about 9 minutes here, mostly the build. Each run went to the background, and the exit code was read from its log in the scratchpad.

## Known Stubs

None.

## Threat Flags

None. There is no new endpoint, file kind or trust boundary.
- T-19-SC: no new crate.
- T-19-36: replay creates at the logged inode number; `reverse_order_creates_replay`.
- T-19-37: records are appended inside the stripe (and cell) hold of their apply; `parallel_namespace_ops_replay`, `crash_replay_matches_apply`.
- T-19-38: `create_inode_at` returns `AlreadyExists`, and replay stops with "write log record N (create) did not replay". Nothing is overwritten.

## Self-Check: PASSED

- `crates/slicefs-cli/tests/common/mod.rs` and this SUMMARY: FOUND
- Commits `5fe95b2`, `bd1ca60`, `f960da3`, `6d7d2d9` and `f1b752c`: FOUND on HEAD (`git rev-list --count b517f10..f1b752c` prints 5)
