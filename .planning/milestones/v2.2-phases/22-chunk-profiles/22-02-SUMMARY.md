---
phase: 22-chunk-profiles
plan: 02
subsystem: store header and command line
tags: [chunk-profile, store-header, cli, seed, mount, stats]
status: complete

requires:
  - phase: 22-chunk-profiles plan 01
    provides: ChunkProfile, Line::Chunks, CHUNK_PROFILE_FLAG/RELEASE, marker_with, msg_profile_differs, content::{profile_of, check_profile, check_write_with}
provides:
  - "header::parse: the chunks line and the ro_compat chunk-profile flag come as a pair (three new M9 reasons)"
  - "content::parse_chunk_profile (the clap parser of --chunk-profile, MIN/AVG/MAX in whole KiB)"
  - "--chunk-profile on `slicefs seed` (records the profile of a new store) and `slicefs mount` (check only)"
  - "seed::run_seed_with(store, src, hash, profile); run_seed keeps its signature"
  - "mount::StartArgs::chunk_profile; run_mount takes chunk_profile after hash; the check in start before any load"
  - "stats: StoreStats::chunk_profile (ProfileOut {min, avg, max} in bytes), the line `Chunk profile    : ...`"
affects: [22-04, phase-23]

actuals:
  tokens: 10950
  tasks: 3
  commits: 3

plan_head_before: 0c803d9e49be46aee1b01cb93c2fd0a5bcd82c75
plan_head_after: c3cc05de97f04762c9862669cf0269703300695e

tech-stack:
  added: []
  patterns:
    - "a post-loop pairing check in the header parse (setting line and its flag)"
    - "a clap value parser that returns Result<_, String> (as parse_dirty_bytes)"

key-files:
  created:
    - .planning/phases/22-chunk-profiles/runs/gate-22-02.log
  modified:
    - crates/slicefs-cli/src/header.rs
    - crates/slicefs-cli/src/content.rs
    - crates/slicefs-cli/src/cli.rs
    - crates/slicefs-cli/src/seed.rs
    - crates/slicefs-cli/src/main.rs
    - crates/slicefs-cli/src/mount.rs
    - crates/slicefs-cli/src/stats.rs
    - crates/slicefs-cli/tests/chunk_profile_tests.rs

key-decisions:
  - "M9 reasons: `chunks line with no chunk-profile flag` (on the chunks line), `chunk-profile flag with no chunks line` and `the feature chunk-profile must be ro_compat` (on the flag line); `two chunks lines` and `bad chunks line` were already in 22-01"
  - "A `chunks` line that names the default profile, with the flag, is accepted (this build never writes it)"
  - "ro_compat class confirmed (D-11), as 22-01 showed with an_old_reader_reads_a_store_of_another_profile"
  - "mount --chunk-profile is a check only, and only when the store has a header; with no header load_store refuses the path as before"
  - "The check in mount::start is a small local block right after header::check; load_store and the run_mount pre-flight are not changed (easy merge with #201)"
  - "stats human line comes from ProfileOut through ChunkProfile::new + Display (`16/64/256 KiB`); JSON is an object of bytes"

requirements-completed: [PROF-01, PROF-02]

duration: 40min
completed: 2026-11-26
---

# Phase 22 Plan 02: Chunk profile on the command line Summary

**`slicefs seed --chunk-profile 16/64/256` makes a store with that FastCDC profile. `seed` and `mount` refuse a store with another profile and change nothing. `stats` shows the profile. The header refuses a `chunks` line without its ro_compat `chunk-profile` flag, and the reverse. Two stores with two profiles work through the file-system layer in one process.**

BASE: `0c803d9e49be46aee1b01cb93c2fd0a5bcd82c75` (head of `origin/feature/phase-22-chunk-profiles`, with 22-01 merged).

## Task Commits

1. **Task 1: pairing rules, parse_chunk_profile, Kani gate** - `74cccb4` (feat)
2. **Task 2: --chunk-profile on seed and mount** - `22d4969` (feat)
3. **Task 3: stats, two profiles through the file system, plan gates** - `c3cc05d` (feat)

## Accomplishments

- Task 1: `parse` refuses a `chunks` line with no flag, the flag with no `chunks` line, and a `compat` or `incompat` flag. Each M9 has the line number. A header with a profile opens read and write for every Want, and `to_text` gives the same bytes back (with the flag line first, too). `parse_chunk_profile` refuses all 12 bad values in the plan.
- Task 2: `seed --chunk-profile P` on a new store writes `marker_with(DEFAULT_HASHER, P)`. With no option, a new store gets `marker(DEFAULT_HASHER)` and 4/8/32 KiB. `seed` and `mount::start` with another profile give an error that names both profiles, and the store tree (names and BLAKE3 sums) does not change. A read-only (ro_compat) mount refuses it too, and does not give the `--hash` message. With the same profile or no option, the mount uses the recorded profile.
- 22-01 note covered: `a_hash_change_keeps_the_chunk_profile`. On a 16/64/256 KiB store, `seed --hash test-keyed-blake3` rewrites FORMAT to `marker_with(keyed, P)`. Then `mount --hash blake3-256 --chunk-profile P` rewrites it back to `marker_with(DEFAULT_HASHER, P)`. In both cases the profile stays P. This is the only path that writes the `chunks` line back.
- Task 3: `stats` prints `Chunk profile    : 16/64/256 KiB` and `Feature flags    : ro_compat chunk-profile (needs SliceFS <CHUNK_PROFILE_RELEASE>; known here)`. JSON `chunk_profile` is `{"min":16384,"avg":65536,"max":262144}`, and a default store shows 4/8/32 KiB. `two_profiles_through_the_file_system` uses a default store and a 16/64/256 KiB store, both open at the same time. Each does a streamed 4 MiB file, a 3 MiB zero run added by a size change, and a 4 KiB in-place edit (a splice at release). Each reads back byte for byte and has the key of `content::put_all`. The keys of the 4 MiB file differ between the two stores.

## Gates

| Gate | Result |
|------|--------|
| Local gate (4 skips), after Task 1 | exit 0; 1587 passed, 0 failed, 12 ignored (79 result lines) |
| Local gate, after Task 2 (first run) | exit 101: flake in `flusher_tests::a_throttled_writer_does_not_wait_when_the_flusher_cannot_free` (see Deviations) |
| Local gate, after Task 2 (rerun) | exit 0; 1593 passed, 0 failed, 12 ignored |
| Local gate, final (Task 3) | exit 0; 1595 passed, 0 failed, 12 ignored (79 result lines) |
| `scripts/safety/kani.sh` (Task 1) | `KANI ok harnesses=9` |
| `scripts/safety/clippy.sh` (before each commit) | `CLIPPY os=Darwin toolchain=stable result=ok`, `UNSAFE files=31 tokens=132 result=ok` |
| Cargo.toml / Cargo.lock / crates/metadata / crates/chunk-store diff from BASE | empty |
| Home paths or forbidden words in gate-22-02.log | 0 |

All lines are in `runs/gate-22-02.log`. No mount, Docker or `run.sh` command was run in this plan (D-07, D-24).

## TDD: red runs and guard mutations

Red runs before the code:
- Task 1 header tests: `a_chunks_line_needs_the_flag_and_the_flag_needs_the_line` FAILED, because `{FORMAT}{HASH}chunks 16384 65536 262144` parsed to a Header. `a_bad_chunks_line_is_refused` and `a_profile_header_round_trips` passed, because 22-01 already has those rules (they are guards, see below).
- Task 1 content tests: 5 errors `E0425 cannot find function parse_chunk_profile`.
- Task 2: compile errors. `Cmd::Seed` and `Cmd::Mount` have no field `chunk_profile` (2 each), there is no `seed::run_seed_with` (6), and `StartArgs` has no field `chunk_profile` (5).
- Task 3: `E0609 no field chunk_profile on type StoreStats`. `two_profiles_through_the_file_system` passed at once on 22-01 code (a guard).

Guard mutations. Each mutation was applied to a copy, run, and then undone, and `git diff` was checked after each undo:
- `parse_chunks` with no digit check (accepts `+16384`): `a_bad_chunks_line_is_refused` FAILED.
- `to_text` writes the `chunks` sizes as min/max/avg: `a_profile_header_round_trips` FAILED.
- The `two chunks lines` check is removed: `a_chunks_line_needs_the_flag_and_the_flag_needs_the_line` FAILED.
- `to_text` drops the `chunks` line: `a_hash_change_keeps_the_chunk_profile` FAILED (the rewritten header has the flag with no `chunks` line).
- `start` does not call `check_profile`: `mount_checks_the_profile` and `a_read_only_mount_refuses_another_profile` FAILED.
- `content::open` uses `ChunkProfile::DEFAULT` in place of `profile_of`: `two_profiles_through_the_file_system` FAILED (profile assert).

## Deviations from Plan

### Auto-fixed Issues

None in the code.

### Other notes

- **Flaky test, not related to this plan:** the first local gate after Task 2 failed in `flusher_tests::a_throttled_writer_does_not_wait_when_the_flusher_cannot_free`. In 10 s the flusher made 0 passes, so the wait reached its limit. This is a timing problem under full-workspace load. The test does not use any code this plan changed. It passed 3 of 3 times alone, and the full gate rerun exited 0. Task 2 was committed after the rerun. I logged it here only. I did not fix it, because it is out of scope.
- **Extra test (from the launch note):** `a_hash_change_keeps_the_chunk_profile` in `chunk_profile_tests.rs`. It covers the `--hash` rewrite of a store with a `chunks` line, through both `seed` and `mount::start`.
- **Kani:** the byte core did not change, and `kani.sh` ran once in Task 1 (9 of 9).
- **PATTERNS.md** says that two header tests must change. They did not change, and they stay green. The `chunk-profile 16 64 256` line is still `Line::Other`, as 22-01 found.
- **For plan 22-04 (docs):** `docs/design/store-format.md` needs the new M9 reasons: `chunks line with no chunk-profile flag`, `chunk-profile flag with no chunks line`, `the feature chunk-profile must be ro_compat`. `two chunks lines` and `bad chunks line` are from 22-01. That file belongs to 22-04, so this plan does not change it.
- **Commit protocol:** `exec/22-02` is outside the generic `agent-*` branch allow-list. The launch instructions and plan rule 1 name this branch, and it is not protected, so the commits went on it (the same as 22-01).
- STATE.md, ROADMAP.md and REQUIREMENTS.md were not updated, by the launch instructions (the orchestrator owns them).

## Known Stubs

None. `CHUNK_PROFILE_RELEASE = "v2.2"` is still the planned placeholder from 22-01 (D-10).

## Next Phase Readiness

- 22-04 can document the option on `seed` and `mount`, the stats line and JSON field, and the M9 reasons above.
- The mount.rs change is one block in `start` right after `header::check`, plus the `StartArgs` field and the `run_mount` argument. `load_store` is not changed, so the later merge of #201 is simple.

## Self-Check: PASSED

- FOUND: runs/gate-22-02.log and all 8 modified source and test files
- FOUND commits: 74cccb4, 22d4969, c3cc05d (ancestors of HEAD)
