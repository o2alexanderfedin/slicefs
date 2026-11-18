---
phase: 14-buffered-delta-re-log
plan: 06
subsystem: write-log
tags: [overlay, relog, replay, write-log, "#93", "#98"]
status: complete

requires:
  - phase: 14-04
    provides: "test_set_dirty_limit, test_rebase_count, test_last_relog_bytes, DIRTY_LIMIT, relog in 1 MiB pieces"
provides:
  - "BUFR-01 relog-size test, BUFR-03 commit-gate test, BUFR-02 replay matrix (PerOp, FlushOnFsync), 64 MiB relog regression"
affects: [14-08]

actuals:
  tokens: 3495      # chars/4 over the added diff lines 0285b2a..90dbeeb (13979 chars)
  tasks: 2
  commits: 2        # git rev-list --count 0285b2a..90dbeeb (task commits; this SUMMARY commit comes after)
plan_head_before: 0285b2a
plan_head_after: 90dbeeb59a43a53c4f0bd1cd66ff65eaad96239a

tech-stack:
  added: []
  patterns:
    - "dump_open: the dump as stat sees it (test_getattr), for a crash with write handles open"
    - "check_with(WalConfig, scenario) returns the replayed fs for more checks"

key-files:
  created: []
  modified:
    - crates/slicefs-cli/tests/write_log_replay_tests.rs
    - crates/slicefs-cli/tests/fsync_tests.rs

key-decisions:
  - "A crash with an open overlay is compared with test_getattr on both sides: the overlay size and mtime are in the handle, not in the inode, until release"
  - "The 70 MiB test sets the dirty limit to 256 MiB, not 96 MiB: the extent capacity doubles to 128 MiB at 65 MiB of length"

requirements-completed: [BUFR-01, BUFR-02, BUFR-03]  # BUFR-03: the commit-gate part (no rebase or hash in fsync and commit_cut)

duration: ~35 min
completed: 2026-11-18
---

# Phase 14 Plan 06: Relog size, no rebase at commit, replay matrix Summary

In-process tests show three things. A commit relogs only the bytes changed since the last rebase: 4096, 8192 and 4096 bytes, the same for an 8 MiB and a 32 MiB file. fsync and commit_cut run no rebase. Replay after a crash gives the same file for both log modes at three crash points, and a relog of more than 64 MiB replays. No production code changed. No test found a production bug.

## New tests (exact names)

| File | Test | Requirement |
|------|------|-------------|
| write_log_replay_tests.rs | `a_commit_relogs_only_the_bytes_changed_since_the_last_rebase` | BUFR-01 (D-05) |
| fsync_tests.rs | `commit_and_fsync_do_not_rebase_an_open_overlay` | BUFR-03, commit-gate part (D-09) |
| write_log_replay_tests.rs | `an_overlay_replays_after_a_crash_with_each_log_mode` | BUFR-02 (D-14, D-15, D-18) |
| write_log_replay_tests.rs | `an_overlay_with_more_than_64_mib_of_relog_replays` | BUFR-02 (D-20) |

New helpers in write_log_replay_tests.rs: `dump_open`, `dump_by` (`dump` now calls it with the same behavior), `check_with`, `open_rw`, `put`, `at`, `overlay_scenario`, `CrashPoint`. `check` is not changed.

## Relog values (BUFR-01)

| Step | 8 MiB file | 32 MiB file |
|------|-----------|-------------|
| One 4 KiB write, commit | 4096 | 4096 |
| A second 4 KiB write elsewhere, no rebase, commit | 8192 | 8192 |
| Limit 4096 for one 8 KiB write (rebase count +1), limit back to `DIRTY_LIMIT`, one 4 KiB write, commit | 4096 | 4096 |

The rebase count does not change at any of the three commits. After the steps, the test crashes with the handle open and replays. The dump and the content are the same.

## Replay matrix (BUFR-02)

`an_overlay_replays_after_a_crash_with_each_log_mode` runs 6 cases: {PerOp, FlushOnFsync} x {RawRecords, RelogRecords, AfterRebase}. Each case uses a fresh store, handlers only, and a frozen clock with a different time for each step. The overlay work is: a 3 MiB a.bin, a commit, open O_RDWR, 4 KiB at 1 MiB (the switch), shrink to 2 MiB, extend to 3.5 MiB, 4 KiB at 3 MiB, 4 KiB at 3.5 MiB + 64 MiB, then chmod 0600.

- RawRecords: no more steps.
- RelogRecords: b.bin streams 50,000 bytes, then 300 bytes at 100 (a switch before the first flush, D-18), and stays open. Then a commit. The assert is `test_last_relog_bytes() == 3 * 4096 + 300`.
- AfterRebase: limit 64 KiB, 128 KiB at 512 KiB (rebase count +1), limit back, 4 KiB at 200, commit (relog == 4096), 4 KiB at 100, then fsync.

The checks are: the full dump as stat sees it (names, modes, sizes, mtime, ctime, atime, xattrs, content hash), the content against a Vec model, zeros in [2 MiB, 3 MiB), and size and mode.

**Mutation check (not committed):** I removed the `Setattr(visible)` relog line in `commit_cut` (filesystem.rs) and ran the matrix. It failed at the dump compare in `check_with`. The panic line showed only the log mode, not the crash point. The failing case is inferred to be RelogRecords: RawRecords has no commit with an open overlay, so the mutation cannot change it. Then I restored filesystem.rs with `git checkout`, and `git status` showed no production change. So the matrix detects a missing shrink record (D-15).

## Test results (final run at HEAD 90dbeeb, the code of this SUMMARY)

```
fsync_tests:            test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.69s
write_log_replay_tests: test result: ok. 18 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 41.73s
```

The 15 old tests of write_log_replay_tests and the 6 old tests of fsync_tests pass, and none of them changed.

**Time (debug build, macOS).** The replay binary took 2.08 s before this plan (the 14-04 crate run). It takes about 42 s now. Measured alone (`--exact`):

| Test | Time |
|------|------|
| `an_overlay_replays_after_a_crash_with_each_log_mode` | 40.4 s |
| `an_overlay_with_more_than_64_mib_of_relog_replays` | 20.4 s |
| `a_commit_relogs_only_the_bytes_changed_since_the_last_rebase` | 10.8 s |

The matrix cost comes from its 6 cases. In each case, the 67.5 MiB sparse file is read 4 times (the live and replayed dumps, the live content check, and the replayed content check), and the release at the end of replay rebases it. AfterRebase adds one more rebase of 67.5 MiB. The orchestrator decides if this fits the time budget. To make the test shorter, move the sparse write closer to the end of the file. The plan asked for 3.5 MiB + 64 MiB, so this plan keeps that offset.

## Deviations from Plan

**1. [Rule 3 - Blocking] `dump_open` and `check_with` compare what stat sees.** The plan said to compare `dump`. `dump` reads the raw inode. For a handle that is open at the crash, the overlay size and mtime are only in the handle until release (`overlay_open_size`). Replay releases every handle at its end. So a raw dump of the live side shows the wrong values by design, not because of a bug. Both sides now use `test_getattr`, which is the stat view that `stat_of_an_open_file_shows_the_time_of_the_last_write` already tests. `dump` keeps its old output, because it calls `dump_by` with `get_inode`. Commit 36b39e3.

**2. [Rule 1 - Plan parameter] Dirty limit 256 MiB in the 70 MiB test, not 96 MiB.** `Overlay::cost` counts the extent capacity. `Vec::resize` doubles the capacity, so it is 128 MiB at 65 MiB of length. The test now asserts that the rebase count stays the same across the 70 writes. I ran the test once with `96 << 20` (not committed). It failed at that assert (`left: 1, right: 0`): one rebase ran inside the loop. Then I restored the file. Commit 90dbeeb.

**3. [Rule 2 - Stronger proof] Exact relog values in the matrix.** RelogRecords asserts `3 * 4096 + 300`, and AfterRebase asserts 4096 after the rebase, not only `> 0`. This mitigates T-14-12.

**4. [Process] TDD / branch.** Both tasks are tests only, and the production code was already merged, so a RED commit was not possible. The mutation check above is the RED evidence for D-15. The branch `feature/14-06-replay` was named by the launcher. It is not in the `agent-*` allow-list, as in 14-04. The protected-branch and detached-HEAD checks passed. STATE.md and ROADMAP.md were not changed.

## Open items

- The 14-04 open item ("no test reaches the relog `Setattr(visible)` record through a replay") is closed by crash point RelogRecords and the mutation check.
- The replay test binary went from about 2 s to about 42 s (see Time above). The orchestrator decides.
- Two temporary edits were made for the probes and reverted before any commit: filesystem.rs (the mutation check) and the 96 MiB limit in the test file. Neither is in a commit.

## Threat Flags

None. Tests only.

## Known Stubs

None.

## Self-Check: PASSED

- FOUND: the 4 test names (grep counts 1 each); `test_last_relog_bytes` 3+ lines and `DIRTY_LIMIT` 2+ lines in write_log_replay_tests.rs; `test_rebase_count` 5 lines in fsync_tests.rs; `WalConfig::FlushOnFsync` 3; `#[test]` 18
- FOUND commits (ancestors of HEAD): 36b39e3, 90dbeeb
- No production file changed: `git diff --stat 0285b2a HEAD` lists only the two test files
