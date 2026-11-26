---
phase: 19-fine-grained-locks
plan: 05
subsystem: concurrency
tags: [lock-01, lock-05, d-03, d-07, d-10, d-13, file-cell, handle-table, readers, keep-budget, multi-handle]

requires:
  - phase: 19-fine-grained-locks
    provides: "19-01 lock shim (Rank::Cell, Rank::Short, timed sites); 19-03 modify_inode; 19-04 WAL append/write-out split"
provides:
  - "Handle table: Handle { ino, write, append } per fh, open count and orphan set per inode, one Rank::Short lock (site handles)"
  - "FileCell { ino, state, dirty } per open inode in a 16-shard cell map (sites file_cell, cell_map); CellState { file, write_handles, retired }"
  - "CellGuard: syncs FileCell::dirty and the global dirty_total by delta under the cell lock (on drop and at sync_dirty)"
  - "Last write-handle release finalizes, retires and removes the cell under the cell lock; open retries on a retired cell"
  - "keep_budget on the global dirty total; the writer steps only its own cell (D-13)"
  - "readers: ino -> Arc<Mutex<Reader>>, map lock only for the Arc; busy reader -> private reader (D-07); test_set_read_hook"
  - "write_rebase_bytes, write_base_bytes, last_rebase_bytes thread-local (D-10)"
  - "crates/slicefs-cli/tests/multi_handle_posix.rs (7 tests)"
affects: [19-06, 19-07, 19-08, 19-09, 19-10, 19-11, 20]

estimate:
  tokens: 160000
  tasks: 3
actuals:
  tokens: 22400
  tasks: 3
  commits: 3

plan_head_before: f6d422b607b6a2439f87eb0fc8efaf8340f1ec87
plan_head_after: a29d163a6b91c51f1c67ebfbc182c66f9dac9ba6
commits: 3

tech-stack:
  added: []
  patterns:
    - "One entry point per site: an outer helper (with_cell, open_write, release_write) looks up, locks, checks retired; the inner form takes &mut OpenFileState and never locks a cell again"
    - "A guard that moves a per-cell atomic and a global total by the delta when it drops, with the lock still held"
    - "Handle out of the table first (take_handle), then the cell work, then uncount: a write handle leaves its cell exactly once"
    - "Exclusive-gate holders (commit cut, GC start, destroy) collect the Arcs shard by shard and lock one cell at a time"

key-files:
  created:
    - crates/slicefs-cli/tests/multi_handle_posix.rs
  modified:
    - crates/slicefs-cli/src/filesystem.rs
    - crates/chunk-store/src/lock_stats.rs
    - crates/slicefs-cli/tests/lock_stats_report.rs
    - crates/slicefs-cli/tests/rebase_step_tests.rs

key-decisions:
  - "Cell map: 16 shards, shard = ino mod 16 (Claude's discretion)"
  - "The orphan set lives in the handle table (one lock with the open count), so the open check and the orphan mark agree"
  - "dirty_total moves through a CellGuard drop: every cell lock syncs, also on early error returns; a retired cell counts 0"
  - "fsync and flush through a read-only handle stay a no-op (the handlers log Flush only for write handles)"
  - "A size change uses the cell of the inode whatever handle the request names; test_release takes the inode from the handle"
  - "A first read of an inode opens its reader with no lock and adds it if still absent; the read hook fires only on the shared reader"

patterns-established:
  - "Code under a cell lock must not call a function that locks a cell (use the *_open / *_state / *_write inner forms)"

requirements-completed: []

duration: 41min
completed: 2026-11-22
status: complete
---

# Phase 19 Plan 05: FileCell per inode, handle table, per-inode readers Summary

**Every open inode has one `Arc<FileCell>` that all its handles lock in place, so no request takes file state out of a table. A handle table knows each handle's mode and keeps O(1) open counts. `keep_budget` reads one atomic total and steps only the writer's own file. `readers` holds one lock per inode. The serialize-off one-file harness run went from 192 mismatches (errno 9) to clean.**

## Performance

- **Duration:** about 41 min
- **Completed:** 2026-11-22
- **Tasks:** 3
- **Files modified:** 5 (4 modified, 1 created)

## Accomplishments

- Handle table (Task 1): `Handle { ino, write, append }` (`append` stays false until Plan 08), `open_count` per inode and the orphan set under one `Rank::Short` lock (site `handles`). `close_handle` and `drop_last_link` use the count, not a scan. `ino_of_fh` keeps its meaning (the inode of a write handle). Reclaim runs after the table lock is released.
- FileCell (Task 2): the per-handle state map, the alias map, the owner lookup and the heir swap are gone. Every site from the plan facts uses a cell lookup by inode. Under the cell lock, each site covers the span the old code covered with the taken-out state, including its chunk, hash and put I/O (Plans 06-07 move that out). Release of the last write handle finalizes, sets `retired` and removes the cell from its shard while it holds the cell lock (rank 4, then 5). `O_TRUNC` at open, truncate to 0 and fsync now also hold the cell lock across the manifest change.
- `keep_budget` (Task 2, D-13): the check reads only `dirty_total`. Over the budget, the writer steps only its own locked cell. No other cell is locked.
- Readers (Task 3, D-07): the map lock is held only to clone or add the `Arc`. A busy per-inode reader leads to a private reader for that read. Lock-stats hold max of `readers` fell from 150 us to 7 us in `lock_stats_report`.
- Counters (Task 3, D-10): `thread_local!` cells; `rebase_count` and `last_relog_bytes` stay atomics.

## Task Commits

1. **Task 1: Handle table with modes and per-inode open counts** - `45345cb` (feat)
2. **Task 2: FileCell per inode, every take-out site converted, keep_budget on atomics** - `ec95d03` (feat)
3. **Task 3: Per-inode readers, thread-local write counters** - `a29d163` (feat)

**Plan metadata:** added by the final docs commit.

## TDD record (red before green, no red commit)

- Task 2 behavior tests (`visibility_across_handles`, `fsync_through_any_write_handle_commits`, `last_release_finalizes_once`, `open_after_last_release_gets_a_new_cell`): **green on the Task 1 tree**, before any cell code. They are single-thread cases, and the alias logic already passed them. They guard behavior that must stay the same.
- The real red for Task 2 was the serialize-off one-file run on the Task 1 tree: `max_inflight=4`, `ops=111`, many `MISMATCH ... op=write block=None errno 9`, and the panic "192 mismatches" (exit 101).
- Task 3, `two_readers_of_one_file_do_not_share_a_reader_wait`: red with the hook placed in the old `read_root` (inside the map lock): "B finished while A held the per-inode reader: Timeout" (5.14 s). Green after the per-inode reader change (0.57 s for the whole file).

## Serialize-off one-file report (Task 3 verify)

`STRESS_SERIALIZE=0 STRESS_MODE=one-file STRESS_SECS=3 STRESS_THREADS=4 STRESS_FILE_MIB=4 cargo test -p slicefs-cli --test stress_harness long_run -- --ignored --exact --nocapture`: exit 0.

```
STRESS seed=1795322941995082000 mode=one-file threads=4 serialize=false ops_per_thread=timed secs=3
op=read n=2086 p50_us=10.9 p99_us=1230.8 max_us=7553.0
op=write n=1261 p50_us=281.1 p99_us=39878.7 max_us=81788.9
op=fsync n=82 p50_us=22249.5 p99_us=129695.7
op=open n=4 / op=release n=4
op=commit_total n=8 p50_us=123863.0
max_inflight=4
ops=2168 dirty_total=0
test long_run ... ok
```

No line with "mismatches", no "errno 9". The run after Task 2 alone was also clean (`ops=2015`, `max_inflight=4`).

The same command with `STRESS_MODE=many-files` (not required by the plan; it puts the handle table, the orphans and the cell map under threads) also passed: `create n=242`, `rename n=145`, `unlink n=143`, `open n=186`, `release n=428`, `max_inflight=4`, `ops=1465 dirty_total=0`, no mismatch.

## Verification

- Full suite command from 19-VALIDATION.md (`cargo test --workspace && cargo check --workspace --all-targets && cargo check --workspace --all-targets --features slicefs-cli/lock-stats && cargo check -p slicefs-cli --features deadlock-detect`): exit 0 on the final tree. 65 `test result: ok`, no `FAILED`, no `lock rank violation`.
- `cargo test -p slicefs-cli --features lock-stats --test lock_stats_report -- --nocapture`: 1 passed. All 12 sites have samples, among them `handles n=20`, `file_cell n=8`, `cell_map n=11`, `readers n=2`.
- `cargo test -p slicefs-cli --test multi_handle_posix`: 7 passed.
- Acceptance greps: `fn open_count_tracks_handles_and_orphans` 1; `values().any` 0; `struct FileCell` 1; `open_files` (non-comment) 0; `write_alias` 0; `fn owner_fh` 0; `OpenFiles` in lock_stats.rs 0; `"open_files"` in lock_stats_report.rs 0; `"file_cell"` 1; `retired` 17; `thread_local!` 1.
- Tracer gate after Task 1: `multi_handle_posix` ran again on the committed tree and passed.

## Decisions Made

See `key-decisions`. All of them can be changed later.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Bug] A budget test expected a step of another file**
- **Found during:** Task 2 (full suite)
- **Issue:** `rebase_step_tests.rs` `the_budget_turns_a_stream_with_pending_writes_into_steps` asserted that writes to file B turn file A's stream into an overlay. D-13 removes exactly that cross-file step in this phase. The plan fact covered `dirty_budget_tests.rs` but not this test.
- **Fix:** The test now checks the Phase 19 rule. Writes to B leave A streaming, and the total stays at A's pending bytes plus at most one write of B. Then A's own in-order writes, still over the budget, turn A into an overlay with bounded steps and bring the total under the budget. The stream-to-overlay path of `free_dirty` stays covered. The cross-file case belongs to Phase 20 (flusher).
- **Files modified:** `crates/slicefs-cli/tests/rebase_step_tests.rs` (not in the plan file list)
- **Commit:** `ec95d03`

**2. [Rule 2 - Missing critical] Orphan mark and open check under one lock**
- **Found during:** Task 1
- **Issue:** With separate `handles` and `orphans` locks, a close between the open check and the orphan mark could leave an orphan that nothing reclaims.
- **Fix:** The orphan set is a field of the handle table. `drop_last_link` changes the inode with no table lock held, then marks the orphan only if the inode is still open. Otherwise it reclaims at once.
- **Commit:** `45345cb`

**3. [Rule 1 - Bug] Double count-out of a write handle**
- **Found during:** Task 2 design
- **Issue:** If release looked up the handle and removed it later, two releases of one fh could both decrement `write_handles`.
- **Fix:** `take_handle` removes the fh first. Only the caller that gets it back runs `release_write`, and `uncount` (open count, orphan reclaim) runs last. So the finalize comes before an orphan reclaim, as before.
- **Commit:** `ec95d03`

### Plan facts

- **Task 1 commit gate:** before the Task 1 commit I ran `cargo test -p slicefs-cli` (exit 0) and the chunk-store `lock_stats` unit test, not the whole workspace. Task 1 changed only one enum in chunk-store. The workspace suite ran before the Task 2 and Task 3 commits.
- **Signatures:** `test_setattr_size` ignores its `fh` (the inode's cell is used). `test_release` takes the inode from the handle table. `test_release_full` now calls `test_release`, because the table gives the mode. The private `flush_handle` takes only `fh`, and `flush_buffer_for_fsync` is gone. `test_destroy` finalizes and retires every live cell. A release after that finds no cell and returns Ok.
- **Read hook:** `test_set_read_hook` is a plain `pub fn`, like the rebase hook, so an integration test can use it. It fires only in a read through the shared per-inode reader. The first read of an inode opens a reader with no lock and never fires it, so the test warms the reader first.
- **Left for a later plan:** the S3 spike in `stress_harness.rs` still lists `"open_files"` in its `SITES` filter (that site now has no samples), and the `#[ignore]` text of `serialize_off_target` still says "open_files". The harness is not in this plan's file list. Plan 09 or 11 can change both when it un-ignores the target.

- **Stale text:** the `DIRTY_BUDGET` doc in `filesystem.rs` and the header of `dirty_budget_tests.rs` still say that the file with the most dirty bytes stores a step. Since D-13, only the writer's own file does. Correct them with the Phase 20 flusher.
- **Budget effect until Phase 20:** the dirty total can stay above the budget when the excess is in a file that nobody writes. The adapted test shows this: A's 3 MiB pending stay against a 2 MiB budget while only B is written. Each write still keeps its own file's share bounded, and the per-file `DIRTY_LIMIT` is unchanged.

**Total deviations:** 3 auto-fixes (2 Rule 1, 1 Rule 2) and 4 notes. **Impact:** all plan truths hold. One test outside the file list now checks the D-13 rule instead of the cross-file step.

## For later plans

- Plans 06-07: each site's I/O is still inside the cell lock: `flush_state` (`end_padded`), `rebase_step` (splice), `to_overlay`, the stream `push` in `test_write`, `settle_pending` (`fill_stream`), `finalize_write`. The inner forms take `&mut OpenFileState`, so you can split each one into copy, I/O and publish without new lookups. `rebase_hook` fires under the cell lock today.
- Code under a cell lock must not call `with_cell`, `open_write`, `release_write`, `test_setattr_size`, `flush_handle` or `test_read`. Each of them locks the cell again, and Cell on Cell panics in debug builds.
- `CellGuard` drop computes `dirty_cost`, which sums a stream's pending map (bounded by `REORDER_WINDOW`).
- Plan 08: set `Handle::append` in `open_handle`. The `Handle` is in `by_fh`. `ino_of_fh` returns `Some` only for write handles.
- Plan 09: `close_handle`, `uncount` and `drop_last_link` are the namespace check-then-act sites. Today `drop_last_link` re-checks the count after `modify_inode`.
- After `test_destroy`, `ino_of_fh` still returns `Some` for an open write handle, so a later `handle_release` logs a `Release` record. The old code returned `None` there. Nothing calls release after destroy today.

## Issues Encountered

- Without `--no-fail-fast`, the first workspace run of Task 2 stopped at the failing `rebase_step_tests` binary. After the fix, I ran the whole suite again with `--no-fail-fast`, and later with the full suite command.

## Known Stubs

None.

## Threat Flags

None. No new endpoint, file kind or trust boundary. T-19-SC: no new crate. T-19-15: `retired` is checked after every cell lock, open loops until it holds a live cell, and the cell leaves the map only with its lock held (`last_release_finalizes_once`, `open_after_last_release_gets_a_new_cell`). T-19-16: a write needs a write handle in the table, else EBADF, and a kernel fh never allocates anything. T-19-17: `dirty_total` moves by delta under each cell lock, and `dirty_budget_tests.rs` passes unchanged. T-19-18: the exclusive-gate holders lock one cell at a time, and the suite showed no rank violation.

## Self-Check: PASSED

- `crates/slicefs-cli/tests/multi_handle_posix.rs` and this SUMMARY: FOUND
- Commits `45345cb`, `ec95d03`, `a29d163`: FOUND on HEAD (`git rev-list --count f6d422b..HEAD` prints 3)
