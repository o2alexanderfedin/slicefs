---
phase: 25-prolly-format-and-layout
plan: 01
subsystem: prolly
tags: [prolly, boundary-rule, fixed-point, node-header, rule-version, salt]
requires: []
provides:
  - integer cut rule (`threshold`, `exp_neg_fp`) over a generated table
  - 10-byte node header with `RULE_VERSION`
  - `Config::new`, `Config::READ`, `MAX_NODE_MAX`, salt (`with_salt`, `boundary_hash_keyed`)
  - `forced_cuts` in `BuildStats` and `ApplyStats`
affects: [25-02, 25-04, 25-07, 25-08]
tech-stack:
  added: []
  patterns: [generated-table-with-check, golden-pins-with-print-mode]
key-files:
  created:
    - scripts/prolly-table/gen_table.py
    - crates/prolly/src/rule_table.rs
    - .planning/phases/25-prolly-format-and-layout/runs/gate-25-01.log
  modified:
    - crates/prolly/src/rule.rs
    - crates/prolly/src/lib.rs
    - crates/prolly/src/node.rs
    - crates/prolly/src/tree.rs
    - crates/prolly/src/apply.rs
    - crates/prolly/src/tests.rs
decisions:
  - "Table step 2^-10 (23 + 1,025 entries); max measured error 1.19e-7 (limit 2^-22)"
  - "L may be any value in 256..=16,384 (u128 division is exact; no power-of-two rule)"
  - "Rule version byte lives in the node header only, not in the root record"
  - "Readers refuse an unknown rule version with Error::RuleVersion"
  - "Readers accept nodes up to MAX_NODE_MAX = 32 KiB (Config::READ)"
  - "Constants stay the Dolt values (L 4,096, NODE_MAX 16 KiB); decided in 25-07, pinned in 25-08"
metrics:
  completed: 2026-11-26
status: complete
actuals:
  tokens: 13500
  tasks: 3
  commits: 3
plan_head_before: f306898d8467be97aa724db42a3be9653cd2fcef
plan_head_after: 4e2930b5ccc827b2b3b38d72465e8be0a5b357c8
---

# Phase 25 Plan 01: Integer boundary rule, rule version byte, bench configs Summary

The float boundary test of `crates/prolly` is now an integer test over a committed, generated table; every node carries a rule version byte; `Config::new`, a keyed-BLAKE3 salt and forced-cut counters are in place for the bench.

## What was built

- **Task 1 (tracer), `f27b88f`.** `scripts/prolly-table/gen_table.py` (Python `decimal`, 60 digits, `--check FILE`) writes `rule_table.rs` (`EXP_INT` 23 entries, `EXP_FRAC` 1,025 entries, scaled by 2^32). `rule::threshold` = `2^32 - exp_neg_fp(delta)` with `delta = ((after^4 - before^4) << 32) / L^4` in u128; `Splitter::push` compares `hash as u64 < threshold`. The float helpers are gone; `rule.rs` has no float type. `golden_root` pins the table hash, eight thresholds, the 58 leaf entry counts, the root and the unsalted boundary hashes; `PROLLY_PRINT_GOLDENS=1` prints them as `pin!(...)` lines.
- **Task 2, `b658cfb`.** `NODE_HEADER` = 10: `SPT1`, `RULE_VERSION` (1), level, count. `decode` returns `Error::RuleVersion { found }` before reading entries; `get`, `iter`, `walk` and `apply` all load through `decode`, and `apply` writes nothing then.
- **Task 3, `4e2930b`.** `Config::new(l, node_max)`, `Config::READ`, `MAX_NODE_MAX`, `with_salt`/`salt`, `boundary_hash_keyed` (BLAKE3 `new_keyed`), `Splitter::forced`, and `forced_cuts` in `BuildStats` (every level) and `ApplyStats` (including the new top levels from the builder). Readers use `Config::READ`.

## Results

- Fixed-point vs float: max error 1.19e-7 (limit 2.4e-7); 0 of 1,000,000 cut decisions differ (limit 4); configs DEFAULT, SMALL, (8192, 16384), (8192, 32768).
- Root of `u64_entries(5_000, 7)` under DEFAULT: `1ec2d422...b220`, depth 3, 58 leaves. The 58 leaf counts are the same as under the float rule: the integer rule makes the same cuts on this input; the root changed only for the 10-byte header.
- Gate log `runs/gate-25-01.log`: six sections, all `rc=0` (prolly tests, bench build, bench smoke, clippy with `-D warnings -D clippy::allow_attributes_without_reason`, unsafe count, `cargo test --workspace`).
- `gen_table.py --check` exits 0.

## Deviations from Plan

**1. [Rule 1 - Plan error] A must-have value is wrong by arithmetic.** The plan says `threshold(Config::new(16384, 32768), 0, 32768)` returns 2^32. There `delta = (32768/16384)^4 = 16`, below 23, so the result is `2^32 - e^-16 * 2^32` = 4,294,966,813 (about 2^32 - 483), not 2^32. `threshold_edges` asserts `2^32 - 1000 < t < 2^32` for this case, and 2^32 exactly for `Config::new(256, 32768)` (`delta = 65,536`). No panic with overflow checks on in both cases. Plan 25-08 must not copy the value 2^32 for L = 16,384.
- **Files:** `crates/prolly/src/tests.rs`. **Commit:** `4e2930b`.

**2. [Rule 3 - Blocking] `golden_root` in print mode prints and does not assert.** The plan says print before asserting. With asserts on, the first mismatch hides the later values and a re-pin would need many runs. So with `PROLLY_PRINT_GOLDENS` set, every `pin!` prints its value in the form of the assert and nothing is asserted. Without the variable, all pins assert. **Commit:** `f27b88f`.

**3. [Rule 1 - Consequence] Test data for the node size limit.** Readers now accept nodes up to `MAX_NODE_MAX` (32 KiB), so `malformed_node_is_an_error` uses 24 entries of 1,500 bytes (36,000 bytes) for "node above the limit". **Commit:** `4e2930b`.

**4. [Rule 2 - Safety] `SIZE_CLAMP` in `threshold`.** `before` and `after` are clamped to 2^20 before the fourth power, so a crafted huge size cannot overflow u128. No result changes: at that size `delta` is far above 23 for every config.

**5. Test-only helper.** `Splitter::at(size, count)` (`#[cfg(test)]`) lets `threshold_edges` test the exact size `force_at` and `force_at + 1`.

**6. Test sizes.** `fixed_point_matches_float` uses 50,000 random pairs per config (200,000 total over four configs) and 250,000 decisions per config (1,000,000 total), to keep the debug test short. `wide_config_tree_reads` also builds a `Config::new(16384, 32768)` tree and asserts that a node above 16 KiB appears and reads back.

No auth gates. No architectural changes.

## Discretion choices (as listed in the plan objective)

Table step 2^-10; any L in 256..=16,384; version byte in the node header only; readers refuse an unknown version; readers accept up to 32 KiB; constants stay the Dolt values for now.

## Notes for later plans

- `rustfmt` for the project's style was not available as a repo config (no `rustfmt.toml` at the root; the installed rustfmt would reformat untouched files), so no formatter was run; clippy is clean.
- The salt is exercised in the library only; the store-level salt and the header line are plan 25-04.
- Goldens (`golden_root`) must be re-pinned in plan 25-08 after the constants are decided (`PROLLY_PRINT_GOLDENS=1 cargo test -p prolly --lib -- --exact tests::golden_root --nocapture`).
- The Linux run of `golden_root` (`scripts/docker-linux-test/run.sh`) was not run here (no mount or Docker work in this plan); it is plan 25-08.

## Known Stubs

None.

## Threat Flags

None. Threat register: T-25-01 (rule version refused, no write: `rule_version_is_refused`), T-25-03 (u128 math, 32 KiB cases in a debug build: `threshold_edges`), T-25-04 (`gen_table.py --check` and the table hash in `golden_root`) are mitigated; T-25-SC: no package or Cargo file change.

## Self-Check: PASSED

- Files exist: `scripts/prolly-table/gen_table.py`, `crates/prolly/src/rule_table.rs`, `runs/gate-25-01.log`.
- Commits `f27b88f`, `b658cfb`, `4e2930b` are on `exec/25-01`.
