---
phase: 05-crash-safety-and-gc
plan: 04
subsystem: gc
tags: [gc, wal, crash-recovery, fuse, cli]

# Dependency graph
requires:
  - phase: 05-crash-safety-and-gc/05-01
    provides: SegmentWriter, SegmentReader, WalStrategy trait + 4 implementations
  - phase: 05-crash-safety-and-gc/05-02
    provides: DictMetadataStore WAL integration, load_store, MountLock
  - phase: 05-crash-safety-and-gc/05-03
    provides: GarbageCollector, collect_live_set, compact_segment, spawn_background_gc
provides:
  - "slicefs gc <store> offline GC CLI command"
  - "Background GC thread spawned on mount, shut down on unmount"
  - "Crash recovery integration tests proving all 5 phase success criteria"
  - "WAL bootstrap fix: set_wal() logs all pre-existing dict entries"
affects: [06-compression-snapshots, 07-cross-platform]

# Tech tracking
tech-stack:
  added: []
  patterns:
    - "Offline GC: check mount.lock → load segments → collect live set → compact → report stats"
    - "Background GC: Weak<DictMetadataStore> lifecycle + explicit shutdown flag + GcHandle::shutdown() after FUSE session ends"
    - "WAL bootstrap: log all existing dict entries when set_wal() is called to guarantee crash recoverability from fresh stores"
    - "Crash recovery test pattern: create store → operate → drop without destroy() → reload from segments → verify state"

key-files:
  created:
    - crates/slicefs-cli/src/gc.rs
    - crates/slicefs-cli/tests/gc_cli_tests.rs
    - crates/slicefs-cli/tests/crash_recovery_tests.rs
  modified:
    - crates/slicefs-cli/src/cli.rs
    - crates/slicefs-cli/src/lib.rs
    - crates/slicefs-cli/src/main.rs
    - crates/slicefs-cli/src/mount.rs
    - crates/metadata/src/store.rs

key-decisions:
  - "set_wal() bootstraps WAL with all existing dict entries: DictMetadataStore::new() creates initial ino=1 dict entries before WAL is set; without bootstrap, crash before first explicit commit loses these entries, causing Corrupted(expected 56 bytes, got 0) on reload"
  - "Offline GC checks mount.lock before running: prevents running GC concurrently with an active mount where segments are actively being written"
  - "Background GC uses Weak<DictMetadataStore>: GC thread auto-exits when filesystem drops; explicit shutdown() joins thread before MountLock drops"

patterns-established:
  - "WAL bootstrap pattern: log all dict entries on set_wal() to ensure segment-only recovery works from any store state"
  - "Crash simulation pattern: drop SliceFsFilesystem without destroy() → reload via load_store_from_segments → load_from_root"

requirements-completed: [GC-01, GC-02, GC-03, POSIX-11]

# Metrics
duration: 6min
completed: 2026-03-29
---

# Phase 5 Plan 04: GC Wire-up, CLI, and Crash Recovery Tests Summary

**Offline `slicefs gc` CLI + background GC in mount lifecycle + 6 crash recovery tests proving all phase success criteria, with WAL bootstrap fix ensuring fresh stores are fully crash-recoverable**

## Performance

- **Duration:** 6 min
- **Started:** 2026-03-29T07:25:41Z
- **Completed:** 2026-03-29T07:32:08Z
- **Tasks:** 2
- **Files modified:** 7 (created 3, modified 4)

## Accomplishments
- Implemented `slicefs gc <store>` offline GC CLI command in gc.rs with mount lock check, segment loading, GC engine invocation, and stats reporting
- Wired background GC thread into `run_mount` using `spawn_background_gc` with `Weak<DictMetadataStore>` + explicit shutdown flag + `GcHandle::shutdown()` after FUSE session ends
- Created 6 end-to-end crash recovery integration tests proving all 5 phase success criteria (SC1-SC5 + bonus multi-fsync cycle)
- Fixed WAL bootstrap bug: `set_wal()` now logs all pre-existing dict entries so fresh stores created via `DictMetadataStore::new()` are fully recoverable after crash

## Task Commits

Each task was committed atomically:

1. **Task 1: Offline GC CLI command + mount with background GC** - `d294768` (feat)
2. **Task 2: End-to-end crash recovery integration tests** - `39d52a0` (feat)

**Plan metadata:** (this commit)

_Note: TDD tasks — tests written, then implementation verified against existing code_

## Files Created/Modified
- `crates/slicefs-cli/src/gc.rs` - Offline GC command: lock check, segment load, GC engine, stats print
- `crates/slicefs-cli/src/cli.rs` - Added `Gc { store: PathBuf }` subcommand
- `crates/slicefs-cli/src/lib.rs` - Exported gc module for integration tests
- `crates/slicefs-cli/src/main.rs` - Wired `Cmd::Gc { store }` to `gc::run_gc`
- `crates/slicefs-cli/src/mount.rs` - `run_mount` now spawns background GC via `spawn_background_gc`
- `crates/slicefs-cli/tests/gc_cli_tests.rs` - 4 tests: clean store, dead entries, locked store, CLI parse
- `crates/slicefs-cli/tests/crash_recovery_tests.rs` - 6 crash recovery tests (SC1-SC5 + bonus)
- `crates/metadata/src/store.rs` - `set_wal()` bootstraps WAL with all existing dict entries

## Decisions Made
- `set_wal()` bootstraps WAL with all existing dict entries: initial ino=1 dict entries created by `DictMetadataStore::new()` before WAL attachment were not in WAL segments, causing `Corrupted(expected 56 bytes, got 0)` on reload after crash; the fix logs all existing entries when WAL is first attached
- Offline GC checks `mount.lock` before running to prevent concurrent modification with an active mount
- Background GC lifecycle: `GcHandle` stored for the FUSE session duration, `shutdown()` called after `mount2` returns but before `_mount_lock` drops

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Bug] WAL bootstrap: set_wal() logs all pre-existing dict entries**
- **Found during:** Task 2 (crash_recovery_tests SC1 test)
- **Issue:** `DictMetadataStore::new()` creates the root directory inode (ino=1) and initial dir entries directly in the in-memory dict before any WAL is attached. When `set_wal()` is later called and mutations happen, only NEW dict entries (delta from current state) are logged. The initial ino=1 entries are never logged. After crash + reload from segments, `get_inode(1)` failed with `Corrupted("expected 56 bytes, got 0")` because the inode bytes were absent from the dict.
- **Fix:** Modified `set_wal()` in `crates/metadata/src/store.rs` to iterate over all current dict entries and log each as a `DictionaryAppend` WAL entry before attaching the WAL strategy. This creates a complete bootstrap snapshot in the first segment.
- **Files modified:** `crates/metadata/src/store.rs`
- **Verification:** All 6 crash recovery tests pass including SC1 which calls `get_inode(1)` after reload
- **Committed in:** `39d52a0` (Task 2 commit)

---

**Total deviations:** 1 auto-fixed (Rule 1 - Bug)
**Impact on plan:** Essential correctness fix. The WAL bootstrap is a fundamental requirement for crash safety when using `DictMetadataStore::new()`. Without it, any fresh store would lose its root directory inode on crash before the first explicit commit. No scope creep.

## Issues Encountered
- SC1 test exposed pre-existing WAL design gap: initial dict entries from `DictMetadataStore::new()` were never logged to WAL. Fixed via Rule 1 (auto-fix bug) by bootstrapping WAL in `set_wal()`. All 389 workspace tests pass after fix.

## User Setup Required
None - no external service configuration required.

## Next Phase Readiness
- Phase 5 (Crash Safety and GC) is complete: all 4 plans done, all success criteria proven
- 389 workspace tests passing (up from 358 before Phase 5)
- Phase 6 (Compression and Snapshots) can proceed: WAL + GC infrastructure is stable and crash-safe
- Snapshot root discovery in GC (`slicefs gc` currently uses single root; Phase 6 adds snapshot roots)

---
*Phase: 05-crash-safety-and-gc*
*Completed: 2026-03-29*

## Self-Check: PASSED

- gc.rs: FOUND
- gc_cli_tests.rs: FOUND
- crash_recovery_tests.rs: FOUND
- 05-04-SUMMARY.md: FOUND
- Commit d294768: FOUND
- Commit 39d52a0: FOUND
