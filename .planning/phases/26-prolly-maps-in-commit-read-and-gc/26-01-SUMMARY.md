---
phase: 26-prolly-maps-in-commit-read-and-gc
plan: 01
subsystem: bench
tags: [pmap07, prolly, commit-bench, docker]
requires: []
provides:
  - "pre-registered PMAP-07 rule and decision script"
  - "commit_bench.rs one_change_commit_at_scale for this tree and v2.3.0"
  - "early flat baseline of v2.3.0 at 100,000 and 1,000,000 files"
affects: [26-13, 26-14]
tech-stack:
  added: []
  patterns: ["rule and script committed before any measurement", "one bench file, per-tree report module"]
key-files:
  created:
    - .planning/phases/26-prolly-maps-in-commit-read-and-gc/runs/pmap07-rule.md
    - .planning/phases/26-prolly-maps-in-commit-read-and-gc/runs/pmap07-early-flat.log
    - .planning/phases/26-prolly-maps-in-commit-read-and-gc/runs/gate-26-01.log
    - scripts/prolly-bench/pmap07.py
    - scripts/prolly-bench/pmap07.sh
    - scripts/prolly-bench/pmap07/report_flat.rs
    - crates/metadata/tests/support/pmap07_report.rs
  modified:
    - crates/metadata/tests/commit_bench.rs
    - docs/design/unsafe-review.md
key-decisions:
  - "Decision platform: Linux in Docker on the disk-backed volume slicefs-pmap07-work; thread CPU by clock_gettime(CLOCK_THREAD_CPUTIME_ID) around commit()"
  - "The CPU of the sync calls stays inside cpu_ms in both builds; this biases against prolly, so a pass is conservative"
  - "cpu_ms covers the calling thread only (not helper threads)"
status: complete
metrics:
  duration: "about 1 h 30 min"
  completed: 2026-11-27
actuals:
  tokens: 11000
  tasks: 3
  commits: 5
plan_head_before: 386212da2ebbdc08764194e730a072992e88eafc
plan_head_after: 497e9ee33dc9e3de4ee8c29bf4c3cf1621719a69
---

# Phase 26 Plan 01: PMAP-07 rule, commit bench and early flat baseline Summary

The PMAP-07 rule (checks A, B, C) and its decision script are committed before any measurement, one bench file measures one-change commits in this tree and in tag `v2.3.0`, and the early flat baseline of `v2.3.0` at 1,000,000 files is 533 ms wall and 510 ms thread CPU per commit.

`commits: 5` counts the five commits up to the gate log; the SUMMARY commit that follows is not in the count.

## What was built

- `runs/pmap07-rule.md` and `scripts/prolly-bench/pmap07.py` (checks A: CPU and nodes <= 0.20 of flat; B: wall lower; C: nine Boost rows better and bytes <= 2 x; `--self-test`, `--summary`, exit 2 on bad input). Commit 8c6d271.
- `crates/metadata/tests/commit_bench.rs`: the old test is unchanged; new ignored test `one_change_commit_at_scale` (env `PMAP07_N`, `PMAP07_SAMPLES`, `PMAP07_LABEL`; lines `seed`, `sample`, `summary`). Only API that exists in this tree and in `v2.3.0` (a diff against `v2.3.0` shows 91 added lines and nothing removed). Report modules: `support/pmap07_report.rs` (this tree, returns `NA` until plan 26-14) and `scripts/prolly-bench/pmap07/report_flat.rs` (`v2.3.0`: nodes written = every object of the six map values and the root record). Commit 836ceb8.
- `scripts/prolly-bench/pmap07.sh`: `prepare-v230` and `commit` (Docker, volume `slicefs-pmap07-work`, refuses while another `pmap07-*` container runs, exit 2 without arguments).
- `docs/design/unsafe-review.md`: entry and count line for the one `clock_gettime` call.

## Early flat baseline (feasibility check, decides nothing)

`v2.3.0` (582a41d), release build, Linux Docker on disk-backed storage, 11 one-change commits after the first commit:

| files | seed | first commit | median wall (ms) | median thread CPU (ms) | nodes written |
|------:|-----:|-------------:|-----------------:|-----------------------:|--------------:|
| 100,000 | 2.2 s | 407 ms | 49.3 | 45.4 | 1,368 |
| 1,000,000 | 95.9 s | 1,956 ms | 533.2 | 510.0 | 14,181 |

Answer to A5 (is the 1/5 target reachable at 1,000,000 inodes): the flat commit scales linearly with the file count (about 11 times the wall and CPU for 10 times the files), and it is CPU-bound, not sync-bound (wall is within 5 % of CPU). So check A needs prolly at 1,000,000 files at or below about 102 ms of thread CPU (0.20 x 510) and at or below about 2,836 nodes (0.20 x 14,181); a prolly commit of one change should touch a few nodes per tree, so both look reachable on paper. Check B needs prolly below 533 ms wall, which leaves a wide margin even if sync is large. `cpu_ms` covers the calling thread only, so any CPU in helper threads is not in it; the rule file is unchanged by this reading. Plan 26-13 decides.

## Decisions

- Platform, thread-CPU method and the sync-in-CPU bias are as in the frontmatter (discretion of the plan).
- The `v2.3.0` worktree was made in a scratch folder (not in the main checkout) as the dispatch message required, and removed at the end; the plan text named `.claude/worktrees/v230-baseline` of the main repository. Plan 26-13 should make its own with `pmap07.sh prepare-v230`.
- Cargo target volume: `slicefs-target-26-01` for all three Docker runs (smoke and both baseline runs), so `v2.3.0` built once.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 2 - Missing critical functionality] A non-number in a recorded-only Boost row stopped the verdict**
- **Found during:** Task 2 review, before any Docker run
- **Issue:** `info_rows` in `pmap07.py` called the number parser on rows that no check gates (for example the version step with NA columns), giving exit 2 and no verdict.
- **Fix:** such a row prints `info pattern=<p> k=<k> unreadable`; gated rows still refuse with exit 2. A self-test case was added.
- **Files modified:** `scripts/prolly-bench/pmap07.py`
- **Commit:** 5cab585 (before the first measurement, so pre-registration holds; the rule file and limits are unchanged)

**2. [Rule 3 - Blocking] Worktree location**
- **Issue:** the plan puts the `v2.3.0` worktree in the main checkout; the dispatch forbids touching it.
- **Fix:** scratch path, removed after the runs.

All edited files are in `files_modified`.

## Verification

- `python3 scripts/prolly-bench/pmap07.py --self-test`: ok (also checked by two mutations of the script, each made the self-test fail).
- Harness smoke on the macOS host (`PMAP07_N=2000`): 11 sample lines with `NA` nodes. Docker smoke on `v2.3.0` (`flat-smoke`, n=2000): 11 samples with `nodes_read=0`, `nodes_written=35`. Neither log is committed.
- Gates (`runs/gate-26-01.log`): `cargo test -p metadata`, `unsafe-count.sh`, `pmap07.py --self-test`, `cargo test --workspace`: all rc=0; `rule_commit_is_ancestor=1`. The `retry_tests` timing check did not fail.
- Lock-stats test not run: no lock changed.

## Known Stubs

None. `pmap07_report.rs` returns `NA` on purpose until plan 26-14 fills it from the commit counters.

## Threat Flags

None.

## Self-Check: PASSED

Files found: the rule, the script, the runner, both report modules, the bench file, both logs. Commits 8c6d271, 5cab585, 836ceb8, 52fc626, 497e9ee are ancestors of HEAD.
