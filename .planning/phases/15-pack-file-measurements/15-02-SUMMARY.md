---
phase: 15-pack-file-measurements
plan: 02
subsystem: design docs, measurements
status: complete
tags: [seed, packs, measurement, chunk-store, macos]
requires: [15-01]
provides:
  - "docs/design/chunk-store.md '### Results (2026-11-18)' under '## Measurements to take'"
  - "$M/results/2026-11-18/seed.csv, machine.txt, load.txt (outside repo)"
affects: [15-03]
tech-stack:
  added: []
  patterns: [one background job, builds one after the other, tables made by a python one-off from seed.csv]
key-files:
  created: []
  modified:
    - docs/design/chunk-store.md
requirements-completed: [PACK-02, PACK-03]
key-decisions:
  - "Measured under host load from other projects (1-minute load 52 to 137 on 32 logical cores); times are an indication only, the doc says so; sizes and file counts are not affected"
  - "Store files after a seed of the Boost tree: c2f82fa 426,739, 7016bb6 170,376, c447551 10 (target met)"
metrics:
  duration: ~2 h 40 min (measure job 8,948 s)
  completed: 2026-11-18
actuals:
  tokens: 2600
  tasks: 2
  commits: 1
plan_head_before: 0b3bd4697962490c9eadd82337d2434822dd5db8
plan_head_after: 68b4277
---

# Phase 15 Plan 02: seed measurements in the chunk-store design note Summary

The three builds (c2f82fa before packs, 7016bb6 before #56, c447551 develop) were measured with `scripts/pack-measure/seed-measure.sh` (3 runs per input, median) in one background job, one build after the other. The medians, the machine and the commits are now in `docs/design/chunk-store.md` as `### Results (2026-11-18)`. After a seed of the Boost tree the store has 426,739 files (c2f82fa), 170,376 files (7016bb6) and 10 files (c447551).

## Tasks

| Task | Name | Commit | Verify |
|------|------|--------|--------|
| 1 | Measure the three builds one after the other | none (no repo file changes) | MEASURE-OK |
| 2 | Write the Results subsection | 68b4277 | DOC-OK |

Results subsection heading as written: `### Results (2026-11-18)`

Total run time of the background job: 8,948 s (09:27:01 to 11:56:08 UTC). c2f82fa took 2 h 11 min, 7016bb6 15 min, c447551 3 min.

## Store file count after a seed of the Boost tree (PACK-03)

| Build | Files | Split |
|-------|-------|-------|
| c2f82fa | 426,739 | vt0/ 426,738, segments/ 1 |
| 7016bb6 | 170,376 | vt0/ 170,366, packs/ 8, segments/ 1, other 1 |
| c447551 | 10 | packs/ 8, segments/ 1, other 1 |

Allocated / apparent on the Boost tree: c2f82fa 2.41, 7016bb6 3.67, c447551 1.00.

## Timing spread (max / min secs over the 3 runs)

| Build | tree | tar | rand |
|-------|------|-----|------|
| c2f82fa | 1.13 | 1.29 | 1.08 |
| 7016bb6 | 1.08 | 1.27 | 1.08 |
| c447551 | 1.32 | 1.02 | 1.04 |

## load.txt (load average per build)

```
c2f82fa before 09:27:01 { 64.27 67.80 76.78 }
c2f82fa after 11:38:01 { 100.33 98.48 99.97 }
7016bb6 before 11:38:01 { 100.33 98.48 99.97 }
7016bb6 after 11:53:20 { 137.39 120.42 111.10 }
c447551 before 11:53:20 { 137.39 120.42 111.10 }
c447551 after 11:56:08 { 56.71 99.04 104.62 }
total_secs=8948
```

## machine.txt

```
cpu=Intel(R) Xeon(R) W-3245 CPU @ 3.20GHz
cores=16
ram_bytes=206158430208
macos=26.2
fs=APFS
ssd=Yes
load_before={ 65.08 68.02 76.91 }
load_after={ 51.99 95.92 103.41 }
```

## seed.csv

```
build,input,run,in_bytes,in_files,secs,mb_s,apparent_bytes,alloc_bytes,files,vt0_files,packs_files,segments_files,other_files,peak_rss_bytes
c2f82fa,tree,1,757529043,77655,634.64,1.2,910899310,2194235392,426740,426739,0,1,0,257257472
c2f82fa,tree,2,757529043,77655,633.47,1.2,910884021,2194206720,426737,426736,0,1,0,263737344
c2f82fa,tree,3,757529043,77655,718.22,1.1,910895382,2194227200,426739,426738,0,1,0,251793408
c2f82fa,tree,median,757529043,77655,634.64,1.2,910895382,2194227200,426739,426738,0,1,0,257257472
c2f82fa,tar,1,823613440,1,512.46,1.6,925472004,1381081088,217099,217098,0,1,0,888897536
c2f82fa,tar,2,823613440,1,617.04,1.3,925471993,1381081088,217099,217098,0,1,0,889098240
c2f82fa,tar,3,823613440,1,658.97,1.2,925472004,1381081088,217099,217098,0,1,0,889167872
c2f82fa,tar,median,823613440,1,617.04,1.3,925472004,1381081088,217099,217098,0,1,0,889098240
c2f82fa,rand,1,1073741824,1,854.61,1.3,1670511327,2514341888,318112,318111,0,1,0,1150091264
c2f82fa,rand,2,1073741824,1,796.41,1.3,1670511338,2514341888,318112,318111,0,1,0,1150070784
c2f82fa,rand,3,1073741824,1,859.60,1.2,1670511338,2514341888,318112,318111,0,1,0,1150062592
c2f82fa,rand,median,1073741824,1,854.61,1.3,1670511338,2514341888,318112,318111,0,1,0,1150070784
7016bb6,tree,1,757529043,77655,189.81,4.0,255300736,936296448,170376,170366,8,1,1,404279296
7016bb6,tree,2,757529043,77655,176.24,4.3,255294081,935936000,170375,170365,8,1,1,364785664
7016bb6,tree,3,757529043,77655,189.32,4.0,255298721,936792064,170376,170366,8,1,1,365830144
7016bb6,tree,median,757529043,77655,189.32,4.0,255298721,936296448,170376,170366,8,1,1,365830144
7016bb6,tar,1,823613440,1,12.28,67.1,199172179,201236480,21,13,6,1,1,1810149376
7016bb6,tar,2,823613440,1,12.74,64.6,199172168,200708096,21,13,6,1,1,1818025984
7016bb6,tar,3,823613440,1,10.04,82.0,199172157,200048640,21,13,6,1,1,1819668480
7016bb6,tar,median,823613440,1,12.28,67.1,199172168,200708096,21,13,6,1,1,1818025984
7016bb6,rand,1,1073741824,1,14.12,76.0,1085198972,1085333504,49,13,34,1,1,2429386752
7016bb6,rand,2,1073741824,1,15.30,70.2,1085198950,1085333504,49,13,34,1,1,2387296256
7016bb6,rand,3,1073741824,1,14.17,75.8,1085198983,1085333504,49,13,34,1,1,2409459712
7016bb6,rand,median,1073741824,1,14.17,75.8,1085198972,1085333504,49,13,34,1,1,2409459712
c447551,tree,1,757529043,77655,33.39,22.7,288609891,289951744,10,0,8,1,1,533204992
c447551,tree,2,757529043,77655,26.74,28.3,288609891,289357824,10,0,8,1,1,538972160
c447551,tree,3,757529043,77655,25.36,29.9,288609891,291094528,10,0,8,1,1,518086656
c447551,tree,median,757529043,77655,26.74,28.3,288609891,289951744,10,0,8,1,1,533204992
c447551,tar,1,823613440,1,9.36,88.0,200051171,218525696,8,0,6,1,1,1806225408
c447551,tar,2,823613440,1,9.16,89.9,200051171,218120192,8,0,6,1,1,1811079168
c447551,tar,3,823613440,1,9.22,89.3,200051171,218243072,8,0,6,1,1,1810841600
c447551,tar,median,823613440,1,9.22,89.3,200051171,218243072,8,0,6,1,1,1810841600
c447551,rand,1,1073741824,1,10.49,102.4,1086605890,1086681088,36,0,34,1,1,2411462656
c447551,rand,2,1073741824,1,10.41,103.1,1086605890,1086681088,36,0,34,1,1,2404618240
c447551,rand,3,1073741824,1,10.81,99.3,1086605890,1086681088,36,0,34,1,1,2414243840
c447551,rand,median,1073741824,1,10.49,102.4,1086605890,1086681088,36,0,34,1,1,2411462656
```

## Verify results

- MEASURE-OK: 36 data rows, 9 median rows, 15 fields each; the tree median rows show vt0-only (c2f82fa), vt0 + packs (7016bb6), packs-only (c447551); input sizes as expected.
- Task 1 acceptance: labels are exactly c2f82fa, 7016bb6, c447551; `grep -ciE 'host|user|/Users/' machine.txt` = 0; no `store-*` dir is left in the work dir; `git status --porcelain` was empty.
- DOC-OK: one Results heading, 3 tables, the three commits, the CPU string, APFS, `seed-measure.sh`, `boost-test.sh`; no home path, host name, user name or licensing text; every median secs, allocated MB and file count is in the subsection.
- Task 2 acceptance: against the merge-base with `origin/develop` (c447551), 0 removed lines and 1 added `### Results (` line; `git diff --stat` shows only `docs/design/chunk-store.md`, 64 insertions.

## Deviations from Plan

1. **Timing under load (most important).** Other projects' builds and tests (not cargo or rustc, so the plan's pgrep check passed) ran on the host for the whole run. The 1-minute load average was 52 to 137 on 32 logical cores. Step 3 ("no other build or test") was therefore not met, and these processes were not ours to stop. The run went ahead, because sizes and file counts do not depend on load. The doc method line says that other work ran, gives the load range, and says the times are an indication only. Load per build is in load.txt above. The runs of one input differ by up to 32 %. **Open item:** take the timing rows again on a quiet machine and replace the seconds, MB/s and peak RSS columns.
2. **No `git fetch origin develop`.** The launcher said no GitHub. The merge-base checks used the local `origin/develop` ref (c447551, the same commit as CUR).
3. **Extra scratch file `load.txt`** in `$M/results/2026-11-18/`, for the per-build load. machine.txt keeps only the eight planned keys.
4. **Branch name:** commits are on `feature/15-pack-measure` (the branch the launcher named), not an `agent-*` branch, the same as 15-01.

## Known Stubs

None. The FSKit round trip numbers are a planned manual run; the doc says so.

## Self-Check: PASSED

- FOUND: docs/design/chunk-store.md has `### Results (2026-11-18)`
- FOUND: commit 68b4277 (ancestor of HEAD)
- FOUND: $M/results/2026-11-18/seed.csv, machine.txt, load.txt
