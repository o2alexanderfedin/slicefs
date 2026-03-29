---
phase: 05-crash-safety-and-gc
plan: "03"
subsystem: metadata-gc
tags: [gc, compaction, background-thread, mark-and-sweep]
dependency_graph:
  requires: ["05-01"]
  provides: ["GC-01", "GC-02", "GC-03"]
  affects: ["crates/metadata/src/gc", "crates/metadata/src/segment/compaction.rs"]
tech_stack:
  added: []
  patterns: ["mark-and-sweep GC", "atomic rename compaction", "Weak<T> thread lifecycle"]
key_files:
  created:
    - crates/metadata/src/gc/mod.rs
    - crates/metadata/src/gc/background.rs
    - crates/metadata/src/segment/compaction.rs
    - crates/metadata/tests/gc_tests.rs
  modified:
    - crates/metadata/src/lib.rs
    - crates/metadata/src/segment/mod.rs
    - crates/metadata/src/store.rs
decisions:
  - "blockset::to_digest224 used to convert Digest256 children to Digest224 keys in mark_reachable"
  - "current_root() added to DictMetadataStore using Mutex<Option<Digest224>> last_root field updated by commit()"
  - "compact_segment writes to .tmp file then atomic rename — crash during write leaves original intact"
  - "GcHandle Drop sets shutdown flag but does not join — avoids blocking in drop()"
  - "Background GC uses count_orphans heuristic (dict.len()) for Phase 5 — refcount-based orphan count deferred"
metrics:
  duration: "~20 min"
  completed_date: "2026-03-29"
  tasks_completed: 2
  files_changed: 7
---

# Phase 5 Plan 3: GC Engine, Segment Compaction, and Background Thread Summary

Mark-and-sweep GC engine with multi-root live-set collection, atomic segment compaction, and background thread using Weak<DictMetadataStore> lifecycle management.

## Tasks Completed

### Task 1: Mark-and-sweep GC engine + segment compaction

**Commits:** f94c668

Implemented:
- `collect_live_set(dict, roots)` — walks the Merkle tree from each root using `mark_reachable`, recursing through `Branches = [Digest256; 2]` children via `blockset::to_digest224`
- `GarbageCollector::run_gc(dict, roots)` — collects live set then compacts all `.seg` files
- `compact_segment(path, live_set, output_dir, id)` — reads segment, writes survivors to `.tmp`, syncs, atomically renames to final path
- `current_root()` on `DictMetadataStore` — returns `Option<Digest224>` of last committed root

**Key design:** `Branches = [Digest256; 2]` where each child is a potential tree node. `to_digest224` returns `Some` only when the Digest256 has the hash suffix set, filtering out raw data leaves automatically.

**Tests (8 passing):**
- `test_collect_live_set_single_root_single_entry`
- `test_collect_live_set_includes_all_descendants`
- `test_collect_live_set_two_roots_union`
- `test_gc_03_snapshot_root_entry_survives` — GC-03 verified
- `test_unreachable_entry_not_in_live_set`
- `test_compact_segment_retains_live_removes_dead`
- `test_compact_segment_all_live_produces_same_output`
- `test_compact_segment_all_dead_produces_empty_segment`
- `test_compact_segment_crash_safety_atomic_rename`

### Task 2: Background GC thread lifecycle

**Commits:** c7090a3

Implemented:
- `GcHandle { shutdown: Arc<AtomicBool>, handle: Option<JoinHandle<()>> }` — `shutdown()` signals + joins; `Drop` signals only (non-blocking)
- `spawn_background_gc(meta: Weak<DictMetadataStore>, ...)` — thread loops: sleep → check shutdown → upgrade Weak → count orphans → run GC if above threshold
- Thread exits gracefully when `Weak::upgrade()` returns `None` (filesystem unmounted)

**Tests (3 passing):**
- `test_background_gc_handle_shutdown`
- `test_background_gc_runs_cycle`
- `test_background_gc_exits_on_store_drop`

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 - Blocking] Module declaration missing for gc and compaction**
- **Found during:** Initial compilation attempt
- **Issue:** `crates/metadata/src/lib.rs` was missing `pub mod gc;` and `crates/metadata/src/segment/mod.rs` was missing `pub mod compaction;`
- **Fix:** Added both module declarations
- **Files modified:** `crates/metadata/src/lib.rs`, `crates/metadata/src/segment/mod.rs`
- **Commit:** f94c668

**2. [Rule 3 - Blocking] blockset::digest224 private module path**
- **Found during:** Task 1 implementation
- **Issue:** `gc/mod.rs` used `blockset::digest224::to_digest224` which is a private module path
- **Fix:** Changed to `blockset::to_digest224` (the public re-export)
- **Files modified:** `crates/metadata/src/gc/mod.rs`
- **Commit:** f94c668

**3. [Rule 2 - Missing functionality] current_root() missing from DictMetadataStore**
- **Found during:** Task 2 (background.rs calls `store.current_root()`)
- **Issue:** Background GC requires the last committed root but `DictMetadataStore` had no such accessor
- **Fix:** Added `last_root: Mutex<Option<Digest224>>` field; `commit()` updates it after computing root digest; `current_root()` reads it; `load_from_root()` initializes it from the provided root
- **Files modified:** `crates/metadata/src/store.rs`
- **Commit:** f94c668

**4. [Rule 3 - Blocking] Test file had imports from private blockset submodules**
- **Found during:** Compilation of gc_tests.rs
- **Issue:** `build_two_level_tree` helper used `blockset::digest::from_digest224` and `blockset::storage::StorageAdd` — both private modules
- **Fix:** Rewrote the helper to not use private imports; the function was marked `#[allow(dead_code)]` since it's unused; added `#![allow(unused_imports)]` to test file
- **Files modified:** `crates/metadata/tests/gc_tests.rs`
- **Commit:** f94c668

## Pre-existing Issues (Not Regressions)

`crates/metadata/tests/segment_store_tests.rs` has 2 failing tests (`test_wal_logs_mutations_and_root_update`, `test_load_store_from_segments_restores_inodes`) that are part of Plan 05-02 (WAL + store integration). These tests were failing before Plan 05-03 work began (previously failing with "Is a directory" I/O error on WAL path construction).

## Self-Check: PASSED

- FOUND: crates/metadata/src/gc/mod.rs
- FOUND: crates/metadata/src/gc/background.rs
- FOUND: crates/metadata/src/segment/compaction.rs
- FOUND: crates/metadata/tests/gc_tests.rs
- FOUND commit: f94c668 (Task 1)
- FOUND commit: c7090a3 (Task 2)
- All 12 GC tests passing
