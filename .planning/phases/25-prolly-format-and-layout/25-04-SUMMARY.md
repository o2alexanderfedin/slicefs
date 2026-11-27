---
phase: 25-prolly-format-and-layout
plan: 04
subsystem: store-format
tags: [store-header, boundary-salt, seed, ro_compat-flag]
requires: ["25-01", "25-03"]
provides:
  - "Header line `boundary-salt <64 lowercase hex>` with the ro_compat flag `boundary-salt v2.4`"
  - "`slicefs seed --boundary-salt random|HEX` on a new store; M18 on an existing store"
  - "`Header::boundary_salt()` for Phase 26 to pass to `prolly::Config`"
affects: [26-prolly-maps]
status: complete
commits: 3
plan_head_before: 59d85ae3df24e1a08cf0b18ac4595acffc39b418
plan_head_after: 2fa5063c2ac23b5cede78c0ce33f8a6b763c57d3
actuals:
  tokens: 11400
  tasks: 3
  commits: 3
tech-stack:
  added: []
  patterns: ["a setting line always comes with its own flag (same shape as chunks / chunk-profile)", "old entry points keep their signature and call the new one with None"]
key-files:
  created:
    - crates/slicefs-cli/tests/boundary_salt_tests.rs
    - .planning/phases/25-prolly-format-and-layout/runs/gate-25-04.log
  modified:
    - crates/slicefs-cli/src/header.rs
    - crates/slicefs-cli/src/content.rs
    - crates/slicefs-cli/src/seed.rs
    - crates/slicefs-cli/src/cli.rs
    - crates/slicefs-cli/src/main.rs
    - docs/design/store-format.md
key-decisions:
  - "Flag class ro_compat: a build that does not know it can read the trees (reading does not use the salt) but must not write them (it would cut with another rule); same reasoning as chunk-profile"
  - "Release word v2.4"
  - "`random` is refused on an existing store even when the store has a salt"
  - "run_seed_with, check_write_with and marker_with keep their signatures and pass no salt"
  - "No salt row in `stats`; the Feature flags row and the JSON flags list the flag"
  - "No dependency on the prolly crate from slicefs-cli in this phase"
requirements-completed: [PMAP-05, PMAP-04]
---

# Phase 25 Plan 04: Per-store boundary salt Summary

A new store can carry a boundary salt (`seed --boundary-salt random|HEX`) in a `boundary-salt` header line with its own ro_compat flag; an existing store refuses any other salt (M18) and its header does not change.

## Tasks

| Task | Commit | What |
|------|--------|------|
| 1 (tracer) | 2f0b1d1 | `BOUNDARY_SALT_FLAG` / `BOUNDARY_SALT_RELEASE`, `Line::BoundarySalt`, `Header::boundary_salt`, `marker_with_salt`; `SaltOpt`, `random_salt` (`/dev/urandom`, `read_exact`), `check_write_with_salt`; `run_seed_with_salt`; 3 integration tests |
| 2 | 0ec72ce | `--boundary-salt` on `seed` (not `mount`); header unit tests; 4 more integration tests (M18 both ways, parallel seeds, stats flag) |
| 3 | 2fa5063 | `docs/design/store-format.md` (salt line, example header, grammar, pairing, flag row, five M9 reasons, M18 row); `runs/gate-25-04.log` |

The grammar errors and the pairing checks (`parse`) and M18 (`msg_salt_differs`, `check_salt`) were written together with Task 1 because they share `header.rs` and `content.rs`; Task 2 adds their tests and the CLI option.

## Tests

- `boundary_salt_tests`: 7 passed (`two_parallel_salted_seeds_make_one_salt` also run 8 times alone, all passed).
- `slicefs-cli --lib header`: 30 passed (new: `boundary_salt_line_errors`, `boundary_salt_pairing`, `marker_with_salt_has_the_line_and_the_flag`, `a_rewrite_keeps_the_boundary_salt_line`); `cli::tests::test_boundary_salt_flag` passed.
- `chunk_profile_tests`, `store_header_tests`, `rehash_tests`: pass (old callers unchanged); `prolly-bench` builds.
- `cargo +stable clippy --locked -p slicefs-cli --all-targets -- -D warnings -D clippy::allow_attributes_without_reason`: clean. `scripts/safety/unsafe-count.sh`: rc=0. `cargo test --workspace`: rc=0. All six gates are `rc=0` in `runs/gate-25-04.log`.
- A run of the built binary: `seed --boundary-salt random` wrote the line and flag; the same option again gave M18; `--boundary-salt ''` was refused at parse.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Lint] clippy `chunks_exact_to_as_chunks` in `parse_salt_hex`**
- **Found during:** Task 3 gate run
- **Fix:** an index loop over the 32 output bytes instead of `chunks_exact(2)`
- **Files modified:** crates/slicefs-cli/src/header.rs
- **Commit:** 2fa5063

### Other notes

- `parse_salt_hex` is `pub` (the plan listed it as private) because the CLI option parser in `content.rs` uses the same validator, so the option and the header accept the same 64 digits.
- The workspace section of the gate log lists only the `Running` and `test result` lines (the full output is large); the other five sections hold the full output.
- No `check_salt` race gap: after `check_with` the returned header is checked again against the salt that was written, in case a parallel create installed a header after the first read.

## Known Stubs

None.

## Threat Flags

None. The only new surface is the `seed` option and the header line, both in the plan's threat model (T-25-11 to T-25-14 mitigated; T-25-15 accepted).

## Self-Check: PASSED

- Files exist: `boundary_salt_tests.rs`, `gate-25-04.log`, `25-04-SUMMARY.md`.
- Commits 2f0b1d1, 0ec72ce, 2fa5063 are on `exec/25-04`.
