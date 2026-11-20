---
phase: 02-metadata-engine
plan: 01
subsystem: metadata
tags: [blockset, data-id, submodule, InodeMeta, serialization, traits, slicefs-traits]
dependency_graph:
  requires: []
  provides: [data-id-submodule, Digest224, Digest256, StorageAdd, StorageGet, MetadataStore, InodeMeta, metadata-crate]
  affects: [slicefs-traits, metadata, all-future-metadata-plans]
tech_stack:
  added: [blockset (data-id path dependency), sha2-compress (0.7.2), proptest]
  patterns: [56-byte LE inode serialization, Dictionary-backed CAS intern/load, blockset::Dictionary as StorageAdd+StorageGet]
key_files:
  created:
    - crates/slicefs-traits/src/digest.rs
    - crates/slicefs-traits/src/storage.rs
    - crates/slicefs-traits/src/metadata.rs
    - crates/metadata/Cargo.toml
    - crates/metadata/src/lib.rs
    - crates/metadata/src/inode.rs
  modified:
    - Cargo.toml
    - Cargo.lock
    - .gitmodules
    - crates/slicefs-traits/Cargo.toml
    - crates/slicefs-traits/src/lib.rs
decisions:
  - "blockset StorageAdd/StorageGet are private traits — intern_inode/load_inode use blockset::Dictionary directly rather than generic bounds"
  - "Digest224/Digest256/Branches redeclared as type aliases in slicefs-traits (not re-exported from private blockset modules)"
  - "StorageAdd/StorageGet redeclared as traits in slicefs-traits/storage.rs — structurally compatible with blockset but independent"
  - "InodeMeta defined in slicefs-traits/metadata.rs as plain data struct — serialization lives in metadata crate"
  - "sha2-compress added to workspace dependencies to satisfy blockset's workspace inheritance"
metrics:
  duration: "~25 minutes"
  completed_date: "2026-03-27"
  tasks_completed: 2
  files_created: 6
  files_modified: 5
---

# Phase 2 Plan 1: data-id Integration and Metadata Type Foundation Summary

**One-liner:** data-id git submodule wired as blockset path dependency, Digest224/Digest256/MetadataStore types added to slicefs-traits, metadata crate created with 56-byte LE InodeMeta serialization and Dictionary CAS round-trip.

## What Was Built

### Task 1: data-id Submodule, blockset Integration, slicefs-traits Redesign

Added the data-id repository as a git submodule at `crates/data-id`. The blockset crate within it is used as a path dependency in both `slicefs-traits` and `metadata`.

Key types established in `slicefs-traits`:

- **`digest.rs`**: Type aliases `Digest224 = [u32; 7]`, `Digest256 = [u32; 8]`, `Branches = [Digest256; 2]` plus helper functions `from_digest224`, `to_digest224`, `digest256_from_bytes`, `digest256_to_data`.
- **`storage.rs`**: `StorageAdd` and `StorageGet` traits (redeclared independently since blockset's `storage` module is private).
- **`metadata.rs`**: `InodeId = u64`, `MetaError` enum (8 variants), `DirEntry` struct, `InodeMeta` struct (10 fields), `InodeMeta::new_directory` / `new_file` constructors, `MetadataStore` trait (16 methods).

**Deviation (Rule 1 — Bug Fix):** blockset's `StorageAdd`/`StorageGet` traits are private and not re-exported from blockset's lib.rs. The plan specified re-exporting them from blockset module paths, but that produces `E0603 private module` errors. Fixed by declaring equivalent traits independently in `slicefs-traits/src/storage.rs` (structurally identical). Similarly, `intern_inode`/`load_inode` use `blockset::Dictionary` directly since it is the only publicly-accessible implementation.

**Deviation (Rule 1 — Bug Fix):** blockset's `sha2-compress` dependency uses workspace inheritance from blockset's own workspace (not ours). Added `sha2-compress = "0.7.1"` to the SliceFS workspace `[workspace.dependencies]` to satisfy Cargo's workspace-inheritance requirement when blockset is used as a path dependency in our workspace.

### Task 2: metadata Crate with InodeMeta Serialization (TDD)

Created `crates/metadata/src/inode.rs` with:

- `serialize_inode(meta: &InodeMeta) -> [u8; 56]` — 56-byte LE pack
- `deserialize_inode(buf: &[u8]) -> Result<InodeMeta, MetaError>` — LE unpack, rejects non-56-byte input
- `intern_inode(dict: &mut Dictionary, meta: &InodeMeta) -> Digest224` — CAS store via `State::push_all`
- `load_inode(dict: &Dictionary, key: &Digest224) -> Result<InodeMeta, MetaError>` — CAS retrieve via `GetData`/`GetBytes`

**9 tests (8 unit + 1 proptest):**
- `test_serialize_size_is_56` — output is exactly 56 bytes
- `test_round_trip` — sample meta round-trips
- `test_round_trip_max_values` — u64::MAX, i64::MIN/MAX, u32::MAX all round-trip
- `test_round_trip_zero_ino` — ino=0 round-trips
- `test_deserialize_rejects_wrong_length` — 55, 57, 0 bytes all rejected
- `test_new_directory_nlinks` — nlinks=2
- `test_new_file_nlinks` — nlinks=1
- `test_intern_and_load_roundtrip` — Dictionary CAS round-trip
- `prop_round_trip` — proptest with 256 arbitrary InodeMeta cases

## Test Results

| Suite | Tests | Result |
|-------|-------|--------|
| blockset | 34 | PASS |
| cas-local | 52 | PASS |
| slicefs-traits | 0 | PASS |
| metadata | 9 | PASS |
| **Total** | **95** | **ALL PASS** |

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Bug] blockset storage module is private — StorageAdd/StorageGet not re-exportable**
- **Found during:** Task 1 (cargo check)
- **Issue:** Plan specified `pub use blockset::storage::StorageAdd` but blockset's `mod storage` is private (`E0603`).
- **Fix:** Declared `StorageAdd` and `StorageGet` as independent traits in `slicefs-traits/src/storage.rs` with identical signatures. `intern_inode`/`load_inode` use `blockset::Dictionary` directly since it is the only public concrete type implementing blockset's private storage traits.
- **Files modified:** `crates/slicefs-traits/src/storage.rs`, `crates/metadata/src/inode.rs`
- **Commit:** 15d437e

**2. [Rule 3 - Blocking] blockset workspace sha2-compress dependency not in SliceFS workspace**
- **Found during:** Task 1 (cargo check)
- **Issue:** blockset uses `sha2-compress = { workspace = true }` which inherits from its own workspace. When used as a path dep in our workspace, Cargo requires the key exists in *our* workspace dependencies too.
- **Fix:** Added `sha2-compress = "0.7.1"` to SliceFS workspace `[workspace.dependencies]`.
- **Files modified:** `Cargo.toml`
- **Commit:** 15d437e

## Self-Check: PASSED

Files exist:
- crates/slicefs-traits/src/digest.rs: FOUND
- crates/slicefs-traits/src/storage.rs: FOUND
- crates/slicefs-traits/src/metadata.rs: FOUND
- crates/metadata/src/inode.rs: FOUND
- .gitmodules: FOUND
- crates/data-id/blockset/Cargo.toml: FOUND

Commits exist:
- 15d437e (Task 1): FOUND
- ac0722e (Task 2): FOUND
