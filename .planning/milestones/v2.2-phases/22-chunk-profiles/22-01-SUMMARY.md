---
phase: 22-chunk-profiles
plan: 01
subsystem: chunk store and store header
tags: [chunk-profile, fastcdc, store-header, feature-flags, tracer]
status: complete

requires:
  - phase: 21.1-store-header-and-format-policy-154
    provides: store format 7 header (Line, parse, decide, check_with, create_with, KNOWN_FLAGS, the rule to add a flag)
provides:
  - "chunk_store::ChunkProfile (new with limits and the zero check, DEFAULT, ranges, min, avg, max, Display)"
  - "chunk_store::Options::profile, ChunkStore::profile(), the per-store zero chunk ChunkStore::zero_chunk() (pub(crate), lazy OnceLock)"
  - "file.rs: the writer, the reader zero path, known_zero and splice read store.profile() and store.zero_chunk()"
  - "slicefs_cli::header: CHUNK_PROFILE_FLAG, CHUNK_PROFILE_RELEASE (placeholder v2.2), KNOWN_FLAGS = [chunk-profile], Line::Chunks, Header::chunk_profile, marker_with, msg_profile_differs, a profile argument on check_with and create_with"
  - "slicefs_cli::content: profile_of (both open points), check_profile, check_write_with"
affects: [22-02, 22-03, 22-04, phase-23, phase-24]

actuals:
  tokens: 16124
  tasks: 3
  commits: 3

plan_head_before: c075af8e7e81378903975d9b0b69cc55354bf92a
plan_head_after: 26625ea0746ec9c825da61a7db2993cba371661b

tech-stack:
  added: []
  patterns:
    - "a store setting carried by Options and read back with an accessor (profile next to hasher)"
    - "a per-store lazy OnceLock for derived constant data (the zero chunk)"

key-files:
  created:
    - crates/chunk-store/tests/profile_tests.rs
    - crates/slicefs-cli/tests/chunk_profile_tests.rs
    - .planning/phases/22-chunk-profiles/runs/red-22-01.log
    - .planning/phases/22-chunk-profiles/runs/gate-22-01.log
  modified:
    - crates/chunk-store/src/lib.rs
    - crates/chunk-store/src/file.rs
    - crates/slicefs-cli/src/header.rs
    - crates/slicefs-cli/src/content.rs

key-decisions:
  - "Header word `chunks` with sizes in bytes; written only for a non-default profile, with `ro_compat chunk-profile <CHUNK_PROFILE_RELEASE>`; a default store keeps the header bytes of format 7"
  - "CHUNK_PROFILE_RELEASE = \"v2.2\" is a placeholder until the owner names the release; no test uses the text"
  - "ChunkProfile::new returns Result<_, String>; checks run power of two, order, fastcdc limits, then the zero premise (2 x max + 1 zeros cut into [max, max, 1]), so FastCDC never panics"
  - "Display: `16/64/256 KiB` when all three sizes are whole KiB, else `64/256/1024 B`"
  - "The zero chunk is a lazy OnceLock field of ChunkStore (built on first use, one max-size buffer per store)"
  - "The `chunks` parse is a str-level helper (parse_chunks) outside the Kani byte core; the byte core is unchanged and kani.sh stays 9/9"
  - "chunk_ranges and CHUNK_MIN/AVG/MAX stay as default-profile helpers (tests and ChunkProfile::DEFAULT); no non-test line of file.rs uses them"

requirements-completed: [PROF-02]

duration: 40min
completed: 2026-11-26
---

# Phase 22 Plan 01: Chunk profile per store Summary

**A FastCDC profile is now a property of the store: `ChunkProfile` in chunk-store, carried by `Options` and read by every size use in `file.rs` (with a per-store zero chunk). It is recorded in FORMAT as `chunks <min> <avg> <max>` plus the ro_compat `chunk-profile` flag, applied at both content open points, and refused on an existing store when it differs. The default stays 4/8/32 KiB with the header bytes of today.**

BASE (recorded before the first edit): `c075af8e7e81378903975d9b0b69cc55354bf92a` (merge-base of `exec/22-01` and `feature/phase-22-chunk-profiles`).

## Performance

- **Duration:** about 40 min
- **Started:** 2026-11-26T06:36:54Z
- **Completed:** 2026-11-26T07:16Z
- **Tasks:** 3 of 3
- **Files modified:** 8 (4 source, 2 new test files, 2 run logs)

## Accomplishments

- Task 1 (tracer): a store made with `check_write_with(.., Some(16/64/256 KiB))` records the profile. `seed` cuts its files with it, `open_read_only` reports it, and a default store keeps `marker(DEFAULT_HASHER)` byte for byte. Another profile is refused with a message that names both profiles, before any rewrite. This also holds with `--hash`.
- Task 2: the process-wide zero chunk is gone. Each store owns its own zero chunk, and the reader zero path, `known_zero` and `splice` (max, zero buffer, read-ahead step, re-chunk) read the store profile. The splice key equals the key of a full write for all five profiles.
- Task 3: the golden cut points of fastcdc 3.2.1 match exactly for the five profiles (pinned values unchanged). Two stores with different profiles work in one process. A default-options open reads a 16/64/256 KiB store byte for byte, but its writes give other keys, which confirms the ro_compat class (D-11).

## Task Commits

1. **Task 1: tracer, a store made with a profile seeds with it** - `7dd2038` (feat)
2. **Task 2: per-store zero chunk and the other file.rs sites** - `65202b3` (feat)
3. **Task 3: golden cut points, two profiles in one process, old reader; plan gates** - `26625ea` (test)

## Gates

| Gate | Result |
|------|--------|
| Local gate (`cargo test --workspace` with the 4 skips), final | exit 0; 1582 passed, 0 failed, 12 ignored (79 result lines) |
| Local gate after Task 1 / Task 2 | exit 0; 1575 / 1579 passed, 0 failed |
| `scripts/safety/kani.sh` | `KANI ok harnesses=9` |
| `scripts/safety/model.sh` (run after Task 2 and again in Task 3) | `MODEL ok` (chunk-store 1, slicefs-cli 6) |
| `scripts/safety/clippy.sh` (before each commit) | `CLIPPY ... result=ok`, `UNSAFE files=31 tokens=132 result=ok` |
| Cargo.toml / Cargo.lock / crates/metadata diff from BASE | empty (`diff_exit=0`) |
| file.rs `pub` / `fn is_cut` diff lines | 0 |
| Home paths or forbidden words in the run logs | 0 |

The known flake `snapshot::tests::test_snapshot_switch_by_name` did not fail in any run.

## TDD: red runs and guard mutations

All are in `runs/red-22-01.log`:
- Task 1: compile errors of `chunk_profile_tests` (no `ChunkProfile`, `check_write_with`, `marker_with`, `CHUNK_PROFILE_*`, `profile()`).
- Task 2: `zero_chunks_of_each_store_need_no_store_read` failed only for the 16/64/256 KiB store (EIO: it read its zero chunks from the store). `a_splice_over_zeros_gives_the_key_of_a_full_write_for_each_profile` failed its key compare at 8/16/64 KiB, the first non-default profile.
- Guard mutations (each one undone and checked with `git diff`):
  - Drop the power-of-two check: `bad_profiles_are_refused` fails on 12288/24576/49152.
  - Make the max limit exclusive: `the_five_profiles_and_the_limits_are_accepted` fails on 1/4/16 MiB.
  - Cap `ranges` at max(32 KiB, 2 x avg): `zero_runs_cut_at_max_for_each_profile` fails, because `new` refuses the profile.
  - Set the `DEFAULT` avg to 16384: `golden_cut_points` fails.
  - Build the zero chunk with `CHUNK_MAX` bytes, as the old process-wide one did: `two_profiles_in_one_process_read_back_byte_for_byte` fails with "16/64/256 KiB: splice key".

## Decisions Made

See `key-decisions` in the frontmatter. All are reversible (no deployed stores).

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 - Blocking] clippy `unusual_byte_groupings` in the tracer test**
- **Found during:** Task 1 (clippy gate)
- **Issue:** The seed literal `0x5eed_22` has unequal hex groups, so `-D warnings` fails.
- **Fix:** Changed it to `0x5eed_0022`, then reran the tracer tests (green).
- **Files modified:** crates/slicefs-cli/tests/chunk_profile_tests.rs
- **Commit:** 7dd2038

**2. [Rule 3 - Blocking] clippy `single_range_in_vec_init` in profile_tests.rs**
- **Found during:** Task 3 (clippy gate)
- **Issue:** `&[at..at + 4096]` as the changed list of `splice`.
- **Fix:** Changed it to `std::slice::from_ref(&changed)`.
- **Files modified:** crates/chunk-store/tests/profile_tests.rs
- **Commit:** 26625ea

### Other notes

- PATTERNS.md says the header tests at header.rs 697 and 799 must change. They did not change, and they stay green. The line `chunk-profile 16 64 256` has no class word as its first word, so it is still `Line::Other`.
- Guard mutation for the two-store test: the plan suggested "known_zero uses the default max" as an example. A zero chunk of the default size is the D-15 regression itself, and it fails the test directly, so I used it.
- `an_old_reader_reads_a_store_of_another_profile` has no guard mutation. It checks how a build with default options behaves, not code that this plan adds.
- `model.sh` also ran before the Task 2 commit, because the `OnceLock` field on `ChunkStore` lands in that task.
- Commit-protocol note: the branch `exec/22-01` is outside the generic `agent-*` allow-list. The launch instructions and plan rule 1 name this branch, and it is not protected, so the commits went on it.
- STATE.md, ROADMAP.md and REQUIREMENTS.md were not updated here. The orchestrator merges this branch and owns the shared planning state.

## Known Stubs

None. `CHUNK_PROFILE_RELEASE = "v2.2"` is a planned placeholder (D-10) until the owner names the release.

## Issues Encountered

None in the code. The golden values matched on the first run.

## Next Phase Readiness

- 22-02 can add the line and flag pairing rules (M9), `--chunk-profile` on seed and mount (through `check_write_with` and `check_profile`), `stats`, and the docs.
- 22-03 can build the benchmark on `Options { profile, .. }`.

## Self-Check: PASSED

- FOUND: crates/chunk-store/tests/profile_tests.rs, crates/slicefs-cli/tests/chunk_profile_tests.rs, runs/red-22-01.log, runs/gate-22-01.log
- FOUND commits: 7dd2038, 65202b3, 26625ea (ancestors of HEAD)
