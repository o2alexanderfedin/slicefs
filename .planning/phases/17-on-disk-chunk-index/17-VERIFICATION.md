---
phase: 17-on-disk-chunk-index
verified: 2026-11-18T00:00:00Z
status: passed
score: 6/6 must-haves verified
covered_files:
  - .planning/phases/17-on-disk-chunk-index/17-01-PLAN.md
  - .planning/phases/17-on-disk-chunk-index/17-01-SUMMARY.md
  - .planning/phases/17-on-disk-chunk-index/17-02-PLAN.md
  - .planning/phases/17-on-disk-chunk-index/17-02-SUMMARY.md
  - .planning/phases/17-on-disk-chunk-index/17-03-PLAN.md
  - .planning/phases/17-on-disk-chunk-index/17-03-SUMMARY.md
  - .planning/phases/17-on-disk-chunk-index/17-04-PLAN.md
  - .planning/phases/17-on-disk-chunk-index/17-04-SUMMARY.md
  - .planning/phases/17-on-disk-chunk-index/17-05-PLAN.md
  - .planning/phases/17-on-disk-chunk-index/17-05-SUMMARY.md
  - .planning/phases/17-on-disk-chunk-index/17-06-PLAN.md
  - .planning/phases/17-on-disk-chunk-index/17-06-SUMMARY.md
  - .planning/phases/17-on-disk-chunk-index/17-07-PLAN.md
  - .planning/phases/17-on-disk-chunk-index/17-07-SUMMARY.md
  - .planning/phases/17-on-disk-chunk-index/17-08-PLAN.md
  - .planning/phases/17-on-disk-chunk-index/17-08-SUMMARY.md
  - .planning/phases/17-on-disk-chunk-index/17-09-PLAN.md
  - .planning/phases/17-on-disk-chunk-index/17-09-SUMMARY.md
  - crates/chunk-store/src/bloom.rs
  - crates/chunk-store/src/file.rs
  - crates/chunk-store/src/index.rs
  - crates/chunk-store/src/lib.rs
  - crates/chunk-store/src/mem.rs
  - crates/chunk-store/src/pack_io.rs
  - crates/chunk-store/src/race_tests.rs
  - crates/chunk-store/src/retry.rs
  - crates/chunk-store/src/run.rs
  - crates/chunk-store/tests/gc_fault_tests.rs
  - crates/chunk-store/tests/gc_tests.rs
  - crates/chunk-store/tests/index_crash_tests.rs
  - crates/chunk-store/tests/index_tests.rs
  - docs/design/chunk-store.md
covered_digest: "v4:sha256:760890d5a0ad5daa13a9ed1b04094b3dcf3538931fdb353b68ce2586fa4a1394"
behavior_unverified: 0
overrides_applied: 0
---

# Phase 17: On-disk chunk index Verification Report

**Phase Goal:** The chunk store uses an on-disk index with a few bytes of RAM per object and a cold open that does not grow linearly with the object count.
**Verified:** 2026-11-18 (retroactive, milestone v2.1 audit)
**Status:** passed (with non-critical tech debt, see below)
**Re-verification:** No. This is the first verification.

Tree under check: `b4b5494` (merge of PR #114). `git diff 07f5836 b4b5494` is empty, so this is the exact tree on which PR #114 CI ran. Both CI checks are SUCCESS: "Linux (tests, FUSE smoke, live checks, pjdfstest, in Docker)" and "macOS (cargo test, FUSE-T build)". The code came from PR #109 (merged as `fa0c354`).

## Goal Achievement

### Observable Truths

| # | Truth (ROADMAP SC + goal) | Status | Evidence |
|---|---|---|---|
| 1 | SC1: a lookup does at most one disk read on a hit and usually none on a miss | VERIFIED (amortized) | `Run::get` (run.rs:662) checks the Bloom filter in RAM, then the fence table in RAM, then reads one page (in place from the mapping on FileIo, one `read_at` on MemIo). `Index::get` (index.rs:551) checks the memtable shard, then the runs from newest to oldest. Test `hits_read_one_page_and_misses_read_almost_none` (index_tests.rs:1023): 12 runs, 1,000 hits read 1,000 to 1,000 + 3 % x runs x 1,000 pages; 1,000 misses read at most that false-positive bound; only whole 4 KiB pages. I ran it: ok. Test `a_hit_on_files_reads_the_run_page_in_place` shows 0 `read_at` calls for lookups on FileIo (mapped). Note: "at most one" holds on average. A Bloom false positive in a newer run, or a key whose 8-byte prefix equals a page fence, adds a page read. CONTEXT.md accepted this ("about 1 % false positives per run"). |
| 2 | SC2: crash during a run flush and a merge loses no object and gives no wrong `Loc` | VERIFIED | `tests/index_crash_tests.rs`: `reopen_and_check` asserts each synced object reads back byte-exact, an unsynced one is exact or missing (and `contains` is false), `stats().objects` equals what reads back, no orphan run, committed runs not removed (so no hidden rebuild). Flush cases: failed pack sync, failed run sync, crash after the run file before the MANIFEST, crash after the MANIFEST, power loss after a flush, crash during a rebuild replay. Merge cases: crash after merge output before MANIFEST, crash after MANIFEST before input removal, read error during a merge, damaged page found by the merge thread, failed merge on the thread. I ran 2 of them: ok. CI ran all. |
| 3 | SC3: no lookup returns an object that online GC removed | VERIFIED | `tests/gc_tests.rs` `on_runs::removed_objects_stay_missing_for_each_kind_of_pack` (deleted, compacted, punched packs; after a reopen), `a_merge_between_gc_begin_and_gc_finish`, `a_merge_after_gc_drops_the_tombstones_and_removed_objects_stay_missing`, `the_put_dedup_check_of_a_removed_object_writes_it` (put writes again, not a stale dedup hit). `src/race_tests.rs`: `a_put_hit_that_gc_removed_before_the_lock_writes_again`, `a_gc_during_a_merge_stays_newer_than_the_merged_run`, read-error passes that change nothing. `tests/gc_fault_tests.rs`: crash in the middle of `gc_finish`. Code: GC writes tombstones to pending runs and installs them (index.rs:856, 892) and bumps `gen` before packs change; lookups outside the lock revalidate on `gen` (lib.rs:1035, 1737). I ran the 2 gc_tests and all 14 race_tests: ok. |
| 4 | SC4: Phase 16 benchmark with the new index shows RAM/object, cold-open time, lookup and write throughput against the baseline | VERIFIED | `docs/design/chunk-store.md` "Index results (#85)" (line 461): Store, Open, Lookups, Recovery and GC tables, each with a "#66" column. macOS 1M/10M/100M, Linux 1M/10M. The same table is in the #85 comment (posted text equals `17-issue-85-comment.md`). Numbers were not re-run in this audit (100M runs take hours). |
| 5 | Goal: a few bytes of RAM per object | VERIFIED | Steady anonymous RAM after open: 2.19 B/object at 10M, 3.29 B at 100M (macOS); 2.38 B at 10M (Linux). The part that grows with N (filters + fences) is 1.36 B/object. Test `the_index_ram_is_about_two_bytes_per_object` asserts filters + fences <= 2 B/object; I ran it: ok. The in-RAM `HashMap<ObjectId, Loc>` is gone from `src/` (grep finds none). Caveat: GC still holds a live set and touched sets that grow with N (follow-up #113). |
| 6 | Goal / IDX-02: cold open reads only footers, tables and filters, and does not grow linearly with object count | VERIFIED (with caveat) | `Run::open` (run.rs:445) reads the footer, tag table, fence table and filter; it reads no page. `Index::open`/`load` (index.rs:352) reads the MANIFEST and opens each listed run. Test `a_cold_open_reads_no_entry_page` (index_tests.rs:1049): run bytes read < 1/20 of entry bytes, `.idx` reads only for packs at or above the hwm, 1 MANIFEST read. I ran it: ok. Measured open: 0.060 s at 10M, 0.492 s at 100M (#66: 12.8 s, 141 s). Caveat: the filter and fence bytes read at open are linear in N (1.36 B/object), and the 10M to 100M open time grew 8x. The doc says the replay above the hwm (bounded by one pack plus the memtable) is most of it, but it is "probable", not measured. All open numbers are warm page cache; a "cold open" here is a new process, not a cold page cache. |

**Score:** 6/6 truths verified (0 present, behavior-unverified)

### Required Artifacts

| Artifact | Expected | Status | Details |
|---|---|---|---|
| `crates/chunk-store/src/run.rs` | sorted run file: pages, tag table, fences, filter, footer; `get`, `get_verified`, `iter` | VERIFIED | 1,392 lines; checked untrusted lengths at open; wired from index.rs |
| `crates/chunk-store/src/index.rs` | memtable (16 shards), MANIFEST with hwm and counts, flush, merge, GC install, rebuild | VERIFIED | 1,233 lines; used by lib.rs `ChunkStore::open` (lib.rs:868) and all lookup paths |
| `crates/chunk-store/src/bloom.rs` | blocked Bloom filter, 10 bits/key | VERIFIED | 167 lines; used by `Run::open` and `RunWriter` |
| `crates/chunk-store/src/lib.rs` | store uses `Index`; background merge thread; GC from snapshot | VERIFIED | merge thread spawned at lib.rs:781 when `index_background_merge` (default true); `request_rebuild` on damage |
| `crates/chunk-store/src/mem.rs` | MemIo read counters, fault knobs, `crash()` | VERIFIED | used by crash, GC fault and read-count tests |
| `crates/chunk-store/tests/index_crash_tests.rs` | flush and merge crash tests | VERIFIED | 14 tests |
| `crates/chunk-store/tests/gc_tests.rs`, `gc_fault_tests.rs`, `src/race_tests.rs` | GC visibility, faults, races | VERIFIED | see truth 3 |
| `docs/design/chunk-store.md` | "Index on disk (#85)" (line 229) and "Index results (#85)" (line 461) | VERIFIED | both sections present with method, tables and notes |

### Key Link Verification

| From | To | Via | Status | Details |
|---|---|---|---|---|
| `ChunkStore::open` | `Index::open` / `Index::rebuild` | lib.rs:868, 872 | WIRED | damage leads to a rebuild from the packs |
| `ChunkStore::contains/get/put` | `Index::get` | lib.rs:1030, 1732, 1810 | WIRED | outside the store lock, with `gen` revalidation |
| `Index::get` | `Run::get` | index.rs:558 | WIRED | newest run first; a run error stops the search |
| `gc_finish` | `Index::write_pending` / `install_gc` | lib.rs (gc path), index.rs:856, 892 | WIRED | tombstones durable before pack change |
| merge thread | `pick_merge` / `write_merge` / `install_merge` | lib.rs:781, index.rs:687-772 | WIRED | `background_merges_keep_the_run_count_small` |

### Data-Flow Trace (Level 4)

Not applicable in the UI sense. The data path is pack record -> memtable -> run -> lookup. Crash tests check that path end to end with byte-exact reads.

### Behavioral Spot-Checks

| Behavior | Command | Result | Status |
|---|---|---|---|
| IDX-01 page reads | `cargo test -p chunk-store --test index_tests -- hits_read_one_page_and_misses_read_almost_none` | ok | PASS |
| IDX-02 open reads | `... --test index_tests -- a_cold_open_reads_no_entry_page the_index_ram_is_about_two_bytes_per_object` | ok, ok | PASS |
| IDX-03 crash | `... --test index_crash_tests -- a_crash_after_the_run_file_before_the_manifest a_crash_after_the_merge_output_before_the_manifest` | ok, ok | PASS |
| IDX-04 GC | `... --test gc_tests -- removed_objects_stay_missing_for_each_kind_of_pack a_merge_after_gc_drops_the_tombstones` | ok, ok | PASS |
| GC races | `cargo test -p chunk-store --lib -- race_tests` | 14 passed | PASS |
| Full suite | PR #114 CI on the same tree (Linux Docker, macOS cargo test) | SUCCESS | PASS |

### Probe Execution

No probe scripts are declared for this phase. Step 7c: not applicable.

### Requirements Coverage

| Requirement | Source Plan | Description | Status | Evidence |
|---|---|---|---|---|
| IDX-01 | 17-01, 17-02, 17-03, 17-05 | Sorted runs of (ObjectId, Loc) on disk with a sparse key table, a memtable, one filter per run in RAM. A hit does at most one disk read, a miss usually none. | SATISFIED | run.rs layout and `Run::get`; index.rs memtable shards; bloom.rs; test `hits_read_one_page_and_misses_read_almost_none` (1 page per hit plus about 1 % per newer run; misses <= false-positive bound). |
| IDX-02 | 17-01, 17-02, 17-03, 17-05 | Cold open reads only run footers and filters; open time does not grow linearly with object count. | SATISFIED (caveat) | `Run::open` reads no entry page; test `a_cold_open_reads_no_entry_page`; open 0.06 s at 10M and 0.49 s at 100M vs 12.8 s and 141 s. Filter/fence read is O(N) at 1.36 B/object; warm cache only. |
| IDX-03 | 17-03, 17-04, 17-06, 17-07 | Packs stay the source of truth; entries not yet in a run are found again from the packs; a crash during flush or merge loses no object and gives no wrong `Loc`. | SATISFIED | MANIFEST hwm and replay above it (index.rs header, lib.rs:2193); `Index::rebuild` from packs on damage; `index_crash_tests.rs` (14 tests); pack and `.idx` formats unchanged. |
| IDX-04 | 17-06, 17-07 | Runs merge in the background so their number stays small; no lookup returns an object that online GC removed. | SATISFIED | merge thread (lib.rs:781), `background_merges_keep_the_run_count_small`; GC tests in gc_tests.rs, gc_fault_tests.rs, race_tests.rs. Run count is 3 / 5 / 9 at 1M / 10M / 100M; that count slows lookups (see debt, #111). |
| IDX-05 | 17-08, 17-09 | The #66 benchmark runs with the new index and shows RAM/object (target: a few bytes), cold-open time, hit and miss lookup throughput, write throughput, against the baseline. | SATISFIED | "Index results (#85)" tables with "#66" columns; #85 closed (2026-11-19T07:06Z) with the comment that equals `17-issue-85-comment.md`. The requirement asks to show the numbers, and it does; some numbers are worse than the baseline (see below). |

No orphaned requirements: REQUIREMENTS.md maps only IDX-01..05 to Phase 17, and each one is claimed by at least one plan.

### Anti-Patterns Found

| File | Line | Pattern | Severity | Impact |
|---|---|---|---|---|
| (none) | - | `TBD`, `FIXME`, `XXX`, `TODO`, `HACK`, `todo!`, `unimplemented!` in chunk-store src/tests and index-bench src | - | none found |
| `crates/chunk-store/src/run.rs` | 608 | `#[allow(dead_code)] pack_range` | Info | kept on purpose ("a GC must not skip a run by its pack range") |

### Results that miss the baseline (honest list)

| Result | New | #66 | Covered by |
|---|---|---|---|
| contains hit, 100M, macOS | 3,183 ns | 820 ns (3.9x slower) | #111 (fewer runs; also asks for a second warm pass to separate run count from page faults) |
| contains hit, 10M, macOS | 1,385 ns | 701 ns (2.0x) | #111 |
| contains miss, 100M / 10M, macOS | 1,171 / 544 ns | 420 / 324 ns | #111 |
| Linux hit/miss at 1M and 10M | 858-1,268 / 501-573 ns | 695-887 / 266-305 ns | #111 |
| put dedup-hit, 100M, macOS | 2,051 ns | 975 ns (2.1x) | #112 (locality cache) and #111 |
| Disk per object | 164 B | 104 B (+58 %) | no follow-up issue; no target was set. The run entry carries the pack number (58 B) and the `.idx` files stay. |
| GC process peak RSS, 100M | 7,427 MB | no baseline | #113 (live set and touched sets) |
| 1M steady RSS per object | 182 B (macOS) | 390 B | better than #66, but far from "a few bytes"; fixed part (memtable + replay of one pack) divided by 1M. Expected, documented. |

Better than the baseline: open time (141 s -> 0.49 s at 100M), RAM per object (105.5 -> 4.02 B steady RSS at 100M), creator peak RSS (16.3 GB -> 3.9 GB at 100M), create puts per CPU second about equal or better.

### Non-critical gaps / tech debt

1. Lookup latency is worse than the RAM map at 10M and 100M. The cause at 100M (run count vs page faults in a new process) is not measured. Covered by #111.
2. "At most one disk read on a hit" is an average, not a per-lookup bound: a false positive in a newer run or a fence-equal prefix adds a page. Accepted in CONTEXT.md; the test asserts the average bound.
3. Open time at 10M -> 100M grew 8x (0.06 -> 0.49 s). The split between filter load (O(N), 1.36 B/object) and the bounded replay is not measured. No follow-up issue.
4. No cold page cache and no HDD numbers. Stated in the #85 comment; no follow-up issue.
5. Disk use +58 % per object (164 vs 104 B). No target and no follow-up issue.
6. GC RAM still grows with the live object count. Covered by #113.
7. 100M was run on macOS only; GC step on macOS only. Documented.
8. Known untested paths (from 17-05/17-06 SUMMARYs): SIGBUS on a mapped read error, merges on FileIo while a reader holds a mapping, `install_merge` window-changed guard. Documented, not tested.
9. The benchmark numbers were taken once (create and lookup) under a loaded host; this audit did not re-run them.

### Human Verification Required

None. All truths have automated test evidence (run in this audit for the key ones, and the full chunk-store suite in PR #114 CI on the same tree). The benchmark numbers are recorded evidence and are not re-run here.

### Gaps Summary

No blocking gaps. The four ROADMAP success criteria and IDX-01..05 are met in the code and tests, and #85 is closed with the posted comment and follow-ups #111, #112, #113 (all open). The main weak result is lookup speed: hits are 2x to 3.9x slower than the #66 RAM map at 10M and 100M on macOS, and misses are 1.7x to 2.8x slower. #111 covers this. Three smaller items have no follow-up issue: the unmeasured open-time split at 100M, no cold-cache numbers, and the +58 % disk use.

---

_Verified: 2026-11-18_
_Verifier: Claude (gsd-verifier)_
