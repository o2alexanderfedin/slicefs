---
phase: 19-fine-grained-locks
plan: 12
subsystem: concurrency
status: complete
tags: [lock-01, chunk-store, fsync, retry, gap-closure, d-14]

requires:
  - phase: 19-fine-grained-locks
    provides: "19-02 lock tails (prepare_append, seal in a thread, MemIo hold knobs, lock_tail_tests); 19-04 SyncMark pattern; spike 19g evidence (phase-2 fsync and record pwrite in the store hold)"
provides:
  - "ChunkStore::sync phase 2 and the index flush fsync the active pack and the directory with no store lock (sync handle + marks, flags cleared by mark)"
  - "RetryingPack::sync_handle (RetrySync): shared copy state with sequence numbers, repair in the handle thread, background flight through the handle"
  - "The record pwrite of a put runs with no store lock: one write in flight per active pack, published under the lock"
  - "PackWrite, PackFile::writer, PackFile::note_written, SyncWait::wait_timeout, STORE_WAIT, Hold::PackWrites"
affects: [19-13, 19-14, 19-15, 19-16, 20]

estimate:
  tokens: 150000
  tasks: 3
actuals:
  tokens: 17500
  tasks: 3
  commits: 3

plan_head_before: b473aaca1cf92fc70b973fe394dc1d40d20df1c3
plan_head_after: a7bff74cefc967ad0d48672c0a9b8b67c71e7bd0
commits: 3

tech-stack:
  added: []
  patterns:
    - "Unlocked I/O by mark: under the lock take a handle and a counter mark; do the I/O with no lock; under the lock again clear a flag only if the counter did not move"
    - "Failure generation for retry copies: a sync that saw a failure (or an unrepaired one) proves nothing; a write that saw one writes again"
    - "One write in flight per append-only file: offset given out under the lock, length and index move only at publish, so no hole"

key-files:
  created: []
  modified:
    - crates/chunk-store/src/lib.rs
    - crates/chunk-store/src/retry.rs
    - crates/chunk-store/src/pack_io.rs
    - crates/chunk-store/src/mem.rs
    - crates/chunk-store/src/lock_tail_tests.rs
    - crates/chunk-store/tests/retry_tests.rs

key-decisions:
  - "G1: ChunkStore::sync phase 2 and flush_frozen take the active pack's sync handle and marks under the store lock, fsync the pack and the directory with no lock, and set failed under the lock on an error"
  - "Dirty flags are cleared after the unlocked fsync by mark (Active::published, Inner::created), not before it"
  - "G2 (store half): one record write in flight per active pack; a later put waits for it with no lock. D-14 is superseded (spike 19g: a page-cache pwrite waits about 20 ms behind any F_FULLFSYNC on macOS)"
  - "The RetryingPack sync handle repairs a failure in its own thread and re-writes all held copies; the background flight syncs through it"
  - "A put-loop wait, lock_no_create and the new no-write waits end after STORE_WAIT (10 s) with an I/O error of kind TimedOut; the store is not failed (typed marker StoreWaitTimeout)"

patterns-established:
  - "MemIo Hold::PackWrites parks a write through a writer handle (the record write of a put), not the locked writes"

requirements-completed: []

duration: 70min
completed: 2026-11-22
---

# Phase 19 Plan 12: Store sync, index flush sync and record pwrite with no store lock Summary

**The store lock now covers memory work only on the put, read and sync paths: the phase-2 fsync of `sync`, the fsync of the index flush, the directory syncs and the record pwrite of a put all run with no `ChunkStore::inner` held. The production retry layer has a real sync handle that drops its copies by sequence and repairs a failed sync with no lock.**

## Performance

- **Duration:** about 70 min
- **Started:** 2026-11-22T13:13:53Z
- **Completed:** 2026-11-22T14:24Z
- **Tasks:** 3
- **Files:** 6 modified

## Accomplishments

- **Task 1 (G1, sync):** `ChunkStore::sync` phase 2 waits for a running seal with no lock (`lock_no_seal`, 10 s limit), joins it, and takes a `SyncJob`: the active pack's handle with its `published` mark, and the directory with the `created` mark. It runs the handle sync and `sync_dir` with no lock. Under the lock again it sets `failed` on an error, else clears `dirty` and `dir_dirty` only if their counters did not move. A dirty pack with no handle goes to `Inner::sync_locked_fallback`.
- **Task 1 (retry layer):** the copy state of `RetryingPack` (copies with sequence numbers, kept, the last cut, the failure count, broken) is in a shared `Rank::Store` mutex. `RetrySync` (the handle) takes its mark when its sync starts, drops the copies up to the mark on success, and on an error repairs (re-write every held copy through the inner writer, sync again, with the backoff waits) with no lock held across the I/O. The background flight syncs through this handle, so `settle` only reports.
- **Task 2 (G1, index flush):** `flush_frozen` takes the handle, the marks, the hwm and the counts and freezes the memtable under the lock. It then syncs with no lock, and writes and installs the run only after that sync.
- **Task 2 (limits):** the put-loop waits and `lock_no_create` end after `STORE_WAIT` (10 s).
- **Task 3 (G2, record write):**
  - Under the lock a put sets `Active::writing` at offset `a.len` and leaves the lock. It writes through the pack's writer handle, then publishes under the lock.
  - At publish `a.len`, the entry, the retry note and the index move, and `published` counts the write.
  - Other puts wait for the write in flight with no lock (`Before::Wait`).
  - `gc_begin`, `gc_begin_full`, `close`, the GC copy loop and the GC install wait with no lock for no write in flight. `lock_no_write`, `held_no_write` and the install loop each have the 10 s limit.
  - The locked append is now `append_locked`. GC copies use it, and so does a put on a pack with no writer handle.

## Task Commits

1. **Task 1: sync phase 2 with no store lock, RetryingPack sync handle** (tracer): `0925802`
2. **Task 2: index flush sync with no store lock; put-loop wait limits** (TDD): `5923100`
3. **Task 3: record pwrite of a put with no store lock** (TDD): `a7bff74`

## TDD and red-run evidence

All red runs were local, before the fix. Each test and its fix were committed together, so there is no red commit.

- **Task 1:** `put_and_read_pass_a_held_phase_two_sync` (plain MemIo) on the old code: `B's put and read finished while A synced: Timeout`. The retry-layer variant was green on the old code too, as the plan expects: A parks in its phase-1 background sync, which holds no lock.
- **Task 2:** `put_and_read_pass_a_held_index_flush_sync` on the Task 1 tree: `B's read, dedup put and stats finished while A's flush synced: Timeout`.
- **Task 3:** the three new tests on the Task 2 tree all failed with `A is in its held record write`. The put wrote its record with `a.file.write_at` under the store lock, so it never used the writer handle.
- **Mutation 1 (Task 3):** moving `a.len` at the start of the write instead of at publish made `a_record_in_flight_is_invisible_and_below_no_hwm` red: `hwm (1, 706) is above the write at 360`. Restored.
- **Mutation 2 (retry layer):** without the `fails` check in `write_kept`, `a_write_that_overlaps_a_failed_sync_is_written_again` was red: after the crash the piece read back as zeros. Restored.

## Locked paths that stay (not on the put, read or sync path)

- GC: the `sync_active` and `sync_dir` in the `gc_group` install, and the GC copies (`append_locked`).
- `gc_begin` and `gc_begin_full`: the seal and `join_seal`.
- `close`: the seal and the directory sync.
- Fallbacks: a pack with no sync handle (`sync_locked_fallback`), and a pack with no writer handle (the locked `append_locked` on the put path). Both happen only when `try_clone` fails.
- A background sync of the retry layer that cannot start its thread syncs in place (`sync_here`). `note_written` (under the store lock) can reach that path, as `write_at` could before.
- The doc comment of `seal` still says that `sync` and a flush wait for the seal through `sync_active`. They now use `lock_no_seal` and `join_seal`. Only the comment is out of date.
- Phase 1 of `sync` and the step-1 waits of `flush_if_full` still have no time limit. They were there before this plan, and they hold no lock.

## User-visible change

- A put that waits for another thread's job (a pack create, a record write, a seal, or a background sync at `KEEP_CAP`) now fails after 10 s with an I/O error of kind `TimedOut`. `gc_begin`, `close` and the GC waits do the same. The file system returns this error to the caller as EIO. Before this plan these waits had no limit. Such a timeout does not mark the store failed, because nothing was lost.

## Threat model notes

- **T-19-42:** the dirty flags are cleared after the unlocked fsync, by mark. A seal still sees `dirty` until a sync has finished.
- **T-19-43:** the hwm is `a.len`, which never includes a write in flight. The run is installed only after the handle sync of the records below it. Mutation 1 is the evidence.
- **T-19-44:** copies drop by sequence, and flights and handles share one sequence. A sync that saw a failure drops nothing (see the deviations).
- **T-19-45:** every new wait has the `STORE_WAIT` limit.
- **T-19-46:** repairs run in the handle or flight thread with no store lock. The copies are cloned as `Arc<[u8]>` and are not locked during the re-write.
- **The 19-02 threat "the record write stays under the lock":** its mitigation is now the one-write rule. The record write leaves the lock, but at most one write is in flight per pack, at its end. `a.len`, the entries and the index move only at publish, so a pack never has a hole.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Bug] A sync next to a failed sync could drop copies whose bytes were never saved**
- **Found during:** Task 1 design
- **Issue:** with the plan's design, one failed background sync (`MemSync` marks every dirty page clean, as Linux does) followed by the new unlocked phase-2 handle sync would lose data. The second sync succeeds with nothing dirty, records "synced to mark", and drops the copies before they are written again.
- **Fix:**
  - The shared state counts failures (`fails`) and the last good repair (`repaired`).
  - A successful sync drops copies only if no failure happened while it ran and every earlier failure was repaired.
  - A repair re-writes every held copy, not only those up to the mark (as `repair` did before).
  - A write that saw a failure while it ran writes again before its copy is kept (`write_kept`).
- **Tests:** `a_write_that_overlaps_a_failed_sync_is_written_again` (Mutation 2 shows it red) and `a_single_sync_error_is_repaired_and_the_data_is_durable` (store level). The "sync that overlaps a failure" rule has no red test. `MemSync` saves the ranges it captured at its start even if another sync failed meanwhile, so the mock cannot show that case.
- **Files:** `retry.rs`, `retry_tests.rs`
- **Commit:** `0925802`

**2. [Rule 3 - Blocking] The background flight syncs through the handle already in Task 1, not in Task 2**
- With the old flight (raw inner handle, repair at `settle`) and the new unlocked handle repair, the two repair paths could race. So the flight goes through `RetrySync` from Task 1. Task 2 needed no further retry change.

**3. [Rule 1 - Bug] The copy of an unlocked record write is kept by the writer wrapper, not by `note_written`**
- **Issue:** if the copy is pushed at publish (as the plan says), a sync failure can happen between the end of the pwrite and the publish. Its repair then misses the copy, and a later sync drops a copy whose pages were marked clean.
- **Fix:** `RetryingPack::writer` returns a `RetryWrite` wrapper. It keeps the copy when the write returns, through the same `write_kept` failure check. `note_written` only starts a background flight past `KEEP_LIMIT`.
- **Commit:** `a7bff74`

**4. Test shape: `a_handle_sync_drops_the_copies_made_before_it` uses a hold**
- The plan's sequence (take the handle, write piece 4, sync) does not test the rule: the mark is taken when the sync starts, so piece 4 is covered too.
- The test parks the handle sync in `hold_syncs`, writes piece 4 while it is parked, and then checks that only piece 4's copy is kept.

**5. `lock_no_create` also waits for a record write; `lock_no_write` beside it**
- `gc_begin` seals the active pack, so it must wait for both. Both go through one `lock_when(busy, what)` helper with the 10 s limit. The GC install loop also waits for no write in flight, as the plan asks for "its sync".

**6. A typed timeout marker (`StoreWaitTimeout`)**
- `flush_if_full` sets `failed` on every error of `flush_frozen`. A `STORE_WAIT` timeout must not fail the store (T-19-45).
- A check of only the error kind would also skip `failed` for a real disk `TimedOut`. So the timeout error carries a private inner type, and `is_store_wait` tests for that type.

**7. The publish replaces an entry that it finds, where the plan says "keep it"**
- No GC copy can run while a write is in flight, so this case should not happen.
- `publish` looks the ID up again only if the index generation changed since the hold that started the write. That lookup runs under the lock, as the put loop does.
- If an entry is found, the new record replaces it (it is the newest record, as in `Inner::replay`) and is not counted again. This keeps the new record's pack `.idx` exact.

**8. Test name:** the retry-layer variant of the phase-2 test is `retrying_put_and_read_pass_a_held_phase_two_sync`. With this name the acceptance grep for `fn put_and_read_pass_a_held_phase_two_sync` counts 1.

### Known limitation

- Many puts in a row can delay `gc_begin` or `close`: after each publish, a waiting put can take the lock first and start the next write. Each single wait has the 10 s limit, and parking_lot's eventual fairness bounds the delay. Nothing in the suite showed this.

## Verification

- `cargo test -p chunk-store --lib -- lock_tail_tests retry::`: 12 lock_tail tests and 3 retry unit tests ok (15 passed). They include `put_and_read_pass_a_held_phase_two_sync`, `put_and_read_pass_a_held_index_flush_sync`, `put_and_read_pass_a_held_record_write`, `a_record_in_flight_is_invisible_and_below_no_hwm`, `gc_begin_waits_for_a_record_write_with_no_lock` and `a_handle_sync_drops_the_copies_made_before_it`.
- `cargo test -p chunk-store --test retry_tests`: 16 passed. They include the 5 that the plan names, plus `a_failed_handle_sync_fails_the_store` and `a_single_sync_error_is_repaired_and_the_data_is_durable`.
- `cargo test -p chunk-store`: all green (lib 127 tests).
- `cargo test -p slicefs-cli --test gc_online_tests --test lock_free_io`: 16 and 15 passed (part of the workspace run).
- `cargo test --workspace` exited 0 before each task commit (three runs).
- `stress_harness`: 9.48 s, the same before and after Task 3 (10 s budget). Nothing was added to it.
- Acceptance greps on the final tree:
  - Task 1: 0 / 1 / 1 / 1 / 1 / 1.
  - Task 2: 0 / 10 / 1.
  - Task 3: 0 / 3 / 1.
- Full suite command of 19-VALIDATION (`cargo test --workspace && cargo check --workspace --all-targets && cargo check --workspace --all-targets --features slicefs-cli/lock-stats && cargo check -p slicefs-cli --features deadlock-detect`): exit 0. The log has 68 `test result: ok` lines (with the `lock_stats_report` line) and 0 FAILED. The only warnings are from the vendored `fuser`. `stress_harness` took 9.64 s in this run.
- `cargo test -p slicefs-cli --features lock-stats --test lock_stats_report`: exit 0 (1 passed).

## Self-Check: PASSED

- All 6 modified files and this SUMMARY exist.
- The commits `0925802`, `5923100` and `a7bff74` are on HEAD.
