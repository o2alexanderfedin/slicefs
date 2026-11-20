---
phase: 17-on-disk-chunk-index
verified: 2026-11-20T09:42:19Z
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
  - crates/chunk-store/src/locality.rs
  - crates/chunk-store/src/locality_tests.rs
  - crates/chunk-store/src/mem.rs
  - crates/chunk-store/src/pack_io.rs
  - crates/chunk-store/src/race_tests.rs
  - crates/chunk-store/src/retry.rs
  - crates/chunk-store/src/run.rs
  - crates/chunk-store/tests/gc_fault_tests.rs
  - crates/chunk-store/tests/gc_tests.rs
  - crates/chunk-store/tests/index_crash_tests.rs
  - crates/chunk-store/tests/index_tests.rs
  - crates/chunk-store/tests/mapped_run_tests.rs
  - docs/design/chunk-store.md
covered_digest: "v4:sha256:9240a29ee47e5a1ed3f050ca999db8e03fe729fc0d35b6d9506d20b843fd8d9e"
behavior_unverified: 0
overrides_applied: 0
re_verification:
  previous_status: passed
  previous_score: 6/6
  reason: "covered files changed after the 2026-11-18 run (follow-up PRs #111, #112, #113, #117, #137 and others between b4b5494 and a7b29a2)"
  gaps_closed: []
  gaps_remaining: []
  regressions: []
---

# Phase 17: On-disk chunk index Verification Report

**Phase Goal:** The chunk store uses an on-disk index with a few bytes of RAM per object and a cold open that does not grow linearly with the object count.
**Verified:** 2026-11-20T09:42:19Z
**Status:** passed
**Re-verification:** Yes. The report of 2026-11-18 (tree `b4b5494`, passed 6/6) was stale: the covered source files changed.

Tree under check: `a7b29a2` (develop, merge of PR #144), on branch `docs/v2.1-close-items` (that tree plus planning-doc edits only; `git diff --stat a7b29a2 -- crates docs` is empty). `git diff e5e55e1 a7b29a2` changes only `.planning/v2.1-MILESTONE-AUDIT.md`, so the code is the same as on `e5e55e1`, where the milestone integration check ran `index_crash_tests`, `gc_mark_tests`, `mapped_run_tests` and `race_tests` (all pass).

CI on `a7b29a2` (run 44704112946, push to develop, `headSha` = a7b29a2): "Linux (tests, FUSE smoke, live checks, pjdfstest, in Docker)" success, "macOS (cargo test, FUSE-T build)" success. The macOS job runs `cargo test --workspace` (`.github/workflows/ci.yml:44-45`). The chunk-store tests cited below have no `#[ignore]`, feature or OS gate (`race_tests` and `locality_tests` are `#[cfg(test)]` modules of lib.rs; the only OS gate in `tests/` is in `punch_tests.rs`, which is not cited), so they ran green on this exact tree.

Code changes since the last run that touch the phase (b4b5494..a7b29a2, chunk-store: +5,511 / -389 lines): run format 2 with per-page field widths and shared key bytes (#137, `run.rs`); idle merge of the newest runs (#111); a bounded locality cache in front of the index for the dedup check of `put` (#112, `locality.rs`); GC mark with a Bloom filter and spilled touched set (#113, `mark.rs`, `extsort.rs`); full GC; background pack seal; mapped-run tests (#117). Each was re-checked below against the four success criteria.

## Goal Achievement

### Observable Truths

| # | Truth (ROADMAP SC + goal) | Status | Evidence |
|---|---|---|---|
| 1 | SC1: a lookup does at most one disk read on a hit and usually none on a miss | VERIFIED (amortized) | `Run::get` (run.rs:898): Bloom filter in RAM, then `locate` by tag table and fences in RAM, then one page (in place from the mapping, or one `read_at`). A second page only if the fence of the page equals the 8-byte prefix of the key (run.rs `search`). `Index::get` (index.rs:593): memtable shard, then runs newest first. Test `hits_read_one_page_and_misses_read_almost_none` (index_tests.rs:1043), unchanged by format 2: I ran `cargo test -p chunk-store --test index_tests` on this tree: 38 passed, 0 failed. `a_hit_on_files_reads_the_run_page_in_place` (index_tests.rs:973): ok. The new locality cache (lib.rs:1388 `dedup_lookup`) answers some `put` dedup checks from RAM before the index; it adds no read. Note (unchanged): "at most one" is an average; a Bloom false positive in a newer run or a fence-equal prefix adds a page. Accepted in CONTEXT.md. #111 idle merge brings a settled store to 1 run (chunk-store.md "Idle merge (#111)"). |
| 2 | SC2: crash during a run flush and a merge loses no object and gives no wrong `Loc` | VERIFIED | `tests/index_crash_tests.rs` (16 tests; was 14): `reopen_and_check` (line 61) asserts synced objects read back byte-exact, unsynced ones exact or missing, exact `stats().objects`, no orphan run, committed runs kept. Flush cases (lines 110-259) and merge cases (lines 301-451) are all present; new: two idle-merge tests (lines 504, 539). Green in CI on a7b29a2 (macOS `cargo test --workspace`) and run by the integration checker on e5e55e1 (same code). |
| 3 | SC3: no lookup returns an object that online GC removed | VERIFIED | Tests present and green in CI on a7b29a2: `gc_tests.rs` `on_runs::removed_objects_stay_missing_for_each_kind_of_pack` (324), `a_merge_between_gc_begin_and_gc_finish` (342), `a_merge_after_gc_drops_the_tombstones_and_removed_objects_stay_missing` (362), `the_put_dedup_check_of_a_removed_object_writes_it` (411); `race_tests.rs` `a_put_hit_that_gc_removed_before_the_lock_writes_again` (42), `a_gc_during_a_merge_stays_newer_than_the_merged_run` (157); `gc_fault_tests.rs` (7 tests). New lookup path (#112 locality cache) has its own GC safety: only exact `.idx` entries are cached, GC clears the exact bit durably and calls `untrust` before any change (locality.rs module doc), with tests `a_gc_drops_the_cached_pack_of_a_removed_object`, `a_cache_hit_that_gc_removed_before_the_lock_writes_again`, `a_crash_at_each_step_of_a_gc_never_leaves_a_stale_exact_idx` and 13 more in `src/locality_tests.rs`. Code: `write_pending` (index.rs:930), `install_gc` (index.rs:966). |
| 4 | SC4: Phase 16 benchmark with the new index shows RAM/object, cold-open time, lookup and write throughput against the baseline | VERIFIED | `docs/design/chunk-store.md` "Index results (#85)" (line 484): Store, Open, Lookups, Recovery and GC tables with "#66" columns; macOS 1M/10M/100M, Linux 1M/10M. Follow-up sections add "Index size" (line 696, format 2: index 59.6 to 39.5 B/object at 10M), "Idle merge (#111)" (line 801) and "Index with a cold page cache" (line 901, Linux, cold in the VM). #85 closed. Numbers not re-run here (100M runs take hours). |
| 5 | Goal: a few bytes of RAM per object | VERIFIED | Test `the_index_ram_is_about_two_bytes_per_object` (index_tests.rs:1093) asserts filter + fence bytes <= 2.0 B/object; ran: ok. Recorded steady anonymous RAM 2.19 B (10M) and 3.29 B (100M) on macOS. No map of all objects in `src/`: the `HashMap<ObjectId, _>` uses are the locality cache `IdMap` (locality.rs:92), capped at `MAX_PACKS` = 16 packs and `MAX_ENTRIES` = 65,536 entries (locality.rs:66-69), and the per-reader node cache of `FileReader` (file.rs:229, `NODE_CACHE` = 16, not new since b4b5494). Both are constant size, not per-object. GC RAM bound is #113 (closed). |
| 6 | Goal / IDX-02: cold open reads only footers, tables and filters, and does not grow linearly with object count | VERIFIED (with caveat) | `Run::open` (run.rs:662) reads footer, tag table, fence table and filter, and no page. `Index::open`/`load` (index.rs:388, 405) read the MANIFEST and open each run. `ChunkStore::open` uses it (lib.rs:1168; rebuild on damage, lib.rs:1172). Test `a_cold_open_reads_no_entry_page` (index_tests.rs:1069): ran, ok. Measured open 0.060 s at 10M, 0.492 s at 100M (#66: 12.8 s, 141 s). Caveat (unchanged): filter and fence bytes read at open are linear in N (about 1.3 B/object), so "not linear" holds for the dominant cost, not for every byte; the cold-page-cache numbers are Linux-only and "cold in the VM". |

**Score:** 6/6 truths verified (0 present, behavior-unverified)

### Required Artifacts

| Artifact | Expected | Status | Details |
|---|---|---|---|
| `crates/chunk-store/src/run.rs` | run file: pages, tag table, fences, filter, footer; `get`, `get_verified` | VERIFIED | 1,799 lines; format 2 (#137); untrusted lengths checked at open; used by index.rs |
| `crates/chunk-store/src/index.rs` | memtable, MANIFEST with hwm, flush, merge, GC install, rebuild | VERIFIED | 1,328 lines; used by `ChunkStore::open` and every lookup path |
| `crates/chunk-store/src/bloom.rs` | blocked Bloom filter | VERIFIED | 167 lines; used by `Run::open` and the run writer |
| `crates/chunk-store/src/lib.rs` | store uses `Index`; merge thread; GC | VERIFIED | merge thread spawned at lib.rs:1051 |
| `crates/chunk-store/src/locality.rs` | (new, #112) bounded dedup cache in front of the index | VERIFIED | 416 lines; bounded; GC-safe by exact-`.idx` rule; 16 tests |
| `crates/chunk-store/src/mem.rs` | MemIo read counters, fault knobs, `crash()` | VERIFIED | used by read-count, crash and fault tests |
| `crates/chunk-store/tests/index_crash_tests.rs` | flush and merge crash tests | VERIFIED | 16 tests |
| `crates/chunk-store/tests/index_tests.rs` | read-count, cold-open, RAM tests | VERIFIED | 38 tests, run here: all pass |
| `gc_tests.rs`, `gc_fault_tests.rs`, `race_tests.rs`, `mapped_run_tests.rs` | GC visibility, faults, races, mapped runs | VERIFIED | 13 + 7 + 14 + 3 tests; green in CI |
| `docs/design/chunk-store.md` | "Index on disk (#85)" and "Index results (#85)" | VERIFIED | lines 233 and 484, plus follow-up sections at 696, 801, 901 |

PLAN frontmatter `must_haves` (all 9 plans): `verify.artifacts` 20/20 passed and `verify.key-links` 17/17 verified (exit 0 for each plan) on this tree. The plans have no `prohibitions:` and no `<human-check>` blocks. Plan-level truths are covered by the six roadmap truths above.

### Key Link Verification

| From | To | Via | Status | Details |
|---|---|---|---|---|
| `ChunkStore::open` | `Index::open` / `Index::rebuild` | lib.rs:1168, 1172 | WIRED | damage leads to a rebuild from the packs |
| `contains` / `get` / `put` dedup | `Index::get` | lib.rs:1392, 2353, 2431, 2906 | WIRED | outside the store lock, `gen` revalidation (lib.rs:2358) |
| `put` dedup | `LocalityCache` then `Index::get` | lib.rs:1388-1392 | WIRED | a cache miss or a disabled cache goes to the index |
| `Index::get` | `Run::get` | index.rs:600-603 | WIRED | newest run first; a run error stops the search |
| GC | `write_pending` / `install_gc` | index.rs:930, 966 | WIRED | tombstones durable before packs change |
| merge thread | `pick_merge` / `write_merge` / `install_merge` | lib.rs:1051, index.rs:729, 796, 843 | WIRED | `background_merges_keep_the_run_count_small` green |

### Data-Flow Trace (Level 4)

Not applicable: a storage library with no rendered output. Lookups return `Loc` values from run pages or the memtable; the read-count and crash tests check the bytes read back against the stored data.

### Behavioral Spot-Checks

| Behavior | Command | Result | Status |
|---|---|---|---|
| SC1 read counts, cold open, RAM per object, plus 35 other index tests | `cargo test -p chunk-store --test index_tests` | 38 passed, 0 failed (1.06 s) | PASS |
| SC2, SC3 crash, GC, race, locality tests | CI run 44704112946 on a7b29a2, macOS `cargo test --workspace` | success | PASS (CI evidence) |

### Probe Execution

None. The phase declares no probe scripts.

### Requirements Coverage

| Requirement | Source Plan | Description | Status | Evidence |
|---|---|---|---|---|
| IDX-01 | 17-02, 17-03, 17-05 | sorted runs, sparse key table, memtable, filter per run; at most one read on a hit, usually none on a miss | SATISFIED | truth 1 |
| IDX-02 | 17-04, 17-05 | cold open reads only footers and filters; open time not linear in object count | SATISFIED | truth 6 (caveat: filter/fence bytes are O(N) at about 1.3 B/object) |
| IDX-03 | 17-03, 17-04, 17-06, 17-07 | packs are the source of truth; replay above the hwm; crash in flush or merge loses nothing | SATISFIED | truth 2; `Index::rebuild` on damage; a format-1 run is damage and leads to a rebuild (no store backward compatibility needed) |
| IDX-04 | 17-06, 17-07 | background merges keep the run count small; no lookup returns a GC-removed object | SATISFIED | truth 3; merge thread and idle merge (#111) |
| IDX-05 | 17-08, 17-09 | #66 benchmark with the new index against the baseline | SATISFIED | truth 4; #85 closed |

No orphaned requirements: REQUIREMENTS.md maps only IDX-01..05 to Phase 17, and each one is claimed by a plan.

### Anti-Patterns Found

| File | Line | Pattern | Severity | Impact |
|---|---|---|---|---|
| (none) | - | `TBD`, `FIXME`, `XXX`, `TODO`, `HACK`, `todo!`, `unimplemented!` in chunk-store `src/` and `tests/` | - | none found |
| `crates/chunk-store/src/run.rs` | 623, 825 | `#[allow(dead_code)]` pack range | Info | kept on purpose ("a GC must not skip a run by its pack range") |

### Human Verification Required

None. All six truths have automated test evidence on this tree (`index_tests` run here; the full workspace suite in CI on a7b29a2). Benchmark numbers are recorded evidence and are not re-run.

### Non-critical notes (tech debt)

1. "At most one disk read on a hit" is an average, not a per-lookup bound. Accepted in CONTEXT.md.
2. Open reads filter and fence bytes that are linear in N (about 1.3 B/object). The dominant cost (the #66 full scan) is gone.
3. The "Index results (#85)" table is for run format 1; format 2 numbers are in "Index size". The 100M results were not re-taken with format 2.
4. Cold-page-cache numbers are Linux in a VM only (host cache may serve reads); no macOS cold run, no HDD, no 100M cold run. Documented in chunk-store.md.
5. A run file cut by another program after the open (SIGBUS on the mapping) stays an accepted risk (audit disposition, #117).

Items of the previous report that are now closed: lookup run count (#111, closed), locality cache (#112, closed), GC mark RAM (#113, closed), disk +58 % (#137: index 59.6 to 39.5 B/object at 10M), untested mapped-run paths (#117).

### Gaps Summary

No gaps. The four ROADMAP success criteria and IDX-01..05 hold on `a7b29a2`. The follow-up changes (run format 2, idle merge, locality cache, new GC mark) keep the phase tests and their thresholds unchanged (format 2 changed only the byte offsets in the damage tests), and the full suite is green in CI on this tree.

---

_Verified: 2026-11-20T09:42:19Z_
_Verifier: Claude (gsd-verifier)_
