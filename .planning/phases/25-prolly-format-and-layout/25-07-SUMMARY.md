---
phase: 25-prolly-format-and-layout
plan: 07
subsystem: prolly-bench
tags: [layout, constants, decision, docker, pre-registration]
requires: [25-06]
provides:
  - "runs/layout/: the CSVs of the full 27-row matrix, the unedited decide.py output, the determinism re-run"
  - "Decision: layout a (five trees, each map key with its own prolly root), L=8192, NODE_MAX=16384"
affects: [25-08]
requirements-completed: [PMAP-01, PMAP-04]
status: complete
commits: 4
plan_head_before: 00423635d93ddf318a84338c529bcec5d1051d27
plan_head_after: 14948cddb9e2d584467bc479eb6e0a5a6c044cd1
actuals:
  tokens: 14000
  tasks: 3
  commits: 4
tech-stack:
  added: []
  patterns: ["pre-registration commit before the data", "one row process per row on a disk-backed Docker volume"]
key-files:
  created:
    - .planning/phases/25-prolly-format-and-layout/runs/layout/prereg.txt
    - .planning/phases/25-prolly-format-and-layout/runs/layout/layout.csv
    - .planning/phases/25-prolly-format-and-layout/runs/layout/shape.csv
    - .planning/phases/25-prolly-format-and-layout/runs/layout/build.csv
    - .planning/phases/25-prolly-format-and-layout/runs/layout/checks.txt
    - .planning/phases/25-prolly-format-and-layout/runs/layout/machine.txt
    - .planning/phases/25-prolly-format-and-layout/runs/layout/decision.txt
    - .planning/phases/25-prolly-format-and-layout/runs/layout/determinism.txt
    - .planning/phases/25-prolly-format-and-layout/runs/layout/run.log
  modified: []
key-decisions:
  - "Layout a: neither b nor c qualified, because both have more nodes at 1B than a (rule 4 of the layout rule)"
  - "Constants: L=8192, NODE_MAX=16384; 8192/32768 also qualified and has the same 1B node count; the smaller NODE_MAX wins by the rule"
---

# Phase 25 Plan 07: Layout and constants run and decision

The full matrix ran in Docker (27 rows, 27 `ok=1`, no retry). The pre-registered rules in `decide.py` (unchanged since commit `75c15ed`) give `verdict layout=a l=8192 node_max=16384`. The deterministic columns reproduce (`same=1`, 50 of 50 commits).

## What was done

- **Task 1.** `prereg.txt` was committed before any row (`decide_py_commit=75c15edd...`, `bench_commit=0042363...`). The pilot (Boost row and synthetic 1M row, layout a, Dolt config) ran through `run.sh prolly-layout` in 171 s with both checks `ok=1`. A 1M row costs about 15 s of commits, so the full matrix was estimated at under 1 hour (the plan limit was 12 h).
- **Task 2.** The full default matrix ran in 28 minutes (07:01:46Z to 07:29:26Z). The two pilot rows were reused by RESUME; 25 rows ran new. Every `.err` file is empty. `checks.txt` has 27 `ok=1` and 0 `ok=0`. `layout.csv` has 1449 rows, `shape.csv` 144, `build.csv` 27. Stores lived on the volume `slicefs-prolly-work`, now removed.
- **Task 3.** `git diff --quiet 75c15ed -- scripts/prolly-bench/decide.py` gives `unchanged=0`. `decision.txt` is the raw output of `decide.py` (exit 0). The determinism re-run used an empty volume (RESUME=0): synthetic 1M, layout a, 8192/16384.

## Owner report

**Verdict (last line of `decision.txt`): `verdict layout=a l=8192 node_max=16384`.**

### Layout: a (five maps, five trees)

The rule was decided at the Dolt config 4096/16384. Neither b nor c qualified, and the only failing rule is the 1B node count. All other rules passed with margin.

| Rule (threshold) | Layout b (one namespace tree, refcounts inside) | Layout c (namespace tree + refcounts tree) |
| --- | --- | --- |
| mean nodes ratio over the 6 small rows (<= 0.80) | 0.681 pass | 0.700 pass |
| mean CPU ratio over the 6 small rows (<= 1.25) | 0.825 pass | 0.948 pass |
| largest bytes ratio over the 6 large rows (<= 1.10) | 0.912 pass | 0.906 pass |
| 1B node count not larger than a (a = 47,291,816) | 48,922,745 (+3.4%) FAIL | 48,428,987 (+2.4%) FAIL |

b and c touch about 30% fewer nodes per small commit and write about 9% fewer bytes on large commits, but at 1B entries the combined key space needs 2.4% to 3.4% more nodes than five separate trees. The rule says "not larger", so the choice is a. The verdict is as computed.

CPU did not decide anything. The CPU ratios of layout c are noisy (for example synthetic 1M `one`: a 3.2 ms, b 2.8 ms, c 4.4 ms; `mixed` k=100: a 79 ms, b 59 ms, c 108 ms), and the host load average was above 100 from other host jobs during the runs (the Docker VM showed a load near 2 to 3).

**Boost refcounts are empty** (the product writes no refcounts today). In the Boost rows the refcount tree of layouts a and c is empty and layout b has no `0x02` keys. The refcount signal and the b-versus-c comparison therefore come from the synthetic rows only (2 refcount keys per file). `decision.txt` carries this as a `note` line before the verdict.

### Constants: L=8192, NODE_MAX=16384 (Dolt 4096/16384 replaced)

All three configs have F(C) (forced cuts / nodes of the 10M base build) below 0.005: Dolt 0.00000, 8192/16384 0.00011, 8192/32768 0.00000.

| Config | N1B | N1B / N1B(D) (<= 0.65) | W bytes (mean of 3 rows) | W / W(D) (<= 2.5) | T ms | T / T(D) (<= 2.0) |
| --- | --- | --- | --- | --- | --- | --- |
| 4096/16384 (Dolt D) | 47,291,816 | 1 | 1.27 MB | 1 | 41.5 | 1 |
| 8192/16384 | 23,479,915 | 0.4965 pass | 2.14 MB | 1.683 pass | 68.0 | 1.639 pass |
| 8192/32768 | 23,479,815 | 0.4965 pass | about the same | 1.683 pass | about the same | 1.610 pass |

Both 8192 configs qualify. Their N1B differs by 100 nodes (the 16384 cap forces a few cuts: 0.011% of nodes), so the tie goes to the smaller NODE_MAX: 16384. The effect of the move from 4096 to 8192: about half the nodes at 1B, in return for about 68% more bytes written and 64% more CPU on a small commit (1.27 MB to 2.14 MB written; 41.5 ms to 68.0 ms).

### 1B extrapolation of the chosen config (layout a, 8192/16384, from the 10M base build)

| Map | Entries at 1B | Depth | Levels (leaves to root) | Nodes |
| --- | --- | --- | --- | --- |
| inode | 1,000,000,000 | 5 | 6,048,100 / 40,600 / 273 / 2 / 1 | 6,088,976 |
| dir | 100,036,700 | 4 | 604,900 / 4,400 / 33 / 1 | 609,334 |
| manifest | 899,963,300 | 5 | 5,448,000 / 37,600 / 260 / 2 / 1 | 5,485,863 |
| xattr | 49,818,700 | 4 | 302,300 / 2,100 / 15 / 1 | 304,416 |
| refcounts | 1,799,926,600 | 5 | 10,880,200 / 110,000 / 1,113 / 12 / 1 | 10,991,326 |
| total | | | | 23,479,915 |

Dolt config for comparison: 47,291,816 nodes, depths 5, 5, 5, 5, 6.

Nodes touched by one change at 10M files (median over 10 commits, nodes read plus written): layout a, Dolt 63; layout a, 8192/16384 52 (b 45.5, c 48 at the Dolt config). At 1M files: 51 at the Dolt config.

### Effect on the root record and the GC mark

- Root record: layout a keeps the 197-byte record of plan 25-02; each of the five map keys points to a prolly root. (Layout b would have been 65 bytes, layout c 98 bytes.)
- GC mark: five tree walks, one per map; no kind dispatch on a combined key space.

### Doubts about a rule, as owner questions (the verdict stays)

1. The 1B node-count rule decided the layout by 2.4% (c) and 3.4% (b), against a roughly 30% gain in nodes touched per small commit. Do you want the rule to allow a small tolerance (for example 5%) for a future re-decision? It would have chosen b or c here. This is a question for later; this run uses the rule as written, and the pre-registered result is a.
2. The 8192 configs double the bytes and CPU of a small commit (1.68x and 1.64x) to halve the node count at 1B. The rules allow this (limits 2.5 and 2.0), and the absolute values stay small (2 MB and 68 ms). If a small commit should stay close to 1.3 MB, the Dolt config is the choice; that changes the rule, not the data.

## Determinism

`determinism.txt`: 50 of 50 commits of the synthetic 1M row (layout a, 8192/16384) equal between the full run and the re-run, on edits, nodes_read, nodes_written, bytes_written and forced_cuts. Last line `same=1`.

## Deviations from Plan

1. **[Environment] `machine.txt` records `commit=unknown`.** The container sees the repo as a read-only copy without git metadata. The bench commit is in `prereg.txt` (`bench_commit=0042363...`).
2. **[Scope] The determinism re-run compared the synthetic 1M row only** (the Boost steps ran as part of the case, as the plan accepts, and their row passed its check, but is not compared).
3. **[Bookkeeping] `plan_head_before`** is the phase-branch commit `0042363`, since the plan commit ledger file was not written in this worktree run; `commits: 4` is the measured `git rev-list --count 0042363..HEAD` before the SUMMARY commit.
4. **[Trailer] Commit trailer** `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` per the owner rule and the plan, not the one in the session reminder.
5. **Host load.** The host showed load averages of 75 to 126 from other jobs during all runs; the VM load stayed below 4. CPU numbers were not re-run to improve them (they only guard the rules).

No auth gates. No row failed twice (no row failed). No file under `crates/` or `scripts/` changed.

## Known Stubs

None.

## Threat Flags

None. T-25-23: `prereg.txt` committed first; `decide.py` unchanged since it. T-25-24: 27 `ok=1`, 0 `ok=0`, decide.py exit 0. T-25-25: the verdict rests on node counts and bytes; CPU only guards, and no CPU ratio was near its limit for the layout rule (the layout failed on nodes only); the constants CPU ratio is 1.64 against 2.0.

## For plan 25-08

Pin: layout a, L=8192, NODE_MAX=16384; record the 1B table above and the rule values in `docs/design/store-format.md`.

## Self-Check: PASSED

- Files exist: all nine files under `runs/layout/`, this file.
- Commits `0e4036f`, `810e044`, `918eb2c`, `14948cd` are ancestors of HEAD on `exec/25-07`.
- `checks.txt`: 27 `ok=1`, 0 `ok=0`; `decision.txt` last line `verdict layout=a l=8192 node_max=16384`; `determinism.txt` last line `same=1`; `git diff --quiet 75c15ed -- scripts/prolly-bench/decide.py` exits 0; greps for user or home paths, `/var/folders` and license text give 0 in all committed result files.
