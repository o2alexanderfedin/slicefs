---
phase: 19-fine-grained-locks
plan: 13
subsystem: concurrency
status: complete
tags: [lock-01, lock-04, wal, segment, mkdir, gap-closure, d-08]

requires:
  - phase: 19-fine-grained-locks
    provides: "19-04 append/write_out split and SyncMark; 19-10 new_entry number-first stripes; 19-12 SyncWait::wait_timeout and the one-write rule for packs; spike 19g (wal_writer hold p99 about 20 ms behind F_FULLFSYNC)"
provides:
  - "Positional SegmentWriter: begin_write / WriteSlot::write / end_write, in_flight; no buffered stream"
  - "PerOpWal and BufferedLog write records with no WAL lock (one write in flight per segment)"
  - "open_next_segment: a cut creates the new segment before it takes writer; cut_segment only swaps and reserves the Cut record"
  - "WRITE_WAIT (10 s) and wait_for: every wait for a record write has a limit, then TimedOut"
  - "DictMetadataStore::reserve_ino; mkdir locks [parent, ino] before the link; test_set_mkdir_hook"
affects: [19-14, 19-15, 19-16, 20]

estimate:
  tokens: 130000
  tasks: 3
actuals:
  tokens: 16700
  tasks: 3
  commits: 3

plan_head_before: 2653597893a7821e207538fd45d21d4e0de4308c
plan_head_after: 7041add5af8c191f6590de5dc13af94f01aced2c
commits: 3

tech-stack:
  added: []
  patterns:
    - "One record write in flight per segment: sequence numbers and offset reserved under writer, frames and CRC built and written with no lock, published under writer; end offset, seq and done move only at publish, so no gap"
    - "Number first, then stripes: reserve the inode number, lock [parent, ino], then link (mkdir now as create, mknod, symlink)"

key-files:
  created: []
  modified:
    - crates/metadata/src/segment/writer.rs
    - crates/metadata/src/wal/mod.rs
    - crates/metadata/src/wal/per_op.rs
    - crates/metadata/src/wal/buffered.rs
    - crates/metadata/src/wal/flush_on_fsync.rs
    - crates/metadata/src/store.rs
    - crates/slicefs-cli/src/filesystem.rs
    - crates/slicefs-cli/src/handlers.rs
    - crates/slicefs-cli/tests/write_log_replay_tests.rs
    - crates/chunk-store/src/lib.rs

key-decisions:
  - "G2 (WAL half): no WAL lock (writer, queue, pending) is held during a record write; one write in flight per segment, at its end"
  - "A cut, rotate or install writes out the records appended before it was called, then waits for no write in flight; records appended later may stay queued and go to the segment that is current when they are written"
  - "A failed log never writes: a write-out takes its queue out as lost; a cut drops the queue, sets old_failed and clears the failure, so the repair commit never waits for the queue"
  - "A failed Cut record write sets the failure again: write-outs and fsyncs fail until the next cut; the cut itself stands"
  - "G4: mkdir reserves the number (reserve_ino), locks [parent, ino], then calls create_directory_at"

patterns-established:
  - "WAL park points for tests: pwrite (the unlocked record write, both strategies), cut_create (the new segment create, both strategies)"

requirements-completed: []

duration: 59min
completed: 2026-11-22
---

# Phase 19 Plan 13: WAL record writes with no WAL lock, and mkdir stripes Summary

**The WAL locks now cover memory work only. Every strategy writes its records with a positional write and no WAL lock held, with one write in flight per segment. A cut creates its new segment file before it takes `writer`. mkdir locks the new directory's stripe before the directory is linked.**

## Performance

- **Duration:** about 59 min
- **Started:** 2026-11-22T14:29:45Z
- **Completed:** 2026-11-22T15:28:40Z
- **Tasks:** 3
- **Files:** 10 modified

## Accomplishments

- **Task 1 (tracer, G2 PerOp):**
  - `SegmentWriter` holds an `Arc<File>` and its end offset. Every write is positional (`write_all_at`). The buffered stream writer is gone.
  - Sequential callers (seeds, the EOF marker, compaction, tests) use `write_entry` into a small buffer. The buffer is written by `flush`, by `sync`, past 64 KiB, or on drop.
  - `begin_write(n)` reserves `seq..seq+n` and the offset and marks a write in flight (a `SyncWait`). `WriteSlot::write` frames the batch (CRC outside any lock) and writes it with one positional write. `end_write` publishes it on success; on an error the offset and seq stay where they were. `end_sync` clears `unsynced` only if no write started or finished since its mark.
  - `PerOpWal::write_upto` takes the batch under `writer` and writes it with no lock. Then it publishes under `writer` and moves `done`. A write-out that needs a later record waits for the write in flight with no lock (`WRITE_WAIT`, 10 s, then `TimedOut`).
  - `settled()` serves cut, rotate and install_root. It writes out every record appended before the call, then returns `writer` with nothing in flight. On a failed log it drops the queue as lost.
  - fsync writes out every LSN given out so far, then runs `sync_current` with a prepare that only checks `failed`.
- **Task 2 (G2 buffered, cut):**
  - `BufferedLog::write_held` uses the same slot for the cap write-out, fsync, cut and install_root.
  - `open_next_segment` creates the next segment (header only) with no WAL lock held. Under `writer`, `cut_segment` checks that `loc` did not move. It moves the old writer to `behind`, installs the new one and reserves the Cut record's slot in the same hold. `write_cut_record` writes the Cut record with no lock. A failure there marks the log failed until the next cut.
  - `rotate` waits for no write in flight and still holds `writer` across its sync (offline only). Its doc comment says so.
- **Task 3 (G4, D-08):**
  - `reserve_ino` takes a number and inserts nothing.
  - `mkdir_logged` takes the number first and locks `[parent, ino]`. Then it checks the live parent, calls `create_directory_at`, and appends the `Mkdir` record in that hold.
  - `test_set_mkdir_hook` fires after the link, with the stripes held.

## Task Commits

1. **Task 1: positional SegmentWriter, PerOp record writes with no WAL lock** (tracer): `f45a060`
2. **Task 2: buffered record writes and the cut file create leave the WAL lock** (TDD): `f4caa75`
3. **Task 3: mkdir locks the parent's and the new directory's stripes** (TDD): `7041add`

## TDD and red-run evidence

All red runs were local, before the fix. Each test was committed together with its fix, so no commit is red.

- **Task 1:** `the_writer_lock_is_free_during_a_record_write`, with the "pwrite" park point inside the old locked `drain`: `the writer lock was held during the record write: Timeout`.
- **Task 2:** with "cut_create" inside the old `writer` hold and "pwrite" inside the old locked buffered write-out, all three new tests failed:
  - `cut_opens_the_new_segment_before_the_writer`: "the writer lock was held while the cut created its segment".
  - `buffered_cut_opens_the_new_segment_before_the_writer`: the same message.
  - `buffered_write_out_passes_a_parked_record_write`: "the writer lock was held during the record write".
- **Task 3:** `a_create_in_a_new_directory_waits_for_its_mkdir_record` on the old mkdir: `write log record 2 (create) did not replay: Create { parent: 3, name: "f", .. ino: 4 }`. B's create finished during the park, and replay stopped at the create record.
- **Tracer gate (Task 1):** the verify ran again after the commit and passed: 23 wal::/segment:: tests, including both named tests.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Bug] Cut, rotate and install wait for the records before the call, not for an empty queue**
- **Issue:** the plan says these wait under `writer` "until nothing is in flight and the queue is empty". With requests that keep appending (`no_deadlock_between_append_cut_and_sync`), an empty queue may never come before the 10 s limit.
- **Fix:** `settled()` writes out up to the queue's last LSN at the time of the call, then waits only for no write in flight (one deadline for the whole loop). Records appended later stay queued. Their LSNs are above every record already written, so they go to whichever segment is current when they are written out, after the Cut record or the seed. In production a cut runs under the exclusive commit gate, so the queue is empty there anyway.
- **Commit:** `f45a060`

**2. install_root also waits for no write in flight**
- It uses the same `settled()` helper, as the plan says. A write in flight then holds only records appended after the call, so the wait is not needed for the promise. It is short and bounded.

**3. `WriteSlot::write` takes an iterator of entries (`Borrow<SegmentEntry>`), not `&[SegmentEntry]`**
- With this, PerOp passes its queue in place, and the buffered log converts one entry at a time, so no second vector of entries is built. The slot still frames the whole batch into one buffer while the batch is alive. The peak is the batch plus one framed copy: about two times the 4 MiB cap for the buffered log, which is bounded.

**4. The fault hook moves into the slot**
- `begin_write` takes `fail_next` into the slot, and `WriteSlot::write` fails. This is the slot-write hook that Task 2 asks for. A PerOp test-only flag `fail_cut_record` arms it on the next cut's new writer.

**5. The buffered commit variant fails at the cap**
- `a_buffered_commit_after_a_failed_write_with_queued_records_succeeds` (in `flush_on_fsync.rs`) gets its failed write from a write-out past the cap. A buffered log does not keep records after a failure, so the "queued" record there is dropped at once.

**6. The buffered cut test compares with the old segment's count**
- B's append during the park comes before the cut's write-out, so the old segment has 4 records, not 3. The assertion reads the count from the old segment. The first version of the test was wrong.

**7. The filler loop in the Task 3 test also checks the file's number**
- The created file gets the number `d + 1`. If its stripe were ROOT's, the old code would block too and the red run would prove nothing. So the setup needs both `stripe(d)` and `stripe(d + 1)` to differ from ROOT's stripe.

### Extra (orchestrator request)

- The doc comment of `seal` in `crates/chunk-store/src/lib.rs` now says that `sync` and a flush wait through `lock_no_seal` and `join_seal`, and that GC (`sync_active`) and the next seal wait for it. Only the comment changed. This file is outside the plan's `files_modified`. Commit `f4caa75`.
- The doc comment of `handle_mkdir_at` (handlers.rs) now names both stripes. Commit `7041add`.

## Locked paths that stay

- `rotate` (both strategies) holds `writer` across the new segment's create, seed write and sync. It runs offline only (`snapshot switch`), after it waits for no write in flight.
- `write_root_segment` writes and syncs the root segment with no WAL lock (unchanged).
- `Behind::settle` and `sync_current` take handles under their locks and sync with no lock (unchanged).

## User-visible change

- A write-out, fsync, cut, rotate or install that waits more than 10 s for another thread's record write fails with an I/O error of kind `TimedOut`. The WAL is not marked failed, because nothing was lost. But the store's `log_write_out` sends every error, `TimedOut` too, to `note_wal_failure`. So after a request's write-out times out, `flush_wal` (fsync) fails until the next commit. Before this plan, these paths waited on the `writer` lock with no limit.

## Threat model notes

- **T-19-47:** one write in flight per segment at its end. The end offset, `seq` and `done` move only at publish (`the_writer_lock_is_free_during_a_record_write` checks the order).
- **T-19-48:** a write-out waits for the write that holds its LSN. fsync writes out up to the last LSN first. `end_sync` clears `unsynced` only if no write started or finished since its mark (`flush_and_sync_waits_for_a_parked_record_write`).
- **T-19-49:** the Cut slot is reserved in the swap hold, before any request can reserve. The tests read the Cut record first (seq 0).
- **T-19-50:** every wait has the `WRITE_WAIT` limit. A cut of a failed log drops the queue instead of waiting. A failed Cut write fails until the next cut (`a_failed_cut_record_write_fails_until_the_next_cut`).
- **T-19-51:** mkdir locks `[parent, ino]` before the link. The Task 3 test is red on the old code.

## Verification

- `cargo test -p metadata --lib -- wal:: segment::`: 23 passed after Task 1. `wal::` had 26 passed after Task 2. The named tests are all ok.
- `cargo test -p metadata --test write_log_tests` (9), `wal_durability_tests` (2), `write_log_replay_tests` (34, then 35), `crash_recovery_tests` (6): all green. `cargo test -p metadata` is green. Its only "running 0 tests" is `Doc-tests metadata`, which has no doc tests (this was already true before the plan).
- `dir_link_tests` (31), `multi_handle_posix` (13), and `metadata --lib create_directory_at` (1): all green.
- `cargo test --workspace` exited 0 before each task commit. Task 3's run was the full suite run below.
- Full suite command of 19-VALIDATION: exit 0, with 68 `test result: ok` lines and 0 FAILED. The only warnings come from the vendored `fuser`.
- `cargo test -p slicefs-cli --features lock-stats --test lock_stats_report`: exit 0. No new lock `Site` was added.
- `stress_harness` took 9.00 s, 9.61 s and 9.37 s in the three runs (budget 10 s). Nothing was added to it.
- Acceptance greps on the final tree:
  - Task 1: 1 / 0 / 1.
  - Task 2: 0 / 1 / 1 / 2.
  - Task 3: 1 / 0 / 1.

## Self-Check: PASSED

- All 10 modified files and this SUMMARY exist.
- The commits `f45a060`, `f4caa75` and `7041add` are on HEAD.
