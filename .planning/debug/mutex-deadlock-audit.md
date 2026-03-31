---
status: awaiting_human_verify
trigger: "mutex-deadlock-audit: 14 zombie mount processes, deadlock suspected in Mutex usage"
created: 2026-03-30T00:00:00Z
updated: 2026-03-30T00:01:00Z
---

## Current Focus

hypothesis: No true deadlock cycle exists, but severe io lock contention in commit() + Mutex poisoning risk from unwrap()-on-get inside lock scopes causes cascading FUSE callback failures
test: Identified specific code patterns; preparing fix
expecting: Fixing poisoning risk and reducing lock contention will prevent zombie mounts
next_action: Fix the identified issues

## Symptoms

expected: SliceFS mount should respond to filesystem operations without hanging
actual: Multiple mount processes accumulated and became unresponsive. Mount point shows "Loading..." in Finder, "permission denied" in terminal. 14 stale process pairs (slicefs + go-nfsv4) were found.
errors: zsh: permission denied: /tmp/slicefs-mount (terminal), "Loading..." stuck forever (Finder)
reproduction: Mount slicefs, perform operations, eventually the mount becomes unresponsive
started: Observed after Phase 7+ changes. Previous fixes for flush/setxattr/O_TRUNC applied.

## Eliminated

- hypothesis: True deadlock cycle between open_files and io locks in filesystem.rs
  evidence: Traced all lock acquisition paths. Only test_write holds open_files->io (sequential write path). No code path acquires io then open_files. No cycle exists.
  timestamp: 2026-03-30T00:00:30Z

- hypothesis: Cross-module deadlock between filesystem.rs open_files and DictMetadataStore internal locks
  evidence: DictMetadataStore never accesses open_files. filesystem.rs properly drops open_files before calling meta.* methods in all paths except test_write sequential (which only acquires io, not meta locks).
  timestamp: 2026-03-30T00:00:30Z

- hypothesis: FUSE callback reentrancy deadlock (callback holding lock triggers another callback)
  evidence: FUSE callbacks don't trigger other FUSE callbacks. Lock scopes don't contain any FUSE-triggering operations.
  timestamp: 2026-03-30T00:00:30Z

- hypothesis: Background GC thread deadlock with FUSE callbacks
  evidence: GC only calls snapshot_roots() (brief lock on snapshots_by_version and last_root) then run_gc_roots_only (no store locks). No contention path with FUSE.
  timestamp: 2026-03-30T00:00:30Z

- hypothesis: WAL flush under lock deadlock
  evidence: commit() drops io_guard before log_wal_entry(). flush_buffer_for_fsync drops all locks before meta.flush_wal(). log_wal_entry comment warns about this and all callers comply.
  timestamp: 2026-03-30T00:00:30Z

## Evidence

- timestamp: 2026-03-30T00:00:10Z
  checked: filesystem.rs lock ordering for open_files and io
  found: test_write sequential path (line 283-357) holds open_files + io simultaneously. This is the only nested lock pattern in filesystem.rs.
  implication: Not a deadlock since no reverse ordering exists, but holds io for duration of push_bytes which blocks all concurrent FUSE ops.

- timestamp: 2026-03-30T00:00:15Z
  checked: DictMetadataStore commit() lock holding duration
  found: commit() holds io lock from line 707 to line 770 (~60 lines) while serializing ALL metadata maps (inode_data, dir_data, manifest_data, xattr_data, refcounts).
  implication: ALL FUSE callbacks that need io (getattr, lookup, read, write, readdir, etc.) block during commit(). Combined with FUSE-T NFS timeouts, this could cause "Loading..." stall.

- timestamp: 2026-03-30T00:00:20Z
  checked: create_directory() line 509 for panic risk under lock
  found: `*self.dir_data.lock().unwrap().get(&parent_ino).unwrap()` can panic (TOCTOU race on dir_data) while holding io_guard. Would poison both dir_data AND io mutexes.
  implication: If this panic fires, every subsequent FUSE callback panics on io.lock().unwrap(), making mount completely unresponsive.

- timestamp: 2026-03-30T00:00:25Z
  checked: All .lock().unwrap() usage patterns across filesystem.rs and store.rs
  found: 34 lock().unwrap() in filesystem.rs, 82 in store.rs. All use unwrap() meaning any Mutex poisoning cascades to all consumers.
  implication: A single panic in any thread holding io, open_files, or any store lock will cascade and crash all FUSE callbacks.

- timestamp: 2026-03-30T00:00:35Z
  checked: commit() io lock scope for reducibility
  found: commit() acquires inode_map+io at start, drops inode_map at line 712, then holds io while serializing 5 BTreeMaps. Each map serialization (intern_u64_digest_map) acquires the map lock briefly, serializes under io/fsa, releases map lock.
  implication: The io lock MUST be held during fsa serialization (fsa borrows io). Cannot trivially split. Need architectural change to reduce contention.

## Resolution

root_cause: Two compounding issues cause mount unresponsiveness:

1. **Mutex poisoning vulnerability** (HIGH): create_directory() line 509 has `.get(&parent_ino).unwrap()` while holding `io` lock. A TOCTOU race between the dir_data check (line 493) and this line can cause a panic that poisons the `io` Mutex. Once poisoned, ALL FUSE callbacks panic on `io.lock().unwrap()`, making the mount completely unresponsive. Similar patterns exist elsewhere in store.rs (commit() assert_eq on line 765).

2. **Severe io lock contention** (MEDIUM): `commit()` holds the `io` lock for the duration of serializing ALL metadata maps (potentially hundreds of milliseconds for large filesystems). During this window, every FUSE callback that needs `io` blocks. FUSE-T NFS layer interprets slow responses as timeouts, causing the "Loading..." hang. `test_write` sequential path also holds both `open_files` and `io` for the duration of `push_bytes`, adding to contention.

fix: Two changes applied:

1. **store.rs create_directory()**: Moved `dir_data` lookup BEFORE acquiring `io` lock. Previously, `.get(&parent_ino).unwrap()` was called while holding `io`, which could panic on TOCTOU race and poison the io Mutex. Now returns MetaError::NotADirectory gracefully instead of panicking.

2. **filesystem.rs test_write()**: Changed from holding `open_files` lock during the entire write (including CAS I/O via push_bytes) to a remove-process-reinsert pattern. The file state is temporarily removed from the HashMap, open_files is released, then io is acquired for push_bytes, and finally the state is re-inserted. This eliminates the open_files->io nested lock pattern and prevents concurrent FUSE reads from blocking on open_files during slow CAS writes.

verification: cargo build succeeds, all 548 tests pass (0 failures)
files_changed:
- crates/metadata/src/store.rs (create_directory: moved dir_data lookup before io lock)
- crates/slicefs-cli/src/filesystem.rs (test_write: remove-process-reinsert pattern to avoid nested locks)
