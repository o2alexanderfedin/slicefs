---
phase: 14-buffered-delta-re-log
plan: 04
subsystem: write-path
tags: [overlay, dirty-limit, rebase, write-log, memory-bound, "#98"]
status: complete

requires:
  - phase: 14-01
    provides: "Overlay, install_root, to_overlay, rebase_overlay"
provides:
  - "DIRTY_LIMIT and the limit rebase in the write callback"
  - "Test hooks: test_set_dirty_limit, test_rebase_count, test_last_rebase_bytes, test_last_relog_bytes, test_set_rebase_hook"
  - "commit_cut relog from descriptors, in pieces of at most RELOG_PIECE after the commit"
  - "Streaming setattr (non-zero, not a pure extension) through to_overlay; the whole-file read helper is deleted"
  - "memory_bound_tests.rs: counting-allocator proof over all 11 inventory sites"
affects: [14-05, 14-06, 14-07, 14-08]

actuals:
  tokens: 7335      # chars/4 over the added diff lines ab6bdb3..669a9a3 (29341 chars)
  tasks: 3
  commits: 4        # task commits: git rev-list --count ab6bdb3..1545be2 = 5, minus the first SUMMARY commit 86fc035
plan_head_before: ab6bdb371ada7edceff43b9aef24046e375517f5
plan_head_after: 1545be29001ecf081f4e874abdbd32dfb83f14ff  # last task commit

tech-stack:
  added: []
  patterns:
    - "Limit check after the write match in test_write: one place covers the switch and the overlay arm"
    - "commit_cut: descriptors before meta.commit(); copy_out of one piece under open_files, unlock, log, drop"
    - "Counting #[global_allocator] in its own test binary with one #[test]"

key-files:
  created:
    - crates/slicefs-cli/tests/memory_bound_tests.rs
    - .planning/phases/14-buffered-delta-re-log/deferred-items.md
  modified:
    - crates/slicefs-cli/src/filesystem.rs
    - scripts/docker-linux-test/verify-phases.sh

key-decisions:
  - "The rebase hook runs after the new root is finished and before install_root, with no lock held (the Arc is cloned out of the Mutex)"
  - "On a rebase error in test_write the state goes back into open_files and the write returns EIO"
  - "commit_cut returns an error if an extent changed between the descriptor and the copy (not possible under the exclusive gate)"
  - "The memory test syncs the store after setup, so the commit in the workload cannot hide a whole-file buffer below the baseline"

requirements-completed: [BUFR-01, BUFR-03]  # BUFR-04 partly: the 11 sites are bounded, but the chunk store retry buffer (Deferred Issues) holds a streamed file below 64 MiB until the next sync; the Docker part is 14-07

duration: ~75 min
completed: 2026-11-18
---

# Phase 14 Plan 04: Dirty limit, relog pieces, last whole-file read Summary

An overlay that passes the dirty limit is stored as a new root in the same write callback. A commit logs only descriptors before `meta.commit()`, then logs 1 MiB pieces one at a time. A Streaming setattr that is not a pure extension switches to an overlay, so no daemon write path reads a whole file. A counting-allocator test shows a peak of 10-12 MiB at every inventory site for both a 16 MiB and a 48 MiB file. A held whole-file read makes all four cases fail.

## Final hook signatures (filesystem.rs)

```rust
pub const DIRTY_LIMIT: usize = 32 << 20;          // start value (D-10); 14-07 sets the final one
pub type RebaseHook = Arc<dyn Fn() + Send + Sync>;
const RELOG_PIECE: u64 = 1 << 20;                  // from 14-01, now with one-at-a-time pieces
impl SliceFsFilesystem {
    pub fn test_set_dirty_limit(&self, bytes: usize);
    pub fn test_rebase_count(&self) -> u64;
    pub fn test_last_rebase_bytes(&self) -> u64;   // whole size for the re-stream
    pub fn test_last_relog_bytes(&self) -> u64;    // Write payload bytes of the last commit_cut (overlay pieces + Streaming pending); 0 if none
    pub fn test_set_rebase_hook(&self, hook: Option<Arc<dyn Fn() + Send + Sync>>);
}
```

The position is tested: `a_switch_that_moves_pending_writes_over_the_limit_rebases_at_once` sets a hook that records the manifest root. The hook runs once, and the root it sees is the switch base, not the rebased root (commit 1545be2).

`rebase_overlay` returns at once for a clean overlay. Otherwise it streams the file, finishes the root, then adds 1 to `rebase_count`, stores `last_rebase_bytes`, and calls the hook. After that it runs `install_root`. Plan 14-05 can run online GC mark and sweep inside the hook.

## Tasks

| Task | Name | Commit |
|------|------|--------|
| 1 | Dirty limit, counters and hook, relog in 1 MiB pieces after the commit (tracer) | 1a7e9f2 |
| 2 | Streaming setattr through to_overlay, helper deleted, switch text + Phase 13 check | cfe369a |
| 3 | Counting-allocator proof (memory_bound_tests.rs) and the crate run | 669a9a3 |
| - | Hook position test (after the advisor review) | 1545be2 |

Tracer gate: the Task 1 verify passed before Task 2 started. The 3 named tests are ok. write_log_replay is 15 ok and fsync is 6 ok. coverage_boost (107), streaming (25), write_path (20), posix_compliance (40) and gc_online (6) also passed.

## Memory test: the four peaks (memory_bound_tests, macOS, debug build)

Limit 1 MiB. Baseline = heap after setup, after a store sync, with the handle open O_RDWR.

| Base | File | Peak above baseline | Case time |
|------|------|---------------------|-----------|
| Random | 16 MiB | 11296 KiB | 3.8 s |
| Random | 48 MiB | 12220 KiB | 8.6 s |
| Repeating | 16 MiB | 10216 KiB | 2.8 s |
| Repeating | 48 MiB | 10712 KiB | 6.1 s |

- Linux (Docker, `slicefs-fusetest` image, debug build, private target volume, no mount): the same four numbers, 11296 / 12220 / 10216 / 10712 KiB, test 18.8 s. The allocator counts requested sizes, so glibc and macOS malloc give the same result. CI runs `cargo test --workspace` on both (ci.yml: Linux in Docker, macOS native).
- Bound 12288 KiB: all four pass. The difference per base kind is 924 KiB (Random) and 496 KiB (Repeating), against a limit of 2048 KiB. The test takes about 21 s.
- **The margin is small: 68 KiB for Random 48 MiB.** Two runs gave the same number, because the seeds are fixed and the test runs on one thread. A diagnostic run that reset the peak at each step found where the peak is. It is the second 1 MiB write (write 200), which runs a limit rebase. About half of that peak is store bytes that two rebases retained with no sync between them (see Deferred Issues). The other part is the rebase work space for a 1 MiB write: the request copy, the merged extent, one 1 MiB piece through view, base read and output, and the FileWriter buffer. All of these have a fixed size.
- **Negative check:** for one run (not committed), I held `fs.read_content(manifest root)` from the start of step 2 to the end of the window. All four cases failed: 20830, 53382, 20939 and 53942 KiB. The test sees a whole-file buffer at the sites after the commit.
- Sites per step: (1) switch, 256 random 4 KiB writes, two 1 MiB writes, limit rebases (sites 1-3), fsync (7), `commit_cut` with dirty extents (8, `test_last_relog_bytes() > 0`), `gc_start` + `gc_abort` (11), 16 reads of 64 KiB, checked in the window (5), release (6). Step 1 needs at least 3 rebases. The rebase count stays the same across the fsync and across the commit. (2) Write at size + `256 << 30` (4), setattr back, setattr to `512 << 30` (9), setattr back, release. The rebase count stays the same at every step. (3) Shrink through a streaming handle to size - 1 MiB (10), then `!test_is_streaming`, then exactly one rebase at release.
- Content checks after the window: 16 seeded 64 KiB ranges, the ranges of the two 1 MiB writes, the last MiB, and the size.

## Grep gates (region before `#[cfg(test)]` at line 2363, whole comment lines skipped)

| Gate | Result |
|------|--------|
| `Buffered\|handle_content\|pwrite_into\|buf.resize(\|content.resize(\|buf.clone()` | 0 |
| `Buffered` | 0 (was 13 at 2cf7170) |
| `handle_content` | 0 (was 3) |
| `buf.resize(\|content.resize(` | 0 (was 4) |
| `content::put_all(` | the symlink target line only |
| `content::get_all(` | `read_content` (tools/tests) and readlink only |
| `switches to overlay` in filesystem.rs / verify-phases.sh | 1 / 1 |
| `switches to Buffered` in verify-phases.sh | 0 |
| `no-switch-to-overlay` / `no-switch-to-buffered` in verify-phases.sh | 1 / 0 |
| 5 hook signatures | 5 |
| `pub const DIRTY_LIMIT: usize = 32 << 20;` | 1 |
| `RELOG_PIECE` | 4 |
| `#[global_allocator]` in memory_bound_tests.rs | 1 |
| `^#[test]` lines in memory_bound_tests.rs | 1 (line 268) |
| `256 << 30\|512 << 30` | 2 |

Test diff against ab6bdb3: `crates/slicefs-cli/tests` changed only by the new `memory_bound_tests.rs`. No existing test changed.

## Crate run (`cargo test -p slicefs-cli`, exit 0, 3 min 55 s wall)

25 result lines, no FAILED. The 13th line is memory_bound_tests (1 passed, 21.46 s). This run was at 669a9a3. After the hook test commit 1545be2, only the lib tests ran again (313 passed).

```
test result: ok. 313 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 14.08s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 107 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.26s
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.96s
test result: ok. 25 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.04s
test result: ok. 78 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.66s
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.41s
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 10.29s
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.82s
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.40s
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.20s
test result: ok. 55 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.09s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 21.46s
test result: ok. 41 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.11s
test result: ok. 40 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.06s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.30s
test result: ok. 0 passed; 0 failed; 4 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
test result: ok. 25 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.26s
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.07s
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 15 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.08s
test result: ok. 20 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

## Deviations from Plan

**1. [Rule 2 - Missing proof] In-order rewrite pass in `an_overlay_rebases_when_it_crosses_the_dirty_limit`.** The 14-01 gap was an in-order rewrite that grows one extent to the file size. The test now rewrites the whole 1 MiB file in order in 4 KiB writes. The overlay cost must stay at or below 2 x limit + one write, and the pass must run at least 8 rebases. Commit 1a7e9f2.

**2. [Rule 2 - Missing proof] The relog test reads the log.** `a_commit_relogs_an_overlay_in_pieces_of_at_most_1_mib` decodes the log segments after the commit. It checks that every `Write` is at most `RELOG_PIECE`, that the bytes match, and that the pieces cover 3 MiB. A byte count alone does not show the split. Commit 1a7e9f2.

**3. [Rule 1 - Bug] Double refcount decrement (14-01 open item) is gone.** The Streaming setattr arm no longer takes `last_committed_root` and decrements it. `install_root` (inside `to_overlay`) owns the count. The Task 2 test asserts this: the root stays at 1 across the switch, and the replaced root is at 0 after release. Commit cfe369a.

**4. [Rule 2] `gc_start` + `gc_abort` in the memory test for site 11.** The plan's workload did not reach site 11. The test does not run a sweep, because pack compaction memory is a GC matter outside the 11 sites. Commit 669a9a3.

**5. [Rule 2] The store is synced after setup in the memory test (`fs.meta().commit()`).** Without the sync, the baseline held the base bytes that the chunk store retained (Random 16 MiB: baseline 17000 KiB; 48 MiB: 51137 KiB). The commit in the workload freed them. After that, sites 4, 5, 6, 9 and 10 ran 16 MiB or 48 MiB below the baseline, so a whole-file buffer there would have passed. A comment in the test explains this. The plan's bound is not changed. Commit 669a9a3.

**6. [Rule 2] Extra asserts in the memory test:** at least 3 rebases in step 1, relog bytes > 0 at the commit, no rebase across the fsync and the commit, and `!test_is_streaming` after the site 10 shrink. Without them, the bound could pass even if a site never ran.

**7. [Process] TDD.** Tasks 1 and 2 have no separate RED commit. I wrote the tests and the code in one edit. The Task 1 tests use hooks that did not exist, so they could not compile before the change. The Task 2 test asserts `!test_is_streaming` after a non-zero shrink. The old code kept that handle streaming, so the test would fail there. For Task 3, the RED evidence is the negative check above.

**8. [Process] Branch.** Commits are on `feature/14-04-limit`, as the launcher asked. That name is not in the stock `agent-*` allow-list, so I skipped that one check. The detached-HEAD check and the protected-branch check passed. STATE.md, ROADMAP.md and REQUIREMENTS.md were not changed. Nothing was pushed.

**9. [Minor] `RELOG_PIECE` stays `u64`** (it was so in 14-01). The plan text shows `usize`, but the gate only counts the name.

## Deferred Issues

- **BLOCKER FOR A DECISION (hard rule):** the 11 inventory sites are bounded and proven. But the rule "the daemon never holds a whole file in RAM" is not met as written for a file below 64 MiB, because of the chunk store retry buffer below.

- **The chunk store retains unsynced pack bytes in RAM, up to one pack (64 MiB).** `RetryingPack::write_at` (chunk-store/src/retry.rs) copies each write into `unsynced` until a sync: a commit, or the seal of the pack at `pack_limit` = 64 MiB. `ChunkStore::open` wraps every pack in this layer (lib.rs:452). A streamed file below 64 MiB stays fully in RAM until the next commit, and each rebase adds its new chunk bytes. The buffer is not one of the 11 sites. It existed before phase 14 and is outside this plan's files. It does not grow with the file size, but it touches the hard rule for files below 64 MiB, and it sets a floor for the VmHWM cap of 14-07. The orchestrator decides. The details are in `deferred-items.md`.
- From 14-01, still open: no test reaches the relog `Setattr(visible)` record (a shrink below the base, open at a commit) through a replay. The 1 MiB split is now tested (deviation 2). The replay matrix of 14-06 owns this.
- While a limit rebase runs, the state is out of `open_files`. The rebase takes longer for a larger file. During it, a read through another handle would see the base without the extents, and a write through an aliased handle would get EBADF. RESEARCH Q6 accepts this: there is one FUSE thread (`n_threads` defaults to 1), so no other callback runs at the same time. Plans 14-05 and 14-06 must keep it in mind if they add threads.
- With the production limit of 32 MiB, one merged extent that grows by Vec doubling can briefly hold its old and new buffer during a realloc (about 1.5 x the limit). `cost()` counts capacity, so the rebase still runs at the limit. This is a note for the 14-07 cap and needs no change now.

## Threat Flags

None. No new endpoint, auth path or file access. T-14-01 (no allocation by offset or size, the limit rebase) and T-14-17 (gates on the non-comment region before `#[cfg(test)]`, one test function, a bound below the smaller file, content checks, and the negative check) are mitigated as planned.

## Known Stubs

None.

## Self-Check: PASSED

- FOUND: crates/slicefs-cli/tests/memory_bound_tests.rs (`#[global_allocator]`, one `#[test]`)
- FOUND: `pub const DIRTY_LIMIT`, the 5 test hooks and `RELOG_PIECE` in filesystem.rs; `switches to overlay` in filesystem.rs and verify-phases.sh (`bash -n` ok)
- FOUND commits (ancestors of HEAD): 1a7e9f2, cfe369a, 669a9a3, 1545be2
