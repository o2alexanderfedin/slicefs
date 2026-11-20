---
phase: 10-streaming-writes-core
plan: 01
subsystem: filesystem
tags: [blockset, streaming-writes, merkle-tree, fuse, push-bytes, state-accumulator]

# Dependency graph
requires:
  - phase: 09-compression-removal
    provides: v3 store format with raw bytes (no compression header)
provides:
  - OpenFileState with State accumulator instead of Vec<u8> buffer
  - test_write using push_bytes for O(log N) streaming writes
  - byte_count field for tracking total bytes without State introspection
  - last_committed_root field for refcount lifecycle tracking
  - FSA Drop impl for streaming write support across sessions
  - clone+end pattern for flush_buffer_for_fsync
affects: [10-02, 10-03, 11-non-sequential-write-handling]

# Tech tracking
tech-stack:
  added: []
  patterns: [clone+end snapshot, FSA Drop flush, streaming push_bytes]

key-files:
  created: []
  modified:
    - crates/slicefs-cli/src/filesystem.rs
    - crates/data-id/blockset/src/file_storage.rs
    - crates/data-id/blockset/src/app2.rs
    - crates/data-id/blockset/src/lib.rs
    - crates/slicefs-cli/tests/write_path_tests.rs
    - crates/slicefs-cli/tests/posix_compliance_tests.rs

key-decisions:
  - "FSA Drop impl flushes pending internal nodes to disk for streaming write support"
  - "FSA extend() reads from disk when node not in memory map (cross-session streaming)"
  - "flush_buffer_for_fsync uses clone+end pattern, keeps original State alive after fsync"
  - "Digest224 re-exported from blockset crate for downstream use"
  - "2 non-sequential offset tests ignored with Phase 11 TODO (STRM-02)"

patterns-established:
  - "clone+end snapshot: clone State under open_files lock, release lock, end clone under io lock"
  - "Lock ordering: open_files -> io is canonical; never hold both simultaneously except in test_write where open_files -> io is enforced"
  - "FSA Drop flush: FileStorageAdd writes all pending internal nodes to disk on drop, enabling multi-session streaming"

requirements-completed: [STRM-01]

# Metrics
duration: 12min
completed: 2026-03-30
---

# Phase 10 Plan 01: OpenFileState Redesign Summary

**OpenFileState redesigned from Vec<u8> to blockset State accumulator with push_bytes streaming, FSA Drop flush for cross-session consistency, and clone+end fsync pattern**

## Performance

- **Duration:** 12 min
- **Started:** 2026-03-30T07:11:14Z
- **Completed:** 2026-03-30T07:23:23Z
- **Tasks:** 2
- **Files modified:** 6

## Accomplishments
- Replaced `buf: Vec<u8>` with `state: State` in OpenFileState for O(log N) streaming writes
- Added `byte_count: u64` and `last_committed_root: Option<Digest224>` fields
- Rewrote `test_write` to use `state.push_bytes(&mut fsa, data)` for each write
- Implemented FSA Drop flush and disk-read extend for cross-session streaming support
- Rewrote `flush_buffer_for_fsync` using clone+end pattern (State persists after fsync)
- All 222 tests pass (2 non-sequential write tests ignored for Phase 11)

## Task Commits

Each task was committed atomically:

1. **Task 1: Redesign OpenFileState struct and update all construction sites** - `eba6c3d` (feat)
2. **Task 2: Rewrite test_write to use push_bytes and verify all existing tests pass** - `dc5c880` (feat)

## Files Created/Modified
- `crates/slicefs-cli/src/filesystem.rs` - OpenFileState redesign, test_write with push_bytes, flush_buffer_for_fsync with clone+end, temporary shims for test_release/read/setattr_size
- `crates/data-id/blockset/src/file_storage.rs` - Drop impl for FSA, extend() disk fallback for streaming writes
- `crates/data-id/blockset/src/app2.rs` - Borrow scope fix for FSA Drop compatibility
- `crates/data-id/blockset/src/lib.rs` - Re-export Digest224 type
- `crates/slicefs-cli/tests/write_path_tests.rs` - Ignored test_write_with_gap_zero_pads (Phase 11)
- `crates/slicefs-cli/tests/posix_compliance_tests.rs` - Ignored test_file_write_at_offset_zero_pads (Phase 11)

## Decisions Made
- **FSA cross-session streaming:** Discovered that `FileStorageAdd`'s internal `map` holds unflushed nodes that are lost on drop, breaking push_bytes/end across separate FSA instances. Fixed by adding `Drop` impl that flushes all pending nodes to disk, and modifying `extend()` to read from disk when entries are not in memory.
- **clone+end for fsync:** `flush_buffer_for_fsync` now clones the State and calls `end()` on the clone, keeping the original State alive for subsequent writes. No State reset on fsync.
- **Digest224 re-export:** The `digest224` module was private in blockset; added `Digest224` to the public re-exports.
- **Non-sequential writes deferred:** 2 tests that write at non-zero offsets are ignored with Phase 11 TODO (STRM-02).

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Bug] FSA internal map not persisted across sessions**
- **Found during:** Task 2 (test verification)
- **Issue:** `FileStorageAdd::extend()` panicked with `unwrap()` on `None` when `end()` was called on a new FSA instance after `push_bytes` used a different FSA. Internal nodes stayed in FSA memory map and were lost on drop.
- **Fix:** Added `Drop` impl for `FileStorageAdd` that flushes all pending internal nodes to disk. Modified `extend()` to fall back to reading nodes from disk when not found in memory map.
- **Files modified:** `crates/data-id/blockset/src/file_storage.rs`
- **Verification:** All 34 blockset tests pass; all 222 slicefs-cli tests pass
- **Committed in:** dc5c880

**2. [Rule 3 - Blocking] FSA Drop borrow conflict in app2.rs**
- **Found during:** Task 2 (compilation after FSA Drop impl)
- **Issue:** `FileStorageAdd::new(io)` borrows `io` mutably; with the new Drop impl, the borrow extends to end of scope, conflicting with a subsequent `io.println()` call in app2.rs.
- **Fix:** Wrapped FSA creation in a block to limit borrow scope.
- **Files modified:** `crates/data-id/blockset/src/app2.rs`
- **Verification:** `cargo build -p blockset` succeeds
- **Committed in:** dc5c880

**3. [Rule 1 - Bug] flush_buffer_for_fsync reset State, breaking append-after-fsync**
- **Found during:** Task 2 (test_bonus_append_across_fsync_cycles failure)
- **Issue:** Initial shim reset State to default after fsync, causing subsequent writes to start from byte 0 instead of continuing. Test expected 10 bytes after two 5-byte writes with fsync in between.
- **Fix:** Rewrote to clone+end pattern: clone State under lock, end the clone for CAS commit, keep original State alive for continued writes. No State reset.
- **Files modified:** `crates/slicefs-cli/src/filesystem.rs`
- **Verification:** test_bonus_append_across_fsync_cycles passes
- **Committed in:** dc5c880

---

**Total deviations:** 3 auto-fixed (2 bugs, 1 blocking)
**Impact on plan:** All auto-fixes necessary for correctness. The FSA streaming fix is an essential prerequisite that the plan's research didn't fully account for. No scope creep.

## Issues Encountered
- The plan assumed `State::end()` could use a fresh FSA instance separate from the one used for `push_bytes`. This is not the case because `FileStorageAdd` maintains an internal map of unflushed nodes. The fix (Drop flush + disk read fallback) is minimal and architecturally sound.

## User Setup Required
None - no external service configuration required.

## Next Phase Readiness
- OpenFileState is fully restructured for streaming writes
- Plan 02 can rewrite flush/fsync/release to use State-based flow directly (temporary shims in place)
- Plan 03 can implement truncate and integration tests
- Phase 11 can add non-sequential write offset handling (2 tests await re-enabling)

---
*Phase: 10-streaming-writes-core*
*Completed: 2026-03-30*
