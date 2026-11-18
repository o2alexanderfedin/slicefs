---
phase: 14-buffered-delta-re-log
plan: 09
subsystem: metadata/wal
tags: [write-log, wal, ram, crash-safety, "#98"]
status: complete
requires: []
provides:
  - "per-op: each operation record is in the segment file when log_mutation returns (no sync)"
  - "flush-on-fsync and periodic: at most BUFFER_CAP_BYTES (4 MiB) of records in RAM"
  - "periodic: a timer thread syncs every interval_secs"
affects: [slicefs-cli mount --wal-strategy, verify-phases.sh 05c]
tech-stack:
  added: []
  patterns: ["shared BufferedLog for the two buffered modes", "timer thread stopped by dropping an mpsc sender"]
key-files:
  created:
    - crates/metadata/src/wal/buffered.rs
  modified:
    - crates/metadata/src/segment/writer.rs
    - crates/metadata/src/wal/mod.rs
    - crates/metadata/src/wal/per_op.rs
    - crates/metadata/src/wal/flush_on_fsync.rs
    - crates/metadata/src/wal/periodic.rs
    - crates/slicefs-cli/src/cli.rs
    - scripts/docker-linux-test/verify-phases.sh
    - docs/design/write-log.md
decisions:
  - "Cap check is 'held bytes > BUFFER_CAP_BYTES' after the push, with the frame size (17 bytes + payload) as the record size, so the RAM after log_mutation is at most 4 MiB"
  - "PeriodicWal::new keeps no timer (existing tests unchanged); PeriodicWal::with_interval(path, id, Duration) starts it; create_wal uses with_interval(interval_secs)"
  - "05c control runs in its own flush-on-fsync mount (fresh store, --commit-interval 0); the per-op mount has only the main steps"
  - "Design note subsection appended at the end of write-log.md to keep away from the sections that plan 14-07 edits"
metrics:
  completed: 2026-11-18
  tasks: 4
actuals:
  tokens: 9000
  tasks: 4
  commits: 6
plan_head_before: 4a27c38
plan_head_after: dd8b461
---

# Phase 14 Plan 09: Write-log RAM, option A Summary

Per-op writes each operation record into the kernel at once (a completed
request survives kill -9 of the daemon); flush-on-fsync and periodic keep at
most 4 MiB of records in RAM; periodic gets its timer. The number of disk
syncs does not change.

## Commits

| Task | Commit | What |
|---|---|---|
| plan | 50b75f7 | 14-09-PLAN.md |
| 1 | 4269b6a | `SegmentWriter::flush`; per-op flushes after each Op record |
| 2 | c31b63c | `BufferedLog` (wal/buffered.rs), `BUFFER_CAP_BYTES = 4 MiB` cap |
| 3 | 658779d | periodic timer thread (`with_interval`), stopped on drop and shutdown |
| 4 | 33b0ebd | help text, wal module docs, verify-phases.sh 05c control |
| 4 | dd8b461 | write-log.md subsection "Write-log RAM (#98)" |

## Tests

New (in-crate, `cargo test -p metadata --lib wal::`):
- `wal::per_op::tests::op_record_reaches_the_file_without_a_sync`
- `wal::flush_on_fsync::tests::ram_stays_at_most_the_cap_with_no_fsync`
- `wal::periodic::tests::ram_stays_at_most_the_cap_with_no_fsync`
- `wal::periodic::tests::timer_syncs_and_drop_stops_it` (interval 300 ms,
  poll to 2 x interval + 300 ms; drop must release the last reference of the
  log, so the thread has ended)

The cap tests also check that the records that left RAM are in the file, in
order (a second reader while the WAL is open), so a cap that drops records
fails.

RED on the old behaviour (the tests and the refactor to `BufferedLog` with no
cap and a `with_interval` that ignored the interval):

```
test wal::per_op::tests::op_record_reaches_the_file_without_a_sync ... FAILED
  the Op record must be in the file before any sync, got 0 records
test wal::periodic::tests::ram_stays_at_most_the_cap_with_no_fsync ... FAILED
  after record 63: 4195392 bytes held in RAM, cap 4194304
test wal::flush_on_fsync::tests::ram_stays_at_most_the_cap_with_no_fsync ... FAILED
  after record 63: 4195392 bytes held in RAM, cap 4194304
test wal::periodic::tests::timer_syncs_and_drop_stops_it ... FAILED
  no sync within 905.821524ms (interval 300ms)
test result: FAILED. 0 passed; 4 failed
```

GREEN, with no change to existing tests:
- `cargo test -p metadata`: all pass (134 lib tests, all integration tests).
- slicefs-cli: crash_recovery_tests 6, fsync_tests 7, mount_lifecycle_tests 41,
  wal_durability_tests 2, wal_strategy_tests 5, write_log_replay_tests 18: all pass.

Docker (`SLICEFS_TARGET_VOL=slicefs-target-14-09 run.sh verify`, debug build,
OrbStack): `RESULT verify-phases: PASS`. 05c lines:

```
    Replayed 38 request(s) from the write log (0 skipped).
  rename-survives-dir-fsync: ok
  old-name-gone-after-dir-fsync: ok
    rename with no sync after it (flush-on-fsync): lost
```

05d (torn last record) and the 1 GiB large copy also pass (VmHWM 72 MiB, cap
512 MiB). The volume `slicefs-target-14-09` was deleted after the run.

## Deviations from Plan

1. **[Rule 3 - Blocking] Commit branch.** The executor commit guard allows only
   `agent-*` branches in a worktree. The brief names `feature/14-09-wal-ram`,
   so the commits are on that branch.
2. **Tests are in-crate, not in tests/wal_tests.rs.** The sync count and the
   held bytes are only visible inside the crate (`#[cfg(test)]` accessors on
   `BufferedLog`). The plan was corrected before its commit.
3. **The brief's "drop without sync, then read" option is not used** for the
   per-op test: `BufWriter` flushes on drop, so it gives no RED. The test reads
   the file with a second reader while the WAL is open.
4. **05c control is an echo, not a step** (as before). Its result is quoted
   above. The step names did not change.

## Notes

- Per-op 05c main steps now pass also because per-op keeps every completed
  request; the flush-on-fsync control is what shows that a rename with no sync
  is lost.
- If `write_entry` fails in the middle of a cap spill, the remaining held
  records are dropped and the error goes to the caller. This is the same as
  `flush_and_sync` before this plan.

## Self-Check: PASSED

- FOUND: crates/metadata/src/wal/buffered.rs
- FOUND: 50b75f7, 4269b6a, c31b63c, 658779d, 33b0ebd, dd8b461 (ancestors of HEAD)
