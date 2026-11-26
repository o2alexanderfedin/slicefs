---
phase: 23-hashes-and-rehash
plan: 02
subsystem: metadata segments and offline GC
tags: [rehash, switch-segment, snapshots, gc, crash-safety]
status: complete

requires:
  - phase: 23-hashes-and-rehash (plans and research)
    provides: D-19 option (a), R-06, R-07, the switch crash table (RESEARCH Q7)
provides:
  - "both segment loaders keep the last SnapshotRecord of each version (private fn last_per_version), ascending version"
  - "metadata::segment::write_switch_segment(dir, root, snapshots) -> Result<u64, SegmentError>: one atomic switch (temp name, sync, rename, dir sync, remove older segments, dir sync)"
  - "metadata::segment::remove_switch_tmps(dir) -> io::Result<usize>; metadata::segment::switch::SWITCH_TMP_SUFFIX = \".seg.tmp\""
  - "metadata::wal::remove_segments_before is pub(crate) (no behavior change)"
  - "slicefs_cli::gc::gc_on_open_store(cs, roots, full) -> Result<GcStats, _>: the chunk-store GC on an open store, no lock, no second ChunkStore, no segment copy; gc_report calls it"
affects: [23-04]

actuals:
  tokens: 6400
  tasks: 2
  commits: 2

plan_head_before: 526609e4de6bba028cb4c1cc049631cffbecb51f
plan_head_after: d4ee240992d9e77f472d7f9061152811fb743096

tech-stack:
  added: []
  patterns:
    - "temp name that fails every segment filter, then rename and directory sync, as the atomic visibility step"
    - "last record per key in file order (BTreeMap insert) at the end of a loader, so no caller changes"

key-files:
  created:
    - crates/metadata/src/segment/switch.rs
    - crates/slicefs-cli/tests/snapshot_switch_gc_tests.rs
    - .planning/phases/23-hashes-and-rehash/runs/red-23-02.log
    - .planning/phases/23-hashes-and-rehash/runs/gate-23-02.log
  modified:
    - crates/metadata/src/segment/mod.rs
    - crates/metadata/src/wal/mod.rs
    - crates/slicefs-cli/src/gc.rs

key-decisions:
  - "The switch segment is written with SegmentWriter::sync (no EOF marker), as write_root_segment does"
  - "The switch id counts every segment-<n>.seg name of any size (as mount::next_segment_id) and ignores .seg.tmp names"
  - "Crash states are tested by file edits (rename back to .seg.tmp, truncate, restore old segments); no stop hooks inside the switch (plan 23-04 adds the run hooks)"

requirements-completed: [HASH-04]

duration: 75min
completed: 2026-11-26
---

# Phase 23 Plan 02: Switch segment and GC on an open store Summary

One crash-safe switch segment (`RootUpdate` plus one `SnapshotRecord` per snapshot, written as `.seg.tmp`, synced, renamed, directory synced, older segments removed), a loader rule that keeps the last record of each snapshot version, and `gc_on_open_store`, the chunk-store GC that runs on a store the caller already holds open and locked.

## Base

`BASE=526609e` (`git merge-base HEAD feature/phase-23-hashes-rehash`). Branch `exec/23-02`, not pushed.

## What was built

- **Loader rule** (`segment/mod.rs`): `last_per_version` at the return of `load_store_from_segments` and `scan_store_with_log`. Return types do not change, so every loader user (mount, gc, online gc, scrub, snapshot list and switch, stats) sees one root per snapshot.
- **Switch** (`segment/switch.rs`): `write_switch_segment`, `remove_switch_tmps`, `SWITCH_TMP_SUFFIX`. Doc comments state both preconditions (store lock held; every object of the new roots synced first). Older segments are removed with the existing `wal::remove_segments_before` (now `pub(crate)`), not a copy.
- **GC entry** (`slicefs-cli/src/gc.rs`): the mark-and-sweep block of `gc_report` moved into `gc_on_open_store` with the same error texts and `gc_abort` calls. `gc_report` calls it at the same place; its output does not change (gc_cli, gc_sweep, gc_online tests green).

## Tests

- Tracer `snapshot_switch_gc_tests::a_superseded_snapshot_root_is_no_longer_kept_by_gc`: seed, snapshot `s1`, new content of `a` (snapshot still on the old root, checked), switch the snapshot to the new root, both loaders give one snapshot `{1, "s1", R2, same created_at}` and an empty log, the old objects are present before the GC, a full `gc_on_open_store` with the roots from the loader frees all of them with `dead_left == Some(0)`, the file reads back and scrub passes.
- Seven unit tests in `segment::switch::tests`: `both_loaders_keep_the_last_record_of_each_version`, `a_switch_makes_the_new_roots_current` (exactly 1 + 2 records), `a_switch_tmp_before_the_rename_is_ignored` (complete and partial tmp), `old_segments_left_after_the_rename_lose_to_the_switch` (all and some old segments left), `a_stale_switch_tmp_is_removed_by_the_next_switch`, `the_switch_id_counts_short_segment_files`, `a_mount_segment_after_the_switch_is_the_write_log`.
- Red evidence in `runs/red-23-02.log`: the tracer compile errors before the code (no `write_switch_segment`, no `gc_on_open_store`); mutation 1 (no rename) turns 6 of 7 switch tests red; mutation 2 (no `last_per_version` in `scan_store_with_log`) turns 2 red. Both mutations undone.

## Commits

- `fd15c24` feat(23-02): switch segment supersedes snapshot roots; GC on an open store
- `d4ee240` test(23-02): crash states of the switch and the last-record-per-version rule
- this SUMMARY: docs(23-02)

## Gates

See `runs/gate-23-02.log`: local workspace gate (4 standard skips) on the Task 2 tree: `GATE_EXIT=0`, 80 result lines, 1616 passed, 0 failed (the 8 new tests included). `scripts/safety/clippy.sh` with `RUST_STABLE=stable`: `CLIPPY ... result=ok`, `UNSAFE files=31 tokens=132 result=ok`.

Notes on the gate runs:
- The executors share one scratch directory, and the first gate logs used names that another executor also used, so those logs were overwritten. The Task 1 gate log (exit 0, 1607 passed) cannot be trusted for that reason. The final gate and clippy runs used a private sub-folder; their output is what `gate-23-02.log` holds. That run is on the Task 2 tree, which is the Task 1 tree plus `#[cfg(test)]` code only, so it covers all Task 1 code.
- One shared-folder log showed `flusher_tests::a_throttled_writer_does_not_wait_when_the_flusher_cannot_free` failing its wall-clock bound (`took 4.58 s`, bound 2 s). That log may be another executor's run. The test passed alone here (0.22 s) and in the private full run.

Prohibitions: `git diff --quiet 526609e -- crates/metadata/src/lib.rs crates/metadata/src/store.rs crates/metadata/src/inode_map.rs crates/metadata/src/segment/writer.rs crates/metadata/src/segment/reader.rs` exits 0. No segment format change.

## Deviations from Plan

1. **[Rule 3 - Blocking] Branch base.** The worktree HEAD was on `main` (`ee2d0ec`), not on the feature branch, so the first `git checkout -b exec/23-02` had no phase files. The empty new branch was re-pointed with `git checkout -B exec/23-02 feature/phase-23-hashes-rehash` before any edit.
2. **[Rule 3 - Blocking] Branch name in the plan.** The plan names `feature/phase-23-hashes-and-rehash`; the real branch is `feature/phase-23-hashes-rehash`. The merge-base and diff checks used the real name (BASE `526609e`).
3. **Tracer test made stricter** (no behavior change): the GC roots come from the loader after the switch (threat T-23-06), not a hand-written `[R2, R2]` (the test asserts they are equal); the test also asserts that before the switch the snapshot is still on `R1`, and that every old object is present before the GC.

No other deviation. No file outside the plan list was touched.

## For the owner

- **Offline GC segment copy (RESEARCH Open Question 4), unchanged:** `GarbageCollector::run_gc` copies every segment to `segment-<id + 100000>.seg` and keeps the source. With the new loader rule the duplicate records are harmless, but after a double fault (crash between the switch rename and the removal of old segments, then an offline GC that stops after copying an old segment but before the switch segment) an old record copy could sort after the switch segment. Rehash does not call `GarbageCollector`, and the switch removes all lower ids right after the rename, so the normal case is closed. Out of scope here.

## Known Stubs

None.

## Self-Check: PASSED

Files: switch.rs, snapshot_switch_gc_tests.rs, red-23-02.log, gate-23-02.log exist. Commits `fd15c24` and `d4ee240` are ancestors of HEAD; `git rev-list --count 526609e..d4ee240` is 2.
