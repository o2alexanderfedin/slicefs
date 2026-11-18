---
phase: 15-pack-file-measurements
plan: 01
subsystem: measurement scripts
status: complete
tags: [seed, packs, measurement, bash, macos]
requires: []
provides:
  - scripts/pack-measure/seed-measure.sh (repeatable seed time and store-size measurement, any build)
  - "$M/bin/slicefs-{c2f82fa,7016bb6,c447551} release binaries + .info files + current.txt (outside repo)"
  - "$M/data/boost_1_85_0.tar and $M/data/x/boost_1_85_0 (outside repo)"
affects: [15-02, 15-03]
tech-stack:
  added: []
  patterns: [bash + inline python3, /usr/bin/time -l, stat -f %z apparent size, median_low over runs]
key-files:
  created:
    - scripts/pack-measure/seed-measure.sh
    - scripts/pack-measure/README.md
  modified: []
decisions:
  - "CUR = c447551 (origin/develop head on 2026-11-18)"
  - "All three builds on the default toolchain rustc 1.95.0-nightly (9e79395f9 2026-02-10), no patches"
  - "The three release builds ran in parallel in one batch (Task 2's 7016bb6 build started together with Task 1's builds)"
metrics:
  duration: ~10 min
  completed: 2026-11-18
actuals:
  tokens: 2300
  tasks: 3
  commits: 2
plan_head_before: 76b5ea0
plan_head_after: 85efd81ce8bb6a95e8d0e0d358655cd239d82d4a
---

# Phase 15 Plan 01: seed-measure script and builds Summary

One bash script, `scripts/pack-measure/seed-measure.sh`, measures `slicefs seed` wall time, MB/s, peak RSS, store apparent and allocated bytes, and store file counts split by `vt0/`, `packs/`, `segments/` and other, for the Boost tree, the Boost tar and a fixed random file. It runs unchanged on c2f82fa, 7016bb6 and the current develop build. The three release binaries and the Boost 1_85_0 data are in the measure root `M=${MEASURE_ROOT:-$HOME/slicefs-measure}`.

## Tasks

| Task | Name | Commit | Verify |
|------|------|--------|--------|
| 1 | Tracer: script measures one random input on fresh c2f82fa and current builds | ea91f2b | TRACER-OK |
| 2 | Build 7016bb6 and fetch the Boost 1_85_0 data | none (no repo file changes) | PREP-OK |
| 3 | Add the Boost tree and tar inputs, median rows, and the README | 85efd81 | SELFTEST-OK |

## Builds (outside the repo)

`$M/bin/current.txt` = `c447551` (CUR).

| Label | .info commit | .info rustc | patches | build time |
|-------|--------------|-------------|---------|------------|
| c2f82fa | c2f82fa14bed15547bbcdf37783134b750b60792 | rustc 1.95.0-nightly (9e79395f9 2026-02-10) | none | 102 s |
| 7016bb6 | 7016bb61074c9ca1f4e98cbd2d8901be5baf8184 | rustc 1.95.0-nightly (9e79395f9 2026-02-10) | none | 93 s |
| c447551 | c4475510ddc79c4db2f213fc0a2d2bfcbe17b904 | rustc 1.95.0-nightly (9e79395f9 2026-02-10) | none | 84 s |

The build times are wall times of three builds that ran at the same time (cold target dirs). Each build used a detached worktree `$M/wt-LABEL` (submodule `crates/data-id` at ec08e9b for the two old commits) and `CARGO_TARGET_DIR=$M/target-LABEL`. All three worktrees were removed with `git worktree remove --force`; no `slicefs-measure/wt-*` entry stays registered. The target dirs `$M/target-*` and the build logs `$M/build-*.log` stay in the measure root.

Boost: `bash scripts/macos-mount-test/fetch-boost.sh $M/data 1_85_0` made `$M/data/boost_1_85_0.tar` (823,613,440 bytes) and `$M/data/x/boost_1_85_0` (77,655 files).

## Verify results

- TRACER-OK (4 MiB random): c2f82fa row has 1257 vt0/ files and 0 packs/; c447551 row has 0 vt0/ and 2 packs/ files.
- PREP-OK: 7016bb6 row has 13 vt0/ and 2 packs/ files; tar size and tree file count match.
- SELFTEST-OK: 12 data rows and 3 median rows per build, 15 fields each; the tree median row shows vt0-only for c2f82fa, vt0+packs for 7016bb6, packs-only for c447551; no store-* dir is left; `/usr/bin/false` as the binary exits non-zero with no median row; a missing Boost version exits 2 and names `fetch-boost.sh`.
- Acceptance greps: no `--hash` in the script; `/usr/bin/time -l -o` present; `median_low` and `in-tar-` present; the README names `fetch-boost.sh` and `boost-test.sh` and has no home paths and no licensing text; the script is mode 100755.

## Deviations from Plan

1. **Order only:** the 7016bb6 worktree and build (Task 2) started in the same parallel batch as the Task 1 builds, and the Boost fetch ran at the same time. Same toolchain, same recipe; nothing is measured during builds, so the results do not change.
2. **Median row write:** `m=$(median "$IN"); row "$m"` instead of `row "$(median ...)"`, so a failure in the python3 median step stops the script under `set -e`.
3. **Branch name:** commits are on `feature/15-pack-measure` (the branch the launcher named), not an `agent-*` branch.

No local source patch was needed. No auth gates.

## Known Stubs

None.

## Self-Check: PASSED

- FOUND: scripts/pack-measure/seed-measure.sh, scripts/pack-measure/README.md
- FOUND: commits ea91f2b, 85efd81 (ancestors of HEAD)
- FOUND: $M/bin/slicefs-c2f82fa, slicefs-7016bb6, slicefs-c447551, current.txt, three .info files; $M/data/boost_1_85_0.tar, $M/data/x/boost_1_85_0
