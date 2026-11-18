---
phase: 14-buffered-delta-re-log
plan: 02
subsystem: chunk-store
status: complete
tags: [splice, fastcdc, manifest-tree, canonical-key, zero-run]
requires: []
provides:
  - "chunk_store::file::splice (re-chunk near the changed bytes only; key equals a full write)"
  - "chunk_store::file::SpliceSource, chunk_store::file::SpliceStats"
affects: [14-05, 14-07, 14-08]
tech-stack:
  added: []
  patterns:
    - "forward leaf cursor with one decoded node per level (FileReader tree checks)"
    - "splice writes through FileWriter::push_entry, so the tree cut rule has one owner"
key-files:
  created: []
  modified:
    - crates/chunk-store/src/file.rs
decisions:
  - "splice restarts at the largest old cut strictly below the first changed byte, and resyncs only at a usable old cut (below keep, or the old end when the size and the tail stay)"
  - "A precondition error of splice is Error::Io with ErrorKind::InvalidInput (lib.rs has no InvalidInput variant and is out of scope); a bad tree is Error::Corrupt"
  - "The zero shortcut puts the zero chunk once per splice (one 32 KiB hash) so that the tree never names an object that the store does not hold"
metrics:
  duration: "about 45 min"
  completed: 2026-11-18
estimate:
  tokens: 95000
  tasks: 2
actuals:
  tokens: 7700   # chars/4 over the realized diff of file.rs (30,904 chars)
  tasks: 2
  commits: 2     # code commits; this SUMMARY is one more docs commit
plan_head_before: 5fce24319860899851a66716622e7e41bc40e817
plan_head_after: 1a2e70d
---

# Phase 14 Plan 02: Chunk-list splice Summary

A streaming `splice` in `chunk-store/src/file.rs` cuts again only the chunks near the changed bytes, copies the other old chunks and whole old nodes by ID, and pushes known zeros without a read or a hash. Oracle tests prove that the key is always the key of a full `FileWriter` write of the same bytes.

## API (for plan 14-05)

```rust
pub trait SpliceSource {
    fn read(&mut self, offset: u64, len: usize) -> Result<Vec<u8>>;
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpliceStats {
    pub rechunked: u64,      // bytes of the chunks that FastCDC cut again
    pub reused_entries: u64, // old level-0 entries copied one by one
    pub reused_nodes: u64,   // whole old nodes copied as one entry
    pub zero_runs: u64,      // zero chunks pushed without a read or a hash
}

pub fn splice(
    store: &ChunkStore,
    base: Option<ObjectId>,
    keep: u64,
    changed: &[Range<u64>],
    new_len: u64,
    src: &mut dyn SpliceSource,
) -> Result<(ObjectId, SpliceStats)>
```

- New byte `x < new_len`: the `src` byte if `x` is in a changed range, else the base byte if `x < keep`, else zero. The splice reads `src` only inside `[0, new_len)` and only in the re-chunked ranges.
- `src.read` is called with pieces of `CHUNK_MAX` to `2 x CHUNK_MAX` bytes. A short read gives `Error::Io(UnexpectedEof)`.
- Errors: a broken precondition (`keep` past the base length or `new_len`, a changed range that is unsorted, overlaps, or is past `new_len`) gives `Error::Io(InvalidInput)`. A bad base tree gives `Error::Corrupt`. There is no panic. Empty changed ranges are skipped.
- Memory: the chunk buffer holds at most `2 x CHUNK_MAX` bytes after a read. One piece of at most `2 x CHUNK_MAX` is in memory at a time. The cursor and the writer hold one decoded node per tree level. The only list is the input list of changed ranges.

## How it works

1. Regions are the changed ranges, plus a tail `[keep, new_len)` when `new_len != base_len` or `keep < base_len`. A changed range that starts at or after `keep` merges into the tail.
2. Copy mode: old entries that end strictly before the next region start are copied. If the cursor is at the start of an old node, the writer has no pending entries at that level or below, and the node is complete and ends before the region, the node entry is pushed one level up. The highest such node is used first.
3. Re-chunk mode starts at the cursor (the largest old cut strictly below the region start). After each new chunk end `e`: every region that starts at or before `e` merges into the current one. The splice resyncs if `e` is at or past the merged end, `e` is an old cut, and `e < keep`, or `e` is the old end with no tail.
4. Zero shortcut: at a chunk start `s` with `s + CHUNK_MAX <= new_len`, the splice pushes the zero chunk if `[s, s + CHUNK_MAX)` is outside every changed range and is in `[keep, new_len)` or in old zero chunks. The old zero chunks can start before `s` (off the 32 KiB grid).

## Tasks

| Task | Name | Commit | Files |
|------|------|--------|-------|
| 1 | Leaf cursor, entry push, canonical splice, full-write oracle | c035209 | crates/chunk-store/src/file.rs |
| 2 | Whole-node reuse, zero-run shortcut, cost and safety tests | 1a2e70d | crates/chunk-store/src/file.rs |

## Test results

- `cargo test -p chunk-store`: all pass (lib 16, and 10, 4, 5, 10, 9, 5, 11 in the integration test binaries).
- `file::tests`: 9 tests pass (3 old, 6 new): `a_splice_gives_the_same_key_as_a_full_write`, `a_splice_across_many_leaves_gives_the_same_key`, `zero_runs_cut_at_chunk_max`, `a_splice_over_zeros_and_repeating_data_gives_the_same_key`, `a_small_splice_reuses_old_nodes`, `a_splice_of_a_malformed_tree_is_an_error_not_a_panic`.
- Run time of `file::tests` in a debug build: 6.1-8.1 s for the test binary (about 16-21 s wall time with the incremental build), on x86_64.
- `cargo clippy -p chunk-store --tests`: no new warnings. The 2 warnings that stay (`lib.rs:286`, `tests/file_tests.rs:69`) were there before this plan.

### Small-splice numbers (`a_small_splice_reuses_old_nodes`)

32 MiB random base, one 4 KiB overwrite at the middle:

| rechunked | reused entries | reused nodes | new objects |
|-----------|----------------|--------------|-------------|
| 35,343 B | 1,602 | 3 | 5 |

Limits in the test: rechunked <= 256 KiB, reused nodes >= 1, new objects <= 12. Before Task 2 (without node reuse) the same splice copied 3,043 entries and reused 0 nodes.

### Mutation checks (by hand, not committed)

| Mutant | Result |
|--------|--------|
| Restart at the largest old cut `<=` the change start (Pitfall 1) | caught by both oracle tests |
| Usable cut `e <= keep` (the old end treated as usable on a shrink, Pitfall 2) | caught after the shrink-to-an-old-cut edits were added |
| Node reuse without the "writer levels empty" check | caught after the "in the last chunk of a leaf" edit was added |
| Node reuse without the "complete node" check | not caught. On an honest tree, only right-spine nodes are not complete. They end at the old end, so they are reused only when the copy runs to the end, and there the result is the same tree. The check stays for the plan rule. |

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 2 - Test coverage] Extra oracle edits for two rules that the random edits did not reach**
- **Found during:** Task 1 and Task 2 (mutation checks)
- **Issue:** The random edits did not prove two things. They had no shrink to exactly an old cut, which is the old-end usable-cut rule. They also had no resync at an old leaf start while the new leaf still had pending entries, which is the guard of node reuse.
- **Fix:** Kind 3 in the 1 MiB test now shrinks to an old cut every other time. The 32 MiB test has a "shrink to a leaf boundary" edit and an "in the last chunk of a leaf" edit. The second edit changes a byte in the first `CHUNK_MIN` bytes of a chunk, which FastCDC does not hash. Kind 9 in the 1 MiB test puts a second extent exactly on the old cut after the first, to test region merge at the resync point.
- **Files modified:** crates/chunk-store/src/file.rs
- **Commits:** c035209, 1a2e70d

**2. [Rule 2 - Test coverage] Deep-tree check in `a_small_splice_reuses_old_nodes`**
- **Found during:** Task 2
- **Issue:** The 32 MiB base has a level-1 root, so only leaves can be reused. Reuse of level-1 nodes was not tested.
- **Fix:** The test also builds a tree of 1,000,000 fake entries (root level 2 or more). It checks that a splice with no change gives the base key, and that a shrink to an old cut gives the key of a `FileWriter` that pushes the same entries.
- **Files modified:** crates/chunk-store/src/file.rs
- **Commit:** 1a2e70d

**3. [Rule 3 - Tooling] Branch name outside the agent allow-list**
- The orchestrator named the branch `feature/14-02-splice`. The generic pre-commit allow-list of the executor accepts only `agent-*` branches. The branch is not protected and HEAD was not detached, so the commits were made on it.

**4. Lint allow in tests**
- `#[allow(clippy::single_range_in_vec_init)]` is on `file::tests`. In the tests, a list of one changed range is on purpose.

## Known Stubs

None.

## Threat Flags

None. The cursor runs the `FileReader` checks (T-14-04). The oracle tests cover T-14-05. As T-14-06 accepts, a resync on non-zero periodic data can run to EOF: CPU is O(run) and memory stays fixed.

## Self-Check: PASSED

- FOUND: crates/chunk-store/src/file.rs (`pub fn splice`, `pub trait SpliceSource`, `pub struct SpliceStats`)
- FOUND: c035209, 1a2e70d (ancestors of HEAD)
- Only crates/chunk-store/src/file.rs changed in the code commits.
