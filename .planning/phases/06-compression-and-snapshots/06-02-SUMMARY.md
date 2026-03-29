---
phase: 06-compression-and-snapshots
plan: "02"
subsystem: compression-fuse-integration
tags: [compression, zstd, lz4, fuse, write-path, read-path, migration, cli]
dependency_graph:
  requires: [slicefs-traits/compressor, slicefs-compression]
  provides: [slicefs-cli/filesystem compression, mount --compressor flag]
  affects: [slicefs-cli]
tech_stack:
  added: [slicefs-compression dependency in slicefs-cli]
  patterns: [to_wire_bytes/from_wire_bytes helpers, store_version gating, pre-Phase-6 fallback]
key_files:
  created:
    - crates/slicefs-cli/tests/compression_tests.rs
  modified:
    - crates/slicefs-cli/Cargo.toml
    - crates/slicefs-cli/src/filesystem.rs
    - crates/slicefs-cli/src/cli.rs
    - crates/slicefs-cli/src/mount.rs
    - crates/slicefs-cli/src/main.rs
    - crates/slicefs-cli/src/snapshot.rs
    - crates/slicefs-cli/tests/write_path_tests.rs
    - crates/slicefs-cli/tests/dir_link_tests.rs
    - crates/slicefs-cli/tests/statfs_tests.rs
    - crates/slicefs-cli/tests/fsync_tests.rs
    - crates/slicefs-cli/tests/crash_recovery_tests.rs
    - crates/slicefs-cli/tests/posix_compliance_tests.rs
key_decisions:
  - "store_version gates both write and read paths: <2 = raw, >=2 = compression header"
  - "to_wire_bytes / from_wire_bytes helpers centralize version-gating logic across all write/read sites"
  - "from_wire_bytes falls back to raw on decompress failure: handles mixed old/new blocks in migrated stores"
  - "NoneCompressor + store_version=1 as default for all existing tests: exact backward compatibility"
  - "inode.size always reflects raw (uncompressed) byte count, not wire bytes length"
  - "Dedup within same compressor: Digest224 is computed on compressed wire bytes"
metrics:
  duration: "863s"
  completed_date: "2026-03-29"
  tasks_completed: 2
  files_changed: 12
---

# Phase 6 Plan 02: Compression Wire-up in FUSE Data Path Summary

Compression integrated end-to-end into the FUSE filesystem: blocks compressed with Zstd/LZ4/None before Dictionary storage and transparently decompressed on read, with store_version gating for pre-Phase-6 backward compatibility and --compressor/--compressor-level CLI flags.

## What Was Built

### Task 1: Compression in filesystem write and read paths (commit 3c2fe19)

- `SliceFsFilesystem` now holds `compressor: Arc<dyn Compressor>` and `store_version: u32` fields
- `SliceFsFilesystem::new()` updated to accept compressor and store_version (5 parameters)
- `to_wire_bytes(raw)`: applies `compress_block` when `store_version >= 2`, else passthrough
- `from_wire_bytes(wire)`: applies `decompress_block` when `store_version >= 2`, falls back to raw on error
- All write paths updated: `flush_buffer_to_cas`, `flush_buffer_for_fsync`, `create_symlink_impl`, `test_setattr_size` (Case B)
- All read paths updated: FUSE `read()`, `test_read()`, `simulate_readlink()`
- `test_read()` helper added: bypasses FUSE machinery for integration tests
- Pre-Phase-6 blocks (store_version < 2): no header in read or write; exact round-trip preserved
- Pre-Phase-6 blocks in migrated store (store_version=2 with old data): graceful fallback on decompress failure
- 11 compression integration tests covering all three compressors, dedup, offset slicing, symlink, truncate, and backward compat

### Task 2: CLI flags for compressor selection and level

Note: the `--compressor` and `--compressor-level` flags were added to cli.rs in the prior plan 06-04 execution (commit 409b3de) along with the Snapshot subcommand. This task verified they work correctly and added 4 CLI parsing tests:

- `test_mount_default_compressor_is_zstd`: default compressor is "zstd" when flag omitted
- `test_mount_compressor_lz4`: `--compressor lz4` parses correctly
- `test_mount_compressor_none`: `--compressor none` parses correctly
- `test_mount_compressor_zstd_with_level`: `--compressor zstd --compressor-level 9` parses correctly

`run_mount` accepts `compressor_name: &str` and `compressor_level: Option<i32>`, calls `parse_compressor` to construct the compressor, wraps it in `Arc`, and passes to `SliceFsFilesystem::new`.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 - Blocking] Snapshot arm missing in main.rs**
- **Found during:** Task 2 (initial compilation)
- **Issue:** cli.rs (from plan 06-03) added `Cmd::Snapshot` variant but main.rs had no match arm for it, causing non-exhaustive pattern error
- **Fix:** Added `Cmd::Snapshot { action }` arm in main.rs; added `run_snapshot` dispatcher to snapshot.rs
- **Files modified:** `src/main.rs`, `src/snapshot.rs`
- **Commit:** Included in Task 1 and Task 2 commits

**2. [Rule 1 - Bug] store_version=1 should skip compression on write path**
- **Found during:** Task 1 GREEN phase testing (2 failing tests)
- **Issue:** `compress_block` was called unconditionally, adding a 0x00 header even for NoneCompressor + store_version=1, causing raw-byte tests to get a leading 0x00 byte
- **Fix:** Gated `compress_block` on `store_version >= 2` via `to_wire_bytes` helper; both write and read paths respect the version
- **Files modified:** `src/filesystem.rs`
- **Commit:** 3c2fe19

**3. [Observation] Task 2 CLI work pre-existed in plan 06-04 commits**
- Prior plan 06-04 execution committed CLI flags, run_mount signature, and snapshot.rs before this plan ran. Task 2 verified correctness and added missing CLI tests.

## Self-Check: PASSED
