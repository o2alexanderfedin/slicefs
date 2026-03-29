---
phase: 07-cross-platform-and-production-hardening
plan: "02"
subsystem: cli
tags: [cli, stats, scrub, json, observability, integrity]
dependency_graph:
  requires: [metadata::segment::load_store_from_segments, metadata::store::DictMetadataStore, metadata::gc::collect_live_set, blockset::Dictionary]
  provides: [slicefs-stats-command, slicefs-scrub-command, global-json-flag]
  affects: [slicefs-cli]
tech_stack:
  added: [serde_json = "1", sha2-compress (scrub verification)]
  patterns: [offline store scan pattern, SHA-224 Merkle tree integrity check, clap global flag]
key_files:
  created:
    - crates/slicefs-cli/src/stats.rs
    - crates/slicefs-cli/src/scrub.rs
  modified:
    - Cargo.toml
    - crates/slicefs-cli/Cargo.toml
    - crates/slicefs-cli/src/cli.rs
    - crates/slicefs-cli/src/main.rs
    - crates/data-id/blockset/src/lib.rs
decisions:
  - "SHA-224 verification uses SHA224.compress directly (not blockset::compress): blockset::compress does data concatenation for small-length inputs, producing a non-hash result; all dictionary keys are always SHA-224 hashes so direct SHA224.compress is the correct verifier"
  - "blockset::compress exported as public API: needed for scrub integrity verification; avoids depending on sha2-compress directly for key re-derivation"
  - "Stats works on mounted stores (read-only scan, no lock refusal): allows monitoring live filesystems; warns on mount.lock"
  - "Scrub warns on mounted stores but continues: scans closed segments only; prints warning about active writes not being covered"
metrics:
  duration: "421s"
  completed_date: "2026-03-29"
  tasks_completed: 2
  files_changed: 7
---

# Phase 7 Plan 02: Global JSON Flag, Stats and Scrub Commands Summary

Added global `--json` flag and `stats`/`scrub` subcommands to the SliceFS CLI, completing CLI-03 (storage analytics), CLI-04 (integrity verification), and CLI-05 (structured JSON output for all commands).

## What Was Built

### Task 1: Global --json flag, Stats/Scrub subcommands, serde_json dependency (commit 7e2d3be)

- Added `serde_json = "1"` to workspace dependencies in root `Cargo.toml`
- Added `serde` and `serde_json` to `slicefs-cli/Cargo.toml`
- Added global `--json` flag to `Cli` struct using `#[arg(long, global = true)]`
- Added `Stats { store: PathBuf }` and `Scrub { store: PathBuf }` variants to `Cmd` enum
- Updated `main.rs` with `mod stats`, `mod scrub`, and match arms for both commands
- JSON error output for all existing commands (mount, unmount, seed, gc, snapshot)
- CLI tests: `test_stats_subcommand`, `test_scrub_subcommand`, `test_json_flag_global`, `test_json_flag_after_subcommand`, `test_no_json_flag_default` — all pass

### Task 2: run_stats and run_scrub implementations (commit 5557aa0)

**stats.rs:**
- Follows the gc.rs offline pattern: loads segments, allows mounted stores with note
- Computes: logical bytes, physical bytes (dict.len() * 92), dedup ratio, block count, snapshot count
- Compressor reported as "zstd (default)" (Phase 6 established zstd as default)
- Refcount distribution: queries `meta.get_refcount(key)` for each dict entry, buckets into unique/shared-2x/shared-3plus
- Per-snapshot stats: uses `collect_live_set` to count reachable blocks per snapshot
- Human-readable tabular output and `--json` via `serde_json::to_string_pretty`
- Serializable structs: `StoreStats`, `RefcountDist`, `SnapshotStats`

**scrub.rs:**
- Loads store from segments; warns on `mount.lock` but continues
- Verifies each `(key, [left, right])` dictionary entry by recomputing `SHA224.compress(left, right)[..7]` and comparing to stored key
- Key insight: all blockset Dictionary keys are SHA-224 hashes; `blockset::compress` does data concatenation for small inputs (not SHA-224), so direct `SHA224.compress` from `sha2-compress` crate is required for verification
- Reports corrupted blocks with stored key, expected key, and corruption type
- Returns `Ok(())` on clean store; returns `Err(...)` (exit 1) if any corruption found
- JSON output: `ScrubReport` with blocks_verified, corrupted_blocks, status, mounted, corrupted entries list

**blockset crate:**
- Added `pub use digest256::compress;` to blockset lib.rs (Rule 2: missing public API needed for integrity checking)

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Bug] Fix blockset scrub verification: blockset::compress != SHA224.compress for small inputs**
- **Found during:** Task 2 test `test_verify_fresh_dictionary_is_clean` panicking
- **Issue:** `blockset::compress(left, right)` does bit-level data concatenation when `len(left) + len(right) <= 248 bits` — it does NOT call SHA224 in that case. Dictionary entries created via `end()` use `SHA224.compress(x, EMPTY)` directly. So `blockset::compress` cannot be used for re-verification.
- **Fix:** Used `sha2_compress::SHA224.compress(left, right)` directly; all dictionary keys are SHA-224 hashes regardless of data length
- **Files modified:** `crates/slicefs-cli/src/scrub.rs`, `crates/slicefs-cli/Cargo.toml`
- **Commit:** 5557aa0

## Test Results

All tests pass:
- `cargo test -p slicefs-cli` — 74 tests passing (lib + bin), plus all integration test suites

## Self-Check

Files created/modified:
- `crates/slicefs-cli/src/stats.rs` — created, 185+ lines
- `crates/slicefs-cli/src/scrub.rs` — created, 170+ lines
- `crates/slicefs-cli/src/cli.rs` — updated with global --json, Stats/Scrub variants, 5 new tests
- `crates/slicefs-cli/src/main.rs` — updated with new modules and match arms
- `crates/data-id/blockset/src/lib.rs` — compress exported
- `Cargo.toml` — serde_json workspace dep added
- `crates/slicefs-cli/Cargo.toml` — serde, serde_json, sha2-compress added

Commits:
- 7e2d3be: feat(07-02): global --json flag, Stats/Scrub subcommands, serde_json dependency
- 5557aa0: feat(07-02): implement run_stats and run_scrub with human and JSON output

## Self-Check: PASSED
