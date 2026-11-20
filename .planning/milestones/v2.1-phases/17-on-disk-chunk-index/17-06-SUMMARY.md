---
phase: 17-on-disk-chunk-index
plan: 06
subsystem: chunk-store index
tags: [index, merge, lsm, tiered, thread, crash, tombstone]
status: complete

requires: [17-05]
provides:
  - Index::pick_merge, write_merge, install_merge, remove_runs (crate-private), index::tier
  - the Newest merge (one place of the newest-wins rule; Snapshot::iter_all uses it)
  - ChunkStore::merge_index_now, merge_index_all, wait_index_idle, index_run_entries
  - Options::index_merge_fan_in (4) and index_background_merge (true); IndexInfo::run_tombstones
  - one merge thread per writable store; impl Drop for ChunkStore (stop and join)
  - cfg(test) hook ChunkStore::after_merge_write (between the merge write and the install)
affects: [17-07, 17-08]

actuals:
  tokens: 26000
  tasks: 2
  commits: 5  # git rev-list --count 906e7ae..86608ee (4 code and test commits, and the first SUMMARY commit f20c60e); this SUMMARY update commit follows
plan_head_before: 906e7ae60f508662e0aca403a3d207b80a589fce
plan_head_after: 86608eed4c732b4632528331ebfdd12bb71fe881

tech-stack:
  added: []
  patterns:
    - a merge picks under the store lock, writes with no lock, and installs under the store lock from the state at install time
    - the output run number comes from next_run in RAM at pick time, so a flush during the merge cannot take it
    - inputs are removed only after the merge MANIFEST is durable; readers keep their Arc<Run>
    - the merge thread holds Arcs of the parts it needs, never the ChunkStore, so Drop runs and joins it
    - lock order: merge step lock, then the store lock; the merge state lock is a leaf

key-files:
  created: []
  modified:
    - crates/chunk-store/src/index.rs
    - crates/chunk-store/src/lib.rs
    - crates/chunk-store/src/run.rs
    - crates/chunk-store/src/race_tests.rs
    - crates/chunk-store/tests/index_tests.rs
    - crates/chunk-store/tests/index_crash_tests.rs
    - crates/chunk-store/tests/gc_fault_tests.rs
    - crates/chunk-store/tests/punch_tests.rs
    - crates/chunk-store/tests/io_tests.rs

key-decisions:
  - "Tier = integer floor of log base fan_in of max(1, entries / memtable_limit), by an integer loop; fan-in is clamped to at least 2"
  - "The install builds the run list and the MANIFEST from the state at install time (a flush or GC during the merge keeps its newer run), and checks that the window is still adjacent (and still the oldest if it drops tombstones)"
  - "After a failed merge MANIFEST write the output file stays (the write may be durable), as for a flush; its number is never used again and the next writable open removes it if unlisted. Failures before that write remove the partial output"
  - "A merge whose output has no entry (every entry a dropped tombstone) installs no run: the inputs go"
  - "A Corrupt input page requests a rebuild; pick_merge returns None while a rebuild is requested, so no merge runs until the next open. An Io error requests nothing"
  - "No merge on a failed store (pick and install both check Inner::failed)"
  - "close() stops and joins the thread before it takes the store lock; merge_index_now and merge_index_all still work after close (they use their own stop flag)"
  - "Tests that use MemIo fault knobs, read counters or a fixed run list set index_background_merge false"

duration: about 60 minutes (17:25 to 18:28 local time)
completed: 2026-11-18
---

# Phase 17 Plan 06: background tiered merges with a crash-safe install Summary

**Index runs now merge by a size-tiered policy (fan-in 4) on one background thread per writable store. A merge reads its inputs through the verified page path, keeps the newest entry per ID, drops tombstones only when its window holds the oldest run, and installs with a durable MANIFEST before it removes its inputs. 2,000 objects in 40 runs end as 4 runs (200, 200, 800, 800 entries). `close()` and `Drop` stop and join the thread, and seven MemIo crash and fault tests show that a merge loses no object and gives no wrong Loc.**

## What was built

### Task 1: the merge step (0300940)

- `index.rs`:
  - `tier(entries, limit, fan_in)`: integer loop, no f64.
  - `Index::pick_merge(fan_in, limit, all)` runs under the store lock. It walks the runs from the oldest to the newest and takes the first window of `fan_in` adjacent runs of one tier. `all` takes every run, and also one run alone if it has tombstones. It takes the output number from `next_run` in RAM. It returns `None` while a rebuild is requested.
  - `Index::write_merge(io, plan, stop)` runs with no lock.
    - It reads each input with `Run::iter`, so every page CRC is checked.
    - It merges with `Newest`: for each ID the entry of the newest input wins.
    - It drops tombstones only if the window holds the oldest run.
    - It streams the output through `RunWriter` (filter sized from the sum of the input counts; sync every 64 MiB and at the end), then `sync_dir`, then `Run::open` of the output, all before the MANIFEST.
    - It checks the stop flag about once per input page.
    - On an error or a stop it removes the partial output. A read error is never skipped.
    - An output with no entries is removed and installs as "no run".
  - `Index::install_merge(io, plan, out)` runs under the store lock. It finds the window in the current run list and in the MANIFEST list, splices the output in, and writes the MANIFEST with `write_durable`: sequence + 1, the same hwm, counts and flags, and `obsolete` = the inputs. Only then does it swap the run list and bump `gen`. It returns the input numbers. If the window moved, it removes the output and returns `None`.
  - `Index::remove_runs(io, nums)`: after the store lock is released. A failed removal is only logged.
  - `Newest`: the newest-wins merge, now the only one. `MergeIter` (`Snapshot::iter_all`, used by GC phase A) filters its put entries.
- `lib.rs`:
  - `Merger` (io, store lock, index, merge control, knobs) with `step(all, stop, between)`:
    1. take the merge step lock;
    2. pick under the store lock (not on a failed store);
    3. write with no lock; a `Corrupt` error calls `Index::request_rebuild` under the store lock (once);
    4. run the `between` hook;
    5. install under the store lock;
    6. remove the inputs.
  - `merge_index_now()`, `merge_index_all()`: one step each; refused on a read-only store.
  - `index_run_entries()`: the entry count of each live run, newest first (the tests compute the tiers from it).
  - `Options::index_merge_fan_in = 4`, `Options::index_background_merge = true`, `IndexInfo::run_tombstones`.
- `run.rs`: `Run::num` and `Run::tombstones` are used now (their `dead_code` allows are gone).

### Task 2: the merge thread (6558928, d269932, 86608ee)

- `MergeCtl`, shared by the store, `Inner` and the thread:
  - the step lock;
  - a stop flag;
  - a state (`pending`, `busy`, `running`) with a `Condvar`.
- `Inner::flush_at` notifies after every good flush. This covers size flushes, the GC flush and the replay flushes at open.
- `ChunkStore::new` starts the thread (`chunk-store-merge`) on a writable store with background merges. It starts with `pending` true, so runs from earlier sessions merge too. The thread holds a `Merger` (Arcs), never the `ChunkStore`.
- The thread loop:
  1. wait until `pending` or stop;
  2. merge until the policy finds no window, or stop;
  3. log an error with one `tracing::warn` line and wait for the next flush. It never sets `Inner::failed`.
  4. A drop guard clears `running` and `busy` at the end, also after a panic.
- `stop_merges()`: sets stop under the state lock, then joins. `close()` calls it before it takes the store lock. `impl Drop for ChunkStore` calls it too.
- `wait_index_idle()`: waits while `running && (pending || busy)`. It returns at once when background merges are off, on a read-only store, and after close.

## Tests

- `cargo test -q --locked -p chunk-store --test index_tests`: 38 passed (30 before, 8 new):
  - `merges_keep_the_run_count_small`: 40 runs become exactly `[200, 200, 800, 800]` entries (newest first). No 4 adjacent runs share a tier, runs ≤ 9 and below 10 + log4(2000), `run_entries` 2000, stats unchanged, every object reads back, the same after a reopen.
  - `a_merge_of_newer_runs_keeps_the_tombstones`: one merged oldest run (tier 1) and four GC runs (tier 0). The policy merges the four GC runs only. The oldest run stays, the tombstone count stays, removed IDs stay missing, and the same holds after a reopen.
  - `merge_index_all_drops_the_tombstones`: 1 run and 0 tombstones. The content is the same, and the same after a reopen. A put of a removed object writes it again.
  - `a_merge_with_nothing_left_writes_no_run` (added): GC removes all 100 objects, then `merge_index_all` gives 0 runs.
  - `a_damaged_page_found_by_a_merge_leads_to_a_rebuild`:
    - `Corrupt` from `merge_index_all`, with no output kept;
    - `merge_index_now` is false after that;
    - puts still work;
    - a read-only open has 0 runs and 301 memtable entries, reads every object, and writes nothing;
    - a writable reopen rebuilds: every object is byte-exact, the stats are exact, and the old runs are gone.
  - `a_read_only_store_keeps_reading_after_a_merge_removed_its_runs`: B opened before the merge and C opened after it both read every object.
  - `a_read_only_store_refuses_a_merge` (added).
  - `lookups_never_miss_a_stored_object_during_background_merges` (added): the 17-05 concurrent lookup test with background merges on, then `wait_index_idle`, runs ≤ 9.
- `--lib race_tests::`: 5 passed (2 new, with the `after_merge_write` hook; no threads, no timing):
  - `a_flush_during_a_merge_keeps_the_new_runs`: two flushes between the write and the install. Runs `[1, 1, 4]` after the merge and after a crash and reopen.
  - `a_gc_during_a_merge_stays_newer_than_the_merged_run`: a GC removes X and moves the others (their pack is deleted) between the write and the install. X stays missing, the others read from the new pack, and the same holds after a crash. A put of X writes it again.
- `--lib index::`: 6 passed (1 new): `a_stopped_merge_leaves_no_output` (a merge with its stop flag set returns `Ok(None)`; run files and MANIFEST unchanged; the next merge works with a new number).
- `--test index_crash_tests`: 14 passed (7 before, 7 new). Each one ends with the 17-04 `reopen_and_check`: synced objects exact, stats equal to what reads back, the run files equal to the MANIFEST runs, and the committed runs still there (no rebuild).
  - `a_crash_after_the_merge_output_before_the_manifest`
  - `a_crash_after_the_merge_manifest_before_the_inputs_are_removed`
  - `a_read_error_during_a_merge_keeps_the_store_working`: runs of 150 entries (3 pages), so `fail_reads_after(5)` fails inside the merge. The error is `Io`, the partial output is removed, no rebuild is requested, and a later merge works.
  - `a_drop_during_background_merges_returns`: limit 16, 2,000 puts, sync, drop at once. After the drop `MemIo::open_handles()` is 0, then crash and reopen.
  - `background_merges_keep_the_run_count_small`: `wait_index_idle`, then the tier rule, runs ≤ 9, inputs removed. After `close()`, 100 more puts flush and `wait_index_idle` still returns.
  - `a_damaged_page_found_by_the_merge_thread_leads_to_a_rebuild` (added): ten runs, a flipped byte in the oldest run, reopen with the thread. After `wait_index_idle`: no output kept, 10 runs, `merge_index_now` false, puts and gets work. The next open rebuilds, and all 360 objects read back.
  - `a_failed_merge_on_the_thread_is_tried_again_after_the_next_flush` (added): the thread's first MANIFEST write fails. After `wait_index_idle`: 10 runs in use, the output file stays. 40 more puts flush, the thread merges (fewer than 10 runs, tier rule), every object reads back. After a crash, the reopen removes the orphan output.
- `cargo test -q --locked -p chunk-store` on 86608ee: every target passed (45, 10, 4, 5, 14, 38, 10, 9, 6, 11, 0).
- `cargo test -q --locked -p slicefs-cli --test crash_recovery_tests --test gc_online_tests --test write_path_tests`: 6, 9, 24 passed, no hang.
- Downstream, `cargo test -q --locked -p slicefs-cli -p metadata -p index-bench` (once, on d269932): 39 test targets all ok, 976 passed, 0 failed, 6 ignored, exit 0, no hang; the output has no `FAILED`, `panicked` or `error` line. The later commit 86608ee changes only chunk-store tests.
- The five thread tests (`background`, `drop_during`, `thread` filters: 5 tests) ran 30 times in a loop, each run under a 60 s timeout: 30 of 30 passed, no hang.
- `cargo clippy -p chunk-store --all-targets` on 86608ee: no new warning. The three that are left were there before (`verified_record` type complexity, `file_tests.rs:69`, `index_tests.rs:512`).
- `grep -c 'thread::sleep' tests/index_crash_tests.rs`: 0. No mounts, no fjall, no dependency change.

### Mutation checks (each run, then reverted)

| Mutant | Tests that failed |
|---|---|
| Tombstones always dropped (`drop_tombs: true`) | `a_merge_of_newer_runs_keeps_the_tombstones`, `merges_keep_the_run_count_small` |
| `pick_merge` does not reserve the output number | `a_flush_during_a_merge_keeps_the_new_runs`, `a_gc_during_a_merge_stays_newer_than_the_merged_run` |
| No rebuild request on a `Corrupt` input page | `a_damaged_page_found_by_a_merge_leads_to_a_rebuild` |
| Inputs removed before the MANIFEST write | `a_crash_after_the_merge_output_before_the_manifest` |
| The thread exit does not clear `running` | `background_merges_keep_the_run_count_small` hangs in `wait_index_idle` after `close` (killed by a 60 s timeout) |
| `Drop` does not stop the thread | `a_drop_during_background_merges_returns` (9 handles still open) |

## TDD Gate Compliance

**Not followed.** In both tasks I wrote the code first and the tests after it. No RED run happened.
- The new tests use APIs that did not exist before (`merge_index_now`, `merge_index_all`, `index_run_entries`, `run_tombstones`, `wait_index_idle`, `after_merge_write`), so a RED run would only have been a compile error.
- Every new test passed at its first run, except one assertion that was wrong in the test (see deviation 10).
- The evidence that the tests can fail is the six mutants in the table above. Each one was run and then reverted.
- This is listed as deviation 11.

## Acceptance checks

Each value was measured on 86608ee:
- `grep -c 'fn merge_index_now\|fn merge_index_all' crates/chunk-store/src/lib.rs`: 2
- `grep -c 'fn merges_keep_the_run_count_small\|fn merge_index_all_drops_the_tombstones\|fn a_read_only_store_keeps_reading_after_a_merge_removed_its_runs' crates/chunk-store/tests/index_tests.rs`: 3
- `grep -c 'index_merge_fan_in' crates/chunk-store/src/lib.rs`: 4
- `grep -c 'fn a_damaged_page_found_by_a_merge_leads_to_a_rebuild' crates/chunk-store/tests/index_tests.rs`: 1
- `grep -c 'impl Drop for ChunkStore' crates/chunk-store/src/lib.rs`: 1
- `grep -c 'fn wait_index_idle' crates/chunk-store/src/lib.rs`: 1
- `grep -c 'thread::sleep' crates/chunk-store/tests/index_crash_tests.rs`: 0
- `grep -cE 'fn a_crash_after_the_merge_(output_before_the_manifest|manifest_before_the_inputs_are_removed)' crates/chunk-store/tests/index_crash_tests.rs`: 2
- `git diff --stat 906e7ae..HEAD -- Cargo.lock crates/chunk-store/Cargo.toml`: empty (no dependency change)

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Bug guard] The install uses the state at install time**
- **Issue:** the plan says "the live list with the output in place of the inputs". A MANIFEST built from the state at pick time drops runs flushed during the merge. Their records are below the hwm, so no replay brings them back.
- **Fix:** `install_merge` finds the window in the current run list and MANIFEST list and splices it there. The test is `a_flush_during_a_merge_keeps_the_new_runs`, a deterministic hook test (no timing).
- **Commit:** 0300940

**2. [Rule 2] A failed merge MANIFEST write keeps the output file**
- **Issue:** the plan says "remove the partial output" for a failure. After a failed `write_durable` the MANIFEST may still be durable, and then it lists the output.
- **Fix:** follow the flush rule. Keep the file, never reuse its number, and let the next writable open remove it if no MANIFEST lists it. The test `a_crash_after_the_merge_output_before_the_manifest` checks both: the file stays, and the reopen removes it.

**3. [Rule 2] An empty merge output installs no run**
- **Issue:** GC can remove every object. Then a merge that drops the tombstones has nothing left.
- **Fix:** the empty file is removed, and the inputs go with no replacement. Test: `a_merge_with_nothing_left_writes_no_run`.

**4. [Rule 2] No merge on a failed store**
- **Issue:** after a failed sync or flush, the in-RAM state may differ from the disk.
- **Fix:** both pick and install check `Inner::failed`. This is a guard; no test exercises it.

**5. [Plan refinement] `index_run_entries()`**
- **What changed:** a small public accessor (entry count per run, newest first). The plan's checks ("no 4 adjacent runs share a tier") need per-run sizes, and `IndexInfo` has only totals.

**6. [Plan refinement] Background merges off in more test helpers**
- **What changed:** besides `index_tests` and `index_crash_tests`, the helpers in `gc_fault_tests`, `punch_tests`, `io_tests` and `race_tests` set `index_background_merge: false`.
- **Why:** they use MemIo fault knobs or counters, which a merge thread would consume.
- The default stays `true`, so `gc_tests`, `store_tests`, the unit tests and all of slicefs-cli run with the thread.

**7. [Plan refinement] Concurrency test with merges**
- `lookups_outside_the_lock_never_miss_a_stored_object` now runs twice: without and with background merges.

**8. [Process] Commits on `feature/17-on-disk-index`**
- The launch message said to commit there, as 17-02 to 17-05 did. STATE.md and ROADMAP.md are not updated, as the launch message said.

**9. [Process] A mutant revert discarded uncommitted Task 2 code**
- During a mutation check I restored `lib.rs` with `git checkout`, before the Task 2 code was committed. That threw away the Task 2 changes to `lib.rs`. I applied the same edits again, ran the full chunk-store suite (all green), and committed. After that, every mutant ran on committed code.
- The "thread exit does not clear `running`" mutant was run on the first copy, which is the same code.
- A perl edit of a test file also damaged its first doc line once. I saw it in the diff, fixed it before the commit, and checked that the diff of that file adds lines only.

**10. [Test fix] A wrong assertion in a new test**
- The first version of `a_failed_merge_on_the_thread_is_tried_again_after_the_next_flush` compared the count of new run files with the live run count. That is not a rule: old runs can stay live. It failed, and I replaced it with the real check: the orphan output file exists before the reopen and is gone after it.
- Commit: 86608ee

**11. [Process] TDD order not followed**
- The code of each task was written before its tests (see "TDD Gate Compliance"). The six mutants are the evidence that the tests detect the faults they target.

**12. [Review additions] Tests added after a review of the first SUMMARY**
- These were added: the two thread-failure tests, the stopped-merge unit test, the exact run sizes, and the 30-run loop.
- The first SUMMARY commit (f20c60e) had these errors, which are now corrected:
  - a wrong commit count;
  - an `index_merge_fan_in` count that was not measured;
  - no Self-Check section.

## Not covered by these tests

- **A window that changed between pick and install:** `install_merge` returns `None`. One merge runs at a time, and flushes only add runs at the new end, so no test path reaches it. It is a guard.
- **`close()` without the thread stop:** no test separates it from `Drop`, which also stops.
- **A stop in the middle of a merge on the thread:** `a_drop_during_background_merges_returns` may join an idle thread (its `sync()` gives the thread time to finish). The stop path of `write_merge` is covered by the unit test `a_stopped_merge_leaves_no_output`, and the thread passes the same flag.
- **`close()` callers:** I checked them. `close()` stops the merges for the rest of the life of the store. In slicefs-cli it is called only at unmount (`filesystem.rs` `destroy`) and at the end of the offline tools (`gc.rs`, `seed.rs`, `scrub.rs` test), so a mount keeps its merges until unmount.
- **FileIo merges:** the merge path on FileIo, with mapped runs and real unlinks while a reader holds a mapping, is not tested in this plan. The 17-05 test `a_run_round_trips_through_each_io` covers reading a mapped run after its file is removed. slicefs-cli runs real stores with the thread on.
- **Merge RAM and speed at scale:** not measured. That is 17-08.

## Notes for later plans

- **17-07 (GC outside the lock):**
  - A merge can run while GC reads its snapshot. GC's run is newer than any merge output, and the install keeps it (the race test above).
  - `gc_finish` installs under the store lock, and the merge install also takes it, so they are ordered.
  - A merge never drops a tombstone unless its window holds the oldest run.
- **17-08 (bench):** call `wait_index_idle()` before measuring lookups. Report the run count. With the default 2^18 memtable limit, a run is about 15 MiB, and a merge of 4 such runs at tier 0 reads and writes about 60 MiB.

## Known Stubs

None.

## Threat Flags

None beyond the plan's threat model:
- T-17-12 is mitigated by the adjacency check, newest-wins, the tombstone rule, the MANIFEST before removal, Arc readers, no skipped page, and the rebuild on `Corrupt`. Tested by three crash tests, the damage test, the read-only test and two race tests.
- T-17-13 is mitigated: the thread holds no `ChunkStore`, the stop flag is checked per page, and `close` and `Drop` join. Tested by the drop test (open handles 0), the slicefs-cli targets and the full downstream run.
- T-17-14 is mitigated: the merge streams one page per input plus the output page, and `RunWriter` syncs every 64 MiB.
- T-17-SC: no dependency change.

## Self-Check: PASSED

- FOUND: crates/chunk-store/src/index.rs, crates/chunk-store/src/lib.rs, crates/chunk-store/src/race_tests.rs, crates/chunk-store/tests/index_tests.rs, crates/chunk-store/tests/index_crash_tests.rs, this SUMMARY
- FOUND: 0300940, 6558928, d269932, f20c60e, 86608ee (in `git log 906e7ae..HEAD`)
