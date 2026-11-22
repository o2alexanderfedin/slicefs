---
phase: 19-fine-grained-locks
plan: 11
subsystem: concurrency
status: complete
tags: [lock-01, lock-04, lock-05, d-09, d-11, stress-harness, lock-stats, posix-2.9.7]

requires:
  - phase: 19-fine-grained-locks
    provides: "19-06/07 copy-I/O-publish with the rebase and push hooks; 19-08 O_APPEND from the size under the cell lock; 19-09 serialize_off_target in the suite; 19-10 crash_replay_matches_apply and tests/common"
provides:
  - "Harness Mode::MultiHandle: O_RDONLY, O_RDWR and O_RDWR|O_APPEND handles per thread on one inode; verify_appends; op=append row; STRESS_MODE=multi-handle"
  - "crates/slicefs-cli/tests/lock_wait_rows.rs (feature lock-stats): readers_do_not_wait_for_writer_io_rows, other_files_do_not_wait_rows"
  - "## Phase 19 result in docs/research/multithreaded-request-handling.md"
  - "A new body for issue #21 (staged for the orchestrator, not applied)"
  - "deferred-items.md: the store-lock and WAL-writer findings"
affects: [20, 21]

estimate:
  tokens: 90000
  tasks: 3
actuals:
  tokens: 9490
  tasks: 3
  commits: 3

plan_head_before: 160b418a226b8eb67d2a60f6f83b81eb8b245290
plan_head_after: 1a6eddf4071f9821956553d855ef88508c4e8388
commits: 3

tech-stack:
  added: []
  patterns:
    - "Lock-row evidence: park the writer's I/O in a hook until the other requests are done (at least 300 ms), then check wait_max < 100 ms and hold_max < 300 ms per site"

key-files:
  created:
    - crates/slicefs-cli/tests/lock_wait_rows.rs
  modified:
    - crates/slicefs-cli/tests/stress_harness.rs
    - docs/research/multithreaded-request-handling.md
    - .planning/phases/19-fine-grained-locks/deferred-items.md

key-decisions:
  - "LOCK-01 is not marked complete: the store lock is still held across disk I/O (the second phase of ChunkStore::sync fsyncs under it, and a record pwrite under it stalls behind a concurrent pack fsync on macOS). A fix changes the store sync protocol and is an owner decision."
  - "LOCK-04 is not marked complete: its last sentence (the #21 body is updated) is true only after the orchestrator applies the staged body. Two older windows (mkdir, closed-file size change against a fields setattr) are open for the owner to judge."
  - "LOCK-05 is marked complete: lock_free_io, lock_wait_rows, multi_handle_posix and the harness multi-handle mode show every clause."
  - "The multi-handle run in serialize_off_target is 1 s (the other modes 2 s), so the stress_harness binary stays under 10 s."

patterns-established:
  - "A harness mode that checks a file region without a whole-file buffer: read in 64 KiB pieces with a bit set of the records seen"

requirements-completed: [LOCK-05]

duration: 52min
completed: 2026-11-22
---

# Phase 19 Plan 11: Multi-handle harness mode, lock wait rows, #21 body and the Phase 19 result Summary

**The harness runs read-only, read-write and `O_APPEND` handles on one inode with serialize off. The lock rows show that readers and other files do not wait for a parked writer I/O. The rows also show that the store lock still has 21-91 ms holds from an fsync and a stalled pwrite, so LOCK-01 is still open.**

## Performance

- **Duration:** 52 min
- **Started:** 2026-11-22T10:39:01Z
- **Completed:** 2026-11-22T11:31:28Z
- **Tasks:** 3
- **Files:** 4 (1 created, 3 modified), plus the staged #21 body outside the repository

## Accomplishments

- **Harness multi-handle mode (D-11, criterion 6):**
  - Each thread opens `O_RDONLY`, `O_RDWR` and `O_RDWR|O_APPEND` handles on the one file.
  - Ops: a one-block write of an owned block through the read-write handle, then a read-back (visibility). Reads of random ranges, where each block must be exactly one allowed version (all or none). An append of a 512-byte record `(t, k)` through the append handle, with a wrong offset on purpose (0 or 999_999+k). An fsync through the read-write handle.
  - Appends stop at 256 per thread, so the file and the dirty total stay small.
  - `verify_appends` reads the region after the blocks in 64 KiB pieces with a bit set. Every record must be there once and whole, and the size must be the blocks plus the records. `verify_files` then checks that no append changed the block region.
  - `model_detects_planted_bug` appends record (0, 0) a second time behind the model's back, and `verify_appends` must report it.
  - The report has an `op=append` row.
- **Lock wait rows (criteria 1, 2):** see the tests below and the table.
- **Hold rows (criterion 3):** the table below is from a serialize-off harness run with `lock-stats`. The store lock fails this criterion (see Findings).
- **#21 body (D-09, criterion 5):** a new body is staged for the orchestrator. It has the log-order rule (append under the locks of the apply, write-out and sync after), the writer and reader steps, the rebase protocol, `O_APPEND`, replay by logged inode, and the rank order with stripes. Two rows of the design table are corrected (the file cell is one mutex; the log append is in the hold of the apply).
- **Research doc:** `## Phase 19 result`. It lists what was done, the decisions that differ from the design text, the tests for each criterion, the store-lock finding, and the work left for Phases 20 and 21.

## Task Commits

1. **Task 1: Harness multi-handle mode** (tracer): `e6d3c25`
2. **Task 2: Lock wait and hold rows** (TDD): `0b4b88a`
3. **Task 3: Phase 19 result in the research doc, deferred items; phase gate**: `1a6eddf`

## TDD evidence (Task 2)

- First run: `readers_do_not_wait_for_writer_io_rows` was **red**, "no samples for lock site readers". A read of an open overlay range does not use the `readers` map. Now every second read reads stored bytes (at 768 KiB), and that path has 52 samples.
- With the real code, both tests are green. This is expected, because Plans 06 and 07 already shipped the behavior.
- Mutation run (local only, not committed): `rebase_unlocked` calls `step_io` with the cell locked. Both tests were **red**:
  - `other_files_do_not_wait_rows`: "lock site file_cell was held 429916 us, across the parked writer I/O".
  - `readers_do_not_wait_for_writer_io_rows`: "the reads did not end while the writer's I/O was parked".
- The test and the fix of the test were committed together (no red commit).

## Lock rows

From `cargo test -p slicefs-cli --features lock-stats --test stress_harness serialize_off_target -- --nocapture`. Conditions: macOS, debug build, 4 threads, serialize off, PerOp log, commit every 250 ms. One-file and many-files ran 2 s, multi-handle 1 s. Times are in microseconds.

| Mode | Site | n | wait p50 / p99 / max | hold p50 / p99 / max |
|---|---|---|---|---|
| one-file | store | 3588 | 0.0 / 0.0 / 0.0 | 1.8 / 30.4 / 23691.3 |
| one-file | readers | 1202 | 0.0 / 0.0 / 0.0 | 0.8 / 2.7 / 21.6 |
| one-file | wal_queue | 1894 | 0.0 / 0.0 / 0.0 | 0.7 / 11.6 / 32.3 |
| one-file | wal_writer | 1045 | 0.0 / 36339.7 / 72024.1 | 79.1 / 23298.0 / 43352.1 |
| one-file | inode_live | 251 | 0.0 / 0.0 / 0.0 | 0.6 / 8.1 / 29.5 |
| one-file | dir_live | 227 | 0.0 / 0.0 / 0.0 | 0.6 / 7.3 / 18.1 |
| one-file | file_cell | 2261 | 0.0 / 1599.5 / 5197.8 | 9.5 / 791.0 / 3682.3 |
| one-file | stripe | 12 | 0.0 / 41.8 / 41.8 | 11.1 / 304.9 / 304.9 |
| many-files | store | 9976 | 0.0 / 372.7 / 42106.9 | 2.1 / 158.1 / 57868.3 |
| many-files | readers | 1001 | 0.0 / 0.0 / 22.0 | 1.5 / 11.0 / 76.7 |
| many-files | wal_queue | 2993 | 0.0 / 0.0 / 0.8 | 0.5 / 2.2 / 7.8 |
| many-files | wal_writer | 1593 | 0.0 / 20480.0 / 41582.6 | 16.9 / 20856.8 / 41484.3 |
| many-files | inode_live | 3843 | 0.0 / 0.0 / 0.6 | 1.6 / 6.9 / 22.0 |
| many-files | dir_live | 2076 | 0.0 / 0.0 / 11.7 | 2.8 / 14.1 / 39.6 |
| many-files | file_cell | 3663 | 0.0 / 0.0 / 0.0 | 3.2 / 203.5 / 37060.6 |
| many-files | stripe | 1526 | 0.0 / 0.9 / 51.1 | 24.1 / 140.7 / 36798.5 |
| multi-handle | store | 3031 | 0.0 / 0.0 / 86.5 | 1.5 / 22.2 / 21479.4 |
| multi-handle | readers | 1670 | 0.0 / 0.0 / 1.3 | 0.7 / 2.0 / 3.6 |
| multi-handle | wal_queue | 1743 | 0.0 / 0.0 / 1.3 | 0.5 / 3.2 / 14.6 |
| multi-handle | wal_writer | 929 | 0.0 / 22855.7 / 36896.8 | 23.3 / 20987.9 / 36634.6 |
| multi-handle | inode_live | 164 | 0.0 / 0.9 / 3.5 | 0.6 / 8.0 / 14.9 |
| multi-handle | dir_live | 131 | 0.0 / 0.0 / 0.0 | 0.6 / 7.1 / 10.9 |
| multi-handle | file_cell | 1642 | 0.0 / 78.4 / 20578.3 | 4.4 / 73.5 / 20578.3 |
| multi-handle | stripe | 28 | 0.0 / 20480.0 / 20480.0 | 5.0 / 20611.1 / 20611.1 |

Across four more runs (three with a probe), the store hold max was 21-91 ms. The many-files store wait max was 23-86 ms.

`lock_wait_rows` (a writer's rebase I/O parked for at least 300 ms):
- Readers test: `file_cell` wait max 1.0 us, hold max 349.7 us. `readers` n=52, wait max 0.0 us. Slowest read 257 us. No site held for 1 ms or more.
- Other-file test: every site waits 0.0 us. The `file_cell` hold max is 306 us and the `wal_writer` hold max is 320 us, with 20 fsyncs of Y on one thread.

## Findings (why LOCK-01 is not marked)

A probe captured backtraces for long holds. It ran locally, one run per probe, and was not committed. It named these paths:

1. **`ChunkStore::sync`, second phase.** `sync_active` and `sync_dir` run with the store lock held. The commit calls `sync` before the gate (`timed_commit`) and as `pre_root_sync`. The hold under the lock was 20-42 ms each time. CONTEXT says an fsync must leave the lock. 19-02 kept this rest under the lock (the #108 pattern).
2. **`ChunkStore::put`, the record `append`.** This is a page-cache pwrite that D-14 keeps under the lock. It stalled 36 ms, from a release splice and from a read that puts a stream tail. The most likely cause is a concurrent fsync of the same pack file (macOS).
3. **`wal_writer`.** `PerOpWal::write_out` holds the writer lock for the record pwrite. With 4 threads its hold p99 is about 20 ms. With one thread (in `lock_wait_rows`) its hold max is 0.3 ms. The most likely cause is the same: another thread's fsync of the same segment. `wal_writer` is not the "WAL append mutex" of criterion 3. That mutex is `wal_queue`, and its hold max is 32 us.
4. **`file_cell` and `stripe` holds of about 20-37 ms.** In multi-handle mode, the store hold max is 21.5 ms, and the cell and stripe hold max is 20.6 ms. So the holder most likely took the store lock (rank 6) inside the cell or stripe hold, and the pwrite stalled. These holds are part of the store-lock item, not a writer's own I/O in a cell hold.

`readers`, `wal_queue`, `dir_live` and `inode_live` meet criterion 3 (tens of microseconds at most). The store lock does not. A fix changes the sync protocol of the chunk store and of the log, outside this plan's files: Rule 4. It is recorded in `deferred-items.md`, in the research doc and as a STATE blocker.

## Requirements

- **LOCK-05: marked complete.**
  - Readers do not wait for writer I/O: `reader_does_not_wait_for_rebase_io`, `reader_does_not_wait_for_streaming_push` and `readers_do_not_wait_for_writer_io_rows`.
  - No contention between files: `many_files_no_cross_file_wait` and `other_files_do_not_wait_rows`.
  - Several handles in different modes: `visibility_across_ro_rw_append_handles` and the harness multi-handle mode.
  - All or none: `all_or_none_reads` (multi-unit writes) and the per-block check of the harness.
  - Appends never overlap and come from the size under the cell lock: `appends_never_overlap`, `append_starts_at_the_committed_size_and_stops_at_the_max` and the harness `verify_appends`.
  - Caveat: a store-lock stall nested in a cell hold (Finding 4) can still make a reader of that file wait about 20 ms. That is the LOCK-01 item.
- **LOCK-01: not marked.** See Findings.
- **LOCK-04: not marked.**
  - The code part has its evidence: records are appended in the apply hold, replay is by logged inode, `crash_replay_matches_apply`, `reverse_order_creates_replay` and `parallel_namespace_ops_replay`.
  - The last sentence of LOCK-04, "The #21 body is updated to match", is true only after the orchestrator applies the staged body. LOCK-04 can be marked then.
  - For the owner: two windows from `deferred-items.md` let a record land out of apply order in a race. (1) mkdir: the new directory can be found by name before its `Mkdir` record is appended. (2) A size change of a closed file against a setattr of other fields of the same inode.

## Verification

- Task 1: `serialize_off_target -- --nocapture` passed. It printed report headers for one-file, many-files and multi-handle, `op=append n=129..400` in multi-handle mode, and no "mismatches". After the commit, the tracer gate ran it again and it passed (`op=append n=191`).
- `stress_harness` binary: 6 passed, 5 ignored, in 9.04 s, 9.41 s and 9.58 s (warm). With a 2 s multi-handle run it was 9.81 s and 10.01 s (see Deviations). Before this plan it was 7.1-7.3 s.
- Task 2: `lock_wait_rows` 2 passed (1.1 s). The lock-stats harness `serialize_off_target` had `lock site=inode_live` and `lock site=wal_queue` rows and exited 0. `lock_stats_report` exited 0 with no "no samples".
- Before each task commit, `cargo test --workspace` exited 0: 66 ok after Task 1, 67 ok after Task 2 (the new binary is empty without the feature). No FAILED and no rank violation.
- Phase gate, before the Task 3 commit (each step exited 0):
  - `cargo test --workspace`
  - `cargo check --workspace --all-targets`
  - `cargo check --workspace --all-targets --features slicefs-cli/lock-stats`
  - `cargo check -p slicefs-cli --features deadlock-detect`
  - `cargo check -p slicefs-cli --all-targets --features deadlock-detect`
  - `cargo test -p slicefs-cli --features deadlock-detect --test stress_harness serialize_off_target`
  - `cargo test -p slicefs-cli --features lock-stats --test lock_stats_report`
  - `cargo test -p slicefs-cli --features lock-stats --test lock_wait_rows`
  - The log has 70 `test result: ok` lines and 0 lines with FAILED, "STRESS deadlock detected" or "lock rank violation".
- Acceptance greps:
  - `MultiHandle` in stress_harness.rs: 10. `O_APPEND`: 4.
  - `fn readers_do_not_wait_for_writer_io_rows`: 1. `fn other_files_do_not_wait_rows`: 1.
  - `## Phase 19 result`: 1. Home paths in the doc: 0. Licensing words in the doc: 0.
- #21 body checks, run on the staged file (not on the live issue): `same locks` 2, `the slow part, no inode lock` 0, licensing words or home paths 0. The same checks against `gh issue view 21` must run after the orchestrator applies the body.

## Decisions Made

See `key-decisions`.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 2 - Missing check] The append check can fail**
- **Found during:** Task 1
- **Issue:** the plan gives no planted bug for the new check, so a check that never fails would go unnoticed.
- **Fix:** `model_detects_planted_bug` appends record (0, 0) a second time, and `verify_appends` must name it. The block region must stay clean.
- **Commit:** `e6d3c25`

**2. [Rule 1 - Test bug] The readers row had no samples**
- **Found during:** Task 2 (first run)
- **Fix:** every second read is of stored bytes, which go through `readers`. The test asserts `count > 0` for `file_cell` and `readers`.
- **Commit:** `0b4b88a`

### Plan facts

- **Multi-handle run length:** 1 s in `serialize_off_target`, where the plan says 2 s. With 2 s the binary took 9.81 s and 10.01 s. The acceptance criterion is at most 10 s.
- **Writes in multi-handle mode are one block,** as the behavior says. So the harness all-or-none check is per block. `all_or_none_reads` in `multi_handle_posix.rs` covers writes of several units.
- **Reads go by inode:** `handle_read` takes the inode, not the handle. The read-only handle is open during the whole run, and reads use its inode.
- **The hook parks, it does not just sleep:** the rebase hook parks its first call until the readers (or the requests on Y) are done, and for at least 300 ms. It goes on by itself after 5 s at most. So every measured request ran while the I/O was held, and the test asserts it. Later calls of the hook (finalize, with the cell locked) do not park.
- **The Y round time is printed, not asserted:** a round includes a disk sync (about 25 ms), and a sync is not a lock wait. The plan's assertion is on the wait rows.
- **Hold check:** both tests also assert that no site was held for 300 ms or more.
- **The store has a write log** (`common::new_fs_with`, PerOp), so the "every site" claim also covers the WAL sites.
- **Issue #21 was not edited here.** As the orchestrator told me, the new body is in a scratch file for the orchestrator to review and apply. The plan's live-issue checks are pending (see Verification).
- **Stale text:** the module header of the harness (it said two modes and serialize on until Phase 20) and the S7 comment about `open_files` are fixed. The S3 `SITES` filter already had `file_cell`. 19-05 assigned the `DIRTY_BUDGET` doc and the `dirty_budget_tests.rs` header to Phase 20, so they were left alone.
- **Requirements:** only LOCK-05 is marked (see Requirements).

**Total deviations:** 2 auto-fixes (1 Rule 2, 1 Rule 1) and 11 plan facts. One finding is held for a Rule 4 owner decision (the store lock). **Impact:** all plan tasks are done and the gate is green. Criterion 3 is not met for the store lock. LOCK-01 and LOCK-04 stay open.

## For later plans

- Phase 20 or a follow-up (owner decision): take the fsync of `ChunkStore::sync` out of the store lock. Also stop the record pwrite (store and WAL writer) from stalling behind a concurrent fsync of the same file on macOS. Then run `lock_wait_rows` and the lock-stats harness again. The store hold max should be in microseconds, as it is for `readers` and `wal_queue`.
- `lock_wait_rows.rs` is the pattern for evidence that a lock is not held across I/O. A flusher test in Phase 20 can use it.
- The harness modes are `one-file`, `many-files` and `multi-handle`. `long_run` takes `STRESS_MODE=multi-handle`.

## Issues Encountered

- A full `cargo test --workspace` takes about 9 minutes here, mostly the build. Each run went to the background, and the exit code was read from its log.

## Known Stubs

None.

## Threat Flags

None. No new endpoint, file kind or trust boundary.
- T-19-SC: no new crate.
- T-19-39: the body and the doc have no home paths and no licensing words (checked by grep). This SUMMARY names functions only, not paths from the backtraces.
- T-19-40: the harness watchdog is unchanged. The `lock_wait_rows` hook goes on by itself after 5 s, and every channel wait has a limit.
- T-19-41: a 300 ms park against a 100 ms bound. Measured waits are 0-14 us.

## Self-Check: PASSED

- `crates/slicefs-cli/tests/lock_wait_rows.rs`, `docs/research/multithreaded-request-handling.md` and this SUMMARY: FOUND
- Commits `e6d3c25`, `0b4b88a` and `1a6eddf`: FOUND on HEAD (`git rev-list --count 160b418..1a6eddf` prints 3)
