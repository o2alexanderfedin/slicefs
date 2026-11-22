---
gsd_state_version: "1.0"
milestone: v2.2
milestone_name: Concurrency & Store Evolution
current_phase: 19
current_phase_name: Fine-grained locks
status: executing
stopped_at: Completed 19-12-PLAN.md
last_updated: "2026-11-22T14:21:37.460Z"
last_activity: 2026-11-22
last_activity_desc: Phase 19 execution started
state_head: a7bff74cefc967ad0d48672c0a9b8b67c71e7bd0
progress:
  total_phases: 7
  completed_phases: 1
  total_plans: 23
  completed_plans: 19
  percent: 14
---

# Project State

## Project Reference

See: .planning/PROJECT.md (updated 2026-11-20)

**Core value:** High-ratio data deduplication that works transparently as a real, daily-driver POSIX filesystem
**Current focus:** Phase 19 — Fine-grained locks

## Current Position

Phase: 19 (Fine-grained locks) — EXECUTING (gap closure)
Plan: 13 of 16
Status: Gap-closure plans 19-12..19-16 ready to execute (LOCK-01, LOCK-04, LOCK-05)
Last activity: 2026-11-22 — Gap-closure plans 19-12..19-16 written

Progress: [█░░░░░░░░░] 14%

## Performance Metrics

**Velocity:**
- Total plans completed: 0 (v2.2)

*Updated after each plan completion*
**Per-Plan Metrics:**

| Plan | Duration | Tasks | Files |
|------|----------|-------|-------|
| Phase 18 P01 | 25min | 2 tasks | 6 files |
| Phase 18 P04 | 45min | 2 tasks | 6 files |
| Phase 18 P02 | 35min | 2 tasks | 7 files |
| Phase 18 P03 | 45min | 3 tasks | 1 files |
| Phase 18 P05 | 60min | 3 tasks | 7 files |
| Phase 18 P06 | 75min | 3 tasks | 8 files |
| Phase 18 P07 | 35 min | 2 tasks | 1 files |
| Phase 19 P01 | 60min | 3 tasks | 23 files |
| Phase 19 P02 | 52min | 3 tasks | 6 files |
| Phase 19 P03 | 50min | 3 tasks | 7 files |
| Phase 19 P04 | 45min | 3 tasks | 11 files |
| Phase 19 P05 | 41min | 3 tasks | 5 files |
| Phase 19 P06 | 25min | 2 tasks | 4 files |
| Phase 19 P07 | 95min | 3 tasks | 4 files |
| Phase 19 P08 | 45min | 3 tasks | 5 files |
| Phase 19 P09 | 55min | 3 tasks | 11 files |
| Phase 19 P10 | 55min | 3 tasks | 7 files |
| Phase 19 P11 | 52min | 3 tasks | 4 files |
| Phase 19 P12 | 70min | 3 tasks | 6 files |

## Accumulated Context

### Decisions

Full log: Key Decisions in .planning/PROJECT.md; per-milestone detail in .planning/milestones/.
Owner decisions for v2.2 (also in ROADMAP.md):

- [v2.2]: SHA-256 is a production second hash; BLAKE3-256 stays the default; rehash is offline only
- [v2.2]: A worker panic aborts the daemon; the write log replays at the next mount
- [v2.2]: Each log record is appended under the same locks as its apply (supersedes the #21 table)
- [v2.2]: Chunk profile is fixed at seed; the default stays 4/8/32 KiB
- [v2.2]: Prolly = prototype and written decision, no migration; macOS worker default is 1 until S1 is recorded
- [Phase 18]: Plan 01: lock-stats uses optional hdrhistogram in chunk-store (D-02); lock_stats::Guard is the std guard when the feature is off
- [Phase 18]: S1: FUSE-T 1.2.9 on macOS FSKit overlaps requests (max_inflight=2) and accepts out-of-order replies; macOS worker default may exceed 1 (Phase 21 to show throughput)
- [Phase 18]: 18-02: lock report test is its own binary (process-global recorder); read_root readers guard left as is for S3
- [Phase 18]: 18-03: short stress runs sized 4x150 ops (one-file) and 4x40 ops (many-files) to fit the 5 s budget; fsync and release cost 20-40 ms each in debug
- [Phase 18]: 18-03: serialize-off target fails today on the shared file (reads match no allowed version); many-files with one owner per file passed with serialize off
- [Phase 18]: 18-05: S4 fusectl mounts with SYS_ADMIN alone (no --privileged); S6/S4/S2 numbers saved in runs/ for Plan 07; S2 on overlayfs, 4 GiB max write 1353 ms in one of three runs
- [Phase 18]: S3 MB/s comes from a feature-off pass and lock rows from a feature-on pass; throughput counts time inside the wrapped calls only
- [Phase 18]: S7 runs a 30 s commit thread like the daemon and reports the 256 MiB cap without asserting it
- [Phase 18]: S3 safe subset (own file per thread, 4 MiB dirty limit, N x 4 MiB below the budget) ran clean with serialize off on macOS and Linux
- [Phase 18]: S1: the macOS default can be more than one worker (FUSE-T 1.2.9 overlaps requests and accepts out-of-order replies); throughput still to be shown in Phase 21
- [Phase 18]: S5: keep std RwLock for the commit gate; release the gate before any wait is required for steps 5 and 6
- [Phase 18]: S2: #108 closes without threads for the typical case; 1 of 6 runs missed the 500 ms bound (1353 ms at 4 GiB), repeat on btrfs before closing
- [Phase 19]: commit_lock of DictMetadataStore has Rank::Rename (2): a commit cut holds it while it locks file-system maps
- [Phase 19]: WritePacer state has Rank::Store (6): the chunk store calls the pacer with its gc lock held
- [Phase 19]: Site::ALL is a slice; shim try_lock runs the rank check
- [Phase 19]: Plan 19-02: seal join, pack create and KEEP_CAP wait leave ChunkStore::inner by a check-wait-retry loop in put; one create and one flush at a time, marked in Inner
- [Phase 19]: Plan 19-02: index flush uses a frozen memtable; run and MANIFEST writes run without the store lock; GC install waits for a running flush
- [Phase 19]: Plan 19-02: trust floor has its own Rank::Store lock held across the TRUST write; read opens uncached packs without the store lock and re-checks the place
- [Phase 19]: 19-03: modify_inode under the inode_live shard lock; filesystem read-modify-write helpers use it
- [Phase 19]: 19-03: inode_live and dir_live are 32 shards (Rank::Store); store order inode_map, inode shard, inode_data; dir shard, dir_data
- [Phase 19]: 19-03: Live.flushing holds a ticket per take; install only on the same ticket (a commit and a cache flush can take one entry)
- [Phase 19]: 19-03: create_*_at counts a number as taken if it is in inode_map, the cache or inode_data; reserve under inode_map
- [Phase 19]: 19-03: hot path uses NodeReader/NodeWriter on the store's Arc<ChunkStore>; unsynced_puts and first put error kept in the store
- [Phase 19]: 19-04: WAL append is a memory copy plus an LSN under one short queue lock; write_out drains in LSN order; one WAL lock order writer, queue or pending, loc, behind
- [Phase 19]: 19-04: PerOp write-out failure is sticky until a cut or rotate, so a record dropped by another thread's failed write-out is still reported
- [Phase 19]: 19-04: WAL fsync runs on a second file handle (begin_sync/end_sync with a SyncMark of segment id and write count); Behind::settle syncs with the behind lock released
- [Phase 19]: 19-05: cell map 16 shards (ino mod 16); orphan set lives in the handle table under one lock with the open count
- [Phase 19]: 19-05: dirty_total moves by delta in a CellGuard drop under the cell lock; keep_budget steps only the writer's own cell (D-13)
- [Phase 19]: 19-05: code under a cell lock must use the inner forms (*_open, *_state, finalize_write); with_cell/open_write/release_write lock the cell again
- [Phase 19]: 19-06: overlay generations come from one process-wide counter, so a new overlay never repeats one
- [Phase 19]: 19-06: a dropped rebase result is retried at once with the cell locked; past the hard cap (limit + 2 * REBASE_STEP) a writer stores a locked step
- [Phase 19]: 19-06: finalize keeps the three rebase phases in one cell-lock hold until Plan 07
- [Phase 19]: Stream generations share the process-wide overlay counter; a stream step publishes only at the same generation
- [Phase 19]: A flush installs only if last_published_gen is older than its snapshot; overlay switch and truncate to 0 also set it
- [Phase 19]: The last release finalizes in steps of FINAL_STEP (8 MiB) and retires only when clean with no write handle
- [Phase 19]: A closed-file truncate runs in a transient cell locked before it goes into the map
- [Phase 19]: 19-08: a Release record is appended only by the release that retired the cell; Flush only when its Phase C installed; replay never appends
- [Phase 19]: 19-08: an O_APPEND write takes EOF under the cell lock in every write-loop turn; the Write record carries the effective offset
- [Phase 19]: Rank 2 is shared by the commit lock and the rename mutex; no thread holds both, and the checker refuses two Rank::Rename locks at once
- [Phase 19]: Namespace ops lock 256 stripes in ascending stripe order (inode number hashed to a stripe); lock_namespace reads, locks, re-reads and retries on a change; cross-directory rename takes the rename mutex first
- [Phase 19]: The setattr size change stays under the cell lock only (it can splice); the other setattr fields and xattr changes run under stripe then cell
- [Phase 19]: Every request takes the commit gate through gate_shared (Rank::Gate entry); the harness runs with the strict-gate rule; serialize_off_target is in the default suite
- [Phase 19]: Replay skips a Setxattr/Removexattr record of an inode it already reclaimed, as it skips a write of a gone inode
- [Phase 19]: 19-10: the namespace log callback is &mut dyn FnMut(Op); the fs function builds the op in its apply hold, the handler appends it and writes out the LSN before the reply
- [Phase 19]: 19-10: a setattr with a size logs a size-only Setattr in the cell hold of the size change, then the other fields in the stripe-and-cell hold
- [Phase 19]: 19-10: the harness crash test (Config.crash) makes no commit in the last 250 ms and none at the end, so the log holds the end of the run
- [Phase 19]: 19-11: LOCK-01 not marked: the store lock is still held across disk I/O (ChunkStore::sync second-phase fsync; a record pwrite under the store or WAL writer lock stalls behind a concurrent fsync on macOS); owner decision
- [Phase 19]: 19-11: LOCK-04 not marked until the orchestrator applies the staged #21 body; mkdir and closed-file size change windows open for the owner
- [Phase 19]: 19-11: LOCK-05 not marked: the tests show readers do not wait on the main path, but finalize and write fall back to a locked rebase step or flush (store I/O in the cell hold) after a dropped or concurrent step, so a reader can wait for a writer's I/O under contention
- [Phase 19]: Gap closure 19-12..19-16 (G1-G6): one record write in flight per pack and per WAL segment (no holes, no lock across the pwrite); dirty flags clear by mark after the unlocked fsync; a per-cell step token replaces the locked fallbacks; reads compose pending writes; LOCK-04 is marked by the orchestrator after it applies the #21 body
- [Phase 19]: 19-12: ChunkStore::sync phase 2 and the index flush fsync with no store lock (sync handle and marks; dirty flags cleared by mark)
- [Phase 19]: 19-12: one record write in flight per active pack, published under the store lock; supersedes D-14
- [Phase 19]: 19-12: RetryingPack sync handle repairs in its own thread; a sync that saw a failure drops no copies; a write that saw one writes again
- [Phase 19]: 19-12: store waits for another thread's job end after STORE_WAIT (10 s) with TimedOut, without failing the store

### Pending Todos

None.

### Blockers/Concerns

- FUSE requests run on one thread; the shared dirty budget is safe only with one thread (#22, #127). Phases 19-21 remove this.
- SPIKE-03 (S1 on macOS FSKit) is a by-hand or macOS CI run; the macOS worker default stays 1 until it is recorded. It also gates POOL-04 in Phase 21.
- `keep_budget` is the last cross-file take-out of `open_files`; it stays until Phase 20 (LOCK-02). Phase 19 concurrency tests must stay below the dirty budget.
- Phase 20 (budget, flusher and gate) has the highest deadlock risk; run research-phase before planning it.
- Idle index merge can rewrite GBs after a short pause and is not stopped by new writes (#141). Not in v2.2.
- LOCK-01 open: store lock held across an fsync (ChunkStore::sync second phase) and record pwrites under the store and WAL writer locks stall 20-36 ms behind a concurrent fsync on macOS (19-11 lock rows; deferred-items.md). Decided (G1, G2); plans 19-12 (store) and 19-13 (WAL), measured in 19-16.
- LOCK-05 open: finalize and the write path fall back to a rebase step or flush with the cell locked after a dropped or concurrent step (19-06/07), so a reader can wait for a writer's I/O under contention (19-11 SUMMARY Finding 5; deferred-items.md). Decided (G3); plans 19-14 (step token, write and read paths) and 19-15 (finalize, setattr, forced-drop evidence).

## Deferred Items

| Category | Item | Status | Deferred At | Milestone |
|----------|------|--------|-------------|-----------|
| *(none)* | | | | |

## Session Continuity

Last session: 2026-11-22T14:21:37.296Z
Stopped at: Completed 19-12-PLAN.md
Resume file: None

## Operator Next Steps

- Plan the first phase with /gsd-plan-phase 18 (research-phase flagged: S5 gate fairness, S1 by hand)
- Phases 22-24 can start on separate branches in parallel with Phases 18-21
