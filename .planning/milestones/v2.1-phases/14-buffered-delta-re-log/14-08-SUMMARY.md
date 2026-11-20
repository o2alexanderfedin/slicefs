---
phase: 14-buffered-delta-re-log
plan: 08
subsystem: close-out
tags: [github, issues, "#98", "#93", close-out]
status: complete
requires:
  - phase: 14-07
    provides: "Docker VmHWM numbers, final DIRTY_LIMIT"
  - phase: 14-09
    provides: "write-log RAM option A (per-op flush, 4 MiB cap, periodic timer, sticky failure)"
provides:
  - "#98 and #93 closed with evidence comments"
  - "Follow-up issues #105 (one memory limit for all open files) and #106 (replay loads all pending records into RAM)"
affects: []
tech-stack:
  added: []
  patterns: []
key-files:
  created:
    - .planning/phases/14-buffered-delta-re-log/14-issue-98-comment.md
    - .planning/phases/14-buffered-delta-re-log/14-issue-93-comment.md
    - .planning/phases/14-buffered-delta-re-log/14-followup-global-limit.md
    - .planning/phases/14-buffered-delta-re-log/14-followup-log-ram.md
  modified: []
decisions:
  - "The write-log RAM follow-up is dropped: 14-09 bounds the flush-on-fsync and periodic modes. 14-followup-log-ram.md now covers replay, which still loads every record after the last root into one Vec (load_store_with_log, mount.rs)"
  - "The 5-8 s slowest write is one 'Open point' sentence in #98, no separate issue (user decision, investigation running)"
  - "PR, merge SHA and CI URL slots filled in Task 1, not Task 3 (launcher request)"
requirements-completed: [BUFR-01, BUFR-02, BUFR-03, BUFR-04, BUFR-05]
metrics:
  completed: 2026-11-18
estimate:
  tokens: 35000
  tasks: 3
actuals:
  tokens: 1940    # chars/4 over the four text files (7752 chars)
  tasks: 3
  commits: 3      # git rev-list --count 75a9743..09d4220 (before this SUMMARY commit)
plan_head_before: 75a9743d6f0e4fbb5cc0fa0c8c6057540e122453
plan_head_after: 09d4220f53d6fb814f5688ebac78b6582a7152a8
---

# Phase 14 Plan 08: Close-out of #98 and #93 Summary

#98 and #93 are closed with evidence comments, and two follow-up issues are open: #105 (one memory limit for all open files) and #106 (replay at mount loads all write-log records after the last commit into RAM).

## Public record

| Item | URL |
|------|-----|
| Follow-up A: one memory limit for all open files | https://github.com/o2alexanderfedin/slicefs/issues/105 |
| Follow-up B: replay loads all pending records into RAM | https://github.com/o2alexanderfedin/slicefs/issues/106 |
| Comment on #93 (then closed, completed) | https://github.com/o2alexanderfedin/slicefs/issues/93#issuecomment-6994417575 |
| Comment on #98 (then closed, completed) | https://github.com/o2alexanderfedin/slicefs/issues/98#issuecomment-6994420162 |
| Drafts PR (open, the coordinator merges it) | https://github.com/o2alexanderfedin/slicefs/pull/104 |

Precondition checked right before any write: PR #103 state MERGED, merge commit 75a9743d6f0e4fbb5cc0fa0c8c6057540e122453, CI run https://github.com/o2alexanderfedin/slicefs/actions/runs/44411885166 conclusion success. Approval of the four texts ("14 approved") came from the user through the coordinator.

Task 3 verify: #98 and #93 are CLOSED. The last comment on #98 has VmHWM, #105 and #106. #105 and #106 are OPEN. No `{{` slot is left in either comment file.

## Evidence run (targeted, debug build, macOS)

```
chunk-store file::tests:  test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 7 filtered out; finished in 6.76s
fsync_tests:              test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.74s
gc_online_tests:          test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 9.51s
memory_bound_tests:       test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 20.14s
write_log_replay_tests:   test result: ok. 18 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 7.73s
write_path_tests:         test result: ok. 24 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.10s
metadata --lib wal::      test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 130 filtered out; finished in 0.33s
```

All 16 test names cited in the #98 and #93 texts have a `... ok` line. A `--nocapture` run of memory_bound_tests on 75a9743 (after the write-log RAM merge) gave the same four peaks as 14-07: 8353, 9272, 7279, 7779 KiB. Gates: no home path, no licensing word, no internal plan ID in the four texts.

## Commits

| Task | Commit | What |
|------|--------|------|
| 1 | 12638e5 | the four draft texts |
| 1 | 2ced010 | drop an unsourced "before: 39 s"; plainer open-point sentence |
| 3 | 09d4220 | fill #105 and #106 in the posted #98 text |

## Deviations from Plan

1. **Wider evidence run.** The #98 text cites five write-log tests from 14-09, so `cargo test -p metadata --lib wal::` was appended to the same log. The plan's literal verify command (two cargo commands) reports those five names as MISSING without that line.
2. **Follow-up B has a new subject.** 14-09 bounds the flush-on-fsync and periodic RAM, so that part of D-11 is done. Replay is still unbounded (verified in code: `load_store_with_log` collects all ops after the last root; `mount.rs` passes the list whole to `oplog::replay`). The file name `14-followup-log-ram.md` stays.
3. **Texts adjusted to the facts:** relog test files are 4 and 16 MiB (not 8 and 32); the MAX_PAYLOAD extent is 65 MiB (not 70); "bound 12 MiB, dirty limit 1 MiB"; added the write-log RAM change, the crash-prefix behaviour, the 8 MiB `KEEP_LIMIT`, the 4 GiB size-change test and the open point on the 5-8 s slowest write.
4. **Slots PR, MERGE_SHA, CI_RUN_URL filled in Task 1** on launcher request; only ISSUE_A and ISSUE_B waited for Task 3.
5. **Docker numbers predate the per-op flush** (measured at 37de6ad; 4269b6a is not its ancestor). The flush writes into the kernel and holds no extra RAM; Docker was not run again. Reported to the user at the checkpoint; the public text does not mention it.
6. **Process.** Branch `docs/14-08-closeout` from origin/develop on launcher request (outside the `agent-*` list). Evidence log in the session scratchpad. STATE.md, ROADMAP.md and REQUIREMENTS.md were not changed (the coordinator owns them).

## Known Stubs

None.

## Self-Check: PASSED

- FOUND: the four text files and this SUMMARY
- FOUND commits (ancestors of HEAD): 12638e5, 2ced010, 09d4220
- #93 CLOSED, #98 CLOSED, #105 OPEN, #106 OPEN
