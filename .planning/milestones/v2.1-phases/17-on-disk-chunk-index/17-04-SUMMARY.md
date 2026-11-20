---
phase: 17-on-disk-chunk-index
plan: 04
subsystem: chunk-store index
tags: [index, manifest, rebuild, crash, orphan, damage]
status: complete

requires: [17-03]
provides:
  - Index::open rebuild rules (damage, missing run, rebuild flag), orphan and obsolete run removal at a writable open
  - Index::rebuild and Index::request_rebuild (crate-private, for 17-06 and 17-07)
  - ChunkStore::request_index_rebuild
  - open_with rebuild when the hwm pack is missing or ends below the hwm, and when a replay lookup is Corrupt
  - MemIo fail_next_write_durable, fail_write_durable_after, fail_sync_after, set_fail_removes
  - tests/index_crash_tests.rs (7 crash tests of the run flush)
affects: [17-05, 17-06, 17-07]

actuals:
  tokens: 13000
  tasks: 2
  commits: 5
plan_head_before: c48d351b2fd3499356dbbb19fb12f5924ad72439
plan_head_after: d5dd20ff6c2955cde1499bf4056634225c447749

tech-stack:
  added: []
  patterns:
    - the index files are derived data; damage gives an empty index and a replay of every pack, never a refused open
    - only Corrupt and a missing listed run are damage; any other I/O error fails the open, so a transient error never removes the index
    - a rebuild removes the MANIFEST first, then the runs, then syncs the directory; new run numbers start past every old one
    - open checks the hwm pack before it changes any pack, so a rebuild starts from unchanged packs

key-files:
  created:
    - crates/chunk-store/tests/index_crash_tests.rs
  modified:
    - crates/chunk-store/src/index.rs
    - crates/chunk-store/src/lib.rs
    - crates/chunk-store/src/mem.rs
    - crates/chunk-store/tests/index_tests.rs

key-decisions:
  - "Damage below the hwm (17-03 note 1): rebuild the index from the packs in the same open. The check: the records of the hwm pack below the hwm must be contiguous from the pack header (from the scan, the resync or the .idx). A writable open first resyncs the hwm pack past the damage, syncs it and writes its .idx durably (so no later open cuts it), then removes the MANIFEST and the runs and replays every pack. If the rebuild stops after the .idx write, the next open finds the same gap through the .idx. A read-only open does the same in RAM and writes nothing."
  - "The hwm pack missing while hwm.offset > 0 is the same damage, and gives the same rebuild"
  - "A Corrupt lookup during the replay (a damaged run page) also rebuilds; an Io error still fails the open"
  - "request_index_rebuild() takes no argument (as planned); the crate-private Index::request_rebuild(io, reason) logs the reason, for 17-06 and 17-07"
  - "A failed removal of an orphan run logs a warning and does not fail the open; the next writable open tries again"

duration: about 35 minutes (16:16 to 16:51 local time)
completed: 2026-11-18
---

# Phase 17 Plan 04: index safety at open and under crashes Summary

**A damaged, unknown-version or flagged index is rebuilt from the packs (a read-only open replays the packs into RAM and writes nothing). A writable open removes orphan and obsolete runs. Damage below the high-water mark no longer leaves run entries that point past the end of a pack, and later appends no longer land below the hwm. Seven MemIo crash tests show that a crash at each step of a run flush loses no synced object and gives no wrong Loc.**

## Item 1 from 17-03: damage below the hwm. Decision and evidence

**The problem, shown on HEAD c48d351.** I ran a probe test before any code change. It puts 120 small objects with two flushes, so the hwm is (1, 5414) inside the active pack. It then cuts pack 1 at byte 1644 (inside record 30), reopens, re-puts object 50, puts 10 new objects, syncs, crashes and reopens:

```
PROBE stats after open: Stats { objects: 100, ... }
PROBE contains(50)=true get(50)=false read(50)=Some(Io(Kind(UnexpectedEof)))
PROBE after re-put: get(50)=false
PROBE synced new objects that read back after crash: 0/10
```

So the old open did three things wrong:
- `contains` was true for objects whose bytes were gone.
- A put of such an object was a stale dedup hit. It wrote nothing, so the object was lost without an error.
- New objects were appended below the hwm. A replay skips records below the hwm, so after a crash 10 of 10 synced objects were lost.

The two planned tests also failed on HEAD: `an_active_pack_cut_below_the_hwm_is_rebuilt_from_the_packs` ("object 30: a run points to bytes that are gone") and `a_damaged_record_below_the_hwm_costs_only_itself` (`!s.contains(id)`).

**Decision: (b), but with the rebuild done in the same open, not a failed open.**
- **Why not (a), runs treated as invalid past the pack end:**
  - The MANIFEST counts stay wrong.
  - The hwm stays above the pack end, so later appends still land below it and a crash still loses them.
  - A newer entry that points into the lost range would hide an older valid entry.
- **Why not a plain `Corrupt` error:** `request_index_rebuild` needs an open store, so a failed open would have no repair path.
- **What the rebuild gives:** it replays every pack, so the index then has exactly the records that are in the packs.
  - No Loc points to a missing byte.
  - Every intact record is found.
  - The stats are exact.
  - New appends go to a new pack above any hwm.

**How:**
- `ChunkStore::load` checks the hwm pack first, before it changes any pack. The open needs a rebuild if `hwm.offset > 0` and either:
  - the hwm pack is missing, or
  - the records of the hwm pack below `hwm.offset` do not fill it from the pack header up to `hwm.offset` with no gap (`covers` in lib.rs). The records come from the scan (active pack), from the resync (sealed pack without an `.idx`), or from the `.idx`. The hwm pack was the active pack of the flush that set the hwm, so its records below the hwm are contiguous. GC punches only packs below the hwm pack.
- The flush syncs the active pack before a run can point to it. So these bytes were durable, and this is damage, not a torn tail.
- **Writable open:** it does not cut the pack. It resyncs the pack, so the records after the damage stay. Then it syncs the pack and writes its `.idx` durably, so no later open (for example after a crash during the rebuild) cuts the pack at the damage. Only then does `Index::rebuild` remove the MANIFEST and the runs. The second pass reads the pack from its `.idx` as a sealed pack, and new objects go to a new pack.
- **A repair that stops after the `.idx` write** (the MANIFEST removal fails, or a crash): the next open reads the pack from its `.idx`, finds the same gap, and starts the rebuild again. The first version checked only packs without an `.idx`, and compared only the end of the last record. A review found the problem, and `a_repair_that_stops_after_the_idx_is_done_again_at_the_next_open` failed on that version (commit 47b2d9a: "object 30: a run points to bytes that are gone").
- **Read-only open:** it ignores the runs and replays every pack into RAM. The damaged pack is resynced, not cut, and nothing is written.

**Evidence after the fix:**
- `an_active_pack_cut_below_the_hwm_is_rebuilt_from_the_packs` passes. Objects 30..119 are not found, and a put writes each of them again. Objects 0..29 read back, and the stats are 30. Ten new objects survive a sync and a crash.
- `a_damaged_record_below_the_hwm_costs_only_itself` passes. One flipped payload byte in record 30 loses only object 30, in a read-only open (no file changes) and in a writable open. All 120 objects and 10 new ones survive a crash.
- `a_repair_that_stops_after_the_idx_is_done_again_at_the_next_open` passes, for both the cut and the flip. The MANIFEST removal fails after the `.idx` write, and the open fails. The next open does the repair again, with the same results as above.
- A probe made every detected gap panic, and I ran the chunk-store tests and the downstream tests with it. The probe fired only in the three damage tests above, so the check gave no false positive in the GC, punch, compaction or replay tests.
- Mutants, each run and reverted:
  - no hwm check: both tests fail.
  - a cut instead of a resync in the writable path: the flip test fails.
  - a read-only rebuild that ignores the resync pack: the flip test fails.
  - no check for a pack that has an `.idx` (the first version, commit e365010): `a_repair_that_stops_after_the_idx_is_done_again_at_the_next_open` fails (commit 47b2d9a).

## Item 2 from 17-03: a damaged MANIFEST or run

`Index::open` now sorts what it finds into three cases:
- **Damage:** a MANIFEST that is `Corrupt` (magic, CRC, unknown flags, counts) or of another version; a listed run that is missing; a run that `Run::open` refuses; or a run entry count that does not match the MANIFEST.
  - Writable open: one `tracing::warn` line, `Index::rebuild` (remove the MANIFEST, then every run file, then sync the directory), then a replay of every pack with flushes at the limit.
  - Read-only open: if a listed run is missing, it reads the MANIFEST once more. If that also fails, it ignores the index, starts from hwm (0, 0) with zero stats, and replays every pack into RAM.
- **The rebuild flag:** treated the same as damage.
- **Any other I/O error** (for example EIO): the open fails and the index stays. Test: `a_read_error_at_open_is_not_damage`.

A `Corrupt` lookup during the replay (a damaged run page that an ID above the hwm hits) also rebuilds. Without this, `a_damaged_run_page_gives_no_wrong_data` could not reopen its store after a re-put.

## What was built

- `index.rs`:
  - `Index::open` = `load` (MANIFEST and runs, or a `Damage`) + `opened` + `rebuild`.
  - `opened`:
    - checks the rebuild flag;
    - sets `next_run` past every run file on disk;
    - removes the obsolete runs and every unlisted run file (NotFound is ignored; any other error is a warning);
    - syncs the directory only if something was removed.
  - `Index::rebuild(io, read_only, reason)` and `Index::request_rebuild(io, reason)`. `request_rebuild` sets the flag in RAM (so a later flush keeps it), increases the sequence and writes the MANIFEST with `write_durable`. It reads no run.
- `lib.rs`:
  - `open_with` = `Index::open` + `load`. A second `load` runs on an empty index if the first one returns `Loaded::Rebuild`.
  - New `ChunkStore::request_index_rebuild()` (an error on a read-only store), the `rec_end` and `covers` helpers, and a doc comment on `put` that says the stats can count an object twice after a failed lookup.
- `mem.rs`: four knobs.
  - `fail_next_write_durable(kind, n)`
  - `fail_write_durable_after(kind, skip)`
  - `fail_sync_after(skip)`: counts pack syncs, directory syncs and durable writes.
  - `set_fail_removes(on)`

  The existing knobs did not change.

## Task Commits

| Task | Name | Commit | Files |
| ---- | ---- | ------ | ----- |
| 1 | Open-time cleanup, rebuild of a damaged index, damage below the hwm, damaged pages | e365010 | index.rs, lib.rs, mem.rs, tests/index_tests.rs |
| 2 | Crash tests of the run flush | d057362 | tests/index_crash_tests.rs |
| 1 (review fix, RED) | A repair that stops after the .idx write is done again | 47b2d9a | tests/index_tests.rs |
| 1 (review fix, GREEN) | Check the hwm pack through its .idx too (contiguous records) | d5dd20f | lib.rs |

## Tests

- `cargo test -q --locked -p chunk-store --test index_tests`: 23 passed (10 from 17-03 and 13 new):
  - `an_active_pack_cut_below_the_hwm_is_rebuilt_from_the_packs`
  - `a_damaged_record_below_the_hwm_costs_only_itself`
  - `a_repair_that_stops_after_the_idx_is_done_again_at_the_next_open`
  - `an_orphan_run_is_removed_at_a_writable_open` (also: a failed removal does not fail the open)
  - `a_read_only_open_keeps_an_orphan_run`
  - `a_damaged_manifest_is_rebuilt_from_the_packs`
  - `a_manifest_of_another_version_is_rebuilt_from_the_packs`
  - `a_manifest_that_names_a_missing_run_is_rebuilt`
  - `a_damaged_run_footer_is_rebuilt`
  - `a_read_error_at_open_is_not_damage`
  - `a_damaged_run_page_gives_no_wrong_data`
  - `a_damaged_newer_page_never_shows_an_older_entry`
  - `a_rebuild_request_rebuilds_the_index_at_the_next_writable_open`

  Each rebuild test also checks the read-only open: no run, 300 memtable entries, and no file changed.
- `cargo test -q --locked -p chunk-store --lib`: 30 passed. One is new: `index::tests::the_obsolete_runs_of_the_manifest_are_removed_at_open`.
- `cargo test -q --locked -p chunk-store --test index_crash_tests`: 7 passed. Each test checks five things after `mem.crash()` and a reopen:
  - synced objects are byte-exact;
  - unsynced objects are exact or missing, and a missing one gives `contains` false;
  - `stats().objects` equals the number of objects that read back;
  - `list_runs` count equals `index_info().runs` (no orphan);
  - every run that was committed before the fault is still there. A rebuild would hide an ordering bug, so the test fails if one happens.
- `cargo test -q --locked -p chunk-store`: every target passed (30, 10, 4, 5, 7, 23, 10, 9, 6, 11, 0).
- Downstream, `cargo test -q --locked -p slicefs-cli -p metadata -p index-bench`: 39 test targets all ok, 976 passed, 0 failed, 6 ignored. The output has no `FAILED`, `panicked` or `error` line. I ran it twice: once on d057362, and once on d5dd20f with the gap probe (a panic on every detected gap), with the same counts and no probe line.
- `cargo clippy -p chunk-store --all-targets`: no new warning. The two warnings that are left were there before: `lib.rs:318` type complexity, and `file_tests.rs:69`.

### Mutation checks (each run and then reverted)

| Mutant | Tests that failed |
| ------ | ----------------- |
| No check for a hwm pack that has an `.idx` (the first version of this plan) | `a_repair_that_stops_after_the_idx_is_done_again_at_the_next_open` (RED commit 47b2d9a) |
| The hwm-pack check compares only the end of the last record (not contiguity) | `a_repair_that_stops_after_the_idx_is_done_again_at_the_next_open` (the flip case) |
| No hwm-pack check (the 17-03 behavior) | `a_damaged_record_below_the_hwm_costs_only_itself`, `an_active_pack_cut_below_the_hwm_is_rebuilt_from_the_packs` |
| Writable damage below the hwm: cut instead of resync | `a_damaged_record_below_the_hwm_costs_only_itself` |
| Read-only rebuild ignores the resync pack | `a_damaged_record_below_the_hwm_costs_only_itself` |
| A Corrupt replay lookup fails the open | `a_damaged_run_page_gives_no_wrong_data` |
| The rebuild flag is not kept in RAM | `a_rebuild_request_rebuilds_the_index_at_the_next_writable_open` |
| An I/O error at a run open rebuilds | `a_read_error_at_open_is_not_damage` |
| No orphan removal | `an_orphan_run_is_removed_at_a_writable_open`; in the crash tests `a_crash_after_the_run_file_before_the_manifest`, `a_crash_during_the_replay_of_a_rebuild`, `gc_copies_are_not_counted_twice_after_a_failed_tombstone_flush` |
| A read-only open with damage fails | the six read-only parts of the rebuild tests |
| The flush does not sync the active pack first | `a_crash_after_the_manifest`, `a_crash_after_the_run_file_before_the_manifest`, `a_failed_run_sync` |
| The run name is not made durable before the MANIFEST | `a_crash_after_the_run_file_before_the_manifest` |
| The replay counts every entry | `gc_copies_are_not_counted_twice_after_a_failed_tombstone_flush` |
| A failed flush does not fail the store | `a_crash_after_the_run_file_before_the_manifest`, `a_failed_run_sync` |
| A replay flush marks the next pack as covered (`num + 2`) | `a_crash_during_the_replay_of_a_rebuild` |

## TDD Gate Compliance

- **Task 1:** I wrote the two item-1 tests first and ran them on HEAD c48d351. Both failed, as shown above, and the probe output is the evidence. The other Task 1 tests use APIs that did not exist yet (`request_index_rebuild`, `set_fail_removes`), so their RED state was a compile error. The tests and the code are in one commit (e365010), because the test file could not compile without the new API. The mutants show that each rule has a test that fails without it.
- **Task 2:** all seven crash tests passed on the Task 1 code at the first run. Two assertions had a wrong expected count in the test (24, but the count is 32 memtable entries), and I fixed the test. The tests found no bug in the flush order. To show that the tests can fail, I ran the six flush-order mutants in the table. I also made `a_crash_after_the_manifest` stronger: it now crashes with no sync after the flush, so only the sync inside the flush can make the records durable. The mutant that removes that sync makes the test fail.

## Acceptance checks

- `grep -c 'fn fail_next_write_durable\|fn fail_write_durable_after\|fn fail_sync_after\|fn set_fail_removes' crates/chunk-store/src/mem.rs`: 4
- `grep -c 'fn a_damaged_run_page_gives_no_wrong_data\|fn a_damaged_manifest_is_rebuilt_from_the_packs\|fn an_orphan_run_is_removed_at_a_writable_open' tests/index_tests.rs`: 3
- `grep -c 'fn a_damaged_newer_page_never_shows_an_older_entry\|fn a_rebuild_request_rebuilds_the_index_at_the_next_writable_open' tests/index_tests.rs`: 2
- `grep -c 'fn request_index_rebuild' crates/chunk-store/src/lib.rs`: 1
- `grep -c '#\[test\]' tests/index_crash_tests.rs`: 7. `grep -c 'mem.crash()'`: 8. `grep -cE 'mount|fuser'`: 0
- No fjall, and no dependency change. The tests use `crc32c`, which is a normal dependency of the crate.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Bug] Damage below the hwm lost synced data (17-03 note 1)**
- **Found during:** Task 1, the first step (as the launch message asked)
- **Issue:** see "Item 1" above. The old open gave `contains` true for lost objects, a re-put that wrote nothing, and 0 of 10 synced appends after a crash.
- **Fix:** a rebuild in the same open; a writable open first resyncs the pack and writes its `.idx` durably.
- **Files:** lib.rs (`load`, `Loaded`, `rec_end`), index.rs (`rebuild`)
- **Commit:** e365010

**2. [Rule 2 - Missing critical functionality] A Corrupt replay lookup rebuilds**
- **Issue:** after a put re-wrote an object whose run page was damaged, every later open failed. The replay looks up the ID above the hwm, the lookup reads the damaged page, and the open failed with `Corrupt`. The plan expected "after a reopen get still returns them".
- **Fix:** a `Corrupt` error from the replay lookup returns `Loaded::Rebuild`. An Io error still fails the open.
- **Commit:** e365010

**3. [Rule 2] An I/O error at open is not damage**
- **Issue:** the plan names `Corrupt` and missing runs as damage. Treating every open error as damage would let one transient EIO remove the whole index.
- **Fix:** only `Corrupt` and a NotFound on a listed run lead to a rebuild. Test: `a_read_error_at_open_is_not_damage`.
- **Commit:** e365010

**4. [Plan refinement] Small test changes**
- The plan names `a_damaged_manifest_is_rebuilt_from_the_packs` and the others. They use one helper, `rebuilt_after`. I added a separate test for a MANIFEST of another version, because the truth names that case.
- `gc_copies_are_not_counted_twice_after_a_failed_tombstone_flush` uses 200 objects of 500 bytes, not 100, so that the 64 KiB packs give at least two packs.
- `a_failed_run_sync` and `a_crash_after_the_run_file_before_the_manifest` check that the flush's sync of the active pack happened first: every object up to the flushing put reads back after the crash.

**5. [Rule 1 - Bug] A repair that stopped after the `.idx` write was not done again**
- **Found during:** review after Task 2
- **Issue:** the first version checked the hwm pack only when it had no valid `.idx`, and compared only where the last record ended. The repair writes the `.idx` first. If the MANIFEST removal then failed (or the process stopped), the next open read the pack from its `.idx`, skipped the check, and used the old runs again. That brought back the stale dedup hit. A resynced pack with a lost record in the middle also passed the end comparison.
- **Fix:** `covers` checks that the records below the hwm are contiguous from the pack header, and the check runs for the `.idx`, scan and resync cases.
- **Commits:** 47b2d9a (RED), d5dd20f (GREEN)

**6. [Process] Commits on `feature/17-on-disk-index`, not on an `agent-*` branch**
- The launch message said to switch to this branch, and the stock worktree branch check expects `agent-*`. This branch is not protected, and HEAD is attached (`refs/heads/feature/17-on-disk-index`). 17-02 and 17-03 did the same.
- STATE.md and ROADMAP.md are not updated, as the launch message said.

## Not covered by these tests

- I did not test the read-only path that reads the MANIFEST a second time after a listed run disappears, which happens when a writer process replaces the runs. MemIo has no hook to change the MANIFEST between two reads. If the second read also fails, the path falls back to the replay into RAM, and the `a_manifest_that_names_a_missing_run_is_rebuilt` test covers that fallback.
- Only the hwm pack is checked at open. A pack below the hwm pack that is damaged later is not checked (to check it, the open would have to read every pack). This is the same as a damaged sealed pack anywhere: the read gives `Corrupt`, and a put of that object is a dedup hit. That was also the behavior before Phase 17 (the `.idx` listed the record). Scrub is the tool for this.
- `stats` can count an object twice after a put re-writes an object behind a damaged page, until the next rebuild (now written in the doc comment of `put`).
- A rebuild brings back objects that GC removed but whose packs are not yet deleted or punched. Their bytes are intact and verified, so this is not wrong data, and the next GC removes them again.

## Notes for later plans

- 17-06 and 17-07: when a merge or GC finds a damaged page, call `inner.index.request_rebuild(&*self.io, reason)` (crate-private; it logs the reason). The public `request_index_rebuild()` takes no argument, as the plan says.
- `Manifest::obsolete` is removed at a writable open. `Index::flush` still clears `obsolete` in the new MANIFEST. 17-06 must keep the obsolete list until the files are gone, or rely on the orphan removal: a run that the MANIFEST does not list is removed at the next writable open anyway.

## Known Stubs

None.

## Threat Flags

None. These threats are mitigated:
- T-17-06: the rebuild of a damaged MANIFEST, footer or table, the read-only replay in RAM, page CRCs, no fall-through to an older run, and the rebuild flag. Tested by `a_damaged_newer_page_never_shows_an_older_entry` and the rebuild tests.
- T-17-07: the flush order and orphan removal. Tested by the seven crash tests and the mutants.
- T-17-08 (accepted): a read-only open of a damaged index replays every pack into RAM.
- T-17-SC: no dependency change.

## Self-Check: PASSED

- FOUND: crates/chunk-store/src/index.rs, crates/chunk-store/src/lib.rs, crates/chunk-store/src/mem.rs, crates/chunk-store/tests/index_tests.rs, crates/chunk-store/tests/index_crash_tests.rs
- FOUND: e365010, d057362, 887dba0, 47b2d9a, d5dd20f (in `git log c48d351..HEAD`)
