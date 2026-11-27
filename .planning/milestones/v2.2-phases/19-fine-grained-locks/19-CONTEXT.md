# Phase 19: Fine-grained locks - Context

**Gathered:** 2026-11-21
**Status:** Ready for planning

<domain>
## Phase Boundary

Phase 19 makes requests safe and parallel on one file and on many files (issue #21). It covers LOCK-01, LOCK-03, LOCK-04 and LOCK-05.

- No global lock is held during disk I/O, a hash calculation or a wait.
- Per-file state stays in its table behind its own lock. Writes, flush and setattr no longer take state out of `open_files`. The cross-file path in `keep_budget` stays until Phase 20 (LOCK-02).
- Locks have one fixed rank, and a debug-build checker fails a test on a rank violation.
- Each log record is appended under the same locks as its apply. Replay gives the same state as the apply, including creates by logged inode number. The body of issue #21 is updated to match.
- With several handles on one inode in different modes (read-only, read-write, `O_APPEND`), the harness shows the POSIX.1 2.9.7 rules (ROADMAP Phase 19 criterion 6).
- The Phase 18 stress harness target `serialize_off_target` (one shared file, free threads) passes and is no longer `#[ignore]`.

Out of scope: the dirty budget flusher and `keep_budget` (Phase 20), the worker pool (Phase 21), WAL group commit (deferred).

</domain>

<decisions>
## Implementation Decisions

### Lock crate
- Replace `std::sync::{Mutex, RwLock, Condvar}` with `parking_lot` 0.12 (feature `arc_lock`) in the hot crates (`slicefs-cli`, `metadata`, `chunk-store`), through one `crate::sync`-style shim that the `lock-stats` wrapper and the rank checker wrap. Use 0.12, not 0.13, so the tree keeps one copy with the vendored fuser. Add a `deadlock-detect` feature (`parking_lot/deadlock_detection`) for stress builds (user choice, 2026-11-21) — **Reversibility:** costly — about 59 lock call sites change, and the shim becomes the lock API of three crates.

### Read while write
- A write does its rebase step (chunk, hash, store put) outside the file lock. Under the lock it copies the range to rebase. It releases the lock and does the I/O. It takes the lock again and publishes the result only if the file generation did not change; otherwise it drops the result and retries later. Readers hold the file lock only for memory copies (copy the overlay view or clone the stream), and then read with no lock. A read sees all of a write or none of it, because a write applies to the overlay under the file lock (user choice, 2026-11-21) — **Reversibility:** costly — it changes the write path and the overlay publish protocol that Phase 20's flusher reuses.

### Decided autonomously
- [Locks] Q: "Per-file lock structure?" → Selected: "Each open inode has one `Arc<FileCell>` in a sharded `ino -> Arc<FileCell>` map. All handles of the inode share it, so `write_alias`, `owner_fh` and the heir swap go away. The last handle drop runs `finalize_write`." — internal structure, as in `.planning/research/ARCHITECTURE.md` Part 1.
- [Locks] Q: "Namespace consistency for inodes that are not open?" → Selected: "`NamespaceLocks`: a fixed striped lock array; multi-inode operations take their stripes in ascending stripe order; a rename mutex for cross-directory rename comes first" — research marks this reversible; the harness confirms it.
- [Locks] Q: "Single-inode metadata read-modify-write?" → Selected: "`DictMetadataStore::modify_inode(ino, FnOnce(&mut InodeMeta))` under the shard lock of `inode_live`" — internal API.
- [Locks] Q: "Lock rank?" → Selected: "1 commit gate (shared), 2 rename mutex, 3 namespace stripes (ascending), 4 `FileCell.state`, 5 short maps (`handles`, `orphans`, cell map shards, `readers` map), 6 store internals (`DictMetadataStore`, WAL, `ChunkStore`). Checker: thread-local stack of held ranks behind `debug_assertions`" — debug-only code.
- [Locks] Q: "`readers` map?" → Selected: "`HashMap<ino, Arc<Mutex<Reader>>>`; clone the Arc under the map lock and lock the per-inode reader only for the read" — internal.
- [Log] Q: "Log order?" → Selected: "The log record append (a memory copy plus an LSN) happens under the same locks as its apply; disk write and sync happen outside the locks. Replay creates an inode with the logged inode number" — owner decision from the milestone; only the code shape is decided here.
- [Log] Q: "Issue #21 body?" → Selected: "Update it in this phase to match the log-order rule (owner decision); no licensing text" — public text, but the owner already asked for it.
- [Tests] Q: "Per-write test counters?" → Selected: "Make `write_rebase_bytes`, `write_base_bytes`, `last_rebase_bytes` thread-local or test-only" — test code.
- [Tests] Q: "How are LOCK-05 and the POSIX rules shown?" → Selected: "Extend the Phase 18 harness: several handles per inode with read-only, read-write and `O_APPEND` modes; checks for visibility across handles, all-or-none reads, and appends that never overlap; lock wait rows show that readers do not wait for writer I/O; many-files runs show no cross-file contention" — test code.
- [Research] Q: "Append offset?" → Selected: "The phase research checks how the kernel FUSE client computes the `O_APPEND` offset in the SliceFS mount mode; the daemon takes the end-of-file offset from the current size under the file lock" — research item.

### Answers to the research open questions (decided autonomously, 2026-11-21)
- [Scope] `keep_budget` cannot stay as written once state no longer leaves `open_files`. This phase adapts it to per-cell dirty atomics and a global total (each writer steps only its own file inline). The background flusher and the throttle stay in Phase 20 (LOCK-02, LOCK-06) — the research shows the old take-out path cannot survive the cell change.
- [LOCK-01] A page-cache `pwrite` (no fsync) under `ChunkStore::inner` does not count as disk I/O. Pack create, seal join, index flush, fsync and the `KEEP_CAP` wait do count, and must leave the lock.
- [LOCK-03] Ascending stripe order is the way to get "two inodes in increasing inode order": it gives the same deadlock freedom and also covers two inodes in one stripe. The plan that adds the stripes updates the LOCK-03 text and ROADMAP criterion 4 to say "in ascending stripe order (inode number hashed to a stripe)".
- [Rank checker] Model the std commit gate in the rank checker through helper methods (rank 1), so a test fails when a cell or stripe is taken without a shared gate hold.
- [O_APPEND] No extra macOS CI probe. The daemon keeps a per-handle append flag, takes EOF under the cell lock, and logs the effective offset; this is correct whatever offset the kernel sends.
- [Deferred] The ancestor check for cross-directory rename is not in this phase (see Deferred Ideas).
- [Std types kept] `Arc<Mutex<StoreIo>>` (about 115 construction sites) and the std commit gate stay std types in this phase, as the research recommends.

### Claude's Discretion
- Shard counts, stripe count, and the exact shim API, inside the rules above.

</decisions>

<code_context>
## Existing Code Insights

### Reusable Assets
- `crates/slicefs-cli/tests/stress_harness.rs`: the Phase 18 harness (model check, planted-bug test, `serialize_off_target`, `long_run`).
- `crates/chunk-store/src/lock_stats.rs`: the `lock-stats` recorder (7 sites) with optional `hdrhistogram`.
- `crates/slicefs-cli/tests/lock_stats_report.rs`: checks that all lock labels record.

### Established Patterns
- `SliceFsFilesystem` in `crates/slicefs-cli/src/filesystem.rs`: `open_files`, `write_alias`, `readers`, `handles`, `orphans` (all `Mutex`), `commit_gate: Arc<RwLock<()>>`.
- Take-out sites: writes, flush and setattr remove `OpenFileState` from `open_files` and put it back; `keep_budget` takes another file's state.
- `handlers.rs`: apply first, append to the log afterwards, in a separate critical section.

### Integration Points
- `docs/research/multithreaded-request-handling.md` steps 2-5 and the Phase 18 spike results (S3: `readers` is the waiting lock at N=8; S5: release the gate before any wait).
- `.planning/research/ARCHITECTURE.md` Part 1 (the 15 single-thread dependencies and what each becomes).

</code_context>

<specifics>
## Specific Ideas

- `serialize_off_target` must pass in the suite at the end of the phase. Phase 19 concurrency tests stay below the dirty budget, because `keep_budget` stays until Phase 20.
- Keep tests short. Never hold whole files in RAM.

</specifics>

<deferred>
## Deferred Ideas

- Ancestor check for cross-directory rename (a directory must not move into its own subtree under concurrency). It does not exist today; add it as a follow-up issue or a later phase.

</deferred>
