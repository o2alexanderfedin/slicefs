---
phase: 17-on-disk-chunk-index
plan: 03
subsystem: chunk-store index
tags: [index, lsm, memtable, manifest, gc, tombstone]
status: complete

requires: [17-02]
provides:
  - crates/chunk-store/src/index.rs (Manifest SFMF encode/decode, Index with memtable, get, insert, tombstone, flush, iter_all, open)
  - ChunkStore without the in-RAM map of every object; open replays only above the high-water mark
  - Options::index_memtable_limit (default 1 << 18), IndexInfo, ChunkStore::index_info()
  - gc_finish in five phases (complete index read, copies, tombstones, durable flush, punch and delete)
affects: [17-04, 17-05, 17-06, 17-07, 17-08]

actuals:
  tokens: 15000
  tasks: 2
  commits: 3
plan_head_before: 354cf957a799071b8d5f5805325234d441f31acb
plan_head_after: 06cf0c21cb6add28144b8ea17828a776a6138ee6

tech-stack:
  added: []
  patterns:
    - MANIFEST as the commit point of each flush (write_durable); the run file, its sync, the directory sync and the run open all come before it
    - newest entry per ID wins in get and in the merged iterator; a tombstone hides only the entry it was written for
    - every index read that a GC decision needs happens before any change; an index read error is returned, never read as "not found"

key-files:
  created:
    - crates/chunk-store/src/index.rs
    - crates/chunk-store/tests/index_tests.rs
  modified:
    - crates/chunk-store/src/lib.rs
    - crates/chunk-store/src/run.rs

key-decisions:
  - "Open sets next_pack to at least the hwm pack, so a new pack never gets a number that the hwm covers (for example after GC deleted every pack)"
  - "A writable open sets next_run past every run file on disk, so a run file left by a failed flush cannot block the next flush"
  - "A writable open syncs a sealed pack that it rescans (no valid .idx) before it writes the .idx, because a later run can point into it"
  - "A failed MANIFEST write does not remove the new run file: the write can still be durable, and then the MANIFEST lists that run"
  - "A damaged MANIFEST or an unreadable run fails the open in this plan (17-04 adds the rebuild rules); only a missing MANIFEST means an empty index"
  - "put counts an object as new when its dedup lookup fails (the object is written again and its memtable entry hides the damaged run)"

duration: about 30 minutes (15:46 to 16:15 local time)
completed: 2026-11-18
---

# Phase 17 Plan 03: on-disk index tracer Summary

**The chunk store finds its objects through run files and a MANIFEST instead of an in-RAM map of every object. Open reads the MANIFEST and the run footers and tables, and replays only the pack records at or above the high-water mark. A GC removal is a tombstone that is durable in a run before any pack is punched or deleted. A GC that cannot read the index changes nothing.**

## What was built

- `index.rs` (new, `pub(crate)`):
  - `Manifest`: magic `SFMF`, version 1, flags (bit 0 is the rebuild flag of 17-04; any other bit is damage), sequence, next run number, hwm (pack, offset), counts (objects, raw_bytes, stored_bytes), live runs (number, length, entry count, newest first), obsolete run numbers, CRC32C. Decode checks every count against the length with checked arithmetic. It also checks that every run number is below the next run number.
  - `Index`:
    - `get` looks in the memtable, then in the runs from newest to oldest. The newest entry decides. A run read error is returned at once.
    - `insert` adds a put entry to the memtable.
    - `tombstone(id, pack)` does no I/O. It does not overwrite a memtable put of the ID in another pack.
    - `flush` writes the memtable to a run, syncs, syncs the directory, opens the run, and only then writes the MANIFEST with `write_durable(0, Kind::Manifest, ..)`.
    - `iter_all` is a k-way merge over the memtable and the runs: newest entry wins, tombstones hide, and an error ends the iteration.
    - `open` returns an empty index only when the MANIFEST is missing. Every other error fails the open.
- `lib.rs`:
  - `Inner::index` is now an `Index`. `HashMap<ObjectId, Loc>` is gone. `mod run` has no blanket `allow(dead_code)`.
  - New: `Options::index_memtable_limit`, `IndexInfo` and `index_info()`.
  - `open_with`:
    - Stats start from the MANIFEST counts. `packs` comes from the pack list.
    - Only packs at or above `hwm.pack` are read. For the hwm pack, entries are filtered by offset.
    - A replayed entry counts in the stats only if `Index::get` finds no put entry for its ID. A lookup error fails the open.
    - A writable open flushes only after a pack that is not the active pack, with hwm `(num + 1, 0)`. A read-only open never flushes or writes.
    - `Active.entries` and a rewritten `.idx` keep every entry of the pack. Only the memtable and the stats skip entries below the hwm.
  - `put`: two dedup lookups, as before. Then `add_new` and a flush when the memtable reaches the limit. The flush syncs the active pack, and syncs the directory if it is dirty. The hwm is `(active, len)`, or `(next_pack, 0)` when no pack is active. A flush failure fails the store.
  - `read` returns an index error. `contains` and `stored_len` answer "not found" on an error.
  - `gc_finish`:
    - (A) Builds `by_pack` from one complete `iter_all` pass. Every item goes through `?`. An error returns before any change, and the store is not failed.
    - (B) Copies the live records of the packs to compact. Each copy goes into the memtable with no stats change. The old `punch_pack` is split: `plan_punch` (victims, ranges, the entries the pack keeps) runs here, and `punch_pack` (.idx without the victims, header mark, punch) runs in (E).
    - (C) Writes tombstones and changes the stats, with no I/O.
    - (D) Syncs the active pack, syncs the directory, then flushes. A failure fails the store.
    - (E) Punches, deletes packs, and syncs the directory.
- `run.rs`: item-level `allow(dead_code)` on `Run::num`, `tombstones`, `pack_range` and the struct fields. 17-06 uses them.

## Task Commits

| Task | Name | Commit | Files |
| ---- | ---- | ------ | ----- |
| 1 | Tracer: an object is found through an on-disk run after reopen | f934c98 | index.rs, lib.rs, run.rs, tests/index_tests.rs |
| 2 | Replay above the high-water mark with exact stats | 4ae5f97 | tests/index_tests.rs |
| 1 (follow-up) | A tombstone hides a put in an older run after a crash | 06cf0c2 | tests/index_tests.rs |

## Tests

- `cargo test -q --locked -p chunk-store --test index_tests`: 10 passed.
  - From Task 1: `objects_are_found_through_runs_after_a_reopen`, `the_same_flow_works_through_retrying_io`, `a_gc_removal_is_a_durable_tombstone`, `a_gc_that_cannot_read_the_index_changes_nothing`.
  - From Task 2: `a_flush_inside_the_active_pack_is_replayed_from_its_offset`, `stats_are_the_same_after_a_reopen`, `a_read_only_open_writes_nothing`, `a_store_without_a_manifest_is_indexed_at_its_first_writable_open`, and one extra test, `a_failed_compaction_is_counted_once_after_a_reopen`.
  - Follow-up to Task 1: `a_tombstone_hides_a_put_in_an_older_run_after_a_crash`. The objects are in runs before the GC (limit 30). After the GC, a re-put, a sync and `mem.crash()`, a reopen finds only the re-put object, `objects` is 1, and a put of a second removed object writes it again (`objects` is 2). In `a_gc_removal_is_a_durable_tombstone` (limit 1000) no run ever holds a put entry, so that test did not cover this path.
- `cargo test -q --locked -p chunk-store`: every target passed. The lib has 29 tests: 25 from before, plus 4 new `index::tests` (MANIFEST round trip and damage, newest wins in get and iter_all, a read error ends get and iter_all, a failed flush keeps the memtable and the old MANIFEST). The other targets had 10, 4, 5, 9, 10, 9, 6 and 11 tests. `store_tests` (including `a_sealed_pack_without_its_index_is_scanned_and_indexed` and `a_corrupt_index_is_ignored_and_rebuilt`) did not change.
- Downstream targets:
  - `metadata --test node_crash_tests --test gc_tests`: 4 and 10 passed.
  - `slicefs-cli --test gc_sweep_tests --test gc_online_tests --test gc_cli_tests --test crash_recovery_tests`: 2, 9, 4 and 6 passed. gc_online_tests now has 9 tests; the plan expected 6.
- `cargo test -q --locked -p slicefs-cli -p metadata -p index-bench`: 39 test targets all ok, 976 passed, 0 failed, 6 ignored. The output has no `FAILED`, `panicked` or `error` line.
- The tracer test also checks that the covered packs are not read at open. Their `.idx` files were removed, and a writable open did not write them again. IndexInfo (runs, hwm, memtable) is the same before and after the removal.
- The tracer gate passed: the verify ran end to end before Task 2 started.

### Mutation checks (each run and then reverted)

| Mutant | Tests that failed |
| ------ | ----------------- |
| GC phase A treats an index read error as the end of the index (`else { break }`) | `a_gc_that_cannot_read_the_index_changes_nothing`, at its `is_err` check (the GC returned Ok) |
| GC phase A skips an item that has an error (`else { continue }`) | the same test, at the same check |
| GC phase A keeps the error, goes on with a partial index, and returns the error at the end | the same test, at "no pack is deleted, punched or cut": every pack was gone |
| Replay filters by pack only, not by offset | `a_flush_inside_the_active_pack_is_replayed_from_its_offset`, `a_read_only_open_writes_nothing` |
| Replay counts every entry (no lookup before the count) | `a_failed_compaction_is_counted_once_after_a_reopen` |
| A read-only open flushes at the limit | `a_read_only_open_writes_nothing` |

## TDD Gate Compliance

Task 2 (`tdd="true"`): the four behavior tests were written first and run on the Task 1 code. All four passed, because Task 1 already applied the replay rules: the offset filter, lookup before count, no read-only flush, and flushes only between packs. A RED commit with passing tests would be false, so there is none. To show that the tests can fail, the mutants in the table above were run. Two tests were also made stronger so that they catch a regression. `a_read_only_open_writes_nothing` now has a sealed pack above the hwm and opens with a memtable limit of 1. `a_failed_compaction_is_counted_once_after_a_reopen` was added, because no planned test caught the lookup-before-count mutant.

## Acceptance checks

- `grep -cF 'HashMap<ObjectId, Loc>' crates/chunk-store/src/lib.rs`: 0
- `grep -c SFMF crates/chunk-store/src/index.rs`: 2
- `fn a_gc_that_cannot_read_the_index_changes_nothing`: 1. `fn a_flush_inside_the_active_pack_is_replayed_from_its_offset`, `fn a_read_only_open_writes_nothing` and `fn a_store_without_a_manifest_is_indexed_at_its_first_writable_open`: 1 each
- `const FORMAT_VERSION: u16 = 2;` did not change
- `git diff --quiet origin/develop...HEAD -- crates/chunk-store/src/file.rs crates/slicefs-cli/src/content.rs`: exit 0
- No fjall, and no dependency change.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Bug] next_pack is never below the hwm pack**
- **Found during:** Task 1
- **Issue:** Open used `last pack + 1` for next_pack. Suppose GC deletes every pack after a flush with hwm `(6, 0)`. A new pack would then get number 1, which is below the hwm, so its records would never be replayed after a crash.
- **Fix:** `next_pack = max(last + 1, hwm.pack, 1)`.
- **Commit:** f934c98

**2. [Rule 2 - Missing critical functionality] next_run past orphan run files**
- **Issue:** A crash or a failed MANIFEST write can leave a run file with the number of the next run. The next flush would then fail with "already exists", and every flush after it would fail too.
- **Fix:** A writable open sets next_run past the largest run number on disk. 17-04 still deletes the orphans.
- **Commit:** f934c98

**3. [Rule 2] Sync a rescanned sealed pack before a run can point into it**
- **Issue:** A sealed pack without a valid `.idx` is rescanned at open. Its bytes were not synced. A replay flush could then write a run that points to bytes that are not durable. After a power loss, put could get a stale dedup hit.
- **Fix:** `io.open_rw(num)?.sync()?` before the `.idx` is written.
- **Commit:** f934c98

**4. [Plan refinement] Flush order and the partial run on a MANIFEST failure**
- The plan opens the new run after the MANIFEST write. Here it is opened before, so nothing that can fail comes after the commit point. If it failed after the commit, the MANIFEST on disk would not match the index in RAM.
- The plan removes the partial run file on a failure of the MANIFEST write. Here the run file stays. A failed `write_durable` can still be durable, for example when the rename worked and the directory sync failed. Removing the file would then leave a MANIFEST that lists a missing run. A failure before the MANIFEST write still removes the partial run.
- **Commit:** f934c98

**5. [Plan refinement] flush is a no-op on an empty memtable, and the GC skips (D) and (E) only when nothing changes**
- `gc_finish` returns early only when there is no pack to delete and no pack to punch. Tombstones from punches still reach (D).

**6. [Process] Commits on `feature/17-on-disk-index`, not on an `agent-*` branch**
- The launch message said to switch to this branch, and the stock worktree branch check expects `agent-*`. As in 17-02: this branch is not a protected branch, and HEAD is attached (`refs/heads/feature/17-on-disk-index`).

## Not covered by these tests

- Suppose the active pack is the hwm pack and its open scan ends below `hwm.offset` (damage, or bytes lost in a crash). The open then cuts bytes that runs point to, and later appends land below the hwm, so a crash after them loses them at replay. This open-time rule is 17-04.
- A damaged MANIFEST or a damaged run fails the open with `Corrupt`. Recovery (the rebuild flag, orphan deletion, a run that disappears during a read-only open) is 17-04.
- Without a Bloom filter (17-05), each lookup reads at most one page per run. A miss in a store with many runs costs one read per run. The replay of an old store does one lookup per replayed entry.
- `gc_finish` still builds a per-pack map of all current entries in RAM. 17-07 replaces it.
- docs/design/chunk-store.md is not updated (17-08).

## Known Stubs

None. The filter block of a run is still empty (type 0) by design, as in 17-02. 17-05 adds the filter.

## Threat Flags

None. These threats are mitigated:
- T-17-03: MANIFEST decode checks the magic, version, flags and CRC, uses checked counts, and checks run numbers.
- T-17-04: the tombstones are durable in a run before any pack is deleted or punched. Tested by `a_gc_removal_is_a_durable_tombstone` (put after GC, and after a reopen).
- T-17-05: the memtable has a limit. During replay it can go over the limit by at most one pack.
- T-17-24: GC phase A does the complete index read first. Tested by `a_gc_that_cannot_read_the_index_changes_nothing` and the two phase-A mutants.
- T-17-SC: no dependency change.

## Self-Check: PASSED

- FOUND: crates/chunk-store/src/index.rs, crates/chunk-store/tests/index_tests.rs, crates/chunk-store/src/lib.rs, crates/chunk-store/src/run.rs
- FOUND: f934c98, 4ae5f97, 06cf0c2 (ancestors of HEAD)
