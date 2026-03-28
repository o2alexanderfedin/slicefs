---
phase: 03-read-only-fuse
plan: "02"
subsystem: slicefs-cli / metadata
tags: [seed, cdc, cas, dictionary, persistence]
dependency_graph:
  requires: ["03-01"]
  provides: ["seed-command", "dict-accessor"]
  affects: ["metadata", "slicefs-cli"]
tech_stack:
  added: []
  patterns: ["State::push_all for CDC chunking", "serialize_dictionary/deserialize_dictionary round-trip", "DictMetadataStore dict() accessor for shared-dictionary pattern"]
key_files:
  created:
    - crates/slicefs-cli/src/seed.rs
  modified:
    - crates/metadata/src/store.rs
    - crates/slicefs-cli/src/main.rs
decisions:
  - "dict() accessor exposes &Mutex<Dictionary> with deadlock warning doc comment"
  - "Seed uses shared Dictionary (not separate): file content and metadata co-resident in same dict"
  - "Seed skips symlinks and non-regular-file entries silently (out of scope for read-only phase)"
  - "Directory entries sorted by file_name for deterministic Dictionary output"
metrics:
  duration: "~8 minutes"
  completed_date: "2026-03-28"
  tasks_completed: 2
  files_changed: 3
---

# Phase 03 Plan 02: Seed Command Summary

Seed command implemented as `slicefs seed <store> <source-dir>`, validating the end-to-end CAS pipeline: read files, CDC chunk via `State::push_all`, build inodes and directory records in `DictMetadataStore`, serialize to `dictionary.bin` + `root.bin`.

## Tasks Completed

| Task | Name | Commit | Files |
|------|------|--------|-------|
| 1 | Expose dict() accessor on DictMetadataStore | b0a1c36 | crates/metadata/src/store.rs |
| 2 | Seed command implementation | 8548578 | crates/slicefs-cli/src/seed.rs, src/main.rs |

## What Was Built

**Task 1 — `dict()` accessor (metadata crate):**
- Added `pub fn dict(&self) -> &Mutex<Dictionary>` to `DictMetadataStore`
- Includes deadlock warning: callers must not hold this lock while calling other store methods
- Added `test_dict_accessor_content_survives_reload` verifying content pushed into the dict survives a full serialize/deserialize/load_from_root cycle

**Task 2 — `seed::run_seed()` (slicefs-cli):**
- `run_seed(store_path, source_dir)` creates the store directory, walks source recursively, and writes `dictionary.bin` + `root.bin`
- `walk_dir()` processes entries sorted by name for determinism; files call `seed_file()`, subdirectories call `create_directory()` then recurse
- `seed_file()` reads bytes, calls `State::push_all` inside a `dict().lock()` scope (lock dropped before returning), creates inode, links, sets manifest
- `file_inode_meta()` / `dir_inode_meta()` capture Unix mode/uid/gid/mtime on Unix; fall back to defaults on non-Unix
- `Cmd::Seed` arm in `main.rs` replaced `todo!` stub with `seed::run_seed()`

## Verification

```
cargo test -p metadata -- test_dict_accessor     # 1 test OK
cargo test -p slicefs-cli -- seed                 # 5 tests OK (cli::seed + 4 seed::tests)
cargo test --workspace                            # 195 tests OK, 0 failures
```

## Deviations from Plan

**1. [Rule 2 - Missing functionality] Removed unused imports to eliminate warnings**
- Found during: Task 2
- Issue: `Dictionary`, `deserialize_dictionary`, and outer-scope `MetadataExt` were imported in seed.rs but not used at the top-level (only needed in tests)
- Fix: Removed `Dictionary` and `deserialize_dictionary` from the top-level imports; kept `PermissionsExt` only; `MetadataExt` used inline via unix cfg block
- Files modified: crates/slicefs-cli/src/seed.rs
- Commit: 8548578 (same task commit)

Pre-existing dead_code/unused warnings in `filesystem.rs` and `store_io.rs` (from plan 01 stubs) are out of scope and logged to deferred items.

## Self-Check: PASSED

Files verified:
- crates/slicefs-cli/src/seed.rs: exists
- crates/metadata/src/store.rs: dict() accessor present
- .planning/phases/03-read-only-fuse/03-02-SUMMARY.md: this file

Commits verified:
- b0a1c36: feat(03-02): expose dict() accessor on DictMetadataStore
- 8548578: feat(03-02): implement seed subcommand for CAS store population
