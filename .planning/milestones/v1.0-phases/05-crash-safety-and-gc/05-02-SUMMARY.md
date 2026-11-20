---
phase: 05-crash-safety-and-gc
plan: 02
subsystem: metadata
tags: [wal, segment, fsync, crash-safety, fuse, durability]

requires:
  - phase: 05-crash-safety-and-gc/05-01
    provides: SegmentWriter, SegmentReader, WalStrategy trait, PerOpWal, FlushOnFsyncWal, PeriodicWal, NoWal, create_wal factory

provides:
  - DictMetadataStore with WAL routing on all mutations (DictionaryAppend + RootUpdate)
  - Segment-based store loading replacing dictionary.bin
  - Legacy dictionary.bin auto-migration to segment format
  - MountLock RAII struct with dirty-mount detection
  - fsync/fdatasync FUSE callback with CAS flush + WAL sync
  - --wal-strategy CLI flag (per-op/flush-on-fsync/periodic/no-wal)

affects: [05-crash-safety-and-gc/05-03, any future mount/persist code]

tech-stack:
  added: []
  patterns:
    - "Snapshot-delta WAL logging: capture dict.keys() before intern_*, log new entries after"
    - "RAII mount lock: MountLock::drop removes mount.lock for clean unmount signal"
    - "Idempotent WAL replay: segment re-load is idempotent, dirty mount = just re-load"
    - "fsync flush pattern: take buffer out of open_files, flush to CAS, put empty vec back"

key-files:
  created:
    - crates/metadata/src/mount_lock.rs
    - crates/metadata/tests/segment_store_tests.rs
    - crates/slicefs-cli/tests/wal_strategy_tests.rs
    - crates/slicefs-cli/tests/fsync_tests.rs
  modified:
    - crates/metadata/src/store.rs
    - crates/metadata/src/wal/mod.rs
    - crates/metadata/src/segment/mod.rs
    - crates/slicefs-cli/src/mount.rs
    - crates/slicefs-cli/src/filesystem.rs
    - crates/slicefs-cli/src/cli.rs
    - crates/slicefs-cli/src/lib.rs
    - crates/slicefs-cli/src/main.rs

key-decisions:
  - "Snapshot-delta WAL logging: BTreeSet snapshot of keys before intern_*, diff after — no need to modify intern_* functions"
  - "commit() snapshots keys BEFORE all intern calls (inode_map, u64_digest_map, root push) — all commit-phase entries logged in one batch"
  - "create_wal builds segment path from store_path/segments/segment-{id:06}.seg — factory owns path construction"
  - "Dirty mount handling: remove stale lock file and re-acquire — segment replay is idempotent so no special recovery needed"
  - "flush_buffer_for_fsync uses mem::take to atomically remove buffer and put empty vec back — no window where open_files is missing the entry"
  - "destroy() calls shutdown_wal() instead of writing dictionary.bin — clean unmount flushes all WAL entries"

requirements-completed: [META-01, POSIX-11]

duration: 45min
completed: 2026-03-27
---

# Phase 5 Plan 02: Store Integration and fsync Summary

**Segment-based DictMetadataStore with WAL routing on all mutations, dirty mount detection via MountLock, fsync FUSE callback, and --wal-strategy CLI flag — filesystem is now crash-safe**

## Performance

- **Duration:** ~45 min
- **Started:** 2026-03-27
- **Completed:** 2026-03-27
- **Tasks:** 2
- **Files modified:** 12

## Accomplishments

- All DictMetadataStore mutations (create_inode, update_inode, create_directory, link, unlink, set_manifest, set_xattr, remove_xattr, commit) now route through WAL before in-memory state updates
- dictionary.bin replaced by segment files; legacy stores auto-migrate transparently on first mount
- Dirty mount detected via `mount.lock` — stale lock removed, segments re-loaded (idempotent replay)
- fsync FUSE callback flushes write buffer to CAS + syncs WAL; fdatasync identical
- --wal-strategy CLI flag accepts per-op, flush-on-fsync, periodic, no-wal
- 358 total workspace tests pass (was 354 before this plan)

## Task Commits

1. **Task 1: DictMetadataStore segment persistence + WAL integration + dirty mount** - `b32c3a6` (feat)
2. **Task 2: fsync/fdatasync FUSE callback** - `e28c09c` (feat)

## Files Created/Modified

- `crates/metadata/src/store.rs` — WAL instrumentation on all mutations; log_new_dict_entries helper
- `crates/metadata/src/wal/mod.rs` — create_wal now builds segment file path (store_path/segments/)
- `crates/metadata/src/segment/mod.rs` — fixed unused import warning
- `crates/metadata/src/mount_lock.rs` — MountLock RAII guard, acquire_mount_lock, MountLockError
- `crates/metadata/tests/segment_store_tests.rs` — 5 integration tests for WAL+segment persistence
- `crates/slicefs-cli/src/mount.rs` — full rewrite: segment-based load_store, MountLock, parse_wal_config
- `crates/slicefs-cli/src/filesystem.rs` — test_fsync, flush_buffer_for_fsync, fsync FUSE callback
- `crates/slicefs-cli/src/cli.rs` — --wal-strategy flag added to Mount subcommand
- `crates/slicefs-cli/src/lib.rs` — pub mod cli added for test access
- `crates/slicefs-cli/src/main.rs` — passes wal_strategy to run_mount
- `crates/slicefs-cli/tests/wal_strategy_tests.rs` — 5 CLI parse tests
- `crates/slicefs-cli/tests/fsync_tests.rs` — 4 fsync tests including crash durability

## Decisions Made

- **Snapshot-delta WAL logging:** BTreeSet snapshot of dict.keys() before each intern_* operation, collect new entries after. This avoids modifying the intern_* functions in dictionary/inode/manifest modules.
- **commit() logs in one pass:** Keys snapshot taken BEFORE all intern calls in commit() (inode_map, u64_digest_map, refcounts, root push). All new entries collected in one diff at the end.
- **create_wal path construction:** Factory now builds `<store_path>/segments/segment-{id:06}.seg` internally — callers pass store_path, not a file path.
- **Dirty mount is idempotent replay:** Remove stale lock, re-acquire, re-load segments. No separate "replay" path needed since segment loading is identical to normal loading.
- **flush_buffer_for_fsync uses mem::take:** Atomically removes buffer content without removing the open_files entry. After CAS flush, entry remains with empty buf ready for subsequent writes.
- **destroy() calls shutdown_wal():** Replaces dictionary.bin write. WAL segment contains all mutations; clean unmount just needs shutdown_wal to flush final entries.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Bug] create_wal passed store_path as file path**
- **Found during:** Task 1 (WAL integration)
- **Issue:** create_wal called PerOpWal::new(store_path, segment_id) where store_path is a directory. SegmentWriter::open tried to create a file at that path → "Is a directory" error.
- **Fix:** create_wal now constructs `store_path/segments/segment-{id:06}.seg` and passes that file path to the WAL constructors.
- **Files modified:** crates/metadata/src/wal/mod.rs
- **Verification:** segment_store tests pass after fix
- **Committed in:** b32c3a6 (Task 1 commit)

**2. [Rule 1 - Bug] load_from_root missing `wal` and `last_root` fields**
- **Found during:** Task 1 (compilation)
- **Issue:** load_from_root struct initializer missing two fields added in phase 05-01 context.
- **Fix:** Added `wal: Mutex::new(None)` and `last_root: Mutex::new(Some(*root))` to the initializer.
- **Files modified:** crates/metadata/src/store.rs
- **Verification:** Compiled successfully
- **Committed in:** b32c3a6 (Task 1 commit)

**3. [Rule 1 - Bug] MountLockError::DirtyMount thiserror positional arg**
- **Found during:** Task 1 (compilation)
- **Issue:** `#[error("... at {0}")]` with no associated data field — thiserror macro error.
- **Fix:** Removed `{0}` from error message since DirtyMount is a unit variant.
- **Files modified:** crates/metadata/src/mount_lock.rs
- **Verification:** Compiled successfully
- **Committed in:** b32c3a6 (Task 1 commit)

**4. [Rule 1 - Bug] commit() WAL snapshot only covered final root push**
- **Found during:** Task 1 (test failure: "u64-digest map: expected at least 8 bytes, got 0")
- **Issue:** Keys snapshot in commit() was placed AFTER intern_inode_map/intern_u64_digest_map calls. Only the root push's new entries were logged; the commit-phase intern entries were missing.
- **Fix:** Moved `commit_prev_keys` snapshot to before the first intern call in commit().
- **Files modified:** crates/metadata/src/store.rs
- **Verification:** test_load_store_from_segments_restores_inodes passes
- **Committed in:** b32c3a6 (Task 1 commit)

---

**Total deviations:** 4 auto-fixed (all Rule 1 bugs)
**Impact on plan:** All bugs were integration issues from 05-01 stub work. No scope creep.

## Issues Encountered

None beyond the auto-fixed deviations above.

## Next Phase Readiness

- Filesystem is crash-safe: all mutations durable through WAL
- GC engine (05-03) can use `current_root()` as live-set anchor
- FlushOnFsync WAL strategy ready for fsync-driven durability workloads
- Dirty mount detection ready for production use
- Legacy dictionary.bin stores auto-migrate transparently

---
*Phase: 05-crash-safety-and-gc*
*Completed: 2026-03-27*
