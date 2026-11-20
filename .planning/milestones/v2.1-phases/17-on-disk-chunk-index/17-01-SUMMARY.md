---
phase: 17-on-disk-chunk-index
plan: 01
subsystem: chunk-store index
tags: [spike, index, lsm, fjall, benchmark, decision]
status: complete

requires: [16-02]
provides:
  - .planning/phases/17-on-disk-chunk-index/17-SPIKE.md (package check, method, results, fjall questions, recommendation, decision)
  - local branch spike/17-index at 8a1f656 (spike-runs and spike-fjall backends; not merged, not pushed)
affects: [17-02, 17-03, 17-04, 17-05, 17-06, 17-07, 17-08]

actuals:
  tokens: 12000
  tasks: 3
  commits: 3
plan_head_before: 6296eb692a5caf41b66fc19c4922a840dd331c29
plan_head_after: 99df4006c79bd997d1d892407f9aaad8c1fd9093

tech-stack:
  added: []
  patterns:
    - index-only spike backends behind the index-bench Backend trait
    - lookup rounds that alternate backends, plus same-session reference lookups, to handle host load

key-files:
  created:
    - .planning/phases/17-on-disk-chunk-index/17-SPIKE.md
  modified: []

key-decisions:
  - "Phase 17 builds the custom sorted runs (user decision at the 17-01 gate): fjall cannot be opened read-only by a second process while a writer holds it"
  - "fjall stays only on the local branch spike/17-index; the phase branch has no fjall dependency"

duration: about 45 min (plus the decision wait)
completed: 2026-11-18
---

# Phase 17 Plan 01: index spike Summary

**The custom run layout reached 1.37 B/object of steady RSS and a 9 ms open at 10M objects. fjall 3.1.13 reached 2.12 B/object and a 0.65 s open, and refuses a second-process open (`Locked`). The user chose the custom sorted runs.**

## Task Commits

1. **Task 1: Package check for fjall.** The user approved fjall for the spike only ("try both"). The record is in 17-SPIKE.md, with no separate commit.
2. **Task 2: Spike.** Spike code `8a1f656` on the local branch `spike/17-index`. On the phase branch: `af8eb32` (results, fjall questions, recommendation) and `32298a7` (read-only search and fjall open caveat).
3. **Task 3: Decision.** `99df400`: "Decision: custom" with the user's reason.

## Key results (macOS, warm cache, load average 76-93)

| Backend | Objects | Steady RSS B/object | Open s | Hit ns | Miss ns | Index-only puts/s | Disk B/object |
|---|---|---|---|---|---|---|---|
| spike-runs | 1M | 1.454 | 0.001 | 5464 | 210 | 339845 | 59.9 |
| spike-runs | 10M | 1.371 | 0.009 | 5856 | 436 | 203012 | 59.9 |
| spike-fjall | 1M | 2.933 | 0.026 | 14342 | 798 | 40656 | 65.6 |
| spike-fjall | 10M | 2.120 | 0.648 | 19304 | 1003 | 31522 | 67.0 |

- Fixed rule: of the fjall conditions, only "at most 4 B/object" held. The read-only open, the 1.5x RSS, the open time and the hit time failed. The miss time failed, but within the noise.
- A spike-runs hit is 8 to 11 times slower than the chunk-store HashMap in the same session, because it reads one page per hit. A miss costs about the same as the HashMap.

## Verification

- `^## Results` 1, spike rows 8, `^## fjall questions` 1, `^## Recommendation` 1, home paths 0, `^Decision: custom` 1.
- `fjall` is absent from Cargo.toml, Cargo.lock and benchmarks/index-bench/Cargo.toml on the phase branch. `git diff origin/develop...HEAD` shows only the 17-01 planning files.
- `spike/17-index` exists locally, and `git ls-remote --heads origin spike/17-index` exits 2.

## Deviations from Plan

1. **[Rule 3 - Blocking] Spike branch in the same worktree.** The isolation guard of the agent refuses git commands on other worktrees. So `spike/17-index` was made and committed in this worktree, and the agent then switched back. There was no `git worktree remove`.
2. **[Rule 3] `main.rs` got two `mod` lines** on the spike branch, to register the new modules.
3. **[Rule 1 - Measurement] The fjall close does more than `persist`.** It flushes the memtable, runs a blocking major compaction, waits for idle, and then persists. Without this, each open would measure a journal replay, and unfinished compaction would run inside the next measured step.
4. **[Measurement] Extra data.** Lookup peak RSS from `/usr/bin/time -l`, three lookup rounds with the backends taking turns, and same-session chunk-store lookups, to handle the host load.
5. **[Environment] Scratch directory.** The stores are in a scratch directory outside the repo, not at the path that step 6 names.
6. **[Safety] Upstream removed.** The phase branch no longer tracks `origin/develop`, so a plain push cannot go to develop.

## Known Limits

- The cause of the 118x growth of the fjall open CPU time from 1M to 10M was not isolated. The likely cause is a 10.4 MB journal that stays after close.
- The spike-runs close merges all runs into one. The real design keeps up to about 10 + log4(N) runs, so its open time and RSS will be a little higher.
- No cold-cache numbers.

## Next Phase Readiness

Plan 17-02 (run file format and its PackIo I/O path) can start on the custom path as written.

## Self-Check: PASSED

- FOUND: .planning/phases/17-on-disk-chunk-index/17-SPIKE.md
- FOUND commits: af8eb32, 32298a7, 99df400 (ancestors of HEAD); 8a1f656 on spike/17-index
