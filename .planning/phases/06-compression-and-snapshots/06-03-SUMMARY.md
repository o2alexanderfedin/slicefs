---
phase: 06-compression-and-snapshots
plan: 03
subsystem: metadata
tags: [snapshots, wal, segment, persistence, gc]
dependency_graph:
  requires:
    - 05-crash-safety-and-gc/05-01 (SegmentEntry, SegmentWriter, SegmentReader)
    - 05-crash-safety-and-gc/05-02 (DictMetadataStore, WalStrategy, WalEntry)
    - 05-crash-safety-and-gc/05-04 (GarbageCollector, background GC)
  provides:
    - SnapshotRecord segment entry type (RecordType 0x03)
    - SnapshotEntry struct
    - DictMetadataStore snapshot CRUD methods
    - snapshot_roots() for GC multi-root anchoring
  affects:
    - slicefs-cli/src/mount.rs (set_snapshots after load_store_from_segments)
    - slicefs-cli/src/gc.rs (snapshot roots included in GC root set)
    - all callers of load_store_from_segments (3-tuple return)
tech_stack:
  added: []
  patterns:
    - WAL-as-snapshot-journal: snapshots are SegmentEntry::SnapshotRecord in WAL, no separate file
    - crash-safe by construction: snapshot durability comes from WAL write guarantees
    - version auto-increment: max(existing versions) + 1, starts at 1
key_files:
  created:
    - crates/metadata/src/snapshot.rs
  modified:
    - crates/metadata/src/segment/mod.rs
    - crates/metadata/src/segment/reader.rs
    - crates/metadata/src/segment/compaction.rs
    - crates/metadata/src/store.rs
    - crates/metadata/src/wal/mod.rs
    - crates/metadata/src/wal/per_op.rs
    - crates/metadata/src/lib.rs
    - crates/metadata/tests/segment_store_tests.rs
    - crates/slicefs-cli/src/mount.rs
    - crates/slicefs-cli/src/gc.rs
    - crates/slicefs-cli/tests/crash_recovery_tests.rs
    - crates/slicefs-cli/tests/fsync_tests.rs
    - crates/slicefs-cli/tests/gc_cli_tests.rs
decisions:
  - "SnapshotRecord as SegmentEntry variant (RecordType 0x03): crash-safe via WAL, no new file format needed"
  - "load_store_from_segments returns 3-tuple (dict, root, snapshots): snapshots reconstructed on replay"
  - "WalEntry::Snapshot variant: routes through wal_entry_to_segment to SegmentEntry::SnapshotRecord"
  - "snapshot_roots() collects all snapshot roots + current live root for GC multi-root anchoring"
  - "compaction.rs always keeps SnapshotRecord entries: GC must not reclaim snapshot-referenced blocks"
  - "Offline GC (gc.rs) now includes snapshot roots in GC root set"
metrics:
  duration: "8min"
  completed_date: "2026-03-29"
  tasks_completed: 2
  files_changed: 14
requirements:
  - SNAP-01
  - SNAP-02
  - SNAP-03
---

# Phase 6 Plan 3: Snapshot Infrastructure Summary

Snapshot persistence via WAL segment entries — crash-safe snapshots with CRUD, GC multi-root anchoring, and full segment replay support.

## What Was Built

### Task 1: SnapshotRecord Segment Entry and SnapshotEntry Struct

- `crates/metadata/src/snapshot.rs` — `SnapshotEntry { version, name, root, created_at }` struct
- `SegmentEntry::SnapshotRecord` variant added to `segment/mod.rs` with `RecordType::SnapshotRecord = 0x03`
- Payload format: `version(8) + root(28) + created_at(8) + name_len(4) + name_bytes(variable)` — minimum 48 bytes
- `parse_snapshot_record()` and `as_snapshot_entry()` methods on `SegmentEntry`
- `SegmentReader` iterator updated to decode and yield `SnapshotRecord` entries
- `load_store_from_segments()` signature changed from 2-tuple to 3-tuple: `(Dictionary, Option<Digest224>, Vec<SnapshotEntry>)`
- `compaction.rs` updated to always preserve `SnapshotRecord` entries during GC compaction
- All 9 callers of `load_store_from_segments` updated (mount.rs, gc.rs, 5 test files)

### Task 2: DictMetadataStore Snapshot Methods

- `snapshots: Mutex<Vec<SnapshotEntry>>` field added to `DictMetadataStore`
- `set_snapshots(Vec<SnapshotEntry>)` — loads snapshot list during store reconstruction from segments
- `create_snapshot(name) -> Result<SnapshotEntry>` — calls `commit()`, auto-increments version, writes `SnapshotRecord` to WAL, appends to in-memory list
- `list_snapshots() -> Vec<SnapshotEntry>` — returns all snapshots sorted by version
- `find_snapshot(reference) -> Option<SnapshotEntry>` — lookup by version string or name
- `snapshot_roots() -> Vec<Digest224>` — all snapshot roots + current live root (for GC)
- `WalEntry::Snapshot` variant added; `wal_entry_to_segment()` maps it to `SegmentEntry::SnapshotRecord`
- `mount.rs::load_store` calls `meta.set_snapshots(snapshots)` after reconstruction
- `gc.rs::run_gc` now adds snapshot roots to the GC root set

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 - Blocker] Missing slicefs-compression/src/lib.rs**
- **Found during:** Task 1 — workspace build failed before any tests could run
- **Issue:** `slicefs-compression` crate existed with source files but no `lib.rs`, causing `cargo test -p metadata` to fail
- **Fix:** Created minimal `lib.rs` re-exporting the three compressor types; a subsequent linter pass enhanced it with `compress_block`, `decompress_block`, `parse_compressor` helpers and 32 tests
- **Files modified:** `crates/slicefs-compression/src/lib.rs`
- **Commit:** a9f1400 (included in Task 1 commit)

**2. [Rule 2 - Auto-fix] compaction.rs non-exhaustive match**
- **Found during:** Task 1 — `SegmentEntry::SnapshotRecord` added as new variant, `compaction.rs` match was not exhaustive
- **Fix:** Added `SnapshotRecord` arm that always keeps the entry (snapshot records must survive GC compaction)
- **Files modified:** `crates/metadata/src/segment/compaction.rs`
- **Commit:** a9f1400

**3. [Rule 2 - Auto-fix] Offline GC did not include snapshot roots**
- **Found during:** Task 1 caller update — `gc.rs` only included the live root; snapshot roots were missing from GC root set
- **Fix:** Added snapshot root collection loop in `run_gc` alongside the `root_opt.into_iter()` call
- **Files modified:** `crates/slicefs-cli/src/gc.rs`
- **Commit:** a9f1400

## Self-Check: PASSED

- `crates/metadata/src/snapshot.rs` — FOUND
- `crates/metadata/src/segment/mod.rs` — FOUND (SnapshotRecord variant present)
- Commit a9f1400 — FOUND
- Commit 51317b4 — FOUND
- All metadata tests pass: `cargo test -p metadata` — 105 tests, 0 failures
- Workspace builds cleanly: `cargo build --workspace` — no errors
