---
phase: 17-on-disk-chunk-index
plan: 07
subsystem: chunk-store index
tags: [index, gc, tombstone, snapshot, lock-hold, crash-safety]
status: complete

requires: [17-06]
provides:
  - gc_finish from an index snapshot outside the store lock (pass 1 totals per pack, pass 2 one pack at a time from its .idx)
  - pending GC runs (tombstones on disk, in no MANIFEST until the install) and Index::install_gc (one MANIFEST write)
  - a second touched set for the time gc_finish runs; the install drops the plan of a pack that holds such an object
  - ChunkStore::last_gc_timing and GcTiming (longest and total store-lock hold, wall time)
  - Snapshot::get_verified, Run::may_contain, Index::take_run_num, write_pending, plan_merge_of
  - MemIo::fail_read_once_after (a transient read error)
  - cfg(test) ChunkStore::gc_pass_hook
affects: [17-08, 17-09]

actuals:
  tokens: 21000
  tasks: 3
  commits: 6
plan_head_before: 7b3e247b66f8bdb4df7547067db9f2ebc57adaae
plan_head_after: 61cb370e39a473c219c0fe81c4e674853f8d1f13 (code and tests; the docs commit follows)

tech-stack:
  added: []
  patterns:
    - every GC decision reads through verified pages (page CRC checked after the filter); a read error or a damaged page ends the GC before the install, and damage requests a rebuild
    - pending runs are installed newest; the memtable run goes right under them (a victim's put can still be in the memtable)
    - a GC never takes MergeCtl::step (lock order GC mutex, then store lock); merges and the GC install both build from the run list at install time
    - per-pack consistency check: the current entries found from the .idx must match the pass-1 count, else the pack is kept

key-files:
  created: []
  modified:
    - crates/chunk-store/src/lib.rs
    - crates/chunk-store/src/index.rs
    - crates/chunk-store/src/run.rs
    - crates/chunk-store/src/mem.rs
    - crates/chunk-store/src/race_tests.rs
    - crates/chunk-store/tests/gc_tests.rs
    - crates/chunk-store/tests/gc_fault_tests.rs
    - crates/chunk-store/tests/index_tests.rs

key-decisions:
  - "No flush at the start of gc_finish: the snapshot copies the memtable (bounded by the memtable limit). A start flush can fail the store on a read error (flush_at sets failed) and broke three existing fault tests"
  - "Install order [pending runs, memtable run, current list]: tombstones must be newer than a victim's put that is still in the memtable"
  - "After the install, the runs of that install are merged into one run (keeping tombstones), so a GC over N packs does not leave N small runs"
  - "GC copies are read with no lock and appended in batches of 1024 records per store-lock hold (the plan said one pack per hold; that held the lock 1.5 s for a 64 MiB pack of small objects)"
  - "A pack whose .idx gives a different current-entry count than pass 1 is kept, not changed (defense in depth)"
  - "The GC install always writes the hwm (end of the active pack) when any pack changes, also with an empty memtable"

duration: about 55 minutes (start not recorded; from the 17-06 summary commit at 18:23 to the 17-07 summary commit at about 19:20 local time)
completed: 2026-11-18
---

# Phase 17 Plan 07: Online GC on the run index Summary

**gc_finish decides from an index snapshot with no store lock held, writes its tombstones to pending runs on disk, installs them with one MANIFEST write, and only then deletes, compacts or punches packs. Any read error before the install changes nothing.**

## Performance

- Duration: about 55 minutes (see the frontmatter; not measured exactly)
- Tasks: 3 of 3
- Files modified: 8

## What gc_finish does now

1. Start (store lock): the touched set so far decides with `live`; a second touched set records puts and reads during the call; a snapshot copies the memtable and takes the run list. The active pack number is noted.
2. Pass 1 (no lock): one merged, verified pass over the snapshot gives per-pack totals (bytes, kept bytes, entries). RAM: one small struct per pack.
3. The decisions use the same rules as before (keep, punch, compact, delete). Unused packs are those at or below the boundary that are not active and have no totals.
4. Pass 2 (no lock, one pack at a time): the entries come from the pack's `.idx`, or from a resync scan if the `.idx` is missing or damaged. An entry is current only if a verified snapshot lookup points to this pack and offset. Victims go as tombstones to a pending run on disk. Copies of kept records are read with no lock and appended in batches of 1024 under the store lock.
5. Install (store lock): a plan is dropped if its pack holds an object from the second touched set. Then a sync of the active pack and the directory, and `Index::install_gc`: the memtable run (if the memtable has entries) and one MANIFEST write. That write sets the hwm to the end of the active pack whenever a pack will change, also with an empty memtable. The pending runs of dropped plans are removed.
6. Destructive step (no lock): punch plans re-derive their remaining entries from the `.idx` with verified lookups in the live index; delete and compact plans remove the `.idx` and then the pack; unused packs are removed. Then a directory sync.
7. Abort (a read error in steps 2, 4 or 5a): the pending runs are removed and `touched` is cleared; a `Corrupt` error requests a rebuild. Nothing changed except harmless duplicate copies.

Line numbers in `crates/chunk-store/src/lib.rs` at 61cb370 (acceptance item):
- The install MANIFEST: line 1265 (`install_gc`; its only `write_durable` is at `index.rs:922`).
- The first punch: line 1299 (`gc_punch`; `io.punch` at line 1612).
- The first pack removal: line 1303.
- The merge of the install's runs: line 1312, after the destructive step.

The other MANIFEST writes before line 1299 are normal size-limit flushes in pass 2. The plan allows them: they hold copies, not tombstones, and they follow a sync of the active pack. A rebuild request, after damage, is in the abort path, which changes no pack.

## GC store-lock holds (`last_gc_timing`, release build, FileIo on APFS, 1M objects of 128 bytes, default options, background merges off, an idle store: no puts or reads during the GC)

These numbers come from the code before the hwm fix (61cb370). That fix adds no work in these three cases: each already wrote a MANIFEST at the install. With an active load, the install hold also grows with the number of objects touched during the GC (one verified snapshot lookup each).

| Case | Longest hold | All holds | Wall | Result |
|------|--------------|-----------|------|--------|
| Compact (1 of 3 live), copies in one hold per pack (first version) | 1.54 s | 3.83 s | 6.85 s | 666,666 removed, 3 packs compacted |
| Compact (1 of 3 live), batched copies (final) | 0.28 s | 2.24 s | 5.64 s | the same |
| Punch (dead runs of 64 records, 3 of 4 live) | 0.19 s | 0.31 s | 6.25 s | 159,929 removed, 3 packs punched |
| Delete (nothing live) | 0.26 s | 0.35 s | 3.39 s | 1,000,000 removed, 3 packs deleted |

The old gc_finish held the store lock for the whole call, so its longest hold was its wall time. In the final version, the longest hold is a memtable flush or the install's memtable run (up to 262,144 entries, then a run write and an fsync). A foreground put that triggers a flush holds the lock for the same time. The probe was a temporary ignored test and is not committed; 17-08 measures at 10M and 100M.

## Task Commits

1. **Task 1: snapshot-based gc_finish** - `faf2a89` (feat)
2. **Task 2: GC visibility tests** - `a26b40f` (test)
3. **Task 3: read errors, damage, failures, lock-free passes** - `030d83e` (test), `d9d66e1` (test: transient read errors, MemIo knob)
4. **Lock-hold fix found by the measurement** - `02965a5` (perf: batched copies)
5. **hwm fix found in review** - `61cb370` (fix: the install moves the hwm past the changed packs)

## TDD and mutant checks

- `many_packs_gc_with_a_small_memtable` failed on the old code ("1 runs added": the old GC never flushed its copies in the middle). It passes on the new code.
- The four visibility tests all failed on a mutant that installs no pending runs (tombstones never visible).
- `a_read_error_in_the_totals_pass_changes_nothing`, `a_transient_read_error_in_a_gc_pass_changes_nothing` and `a_damaged_page_in_the_gc_passes_requests_a_rebuild` failed on a mutant that skips unreadable items in pass 1.
- A mutant that reads a lookup error in pass 2 as "not current" survived the persistent-fault tests at first, for two reasons: the per-pack count check keeps the pack, and the next `.idx` read fails anyway. I added `MemIo::fail_read_once_after` and `a_transient_read_error_in_a_gc_pass_changes_nothing`. That test catches the mutant: the GC went on and returned Ok after it changed 4 packs.
- `the_store_lock_is_free_during_the_gc_passes` failed on a mutant that drops no plan for objects touched during the GC (X was lost). It also failed on a mutant that holds the store lock in the pass hook.
- `a_damaged_page_in_the_gc_passes_requests_a_rebuild` failed on a mutant with no rebuild request in the abort path.
- `a_read_error_after_pending_runs_changes_nothing` and the transient sweep failed on a mutant that leaves the pending runs after an abort.
- `a_gc_with_an_empty_memtable_moves_the_hwm_past_the_changed_packs` failed before the hwm fix (`hwm_pack: 5`, a pack the GC deleted).

Each mutant was applied to committed code and removed with `git checkout` of that one file, with no other uncommitted work.

## Test results

- `cargo test -q --locked -p chunk-store` at 61cb370: every target passes. lib 51 (race_tests 11; the plan asked for at least 6), file_tests 10, gc_fault_tests 7, gc_tests 11, index_crash_tests 14, index_tests 38, io_tests 10, punch_tests 9, retry_tests 6, store_tests 11.
- `cargo test --locked -p slicefs-cli -p metadata -p index-bench` at 61cb370: exit 0, 976 passed, 0 failed, 6 ignored over 39 test targets; no FAILED or panicked line. This includes the plan's metadata gc_tests and node_crash_tests and the slicefs-cli gc_sweep_tests, gc_online_tests and gc_cli_tests. Earlier runs at d9d66e1 (exit 0, no FAILED line) and at 02965a5 (976 passed, 0 failed) passed too.
- The acceptance greps: `by_pack` 0; `fn last_gc_timing` 1; `fn many_packs_gc_with_a_small_memtable` 1; the four visibility test names 4; `#[test]` in gc_tests.rs 11; the three race test names 3; the three fault test names 3; `sleep` in race_tests.rs 0.
- clippy on chunk-store: only the three warnings that were there before (lib.rs `verified_record`, file_tests.rs, index_tests.rs).

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Bug] No start flush; the snapshot copies the memtable**
- **Found during:** Task 1 (design, confirmed by the existing tests)
- **Issue:** Step 1 says "flush the memtable if it is not empty (a normal flush)". `flush_at` fails the store on any error, also on the reads of `Run::open`. Three existing tests would then break: `a_gc_that_fails_on_a_read_loses_nothing` and `a_gc_that_fails_in_the_middle_of_compaction_loses_nothing` (default memtable limit, read fault from the first read), and `gc_copies_are_not_counted_twice_after_a_failed_tombstone_flush` (its MANIFEST fault would hit the start flush before any copy).
- **Fix:** `Index::snapshot()` (a memtable copy, bounded by the memtable limit) plus `Snapshot::get_verified`. The install puts the pending runs above the memtable run, because a victim's put can still be in the memtable. The start also does no sync of the active pack: `gc_begin` sealed it, and the install syncs the active pack before its MANIFEST write.
- **Files modified:** index.rs, lib.rs
- **Commit:** faf2a89

**2. [Rule 2 - Missing critical functionality] One merged run per GC install**
- **Found during:** Task 1 (`store_with_tombstone_runs` in index_tests.rs expects 5 runs after four GC rounds)
- **Issue:** With one pending run per pack, a GC over N packs adds N small runs. Every lookup then checks N more filters until the background merges catch up.
- **Fix:** After the destructive step, the runs of the install are merged into one run with the normal `write_merge` and `install_merge`. Tombstones are kept even if the window holds the oldest run. A failure is only logged. The merge does not take `MergeCtl::step`, because a GC can run inside the 17-06 merge test hook.
- **Files modified:** index.rs (`plan_merge_of`), lib.rs (`gc_merge_added`)
- **Commit:** faf2a89

**3. [Rule 1 - Bug] Copies batched, reads with no lock**
- **Found during:** the lock-hold measurement
- **Issue:** The plan's "one pack per lock hold" held the store lock for 1.54 s for one 64 MiB pack of 128-byte objects.
- **Fix:** Up to 1,024 records are read with no lock (the pack is sealed, and only GC changes it), then appended under one hold.
- **Commit:** 02965a5

**4. [Rule 1 - Bug] The install moves the hwm also with an empty memtable**
- **Found during:** the final review
- **Issue:** `install_gc` kept the old hwm when it wrote no memtable run, and the install was skipped when there was nothing to add. The old GC always flushed its tombstones from the memtable, so it always moved the hwm. The new GC can delete or punch the hwm pack with an empty memtable. The next open then found the hwm pack missing (or a gap below the hwm) and rebuilt the whole index from the packs.
- **Fix:** `install_gc` always sets the hwm to the end of the active pack, and the install runs whenever a pack changes.
- **Files modified:** index.rs, lib.rs, gc_tests.rs (new test)
- **Commit:** 61cb370

**5. [Rule 2] A per-pack consistency check**
- If the current entries found from a pack's `.idx` do not match the pass-1 count, the GC keeps the pack and logs a warning. A missing `.idx` entry would otherwise leave an index entry that points to a deleted pack.
- **Commit:** faf2a89

**6. [Rule 3] Two existing tests adjusted (the GcStats and Stats checks are unchanged)**
- `race_tests::a_gc_during_a_merge_stays_newer_than_the_merged_run`: runs 2 changed to 3. With a memtable limit of 1, the GC copies now flush in the middle of the GC (Task 1 asks for this). So the GC leaves its tombstone run and its copy run, next to the merged run.
- `index_tests::a_failed_compaction_is_counted_once_after_a_reopen`: `fail_reads_after(15)` now lands in pass 2, before any copy. The test now searches for the first fault point in steps of 5 that leaves copies in the memtable. Its assertions are the same.
- **Commit:** faf2a89

**7. [Rule 2] Extra tests and a MemIo knob**
- `MemIo::fail_read_once_after` (a transient read error) and `a_transient_read_error_in_a_gc_pass_changes_nothing`: the plan's truth names transient read errors, and persistent faults cannot show that bug (see the mutant checks).
- `a_read_error_after_pending_runs_changes_nothing`: nothing live, faults at 70, 150 and 250 reads into pass 2. It checks that no pending run is left.
- **Commits:** 030d83e, d9d66e1

**8. Other notes**
- `Index::tombstone` is now `#[cfg(test)]` (index.rs unit tests use it), not removed. `Index::get_verified` checks the run filter before the page read; without that check, the punch re-derivation would read a page in every run for every entry.
- `a_listed_but_missing_run_requests_a_rebuild` is in race_tests.rs, as the plan places it. It reuses the 17-04 rebuild.
- In Task 2 the memtable limit is 4, not 32. With 12 objects, a limit of 32 never flushes, and the plan wants the objects in runs before the GC.
- The executor's commit check expects an `agent-*` branch. As the dispatch message said, the work is on `feature/17-on-disk-index`.

## Merge and GC install interaction (checked)

Both installs hold the store lock and build the new MANIFEST and run list from the state at install time. `install_gc` adds its runs at the front of the current list. `install_merge` finds its window by run numbers in the current list, and the window stays adjacent when runs are added at the front. Pending GC runs are in no MANIFEST and in no run list until `install_gc`, so a merge cannot pick them. The GC's own post-install merge uses the same window check, so a merge that runs at the same time cannot make either install lose runs. `a_merge_between_gc_begin_and_gc_finish` and the 17-06 race test `a_gc_during_a_merge_stays_newer_than_the_merged_run` cover this.

## Known Stubs

None.

## Threat Flags

None. No new network, auth or file-access surface: the new files are run files in the existing index directory, written through PackIo.

## Next Phase Readiness

- 17-08 can use `last_gc_timing` for the index-bench gc step at 10M and 100M.
- Open: the RAM of the mark set and of the touched sets (deferred in 17-CONTEXT.md; 17-09 drafts the issue). Pending runs keep their filter and fences in RAM until the install (about 2 B per victim).
- Limit for 17-08 (not measured here): each pending run stays open from its write until the post-install merge. With FileIo that is one file descriptor per pack that has victims. A GC over more such packs than the fd limit ends with EMFILE before the install: a clean abort that changes nothing, but the GC never completes. One pending run per pack is the plan's design. Fixes could be to open pending runs only at the install, or to merge them in groups during pass 2. The probe had only 3 packs, so it could not show this.

## Self-Check: PASSED
