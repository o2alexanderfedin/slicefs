---
phase: 19-fine-grained-locks
plan: 04
subsystem: metadata
tags: [lock-01, lock-04, d-06, d-08, wal, lsn, append, write-out, fsync, lock-order]

requires:
  - phase: 19-fine-grained-locks
    provides: "19-01 lock shim (Rank::Store, timed sites); 19-03 metadata hot path (wal_handle clone pattern)"
provides:
  - "wal::Lsn; WalStrategy::append (memory copy and LSN under one short lock) and write_out (idempotent drain in LSN order); default log_mutation = append then write_out"
  - "PerOpWal: in-memory queue timed as Site::WalQueue (\"wal_queue\"); sticky write-out failure until a cut or rotate"
  - "BufferedLog (FlushOnFsync, Periodic): append holds in RAM; write_out writes only past BUFFER_CAP_BYTES"
  - "One WAL lock order: writer, then queue or pending, then loc, then behind"
  - "cut, install_root and shutdown write the queue out first (PerOp and buffered); PerOp rotate writes it out too; buffered rotate drops the held records once the new segment exists (as before)"
  - "SegmentWriter::begin_sync / end_sync (SyncMark) and wal::sync_current: no WAL lock across a disk sync in flush_and_sync"
  - "Behind::settle syncs the old segments with the behind lock released"
  - "DictMetadataStore::log_append(bytes) -> Option<Lsn> and log_write_out(Option<Lsn>); log_op is both"
affects: [19-08, 19-10, 19-11]

estimate:
  tokens: 110000
  tasks: 3
actuals:
  tokens: 13800
  tasks: 3
  commits: 3

plan_head_before: 1f4f9a586740f14fe41b2858785431886f65c82e
plan_head_after: 9d0d540ed96f3f19b25b86d3a03beaa005eca1fc
commits: 3

tech-stack:
  added: []
  patterns:
    - "WAL ticket: append = copy outside the lock, then LSN and push under the queue lock; write_out takes the writer, takes the batch out of the queue, releases the queue, writes"
    - "Watermark plus sticky failure: `done` (highest LSN taken out) and `failed` change only under the writer; the fast path reads both with no lock"
    - "Sync on a second file handle: begin_sync under the lock, sync_all with no lock, end_sync with a mark (segment id, write count)"
    - "Parker test hook: a named point parks a thread until the test releases it (5 s limit), fires once per arm"

key-files:
  created: []
  modified:
    - crates/metadata/src/wal/mod.rs
    - crates/metadata/src/wal/per_op.rs
    - crates/metadata/src/wal/buffered.rs
    - crates/metadata/src/wal/flush_on_fsync.rs
    - crates/metadata/src/wal/periodic.rs
    - crates/metadata/src/wal/no_wal.rs
    - crates/metadata/src/segment/writer.rs
    - crates/metadata/src/store.rs
    - crates/chunk-store/src/lock_stats.rs
    - crates/slicefs-cli/tests/lock_stats_report.rs
    - crates/slicefs-cli/tests/wal_durability_tests.rs

key-decisions:
  - "append and write_out are required trait methods. Two defaults that call each other would recurse for a strategy that implements neither"
  - "PerOp write_out drains the whole queue, not only up to `upto`: the writer is held anyway, and this keeps the queue short"
  - "A PerOp write-out failure is sticky until a cut or rotate (as in the buffered strategies): write_out and flush_and_sync fail meanwhile, so a record that another thread's failed write-out dropped is still reported (T-19-14)"
  - "The buffered write_out takes the records out of `pending` before the write, so appends go on during a 4 MiB write-out. While one batch is written, `pending` can fill again, so the peak RAM is about two caps plus the records of the requests in flight (T-19-11)"
  - "end_sync takes a SyncMark (segment id, write count) instead of the plan's argument-free form: overlapping syncs, or a cut between begin and end, must not clear `unsynced` of records that the sync did not cover"
  - "Behind::settle syncs second handles of the old segments and removes a writer only after its sync succeeded (by segment id); `dir` is cleared only if no cut came meanwhile (`cuts` counter, kept by `reset`)"
  - "Rotate still holds `writer` across the fsync of the new segment (both strategies; buffered no longer holds `pending` across it). For PerOp and the buffered strategies, rotate runs only from `commit_root` (the offline `snapshot switch` command): their `cut` always returns a cut, so the commit path of a mount uses cut and install_root"

patterns-established:
  - "A request calls log_append under its apply locks and log_write_out after it released them, before the reply, with the shared commit gate held across both (a cut drains the queue under the exclusive gate)"

requirements-completed: []

duration: 45min
completed: 2026-11-21
status: complete
---

# Phase 19 Plan 04: WAL append and write-out split Summary

**The write log now takes a record in two steps: `append` (a memory copy plus an LSN under one short queue lock) and `write_out` (drain to the segment in LSN order). Every WAL fsync runs on a second file handle with no WAL lock held, and one lock order (writer, queue or pending, loc, behind) holds in every path.**

## Performance

- **Duration:** about 45 min
- **Completed:** 2026-11-21
- **Tasks:** 3
- **Files modified:** 11

## Per-row fixes (RESEARCH Lock Inventory A, rows 9-11)

| Row | Site | Held before | Fix | Task | Test |
|-----|------|-------------|-----|------|------|
| 9 | `store.rs` `log_wal_entry` | `wal` slot (Site::Wal) across `log_mutation` (record copy and `write()`) | `log_append`, `log_write_out`, `log_wal_entry` and `shutdown_wal` clone the strategy `Arc` with `wal_handle()` and call it with no slot lock. Report: `wal` hold max 1.8 us | 1 | `store_log_op_writes_one_record_and_a_failed_write_out_fails_fsync` |
| 10 | `wal/per_op.rs` `log_mutation`, `flush_and_sync` | `writer` across write, flush and `sync()`; `behind` across `Behind::settle` (fsyncs) | `append` takes only the queue. `write_out` writes and flushes to the kernel under `writer`, with the queue released. `flush_and_sync` uses `sync_current` (sync_all with no lock). `settle` syncs with `behind` released | 1, 3 | `append_returns_while_the_writer_is_held`, `append_and_write_out_pass_a_parked_fsync`, `settle_syncs_the_old_segments_with_the_behind_lock_released` |
| 11 | `wal/buffered.rs` `log`, `flush_and_sync` | `pending` and `writer` across the cap write-out and the fsync | `append` takes only `pending`. `write_out` takes the records out of `pending` and writes them under `writer` alone. `flush_and_sync` uses `sync_current`. The order is now writer then pending everywhere, also for the periodic timer | 2, 3 | `append_holds_records_until_a_write_out_past_the_cap`, `no_deadlock_between_append_cut_and_sync` |

## Task Commits

1. **Task 1: Lsn, append and write_out through PerOpWal and the store** - `a6b406d` (feat)
2. **Task 2: buffered strategies on the same API, queue drain, one lock order** - `04ee34e` (feat)
3. **Task 3: WAL fsync outside the writer and behind locks, wal_queue label** - `9d0d540` (feat)

**Plan metadata:** added by the final docs commit.

## TDD record (red before green, no red commit)

- Task 1 is a tracer task, not TDD. Its tests passed at the first build. The tracer gate ran again after the commit (`cargo test -p metadata --lib wal::`: 11 passed).
- Task 2, `append_holds_records_until_a_write_out_past_the_cap`: red on the Task 1 code (the bridge `append` still wrote out at the cap): "append must only hold the records (memory only), held 131106". The other Task 2 tests (`cut_keeps_unwritten_appends_in_lsn_order`, `rotate_after_unwritten_appends_starts_with_the_seed_only`, `no_deadlock_between_append_cut_and_sync`) passed before and after. They guard order and freedom from deadlock, which the old code also had.
- Task 3, `append_and_write_out_pass_a_parked_fsync`: red with the park point placed in the old code (writer held across `sync()`): "append and write_out must finish while an fsync is parked" (5.0 s).
- Task 3, `settle_syncs_the_old_segments_with_the_behind_lock_released`: red with the park point inside the old `behind` hold: "a cut must finish while settle syncs".

## Verification

- `cargo test -p metadata --lib wal::`: 17 passed, among them `append_returns_while_the_writer_is_held`, `no_deadlock_between_append_cut_and_sync` and `append_and_write_out_pass_a_parked_fsync`.
- `cargo test -p metadata --lib`: 157 passed. `cargo test -p metadata --test write_log_tests`: 9 passed.
- Full suite command from 19-VALIDATION.md (`cargo test --workspace && cargo check --workspace --all-targets && cargo check --workspace --all-targets --features slicefs-cli/lock-stats && cargo check -p slicefs-cli --features deadlock-detect`): exit 0, 65 `test result: ok`, no `FAILED`, no `lock rank violation`.
- `cargo test -p slicefs-cli --features lock-stats --test lock_stats_report -- --nocapture`: exit 0. All 10 sites have samples: `wal_queue n=11 hold_max_us=19.6`, `wal n=21 hold_max_us=1.8`, `wal_pending n=11`, `wal_writer n=12 hold_max_us=2449.4`. I did not split the `wal_writer` maximum by path; it can be a cut or rotate of the commit, or the write-out of a large record.
- `cargo test --workspace` exited 0 before each of the three commits.
- Acceptance greps: `fn write_out` in wal/mod.rs 1; `struct Lsn` 1; `fn log_append` 1; `WalQueue` in lock_stats.rs 3; `pending, then writer` 0; `fn append` in buffered.rs 1; `fn begin_sync` 1; `"wal_queue"` 1.

## Decisions Made

See `key-decisions`. All of them can be changed later.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 - Blocking] `ProbeWal` test strategy outside the plan file list**
- **Found during:** Task 1
- **Issue:** `crates/slicefs-cli/tests/wal_durability_tests.rs` implements `WalStrategy` with only `log_mutation`. Its failed-append test goes through `log_op`, which now calls `append` and `write_out`.
- **Fix:** `ProbeWal` implements `append` (it fails when `fail_appends` is set) and `write_out`.
- **Commit:** `a6b406d`

**2. [Rule 1 - Bug] Root and snapshot records lost their sync under the default `log_mutation`**
- **Found during:** Task 1 design
- **Issue:** The default `log_mutation` (append, then write_out) only flushes to the kernel. Before, PerOp synced root and snapshot records.
- **Fix:** PerOp overrides `log_mutation` and syncs after non-`Op` records (with `sync_current` from Task 3 on).
- **Commit:** `a6b406d`, `9d0d540`

**3. [Rule 2 - Missing critical] A dropped record must still fail (T-19-14)**
- **Found during:** Task 1 design
- **Issue:** With a shared queue, a write-out of thread A can take thread B's record and fail. B's `write_out` then sees its LSN as done and returns Ok, so B's request would not bump `wal_fail_seq`.
- **Fix:** A sticky `failed` flag in PerOp, set under `writer` before `done` moves. `write_out` and `flush_and_sync` fail until a cut or rotate. A cut folds a drain failure into `old_failed` and does not return early, because the commit is the way back. Before this plan, PerOp had no such state. After a failed write, the segment can end in a torn record, so the records after it could not replay anyway.
- **Commit:** `a6b406d`

**4. [Rule 1 - Bug] `end_sync` with no argument can clear `unsynced` wrongly**
- **Found during:** Task 3 design
- **Issue:** The plan's `begin_sync() -> Option<File>` and `end_sync()` forms keep one write counter in the writer. Two overlapping syncs, or a cut or rotate between begin and end (the periodic timer and the Task 2 test do this), would let an earlier sync clear `unsynced` for records it did not cover.
- **Fix:** `begin_sync() -> io::Result<Option<(File, SyncMark)>>` and `end_sync(SyncMark)`. The mark holds the segment id and the write count. `end_sync` does nothing on another segment. It always counts the sync, and it clears `unsynced` only if nothing was written since. `begin_sync` calls the test fault hook as `sync` does.
- **Commit:** `9d0d540`

**5. [Rule 1 - Bug] Taking the old writers out of `behind` breaks fsync for a second caller**
- **Found during:** Task 3 design
- **Issue:** If `settle` moved the old writers out and synced them, a second `flush_and_sync` meanwhile would see `behind` empty and return Ok before the segments before the cut were durable. A failed sync would also lose them.
- **Fix:** `Behind::settle` takes second handles (`begin_sync`) under the lock, syncs them with the lock released, and then removes by segment id only the writers it synced. `dir` is cleared only if no cut came meanwhile (`cuts` counter). `Behind::reset` replaces the `Behind::default()` assignments, so the counter is never reset.
- **Commit:** `9d0d540`

**6. [Plan fact] Task boundary: bridge in Task 1, PerOp drains in Task 1**
- In Task 1, FlushOnFsync and Periodic had to implement the two new trait methods to compile: `append` called the old `log`, and `write_out` did nothing. Task 2 replaced this bridge.
- The PerOp queue drain in `cut`, `rotate`, `install_root` and `shutdown` came in Task 1, because a queue without it could put records in the wrong segment. Task 2 added the tests for it.
- PerOp `shutdown` is now `flush_and_sync`. It also syncs the segments left behind, and it fails after a failed write-out.

**7. [Plan fact] Buffered rotate drops the held records; it does not write them out**
- The plan's must-have says rotate writes out the queue first. The buffered `rotate` keeps today's behavior: it clears `pending` after the new segment exists, because the new root holds those records and the old segment is deleted. Writing them out first would only write into a file that is then removed. `rotate_after_unwritten_appends_starts_with_the_seed_only` checks the property that matters: no record from before the rotate follows the seed.

**Total deviations:** 5 auto-fixes (3 Rule 1, 1 Rule 2, 1 Rule 3) and 2 notes. **Impact:** inventory rows 9-11 leave their locks. Two plan signatures are stronger than the plan text (`SyncMark`, settle by handle).

## For later plans

- Plans 08 and 10: call `log_append` under the apply locks and `log_write_out` after the release, before the reply. Hold the shared commit gate across both. `cut` runs under the exclusive gate and drains the queue, so no request can be between the two steps at a cut.
- `log_append` returns `None` when there is no write log or the append failed (then `wal_fail_seq` was already bumped). `log_write_out(None)` does nothing.
- After a failed PerOp write-out, every `log_write_out` bumps `wal_fail_seq` until the next commit. This is harmless, because `flush_wal` fails until then anyway.
- Still holding a WAL lock across I/O: `rotate` holds `writer` across the fsync of the new segment (it runs only from the offline `snapshot switch`), and `cut` holds `writer` and `pending` across the create of the new segment (no fsync; under the exclusive gate).
- A buffered `rotate` drops the records held in `pending` after the new segment exists, also records appended during its fsync. Today no request runs then (offline command). A plan that calls rotate on a live mount must drop only the records held before `rotate_segment`.
- Key link: the plan names `begin_sync` on the link from `per_op.rs` to `segment/writer.rs`. The call is in `wal::sync_current` (`wal/mod.rs`), which `PerOpWal::flush_and_sync` and `BufferedLog::flush_and_sync` call. Group commit and `durable_lsn` stay deferred.
- `wal::Parker` (tests only) can park a thread at a named point of a WAL path.

## Issues Encountered

- The first red run of the Task 3 tests panicked on `release.send` after the 5 s park timeout and hid the assertion message. The tests now ignore a failed release, so the real assertion shows.

## Known Stubs

None.

## Threat Flags

None. No new endpoint, file kind or trust boundary. T-19-SC: no new crate. T-19-11: each request writes out its own LSN before it replies (`log_op`), and the buffered cap stays. T-19-12: `cut_keeps_unwritten_appends_in_lsn_order` and `rotate_after_unwritten_appends_starts_with_the_seed_only`. T-19-13: `no_deadlock_between_append_cut_and_sync`. T-19-14: the sticky `failed` flag, and `store_log_op_writes_one_record_and_a_failed_write_out_fails_fsync`.

## Self-Check: PASSED

- This SUMMARY and the 11 modified files: FOUND
- Commits `a6b406d`, `04ee34e`, `9d0d540`: FOUND on HEAD (`git rev-list --count 1f4f9a5..HEAD` prints 3)
