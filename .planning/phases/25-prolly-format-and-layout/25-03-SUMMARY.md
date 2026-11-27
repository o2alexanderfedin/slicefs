---
phase: 25-prolly-format-and-layout
plan: 03
subsystem: store-format
tags: [store-header, format-8, upgrade-doc, v2.3.0-compat]
requires: []
provides:
  - "Store format 8 header (FORMAT_NUMBER 8, first line names prolly metadata maps)"
  - "Format 7 (release v2.3.0) refused at every open point with a message that names docs/user/upgrade.md"
  - "docs/user/upgrade.md, store-format.md format 8 text, M17, format policy (D-13)"
  - "Both-directions check with a real v2.3.0 binary (runs/v230-format-check.log)"
affects: [25-04, 27-migration]
status: complete
commits: 3
plan_head_before: f306898d8467be97aa724db42a3be9653cd2fcef
plan_head_after: a9a6a90a15862c95d072be3502de169d62ef7596
actuals:
  tokens: 7000
  tasks: 3
  commits: 3
tech-stack:
  added: []
  patterns: ["one explicit arm per older format number, with a compile-time assert on FORMAT_NUMBER"]
key-files:
  created:
    - docs/user/upgrade.md
    - .planning/phases/25-prolly-format-and-layout/runs/v230-format-check.log
    - .planning/phases/25-prolly-format-and-layout/runs/gate-25-03.log
  modified:
    - crates/slicefs-cli/src/header.rs
    - crates/slicefs-cli/src/stats.rs
    - crates/slicefs-cli/tests/store_header_tests.rs
    - docs/design/store-format.md
    - docs/design/chunk-store.md
key-decisions:
  - "The format 7 message says SliceFS v2.3.0 (v2.2.0 wrote format 6, so only v2.3.0 wrote format 7)"
  - "The new message is M17 in the store-format doc"
  - "docs/user/upgrade.md is created now; Phase 27 (#155) adds the copy steps to the same page"
requirements-completed: [PMAP-05]
---

# Phase 25 Plan 03: Store format 8 and the format 7 refusal Summary

New stores get the format 8 header, a format 7 store (release v2.3.0) is refused by every command with a message that names `docs/user/upgrade.md`, and a real v2.3.0 binary refuses a format 8 store.

## Tasks

| Task | Commit | What |
|------|--------|------|
| 1 (tracer) | 6287396 | `FORMAT_NUMBER` 8, new first line, `msg_flat_maps`, one arm per older number plus compile-time assert, tests for formats 6, 7 and 9, `docs/user/upgrade.md` |
| 2 | 20b342b | `runs/v230-format-check.log`: both directions with a real v2.3.0 binary |
| 3 | a9a6a90 | store-format.md (format 8 examples, parse order, M2/M3/M17, policy D-13), chunk-store.md, `runs/gate-25-03.log` |

## Verification

- `cargo test -p slicefs-cli --lib header`: 26 passed. `--lib stats`: 25 passed. `--test store_header_tests`: 10 passed, including `a_format_7_store_is_refused`, `a_format_6_store_is_refused`, `seed_writes_the_header_first`, `data_and_no_header_is_refused_at_every_open_point`.
- `runs/gate-25-03.log`: five sections, all `rc=0` (header tests, store_header_tests, clippy with `-D warnings -D clippy::allow_attributes_without_reason`, `scripts/safety/unsafe-count.sh`, `cargo test --workspace`).
- `runs/v230-format-check.log` (v2.3.0 built in Docker from a separate detached worktree of the tag, `--locked`):
  - v2.3.0 `stats` and `mount` of a format 8 store both exit 1 with `newer SliceFS (store format 8)`; `mounted=no`.
  - v2.3.0 `seed` makes a store whose FORMAT starts `slicefs-store 7`.
  - This build `stats` of that store exits 1 with the format 7 message and `docs/user/upgrade.md`; the FORMAT SHA-256 is equal before and after.
  - The tag worktree and the throwaway Docker volume were removed.

## Deviations from Plan

None in the code or docs. Notes:

- The log header line "v2.3.0" shows the commit `582a41d` (the merge commit that the tag points to), not the tag object.
- The log file has no separate `== this build seed of store8` command output besides the FORMAT first line; the check is the first line.
- The gate log keeps only the last lines of each command (the filtered output), not the full cargo output.

## Known Stubs

None.

## Threat Flags

None. Mitigations T-25-08, T-25-09, T-25-10 are covered by the logs and tests above.

## Self-Check: PASSED

Files exist (header.rs, stats.rs, store_header_tests.rs, upgrade.md, store-format.md, chunk-store.md, both logs); commits 6287396, 20b342b, a9a6a90 are on `exec/25-03`. STATE.md and ROADMAP.md were not changed.
