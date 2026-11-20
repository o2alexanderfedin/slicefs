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
  tokens: 3703      # chars/4 over the added test diff lines 0285b2a..bc0fbe7 (14812 chars)
  tasks: 2
  commits: 3        # test commits 36b39e3, 90dbeeb, bc0fbe7 (git rev-list --count 0285b2a..bc0fbe7 = 5, minus 2 SUMMARY commits)
plan_head_before: 0285b2a
plan_head_after: bc0fbe7df2c393fc16985ae4974c62bdef662b79  # last test commit

tech-stack:
  added: []
  patterns:
    - "dump_open: the dump as stat sees it (test_getattr), for a crash with write handles open"
    - "check_with(WalConfig, scenario) returns the replayed fs for more checks"
    - "reload_and_replay_with(store, setup): a test hook set before replay"

key-files:
  created: []
  modified:
    - crates/slicefs-cli/tests/write_log_replay_tests.rs
    - crates/slicefs-cli/tests/fsync_tests.rs

key-decisions:
  - "A crash with an open overlay is compared with test_getattr on both sides: the overlay size and mtime are in the handle, not in the inode, until release"
  - "The MAX_PAYLOAD test sets the dirty limit to 256 MiB, not 96 MiB: the extent capacity doubles to 128 MiB at 65 MiB of length"
  - "Test sizes cut on the coordinator's request (short tests): matrix file 4.5 MiB, relog test 4 and 16 MiB, MAX_PAYLOAD test 65 MiB"

requirements-completed: [BUFR-01, BUFR-02, BUFR-03]  # BUFR-03: the commit-gate part (no rebase or hash in fsync and commit_cut)

duration: ~60 min
completed: 2026-11-18
---

# Phase 14 Plan 06: Relog size, no rebase at commit, replay matrix Summary

In-process tests show four things:
- A commit relogs only the bytes changed since the last rebase: 4096, 8192 and 4096 bytes. The values are the same for a 4 MiB and a 16 MiB file.
- fsync and commit_cut run no rebase.
- Replay after a crash gives the same file for both log modes at three crash points.
- A relog of more than 64 MiB replays.

No production code changed. No test found a production bug.

## New tests (exact names)

| File | Test | Requirement |
|------|------|-------------|
| write_log_replay_tests.rs | `a_commit_relogs_only_the_bytes_changed_since_the_last_rebase` | BUFR-01 (D-05) |
| fsync_tests.rs | `commit_and_fsync_do_not_rebase_an_open_overlay` | BUFR-03, commit-gate part (D-09) |
| write_log_replay_tests.rs | `an_overlay_replays_after_a_crash_with_each_log_mode` | BUFR-02 (D-14, D-15, D-18) |
| write_log_replay_tests.rs | `an_overlay_with_more_than_64_mib_of_relog_replays` | BUFR-02 (D-20) |

New helpers in write_log_replay_tests.rs:
- `dump_open` and `dump_by`. `dump` now calls `dump_by` and gives the same output as before.
- `check_with`, `reload_and_replay_with`. `reload_and_replay` now calls `reload_and_replay_with` with no setup.
- `open_rw`, `put`, `at`, `overlay_scenario`, `CrashPoint`, `MIB`.

`check` is not changed.

## Relog values (BUFR-01)

| Step | 4 MiB file | 16 MiB file |
|------|-----------|-------------|
| One 4 KiB write, commit | 4096 | 4096 |
| A second 4 KiB write elsewhere, no rebase, commit | 8192 | 8192 |
| Limit 4096 for one 8 KiB write (rebase count +1), limit back to `DIRTY_LIMIT`, one 4 KiB write, commit | 4096 | 4096 |

The first version used 8 MiB and 32 MiB files and gave the same values: 4096, 8192, 4096. The rebase count does not change at any of the three commits. After the steps, the test crashes with the handle open and replays. The dump and the content are the same.

## Replay matrix (BUFR-02)

`an_overlay_replays_after_a_crash_with_each_log_mode` runs 6 cases: {PerOp, FlushOnFsync} x {RawRecords, RelogRecords, AfterRebase}. Each case uses a fresh store and handlers only. The clock is frozen at a different time for each step.

The overlay work is:
1. Stream a 3 MiB a.bin in 64 KiB writes, then a commit.
2. Open O_RDWR, then write 4 KiB at 1 MiB (the switch).
3. Shrink to 2 MiB, then extend to 3.5 MiB.
4. Write 4 KiB at 3 MiB.
5. Write 4 KiB at 4.5 MiB. This is a sparse write 1 MiB past the end, so [3.5 MiB, 4.5 MiB) is a hole.
6. chmod 0600.

The crash points add these steps:
- **RawRecords:** no more steps.
- **RelogRecords:** b.bin streams 50,000 bytes, then writes 300 bytes at 100, and stays open. This is a switch before the first flush (D-18). Then a commit. The test asserts `test_last_relog_bytes() == 3 * 4096 + 300`.
- **AfterRebase:**
  1. Set the limit to 64 KiB, then write 128 KiB at 512 KiB. The rebase count goes up by 1.
  2. Set the limit back, then write 4 KiB at 200.
  3. Commit. The relog is 4096 bytes.
  4. Write 4 KiB at 100, then fsync.

The test checks these items:
- The full dump as stat sees it: names, modes, sizes, mtime, ctime, atime, xattrs and the content hash.
- The content against a Vec model.
- Zeros in [2 MiB, 3 MiB).
- The size and the mode.

**Mutation check (not committed; done twice):** I removed the `Setattr(visible)` relog line in `commit_cut` (filesystem.rs) and ran the matrix. I did this on the first version (67.5 MiB file) and again on the final version (4.5 MiB file). Both runs failed at the dump compare in `check_with`, line 156, `assertion left == right failed: PerOp`. The final version failed in 2.18 s.

The panic line shows only the log mode, not the crash point. The failing case is inferred to be RelogRecords: RawRecords has no commit with an open overlay, so the mutation cannot change it. After each run, I restored filesystem.rs with `git checkout`. `git status` then showed no production change. So the matrix detects a missing shrink record (D-15).

## MAX_PAYLOAD regression (D-20)

`an_overlay_with_more_than_64_mib_of_relog_replays` uses one extent of 65 MiB, the smallest whole number of MiB above 64 MiB, plus 4 KiB at 0:
- **Data:** each MiB is one random MiB with its index in the first 8 bytes. So the test generates random data only once.
- **Limit:** the dirty limit is 256 MiB, both live and in replay (`reload_and_replay_with`). No rebase runs while the 65 pieces come in. One rebase runs at the end of replay.
- **Asserts:** no rebase in the loop, and the relog is more than 64 MiB. After the crash and replay, one read and one memcmp check the content. The test finds the first differing byte only on a failure.

The checks run on the final version are:
- A run with the limit at `96 << 20` failed the "no rebase" assert (`left: 1, right: 0`). Then I restored the file.
- Replay with rebases inside it is no longer exercised by this test. Before the cut, it was a side effect of the 70 MiB size and not a plan requirement.

## Test results (final run at bc0fbe7)

```
fsync_tests:            test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.72s
write_log_replay_tests: test result: ok. 18 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 8.55s
```

The 15 old tests of write_log_replay_tests and the 6 old tests of fsync_tests pass, and none of them changed.

**Time (debug build, macOS).** The replay binary took 2.08 s before this plan, in the 14-04 crate run. It took about 42 s with the first version, and takes 8.5 to 9 s now. The tests below were measured alone with `--exact`. A second plan ran on the same machine at the same time, so the numbers vary by about 1 s.

| Test | First version | Now |
|------|---------------|-----|
| `an_overlay_replays_after_a_crash_with_each_log_mode` | 40.4 s | 5.8 to 7.1 s |
| `an_overlay_with_more_than_64_mib_of_relog_replays` | 20.4 s | 7.4 s |
| `a_commit_relogs_only_the_bytes_changed_since_the_last_rebase` | 10.8 s | 4.5 to 5.7 s |

Where the time goes now:
- **Matrix:** about 0.9 s per case, measured with temporary timers that were not committed. The scenario takes 0.3 to 0.6 s: it generates 3 MiB of data, streams it and hashes it in a debug build. Replay takes about 0.1 s. The dumps and content reads take the rest.
- **MAX_PAYLOAD test:** about 2 s for 65 PerOp `Write` records of 1 MiB, then about 2.6 s for replay with the final rebase. A byte loop compare took about 4 s, so the test now uses memcmp.

## Deviations from Plan

**1. [Rule 3 - Blocking] `dump_open` and `check_with` compare what stat sees.** The plan said to compare `dump`, but `dump` reads the raw inode. For a handle that is open at the crash, the overlay size and mtime stay in the handle until release (`overlay_open_size`). Replay releases every handle at its end. So a raw dump of the live side shows the wrong values by design, not because of a bug. Both sides now use `test_getattr`. This is the stat view that `stat_of_an_open_file_shows_the_time_of_the_last_write` already tests. Commit 36b39e3.

**2. [Rule 1 - Plan parameter] Dirty limit 256 MiB in the MAX_PAYLOAD test, not 96 MiB.** `Overlay::cost` counts the extent capacity. `Vec::resize` doubles the capacity, so it is 128 MiB at 65 MiB of length. A run with 96 MiB confirmed this. Commit 90dbeeb.

**3. [Rule 2 - Stronger proof] Exact relog values in the matrix.** RelogRecords asserts `3 * 4096 + 300`, and AfterRebase asserts 4096 after the rebase, not only `> 0`. This mitigates T-14-12.

**4. [Coordinator request] Smaller test sizes.** The changes against the plan's numbers are below. Every case still covers the same paths: the sparse extend, the shrink below the base, the rebase, the 3 crash points and both log modes. Commit bc0fbe7.

| Item | Plan | Now |
|------|------|-----|
| Matrix sparse write | 3.5 MiB + 64 MiB | 3.5 MiB + 1 MiB |
| Relog test file sizes | 8 MiB and 32 MiB | 4 MiB and 16 MiB |
| MAX_PAYLOAD extent | 70 MiB | 65 MiB |
| Dirty limit in replay of the MAX_PAYLOAD test | default | 256 MiB |

**5. [Process] TDD / branch.** Both tasks are tests only, and the production code was already merged, so a RED commit was not possible. The mutation check above is the RED evidence for D-15. The launcher named the branch `feature/14-06-replay`. That name is not in the `agent-*` allow-list, as in 14-04. The protected-branch and detached-HEAD checks passed. STATE.md and ROADMAP.md were not changed.

## Open items

- The 14-04 open item is closed by crash point RelogRecords and the mutation check. That item was: no test reaches the relog `Setattr(visible)` record through a replay.
- Temporary edits for the probes were all reverted before any commit and are in no commit. They were: the filesystem.rs mutation (twice), the 96 MiB limit, a 1 MiB stream-write trial in the matrix (no time gain, so it was reverted), and the timing `eprintln!` lines.

## Threat Flags

None. Tests only.

## Known Stubs

None.

## Self-Check: PASSED

- FOUND: the 4 test names, 1 grep match each.
- FOUND in write_log_replay_tests.rs: `test_last_relog_bytes` on 3 or more lines, `DIRTY_LIMIT` on 2 or more, `WalConfig::FlushOnFsync` on 3, and 18 `#[test]` lines.
- FOUND in fsync_tests.rs: `test_rebase_count` on 5 lines.
- FOUND commits (ancestors of HEAD): 36b39e3, 90dbeeb, bc0fbe7.
- No production file changed: `git diff --stat 0285b2a HEAD` lists only the two test files and this SUMMARY.
