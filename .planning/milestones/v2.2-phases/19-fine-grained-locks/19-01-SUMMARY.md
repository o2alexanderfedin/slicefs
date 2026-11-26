---
phase: 19-fine-grained-locks
plan: 01
subsystem: concurrency
tags: [parking_lot, lock-rank, deadlock-detect, lock-stats, chunk-store, metadata, slicefs-cli]

requires:
  - phase: 18-concurrency-spike-and-stress-harness
    provides: lock-stats recorder, stress harness, the seven Phase 18 lock labels
provides:
  - "chunk_store::sync shim: Mutex, RwLock, Condvar over parking_lot 0.12 with Rank, a debug rank checker, a gate marker and the strict-gate rule"
  - "every hot-path lock of chunk-store, metadata and slicefs-cli on the shim (only the StoreIo mutex and the commit gate stay std)"
  - "deadlock-detect feature in three crates and a harness watchdog that aborts on a lock cycle"
  - "lock_stats recorder sized by Site::ALL.len(), record hook used by the shim guards"
affects: [19-02, 19-03, 19-04, 19-05, 19-06, 19-07, 19-08, 19-09, 19-10, 19-11]

actuals:
  tokens: 42000
  tasks: 3
  commits: 3

plan_head_before: 7341d4cc123a120a44d67960d7c7e3a495cdf47c
plan_head_after: 84614685e6613da79160a9904db1a23865a842c9
commits: 3

tech-stack:
  added: [parking_lot 0.12 (arc_lock; already in the tree through the vendored fuser), "deadlock_detection transitive crates, built only with --features deadlock-detect"]
  patterns:
    - "Every lock has a Rank; locks are taken in ascending rank; the debug checker panics on a violation"
    - "A lock built with .timed(Site) records wait and hold into lock_stats (lock-stats feature)"
    - "rank::enter(Rank::Gate) models the std commit gate; rank::require_gate(true) makes Stripe and Cell locks need it"

key-files:
  created:
    - crates/chunk-store/src/sync.rs
  modified:
    - Cargo.toml
    - Cargo.lock
    - crates/chunk-store/Cargo.toml
    - crates/metadata/Cargo.toml
    - crates/slicefs-cli/Cargo.toml
    - crates/chunk-store/src/lock_stats.rs
    - crates/chunk-store/src/lib.rs
    - crates/chunk-store/src/index.rs
    - crates/chunk-store/src/pack_io.rs
    - crates/chunk-store/src/locality.rs
    - crates/chunk-store/src/mem.rs
    - crates/chunk-store/src/race_tests.rs
    - crates/chunk-store/src/locality_tests.rs
    - crates/metadata/src/store.rs
    - crates/metadata/src/wal/mod.rs
    - crates/metadata/src/wal/per_op.rs
    - crates/metadata/src/wal/buffered.rs
    - crates/metadata/src/wal/periodic.rs
    - crates/slicefs-cli/src/filesystem.rs
    - crates/slicefs-cli/src/gc_pacing.rs
    - crates/slicefs-cli/tests/stress_harness.rs
    - crates/slicefs-cli/tests/write_log_replay_tests.rs

key-decisions:
  - "The metadata commit lock has Rank::Rename (2), not Rank::Store: a commit cut holds it and then locks cell and map locks"
  - "The gc pacer state has Rank::Store (6), not Rank::Short: the chunk store calls the pacer with its gc lock held"
  - "Site::ALL is a slice, so a new Site needs no array length change"

patterns-established:
  - "Same-rank rules: Gate, Rename, Cell never nest; Stripe nests only in ascending key; Short and Store may nest"
  - "A test that forgets a guard (simulated crash) must run on its own thread: the forgotten guard leaves its entry in the thread-local rank stack"

requirements-completed: []

duration: 60min
completed: 2026-11-21
status: complete
---

# Phase 19 Plan 01: Lock shim, rank checker and deadlock watchdog Summary

**All hot-path locks of chunk-store, metadata and slicefs-cli are parking_lot locks behind `chunk_store::sync`, with a debug-build rank checker that panics on a lock taken against the order, lock-stats timing inside the shim, and a `deadlock-detect` watchdog in the stress harness.**

## Performance

- **Duration:** about 60 min
- **Completed:** 2026-11-21
- **Tasks:** 3
- **Files modified:** 23 (22 modified, 1 created)

## Accomplishments

- `sync.rs`: `Mutex`, `RwLock`, `Condvar` over parking_lot (no poison), `Rank` (Gate 1, Rename 2, Stripe 3, Cell 4, Short 5, Store 6), a thread-local debug rank stack, `rank::enter`, `rank::require_gate`, `rank::held`. 22 unit tests (a 23rd with `lock-stats`) cover every forbidden order, out-of-order guard drops, condvar entry suspend and resume, the gate marker, the strict-gate rule and the exclusive-gate holder locking cells one at a time.
- Timing moved into the shim: a lock built with `.timed(Site)` records wait and hold into `lock_stats` with the `lock-stats` feature; with the feature off there is no timing code. The seven Phase 18 labels still record (`lock_stats_report` passes).
- About 270 lock sites migrated. The std `Mutex<StoreIo>` and the commit gate stay std (D-18). `MergeCtl`, `LocalityCache`, `SyncWait` and `MemIo` lost their derived `Default` and got a manual one, because a shim lock needs a rank.
- `deadlock-detect` feature in chunk-store, metadata and slicefs-cli; the harness starts one detached thread that calls `parking_lot::deadlock::check_deadlock` every second and aborts on a cycle. `short_one_file` and `short_many_files` pass with it.
- The tree has one copy of parking_lot (0.12.5).

## Task Commits

1. **Task 1: Shim, rank checker and site timing, proven on the store lock** - `df1ba4c` (feat)
2. **Task 2: chunk-store and metadata onto the shim** - `ec1c21d` (refactor)
3. **Task 3: slicefs-cli onto the shim; deadlock watchdog** - `8461468` (refactor)

**Plan metadata:** added by the final docs commit.

## Decisions Made

- `commit_lock` of `DictMetadataStore` is `Rank::Rename`, `gc_pacing` `WritePacer.state` is `Rank::Store`. See the deviations.
- `Site::ALL` changed from `[Site; 7]` to `&'static [Site]`, with a test that it lists every site in discriminant order.
- A shim `try_lock` still runs the rank check (a refused order panics even if the lock would be free). Simple and strict; relax it if a later plan needs a try-lock out of order.
- Condvar waits: the hold time of a timed mutex includes the wait. No timed lock is waited on today.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Bug] Commit lock rank (plan: every metadata lock is Rank::Store)**
- **Found during:** Task 3 (stress harness `short_` run)
- **Issue:** `begin_commit` returns a `PreparedCommit` that holds `commit_lock`; `commit_cut_begin` then locks `open_files` (Short). With rank 6 the checker panicked: "Short/0 taken while holding Store/0". The real order is gate, commit lock, file-system maps, store locks.
- **Fix:** `commit_lock` is `Rank::Rename` (rank 2: below Cell and Short, above the gate). One thread never holds both the commit lock and the rename mutex (commit and GC threads versus request threads), and the checker refuses two rank-2 locks at once. Documented on `Rank::Rename` and on the field.
- **Files modified:** `crates/metadata/src/store.rs`, `crates/chunk-store/src/sync.rs`
- **Commit:** `8461468`
- The Task 2 commit message says every metadata lock is rank 6; that was true at `ec1c21d` and is corrected in the Task 3 commit message.

**2. [Rule 1 - Bug] gc pacer state rank (plan: gc_pacing state is Rank::Short)**
- **Found during:** Task 3 (`gc_online_tests`)
- **Issue:** `ChunkStore::gc_groups` calls `GcPacer::at_safe_point` with the store `gc` mutex (rank 6) held; the pacer's state lock at rank 5 was taken after a rank 6 lock.
- **Fix:** `WritePacer.state` is `Rank::Store`. It is a leaf lock (only the clock and the free-ratio callback run under it), so a store rank nests safely everywhere.
- **Files modified:** `crates/slicefs-cli/src/gc_pacing.rs`
- **Commit:** `8461468`

**3. [Rule 3 - Blocking] Crash-window test leaves a rank-stack entry**
- **Found during:** Task 3 (`a_crash_between_the_cut_and_the_root_replays`)
- **Issue:** the `AfterCut` window `mem::forget`s the `CommitCut` (a simulated crash), which leaks its lock-order entry on the test thread; the next case then panicked "Rename/0 taken while holding Rename/0".
- **Fix:** each case runs on its own thread, with a comment.
- **Files modified:** `crates/slicefs-cli/tests/write_log_replay_tests.rs`
- **Commit:** `8461468`

**4. [Rule 3 - Blocking] Test sites outside the plan file list**
- `crates/chunk-store/src/locality_tests.rs` (Task 1 commit) and `crates/chunk-store/src/index.rs` tests (Task 2): `.lock().unwrap()` and `get_mut().unwrap()` on locks that moved to the shim.

**5. [Plan fact] `Cargo.lock` gained nine packages**
- The plan says "No other crate is added". The `deadlock-detect` feature (`parking_lot/deadlock_detection`) pulls `backtrace`, `petgraph`, `addr2line`, `adler2`, `fixedbitset`, `gimli`, `miniz_oxide`, `object` and `rustc-demangle` into `Cargo.lock`. They are optional dependencies of parking_lot_core, compiled only with `--features deadlock-detect`; a normal build does not compile them. See Threat Flags.

**Total deviations:** 3 auto-fixes (2 Rule 1, 1 Rule 3), plus 2 notes. **Impact:** the six-rank table is unchanged; two locks have a different rank than the plan listed.

## For later plans

- Plan 09 wires `rank::enter(Rank::Gate)` in `gate_shared` and `gate_exclusive`. The commit thread takes the gate before `begin_commit`, so the commit lock (rank 2) comes after the gate: no conflict. `gc_start_mode` calls `wait_for_commit` (rank 2, dropped at once) before it takes the gate: also fine, as long as it holds no gate marker at that point.
- A plan that takes the rename mutex (rank 2) on a thread that holds a `PreparedCommit` would panic in debug builds. Today only the commit thread holds one.
- A test that forgets a guard (`mem::forget`) must run on its own thread.
- A `Short` lock cannot be taken while a `Store` lock is held. Any plan that calls back from store code into file-system code (a hook, a pacer, `pre_root_sync`) must check the rank of the callee.

## Issues Encountered

- The TDD flag is not set on these tasks, so there is no red commit to record. The unit tests in `sync.rs` were written with the shim and run green at the first build.

## Known Stubs

None.

## Threat Flags

| Flag | File | Description |
|------|------|-------------|
| threat_flag: supply-chain | Cargo.lock | Nine extra locked packages (`backtrace`, `petgraph`, `gimli` and others) through the optional `deadlock_detection` feature of parking_lot; compiled only for stress builds. T-19-SC covered parking_lot and lock_api only. |

## Next Phase Readiness

The lock API and the checker exist, so Plans 02 to 11 can add locks with a rank. Open checks for Plan 09: the strict-gate rule is only unit-tested here; no production thread calls `rank::require_gate(true)` yet.

## Self-Check: PASSED

- `crates/chunk-store/src/sync.rs`: FOUND
- Commits `df1ba4c`, `ec1c21d`, `8461468`: FOUND on HEAD (`git rev-list --count 7341d4c..HEAD` prints 3)
- `cargo test --workspace` exit 0 on the final tree; `cargo check` with `lock-stats` and `deadlock-detect` exit 0; `short_` stress tests with `deadlock-detect` pass; `lock_stats_report` passes with all seven labels.
