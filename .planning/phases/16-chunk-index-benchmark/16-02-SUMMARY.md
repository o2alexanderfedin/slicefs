---
phase: 16-chunk-index-benchmark
plan: 02
subsystem: benchmarks
tags: [chunk-store, index, benchmark, rss, recovery, docs]
status: complete

requires: [16-01]
provides:
  - benchmarks/index-bench/README.md (Run, Steps, Output, Cache state, Disk and memory, Recovery paths, Linux, Other backends)
  - scripts/docker-linux-test/run.sh index-bench [SIZES...] mode (no FUSE device, no mount)
  - docs/design/chunk-store.md corrected "Index in memory" text and new "Index baseline (#66)" section
affects: [16-03, 17-on-disk-chunk-index]

actuals:
  tokens: 5500
  tasks: 3
  commits: 3
plan_head_before: 321a443ab087ef66e103ba0a35ab75ed88a278ff
plan_head_after: 2f6710ad4e355191cda7da0ad989571058409877

tech-stack:
  added: []
  patterns:
    - recovery scan rate from CPU time (median recover cpu_s - median open cpu_s), because the wall-time delta is negative under host load

key-files:
  created:
    - benchmarks/index-bench/README.md
  modified:
    - scripts/docker-linux-test/run.sh
    - docs/design/chunk-store.md

key-decisions:
  - "Recovery table gets two added columns (Extra CPU s, Scan MB/s (CPU)) from the raw cpu_s keys; the tool output is pasted unchanged with its n/a cells"
  - "macOS steady RSS (390/196/106 B/object) is above the live table because freed memory stays in the RSS; a probe and the Linux numbers (153/123) confirm it"
  - "Linux 100M not run: the VM had 12.6 GB MemAvailable, below the 20 GB condition"

duration: about 80 min of work (plus a pause for the API limit)
completed: 2026-11-18
---

# Phase 16 Plan 02: index baseline Summary

**The index-bench README and a Docker `index-bench` mode are added. The macOS baseline at 1M, 10M and 100M objects and the Linux baseline at 1M and 10M are in a new "Index baseline (#66)" section of docs/design/chunk-store.md. All timings were measured under heavy host load.**

## Task Commits

1. **Task 1: README and run.sh index-bench mode:** `988f766` (docs)
2. **Task 2: macOS baseline, corrected Index in memory text:** `a184284` (docs)
3. **Task 3: Linux rows:** `2f6710a` (docs)

## Key results (warm cache, under load)

| OS | Objects | Open s (median) | Open CPU s | Peak RSS B/object | Steady RSS B/object | contains hit ns | contains miss ns |
|---|---|---|---|---|---|---|---|
| macOS | 1M | 0.964 | 0.591 | 390.2 | 390.2 | 1304.8 | 727.5 |
| macOS | 10M | 12.844 | 5.881 | 256.3 | 195.8 | 700.5 | 324.0 |
| macOS | 100M | 141.241 | 62.239 | 154.2 | 105.5 | 819.5 | 419.8 |
| Linux | 1M | 0.595 | 0.593 | 302.0 | 153.4 | 695.4 | 266.3 |
| Linux | 10M | 10.364 | 10.198 | 192.7 | 123.2 | 886.8 | 305.3 |

- Disk: 104 B/object at every size; 1, 9 and 81 packs.
- Create: about 64k-72k puts/s on macOS, 132k-145k on Linux. Creator peak RSS 16.3 GB at 100M (macOS).
- Recovery scan, CPU-based: "all ro" 235-283 MB/s of CPU on macOS at 10M/100M (370 on Linux at 10M). The rw rows also write a new .idx, about 1.2 s of CPU for each sealed pack, so 30-48 MB/s.
- Load average (1 min) on the macOS host: 139 -> 130 (1M), 122 -> 126 (10M), 126 -> 79 (100M). Host load during the Linux run: 57 -> 69.
- RSS cross-check at 10M: `/usr/bin/time -l` maximum resident set size = `max_rss_after_bytes` = 2564218880.

## Verification

- `bash -n scripts/docker-linux-test/run.sh` passes; `^  index-bench)` count 1; no `dev/fuse` in the new case; the 5 old modes stay.
- README: `## Run` 1, `## Output` 1, no home path.
- Doc: `### Index baseline (#66)` 1; 24 macOS rows (>= 21); 16 Linux rows (>= 14); the "100M was not run on Linux" line is there; `57 bytes per object` 0; the section is before "Measurements to take", which has no macOS row and no change; no home path, host name or user name.
- No store file in the repo. The stores and the raw output are in a scratch directory outside the repo and in the Docker volume `slicefs-index-bench`.

## Deviations from Plan

1. **[Rule 1 - Measurement] CPU-based scan columns.** Under the host load (load average 80-140), the wall-time "Extra s" is negative in 6 of 12 macOS rows and 3 of 8 Linux rows, so the tool prints `n/a`. Two columns were added in the doc from the raw `cpu_s` keys: Extra CPU s = median recover `cpu_s` - median open `cpu_s`, and Scan MB/s (CPU). The tool code (`main.rs`) was not changed (not in this plan's files). **Phase 17 must compute the scan rate the same way** to compare with this baseline.
2. **[Rule 1 - Sanity check > 2x] macOS steady RSS.** 1M steady is 390 B/object against the expected 153. Cause, found with a small probe program (scratch only, not committed) that fills the same `HashMap<[u8;33], Loc>` on macOS: 153.1 B/object with `reserve`, 306 without, 390.0 with the same temporary buffers as the open; 122.5 at 10M with `reserve`. Linux steady RSS is 153.4 and 123.2, the same as the live table. The doc says this; it does not name an allocator mechanism.
3. **[Plan note rewritten]** The plan's note "the scan rate is a little too high" assumed clean wall deltas. The doc keeps that note and adds what the data shows: the 1M rows and the "one ro" rows at 10M/100M are in the range of the noise.
4. **[Environment] Load and swap.** The host load average was 80-140 during the macOS runs; host swap use went from 7.5 GB to 9.2 GB during the 100M run. Labeled in the doc. The wall time is about 2x the CPU time on macOS.
5. **[Environment] Branch.** Work is on `feature/16-index-bench` (coordinator choice), after the merge of `5fce243` (merge commit `321a443`, the measured commit).
6. **[Script detail] run.sh mode** also prints the VM load average before and after each size.

## Known Limits

- No cold-cache numbers (macOS needs `sudo purge` by hand; the container is not privileged).
- Linux CPU time in the VM is almost equal to wall time and varies (8.4-12.1 s for the 10M open), so the Linux CPU columns are noisy.
- Linux 100M not run (12.6 GB available in the VM, the run needs about 25 GB).

## Next Phase Readiness

Plan 16-03 can post the comment on #66 and link it from #85, with the tables from docs/design/chunk-store.md "Index baseline (#66)".

## Self-Check: PASSED

- FOUND: benchmarks/index-bench/README.md, scripts/docker-linux-test/run.sh (index-bench case), docs/design/chunk-store.md (Index baseline (#66))
- FOUND commits: 988f766, a184284, 2f6710a (ancestors of HEAD)
