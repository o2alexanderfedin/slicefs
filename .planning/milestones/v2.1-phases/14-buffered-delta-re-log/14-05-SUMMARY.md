---
phase: 14-buffered-delta-re-log
plan: 05
subsystem: write-path
status: complete
tags: [splice, rebase, overlay, online-gc, zero-shortcut, "#98"]

requires:
  - phase: 14-02
    provides: "chunk_store::file::{splice, SpliceSource, SpliceStats}"
  - phase: 14-04
    provides: "dirty limit, rebase hooks and counters, memory_bound_tests"
provides:
  - "content::splice (ObjectKey / io::Error wrapper of chunk_store::file::splice)"
  - "OverlaySource: the new content of a splice, read through Overlay::view"
  - "rebase_overlay is a splice (O(change + resync + depth), not O(file))"
  - "Case B of test_setattr_size (no open handle) is a splice with the zero shortcut"
  - "test_last_rebase_bytes() = SpliceStats::rechunked of the last splice"
affects: [14-06, 14-07, 14-08]

tech-stack:
  added: []
  patterns:
    - "One SpliceSource over an Overlay serves both the rebase and the no-handle size change"

key-files:
  created: []
  modified:
    - crates/slicefs-cli/src/content.rs
    - crates/slicefs-cli/src/filesystem.rs
    - crates/slicefs-cli/tests/write_path_tests.rs
    - crates/slicefs-cli/tests/gc_online_tests.rs

key-decisions:
  - "test_last_rebase_bytes now reports the bytes FastCDC cut again (SpliceStats::rechunked), not the file size; Case B stores it too, but does not add to rebase_count and does not call the rebase hook"
  - "Case B builds a temporary Overlay(old root, L, L).truncate(new_size) and splices with keep = visible and no changed ranges, so the old content, a cut and an extend all go through one source type"
  - "REBASE_PIECE is deleted (no user is left)"

requirements-completed: [BUFR-03, BUFR-05]  # BUFR-04: memory bound still green (peaks below); the Docker part is 14-07

estimate:
  tokens: 100000
  tasks: 3
actuals:
  tokens: 4800      # chars/4 over the added diff lines 0285b2a..fd62c97 (19217 chars)
  tasks: 3
  commits: 4        # git rev-list --count 0285b2a..a49498b (3 task commits + 1 clippy fix; before this SUMMARY commit)
plan_head_before: 0285b2a
plan_head_after: a49498b1abcc4dd3992e7ecb86d6dcc382beeda1

duration: ~70 min
completed: 2026-11-18
---

# Phase 14 Plan 05: Splice wiring Summary

`rebase_overlay` now calls `content::splice` over an `OverlaySource` instead of streaming the whole file again. A size change with no open handle uses the same splice. After a 4 KiB write in a 32 MiB file, the rebase cuts 38 KB again and adds 5 objects. A 1 MiB to 4 GiB extend cuts 36 KB again in 0.14 s; before this plan it took 39 s. Online GC that runs inside a rebase loses no chunk.

## Reading of BUFR-03 (roadmap success criterion 4)

The commit gate is never held **exclusively** for the hash of file content: `commit_cut` and `gc_start` never rebase and never hash file content. A rebase runs inside a FUSE callback (write at the dirty limit, setattr, release, destroy), and that callback holds the gate **shared**, as every callback does. On random data, the splice cuts again only near the change, so the shared hold is short (the numbers below). On long repeating non-zero data, canonical chunking (D-08) can make the splice cut again up to the end of the file. Then a timed commit or a `gc_start` waits until that callback ends. Memory stays fixed in that case (memory_bound_tests, repeating-base cases). Plans 14-07 (write-log.md) and 14-08 (#98 comment) repeat this reading.

## What changed

- `content.rs`: `pub fn splice(cs, base: Option<&ObjectKey>, keep, changed, new_len, src) -> io::Result<(ObjectKey, SpliceStats)>`. It is a thin wrapper that uses `to_id`, `to_key` and `io::Error::other`.
- `filesystem.rs`:
  - `OverlaySource<'a> { cs, ov: &Overlay, reader: Option<content::Reader> }` implements `SpliceSource`. `read(off, len)` is `ov.view(off, len).read(base reader)`, and an I/O error becomes `chunk_store::Error::Io`.
  - `rebase_overlay`: a clean overlay returns at once, as before. Size 0 installs `None`, as before. Otherwise it runs `content::splice(cs, ov.base, ov.visible, &ov.extent_ranges(), ov.size, &mut src)`. Then the steps run in the same order as before: `rebase_count += 1`, `last_rebase_bytes = stats.rechunked`, the hook, `install_root`, and a clean overlay. The triggers, the limit and the hook point did not change.
  - Case B of `test_setattr_size`: `Overlay::new(Some(R), L, L).truncate(new_size)`, then `content::splice(cs, Some(&R), visible, &[], new_size, ..)`. The refcount steps (decrement old, set manifest, increment new) and the inode update did not change. `new_size == 0` keeps the empty-manifest path.
  - The two lib tests that asserted the whole size in `test_last_rebase_bytes` now assert bounds. The in-order rewrite must be `> 0` and `<= 2 x limit + 4 KiB + 8 x CHUNK_MAX`. The switch with pending writes must be from 128 KiB up to below 256 KiB, because the zero gap is not hashed.

## Test numbers (macOS, debug build)

| Test | Numbers | Time |
|------|---------|------|
| `a_rebase_rechunks_only_near_the_change` | 4 KiB write at 16 MiB + 1234 in 32 MiB random: **38,355 bytes** re-chunked (bound 256 KiB), `rebase_count` + 1 | 3.2 s for both (in parallel) |
| `a_small_change_adds_only_the_changed_chunks` | **5** objects new in N vs O (bound 12); CAS raw bytes **+124,310** (bound 1 MiB); every canonical chunk of O outside [16 MiB - 64 KiB, 16 MiB + 64 KiB) is in N (more than 3,000 checked; each derived chunk ID was first checked to be in `file_objects(O)`) | (see above) |
| `an_extend_without_a_handle_rechunks_little` | 1 MiB -> 4 GiB with no handle: **35,628 bytes** re-chunked; the shrink back to 1 MiB: **2,860 bytes**, the same key as before | **0.14 s** (old code: 39 s, RED run) |
| `a_sparse_extend_on_an_open_overlay_rebases_without_hashing_the_zeros` | write 4 KiB at 0, setattr to 4 GiB through the handle, write 4 KiB at 4 GiB - 4 KiB, release: **81,441 bytes** re-chunked, one rebase | **0.14 s** |
| `online_gc_during_a_rebase_keeps_every_chunk` | 32 MiB stream, then a write back (the switch installs the ended stream); mark and sweep in the hook; GC freed all 300,000 garbage bytes; all bytes read back, then again after remount + offline GC | part of 9 s file |
| `gc_start_waits_for_a_rebase_in_a_callback` | after 300 ms `gc_start` had not returned while the callback held the shared gate inside the hook; after "go" it returned; `gc_content` freed the garbage; remount check ok | part of 9 s file |
| `an_open_overlay_keeps_its_base_through_online_gc` | full `gc_online` with an open overlay, then a write and an append, release, remount check ok | part of 9 s file |

The RED evidence for Task 2: before the Case B change, `an_extend_without_a_handle_rechunks_little` failed with "the extend re-chunked 0 bytes" and ran for 39 s. The sparse-extend test already passed after Task 1, because it goes through the overlay rebase.

## memory_bound_tests: the four peaks with the splice

| Base | File | 0285b2a (re-stream) | With the splice |
|------|------|--------------------:|----------------:|
| Random | 16 MiB | 11,296 KiB | **8,358 KiB** |
| Random | 48 MiB | 12,220 KiB | **9,272 KiB** |
| Repeating | 16 MiB | 10,216 KiB | **7,284 KiB** |
| Repeating | 48 MiB | 10,712 KiB | **7,779 KiB** |

Bound 12,288 KiB: all four pass. The margin for Random 48 MiB grew from 68 KiB to about 3 MiB. memory_bound_tests.rs did not change. The repeating-base cases now run the splice path that runs to EOF.

## Discrimination check (by hand, not committed)

Mutant: the overlay arm of the `gc_start` file roots became `{}`, and `to_overlay` called `stamp_content` instead of `install_root`, so the old manifest stayed. The result against the final test file:

- `online_gc_during_a_rebase_keeps_every_chunk` **FAILED**: `test_read` returned EIO (errno 5) before the remount. The splice reused, by ID, base chunks that the sweep had deleted.
- `an_open_overlay_keeps_its_base_through_online_gc` **FAILED**: `test_release` returned EIO. The rebase read a chunk that had been swept.
- `gc_start_waits_for_a_rebase_in_a_callback` passed, as expected, because this mutant does not touch the gate.

The mutant was reverted with `git checkout -- crates/slicefs-cli/src/filesystem.rs`. `grep MUTANT` gives 0, and the GC tests pass again.

## Grep gates

| Gate | Result |
|------|--------|
| `pub fn splice` in content.rs | 1 |
| `content::splice` in filesystem.rs | 2 (rebase, Case B) |
| `impl.*SpliceSource for OverlaySource` | 1 |
| 14-04 gate `Buffered\|handle_content\|pwrite_into\|buf.resize(\|content.resize(\|buf.clone()` (non-comment lines before `#[cfg(test)]`) | 0 |
| `content::put_all(` / `content::get_all(` before `#[cfg(test)]` | symlink target / `read_content` and readlink only (same as 14-04) |
| `fn online_gc_during_a_rebase_keeps_every_chunk`, `fn gc_start_waits_for_a_rebase_in_a_callback` | 1 / 1 |
| `test_set_rebase_hook` / `commit_gate()` in gc_online_tests.rs | 4 / 2 |

## Tasks

| Task | Name | Commit | Files |
|------|------|--------|-------|
| 1 (tracer) | The splice as the rebase body, re-chunk bound and store-growth tests | 4bc338a | content.rs, filesystem.rs, write_path_tests.rs |
| 2 | A size change with no open handle through the splice (zero shortcut) | 92c2d0c | filesystem.rs, write_path_tests.rs |
| 3 | Online GC during a rebase (D-06) | fd62c97 | gc_online_tests.rs |
| - | Clippy: range contains in a lib-test assert | a49498b | filesystem.rs |

Tracer gate: the Task 1 verify passed on exactly the committed bytes before Task 2 started. The results were memory_bound 1, write_log_replay 15, fsync 6, streaming 25, write_path 22 and lib 313.

## Crate run (`cargo test -p slicefs-cli -p chunk-store`)

The run was at fd62c97 and exited with 0. It gave 34 result lines and none failed. The chunk-store lines are 17, 10, 4, 5, 10, 9, 6 and 11. The slicefs-cli lines include lib 313, gc_online 9, gc_sweep 2, memory_bound 1 (16.4 s), write_log_replay 15, fsync 6, streaming 25, write_path 24, posix_compliance 40 and e2e 78. The memory peaks in this run are the same four numbers as in the table above.

After the run, `cargo clippy -p slicefs-cli --tests` found one new lint on a line from this plan: a manual range check in a lib-test assert. It was fixed in a separate commit, and that lib test was run again (ok). The other clippy warnings in filesystem.rs come from before this plan.

## Deviations from Plan

**1. [Rule 1 - Test fix] Two lib tests asserted the old meaning of `test_last_rebase_bytes`.** `an_overlay_rebases_when_it_crosses_the_dirty_limit` asserted `== 1 MiB`, and `a_switch_that_moves_pending_writes_over_the_limit_rebases_at_once` asserted `== 1 MiB + 128 KiB`. Both values were the whole size of the re-stream. Both tests now assert the bounds listed under "What changed". The bounds are tighter than the old values, so they also check the splice. The doc comment of the hook was updated. Commit 4bc338a.

**2. [Rule 1 - Flaky assert] The GC tests assert on freed bytes, not on the object count.** At first the new tests used `removed_objects > 30`, as the older tests do. In some runs the sweep removed 26 objects, with `removed_bytes` exactly 300,000; in other runs it removed more than 30. I did not find the cause of the variation (see Open items). The three new tests now assert `removed_bytes >= 300_000`, which means every byte of the incompressible garbage file is freed. Five runs in a row passed, and the discrimination check was run again against this version. Commit fd62c97.

**3. [Rule 2] Extra checks in the Task 1 and Task 2 tests.** The chunk-offset mapping proves itself: each canonical chunk ID of the old content must be in `file_objects(O)` before the "kept" check uses it. The 4 KiB change is the bitwise NOT of the old bytes, so it always differs from them. Task 2 asserts `rechunked > 0`, so a value of 0 that was never stored cannot pass. That assert was the RED failure. The 4 GiB file is read only in ranges with `test_read`, never as a whole.

**4. [Process] TDD shape.** Task 2 (`tdd="true"`) has one commit. RED was seen first: before the Case B change, the extend test failed with 0 bytes after 39 s. Task 3 holds tests only. Its RED evidence is the mutant run.

**5. [Process] Branch and state.** The commits are on `feature/14-05-splice-wiring` from 0285b2a, as the launcher asked. This name is not in the `agent-*` allow-list, so that one check was skipped. The detached-HEAD check and the protected-branch check pass. STATE.md, ROADMAP.md and REQUIREMENTS.md were not changed, on orchestrator instruction. Nothing was pushed. The 14-06 files (`write_log_replay_tests.rs`, `fsync_tests.rs`) were not changed. They were only run as guards.

**6. [Process] Baseline before the change.** The 4 memory peaks at 0285b2a, before any edit, were measured by running the original source files. They are the same as the 14-04 numbers. So the drop in the table is caused by the splice and not by the KEEP_LIMIT fix.

## Known Stubs

None.

## Threat Flags

None. There is no new endpoint, auth path or file access. T-14-09 is mitigated and tested by the three GC tests and the recorded mutant. T-14-10 is mitigated: a 4 GiB extend cuts again 35,628 bytes with no handle and 81,441 bytes through an open overlay. Both are below the 1 MiB bound.

## Open items for later plans

- The rebase still runs with the state out of `open_files`, as in 14-04 (one FUSE thread, RESEARCH Q6). It is now short on random data. On repeating non-zero data it can still run to EOF (see the BUFR-03 reading).
- Not investigated: the online GC object count changes from run to run (26 or more than 30) for the same 300,000-byte garbage file in `online_gc_during_a_rebase_keeps_every_chunk`. The freed bytes are the same each time.
- Case B opens the old root twice: once to get its length and once in `OverlaySource`. That is one extra root-node load per no-handle size change, and it does not depend on the file size.

## Self-Check: PASSED

- FOUND: content::splice, OverlaySource (SpliceSource impl), 4 write_path tests, 3 gc_online tests
- FOUND commits (in 0285b2a..HEAD): 4bc338a, 92c2d0c, fd62c97, a49498b
- The working tree is clean apart from this SUMMARY. No MUTANT line is left in filesystem.rs.
