---
phase: 22-chunk-profiles
plan: 04
subsystem: docs and benchmark report
tags: [chunk-profile, fastcdc, benchmark, report, store-format]
status: complete

requires:
  - phase: 22-chunk-profiles plan 02
    provides: "--chunk-profile on seed and mount, the M9 pairing reasons, msg_profile_differs, the stats line and JSON field"
  - phase: 22-chunk-profiles plan 03
    provides: "crates/chunk-store/examples/chunk_profile_bench.rs, scripts/chunk-profiles/bench.sh and README"
provides:
  - "docs/design/store-format.md: Chunks line, chunk-profile ro_compat flag row, example 16/64/256 KiB header, five M9 reasons, M16 (another profile), stats line and JSON field"
  - "docs/design/chunk-store.md: per-store profile (Options::profile), metadata note (D-14), zero shortcut at the store max, stream memory (D-17)"
  - "README.md: the --chunk-profile line"
  - "docs/research/chunk-profiles.md: the five-profile report and a recommended later default"
  - "runs/chunk-profiles-results.csv, chunk-profiles-edits.csv, chunk-profiles-machine.txt, gate-22-04.log"
affects: [phase-23, phase-24]

actuals:
  tokens: 12617
  tasks: 3
  commits: 3

plan_head_before: ed1694a5df6e1140163c841c05f69c557ba9ade1
plan_head_after: 3107c64dda919e898f04fa6212a18c8a7107b4e2

tech-stack:
  added: []
  patterns:
    - "report tables generated from the committed CSV by a script, then checked by the plan's python check"

key-files:
  created:
    - docs/research/chunk-profiles.md
    - .planning/phases/22-chunk-profiles/runs/chunk-profiles-results.csv
    - .planning/phases/22-chunk-profiles/runs/chunk-profiles-edits.csv
    - .planning/phases/22-chunk-profiles/runs/chunk-profiles-machine.txt
    - .planning/phases/22-chunk-profiles/runs/gate-22-04.log
  modified:
    - docs/design/store-format.md
    - docs/design/chunk-store.md
    - README.md

key-decisions:
  - "The message of a different profile is M16 in store-format.md, not M15: the #201 mount-lock fix (in the rebase base) already uses M15"
  - "The report recommends 8/32/128 KiB as a later default (8/16/64 KiB as the low-risk alternative); the code default stays 4/8/32 KiB in v2.2 (D-01)"

requirements-completed: [PROF-01, PROF-03]

duration: 35min
completed: 2026-11-26
---

# Phase 22 Plan 04: Chunk profile docs, benchmark and report Summary

**The format, chunk-store and README docs now describe the per-store FastCDC profile with the texts of the code. The five-profile benchmark ran by hand on Boost 1_84_0 + 1_85_0 trees and the 1_85_0 tar (3 runs, 5 min 49 s). The report recommends 8/32/128 KiB as a later default, and the code default stays 4/8/32 KiB in v2.2.**

BASE: `ed1694a5df6e1140163c841c05f69c557ba9ade1` (head of `origin/feature/phase-22-chunk-profiles`, with 22-01 to 22-03 merged and the #201 fix).

## Performance

- **Started:** about 2026-11-26T10:05Z (fetch and gate start at 10:07:56Z)
- **Completed:** 2026-11-26T10:42Z
- **Tasks:** 3 of 3
- **Files:** 8 (3 docs changed, 1 report, 4 run files)

## Task Commits

1. **Task 1: phase gate; store-format, chunk-store and README docs** - `3894905` (docs)
2. **Task 2: five-profile benchmark data** - `5536494` (chore)
3. **Task 3: the report** - `3107c64` (docs)

## Gates

| Gate | Result |
|------|--------|
| Local gate (`cargo test --workspace`, 4 skips), merged tree, before the benchmark | exit 0; 79 result lines, 1605 passed, 0 failed, 12 ignored (10:07:56Z to 10:13:17Z, 5 min 21 s, fresh target) |
| `RUST_STABLE=stable scripts/safety/clippy.sh` | `CLIPPY os=Darwin toolchain=stable result=ok`, `UNSAFE files=31 tokens=132 result=ok` |
| `git diff ed1694a -- crates/ Cargo.toml Cargo.lock` | empty (the code default is unchanged) |
| Task 1 doc greps (Chunks line, flag row, no `none yet`, M15 row, stats line, M16 text = `msg_profile_differs` literal, five M9 reasons, README, `Options::profile`, `chunk-profiles.md`, `16384 65536 262144`, `2 x max`) | all as expected |
| Task 2 CSV check | `runs=30 medians=10 profiles=5 bad=0`; machine lines 4; edit rows 135; tar medians 5 |
| Task 3 python check | `REPORT-OK` |
| Home path, forbidden-word, host name and user name greps on the report, CSVs, machine.txt and gate log | 0 |

No test flaked in this plan's gate run. No cargo test or clippy ran during the benchmark. The docs-only commits after Task 1 needed no second gate (plan rule 3).

## Benchmark run (Task 2)

- Data: `boost_1_84_0` was not there; `timeout 1200 scripts/macos-mount-test/fetch-boost.sh "$M/data" 1_84_0` fetched it (rc 0, under a minute). `boost_1_85_0` tree and tar were already there.
- Quiet check: `uptime` before the gate was `load averages: 13.75 13.88 11.59`; after the release build `11.63 13.72 13.64`. The other load was test binaries of another project and OrbStack, not cargo or SliceFS. The quiet loop (5-minute checks) saw load1 11.09, 14.21, 8.52 and 7.73 (10:32:19Z), then the run started. `uptime` at the start: `8.00 8.98 10.79`; at the end: `9.39 9.19 10.27`.
- Build: `timeout 1200 cargo build --release -p chunk-store --example chunk_profile_bench` (17.9 s).
- Command (through a scratch launcher script, cwd = the worktree):

```sh
M=${MEASURE_ROOT:-$HOME/slicefs-measure}
OUT="$M/results/chunk-profiles-2026-11-26"
WORK="$OUT" BIN=<repo>/target/release/examples/chunk_profile_bench \
  timeout 5400 bash scripts/chunk-profiles/bench.sh "$M/data" 3 \
  > "$OUT.stdout.csv" 2> "$OUT.progress.log"
```

- Env: script defaults (five profiles, `INPUTS="trees tar"`, `BOOST_VERS="1_84_0 1_85_0"`, `SEQ_BYTES`/`EDIT_BYTES` 64 MiB, `READS=10000`, `EDIT_SIZE=4096`).
- Fallback: none.
- Start 10:32:32Z, end 10:38:21Z UTC: **5 min 49 s** wall time, exit 0. No store dir was left in WORK.
- Outputs stay in `$M/results/chunk-profiles-2026-11-26*` (outside the repo); results, edits and machine info were copied through the path sed into `runs/`.

## Headline numbers (median rows)

| Profile (KiB) | trees stored MB | trees objects | trees dedup | tar stored MB | tar objects | tar cold open ms | tar ingest MB/s | rand p50 / p99 us | 9 edits stored KB |
|---|---|---|---|---|---|---|---|---|---|
| 4/8/32 | 241.2 | 220,311 | 1.78x | 194.2 | 60,997 | 28.7 | 180.3 | 52.7 / 168.3 | 211.1 |
| 8/16/64 | 234.1 | 195,817 | 1.73x | 175.4 | 29,445 | 13.2 | 215.6 | 60.4 / 140.9 | 313.4 |
| 8/32/128 | 231.0 | 187,101 | 1.71x | 165.1 | 15,458 | 6.7 | 239.3 | 72.1 / 219.2 | 335.1 |
| 16/64/256 | 229.8 | 182,527 | 1.68x | 158.6 | 7,614 | 3.1 | 250.9 | 121.8 / 320.3 | 359.8 |
| 32/128/512 | 229.1 | 180,470 | 1.66x | 152.4 | 3,469 | 1.5 | 250.4 | 257.0 / 576.3 | 328.3 |

Byte and object counts were the same in all three runs; times varied with the load.

## Draft for issue #67 (for the orchestrator to post; this executor ran no `gh` command)

> Phase 22 measured the five chunk profiles on Boost 1_84_0 + 1_85_0 trees (one store) and the 1_85_0 tar, in process, 3 runs: `docs/research/chunk-profiles.md` (data: `.planning/phases/22-chunk-profiles/runs/chunk-profiles-results.csv`).
> Larger chunks dedup a little less but compress better, so every larger profile stores less. Trees: -3.0 % to -5.0 % stored, -11 % to -18 % objects. Tar: -9.7 % to -21.5 % stored, -52 % to -94 % objects.
> Cost: random 4 KiB reads get slower (p50 +15 % at 8/16/64, +37 % at 8/32/128, +131 % at 16/64/256, +388 % at 32/128/512), and nine 4 KiB edits store +49 % to +70 % more.
> Recommendation for a later default: 8/32/128 KiB (8/16/64 KiB as the low-risk step).
> The code default stays 4/8/32 KiB in v2.2; a change is an owner decision. A store can already use another profile with `slicefs seed --chunk-profile MIN/AVG/MAX`.
> Limits: warm page cache, no FUSE, file content only (metadata values also follow the store profile, not measured here).

## Deviations from Plan

### Auto-fixed Issues

None in code (this plan changes docs and data only).

### Other notes

- **M16, not M15 (plan text says M15).** The #201 mount-lock fix in the base already took M15 (`a writable mount while ... holds the store lock`). The profile message is M16, with the `msg_profile_differs` literal. The plan grep `^| M15 |` = 1 still holds (the #201 row). The `{have}`/`{want}` note points to M16.
- **Read-only mount order in store-format.md.** The plan did not list it, but the doc's "mount is refused when (in this order)" list and the store-lock order sentence now name the `--chunk-profile` check (M16) first, as `mount::start` does it (right after `header::check`, before M11).
- **JSON sample order** in the stats section follows the struct (`format`, `flags`, `chunk_profile`, `access`).
- **`rustc=unknown` in machine.txt.** The launcher shell did not have the Rust tools in PATH, so `bench.sh` printed `unknown`. The binary was built just before with the PATH set (`rustc -V`: 1.95.0-nightly); the report says so.
- **Load above 8 for most of the wait.** The run started at load1 7.73 (8.00 by `uptime` at the start), inside the 30-minute limit. The report says that the times are indicative and the byte and object counts are firm.
- **Task 2 commit type.** First committed as `data(22-04)`, then amended (local, before any other commit) to `chore(22-04)`, a type of the commit protocol.
- **Branch.** `exec/22-04` is outside the generic `agent-*` allow-list; the launch instructions and plan rule 1 name it, and it is not protected (as in 22-01 to 22-03).
- STATE.md, ROADMAP.md and REQUIREMENTS.md were not changed (the orchestrator owns them).

## Known Stubs

None. `CHUNK_PROFILE_RELEASE = "v2.2"` is still the planned placeholder (D-10); the docs say so.

## Issues Encountered

None. The full run fit easily in the 5400 s limit, so no fallback was needed.

## Next Phase Readiness

- The owner can decide the default from `docs/research/chunk-profiles.md`. Open measurement gaps named in the report: a read test through FUSE with a cold cache, a store large enough to have index runs on disk, and the metadata rewrite cost per edit (Phase 24 combined benchmark).

## Self-Check: PASSED

- FOUND: docs/research/chunk-profiles.md, runs/chunk-profiles-results.csv, runs/chunk-profiles-edits.csv, runs/chunk-profiles-machine.txt, runs/gate-22-04.log
- FOUND commits: 3894905, 5536494, 3107c64 (ancestors of HEAD)
