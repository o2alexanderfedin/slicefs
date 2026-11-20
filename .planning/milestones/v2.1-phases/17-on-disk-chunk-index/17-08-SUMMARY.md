---
phase: 17-on-disk-chunk-index
plan: 08
subsystem: chunk-store index, benchmarks
tags: [index, benchmark, rss, gc, lock-hold, emfile, docs]
status: complete

requires: [17-07]
provides:
  - index-bench store-wide disk_bytes, index_bytes, --memtable-limit, index keys in open, anon/file RSS split, covered_packs in recover, gc step
  - gc_finish in groups of GC_GROUP_PACKS (32) packs, so a GC holds a bounded number of run files open (EMFILE fix)
  - MemIo::peak_handles / reset_peak_handles
  - docs/design/chunk-store.md "Index on disk (#85)", "Index results (#85)", and index text in Storage strategies, Durability and GC
affects: [17-09]

actuals:
  tokens: 25000
  tasks: 2
  commits: 6
plan_head_before: 8393b570235737f6d0b4d7160fcb7dbe6458c869
plan_head_after: d6fef98 (the SUMMARY commit follows; commits counts it)

tech-stack:
  added: []
  patterns:
    - RSS split per process: RssAnon/RssFile (Linux), task_info(TASK_VM_INFO) internal/external (macOS, struct copied from the SDK header, no new crate)
    - the GC runs steps 4 to 6 per group of packs; touched packs carry over between groups

key-files:
  created:
    - .planning/phases/17-on-disk-chunk-index/17-08-SUMMARY.md
  modified:
    - benchmarks/index-bench/src/main.rs
    - benchmarks/index-bench/src/backend.rs
    - benchmarks/index-bench/tests/cli_tests.rs
    - benchmarks/index-bench/README.md
    - crates/chunk-store/src/lib.rs
    - crates/chunk-store/src/mem.rs
    - crates/chunk-store/src/race_tests.rs
    - crates/chunk-store/tests/gc_tests.rs
    - docs/design/chunk-store.md

key-decisions:
  - "EMFILE was real: a GC over 330 packs with victims failed at ulimit -n 256 (Too many open files). Fixed by grouped GC (32 packs per install/destroy/merge), not by merging pending runs across packs (that breaks the per-pack plan drop) and not by reopening runs by path (an open per verified lookup)"
  - "gc step default is the plan's literal pattern (every 4th object dead, --dead-run 1); the measured GC runs use --dead-run 1024, because in a full 64 MiB pack single dead 8-byte objects never cover a whole 4 KiB block, so nothing is removed"
  - "recover reads hwm_pack with an untimed read-only open before the .idx files move"
  - "The post-install merge of a GC group takes the prefix of runs that were not in the list before the install, so a background merge of some of them does not stop it"

duration: about 1 h 45 min (19:22 to about 21:05 local; the benchmark runs took about 50 min of it)
completed: 2026-11-18
---

# Phase 17 Plan 08: index benchmark against the #66 baseline Summary

**The on-disk index needs 2.2 to 3.3 bytes of anonymous RAM for each object after open (#66: 106 to 196 B), opens a 100M store in 0.49 s (#66: 141 s), and holds the store lock at most about 105 ms in a GC at 10M and at 100M. Lookups are slower than with the map in RAM at 10M and 100M (a hit up to 3.9 times at 100M: 3.2 µs against 0.8 µs); at 1M on macOS they are not. The benchmark also found a real EMFILE limit in the 17-07 GC, now fixed.**

## Task Commits

1. **Task 1: bench changes** - `0eb63bb` (feat): store-wide disk_bytes and index_bytes, --memtable-limit, wait_index_idle before close, index keys, anon/file RSS split, covered_packs, gc step, new table columns, README.
2. **EMFILE fix (deviation)** - `c1c20d7` (fix): GC in groups of 32 packs, MemIo peak counter, three tests.
3. **Task 2: design note** - `92421b2` (docs): "Index on disk (#85)", "Index results (#85)", Storage strategies, Durability, GC; README disk size.
4. **First SUMMARY** - `2be04aa` (docs).
5. **Review fixes** - `d6fef98` (test): a race test for the touched-set re-arm after each group install (fails on a mutant without the re-arm), the open-run bound stated, lookup notes corrected. Test and comment only; the measured code is unchanged.

All measurements are from commit `c1c20d7` (the binary was copied out of the target directory before the runs). The stores lived in a scratch directory outside the repo ($D) and the Docker volume; all were deleted after each size, and the Docker volumes `slicefs-index-bench` and `slicefs-target-idx85` were removed at the end.

## Results (warm cache; "#66" = baseline)

macOS host: Xeon W-3245, 192 GiB, macOS 26.2, APFS, load average 13 to 19 (#66: 80 to 140). Linux: OrbStack VM, kernel 7.2.12, btrfs, VM load 1 to 3.

| OS | Objects | Open s | #66 | Steady anon B/obj | Steady file B/obj | Steady RSS B/obj | #66 | Peak B/obj | #66 | hit ns | #66 | miss ns | #66 | put dedup ns | #66 | Create puts/s | #66 | Disk B/obj | #66 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| macOS | 1M | 0.626 | 0.964 | 153.8 | 22.0 | 182.0 | 390.2 | 182.0 | 390.2 | 1,056 | 1,305 | 701 | 727 | 1,280 | 1,284 | 124,120 | 68,240 | 151.1 | 104 |
| macOS | 10M | 0.060 | 12.844 | 2.19 | 0.90 | 4.56 | 195.8 | 4.56 | 256.3 | 1,385 | 701 | 544 | 324 | 1,374 | 945 | 129,574 | 64,026 | 163.6 | 104 |
| macOS | 100M | 0.492 | 141.241 | 3.29 | 0.50 | 4.02 | 105.5 | 4.02 | 154.2 | 3,183 | 820 | 1,171 | 420 | 2,051 | 975 | 113,722 | 72,220 | 163.8 | 104 |
| Linux | 1M | 0.506 | 0.595 | 31.2 | 46.4 | 77.6 | 153.4 | 149.7 | 302.0 | 858 | 695 | 501 | 266 | 1,125 | 796 | 239,909 | 132,072 | 151.1 | 104 |
| Linux | 10M | 0.039 | 10.364 | 2.38 | 10.27 | 12.6 | 123.2 | 12.7 | 192.7 | 1,268 | 887 | 573 | 305 | 1,392 | 932 | 233,179 | 144,880 | 163.6 | 104 |

- Marginal RAM (100M - 10M) / 90M: 3.41 B anon (3.95 B all RSS). Filters + fences are 1.36 B/object at both sizes; the rest is fixed (memtable plus the replay of the hwm pack, whose whole `.idx` is decoded: 579,440 entries at 100M, 57,944 at 10M).
- Recovery: only the packs at or above the hwm are scanned (80 of 81 packs covered at 100M), so a recovery open is about a normal open; not like for like with #66 (said in the doc).
- Create puts per CPU s: macOS 141.7k / 138.0k / 117.8k (#66 117k / 125k / 122k); Linux 251.6k / 208.1k (#66 137k / 152k). Creator peak RSS 3.9 GB at 100M (#66 16.3 GB).

## GC (macOS, fresh stores, 3 of 4 live, ulimit -n 256)

| Objects | Dead run R | Wall s | Lock hold max ms | Lock hold total ms | Removed | Punched / compacted / deleted | gc peak RSS MB |
|---|---|---|---|---|---|---|---|
| 10M | 1,024 | 60.6 | 108.3 | 336.4 | 2,387,518 of 2,500,608 | 8 / 1 / 0 | 1,416 |
| 100M | 1,024 | 745.5 | 104.5 | 384.1 | 24,073,153 of 25,000,192 | 81 / 0 / 0 | 7,427 |
| 10M | 1 | 34.0 | 111.9 | 345.7 | 14,486 of 2,500,000 | 0 / 1 / 0 | n/a |

The longest hold does not grow with the object count. The wall time is about 6 to 7.5 µs per object.

## EMFILE finding

- `ulimit -n` in the agent shell: 1048576 (kern.maxfilesperproc 245760); `launchctl limit maxfiles`: soft 256. No code in the repo raises the limit.
- The 100M store has only 81 packs, so the 100M GC could not show the limit (with the old code it needed about 81 run files plus up to 64 pack readers).
- Reproduction (index-bench, 400,000 objects, `--pack-limit 65536`: 330 packs; gc `--dead-run 1`, so every pack has victims; `ulimit -n 256`): `chunk store I/O: Too many open files (os error 24)`, nothing changed. With `--dead-run 1024` only about half of the packs had victims and it passed, which is why the first try did not show it.
- Fix (`c1c20d7`): steps 4 to 6 of gc_finish run for groups of at most 32 packs. Test `a_gc_over_many_packs_keeps_a_bounded_number_of_handles_open` (150 packs): 165 handles at one time before the fix (bound 104), passes after it. A mutant with one group (the old behavior) gave 216 handles and failed. Two more tests: a put during group 1 keeps its pack in the last group (killed a mutant that clears the touched packs per group); a read error in group 2 leaves group 1 done, the store not failed, and the counts exact after a reopen. In d6fef98 a fourth test: a put during the pass of group 2 keeps its pack (killed a mutant that does not arm the touched set again after an install). Open-run bound: at most 32 pending runs plus one merged run per group done, until the background merges take them; shown up to 330 packs.
- After the fix the same reproduction completes at `ulimit -n 256` and at 128. All three measured GC runs ran at 256.

## Deviations from Plan

1. **[Rule 1 - Bug] EMFILE in GC over many packs** (see above). Changes the error meaning: a read error in group k leaves groups 1..k-1 done (each is a complete GC of its packs). The gc_finish doc comment and the design note say this.
2. **[Rule 1 - Plan assumption] gc liveness pattern.** The plan's test expects `removed_objects=1250` with "i mod 4 not 0 live". That holds only because the test store's packs are small and get compacted. In a full 64 MiB pack, single dead 8-byte records never cover a whole 4 KiB block, so the GC removes nothing (measured: 14,486 of 2,500,000 at 10M, only the small last pack). I added `--dead-run R` (dead runs of R objects); the default R = 1 is the plan's pattern and the test asserts 1250; the measured runs use R = 1024.
3. **[Rule 2] RSS split.** The user asked for anonymous and file-backed RSS separately. Linux: `RssAnon`/`RssFile`. macOS: `task_info(TASK_VM_INFO)` `internal`/`external`, struct copied from the SDK header (`mach/task_info.h`, pack(4), revision 1). Check: `internal + external` = `resident_size` within one page in a test run. The lookup step also prints the split around the timed lookups.
4. **[Rule 1] recover reads the hwm before the drop**, with an untimed read-only open. No drift was seen (at each size, index_runs and memtable entries were the same in all 5 opens, and hwm_pack in all 17 outputs that print it), but reading after a writable open could move the hwm.
5. **[Addition] Run tables** got CPU-based scan columns (Extra CPU s, Scan MB/s (CPU)), index B/object, anon/file RSS and index runs, so the 16-02 method needs no hand computation.
6. **[Addition] A 10M GC with R = 1**, to measure the finding in deviation 2.
7. **[Process] No STATE.md / ROADMAP.md update and no push**, as the dispatcher asked. The commits are on `feature/17-on-disk-index`, not an `agent-*` branch (dispatcher choice). The plan ledger file could not be written (the shell refused the compound command); the base is 8393b57, given by the dispatcher.

## TDD

- Task 1: the four new cli tests were written first; the build then failed (backend signature), and the HEAD binary has no `--memtable-limit` or `gc` (exit 2). All 15 pass after the change.
- EMFILE fix: RED 165 > 104 handles, GREEN after the change; mutant checks as above.

## Test results

- `cargo test --locked -p chunk-store -p index-bench`: exit 0, 186 passed, 0 failed, 13 targets (at d6fef98).
- `cargo test --locked -p slicefs-cli -p metadata`: exit 0, 965 passed, 0 failed, 6 ignored, 37 targets.
- clippy: index-bench clean with `-D warnings --no-deps`; chunk-store has only the three warnings that were there before.
- Doc checks: `### Index results (#85)` 1; macOS rows 24 (>= 9); Linux rows 14 (>= 6); "lock hold" present; `### Index on disk (#85)` 1; `### Index in memory` 0; "marginal" present; no home path; no store file in the repo; "### File manifest" not changed (`git diff origin/develop...HEAD` has no hunk in it; the first hunk starts at the store layout block).

## Surprises

- Lookups got slower: a 100M hit is 3.2 µs against 0.8 µs. A probable cause, not measured: each lookup step is a fresh process, so many hits take a page fault on the mapped run (the file-backed RSS grew by 31.5 B per object at 100M, about 3.2 GB of the 6 GB of runs, and 57 B at 10M). A miss checks 9 filters at 100M. Worth a follow-up (fewer runs, or a check of the fault cost in a long-lived process).
- A normal open decodes the whole `.idx` of the hwm pack to replay only the records above the hwm; at 1M (one pack) this makes the normal open slower than the recovery open, and it is most of the fixed RAM at open on macOS (192 MB at 100M).
- The 100M GC takes 12.4 min of wall time (one verified lookup per `.idx` entry), but holds the lock only 384 ms in total.
- The creator peak RSS (3.9 GB at 100M) includes mapped run pages touched by the dedup checks; `ru_maxrss` cannot split it.

## Known Stubs

None.

## Threat Flags

None. No new network or auth surface. The docs use $D only; no host path.

## Self-Check: PASSED

- FOUND: benchmarks/index-bench/src/main.rs (index_bytes, covered_packs, lock_hold_max_ms), backend.rs (wait_index_idle), README.md (index_bytes), docs/design/chunk-store.md (both new headings).
- FOUND commits: 0eb63bb, c1c20d7, 92421b2 (ancestors of HEAD).
