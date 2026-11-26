---
phase: 24-prolly-evaluation
plan: 05
subsystem: prolly evaluation
status: complete
tags: [prolly, benchmark, report, decision]
requires:
  - "24-01 and 24-02: crates/prolly (build, get, apply, walk)"
  - "24-03: benchmarks/prolly-bench"
  - "24-04: scripts/prolly-bench/bench.sh and README"
provides:
  - "runs/report-tables.py: report tables from the CSV, and --check (REPORT-OK)"
  - "runs/prolly-*: the full Boost run on macOS (3 runs, bench_data=boost)"
  - "runs/prolly-linux-*: the same run in Linux Docker"
  - "runs/prolly-quick-*: the quick pass; runs/gate-24-05.log: the phase gate"
  - "docs/research/prolly-trees.md: the report; recommendation later"
  - "docs/design/metadata-maps.md: the decision section"
  - "24-ISSUE-19-COMMENT.md: the #19 text (not posted)"
affects: []
tech-stack:
  added: []
  patterns:
    - "every report table is printed from committed CSV and checked with --check"
key-files:
  created:
    - .planning/phases/24-prolly-evaluation/runs/report-tables.py
    - .planning/phases/24-prolly-evaluation/runs/gate-24-05.log
    - .planning/phases/24-prolly-evaluation/runs/prolly-quick-results.csv
    - .planning/phases/24-prolly-evaluation/runs/prolly-quick-scale.csv
    - .planning/phases/24-prolly-evaluation/runs/prolly-quick-disk.csv
    - .planning/phases/24-prolly-evaluation/runs/prolly-quick-shape.txt
    - .planning/phases/24-prolly-evaluation/runs/prolly-quick-machine.txt
    - .planning/phases/24-prolly-evaluation/runs/prolly-results.csv
    - .planning/phases/24-prolly-evaluation/runs/prolly-scale.csv
    - .planning/phases/24-prolly-evaluation/runs/prolly-disk.csv
    - .planning/phases/24-prolly-evaluation/runs/prolly-edits.csv
    - .planning/phases/24-prolly-evaluation/runs/prolly-batches.csv
    - .planning/phases/24-prolly-evaluation/runs/prolly-shape.txt
    - .planning/phases/24-prolly-evaluation/runs/prolly-machine.txt
    - .planning/phases/24-prolly-evaluation/runs/prolly-linux-results.csv
    - .planning/phases/24-prolly-evaluation/runs/prolly-linux-scale.csv
    - .planning/phases/24-prolly-evaluation/runs/prolly-linux-disk.csv
    - .planning/phases/24-prolly-evaluation/runs/prolly-linux-shape.txt
    - .planning/phases/24-prolly-evaluation/runs/prolly-linux-machine.txt
    - docs/research/prolly-trees.md
    - docs/design/metadata-maps.md
    - .planning/phases/24-prolly-evaluation/24-ISSUE-19-COMMENT.md
  modified: []
decisions:
  - "Recommendation later (rubric of the plan): prolly is better than flat for one map, but the estimated whole-commit gain at the Boost scale is about 5 ms (below 10 ms)"
  - "Reading of the rubric: on macOS the wall time of a small commit is mostly one F_FULLFSYNC (about 20 ms, sometimes 40), so CPU time, serialized bytes and the scale rows carry the decision; a wall difference of one sync is noise"
  - "The design-doc file is a new docs/design/metadata-maps.md (D-26)"
  - "Table layout of report-tables.py: one column group per mode; extra CPU-estimate columns in the estimate table; peak memory is the median_low of the mode's median rows"
  - "A second full run in Linux Docker (the launch message asked for it), stem prolly-linux"
metrics:
  duration: "about 45 min"
  completed: 2026-11-26
requirements: [PROLLY-02, PROLLY-03]
actuals:
  tokens: 68000    # chars/4 over the realized diff (data included); about 14,300 for the script and the three docs alone
  tasks: 3
  commits: 4       # git rev-list --count 7bbe75e..1d821aa (this SUMMARY commit not counted)
plan_head_before: 7bbe75e
plan_head_after: 1d821aa
---

# Phase 24 Plan 05: Prolly benchmark run and decision Summary

The prolly benchmark ran on the Boost trees 1_84_0 and 1_85_0 (3 runs, in process, no mount) on macOS and once more in Linux Docker. The report `docs/research/prolly-trees.md` recommends **later**: a prolly `inode_data` map does about 10 times less CPU work and about 1,000 times fewer serialized bytes per small commit than the flat map, but at 84,104 inodes it saves only about 5 ms of a 55 to 63 ms commit. The gain becomes large at about a million inodes and more.

## Commits

| Task | Commit | Subject |
|------|--------|---------|
| 1 (tracer) | 50965cb | feat(24-05): phase gate, quick Boost pass and the report tables script |
| 2 | e16962c | feat(24-05): full prolly benchmark on Boost 1_84_0 and 1_85_0 (macOS) |
| 2 (extra) | ff56872 | feat(24-05): prolly benchmark on Boost in Linux Docker |
| 3 | 1d821aa | docs(24-05): prolly decision report, design section and the #19 text |

## Gates

- `RUST_STABLE=stable timeout 1800 scripts/safety/clippy.sh`: `CLIPPY os=Darwin toolchain=stable result=ok`, `UNSAFE files=32 tokens=134 result=ok`.
- `timeout 900 cargo test -p prolly -p prolly-bench`: exit 0; prolly 21 passed; prolly-bench smoke 2 passed.
- Workspace suite, once (`timeout 3600 cargo test --workspace` with the four `--skip` names): exit 0, 85 `test result` lines, 1,694 passed, 0 failed, 13 ignored (20:26:14Z to 20:32:08Z).
- All three ran on the merged tree `7bbe75e` before any benchmark. Log: `runs/gate-24-05.log`. After that, this plan changed only data and docs.

## Task 1: quick pass

- Command: `WORK="$M/results/prolly-quick-2026-11-26" BIN=target/release/prolly-bench RUNS=1 KS=1 COMMITS=1 PATTERNS=scatter SCALE_NS=100000 timeout 2400 bash scripts/prolly-bench/bench.sh "$M/data"`, after `timeout 1200 cargo build --release -p prolly-bench`.
- Wall time 76 s (20:33:10Z to 20:34:26Z). `seed secs=16.841 inode_count=84104`. `bench_data=boost`. Two `ok=1` checks (flat and prolly, 85,081 entries). The 1-minute load was 27.08 before (Spotlight indexing and jobs of other projects) and 19.04 after; the quick pass checks the tools only, and its numbers are not used in the report.
- Tracer gate: the three verify blocks of Task 1 passed (`tables=0`, 42 table lines, `REPORT-OK`, `empty_check=1`; ok=1 count 2; bench_data count 1; 0 home paths; `result=ok` count 3) before the Task 1 commit.

## Task 2: full run

- Quiet machine: at 20:35:32Z the 1-minute load was 13.67; the script waited 5 minutes; at 20:40:32Z it was 5.78 (`uptime`: `load averages: 5.78 11.19 15.18`), and the run started. No cargo, rustc or SliceFS job ran. Jobs of other projects used about 3 CPUs at the start and about 6 at the end (of 32).
- Command: `WORK="$M/results/prolly-2026-11-26" BIN=target/release/prolly-bench timeout 5400 bash scripts/prolly-bench/bench.sh "$M/data" 3` with the defaults (`BOOST_VERS="1_84_0 1_85_0"`, `KS="1 10 100 1000"`, `COMMITS=10`, `PATTERNS="scatter cluster"`, `SEED=24`, `SCALE_NS="100000 1000000 4000000"`). `M` is the measure dir outside the repo.
- Start 20:40:32Z, end 20:42:59Z, wall time 147 s. Load before 5.78, after 7.79 (`7.79 9.58 13.85`).
- Fallback: none. `runs=738 medians=30 modes=3 bad=0`; `bench_data=boost runs=3`; 6 `ok=1` checks; 0 home paths; 4 machine lines.
- Linux Docker run (extra, see deviations): `timeout 7200 docker run ... slicefs-fusetest` with `-v DATA:/data:ro`, `apt-get install -y time`, then `bench.sh /data 3` in the container. Start 20:44:32Z, end 20:47:09Z, VM load 2.40 before and 1.17 after (the host was not quiet). 738 run rows, 30 median rows, 6 `ok=1` checks, peak RSS measured (GNU time).

## Task 3: documents

- `report-tables.py --check docs/research/prolly-trees.md`: `REPORT-OK` for `--stem prolly` and for `--stem prolly-linux`.
- Section and fact greps: smallest count 1. `data=boost word=1 pending=0 waits=0`. `content-cut trees` in all three documents; the section line count 1; the link `prolly-trees.md` in the design doc and the #19 text. 0 home paths, host or user names or banned words. `git diff --quiet a882251 -- crates/metadata crates/slicefs-cli crates/chunk-store docs/design/store-format.md`: `diff_rc=0`.
- Posting the #19 text (`24-ISSUE-19-COMMENT.md`) is left to the orchestrator or the owner. Nothing was posted.

## Headline numbers (macOS, median; one changed file, scatter K = 1)

| | whole commit | flat `inode_data` | prolly `inode_data` |
|---|---|---|---|
| wall ms | 61.7 | 26.9 | 21.6 |
| CPU ms | 40.8 | 6.1 | 0.6 |
| serialized KB | 10,345.2 | 3,448.3 | 3.3 |
| new stored KB | 36.3 | 15.4 | 2.6 |
| process peak RSS MiB | 390.0 | 107.8 | 26.6 |
| disk growth over the script MiB | 160.1 | 31.2 | 17.5 |

- Estimated whole commit with a prolly `inode_data`: 56.4 ms (91.4 %), 35.2 CPU ms (86.5 %). Linux: 24.6 ms of 30.7 (80.1 %).
- Scale, one changed value at 4,000,000 entries: flat 229.9 ms (208.7 CPU ms), prolly 21.8 ms (0.8 CPU ms). One flat map passes 100 ms per commit at about 1.67 million entries (Linux: about 0.91 million).
- Prolly is slower than flat at scatter K = 1,000 (31.7 against 21.2 CPU ms) and at the version step (79.9 against 27.2 CPU ms).
- Tree: depth 3, 1,026 leaves of 549 to 6,804 bytes (mean 3,740); boundary hash 94.3 ns per key.

## Deviations from Plan

1. **[Launch message] A second full run in Linux Docker.** The launch message asked for it if time allows. A scratch copy of the `prolly-bench-smoke` case of `scripts/docker-linux-test/run.sh` (no edit of `scripts/`), with the Boost data as a read-only volume, GNU `time`, the container name `prolly-x2405` and the volume `slicefs-target-x2405` (removed after the run). Its five files `runs/prolly-linux-*` are not in `files_modified`. The report has its own subsection and its tables pass `--check`.
2. **[Discretion] Reading of the rubric.** On macOS the wall time minus the CPU time of a small commit is about 20 ms (one `F_FULLFSYNC`) and sometimes about 40 ms, in every mode. The report says this and lets CPU time, serialized bytes and the scale rows carry the decision. With this reading, scatter K = 100 (prolly 45.7 against flat 30.3 ms wall, but 5.8 against 9.3 CPU ms) counts as "better". The Linux run (sync about 2 ms) has prolly faster in wall time in all six rows for K up to 100. The word would be `later` in every reading that does not make one sync-noise row a `drop`.
3. **[Discretion] Extra columns.** The estimate table also has the CPU estimate (whole - flat + prolly) next to the wall estimate. The peak memory table has `ru_maxrss` after the last commit. The plan named the other columns.
4. **[Discretion] Scratch folder.** The launch message gave a fixed private scratch folder, so the plan's `mktemp -d` was not used. Every output went through the sed for the repo, home and scratch paths (and the host name).
5. **[Process] GSD state steps not done.** The launch message did not permit changes to `STATE.md` and `ROADMAP.md`. The plan-commit ledger file was not written; the base `7bbe75e` is known from the start step.

## Known Stubs

None.

## Threat Flags

None. No product code changed. No GitHub write (T-24-14). The committed data and documents have no home path, host or user name (T-24-13). Every report table is printed from committed CSV and checked (T-24-12).

## Open owner questions

Q-1 to Q-3 stay open. They are one-way doors, and this plan does not decide them:

- Q-1: adopt prolly maps (a store format change), and in which milestone? Recommendation: later, when stores of about a million inodes or more are a target; first remove the duplicate of `inode_map` and `inode_data`.
- Q-2: one prolly map per metadata map, or one combined namespace map? Suggestion: decide it with Q-1.
- Q-3: do the boundary rule and its constants become part of the frozen store format? Suggestion: yes if adopted, with a fixed-point table in place of the float test and a version byte for the rule.

## Self-Check: PASSED

- All 22 created files exist (`git diff --name-only 7bbe75e..1d821aa`).
- Commits 50965cb, e16962c, ff56872 and 1d821aa are on `exec/24-05`.
