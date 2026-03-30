---
phase: 08-correctness-fixes
plan: 01
subsystem: metadata
tags: [refcount, saturation, inode-count, statfs, statvfs, scrub, atomicu64, libc]

# Dependency graph
requires:
  - phase: 07.1-filestorage-migration
    provides: DictMetadataStore with file-backed StoreIo, increment_refcount/decrement_refcount, statfs handler

provides:
  - saturating refcount arithmetic at u64::MAX (immortal blocks, no wrap-to-0)
  - inode_count AtomicU64 tracking create/delete lifecycle + load_from_root round-trip
  - statfs with three-tier space reporting via libc::statvfs (real host disk values)
  - ScrubReport.saturated_blocks field + warning output
  - stats CLI three-tier display (logical, CAS bytes, host disk used)
  - FIX-03/FIX-04 confirmed: snapshots_by_version + snapshots_by_name HashMap indexes present

affects: [09-compression-removal, 10-streaming-writes-core, 11-non-sequential-write-handling]

# Tech tracking
tech-stack:
  added: [tracing (added to metadata crate), libc::statvfs (used directly in filesystem.rs)]
  patterns:
    - saturating-refcount: increment_refcount saturates at u64::MAX with tracing::warn; decrement is no-op at MAX
    - inode-count-atomic: AtomicU64 incremented in create_inode/create_directory, decremented in delete_inode, recomputed in load_from_root
    - compute-statfs-extracted: statfs logic extracted to compute_statfs() called by both FUSE handler and test helper
    - three-tier-statfs: blocks/bfree/bavail from libc::statvfs; files from inode_count(); fallback to 0 when store_path is None

key-files:
  created: []
  modified:
    - crates/metadata/src/store.rs
    - crates/metadata/Cargo.toml
    - crates/slicefs-cli/src/filesystem.rs
    - crates/slicefs-cli/src/scrub.rs
    - crates/slicefs-cli/src/stats.rs
    - crates/slicefs-cli/tests/statfs_tests.rs

key-decisions:
  - "tracing crate added to metadata for saturated refcount warning — only stdlib alternative was eprintln which bypasses log aggregation"
  - "compute_statfs() extracted from Filesystem trait impl to regular impl block — Filesystem trait does not permit pub methods"
  - "statvfs graceful fallback to (0,0,0) when store_path is None — avoids panic; tests cover the None path explicitly"
  - "inode_count initialized to 1 (root inode) in new() — root is inserted at construction time"
  - "host_disk_bytes added to StoreStats as separate field — dir_size(store_path) vs dir_size(vt0/) reflects full vs CAS storage"

patterns-established:
  - "Saturating arithmetic pattern: check == u64::MAX before increment, return early on saturated decrement"
  - "AtomicU64 counter pattern mirrors logical_bytes: fetch_add on create, fetch_update+saturating_sub on delete, recomputed in load_from_root"
  - "Test helper extraction: test_statfs_values() calls compute_statfs() — same values as FUSE without a mock request"

requirements-completed: [FIX-01, FIX-02, FIX-03, FIX-04]

# Metrics
duration: 20min
completed: 2026-03-30
---

# Phase 8 Plan 01: Correctness Fixes Summary

**Saturating refcount arithmetic (no wrap-to-0 at u64::MAX), real-disk statfs via libc::statvfs, inode_count AtomicU64, and scrub immortal-block reporting — four correctness bugs closed before Phase 9 write-path surgery**

## Performance

- **Duration:** ~20 min
- **Started:** 2026-03-30T03:36:54Z
- **Completed:** 2026-03-30T03:53:41Z
- **Tasks:** 2 (both TDD)
- **Files modified:** 6

## Accomplishments
- FIX-01: `increment_refcount` saturates at `u64::MAX` (no integer wrap); `decrement_refcount` is a no-op at `u64::MAX`; `saturated_refcount_count()` counts immortal blocks
- FIX-02: `inode_count` AtomicU64 tracks create/delete lifecycle; `statfs()` now calls `libc::statvfs` on the backing store path for real blocks/bfree/bavail; `files` field = actual inode count (not 1,000,000)
- FIX-01 scrub: `ScrubReport.saturated_blocks` populated after metadata reload; human output prints warning for immortal blocks
- FIX-02 stats: `host_disk_bytes` (full store dir) added; "Physical bytes" label renamed to "CAS bytes"
- FIX-03/FIX-04 verified: `snapshots_by_version` and `snapshots_by_name` HashMap fields confirmed present and used (inserted Phase 7.1)
- Full workspace test suite: 0 failures across all crates

## Task Commits

1. **Task 1: Saturating refcount + inode_count AtomicU64** - `9100c58` (feat)
2. **Task 2: Three-tier statfs, scrub saturated reporting, stats update** - `9728d03` (feat)

## Files Created/Modified
- `crates/metadata/src/store.rs` - saturating increment_refcount/decrement_refcount, saturated_refcount_count(), inode_count AtomicU64 field + getter, 9 new TDD tests
- `crates/metadata/Cargo.toml` - added tracing dependency
- `crates/slicefs-cli/src/filesystem.rs` - compute_statfs() with libc::statvfs, test_statfs_values() helper, updated inline test
- `crates/slicefs-cli/src/scrub.rs` - ScrubReport.saturated_blocks field, warning output, 2 new tests
- `crates/slicefs-cli/src/stats.rs` - host_disk_bytes field, CAS bytes label rename
- `crates/slicefs-cli/tests/statfs_tests.rs` - 3 new TDD integration tests (files reflects inode_count, blocks from host, fallback None)

## Decisions Made
- `tracing` crate added to metadata — saturated block warning should be structured log, not bare `eprintln`
- `compute_statfs()` moved to regular `impl SliceFsFilesystem` block — Filesystem trait impl does not permit `pub` methods
- `libc::statvfs` uses `f_frsize` (not `f_bsize`) per RESEARCH.md pitfall 3 — frsize is the actual fragment/block size
- `host_disk_bytes` is a separate field in StoreStats rather than replacing physical_bytes — both metrics have distinct value

## Deviations from Plan

None - plan executed exactly as written.

## Issues Encountered

- `compute_statfs` initially placed inside `impl Filesystem for SliceFsFilesystem` block, causing E0407 (method not member of trait) and E0449 (pub visibility not permitted). Fixed by moving to separate `impl SliceFsFilesystem` block (Rule 3 auto-fix).
- `tracing` not in metadata Cargo.toml — added as workspace dependency (Rule 3 auto-fix).
- macOS `libc::statvfs` fields (`f_blocks`, `f_bfree`, `f_bavail`) are `u64` on Darwin — explicit `as u64` casts added to silence type mismatch (Rule 1 bug fix).

## Next Phase Readiness
- Phase 9 (Compression Removal) can proceed: all four FIX requirements closed, no data-loss risk from refcount overflow
- `flush_buffer_to_cas` is the shared mutation point for Phase 9 — refcount/statfs changes are orthogonal to compression removal
- FIX-03/FIX-04 confirmed present: no snapshot lookup work needed before Phase 9

---
*Phase: 08-correctness-fixes*
*Completed: 2026-03-30*
