---
phase: 14-buffered-delta-re-log
plan: 07
subsystem: testing
tags: [docker, libfuse, memory, overlay, dirty-limit, "#98"]
status: complete

requires:
  - phase: 14-03
    provides: "bigwrite.sh and the 'before' number (VmHWM 6224 MiB at 1 GiB)"
  - phase: 14-05
    provides: "splice in rebase_overlay"
  - phase: 14-06
    provides: "relog and replay tests; the extent doubling finding"
provides:
  - "CAP_MIB=256 in bigwrite.sh (one cap for 1 GiB and 4 GiB)"
  - "DIRTY_LIMIT final value 32 MiB with a Measured line"
  - "Overlay extents grow by at most 1/8 (GROW_DIV), not by doubling"
  - "write-log.md part 2 (overlays, BUFR-03 reading); chunk-store.md Splice (#98) with the Docker numbers"
affects: [14-08]

tech-stack:
  added: []
  patterns: ["Vec::reserve_exact to max(need, cap + cap/8) when an extent grows"]

key-files:
  created: []
  modified:
    - crates/slicefs-cli/src/overlay.rs
    - crates/slicefs-cli/src/filesystem.rs
    - crates/slicefs-cli/tests/write_log_replay_tests.rs
    - scripts/docker-linux-test/bigwrite.sh
    - docs/design/write-log.md
    - docs/design/chunk-store.md

key-decisions:
  - "Cap rule: P1 = 117 MiB <= 170 MiB, so CAP_MIB = 256"
  - "DIRTY_LIMIT stays 32 MiB: the slowest write was above 2000 ms, the 16 MiB rerun had a larger slowest write (7778 ms against 7253 ms)"
  - "Extent growth: at most 1/8 of the length as spare capacity, so the overlay cost (capacity) stays near the dirty bytes"

requirements-completed: [BUFR-04]

duration: ~40 min
completed: 2026-11-18

actuals:
  tokens: 3283      # chars/4 over the added diff lines cef0bb0..5fc3d26 (13131 chars)
  tasks: 2          # plus the extra extent-growth fix the launcher asked for
  commits: 3        # git rev-list --count cef0bb0..HEAD before this SUMMARY commit
plan_head_before: cef0bb0faf739ce52ce903a237ee4b555a6b581d
plan_head_after: 5fc3d26df9306401995ea7c2e2dae101742c8bbc
---

# Phase 14 Plan 07: Docker 1 GiB and 4 GiB under one VmHWM cap Summary

On Linux libfuse (OrbStack, release build), a 1 GiB and a 4 GiB random-write run both pass under one cap of **256 MiB**: VmHWM **114 MiB** and **161 MiB**. Before #98 the 1 GiB run had **6224 MiB**. DIRTY_LIMIT stays **32 MiB**. An overlay extent now grows by at most 1/8 of its length, not by doubling. The two design notes describe the shipped design and give the numbers.

## Commits

| Step | Name | Commit | Files |
|------|------|--------|-------|
| Extra | Extent grows by at most 1/8, not by doubling (with a test) | e8d32bd | overlay.rs, filesystem.rs (doc), write_log_replay_tests.rs |
| Task 1 | CAP_MIB from the rule, the dirty-limit rule, Measured line | 2f5180b | bigwrite.sh, filesystem.rs |
| Task 2 | write-log.md part 2 and chunk-store.md splice with the numbers | 5fc3d26 | docs/design/write-log.md, docs/design/chunk-store.md |

## Extra fix: extent capacity (from 14-06)

- `Overlay::put` grew a reused extent with `Vec::resize`, so the capacity doubled (a 65 MiB extent had 128 MiB). The cost counts capacity, so one write could put the overlay near 2 x the dirty limit before the rebase ran.
- Now: when an extent must grow, it reserves exactly `max(need, cap + cap / GROW_DIV)` with `GROW_DIV = 8`. The spare capacity is at most 1/8 of the length. Growth stays geometric, so in-order writes stay amortized O(1). The other paths already allocated exactly.
- RED first: `a_growing_extent_does_not_double_its_capacity` failed on the old code at 3 MiB (`cost 4194368`), then passed. It grows one extent in 1 MiB steps to 65 MiB and in 4 KiB steps to 16 MiB and asserts `cost <= len + len/8 + EXTENT_OVERHEAD` after each write.
- The 14-06 test `an_overlay_with_more_than_64_mib_of_relog_replays` runs again with the plan's 96 MiB limit, not 256 MiB, and passes (no rebase during the 65 MiB extent). Its comment is updated.

## Step 0: tests on the final tree (macOS host, debug build)

`cargo test -p slicefs-cli --lib --test write_log_replay_tests --test fsync_tests --test write_path_tests --test gc_online_tests --test memory_bound_tests` at e8d32bd (DIRTY_LIMIT 32 MiB; the later Task 1 change to filesystem.rs is a doc comment only):

```
lib:                    314 passed; 0 failed (14.02 s)
fsync_tests:              7 passed; 0 failed
gc_online_tests:          9 passed; 0 failed
memory_bound_tests:       1 passed; 0 failed (22.32 s)
write_log_replay_tests:  18 passed; 0 failed
write_path_tests:        24 passed; 0 failed
```

The same set passed again with DIRTY_LIMIT = 16 MiB before the 16 MiB Docker run (same counts).

memory_bound_tests, the four peaks after the extent fix (limit 1 MiB, bound 12,288 KiB):

| Base | File | 14-05 | 14-07 |
|------|------|-------|-------|
| Random | 16 MiB | 8,358 KiB | **8,353 KiB** |
| Random | 48 MiB | 9,272 KiB | **9,272 KiB** |
| Repeating | 16 MiB | 7,284 KiB | **7,279 KiB** |
| Repeating | 48 MiB | 7,779 KiB | **7,779 KiB** |

The change is small here, because the test limit is 1 MiB and its extents are small.

## Task 1: Docker runs

All runs: `SLICEFS_TARGET_VOL=slicefs-target-1407 scripts/docker-linux-test/run.sh ...`. I created that volume and removed it at the end. Load averages are 1, 5 and 15 minutes.

### Measure-only run (CAP_MIB empty), 1024 MiB

Load before 73.32 80.58 82.62, after 64.02 77.19 81.08. Wall 158 s, script run time 103 s.

```
    bigwrite 1024 MiB: VmHWM after cp 30, after random write 82, after cmp 117 MiB (measure only); writes 65792, max write 5418 ms, max fsync 351 ms; overlay switches 1; CAS bytes 2666083776; times cp/rw/cmp 20/59/9 s
RESULT bigwrite: PASS
```

### Cap arithmetic

- P1 = VmHWM after cmp = **117 MiB**.
- 117 <= 170, so **CAP_MIB = 256**. 256 < 1024.
- H1 = VmHWM after cp of the same run = 30 MiB.
- E = DIRTY_LIMIT + REORDER_WINDOW + H1 = 32 + 64 + 30 = **126 MiB**. 2 x E = 252 MiB.
- P1 = 117 <= 252, so **no FLAG**.

### Check run, CAP_MIB=256, DIRTY_LIMIT 32 MiB, 1024 and 4096 (final code)

Load before 54.87 73.71 79.68, after 86.12 86.38 83.14. Wall 439 s, script run time 391 s.

```
  bw-cp-1024: ok
  bw-random-write-1024: ok
  bw-cmp-1024: ok
  bw-rss-bounded-1024: ok
  bw-overlay-used-1024: ok
    bigwrite 1024 MiB: VmHWM after cp 30, after random write 79, after cmp 114 MiB (cap 256 MiB); writes 65792, max write 5870 ms, max fsync 1293 ms; overlay switches 1; CAS bytes 2672373748; times cp/rw/cmp 19/62/8 s
  bw-cmp-after-remount-1024: ok
  bw-cp-4096: ok
  bw-random-write-4096: ok
  bw-cmp-4096: ok
  bw-rss-bounded-4096: ok
  bw-overlay-used-4096: ok
    bigwrite 4096 MiB: VmHWM after cp 82, after random write 161, after cmp 161 MiB (cap 256 MiB); writes 65792, max write 7253 ms, max fsync 191 ms; overlay switches 1; CAS bytes 6168209263; times cp/rw/cmp 85/106/33 s
  bw-cmp-after-remount-4096: ok
    run time: 391 s
RESULT bigwrite: PASS
```

- P4 = 161 MiB. P4 - P1 = 161 - 117 = **44 MiB**. Against the 1 GiB value of the same run (114 MiB), the difference is 47 MiB. The "after cp" values differ by 52 MiB (82 against 30), so the difference comes before the random writes (the copy path and the chunk index, RESEARCH Pitfall 9). This is not checked further.
- CAS bytes: 2,672,373,748 (1 GiB) and 6,168,209,263 (4 GiB). The 1 GiB value is larger than before (1,912,157,485). The probable cause is that each rebase stores new chunks and GC is off in this script. This is not checked.

### Dirty-limit rule

The slowest write was 5870 ms (1 GiB) and 7253 ms (4 GiB), above 2000 ms. So I set DIRTY_LIMIT = 16 MiB, ran the step-0 tests again (all passed) and ran both sizes once.

16 MiB run: load before 73.94 79.54 80.75, after 93.14 86.89 84.70. Wall 518 s, script run time 470 s. `RESULT bigwrite: PASS`, all steps ok.

```
    bigwrite 1024 MiB: VmHWM after cp 31, after random write 97, after cmp 97 MiB (cap 256 MiB); writes 65792, max write 7778 ms, max fsync 515 ms; overlay switches 1; CAS bytes 2755445492; times cp/rw/cmp 37/88/9 s
    bigwrite 4096 MiB: VmHWM after cp 84, after random write 146, after cmp 146 MiB (cap 256 MiB); writes 65792, max write 5726 ms, max fsync 138 ms; overlay switches 1; CAS bytes 6364447131; times cp/rw/cmp 96/106/41 s
```

| DIRTY_LIMIT | Slowest write 1 GiB | Slowest write 4 GiB | Slowest of the run | VmHWM 1 GiB / 4 GiB |
|---|---|---|---|---|
| 32 MiB | 5870 ms | 7253 ms | **7253 ms** | 114 / 161 MiB |
| 16 MiB | 7778 ms | 5726 ms | 7778 ms | 97 / 146 MiB |

Both pass the cap. The slowest write of the run is smaller with 32 MiB, so **DIRTY_LIMIT stays 32 MiB**. Per size the result is mixed (32 MiB is better at 1 GiB, 16 MiB is better at 4 GiB). The load changed from run to run, so the slowest write is noisy. Its cause is not checked: rw.py does not record which write was the slowest. A wait for the exclusive commit gate or a rebase can be the cause.

The final check log is the 32 MiB run. The final code is the code of that run plus the Measured doc line.

### DIRTY_LIMIT doc comment

```
/// Measured (#98, Docker, release build): VmHWM 114 MiB (1 GiB) and 161 MiB (4 GiB), cap 256 MiB;
/// slowest write 5870 ms (1 GiB) and 7253 ms (4 GiB). Both are above 2000 ms,
/// so 16 MiB was also run: 7778 ms and 5726 ms (VmHWM 97 and 146 MiB). The
/// slowest write did not get smaller, so the value stays 32 MiB.
```

The sentence "Start value; plan 14-07 sets the final value" is removed.

### Verify run (Phase 13 checks, final code, debug build)

`run.sh verify`: load before 81.80 84.70 83.99, after 66.69 76.68 80.79. Wall 202 s. Last lines:

```
  no-switch-to-overlay: ok
RESULT verify-phases: PASS
```

### Task 1 verify command

Exit 0 with `TMPDIR` set to the log folder: the memory test "... ok" line 1, `RESULT bigwrite: PASS` 1, `bw-rss-bounded-4096: ok` 1, `bw-rss-bounded-1024: ok` 1, `RESULT verify-phases: PASS` 1, `CAP_MIB=` 1. The `bw-overlay-used` and four cmp lines: 6 of 6 ok.

## Task 2: design notes

- write-log.md: the status line says part 2 is done for the write path by #98. A new section "Overlays (#98, part 2)" covers the base in the manifest (a GC root), the extents and their 1/8 growth, the dirty limit and the rebase triggers, the meaning of `test_last_rebase_bytes()` (bytes cut again), the relog order (Setattr visible, Setattr size, Write pieces of at most 1 MiB, D-05, BUFR-01, #93), no rebase at fsync (D-09), replay, the GC argument and the BUFR-03 reading ("never held exclusively", a shared hold can make a commit or a GC wait on long repeating non-zero data). The lines that said "Buffered: hash the buffer" (Commit changes, the open point and the implementation plan) are fixed. The Tests section lists the new tests. The two follow-up issues are named without numbers.
- chunk-store.md: a subsection "Splice (#98)" under File manifest (restart, resync, whole-node reuse, exact zero shortcut, canonical key, D-08), the small-splice numbers of 14-02 and 14-05, and the Docker table (before, measure, cap 256 MiB at 1 GiB and 4 GiB, and the 16 MiB runs), with the machine and the load. The Write path line now names the overlay.
- Verify: `#98` 6 times in write-log.md, `splice` 7 times in chunk-store.md, `held exclusively` 1, `VmHWM` 2, no licensing word and no home path.

## Deviations from Plan

1. **[Launcher request] Extra fix before the Docker runs: extent growth by 1/8, not by doubling.** Test first (RED at 3 MiB, then GREEN). The 14-06 relog test is back at the plan's 96 MiB limit. Commit e8d32bd.
2. **[Rule: dirty-limit rule] 16 MiB rerun.** The slowest write was above 2000 ms, so the rule made me run 16 MiB. It did not make the slowest write smaller, so the value stays 32 MiB. Both runs are reported above.
3. **[Process] Log folder.** The logs are in a session scratch folder, not `${TMPDIR:-/tmp}`. The verify command was run with `TMPDIR` set to that folder. The 32 MiB check log was renamed during the 16 MiB run and copied back as `14-07-check.log`; it is byte-equal to the original.
4. **[Process] Branch.** The launcher named the branch `feature/14-07-measure`. That name is not in the `agent-*` allow-list of the executor commit protocol, as in 14-03 to 14-06. The detached-HEAD and protected-branch checks passed. STATE.md, ROADMAP.md and REQUIREMENTS.md were not changed. Nothing was pushed.
5. **[Process] Docs drafted early.** The text of both notes without numbers was written while the Docker runs were in progress. It was staged and committed only in the Task 2 commit, after the Task 1 verify passed.

## Open items

- The slowest write is 5 to 8 s in every run on this loaded machine (load 55 to 93). It was 17 s before #98. Which write it is, is not recorded. A by-hand run on a quiet machine, or a per-write log in rw.py, can find it.
- The chunk store retry buffer (deferred-items.md, from 14-04) is still open. It is inside the measured VmHWM.

## Threat Flags

None. T-14-13 is mitigated: the cap came from the fixed rule on the 1 GiB run only, it is below 1 GiB, the 4 GiB run did not change it, and the check mode required an overlay switch per size.

## Known Stubs

None.

## Logs (scratch folder, not committed)

- /private/tmp/claude-501/-Users-alexanderfedin-projects-slicefs/d764d3d1-c5db-45fa-99e9-83e307688877/scratchpad/1407/14-07-tests.log
- /private/tmp/claude-501/-Users-alexanderfedin-projects-slicefs/d764d3d1-c5db-45fa-99e9-83e307688877/scratchpad/1407/14-07-membound.log
- /private/tmp/claude-501/-Users-alexanderfedin-projects-slicefs/d764d3d1-c5db-45fa-99e9-83e307688877/scratchpad/1407/14-07-measure.log
- /private/tmp/claude-501/-Users-alexanderfedin-projects-slicefs/d764d3d1-c5db-45fa-99e9-83e307688877/scratchpad/1407/14-07-check.log (32 MiB, final)
- /private/tmp/claude-501/-Users-alexanderfedin-projects-slicefs/d764d3d1-c5db-45fa-99e9-83e307688877/scratchpad/1407/14-07-check-16.log
- /private/tmp/claude-501/-Users-alexanderfedin-projects-slicefs/d764d3d1-c5db-45fa-99e9-83e307688877/scratchpad/1407/14-07-tests-16.log
- /private/tmp/claude-501/-Users-alexanderfedin-projects-slicefs/d764d3d1-c5db-45fa-99e9-83e307688877/scratchpad/1407/14-07-verify.log

## Self-Check: PASSED

- FOUND: `CAP_MIB=256` in scripts/docker-linux-test/bigwrite.sh (`bash -n` ok)
- FOUND: the `/// Measured (#98, Docker, release build):` line (grep count 1); `pub const DIRTY_LIMIT: usize = 32 << 20;`
- FOUND: `GROW_DIV` and `a_growing_extent_does_not_double_its_capacity` in overlay.rs
- FOUND commits (ancestors of HEAD): e8d32bd, 2f5180b, 5fc3d26; `git rev-list --count cef0bb0..5fc3d26` = 3
- Volume `slicefs-target-1407` removed; no other volume touched
