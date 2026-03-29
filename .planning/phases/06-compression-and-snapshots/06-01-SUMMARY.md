---
phase: 06-compression-and-snapshots
plan: "01"
subsystem: compression
tags: [compression, zstd, lz4, traits, wire-format]
dependency_graph:
  requires: []
  provides: [slicefs-traits/compressor, slicefs-compression]
  affects: [slicefs-cli, slicefs-fuse]
tech_stack:
  added: [zstd=0.13, lz4_flex=0.11, slicefs-compression crate]
  patterns: [impl Compressor for ZstdCompressor, impl Compressor for Lz4Compressor, impl Compressor for NoneCompressor]
key_files:
  created:
    - crates/slicefs-traits/src/compressor.rs
    - crates/slicefs-compression/Cargo.toml
    - crates/slicefs-compression/src/lib.rs
    - crates/slicefs-compression/src/zstd_compressor.rs
    - crates/slicefs-compression/src/lz4_compressor.rs
    - crates/slicefs-compression/src/none_compressor.rs
  modified:
    - crates/slicefs-traits/src/lib.rs
    - Cargo.toml
key_decisions:
  - "AlgorithmId::Raw used for incompressible data (compressed >= input): blocks never inflated"
  - "Empty input returns (Raw, []) from Zstd/Lz4: consistent with incompressible fast-path, avoids zstd frame overhead for 0-byte blocks"
  - "NoneCompressor rejects Zstd/Lz4 in decompress: explicit error surface for accidental cross-compressor reads"
  - "compress_block fallback to Raw on compress error: wire format always writable even if compression unavailable"
  - "parse_compressor panics on unknown name: CLI validation catches typos before any I/O"
  - "lz4_flex 0.11 (not 0.13): 0.13 not yet released on crates.io; 0.11 provides same compress_prepend_size API"
metrics:
  duration: "202s"
  completed_date: "2026-03-29"
  tasks_completed: 2
  files_changed: 8
---

# Phase 6 Plan 01: Compression Trait and Implementations Summary

Pluggable compression interface (Compressor trait + AlgorithmId enum) in slicefs-traits, plus concrete Zstd/LZ4/None implementations and wire-format helpers in a new slicefs-compression crate.

## What Was Built

### Task 1: Compressor trait and AlgorithmId in slicefs-traits (commit d865fb9)

- `AlgorithmId` enum (`#[repr(u8)]`): None=0x00, Zstd=0x01, Lz4=0x02, Raw=0x03 with `from_u8` round-trip
- `CompressorError` enum via thiserror: `Compress(String)` and `Decompress(String)` variants
- `Compressor` trait (Send+Sync): `compress`, `decompress`, `algorithm_id` methods
- Re-exported from `slicefs-traits` lib.rs as `AlgorithmId`, `Compressor`, `CompressorError`
- 4 tests: round-trip, invalid values, object-safety, error display — all pass

### Task 2: slicefs-compression crate (commit 80e8821)

- `ZstdCompressor { level: i32 }`: configurable level (default 3), falls back to Raw when compressed >= input
- `Lz4Compressor {}`: uses `compress_prepend_size`/`decompress_size_prepended`, falls back to Raw when incompressible
- `NoneCompressor {}`: always (AlgorithmId::None, verbatim), rejects Zstd/Lz4 in decompress
- `compress_block` / `decompress_block`: 1-byte AlgorithmId header + payload wire format
- `parse_compressor(name, level)`: factory for CLI use
- 32 tests covering all behaviors — all pass

## Deviations from Plan

### Auto-fixed Issues

None - plan executed exactly as written.

### Out-of-scope Issues Observed

Pre-existing compilation errors in `crates/metadata` (SnapshotRecord match arm not exhaustive) were present before this plan. Logged to deferred-items rather than fixed.

## Self-Check: PASSED

All 5 source files confirmed present on disk.
Both commits confirmed in git log: d865fb9, 80e8821.
