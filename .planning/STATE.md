---
gsd_state_version: "1.0"
milestone: v2.2
milestone_name: Concurrency & Store Evolution
current_phase: "21.2"
current_phase_name: Thread-safety verification and CI gates
current_plan: 4
status: executing
stopped_at: Completed 21.2-03-PLAN.md
last_updated: "2026-11-25T16:06:21.034Z"
last_activity: 2026-11-25
last_activity_desc: Phase 21.1 complete, transitioned to Phase 22
state_head: 57510b949be7e58c7c5f199af33a83eb061633dd
progress:
  total_phases: 9
  completed_phases: 4
  total_plans: 55
  completed_plans: 47
  percent: 44
---

# Project State

## Project Reference

See: .planning/PROJECT.md (updated 2026-11-20)

**Core value:** High-ratio data deduplication that works transparently as a real, daily-driver POSIX filesystem
**Current focus:** Phase 21.2 — Thread-safety verification and CI gates

## Current Position

Current Plan: 4
Total Plans in Phase: 11
Phase: 21.2 (Thread-safety verification and CI gates) — EXECUTING
Plan: 21.2-01 and 21.2-02 complete (2 of 11); next: 21.2-03
Status: Ready to execute
Last activity: 2026-11-25 — 21.2-02 complete: race (b) fixed (ancestor walk, EINVAL), stress test in the suite, 20 of 20 repeat runs pass

Progress: [████░░░░░░] 44%

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
| Phase 19 P13 | 59min | 3 tasks | 10 files |
| Phase 19 P14 | 320min | 2 tasks | 3 files |
| Phase 19 P15 | 85 min | 4 tasks | 4 files |
| Phase 19 P16 | 35 min | 3 tasks | 8 files |
| Phase 20 P01 | 60min | 3 tasks | 7 files |
| Phase 20 P02 | 50min | 2 tasks | 6 files |
| Phase 20 P03 | 45 min | 2 tasks | 7 files |
| Phase 20 P04 | 40 min | 2 tasks | 1 files |
| Phase 20 P05 | 39 min | 2 tasks | 3 files |
| Phase 20 P06 | 36 min | 2 tasks | 6 files |
| Phase 20 P07 | 55 min | 3 tasks | 4 files |
| Phase 20 P08 | 75min | 2 tasks | 7 files |
| Phase 21 P01 | 70 min | 3 tasks | 10 files |
| Phase 21 P02 | 55 min | 3 tasks | 5 files |
| Phase 21 P03 | 66 min | 2 tasks | 7 files |
| Phase 21 P04 | 46 min | 2 tasks | 3 files |
| Phase 21 P05 | 40 min | 2 tasks | 11 files |
| Phase 21 P06 | 45 min | 2 tasks | 2 files |
| Phase 21 P07 | 75 min | 3 tasks | 10 files |
| Phase 21 P08 | 50 min | 3 tasks | 12 files |
| Phase 21 P09 | 60 min | 2 tasks | 8 files |
| Phase 21.1 P01 | 74min | 3 tasks | 7 files |
| Phase 21.1 P02 | 57min | 3 tasks | 16 files |
| Phase 21.1 P03 | 50min | 3 tasks | 7 files |
| Phase 21.1 P04 | 45min | 2 tasks | 6 files |
| Phase 21.2 P01 | 11h (gates on a loaded host) | 3 tasks | 13 files |
| Phase 21.2 P02 | about 2 h | 2 tasks | 5 files |
| Phase 21.2 P03 | about 6 h | 3 tasks | 48 files |

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
- [Phase 19]: G2 WAL half: no WAL lock during a record write; one write in flight per segment (19-13)
- [Phase 19]: Cut/rotate/install write out records appended before the call, then wait for no write in flight; a failed log drops its queue as lost (19-13)
- [Phase 19]: G4: mkdir reserves the inode number and locks [parent, ino] before the link (19-13)
- [Phase 19]: G3 (write/read paths): one step token per cell (CellState::stepping); stream writers wait for it with the cell unlocked (STEP_WAIT 15 s), readers never do; dropped steps retry unlocked (STEP_TRIES 16, then EIO; rebase REBASE_TRIES 2, then retries later)
- [Phase 19]: Reads compose stream prefix, zeros and pending parts from memory; no settle, no stream step in a read
- [Phase 19]: hard_cap() = limit + limit/GROW_DIV + 2*REBASE_STEP: extent regrowth near the 32 MiB limit passed limit + 2 MiB; the overlay apply is gated on Overlay::cost_after
- [Phase 19]: G5: overlay-switch and rebase trace lines print with no cell lock held
- [Phase 19]: G3 release part: the last release runs only unlocked steps under the step token; after FINAL_TRIES (8) forced drops in a row a finalize stops consulting the drop hook; finalize_locked only for destroy
- [Phase 19]: Every size change of an open cell waits for the step token before it changes state; its I/O runs as unlocked steps (STEP_TRIES then EIO); reads and getattr do not lock a transient cell
- [Phase 19]: Hard-cap gate steps free at most final_step each; the writer steps again until it is under the cap (orchestrator-added)
- [Phase 19]: 19-16: LOCK-01 and LOCK-05 complete on measured evidence (macOS and Docker Linux, 0 rows over the bar); LOCK-04 code part complete, the orchestrator applies the #21 body and marks it
- [Phase 19]: 19-16 finding for the owner: unlink_logged holds its namespace stripes about 20 ms on macOS while reclaim_inode reads the manifest from the store; no global lock held; fix is an owner decision
- [Phase 20]: 20-01: CAP_MIB = 320 from P_old/P0 by the stated rule (Pm 267 + F 8 + R 40, rounded to 8); above the Phase 18 cap of 256
- [Phase 20]: 20-02: FLUSH_STEP 4 MiB, throttle wait 5 s in 50 ms slices, pass 100 ms, background threshold budget/2
- [Phase 20]: 20-02: flusher step helper is SliceFsFilesystem::flush_step (gate shared, cell lock, free_dirty); FileCell::stream atomic lets the pick put overlays first
- [Phase 20]: 20-03: throttle exits at a stop or with no flusher are counted in stop_exits, not timeouts
- [Phase 20]: 20-03: shutdown = stop and join the flusher with no lock held, then the gate, then test_destroy; FUSE destroy calls it
- [Phase 20]: 20-03: run_mount keeps an Arc clone only for the stop after mount() and drops it right after
- [Phase 20]: 20-04: criterion 3 holds with no source change; the red runs are guard mutations (gate hold while paused; hold kept across the pass wait; wait_step in place of the skip)
- [Phase 20]: 20-05: the flusher clock offset is per file system (FlushCtl field), so parallel tests do not move the clock of each other
- [Phase 20]: 20-05: hot window 300 ms; below the budget the flusher never steps a hot cell, in any mode; at or above the budget hot cells are budget-full candidates
- [Phase 20]: 20-05: budget-full pick is overlays first, then quiet cells, then the largest; a quiet cell at its own limit is stepped below half the budget
- [Phase 20]: 20-06: MIN_DIRTY_BYTES (4 MiB) is a public constant used by parse_dirty_bytes and DirtyConfig::check
- [Phase 20]: 20-06: run_mount checks DirtyConfig before the mount point is made and sets it before the write-log replay
- [Phase 20]: 20-06: set_dirty_config is a plain store; DirtyConfig::check refuses times above u64::MAX / 1000 s like the clap range
- [Phase 20]: 20-07: S7 flusher form asserts VmHWM (not getrusage) against CAP_MIB 320; P1 = 287 MiB, margin 33 MiB
- [Phase 20]: Empty-pass fix (4d8ac3d): after a pass with no candidate, a write below the budget does not kick the flusher
- [Phase 20]: LOCK-02 and LOCK-06 marked complete on evidence (gate, smoke 20-08, P1 under CAP_MIB)
- [Phase 20]: Second empty-pass fix (e44d57d): flag set only when the last pick found no candidate at half the budget or more
- [Phase 21]: 21-01: request queue is max(2 x N, 4) slots and 8 MiB; an empty queue admits one job of any weight
- [Phase 21]: 21-01: a pool worker counts its exit with a drop guard, so a panicked worker does not hold the drain to its limit
- [Phase 21]: 21-01: a pool spawn error becomes the mount error and goes through the stop steps after mount()
- [Phase 21]: 21-02: all 31 request callbacks run as jobs and take the gate only inside serve; the reply is sent after serve; write borrows its payload with 1 worker and copies it with a pool
- [Phase 21]: 21-02: SLICEFS_FUSE_TRACE=0 (read once) turns off the per-request ENTRY and EXIT lines; default on
- [Phase 21]: 21-03: a panic on a request thread (fuser- or slicefs-worker-) aborts from the panic hook before the unwind; no reply, no destroy() commit; the write log replays at the next mount
- [Phase 21]: 21-03: only request threads abort; the flusher, the GC and the commit timer keep the Phase 20 behavior (abort on a flusher panic is a deferred item)
- [Phase 21]: 21-03: SLICEFS_TEST_PANIC_CREATE=k (debug builds only) makes the k-th new entry of a mount panic; panic-replay.sh checks it at 4 and 1 workers
- [Phase 21]: 21-04: S7 cap_checked = flusher form and writers <= DEFAULT_WORKERS_MAX (8); it does not depend on VmHWM
- [Phase 21]: 21-04: before the step limit, S7 VmHWM is 90-97 MiB at 1 writer, 198-216 at 2, 419-441 at 8 (above CAP_MIB 320 in all 3 runs), 688-730 at 16
- [Phase 21]: 21-05: mallopt(M_MMAP_THRESHOLD, 128 KiB) first in run_mount on Linux glibc; all allocator calls in alloc_tune.rs; no step limit unless 21-06 misses CAP_MIB 320
- [Phase 21]: 21-06: S7 with the 21-05 mmap threshold passes the 320 MiB bar at 1-8 writers (R1=pass, Pm(8)=290); 16 writers 412-426 MiB (R2, R3 miss, report only); no step limit needed for the S7 bar; the 21-08 mem_bar is still open
- [Phase 21]: 21-07: the Linux CI scripts add --workers from SLICEFS_WORKERS only when a mount names none, and read the Workers line with grep -o (a cut line of a killed daemon cannot hide it)
- [Phase 21]: 21-08: POOL-05 bench verdict=PASS on the read phase (ratio_read_4_1=2.69; write 1.26), mem_bar=pass (178 MiB at 8 workers), mem16_max_mib=223; the metadata phase runs in its own mount so lock-stats rows cover it alone
- [Phase 21]: 21-09: POOL-01..05 Complete on evidence (POOL-04 on ci-evidence for 7853542; POOL-05 on verdict=PASS, mem_bar=pass, R1=pass); [Step limit] closed by the 21-05 mmap threshold, no step limit
- [Phase 21]: 21-09: no follow-up issue for POOL-05; write plateau (1.26x), S7 R2/R3 at 16 writers and no gain at 16 workers are owner reports in deferred-items.md
- [Phase 21.1]: 21.1-01: header::read checks the 64 KiB bound before UTF-8; a header that is not UTF-8 has its own text (msg_not_text), to document next to M10
- [Phase 21.1]: 21.1-01: Started::stop drops the file system before mount.lock; on a failed mount it also stops the commit timer and the segment GC
- [Phase 21.1]: 21.1-01: --hash and --auto-snapshot are ignored (nothing written) on a ro_compat mount until plan 21.1-02 adds M11 and M12
- [Phase 21.1]: 21.1-02: header::check is the first statement of every command that writes; a store with data and no header is refused (M1) for every Want
- [Phase 21.1]: 21.1-02: write commands on a path with no store write FORMAT before they fail (gc, snapshot create/switch); a later small fix can refuse a path with no segments/ first
- [Phase 21.1]: stats shows format (null with no header), flags by class (name, release, known) and access, in text and JSON
- [Phase 21.1]: gc, snapshot create and snapshot switch refuse a path with no segments/ before the header check (content::require_store); seed and mount still make new stores
- [Phase 21.1]: store-format.md leaves out the unverified ext4 journal-replay claim (RESEARCH A1); deferred list item
- [Phase 21.2]: 21.2-01: loom 0.7.3 models through one cfg point (chunk_store::sync + sync::raw); CI bound LOOM_MAX_PREEMPTIONS=3 (bound-3 sum 6.94 s, bound-4 sum 41.64 s)
- [Phase 21.2]: 21.2-01: pool_panic_tests child tests flaky on a loaded Mac (slow binary start); local gate skips 3 of them, CI on Linux and macOS must pass them
- [Phase 21.2]: 21.2-02: race (b) fixed - check_not_ancestor walks .. under the rename mutex, EINVAL; ELOOP guard 1<<20; D-29 not needed (20 of 20 runs pass)
- [Phase 21.2]: 21.2-03: lint allow reasons use the reason = argument on one line; clippy.sh checks it
- [Phase 21.2]: 21.2-03: too_many_arguments, index loops with a meaningful index, and one-row case tables get an allow with a reason, not a refactor
- [Phase 21.2]: 21.2-03: ci.yml jobs linux and macos get timeout-minutes 30 (10-run maximum times 1.5 is below the 30 min floor)
- [Phase 21.2]: 21.2-03: Linux clippy evidence is CI Safety run 45649551248; local Docker had under 1 GiB free

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

### Roadmap Evolution

- Phase 21.1 inserted after Phase 21: Store header with compat, ro-compat and incompat feature flags, and a written format policy (#154); owner agreed #154 joins v2.2 after Phase 21
- Phase 21.2 inserted after Phase 21.1: Thread-safety verification and CI gates (user asked for formal verification and static analysis on every build) (URGENT)

## Deferred Items

| Category | Item | Status | Deferred At | Milestone |
|----------|------|--------|-------------|-----------|
| *(none)* | | | | |

## Session Continuity

Last session: 2026-11-25T16:06:20.392Z
Stopped at: Completed 21.2-03-PLAN.md
Resume file: None

## Operator Next Steps

- Plan the first phase with /gsd-plan-phase 18 (research-phase flagged: S5 gate fairness, S1 by hand)
- Phases 22-24 can start on separate branches in parallel with Phases 18-21
