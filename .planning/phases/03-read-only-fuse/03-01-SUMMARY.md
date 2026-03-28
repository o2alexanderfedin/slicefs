---
phase: 03-read-only-fuse
plan: 01
subsystem: slicefs-cli
tags: [fuse, cli, clap, store-io, filesystem-adapter, read-only, erofs]
dependency_graph:
  requires:
    - 02-03 (DictMetadataStore with full inode CRUD, commit/load_from_root)
    - data-id/blockset (Dictionary, GetBytes, GetData, State)
    - slicefs-traits (MetaError, InodeMeta, MetadataStore, Digest224/256)
  provides:
    - slicefs-cli binary crate with clap-derived CLI
    - SliceFsFilesystem implementing fuser::Filesystem (read-only)
    - StoreIo implementing blockset::Io for directory-backed storage
    - meta_error_to_fuse_errno / meta_error_to_errno helpers
    - inode_to_file_attr conversion helper
  affects:
    - 03-02 (seed command — uses Cli, StoreIo, SliceFsFilesystem, DictMetadataStore)
    - 03-03 (mount command — uses SliceFsFilesystem::new, meta(), dict())
tech_stack:
  added:
    - fuser 0.17 with macos-no-mount feature (FUSE adapter, no macFUSE install required)
    - clap 4 with derive feature (CLI argument parsing)
    - libc 0.2 (POSIX errno constants for error mapping)
  patterns:
    - Arc<DictMetadataStore> + Arc<Mutex<Dictionary>> dual-ownership for deadlock avoidance
    - fuser::Errno constants (EROFS, ENOENT, etc.) for type-safe error returns
    - TDD: tests written before implementation for both tasks
key_files:
  created:
    - Cargo.toml (workspace: added slicefs-cli member, clap workspace dep, fuser macos-no-mount)
    - crates/slicefs-cli/Cargo.toml
    - crates/slicefs-cli/src/main.rs
    - crates/slicefs-cli/src/cli.rs
    - crates/slicefs-cli/src/store_io.rs
    - crates/slicefs-cli/src/filesystem.rs
  modified: []
decisions:
  - "fuser macos-no-mount feature used: macOS lacks macFUSE install on this dev machine; the feature compiles fuser without pkg-config for FUSE libraries while keeping full Filesystem API available for unit tests"
  - "EROFS for write ops: all write callbacks return Errno::EROFS, not ENOSYS — signals read-only filesystem to kernel, not missing implementation"
  - "Dual dict approach: SliceFsFilesystem holds meta: Arc<DictMetadataStore> + dict: Arc<Mutex<Dictionary>> separately to avoid deadlock with DictMetadataStore's internal Mutex"
  - "CLI includes --allow-other flag on Mount subcommand per plan checker warning"
metrics:
  duration: "402 seconds (~7 minutes)"
  completed_date: "2026-03-28"
  tasks_completed: 2
  files_created: 6
  tests_added: 16
  tests_total_workspace: 190
---

# Phase 3 Plan 01: CLI Crate Scaffold and SliceFsFilesystem FUSE Adapter Summary

slicefs-cli binary crate with clap-derived CLI (mount/unmount/seed), StoreIo for blockset::Io, and SliceFsFilesystem implementing all read-only fuser::Filesystem callbacks with EROFS for writes.

## Tasks Completed

| Task | Name | Commit | Files |
|------|------|--------|-------|
| 1 | CLI crate scaffold, StoreIo, and clap subcommands | 67c7f26 | Cargo.toml, slicefs-cli/Cargo.toml, main.rs, cli.rs, store_io.rs, filesystem.rs (stub) |
| 2 | SliceFsFilesystem FUSE adapter with full read-only callbacks | d4fa28c | filesystem.rs (full implementation, 600 lines) |

## Decisions Made

1. **fuser `macos-no-mount` feature**: macOS dev environment lacks macFUSE installation. The `macos-no-mount` feature compiles fuser without requiring pkg-config `fuse.pc` while providing the complete `fuser::Filesystem` trait API. Unit tests work fully. Actual FUSE mounting requires macFUSE at runtime (not at build time).

2. **EROFS for write callbacks**: All write operations (`write`, `create`, `mkdir`, `mknod`, `symlink`, `link`, `unlink`, `rmdir`, `rename`, `setattr`, `fallocate`) return `fuser::Errno::EROFS`. This signals "read-only filesystem" to the kernel rather than "not implemented" (ENOSYS), which is the correct POSIX behavior for a read-only mount.

3. **Dual Arc pattern**: `SliceFsFilesystem` holds `meta: Arc<DictMetadataStore>` and `dict: Arc<Mutex<Dictionary>>` as separate fields. This avoids deadlocking with `DictMetadataStore`'s internal `Mutex<Dictionary>` when content reads via `GetBytes` need concurrent dictionary access.

4. **`--allow-other` CLI flag**: Added to the `Mount` subcommand per the plan checker warning about `MountOption::AllowOther`. Plans 02/03 will wire this into the actual mount call.

## Test Results

All 16 slicefs-cli unit tests pass:
- 4 CLI parse tests (mount, unmount, seed, mount with options)
- 3 StoreIo tests (round-trip, subdirectory creation, missing file error)
- 9 filesystem tests:
  - `test_inode_to_file_attr_directory` — mode→Directory, perm extraction
  - `test_inode_to_file_attr_regular_file` — mode→RegularFile
  - `test_meta_error_to_errno` — ENOENT, EEXIST, ENOTDIR, EISDIR, EIO
  - `test_getattr_root_is_directory` — inode 1 is Directory kind
  - `test_lookup_nonexistent_returns_notfound` — ENOENT on missing name
  - `test_readdir_root_contains_dot_entries` — "." and ".." present
  - `test_read_returns_correct_bytes` — GetBytes reads seeded content
  - `test_read_with_offset` — offset read returns correct slice
  - `test_statfs_returns_nonzero_blocks` — hardcoded non-zero values

Workspace: 190 tests pass, zero failures, zero regressions.

## Deviations from Plan

None — plan executed exactly as written. The `macos-no-mount` fuser feature discovery was anticipated by the plan checker note, resolved automatically as Rule 3 (blocking issue: no macFUSE installed).

## Self-Check: PASSED

All created files verified present on disk. Both task commits (67c7f26, d4fa28c) confirmed in git log.
