---
phase: 19-fine-grained-locks
plan: 02
subsystem: chunk-store
tags: [lock-01, store-lock, seal, pack-create, keep-cap, index-flush, frozen-memtable, trust-floor, pack-open, memio-hold]

requires:
  - phase: 19-fine-grained-locks
    provides: "19-01 lock shim (Rank::Store), lock_stats sites"
provides:
  - "MemIo hold knobs: hold_syncs, hold_creates, hold_durable, hold_opens, with parked(Hold) and wait_parked (a held call waits at most 10 s)"
  - "crates/chunk-store/src/lock_tail_tests.rs: one deterministic test per inventory row 12-15 (6 tests)"
  - "put: every slow step before the append runs without ChunkStore::inner (prepare_append: create, seal wait, KEEP_CAP wait)"
  - "Index frozen memtable (freeze, write_frozen, install_frozen): the run write and the MANIFEST write run without ChunkStore::inner"
  - "ChunkStore::trust: trust-floor writes without ChunkStore::inner"
  - "read: pack open without ChunkStore::inner, with a re-check that the place is still current"
  - "PackFile::write_wait, SyncWait::is_done"
affects: [19-03, 19-04, 19-07, 19-11]

estimate:
  tokens: 100000
  tasks: 3
actuals:
  tokens: 14400
  tasks: 3
  commits: 3

plan_head_before: 4afa591c040984fadce236ef29aca4cc2b548927
plan_head_after: 3fb6def7d1c6bc844c7e8fbfb895e61a10917147
commits: 3

tech-stack:
  added: []
  patterns:
    - "Check under the lock, wait or do the I/O without it, lock again and start the check again (put loop in ChunkStore::put)"
    - "One slow job of a kind at a time, marked in Inner (creating, flushing, sealing) with a SyncWait; others wait outside the lock"
    - "Frozen memtable: lookups read live shard, then frozen copy, then runs; install puts the run in before it clears the frozen copy"
    - "A test proves 'B is not blocked' only after it has seen A parked (MemIo::wait_parked or the store's test counter of waits)"

key-files:
  created:
    - crates/chunk-store/src/lock_tail_tests.rs
  modified:
    - crates/chunk-store/src/mem.rs
    - crates/chunk-store/src/lib.rs
    - crates/chunk-store/src/index.rs
    - crates/chunk-store/src/retry.rs
    - crates/chunk-store/src/pack_io.rs

key-decisions:
  - "Seal join: put waits for Sealing.done outside the lock before the append that fills the pack (the pattern of ChunkStore::sync); the full pack is not moved out of Inner"
  - "Pack create: one create at a time with no spare pack; a pack created while a GC copy made another active pack is removed (a spare would get a number below the active pack)"
  - "gc_begin waits for a pack create in flight, so gc_select cannot take the new pack as unused and delete it"
  - "Index flush: frozen memtable; the run write and the MANIFEST write run with no store lock; the MANIFEST write holds only the MANIFEST lock; a GC install waits for a running flush"
  - "KEEP_CAP: the check and the append happen in one hold after the wait, so the cap is never passed (no overshoot)"
  - "Trust floor: its own Rank::Store lock held across the TRUST write (one writer, so the file never goes back to a lower floor)"

patterns-established:
  - "lock_tail_tests: park A in a MemIo hold, confirm the park, run B with recv_timeout(5 s), release the hold before the asserts"

requirements-completed: []

duration: 52min
completed: 2026-11-21
status: complete
---

# Phase 19 Plan 02: Chunk-store lock tails Summary

**`ChunkStore::inner` no longer covers a seal join, a pack create, the KEEP_CAP wait, an index run or MANIFEST write, a trust-floor write or a pack open on the put and read paths. Six deterministic tests prove it: each parks thread A in a MemIo hold and checks that thread B's put or read finishes. The page-cache pwrite of one record stays under the lock (D-14).**

## Performance

- **Duration:** about 52 min
- **Completed:** 2026-11-21
- **Tasks:** 3
- **Files modified:** 6 (5 modified, 1 created)

## Per-row fixes (RESEARCH Lock Inventory A)

| Row | Slow operation | Red before the fix | Fix | Test |
|-----|----------------|--------------------|-----|------|
| 12 | seal join | yes: B timed out (5.01 s) | `prepare_append`: when the record fills the active pack and a seal runs, `put` waits for `Sealing.done` without the lock and starts its locked section again. The join under the lock then takes no time. | `put_and_read_pass_a_held_seal_join` |
| 12 | pack create (file create and header write) | yes: B timed out | `prepare_append` reserves `next_pack` and sets `Inner::creating`. `create_pack` runs without the lock, then `install_pack` installs the pack under the lock. One create runs at a time; other puts wait outside the lock. `hwm_end` gives `(num, 0)` while a create runs. `gc_begin` waits for a create in flight (`lock_no_create`). | `put_and_read_pass_a_held_pack_create` |
| 12 | index run write | yes: B timed out | `flush_if_full` runs with no store lock and allows one flush at a time (`Inner::flushing`). (1) Without the lock it waits for the seal and for `sync_soon`. (2) Under the lock it syncs the rest, syncs the directory, takes the hwm and counts, and calls `Index::freeze`. (3) Without the lock it calls `write_frozen`, then `install_frozen`, which holds the MANIFEST lock only. Lookups, `mem_has` and `snapshot` see the frozen copy. A GC install waits for a running flush. | `put_and_read_pass_a_held_run_write` (B also reads an object that is only in the frozen memtable) |
| 13 | KEEP_CAP wait | yes: B timed out | `PackFile::write_wait(len)` (RetryingPack: the flight's `SyncWait` when `kept + len > KEEP_CAP`). `put` waits for it outside the lock; the check and the append then happen in one hold. | `put_and_read_pass_a_held_keep_cap_wait` |
| 14 | trust-floor write | yes: B timed out | `ChunkStore::trust` (Rank::Store) is held across `write_durable(TRUST)`, with no `inner` held. `locked_lookup` returns `Err(floor)` when the durable floor is below `next_pack`; `put` drops the lock, raises the floor, and looks again. The record is written only after the floor is durable. | `put_passes_a_held_trust_floor_write` |
| 15 | pack open on read | yes: B timed out | `read` uses `cached_reader` under the lock. If the pack is not cached, it drops the lock, calls `open_read`, and locks again. It uses the handle only if the place is unchanged (`add_reader`); otherwise it starts again at the new place. | `put_passes_a_held_pack_open` |

`start()` (the spawn of the pack-sync thread) stays under the lock: it is not disk I/O. `sync_here` runs inline only when `sync_handle` returns `None` (for `FileIo`, only when `try_clone` fails) or when a thread cannot start. Both are rare fallbacks, not the hot path.

## What still runs under `ChunkStore::inner`

- The record `write_at` (D-14; there is a comment at `append`).
- Flush step 2: the fsync of the bytes put since the background sync, and `sync_dir` when a pack was created. This is the same pattern as `ChunkStore::sync` (#108). Only this rest is synced under the lock.
- The GC copy loop appends under its own hold, so a pack create, a seal join or a KEEP_CAP wait there still happens under the lock. GC is not on the put or read path, and it calls `flush_if_full` without the lock now.
- `gc_begin`, `close` and the GC install still join a seal and sync under the lock (RESEARCH Inventory B, not changed).
- The re-lookup under the lock when the index generation changed (a run page read; existing logic, kept as the plan says).
- Error paths: `RetryingPack::repair` after a failed sync, and an inline seal when the seal thread cannot start.
- The MANIFEST lock is held during the MANIFEST write of a frozen flush. A merge pick or install, or `index_info`, that needs that lock waits for it while it holds `inner`. Puts and reads do not take the MANIFEST lock.

## Task Commits

1. **Task 1: hold knobs, seal-join test, seal join out of the store lock** - `8366c1f` (feat)
2. **Task 2: pack create, KEEP_CAP wait and run write out of the store lock** - `27cb7c0` (feat)
3. **Task 3: trust-floor write and pack open out of the store lock** - `3fb6def` (feat)

**Plan metadata:** added by the final docs commit.

## TDD record (red before green, no red commit)

- Task 1: before the fix, `put_and_read_pass_a_held_seal_join` failed: "B's put and read finished while A waited for the seal join: Timeout" (5.01 s). The test counter was first placed at the old join point in `seal()`.
- Task 2: before the fix, `pack_create`, `keep_cap_wait` and `run_write` all failed at B's `recv_timeout` (lines 118, 150, 186). For the KEEP_CAP red run, the test counter was placed in `append` in front of the in-lock wait; it was removed in the fix.
- Task 3: before the fix, `trust_floor_write` and `pack_open` failed: "B's read/put finished while A ...: Timeout".

## Verification

- `cargo test -p chunk-store --lib lock_tail_tests`: 6 passed, 0.81 s.
- Full suite command from 19-VALIDATION.md (`cargo test --workspace && cargo check --workspace --all-targets && cargo check ... --features slicefs-cli/lock-stats && cargo check -p slicefs-cli --features deadlock-detect`): exit 0. 64 test binaries `ok`, no `FAILED`, no `lock rank violation`. The only warnings are the old ones from the vendored fuser.
- `cargo test -p slicefs-cli --features lock-stats --test lock_stats_report`: 1 passed, no "no samples for lock site".
- `cargo test --workspace` exited 0 before each of the three commits. The online GC tests of slicefs-cli are part of it.

## Decisions Made

See `key-decisions`. All of them can be changed later. Following the delegation note, I decided them myself and report them here.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Bug] Plan shape for the pack create (a spare pack) would break pack order**
- **Found during:** Task 2 design
- **Issue:** The plan recommends keeping a new file as "the next spare" when another thread installed an active pack meanwhile. That spare has a lower number than the active pack. A later use of it puts newer records in a lower-numbered pack. The replay goes by hwm (pack, offset), so it would skip those records after a crash. `gc_select` also deletes every pack at or below the boundary that has no entries, which includes a pack whose number is taken but whose file is not installed.
- **Fix:** One create runs at a time (`Inner::creating`), with no spare. In the rare race with a GC copy, the new file is removed without the lock. `gc_begin` and `gc_begin_full` wait for a create in flight. `hwm_end` covers the pack in creation.
- **Commit:** `27cb7c0`

**2. [Rule 1 - Bug] Seal join shape**
- **Found during:** Task 1 design
- **Issue:** The plan's recommended shape (take the full pack out, let other puts use a new pack) leaves a pack that is in neither `active` nor `sealing`. Then `sync`, the flush hwm and `gc_begin` would not know that its records still need a sync.
- **Fix:** Wait for `Sealing.done` outside the lock before the append that fills the pack. Puts whose records do not fill the pack go on during the seal.
- **Commit:** `8366c1f`

**3. [Rule 1 - Bug] Test data was not unique**
- **Found during:** Task 2 (the KEEP_CAP test never reached the cap)
- **Issue:** `data(i, len)` reduced `i` to one byte constant, so some of the 26 objects of 1 MiB had the same bytes. Their puts were dedup hits and wrote nothing.
- **Fix:** `i` now goes in the first 4 bytes of each object.
- **Commit:** `27cb7c0`

**4. [Rule 2 - Missing critical] GC install and frozen flushes, run order**
- **Found during:** Task 2
- **Issue:** A frozen run installed after a GC install would be newer than the GC tombstones, so it would bring back entries that point into deleted packs.
- **Fix:** The GC install waits (without the lock) for a running flush. The GC copy path calls `flush_if_full` without the lock. `install_merge` reads the run list under the MANIFEST lock.
- **Commit:** `27cb7c0`

**5. [Rule 2 - Missing critical] Concurrent trust-floor writers**
- **Found during:** Task 3
- **Issue:** With the write outside `inner`, two writers could finish out of order and leave a lower floor in the TRUST file.
- **Fix:** One `trust` lock is held across the write. The durable value moved from `Inner::trust_floor` to `ChunkStore::trust`; `Inner::trust_floor` keeps only the value read at open.
- **Commit:** `3fb6def`

**6. [Rule 3 - Blocking] Files outside the plan list, and test helpers**
- `crates/chunk-store/src/pack_io.rs`: `SyncWait::is_done` and `PackFile::write_wait`.
- `#[cfg(test)] ChunkStore::waits` with `note_wait` and `test_waits`. A wait on a `SyncWait` is not a MemIo hold, so the seal-join and KEEP_CAP tests need this counter to see that A is parked.
- `mem.rs`: a `Hold` enum, plus a 10 s limit on every held wait (project rule: every wait has a time limit). This also applies to `hold_syncs`, which had no limit before.

**Total deviations:** 6 (3 Rule 1, 2 Rule 2, 1 Rule 3). **Impact:** the rows the plan names all leave the lock. Two recommended shapes were replaced by safer ones.

## For later plans

- Plan 03 (`hold_pack_reads`): the `Hold` enum in `mem.rs` takes a new kind with one array slot.
- A put can now return after it waited outside the lock, so every check before the append runs again in the loop. New code that adds a check must add it inside that loop.
- `Index` has a third lookup layer (frozen). A new reader of the memtable must use `get`, `mem_has` or `snapshot`, and must not read `shards` directly.

## Issues Encountered

- The first full-suite run in Task 1 printed a cut summary (I used `head`), so I ran it again with a full log before the commit.

## Known Stubs

None.

## Threat Flags

None. No new endpoint, file kind or trust boundary. T-19-03 is unchanged: the record write stays under the lock. T-19-04: no overshoot. T-19-05: `trust` lock plus the durable-before-write rule. T-19-06: the frozen memtable is visible to lookups, and `run_write` reads an object from it.

## Self-Check: PASSED

- `crates/chunk-store/src/lock_tail_tests.rs` and this SUMMARY: FOUND
- Commits `8366c1f`, `27cb7c0`, `3fb6def`: FOUND on HEAD (`git rev-list --count 4afa591..HEAD` prints 3)
- Acceptance greps: `fn hold_creates` 1, `mod lock_tail_tests` 1, seal-join test 1, Task 2 tests 3, Task 3 tests 2
