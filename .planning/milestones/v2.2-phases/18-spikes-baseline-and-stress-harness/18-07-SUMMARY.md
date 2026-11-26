---
phase: 18-spikes-baseline-and-stress-harness
plan: 07
subsystem: docs
tags: [spike-results, research-doc, fuse-t, fskit, migration-steps]
requires: ["18-04", "18-05", "18-06"]
provides:
  - "docs/research/multithreaded-request-handling.md: ## Spike results (Phase 18) with ### S1 result to ### S7 result, each ending in a Decides line"
  - "SUPERSEDED marks on the old baselines (1124 / 1772 ms, inline pack fsync, 1.3 s gate hold, flush_wal, step 7 switch)"
affects: [19, 20, 21]
tech-stack:
  added: []
  patterns:
    - "Public doc numbers come only from saved run logs; derived values (ratios, margins) are named as derived"
key-files:
  created:
    - .planning/phases/18-spikes-baseline-and-stress-harness/18-07-SUMMARY.md
  modified:
    - docs/research/multithreaded-request-handling.md
key-decisions:
  - "S1 sentence: The macOS default can be more than one worker (case a: max_inflight 2, 5 inversions, 250 of 250 stats during the slow READ, no error)"
  - "S2: #108 does not need threads for the typical case (median slowest write 224 / 343 ms); the bound was missed in 1 of 6 runs (1353 ms at 4 GiB), so the doc says close #108 only after a btrfs repeat"
  - "S3: no support for default N above 2; readers lock is the one that grows with N; no evidence for per-worker packs"
  - "S5: keep std RwLock for the gate; rule 'release the gate before any wait' is required for steps 5 and 6"
  - "S6: group commit has higher priority on macOS (about 20 ms per fsync, buffered strategies do not help)"
  - "S7: baseline only; 22 MiB margin to the 256 MiB cap is the limit new buffers must fit in"
requirements-completed: [SPIKE-01, SPIKE-03]
duration: ~35 min
completed: 2026-11-21
status: complete
actuals:
  tokens: 11000
  tasks: 2
  commits: 2
plan_head_before: 399a8d13aafebed280534feff303bb7c1b1a4f35
plan_head_after: e8dba178e57bc9c4ec19a3aea44681e8d9911b77
commits: 2
---

# Phase 18 Plan 07: Spike results in the threading research doc Summary

The research doc now has a `## Spike results (Phase 18)` section with a result for each of S1 to S7, each ending in a `**Decides:** step N` line, plus 6 SUPERSEDED marks on baselines that no longer hold, and the sentence "The macOS default can be more than one worker."

## Tasks

| Task | Name | Commit |
|------|------|--------|
| 1 (tracer) | Results section with the S2 result, SUPERSEDED marks | c0de8c0 |
| 2 | S1 and S3-S7 results, macOS default sentence | e8dba17 |

## Decided steps

| Spike | Decides | Result in one line |
|-------|---------|--------------------|
| S1 | step 6 | macOS FSKit CI (FUSE-T 1.2.9, macOS 26.7.2): max_inflight 2, 5 reply inversions, 250 of 250 stats done during the slow READ; macOS default can be more than one worker, throughput is for Phase 21 |
| S2 | step 1 | Overlayfs, 3 runs per size: median slowest write 224 / 343 ms, max 244 / 1353 ms; bound of 500 ms held in 5 of 6 runs |
| S3 | step 6 | Writes flat past N = 2; `readers` wait p50 grows to about 9 ms at N = 8; `store` waits only in the tail |
| S4 | step 2 | Queue depth: write 1, readahead 2, both up to 7 (p50 3); no overlap of writes alone |
| S5 | step 7 | New-reader wait while a commit waits: 597.7 ms Linux, 508.3 ms macOS (longest hold 505.8 / 510.0) |
| S6 | step 7 | Linux buffered strategies faster; macOS in process about 20 ms per fsync for all strategies |
| S7 | step 5 | Peak RSS 234 of 256 MiB (serialized baseline); thresholds open until Phase 20 |

## Checks

- Greps on the doc: 1 `## Spike results (Phase 18)`, 7 `### S<n> result` (one each), 7 `**Decides:** step`, 1 `**Decides:** step 1`, 1 macOS default sentence, 1 `actions/runs/` link, 6 lines with SUPERSEDED; 0 `/Users/`; 0 for the terms-of-use words.
- Number trace (T-18-24): a script extracted every number in the results section and looked it up in the saved run logs. All numbers appear in a log except derived values: 250 (5 rounds of 50 stats), the ratios 1.36, 1.4 and about 9% (from the S3 throughput table), 91.9 ms (597.7 minus 505.8), 22 MiB (256 minus 234), and the old 1.3 s and the issue numbers #131 and #133 (from the existing doc text and the plan). S2 values were also checked by hand against `s2-bigwrite-linux.log`.
- The doc still has one pre-existing link to a `.planning/debug` file at commit 6fd1e11 (in the "I/O path" part). It was there before this plan; this plan added no `.planning` link.

## Deviations from Plan

**1. [Rule 2 - consistency] Extra SUPERSEDED marks.** The plan lists 5 marks. The same stale claims appear elsewhere, so the mark was also added to the intro paragraph (inline pack fsync), the "Today a SliceFS commit holds the gate ... 1.3 s" sentence, the step 1 table row, and the "more threads move that tail around" sentence. The old text was kept.

**2. [Process] One commit was amended.** A first Task 2 commit went in with a missing edit batch (a failed assertion in my edit script stopped the write, and the commit ran anyway). I applied the batch and amended my own unpushed commit (c0de8c0 stays; the Task 2 commit is e8dba17). No other commit was changed.

**3. Added one sentence to the Conclusion** saying Phase 18 settled the overlap part of the macOS question and the throughput gain is still open (the old text said S1 "can settle" it).

## Known Stubs

None.

## Threat Flags

None. Only a doc changed; no new endpoint, auth path or schema.

## Notes for later phases

- S3 must run again after the `readers` lock is narrowed (step 1); the numbers here are a baseline of today's lock layout.
- S2 needs a repeat on btrfs before #108 closes (the 1353 ms 4 GiB run).
- macOS logs for S3, S5 and S6 do not name the store file system type; the doc says so.
- Commit 0b2d5a5 noted in Plan 05 is the orchestrator's own LOCK-05 requirements commit.

## Self-Check: PASSED

- FOUND: docs/research/multithreaded-request-handling.md (modified), this SUMMARY
- FOUND commits: c0de8c0, e8dba17 (`git merge-base --is-ancestor` checked after the write)
