---
phase: 05-crash-safety-and-gc
plan: "01"
subsystem: metadata
tags: [segment-format, wal, crash-safety, persistence]
dependency_graph:
  requires: []
  provides:
    - segment file format (SLSG magic, 16-byte header, framed records)
    - SegmentWriter / SegmentReader
    - WalStrategy trait with 4 implementations
    - WalEntry / WalError / WalConfig
  affects:
    - crates/metadata (new segment + wal modules)
tech_stack:
  added: []
  patterns:
    - append-only segment framing (type + payload_len + payload)
    - crash-tolerant reader (skip truncated + unknown records)
    - pluggable WAL strategy trait (Strategy pattern)
    - Mutex<SegmentWriter> for shared mutable segment access
key_files:
  created:
    - crates/metadata/src/segment/mod.rs
    - crates/metadata/src/segment/writer.rs
    - crates/metadata/src/segment/reader.rs
    - crates/metadata/src/wal/mod.rs
    - crates/metadata/src/wal/no_wal.rs
    - crates/metadata/src/wal/per_op.rs
    - crates/metadata/src/wal/flush_on_fsync.rs
    - crates/metadata/src/wal/periodic.rs
    - crates/metadata/tests/segment_tests.rs
    - crates/metadata/tests/wal_tests.rs
  modified:
    - crates/metadata/src/lib.rs (added pub mod segment; pub mod wal;)
    - crates/metadata/Cargo.toml (added tempfile dev-dep)
decisions:
  - "SegmentEntry defined in segment/mod.rs (not reader.rs) — single source of truth; reader uses super::SegmentEntry"
  - "PerOpWal uses Mutex<SegmentWriter> — WalStrategy requires Send+Sync; Mutex provides interior mutability"
  - "FlushOnFsyncWal::shutdown_without_flush() test helper exposes drop-without-flush for buffer verification test"
  - "PeriodicWal structurally identical to FlushOnFsyncWal — background timer wiring deferred to Plan 04 GC thread"
  - "WalError wraps io::Error via thiserror #[from] — clean conversion at all call sites"
metrics:
  duration: 8min
  completed: "2026-03-27"
  tasks_completed: 2
  files_changed: 12
---

# Phase 5 Plan 01: Segment File Format and WAL Strategy Summary

Append-only segment I/O layer (SLSG magic, crash-tolerant reader, 4 WAL strategy implementations) with full TDD test coverage for the crash-safety foundation.

## What Was Built

### Task 1: Segment File Format

The segment file is the durable storage unit replacing `dictionary.bin`. Format:

- 16-byte header: magic `[0x53,0x4C,0x53,0x47]` ("SLSG") + version `1` (u32 LE) + segment_id (u64 LE)
- Records framed as: `record_type (u8)` + `payload_len (u32 LE)` + `payload bytes`
- `DictEntry` payload: 92 bytes (Digest224 as 7×u32 LE + Branches as 2×8×u32 LE)
- `RootUpdate` payload: 28 bytes (Digest224 as 7×u32 LE)
- `EofMarker` (0xFF): terminates iteration

`SegmentWriter` creates the file with header and appends records. `close()` writes an EOF marker before syncing. `SegmentReader` implements `Iterator<Item=SegmentEntry>` with crash-tolerant parsing: unknown record types are skipped by seeking `payload_len` bytes forward; truncated reads return `None` stopping iteration cleanly.

### Task 2: WAL Strategy Trait

`WalStrategy` trait unifies four durability modes:

| Implementation  | Durability            | I/O on log_mutation |
|-----------------|---------------------- |---------------------|
| `PerOpWal`      | Per mutation (sync)   | write + sync_all    |
| `FlushOnFsyncWal` | On flush_and_sync  | none (buffered)     |
| `PeriodicWal`   | On shutdown/timer     | none (buffered)     |
| `NoWal`         | None                  | none                |

`PerOpWal` holds `Mutex<SegmentWriter>` and syncs after every `log_mutation`. `FlushOnFsyncWal` and `PeriodicWal` hold a `Mutex<Vec<WalEntry>>` buffer; `flush_and_sync` drains the buffer and writes all entries. `create_wal` factory converts `WalConfig` to `Box<dyn WalStrategy>`.

## Test Coverage

- **Segment tests (6):** round-trip DictEntry (×3), RootUpdate round-trip, truncated record tolerance, unknown type skip (0xFE), empty segment, header magic/version validation
- **WAL tests (9):** NoWal all-Ok, PerOpWal DictEntry write+readback, PerOpWal RootUpdate write+readback, PerOpWal flush, FlushOnFsyncWal buffer (no-disk), FlushOnFsyncWal flush writes, PeriodicWal shutdown flushes

Total metadata tests: 89 existing + 6 segment + 9 WAL = **104 tests, all passing**. Full workspace: **no regressions**.

## Commits

| Hash      | Description                                                |
|-----------|------------------------------------------------------------|
| `8046d06` | feat(05-01): implement segment file format writer and reader |
| `06aa9a0` | feat(05-01): implement WalStrategy trait with 4 implementations |

## Deviations from Plan

None — plan executed exactly as written.

## Self-Check

Files created:
- crates/metadata/src/segment/mod.rs
- crates/metadata/src/segment/writer.rs
- crates/metadata/src/segment/reader.rs
- crates/metadata/src/wal/mod.rs
- crates/metadata/src/wal/no_wal.rs
- crates/metadata/src/wal/per_op.rs
- crates/metadata/src/wal/flush_on_fsync.rs
- crates/metadata/src/wal/periodic.rs
- crates/metadata/tests/segment_tests.rs
- crates/metadata/tests/wal_tests.rs
