---
phase: 14-buffered-delta-re-log
plan: 01
subsystem: write-path
tags: [overlay, fuse, write-log, refcount, online-gc]
status: complete

requires:
  - phase: 13
    provides: "write log close-out (PR #100): commit_cut relog with flush_wal, replay tests"
provides:
  - "crate::overlay::Overlay and RangeView (pure data, property-tested)"
  - "WriteMode::Overlay replaces the full-file buffer at every arm"
  - "install_root / to_overlay / rebase_overlay in filesystem.rs"
affects: [14-04, 14-05, 14-06]

actuals:
  tokens: 7250      # chars/4 over the added diff lines (28982 chars)
  tasks: 2
  commits: 2
plan_head_before: 5fce24319860899851a66716622e7e41bc40e817
plan_head_after: 72b429a  # task commits only; the SUMMARY commit follows

tech-stack:
  added: []
  patterns:
    - "Overlay: base root in the manifest + BTreeMap extents; reads merge base, zeros, extents"
    - "Every base install goes through install_root (manifest, refcount +1/-1, stamp_content)"

key-files:
  created:
    - crates/slicefs-cli/src/overlay.rs
  modified:
    - crates/slicefs-cli/src/filesystem.rs
    - crates/slicefs-cli/src/lib.rs
    - crates/slicefs-cli/tests/coverage_boost_tests.rs

key-decisions:
  - "The switch installs the base in the manifest at once and keeps its refcount; only a replacing root releases it (Pitfall 5)"
  - "flush_handle lost its include_buffered parameter: an overlay always returns Ok at once (D-09)"
  - "RangeView::read returns an error on a short base read, so a damaged base never turns into zeros in a rebase"
  - "The switch log text 'switches to Buffered' stays byte-identical (D-12; 14-04 changes it with the Phase 13 check)"

requirements-completed: []  # BUFR-02/BUFR-04 are only partly met here; 14-04 completes them (see Open items)

duration: ~45 min
completed: 2026-11-18
---

# Phase 14 Plan 01: Overlay write path Summary

Random writes now go to an overlay (base root in the manifest + sparse extents), not a full-file buffer. Switch, write, read, release, commit relog, fsync, truncate and GC all have a real overlay arm. Release and destroy rebase by a re-stream in 1 MiB pieces.

## Final names and signatures

overlay.rs (`pub mod overlay;` in lib.rs):
```rust
pub const EXTENT_OVERHEAD: usize = 64;
pub struct Overlay { pub base: Option<ObjectKey>, pub base_len: u64, pub visible: u64, pub size: u64, extents: BTreeMap<u64, Vec<u8>>, cost: usize }
impl Overlay {
    pub fn new(base: Option<ObjectKey>, base_len: u64, size: u64) -> Self;   // visible = base_len
    pub fn write(&mut self, off: u64, data: &[u8]);
    pub fn put(&mut self, off: u64, data: Vec<u8>);     // owned write; no copy if it touches no extent
    pub fn truncate(&mut self, n: u64);
    pub fn cost(&self) -> usize;                        // sum of capacities + EXTENT_OVERHEAD per extent
    pub fn is_clean(&self) -> bool;
    pub fn extent_ranges(&self) -> Vec<Range<u64>>;
    pub fn extent_count(&self) -> usize;
    pub fn copy_out(&self, off: u64, len: usize) -> Vec<u8>;   // empty if not inside one extent
    pub fn view(&self, off: u64, len: u64) -> RangeView;
}
pub struct RangeView { pub base: Option<ObjectKey>, off, end, visible, parts: Vec<(u64, Vec<u8>)> }
impl RangeView {
    pub fn read(self, base_read: impl FnMut(u64, usize) -> io::Result<Vec<u8>>) -> io::Result<Vec<u8>>;
}
```

filesystem.rs:
```rust
enum WriteMode { Streaming { state, next_expected_offset, pending }, Overlay(Overlay) }
const REBASE_PIECE: u64 = 1 << 20;
const RELOG_PIECE: u64 = 1 << 20;
fn install_root(&self, ino: u64, new: Option<ObjectKey>, size: u64, modified: Option<(i64, u32)>) -> Result<(), i32>;
fn to_overlay(&self, s: &mut OpenFileState) -> Result<(), i32>;          // fh is not needed: the eprintln stays in test_write
fn rebase_overlay(&self, ino: u64, s: &mut OpenFileState) -> Result<(), i32>;
fn overlay_write(s: &mut OpenFileState, offset: u64, data: &[u8]) -> Result<u32, i32>;
fn read_root(&self, ino: u64, root: &ObjectKey, offset: u64, size: usize) -> io::Result<Vec<u8>>;  // the per-inode readers cache, now shared
fn flush_handle(&self, ino: u64, fh: u64) -> Result<(), i32>;            // include_buffered removed
```

## Arms

- **Switch** (`to_overlay`, from `test_write`): base = `state.clone().finish()` if streamed, else the manifest root if `has_content`, else None. `install_root` with size = max(base_len, byte_count, min_size). Pending writes move in by `Overlay::put`. Fallible steps run before `write_mode` changes, so an error leaves a valid streaming handle.
- **Write**: `Overlay::write`, `byte_count = ov.size`.
- **Read**: under `open_files`, only `ov.view(offset, size)`. Then the base part is read through the `readers` cache.
- **Release / destroy** (`finalize_write`): `rebase_overlay` (a clean overlay is kept as it is).
- **fsync** (`flush_handle`): an overlay returns Ok before `cas_committed` is set.
- **commit_cut**: Setattr(visible) if shrunk, Setattr(size) if changed, Write records of at most 1 MiB per extent, then the existing ctime Setattr. No Truncate.
- **setattr size, Case A**: 0 gives a fresh Streaming state and clears the manifest. Any other size gives `Overlay::truncate`.
- **gc_start**: `ov.base` is a file root.

## Tasks

| Task | Name | Commit |
|------|------|--------|
| 1 | Overlay end to end (tracer) | 88db125 |
| 2 | Property tests, alias test, crate run | 72b429a |

Tracer gate: Task 1 verify passed (`WriteMode::Overlay` on 13 lines; lib 305 ok; write_log_replay 15, fsync 6, streaming 25, gc_online 6 ok) before Task 2 started.

## Test results (final `cargo test -p slicefs-cli`, exit 0)

```
test result: ok. 309 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 13.76s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 107 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.26s
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.89s
test result: ok. 25 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.03s
test result: ok. 78 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.60s
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.41s
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 10.41s
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 4.19s
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.52s
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.38s
test result: ok. 55 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.17s
test result: ok. 41 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.14s
test result: ok. 40 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.06s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.31s
test result: ok. 0 passed; 0 failed; 4 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
test result: ok. 25 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.33s
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.07s
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 15 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.18s
test result: ok. 20 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.03s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```
The 4 named tests print `... ok`: overlay_reads_match_a_vec_model, touching_writes_merge_and_cost_counts_each_extent, a_far_write_and_a_huge_extend_allocate_nothing_by_offset, two_aliased_handles_on_an_overlay_keep_every_write.

TDD note (Task 2): the tests passed on the first run, because Task 1 already holds the implementation. I did not fake a RED step. To show the model test can fail, I removed the `visible` clamp in `Overlay::truncate` for one run: `overlay_reads_match_a_vec_model` FAILED. Then I put the clamp back.

## Deviations from Plan

**1. [Rule 1 - Bug in a test] coverage_boost_tests.rs `test_write_fallback_with_prior_committed_root_decrements_refcount` changed**
- **Found during:** Task 2 (whole crate run)
- **Issue:** the test checked the old switch bug: the refcount of the fsynced root went to 0 at the switch, but the manifest still named that root (RESEARCH Pitfall 5, CONTEXT Risks). The plan says both "every existing test unchanged" and "never decrement a root that the manifest still names". These two rules conflict on this test.
- **Fix:** the test now checks that the refcount stays 1 across the switch, goes to 0 when release installs the new root, and that the content reads "world". The test name stays the same.
- **Commit:** 72b429a

**2. [Process] Branch name**: commits are on `feature/14-01-overlay`, as the orchestrator asked. The stock pre-commit allow-list (`agent-*`) does not accept that name. The detached-HEAD check and the protected-branch check passed.

**3. [Process] Baseline**: the baseline run on untouched HEAD ran at the same time as my first edits, so its results are not valid (compile errors from my half-done swap). After each task, the plan's verify commands ran on the finished code. No other existing test failed.

**4. [Minor] to_overlay takes no `fh`**: the `switches to Buffered` eprintln stays in `test_write`, with the same text and fields. It runs just before the call, so `to_overlay` does not need `fh`.

## Open items owned by 14-04 (hard rule not fully met yet, by plan)

- No dirty limit yet. A sequential in-place rewrite of an existing file merges into one extent, and that extent grows to the file size until release.
- `handle_content` still loads the whole file, in the Streaming non-extension setattr arm.
- `commit_cut` copies all extents (1 MiB pieces) into `Vec<Record>` before `meta.commit()`. It does not use descriptors yet.
- Refcount note: the legacy Streaming setattr arm decrements `last_committed_root`, but the manifest still names that root. A later switch then decrements M again. `decrement_refcount` does nothing at 0 and GC is mark-and-sweep, so this is bookkeeping only. 14-04 removes that arm.

## Known Stubs

None.

## Self-Check: PASSED

- FOUND: crates/slicefs-cli/src/overlay.rs (`pub struct Overlay`)
- FOUND: `pub mod overlay;` in lib.rs; `fn install_root`, `fn to_overlay`, `fn rebase_overlay` in filesystem.rs
- `pwrite_into` in non-test, non-comment code: 0
- FOUND commits: 88db125, 72b429a
