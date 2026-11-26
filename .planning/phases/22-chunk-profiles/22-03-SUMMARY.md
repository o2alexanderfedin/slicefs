---
phase: 22-chunk-profiles
plan: 03
subsystem: benchmark tooling
tags: [chunk-profile, benchmark, fastcdc, docker, smoke]
status: complete

requires:
  - phase: 22-01
    provides: "ChunkProfile::new, Options::profile, ChunkStore::profile(), per-store zero chunk"
provides:
  - "crates/chunk-store/examples/chunk_profile_bench.rs: gen, ingest, open, probe (key=value lines)"
  - "scripts/chunk-profiles/bench.sh: runs x inputs x profiles, 28-column CSV, median_low rows, edits.csv, machine.txt, SMOKE=1"
  - "scripts/chunk-profiles/README.md: run lines, inputs, columns, edits, index mechanism notes"
  - "scripts/docker-linux-test/run.sh chunk-profiles-smoke (no FUSE)"
affects: [22-04]

actuals:
  tokens: 15226
  tasks: 3
  commits: 3

plan_head_before: 0c803d9e49be46aee1b01cb93c2fd0a5bcd82c75
plan_head_after: e22edcae8a54df0e44d2cdb360b98a61e8e6fd71

tech-stack:
  added: []
  patterns:
    - "an in-process benchmark example on the library API, driven by a portable bash script"
    - "a SpliceSource made of segments (base through a FileReader, or a few KiB of new bytes)"

key-files:
  created:
    - crates/chunk-store/examples/chunk_profile_bench.rs
    - scripts/chunk-profiles/bench.sh
    - scripts/chunk-profiles/README.md
    - .planning/phases/22-chunk-profiles/runs/smoke-22-03-macos.csv
    - .planning/phases/22-chunk-profiles/runs/smoke-22-03-linux.log
    - .planning/phases/22-chunk-profiles/runs/gate-22-03.log
  modified:
    - scripts/docker-linux-test/run.sh

key-decisions:
  - "The example lives in chunk-store (no new crate, no Cargo.toml change); output lines are the subcommand word plus key=value fields"
  - "gen tree: each file has its own generator from (seed, file number), so --change-every K changes 1 KiB in the middle of every K-th file and the other files stay equal (cross-version dedup works: 50.7 MB in, 25.8 MB stored at 4/8/32 KiB)"
  - "ingest_secs is the wall time of store open, all writes and close (the seal and sync are part of the ingest cost)"
  - "The page cache is warmed: bench.sh reads each input once before the runs (as seed-measure.sh does)"
  - "RAND_BYTES and SPARSE_BYTES default to 1 GiB (the seed-measure.sh default); rand and sparse are not in the default INPUTS"
  - "In SMOKE=1 a given data dir holds the smoke data; else it is WORK/smoke-data"
  - "median rows: median_low over the non-NA values of a column; NA when every run is NA"
  - "Edit read-back window: 32 KiB on each side of p (up to 64 KiB plus the edit), and the length of the new content is checked"
  - "A probe file under 4096 bytes, or an edit base under 2 x EDIT_SIZE, exits 1 with a message (no underflow)"

requirements-completed: [PROF-03]

duration: 30min
completed: 2026-11-26
---

# Phase 22 Plan 03: Chunk profile benchmark Summary

**An in-process benchmark of the chunk profiles: the example `chunk_profile_bench` (gen, ingest, open, probe on the chunk-store API, no mount, no `unsafe`) and the driver `scripts/chunk-profiles/bench.sh`, which writes one 28-column CSV row per run, input and profile, median rows, per-edit rows and machine info. Its smoke mode runs on macOS and in Linux Docker without FUSE, with the same counts on both.**

## Performance

- **Duration:** about 30 min
- **Started:** 2026-11-26T07:20Z
- **Completed:** 2026-11-26T07:52Z
- **Tasks:** 3 of 3
- **Files:** 7 (1 example, 2 script files, 1 modified script, 3 run logs)

## Accomplishments

- Task 1: the example. `ingest` streams files in 1 MiB pieces through `FileWriter`. `open` is a read-only cold open in a new process: it times `ChunkStore::open` plus `index_info`, and then walks the store dir outside the timer. `probe` times sequential and random 4 KiB reads, writes the edit base, and makes nine splice edits. The edit source reads the base through a `FileReader` and holds only the k new bytes. Each edit is read back and compared, and its length is checked.
- Task 2: the driver and the README. The loop order is run, input, profile. CPU time comes from the bash `time` keyword, and peak RSS from `/usr/bin/time -l` or `-v` (else `NA`). Sizes are measured in the example, so the script needs no `stat -f`.
- Task 3: the `chunk-profiles-smoke` mode of `run.sh` (no `--device`, no `--cap-add`, no `unshare`). The Linux smoke gives the same objects, raw and stored bytes and edit numbers as macOS.

Task 1 verify output (as printed by the verify, `cut -c1-160`):

```
ingest in_bytes=8388608 in_files=1 secs=0.485 objects=103 raw_bytes=8392799 stored_bytes=8392260 key=1e5518d295c15efe7197e8ada296e27dfb5500841d3645dfc08dd01c378
open open_ms=0.272 index_runs=0 index_run_entries=0 index_memtable_entries=103 index_filter_bytes=0 index_fence_bytes=0 apparent_bytes=8402180 alloc_bytes=84090
probe seq_reads=1024 seq_p50_us=0.2 seq_p99_us=63.2 rand_reads=200 rand_p50_us=57.3 rand_p99_us=248.4 edit_objects=18 edit_stored_bytes=597081 edit_rechunked_by
```

plus nine `edit` lines (rc=0). The `#[test]` and `unsafe` counts of the example are 0.

## Task Commits

1. **Task 1: the benchmark example** - `a018afa` (feat)
2. **Task 2: bench.sh, README, macOS smoke CSV** - `aa5145c` (feat)
3. **Task 3: Linux Docker smoke mode, Linux smoke log, gate log** - `e22edca` (feat)

## Gates

| Gate | Result |
|------|--------|
| Local gate before the Task 1 commit | exit 0; 1582 passed, 0 failed, 12 ignored (79 result lines) |
| Local gate before the Task 2 and Task 3 commits (one run; no `.rs` change after Task 1) | exit 0; 1582 passed, 0 failed, 12 ignored (79 result lines) |
| `scripts/safety/clippy.sh` (before Task 1, and before Tasks 2 and 3) | `CLIPPY os=Darwin toolchain=stable result=ok`, `UNSAFE files=31 tokens=132 result=ok` |
| macOS smoke (`SMOKE=1 RUNS=1`, two profiles) | rc=0, `rows=8 medians=4 bad=0`, `edits=18`, machine.txt cpu and load lines 2, home paths 0 |
| Linux Docker smoke | rc=0, header plus 4 median rows = 5, `os=Linux` 1, edits 18 |
| Cargo.toml / Cargo.lock diff from 0c803d9 | empty |
| Home paths or forbidden words in committed logs and CSV | 0 |

## Decisions Made

See `key-decisions` in the frontmatter. All are reversible.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Bug] mem_bytes and GNU-time RSS printed in scientific notation on Linux**
- **Found during:** Task 3 (first Linux smoke: `mem_bytes=1.68373e+10`)
- **Issue:** mawk prints large products with `%.6g`.
- **Fix:** `printf "%.0f\n"` for `MemTotal * 1024` and for the GNU `time -v` kbytes x 1024. Both smokes were run again, and the logs were saved from the second runs.
- **Files modified:** scripts/chunk-profiles/bench.sh
- **Commit:** aa5145c

### Other notes

- Clippy ran before the Task 1 commit (the launch rules ask for it before each code commit; the plan put it in Task 3).
- One gate run covers the Task 2 and Task 3 commits. Both commits change only scripts, a README and logs, and the gate ran on the tree that holds all of them.
- The README Linux note (a Task 3 file) went into the Task 2 commit, because the Linux smoke ran before the Task 2 commit.
- The Docker image has no GNU `/usr/bin/time`, so `open_peak_rss_bytes` is `NA` in Linux Docker. The README says so. The Dockerfile was not changed (not in this plan's files).
- `commit=unknown` in the Linux machine.txt: git does not run on the read-only `/src` in the container. This is the planned fallback.
- Scratch files went to `target/scratch` in the worktree (ignored by git), through `TMPDIR`.
- Commit-protocol note: the branch `exec/22-03` is outside the generic `agent-*` allow-list. The launch instructions name this branch, and it is not protected.
- STATE.md, ROADMAP.md and REQUIREMENTS.md were not updated. The orchestrator owns them.
- No file of plan 22-02 was needed or touched.

## Observations for 22-04

- Insert and delete at the start or middle make `splice` cut all bytes from p to the end of the edit base again (`rechunked_bytes` is about L - p), but only about 2 new objects are stored: FastCDC finds the old cuts again, and the chunks dedup. So `edit_rechunked_bytes` measures CPU work, and `edit_objects` / `edit_stored_bytes` measure the space cost.
- In the smoke, no store reaches 262,144 index entries, so every cold open replays the `.idx` files (`index_runs=0`), as the README describes.

## Known Stubs

None.

## Issues Encountered

None beyond the deviation above.

## Next Phase Readiness

- 22-04 can run `bench.sh DATA` on the Boost data (it needs 1_84_0 next to 1_85_0: `fetch-boost.sh DATA 1_84_0 1_85_0`) and write the report and the run time.

## Self-Check: PASSED

- FOUND: crates/chunk-store/examples/chunk_profile_bench.rs, scripts/chunk-profiles/bench.sh (executable), scripts/chunk-profiles/README.md, runs/smoke-22-03-macos.csv, runs/smoke-22-03-linux.log, runs/gate-22-03.log
- FOUND commits: a018afa, aa5145c, e22edca (ancestors of HEAD)
