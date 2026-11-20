---
phase: 13-write-log-close-out
plan: 01
subsystem: write-log tests
tags: [write-log, wal, crash, replay, "#20"]
status: complete
requires: []
provides:
  - "In-process test: a rename is durable after fsync of the directory (flush-on-fsync log), plus its control test"
  - "End-to-end in-process test: a torn last record does not stop the mount"
affects: [13-03]
tech-stack:
  added: []
  patterns: ["simulated crash = drop or mem::forget of SliceFsFilesystem, then reload_and_replay"]
key-files:
  created: []
  modified:
    - crates/slicefs-cli/tests/write_log_replay_tests.rs
decisions:
  - "Both rename tests end with drop(fs), not mem::forget: no Drop implementation flushes the log, so the control test can catch a hidden flush"
  - "The sacrificial last record of the torn-tail test is one mkdir, which logs exactly one Op record"
metrics:
  duration: ~15 min
  completed: 2026-11-18
actuals:
  tokens: 2600
  tasks: 2
  commits: 2
plan_head_before: 1c22b19370e261722b0dbeee613a1bc6657bd90b
plan_head_after: 298c770
---

# Phase 13 Plan 01: Write log crash tests Summary

Three new in-process (simulated crash) tests for #20 in
`crates/slicefs-cli/tests/write_log_replay_tests.rs`. They cover a rename
that is durable only after an fsync of the directory, and a torn last record
in the newest log segment. No production code changed.

## New tests (exact names, for the #20 comment)

| Test | #20 item | What it shows |
|------|----------|---------------|
| `a_rename_survives_a_crash_after_fsync_of_the_directory` | Rename after directory fsync | Flush-on-fsync log. Write `doc.txt.tmp`, fsync it, rename it over `doc.txt`, call `handle_fsyncdir`, crash (`drop(fs)`), replay. `doc.txt` has the new bytes. `doc.txt.tmp` is gone. |
| `a_rename_with_no_sync_after_it_is_lost_with_a_buffered_log` | Control for the test above | The same steps with no `handle_fsyncdir`. After the replay, `doc.txt` still reads `old` and `doc.txt.tmp` still exists. Thus the first test passes only because of the fsync of the directory. |
| `a_torn_last_record_does_not_stop_the_mount` | Torn last record | PerOp log. Requests after the commit, then one mkdir `last` as the last record. Crash, then cut 3 bytes from the newest segment file. `load_store_with_log` and `oplog::replay` return Ok. The dump is equal to the dump before the mkdir, and `last` does not exist. After `commit()` (as a mount does), a new file is written and fsynced. It survives a second crash and replay, with `kept.bin` and `d/renamed`. |

Helpers added: `safe_save_then_crash(store, fsync_dir)` and `newest_segment(store)`
(the segment with the largest number, by number and not by name).

## Drop check

- `grep -rnE "impl(<[^>]*>)? *Drop" crates/metadata/src/wal crates/metadata/src/segment`
  finds nothing (0 lines).
- A wider grep over `crates/*/src` finds only `MemPack`, `MountLock`,
  `GcHandle`, `ReplayGuard` and `Clock`. None of them flushes the write log.
- In `flush_on_fsync.rs`, `log_mutation` only pushes to the in-memory
  `buffer: Mutex<Vec<WalEntry>>`. Only `flush_and_sync` and `rotate` write to
  the `SegmentWriter`, and both sync right after.
- Result: no Drop flush exists. The rename tests use `drop(fs)`. The control
  test passes, so neither a Drop flush nor a hidden sync keeps the rename.

## Torn-tail check

`handle_mkdir` logs one `Op::Mkdir` record. `DictMetadataStore::log_op` logs
one `WalEntry::Op`. The other `log_wal_entry` calls in `store.rs` are only for
`Snapshot` and `RootUpdate`. Thus the 3-byte cut tears only the mkdir record.
The cut is made before `reload_and_replay`, which makes segment 99.

## Test result

`cargo test -p slicefs-cli --test write_log_replay_tests`:

```
test result: ok. 15 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.31s
```

## Commits

- `d8b49d8` test(13-01): a rename survives a crash only after fsync of the directory
- `298c770` test(13-01): a torn last record does not stop the mount

## Deviations from Plan

None in the code. Process differences, set by the launcher:
- The work is on branch `feature/13-write-log-tests` (from `1c22b19`), not on an `agent-*` branch.
- `STATE.md`, `ROADMAP.md` and `REQUIREMENTS.md` were not updated.
- The plan commit ledger file was not written: the worktree guard refused the
  compound git command. `plan_head_before` is the base commit `1c22b19`.

## Known Stubs

None.

## Self-Check: PASSED

- `crates/slicefs-cli/tests/write_log_replay_tests.rs` has the three test names (1 match each).
- Commits `d8b49d8` and `298c770` are on `HEAD`.
- `git diff --stat 1c22b19..HEAD` shows only the test file (before this SUMMARY).
