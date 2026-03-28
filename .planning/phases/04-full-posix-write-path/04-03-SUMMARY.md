---
phase: 04-full-posix-write-path
plan: "03"
subsystem: fuse-adapter
tags: [fuse, posix, mkdir, rmdir, unlink, link, rename, symlink, readlink, nlinks, refcount, cas]
dependency_graph:
  requires:
    - phase: 04-full-posix-write-path/04-01
      provides: refcount-infrastructure, write-handle-table, rw-mount
  provides:
    - simulate_mkdir/rmdir with parent nlinks management
    - simulate_unlink with nlinks lifecycle and CAS refcount decrement at zero
    - simulate_link with EPERM guard for hard links to directories
    - simulate_rename with RENAME_NOREPLACE/RENAME_EXCHANGE/overwrite support
    - simulate_symlink storing target as CAS content via manifest
    - simulate_readlink retrieving target bytes from dictionary
    - All 6 FUSE callbacks updated (mkdir, rmdir, unlink, link, rename, symlink+readlink)
  affects: [04-04, 04-05]
tech_stack:
  added: []
  patterns:
    - simulate_* methods expose FUSE callback logic without request/reply machinery
    - TDD-red-green: failing tests committed first, then implementation
    - Integration tests as separate binaries in tests/ directory (independent compilation)
    - nlinks lifecycle: decrement at unlink, delete inode + decrement refcounts at zero
    - Rename via link+unlink composition (cross-dir and same-dir)
key_files:
  created:
    - crates/slicefs-cli/tests/dir_link_tests.rs
  modified:
    - crates/slicefs-cli/src/filesystem.rs
key_decisions:
  - "simulate_rename uses flags as raw u32 bits (0=normal, 1=NOREPLACE, 2=EXCHANGE) — RenameFlags constants are #[cfg(linux)] so raw bits portable for macOS test env"
  - "simulate_mkdir delegates entirely to DictMetadataStore::create_directory — it already handles nlinks increment, . and .. entries, parent dir update"
  - "Displacement in rename: old target's nlinks managed like unlink — refcounts decremented and inode deleted at zero; directories orphaned (no dir hard links)"
  - "simulate_readlink returns String (not bytes) to match test expectations; simulate_symlink stores raw target bytes in CAS"
  - "Integration tests in separate files in tests/ compile independently — write_path_tests.rs compilation errors (plan 04-02 RED phase) don't block dir_link_tests.rs"
patterns-established:
  - "Per-operation simulate_* methods: testable logic units, FUSE callbacks delegate via one match"
  - "nlinks never underflows: guarded with `if inode.nlinks > 0` before decrement"
requirements-completed: [POSIX-02, POSIX-03, POSIX-04, POSIX-05]
duration: 273s
completed: "2026-03-27"
---

# Phase 4 Plan 03: Directory Operations, Rename, Symlinks, and Hard Links Summary

**mkdir/rmdir/unlink/link/rename/symlink/readlink FUSE callbacks with full POSIX nlinks lifecycle and CAS refcount management via simulate_* testable helpers**

## Performance

- **Duration:** 273 seconds (~4.5 min)
- **Started:** 2026-03-27T02:55:54Z
- **Completed:** 2026-03-27T03:00:27Z
- **Tasks:** 2
- **Files modified:** 2 (filesystem.rs, dir_link_tests.rs)
- **Files created:** 1 (dir_link_tests.rs)

## Accomplishments

- 25 new integration tests in `dir_link_tests.rs` covering all directory, link, rename, and symlink operations
- 7 new `simulate_*` methods on `SliceFsFilesystem` exposing FUSE callbacks as testable pure logic
- All 6 previously-stubbed FUSE callbacks replaced with real implementations
- nlinks lifecycle fully correct: parent incremented on mkdir, decremented on rmdir/unlink, hard links tracked with EPERM for dir links
- CAS refcounts decremented when file nlinks reach 0 (unlink and rename-with-overwrite)

## Task Commits

Each task was committed atomically:

1. **RED - Task 1+2: Failing tests** - `908260a` (test)
2. **GREEN - Task 1+2: Implementation** - `03aabe4` (feat)

## Files Created/Modified

- `crates/slicefs-cli/src/filesystem.rs` — Added 7 simulate_* methods (simulate_mkdir, simulate_rmdir, simulate_unlink, simulate_link, simulate_rename, simulate_symlink, simulate_readlink); updated FUSE callbacks mkdir/rmdir/unlink/link/rename/symlink/readlink to delegate
- `crates/slicefs-cli/tests/dir_link_tests.rs` — 25 integration tests using DictMetadataStore directly (no FUSE mount required)

## Decisions Made

- `simulate_mkdir` delegates to `DictMetadataStore::create_directory` since it already handles all the nlinks/dot-entry/parent-update mechanics. No duplication needed.
- `simulate_rename` flags checked as raw u32 bits (0, 1, 2) instead of `RenameFlags` constants because `RENAME_NOREPLACE`/`RENAME_EXCHANGE` are `#[cfg(linux)]` — raw bits work correctly on macOS for test verification.
- When rename overwrites a target: managed identically to `simulate_unlink` (decrement nlinks, at zero: decrement refcounts + delete inode). Directories are orphaned on overwrite (consistent with not supporting hard links to dirs).
- `simulate_readlink` returns `String` to keep test assertions simple; target bytes are UTF-8 paths.
- Each `tests/*.rs` file in Rust compiles as an independent binary. `write_path_tests.rs` (plan 04-02 RED phase) having missing method errors does NOT prevent `dir_link_tests.rs` from compiling and running.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 - Blocking] `Errno(errno)` private constructor in FUSE create() callback**
- **Found during:** Task 1 setup (build verification)
- **Issue:** Plan 04-02's partial commit left `reply.error(Errno(errno))` in the `create()` FUSE callback. `Errno`'s inner `NonZeroI32` field is private in fuser 0.17; this caused a compile error preventing any tests from running.
- **Fix:** Changed to `reply.error(Errno::from_i32(errno))` — the correct public API.
- **Files modified:** `crates/slicefs-cli/src/filesystem.rs`
- **Verification:** `cargo build --package slicefs-cli` succeeded after fix.
- **Committed in:** `03aabe4` (included in the main implementation commit)

---

**Total deviations:** 1 auto-fixed (Rule 3 - blocking)
**Impact on plan:** Fix was a 1-line correction of plan 04-02's partial work. No scope creep.

## Issues Encountered

- plan 04-02's `write_path_tests.rs` uses `simulate_create`/`simulate_write` etc. but plan 04-02 implemented `test_create`/`test_write`. This naming mismatch means `write_path_tests.rs` cannot compile; however, Rust integration test files compile independently so `dir_link_tests.rs` works fine.

## Next Phase Readiness

- All basic directory mutation FUSE callbacks are now implemented and tested
- Plan 04-02 (create/write/release/setattr) works in parallel on the same file without conflict
- Ready for plan 04-04 (persistence, round-trip tests) once 04-02 completes

## Self-Check: PASSED
