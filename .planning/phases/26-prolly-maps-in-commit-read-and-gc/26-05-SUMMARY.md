---
phase: 26-prolly-maps-in-commit-read-and-gc
plan: 05
subsystem: metadata
tags: [prolly, commit-protocol, crash-tests, fault-points, pmap-03, pmap-06, pmap-08]
requires: ["26-04", "26-14"]
provides:
  - "CommitFault::{AfterApply, AfterInstall, BeforePreRootSync} (with BeforeSync, BeforeRoot): a fault point at every write and sync step of finish_commit"
  - "DictMetadataStore::requeue_all(trees_installed): one place that makes everything a failed or dropped commit took dirty again"
  - "test hooks: test_fail_next_flush, test_set_flush_hook / test_clear_flush_hook, test_set_tree_flush"
  - "commit_crash_tests: check_tree validator and the fault x crash matrix"
  - "write-log windows AfterApply, AfterInstall, BeforePreRootSync and a committed-root-at-the-cut check"
affects: [26-07, 26-08, 26-09, 26-12]
key-files:
  created:
    - crates/metadata/tests/commit_crash_tests.rs
    - .planning/phases/26-prolly-maps-in-commit-read-and-gc/runs/gate-26-05.log
  modified:
    - crates/metadata/src/store.rs
    - crates/metadata/src/pmap.rs
    - crates/metadata/tests/gc_prolly_tests.rs
    - crates/slicefs-cli/tests/write_log_replay_tests.rs
key-decisions:
  - "Fault-point list as in the plan (discretion choice): AfterApply, BeforeSync, AfterInstall, BeforePreRootSync, BeforeRoot"
  - "requeue_all(trees_installed): before the install it requeues the tree overlays and ends the prepared state; after it only the inode and directory caches (already settled, so a no-op). Later plans add their tree to this one function."
  - "Flush limit is a store field (tree_flush, default TREE_FLUSH) with a doc(hidden) setter, so integration tests reach the hot-path flush without 16,384 entries"
status: complete
metrics:
  completed: 2026-11-28
actuals:
  tokens: 14000
  tasks: 2
  commits: 2
plan_head_before: 27250d0
plan_head_after: 05958eab356ac5050accb2234492dd3c4df4b064
---

# Phase 26 Plan 05: Commit protocol proof on the manifest tree Summary

The commit protocol of 26-04 now has a fault point at every write and sync step, requeues everything a failed commit took, and is tested under races, under the online GC order, and with a power loss at each point. No product behavior changed except the new fault points and the `requeue_all` refactor.

## What was built

- `store.rs`: `CommitFault` gained `AfterApply = 3` (trees applied, no root record), `AfterInstall = 4` (roots installed and settled, not synced) and `BeforePreRootSync = 5` (chunk store synced, content not). `requeue_all(trees_installed)` replaces the inline requeue in `Drop for PreparedCommit`. `tree_flush` field plus `test_set_tree_flush`, `test_fail_next_flush`, `test_set_flush_hook`, `test_clear_flush_hook`. Lock-order comment names the hook lock as a leaf.
- `pmap.rs`: `MapTree` has an injected flush failure (after the take: requeue and return an error) and a flush hook (run between the install of the new root and the settle).
- Unit tests in `store.rs` (all with a flush limit of 8): `a_failed_commit_before_install_requeues_every_overlay` (AfterApply, BeforeSync), `a_failed_commit_after_install_keeps_the_state` (AfterInstall, BeforePreRootSync, BeforeRoot), `a_manifest_set_racing_flushes_and_commits_loses_nothing` (4 writers x 64 inodes x 40 versions, a commit thread; each writer reads back its own write at once, so an older value in a flush or install window fails), `a_read_in_the_flush_window_sees_the_new_value`, `a_post_cut_change_is_not_in_the_committed_root`, `writes_during_the_finish_are_not_in_the_committed_root` (writes from the main thread while another thread finishes the commit, 10 rounds), `a_failed_flush_requeues_and_fails_the_next_commit`.
- `gc_prolly_tests.rs`: `nodes_of_a_flush_after_gc_begin_survive_the_sweep` (20,000 manifests so the real 16,384 limit flushes; asserts the object count grew after `gc_begin_full`).
- `commit_crash_tests.rs` (MemIo): `check_tree` (prolly::visit over the tree of the root, exact ino set, each ino an inode with the expected manifest, tree value equals `manifest_digest`, root directory listing equals the model); `a_cut_commit_reloads_to_a_consistent_tree` (5 faults), `packs_that_seal_inside_a_commit` (64 KiB packs, asserts a pack sealed in the failed commit), `a_flush_before_the_cut_does_not_break_the_matrix` (flush limit 8), `a_failed_sync_then_a_good_commit`, `a_failed_chunk_store_sync_stops_writes_until_a_remount`. Each matrix run reloads root A after a crash, then on the same store commits B, crashes again and reloads B. Whole file about 4 s.
- `write_log_replay_tests.rs`: `Window::{AfterApply, AfterInstall, BeforePreRootSync}` in the loop (3 write-log strategies); `cut_window_scenario` sets the flush limit to 1 so every window runs flushes before the cut and after a failed finish. `Window::Done` loads the committed root with no log: pre-cut names, the 20 `d/x*` files with manifests are there; `kept2.bin` and `late.bin` are not, and the live store has the manifest of `late.bin` while the root has neither inode nor manifest.

## Mutation checks

Skipping the manifest requeue fails the before-install test; skipping `cs.sync()` fails every matrix test; removing the `prepared` guard from `flush_trees` fails the post-cut unit tests and the write-log windows.

## Deviations from Plan

**1. [Premise wrong] `a_failed_sync_then_a_good_commit` does not use `fail_sync_after(0)`**
- The chunk store retries pack syncs, and a failed directory sync (what `fail_sync_after(0)` hits) stops all writes until the store is opened again (documented behavior: "unmount and mount the store again"). A "next commit succeeds" test cannot use it. The test fails the pre-root content sync instead (`set_pre_root_sync`), then a good commit. A second test, `a_failed_chunk_store_sync_stops_writes_until_a_remount`, pins the chunk-store behavior: the next commit fails too, a crash keeps root A consistent, and the reloaded store commits again.

**2. [Rule 3 - Blocking] Test knobs not in the plan's artifact list**
- `test_set_tree_flush`, `test_set_flush_hook`, `test_clear_flush_hook`. Integration tests build the library without `cfg(test)`, so the flush limit is 16,384 there (26-04 note). A first version of the flush-before-the-cut test used 17,000 files; one commit then took about 17 s in a debug MemIo run, 66 s for three faults. The setter replaced it and the file runs in 4 s.

**3. GC test uses a file store**
- GC needs `gc_tmp_dir`, which MemIo stores do not set; the test uses `StoreIo::open` on a temp dir.

**4. Extra tests** beyond the plan: `writes_during_the_finish_are_not_in_the_committed_root`, `a_flush_before_the_cut_does_not_break_the_matrix`, `a_failed_chunk_store_sync_stops_writes_until_a_remount`.

**5. One code commit for both tasks** (rule 2: only the combined tree has a workspace run), then the gate log.

**6. Clippy** `type_complexity` in the new flush-window test; fixed with a type alias, and all gates were run again on the final tree.

## Verification

`runs/gate-26-05.log`: five sections, all `rc=0` (`cargo test -p metadata`; write_log_replay, crash_recovery, gc_online and snapshot_switch_gc tests; lock_stats_report with `--features lock-stats`; clippy `-D warnings` for metadata; `cargo test --workspace`). `retry_tests` did not fail. The log has no home or user paths.

## Notes for later plans

- Each new tree (26-07 to 26-09) adds itself to `requeue_all` (before-install branch), to the `AfterApply` position (after its apply), and to `flush_trees`. `test_fail_next_flush` and the flush hook act on the manifest tree only; widen them per tree in 26-12.
- `check_tree` reads only the manifest field (offset `MANIFEST_DATA`) through the public API; 26-12 extends it to the other trees and the same `World` model.
- A crash between two node puts is covered by `MemIo::crash()` at AfterApply and BeforeSync (no sync between the puts). Torn records inside a pack are the chunk store's recovery, not tested here (flagged assumption of the plan).
- A failed chunk-store directory sync stops writes until a remount; the write log replays the lost requests then.
- A debug-build commit of a 17,000-inode MemIo store takes about 16 s (not investigated; no early optimization).

## Known Stubs

None.

## Threat Flags

None beyond the plan's register. T-26-13 is covered by `a_post_cut_change_is_not_in_the_committed_root`, `writes_during_the_finish_are_not_in_the_committed_root` and the `Window::Done` check; T-26-14 by the two requeue tests and the failed-flush test; T-26-15 by the crash matrix at all five points.

## Self-Check: PASSED

`commit_crash_tests.rs`, `gate-26-05.log` exist; commits `c77cc3b` and `05958ea` are ancestors of HEAD; the gate log has five `rc=0` lines and no `rc=[1-9]`.
