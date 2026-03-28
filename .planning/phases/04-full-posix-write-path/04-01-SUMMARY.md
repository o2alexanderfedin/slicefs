---
phase: 04-full-posix-write-path
plan: "01"
subsystem: metadata + fuse-adapter
tags: [refcounts, write-path, fuse, persistence, cas]
dependency_graph:
  requires: [03-read-only-fuse/03-01, 03-read-only-fuse/03-02, 03-read-only-fuse/03-03]
  provides: [refcount-infrastructure, write-handle-table, rw-mount, destroy-persistence]
  affects: [04-02, 04-03, 04-04, 04-05]
tech_stack:
  added: []
  patterns: [TDD-red-green, backward-compat-format-versioning, atomic-handle-counter]
key_files:
  created:
    - crates/metadata/tests/refcount_tests.rs
  modified:
    - crates/metadata/src/store.rs
    - crates/slicefs-cli/src/filesystem.rs
    - crates/slicefs-cli/src/mount.rs
decisions:
  - "Root record expanded from 156 to 184 bytes: adds refcount_data_digest as 7th Digest224 at offset 156"
  - "Backward compat: 156-byte root records accepted by load_from_root and treated as empty refcounts"
  - "OpenFlags.acc_mode() used instead of contains() — fuser 0.17 OpenFlags is a newtype i32 with no bitfield methods"
  - "OpenFileState fields (ino, buf) are write-through stubs — actual flush logic comes in Plan 02"
  - "MountOption::RO removed: filesystem now mounts read-write; destroy() persists dictionary.bin + root.bin"
metrics:
  duration: "249 seconds"
  completed_date: "2026-03-28"
  tasks_completed: 2
  files_modified: 3
  files_created: 1
---

# Phase 4 Plan 01: Write Path Scaffolding Summary

Refcount table, open-file handle state machine, and post-session persistence are all in place. Every subsequent write plan depends on these primitives.

## Tasks Completed

| Task | Name | Commit | Files |
|------|------|--------|-------|
| 1 | Add refcount infrastructure to DictMetadataStore | 7e7fa62 | store.rs, refcount_tests.rs |
| 2 | Add write state types, RW mount, destroy persistence | c9c7cd1 | filesystem.rs, mount.rs |

## Key Changes

### crates/metadata/src/store.rs

- Added `refcounts: Mutex<BTreeMap<Digest224, u64>>` field to `DictMetadataStore`
- Added `increment_refcount(&self, digest: &Digest224)`, `decrement_refcount(&self, digest: &Digest224)`, `get_refcount(&self, digest: &Digest224) -> u64` public methods
- `commit()` now serializes refcounts via new `intern_digest224_u64_map()` helper and stores the digest as 7th field in the root record (184-byte format)
- `load_from_root()` handles both 156-byte (v1, no refcounts) and 184-byte (v2, with refcounts) formats
- Added `intern_digest224_u64_map()` and `load_digest224_u64_map()` private helpers

### crates/slicefs-cli/src/filesystem.rs

- Added `OpenFileState { ino: u64, buf: Vec<u8> }` struct for per-handle write buffers
- Expanded `SliceFsFilesystem` with `open_files: Mutex<HashMap<u64, OpenFileState>>`, `next_fh: AtomicU64`, `store_path: Option<PathBuf>`
- `new()` now accepts `store_path: Option<PathBuf>` as third parameter
- `open()` allocates unique file handles (via `next_fh.fetch_add`) for O_WRONLY/O_RDWR opens; returns handle 0 for read-only
- `release()` removes the entry from `open_files` when fh > 0
- `destroy()` now persists `dictionary.bin` and `root.bin` to `store_path` after successful `meta.commit()`

### crates/slicefs-cli/src/mount.rs

- Removed `MountOption::RO` from `build_mount_options()` — filesystem now mounts read-write
- `run_mount()` passes `Some(store_path.to_path_buf())` to `SliceFsFilesystem::new()`
- Updated `test_build_mount_options_with_noatime` and `test_build_mount_options_without_noatime` to assert RO is absent

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Bug] fuser OpenFlags API mismatch**
- **Found during:** Task 2
- **Issue:** Plan specified `flags.contains(OpenFlags::O_WRONLY)` but fuser 0.17 `OpenFlags` is a newtype `i32` wrapper with no `contains()` method or associated constants. The correct API is `flags.acc_mode()` returning an `OpenAccMode` enum.
- **Fix:** Changed to `flags.acc_mode() == OpenAccMode::O_WRONLY || flags.acc_mode() == OpenAccMode::O_RDWR`
- **Files modified:** crates/slicefs-cli/src/filesystem.rs
- **Commit:** c9c7cd1

## Test Coverage

- 5 new integration tests in `crates/metadata/tests/refcount_tests.rs`
- All 89 pre-existing metadata tests still pass
- All 28 slicefs-cli tests pass (including updated mount option assertions)
- Full suite: 122 tests passing (89 metadata + 28 slicefs-cli + 5 refcount)

## Self-Check: PASSED
