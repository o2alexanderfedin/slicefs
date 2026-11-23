---
phase: 19-fine-grained-locks
plan: 03
subsystem: metadata
tags: [lock-01, d-05, d-08, modify-inode, sharded-cache, inode-live, dir-live, flush-tickets, storeio, memio-hold]

requires:
  - phase: 19-fine-grained-locks
    provides: "19-01 lock shim (Rank::Store, timed sites); 19-02 MemIo hold knobs (Hold enum, wait_parked)"
provides:
  - "DictMetadataStore::modify_inode(ino, FnOnce(&mut InodeMeta) -> R): one-inode change in one shard hold, logical_bytes in the same hold"
  - "MetadataStore::create_inode_at and create_directory_at (store side of D-08; replay uses them in Plan 10)"
  - "inode_live and dir_live: 32 shards each, timed as lock-stats sites inode_live and dir_live"
  - "Cold inode and directory loads with no shard lock and no io lock; cache insert only if still absent and the digest is unchanged"
  - "flush_inodes / flush_dirs: take with a ticket, write with no lock, install only if the ticket is unchanged"
  - "No StoreIo lock on the hot path (inode, directory, manifest, xattr nodes); unsynced_puts and the first put error kept in the store"
  - "MemIo::hold_pack_reads (Hold::PackReads)"
affects: [19-04, 19-05, 19-06, 19-08, 19-09, 19-10, 19-11]

estimate:
  tokens: 140000
  tasks: 3
actuals:
  tokens: 14900
  tasks: 3
  commits: 3

plan_head_before: 0c3bdc7a2c7a337d715fc329806b6e85d86f64a9
plan_head_after: bdeb0bfc5789b7d001246d82f98120373f7ac3b8
commits: 3

tech-stack:
  added: []
  patterns:
    - "Cold load: read with no lock, then under the shard lock insert only if the entry is still absent and its digest did not change (else read again)"
    - "Take with a ticket: an entry in flight has the ticket of its take; a writer installs its digest only if the entry still has that ticket"
    - "Sharded cache with global counts: the shard guard moves the atomics by its delta before it unlocks"
    - "Test hook point as a closure argument (flush_inodes_between): runs on the same thread, no timing, no stored callback"

key-files:
  created: []
  modified:
    - crates/metadata/src/store.rs
    - crates/metadata/src/store_io.rs
    - crates/slicefs-traits/src/metadata.rs
    - crates/slicefs-cli/src/filesystem.rs
    - crates/chunk-store/src/mem.rs
    - crates/chunk-store/src/lock_stats.rs
    - crates/slicefs-cli/tests/lock_stats_report.rs

key-decisions:
  - "Shard count 32 for inode_live and dir_live (Claude's discretion); a thread holds at most one shard of a cache"
  - "Lock order inside the store: inode_map, one inode_live shard, inode_data; one dir_live shard, dir_data. The rank checker allows Store on Store, so this order is kept by code, not by the checker"
  - "Live.flushing is a map ino -> ticket (not a set): with a cache flush outside the lock there can be two takers of one entry (a commit and a flush)"
  - "A commit takes the dirty entries and the entries in flight, so its root has an entry that a flush has taken but not installed"
  - "create_*_at: a number is taken if it is in inode_map, in the inode cache or in inode_data (a new inode is only in the cache until a flush); it is taken and the inode cached under inode_map"
  - "create_directory takes the number (inode cached) first, then adds the name; a name clash deletes the new inode again (no partial entry)"
  - "The commit still calls StoreIo::begin_commit_sync after the store's own put check: other StoreIo users keep their error and count, and Site::Io keeps recording"

patterns-established:
  - "A modify_inode closure must not call the store or take a lock (setattr_full updates open_files after the closure)"

requirements-completed: []

duration: 50min
completed: 2026-11-21
status: complete
---

# Phase 19 Plan 03: Metadata store hot path Summary

**`modify_inode` makes a one-inode change atomic, the inode and directory caches are 32 shards each with cold loads and flushes outside every lock, a ticket on each take keeps a late install from undoing a delete or a newer change, and the inode, directory, manifest and xattr paths no longer lock `StoreIo`.**

## Performance

- **Duration:** about 50 min
- **Completed:** 2026-11-21
- **Tasks:** 3
- **Files modified:** 7

## Per-row fixes (RESEARCH Lock Inventory A, rows 2-8)

| Row | Site | Held before | Fix | Task | Test |
|-----|------|-------------|-----|------|------|
| 2 | `with_dir` cold load | `dir_live` + `io` across `DirList::load` | `cached_dir`: read with `NodeReader` and no lock; cache only if still absent and `dir_data` has the same digest. The flush and the eviction run after the shard lock is released | 2 | `cold_dir_load_does_not_block_other_inodes` |
| 3 | `flush_dirs` | `dir_live` + `io` + `dir_data` across the intern of every dirty directory | Per shard: take with a ticket, intern with a `NodeWriter` and no lock, install under the shard lock and `dir_data` only if the ticket is unchanged | 2, 3 | covered by the inode tests (same code shape) |
| 4 | `load_meta` | `io` across `load_inode` | `cached_inode`: read with `NodeReader` and no lock; insert only if still absent and the `inode_data` digest is unchanged | 1, 2, 3 | `cold_dir_load_does_not_block_other_inodes` (B changes a cached inode) |
| 5 | `flush_inodes` | `inode_map` + `inode_live` + `io` + `inode_data` across the intern of every dirty inode | `flush_inodes_between`: per shard, take with a ticket, intern with no lock, install only if the ticket is unchanged | 2, 3 | `flush_install_does_not_resurrect_a_deleted_inode`, `late_older_install_does_not_overwrite_a_newer_change` |
| 6 | `set_manifest` / `get_manifest` | `io` across `intern_manifest` / `load_manifest` | `NodeWriter` / `NodeReader` with no lock; `manifest_data` is locked only to read or swap the digest | 3 | `manifest_read_does_not_block_inode_changes` |
| 7 | xattr methods | `io` across `load_xattrs` / `intern_xattrs` | `change_xattrs`: read and store with no lock; swap the digest only if the old one is still current, else run again | 3 | existing xattr tests (14) |
| 8 | `create_directory` | `inode_map` across `with_dir` | `insert_inode` takes the number and caches the inode, then releases `inode_map`; the parent is read and changed after that | 1 | `create_directory_at_adds_the_entry_or_nothing` |

## Task Commits

1. **Task 1: modify_inode and create by number, used by the filesystem helpers** - `39d320e` (feat)
2. **Task 2: sharded caches, cold loads and flushes with no shard lock, install checks** - `9c94d23` (feat)
3. **Task 3: no StoreIo lock on the metadata hot path** - `bdeb0bf` (feat)

**Plan metadata:** added by the final docs commit.

## TDD record (red before green, no red commit)

- Task 2, `cold_dir_load_does_not_block_other_inodes`: red on the Task 1 code, "B finished while A loaded a cold directory: Timeout" (5.00 s), with A confirmed parked in the held pack read.
- Task 2, `flush_install_does_not_resurrect_a_deleted_inode`: red with the split flush and no install check ("assertion failed: matches!(store.get_inode(x), Err(MetaError::NotFound(_)))").
- Task 2, `late_older_install_does_not_overwrite_a_newer_change`: red with a plain set check instead of tickets (size 1 came back instead of 2). Green with tickets.
- Task 3, `manifest_read_does_not_block_inode_changes`: red on the Task 2 code, "B finished while A read a manifest: Timeout" (5.01 s).
- Task 3, `hot_path_put_error_fails_the_next_commit_and_puts_are_synced`: green before and after. It guards behavior that must stay the same after the bookkeeping move (T-19-10), so it has no red run.

## Verification

- `cargo test -p metadata --lib store::`: 69 passed (Task 1 added 5 tests, Task 2 added 3, Task 3 added 2).
- Full suite command from 19-VALIDATION.md (`cargo test --workspace && cargo check --workspace --all-targets && cargo check --workspace --all-targets --features slicefs-cli/lock-stats && cargo check -p slicefs-cli --features deadlock-detect`): exit 0. 64 `test result: ok`, no `FAILED`, no `lock rank violation`.
- `cargo test -p slicefs-cli --features lock-stats --test lock_stats_report -- --nocapture`: 1 passed. Samples for all 9 sites, among them `dir_live n=70` and `inode_live n=81`; `io n=4` (commit time).
- `cargo test --workspace` exited 0 before each task commit. Task 3's last change (a lock-order comment) was checked with `cargo test -p metadata`.
- Acceptance greps: `fn modify_inode` 1; `fn create_inode_at` 1; `fn create_directory_at` 1; `modify_inode` in filesystem.rs 14; `DirLive` 3; `InodeLive` 3; `"dir_live"` 1; `fn hold_pack_reads` 1; `unsynced_puts` 5.

## Decisions Made

See `key-decisions`. All of them can be changed later. Following the delegation note, I decided them myself and report them here.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Bug] `flushing` as a set loses a change when two takers exist**
- **Found during:** Task 2 design
- **Issue:** The plan says to install a digest "only if the entry is still in `flushing`". With a cache flush outside the lock, a commit and a flush can both take one entry. The commit takes X (v1), X changes (v2), the flush takes X, and then the commit's late install finds X in the set, installs the v1 digest, and leaves v2 clean. An eviction then loses v2.
- **Fix:** `Live.flushing` maps an entry to the ticket of its take; `settle` installs only on the same ticket. A commit also takes the entries in flight. Red/green test: `late_older_install_does_not_overwrite_a_newer_change`.
- **Commit:** `9c94d23`

**2. [Rule 1 - Bug] A cold load could put back an inode that was deleted meanwhile**
- **Found during:** Task 1
- **Issue:** Insert-if-absent alone lets a load that read the digest before a `delete_inode` cache the inode again after it.
- **Fix:** The insert checks that the digest map still has the same digest, under the shard lock. `delete_inode` removes the cache entry and the digest in one shard hold (inodes and directories).
- **Commit:** `39d320e`

**3. [Rule 1 - Bug] Two creates could get the same number**
- **Found during:** Task 1
- **Issue:** `create_inode` released `inode_map` before it cached the inode. A `create_inode_at` of that number in between would see it free. A new inode is also not in `inode_map` until a flush.
- **Fix:** `insert_inode` takes or checks the number and caches the inode under `inode_map`. "Taken" means in `inode_map`, in the cache or in `inode_data`.
- **Commit:** `39d320e`

**4. [Rule 2 - Missing critical] More read-modify-write sites than the plan listed**
- **Found during:** Task 1
- **Issue:** `create_directory` (parent nlinks), `drop_last_link` (wrote a stale copy), the three size updates of `test_setattr_size` and the symlink size had the same get-then-update race.
- **Fix:** They use `modify_inode` too (helpers `stamp_size`, `drop_link`). `test_setattr_full` updates `open_files` after the closure, because a Short lock inside a Store hold breaks the rank rule.
- **Commit:** `39d320e`

**5. [Rule 2 - Missing critical] Lost xattr change between two threads**
- **Found during:** Task 3
- **Issue:** With the load and the intern outside every lock, two `set_xattr` calls on one inode could each write a set without the other's name.
- **Fix:** `change_xattrs` swaps the digest only if the old one is still current, and otherwise runs again on the newer set.
- **Commit:** `bdeb0bf`

**6. [Rule 3 - Blocking] Arc<ChunkStore> clone in Task 2, not Task 3**
- The plan puts the `cs` field in Task 3, but Task 2 needs cold loads with no `io` lock. The field and `NodeReader` reads came in Task 2; the writes and the bookkeeping came in Task 3.

**7. [Plan fact] Hook point as a closure argument**
- The hook point between the write and the install is `flush_inodes_between(between: impl FnOnce())`. `flush_inodes` calls it with an empty closure. A stored callback would need `'static`, and the commit-race test passes a `PreparedCommit` that borrows the store.

**Total deviations:** 7 (3 Rule 1, 2 Rule 2, 1 Rule 3, 1 note). **Impact:** every inventory row the plan names (2-8) leaves its lock. The install check is stronger than the plan text.

## For later plans

- Plan 10 (replay): use `create_inode_at` / `create_directory_at`; `advance_next_ino` is still there for tests.
- A `modify_inode` closure runs under an `inode_live` shard lock (Rank::Store): it must not call the store or take a Short, Cell or Stripe lock.
- The checker does not catch two shards of one cache held at once (Store on Store is allowed). New store code must hold at most one shard of each cache.
- Not closed here: `set_xattr` that runs at the same time as `delete_inode` of the same inode can leave an xattr entry for the deleted inode. The namespace stripes (Plan 09) serialize these two operations on one inode.

## Issues Encountered

- `lock_stats_report` and the stress harness take a few minutes in a full debug run. The suite ran in the background with its log in a file, and the exit code was read from that file.

## Known Stubs

None.

## Threat Flags

None. No new endpoint, file kind or trust boundary. T-19-07: the ticket check; existence follows from it, because `Live::remove` (in `delete_inode`) drops the ticket (`flush_install_does_not_resurrect_a_deleted_inode`). T-19-08: insert only if absent and the digest is unchanged. T-19-09: `AlreadyExists` with no partial entry (`create_directory_at_adds_the_entry_or_nothing`). T-19-10: the first put error is kept in the store and the next commit returns it (`hot_path_put_error_fails_the_next_commit_and_puts_are_synced`). T-19-SC: no new crate.

## Self-Check: PASSED

- This SUMMARY and the 7 modified files: FOUND
- Commits `39d320e`, `9c94d23`, `bdeb0bf`: FOUND on HEAD (`git rev-list --count 0c3bdc7..HEAD` prints 3)
