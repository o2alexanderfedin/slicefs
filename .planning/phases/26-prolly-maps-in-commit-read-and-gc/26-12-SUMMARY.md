---
phase: 26-prolly-maps-in-commit-read-and-gc
plan: 12
subsystem: metadata, slicefs-cli (tests)
tags: [prolly, crash-tests, fault-points, pmap-08]
requires: ["26-10"]
provides:
  - "commit_crash_tests: check_tree over the five trees (rules between the trees, then a model kept by the test)"
  - "fault-point matrix with edits in all five trees; random_cuts_replay_to_consistent_trees (PMAP08_SEED, PMAP08_CASES)"
  - "a failed flush and a change in the flush window, for each tree"
  - "DictMetadataStore::test_fail_next_flush_of / test_set_flush_hook_of (TestTree selector); old manifest-only calls kept"
  - "create_inode_at probe-again unit test (read hook between the probe and the check)"
  - "write-log windows with cutmap- requests over every map before and after the cut"
affects: [26-13]
key-files:
  created:
    - .planning/phases/26-prolly-maps-in-commit-read-and-gc/runs/gate-26-12.log
  modified:
    - crates/metadata/tests/commit_crash_tests.rs
    - crates/slicefs-cli/tests/write_log_replay_tests.rs
    - crates/metadata/src/store.rs
    - crates/metadata/src/pmap.rs
key-decisions:
  - "32 random cases with a fixed seed (0x261208085eed) in the default run; PMAP08_SEED and PMAP08_CASES for a longer run by hand (discretion choice of the plan)"
  - "check_tree returns Result<(), String> (a panicking wrapper for the tests), so the negative test can assert the error"
  - "The flush hooks and flush faults take a TestTree selector; test_fail_next_flush and test_set_flush_hook stay on the manifest tree for the existing unit tests"
  - "The post-cut requests of cut_window_scenario and the crashed mount run on their own threads, as in a real mount"
status: complete
metrics:
  completed: 2026-11-27
actuals:
  tokens: 16000
  tasks: 2
  commits: 3
plan_head_before: 53eff1f808a3aa925b4426b25c376a3ce147179a
plan_head_after: 7b9b3249c39f507ff0a4842c3aac67600d358088
---

# Phase 26 Plan 12: crash tests over all five trees Summary

A commit cut at each of the five fault points, then `MemIo::crash()` and a reload, now gives five consistent trees. The validator walks all five trees of the root record and checks the rules between them. It then compares the state with a model that the test keeps and never reads back from the store. 32 seeded random schedules (200 in a release run) and the write-log windows over every map pass too.

## What was built

- **`check_tree`** (`commit_crash_tests.rs`). It reads the root record and walks each of the five trees with `prolly::visit`. Every node must pass the prolly checks, the keys must ascend, and `prolly::count` must equal the walk. Then the rules between the trees:
  - `inode_data`: each record is 56 or 72 bytes and holds its own ino. `inode_count()` equals the entry count, and `logical_bytes()` equals the sum of the sizes.
  - `dir_data`: each value is 9 bytes. Each entry is in a directory. Its child is an inode, and its type byte is `(mode & S_IFMT) >> 12` of the child. Every directory has `.` (itself) and `..` (a directory; 1 for the root). `list_directory` gives the same entries as the tree.
  - `manifest_data`: each ino is an inode, the value is `manifest_digest`, and `get_manifest` reads.
  - `xattr_data`: the first 8 key bytes are an inode. `xattr_len` and `read_xattr` read every value. Tag 0 means inline and must fit; tag 1 means 33 bytes and must not fit.
  - `refcounts`: each count is at least 1 and equals `get_refcount`.
  - Then the state equals the model: inodes (mode, size, nlinks), names, `..` targets, manifests, xattrs (values, and the names from `for_each_xattr_name`) and refcounts.
- **Model and `World`**. The changes keep the store and the model in step, and mirror what the store does. `link` and `unlink` do not change `nlinks`, so the test does that, as the file system does. `create_directory` adds 1 to the parent's links. `delete_inode` drops the manifest, the xattrs and the directory entries. `every_tree()` makes edits in each tree:
  - a directory with files;
  - inline and spilled xattrs, an inline-to-spilled replace, a remove, and an xattr on a directory;
  - a hard link across directories;
  - a rename of a file and of an empty directory across directories (with `set_parent`);
  - an rmdir;
  - refcounts up and down, and a decrement of an absent key.
- **Matrix.** `cut_then_crash` (with `a_cut_commit_reloads_to_a_consistent_tree`, `packs_that_seal_inside_a_commit` and `a_flush_before_the_cut_does_not_break_the_matrix`) edits all five trees before root A and between A and the cut commit. After the crash it reloads A. It then commits B on the reloaded store, crashes again and reloads B. `a_failed_sync_then_a_good_commit` and `a_failed_chunk_store_sync_stops_writes_until_a_remount` use the same validator.
- **`random_cuts_replay_to_consistent_trees`**. Each case draws a flush limit (1, 4, 8 or the default) and runs 20-60 random operations, then a good commit A. Next come 20-60 more operations and a commit at a random fault point (or none). The operations are create, write, resize, xattr set and remove (25% spilled), hard link, rename across directories, mkdir, rmdir, unlink, and refcount increment and decrement. After a power loss, the case reloads the last logged root (A, or the new root) and checks it against the model of that root. Then come 10-30 more operations on the reloaded store, a good commit, a crash and a check. The seed line and each case's seed go to stderr. Case 0 uses the seed itself, so `PMAP08_SEED=<case seed> PMAP08_CASES=1` repeats one case.
- **`check_tree_finds_a_name_whose_inode_is_gone`**. `delete_inode` without `unlink` (the public calls in the wrong order), then a commit. The validator gives `dir_data: child <f> of 1/<name> is not an inode`. The same store passed before the bad delete.
- **Per-tree hooks** (`store.rs`). `TestTree` (`Inode`, `Dir`, `Manifest`, `Xattr`, `Refcount`) with `test_fail_next_flush_of(tree)` and `test_set_flush_hook_of(tree, Option<hook>)`. They reach the existing per-`MapTree` slots, so no lock is new. Two tests use them, each for all five trees:
  - `a_failed_flush_of_each_tree_then_a_good_commit`: a failed flush, then the next commit fails with "injected flush fault", then a good commit, a crash, and the model.
  - `a_change_in_the_flush_window_of_each_tree_is_kept`: the hook changes a key of the flush between the install and the settle, then a commit, a crash, and the model.
- **`create_inode_at` retry branch** (26-08 note). `MapTree::run_read_hook` (cfg(test)) is the read hook of `read_modify_write`, factored out. `insert_inode` calls it between its probe and its check. The new unit test `create_inode_at_probes_again_when_the_tree_moves`: in the window, a rival takes the same number and is flushed. The generation moves, the create probes again and gives `AlreadyExists`, and the rival's inode (size 7) is kept before and after a reload.
- **Write-log windows** (`write_log_replay_tests.rs`). `cutmap_requests` makes, under `cutmap-<tag>-*` names:
  - a mkdir with a written file (inode, dir, manifest and refcount entries);
  - inline and spilled xattrs, a set then a remove, and an xattr on the directory;
  - a hard link, a rename across directories, and a mkdir then an rmdir.

  It runs before the cut (`pre`) and after it (`post`). After the cut, the scenario also removes and replaces xattrs of the pre-cut file, moves its hard link across directories, and adds one more link. `Window::Done` checks that the committed root has the pre-cut objects (names, the moved file, the rmdir, xattrs, manifest) and none of the post-cut changes. `dump_by` in `tests/common/mod.rs` already prints `nlinks` and the xattrs, so it was not changed.

## Mutation checks

Each mutation was applied by hand, the tests were run, and the code was put back:

- The settle drops every taken key, so a newer edit is lost: `a_change_in_the_flush_window_of_each_tree_is_kept` fails.
- The flush requeue drops the in-flight keys: `a_failed_flush_of_each_tree_then_a_good_commit` fails.
- The root record gets the cut root of the refcount tree, not the applied one: 9 of the 10 tests fail, `random_cuts_*` among them (the old xattr-only test passes).
- `insert_inode` pairs its stale probe with the current generation: `create_inode_at_probes_again_when_the_tree_moves` fails.

## Decisions

- 32 random cases with the fixed seed `0x261208085eed` in the default run, and `PMAP08_SEED` / `PMAP08_CASES` for a longer run by hand (the plan's discretion choice). The release run with 200 cases takes about 1 s, and the debug file takes about 7 s.
- Inodes with no name are legal (the xattr test makes them), so the validator takes the inode set from the inode tree, not from the namespace walk. The model then pins the exact set.
- The two old hook calls keep their manifest-only meaning, because unit tests call them.

## Deviations from Plan

**1. [Rule 3] `crates/metadata/src/store.rs` and `crates/metadata/src/pmap.rs` are not in `files_modified`.** The orchestrator's carried notes asked for the per-tree hooks and the `create_inode_at` retry test. The change is the smallest that does this: one enum, two selector functions, one cfg(test) call site in `insert_inode`, and one cfg(test) helper in `pmap.rs` that the old read hook code now uses. No product behavior changed, no lock is new, and the lock-order comment is still true, so `lock_stats_report` was not needed (rule 12).

**2. [Rule 1, test harness] Two threads in `a_crash_between_the_cut_and_the_root_replays`.**
- The cut holds the store's `commit_lock` (`Rank::Rename`). A rename across directories takes the namespace rename lock, which has the same rank. On one thread, the lock-rank check panics.
- In a mount, requests run on other threads than the commit, so the post-cut requests now run on a scoped thread.
- The `AfterCut` window forgets the cut, and so keeps its rank entry on the thread. The replay (a new mount) of a cross-directory rename then panicked on the same thread, so the crashed mount runs on a thread of its own.
- This is not a product bug: the commit thread never runs requests.

**3. One code commit for both tasks** (as 26-05 did). `cargo test --workspace` ran on the final tree only, and that tree has both tasks. Then comes the gate log commit.

**4. Extra gates:** clippy `-D warnings` for metadata and slicefs-cli (all targets) and `cargo test -p metadata --lib`, because `store.rs` changed. The first gate run failed clippy (`type_complexity` on the `walk` return type). The fix is a type alias, and then all gates ran again on the final tree.

**5. STATE.md, ROADMAP.md and REQUIREMENTS.md were not updated here.** 26-11 runs in parallel, and the orchestrator asked only for the SUMMARY and the gate log.

## Verification

`runs/gate-26-12.log` has 6 sections, all `rc=0`, and no `rc=[1-9]`:

- clippy `-D warnings` for metadata and slicefs-cli;
- `cargo test -p metadata --lib` (199 passed);
- `commit_crash_tests` (10 passed);
- `write_log_replay_tests`, `crash_recovery_tests`, `wal_durability_tests`, `replay_memory_tests` and `fsync_tests` (all ok);
- `PMAP08_CASES=200` in release: `PMAP08_SEED=0x261208085eed PMAP08_CASES=200`, 171 cases with a fault and 29 with none;
- `cargo test --workspace` (92 `test result: ok` lines, 0 failed). `retry_tests` did not fail.

The log has no home, user or scratch paths.

Acceptance greps:

| Grep | Count | Expected |
|------|-------|----------|
| `fn random_cuts_replay_to_consistent_trees` | 1 | 1 |
| `PMAP08_SEED` in commit_crash_tests.rs | 6 | at least 1 |
| `cutmap-` in write_log_replay_tests.rs | 17 | at least 1 |
| `^rc=0$` in the gate log | 6 | at least 4 |
| `^rc=[1-9]` in the gate log | 0 | 0 |

## Commits

- `06be99c` test(26-12): crash tests over all five trees (PMAP-08)
- `7b9b324` chore(26-12): gate log
- the summary commit

`commits: 3` counts these three. `tokens` is chars/4 of the code diff.

## Notes for 26-13

- `a_crash_between_the_cut_and_the_root_replays` took 82 s alone in one debug run. The whole file took 47 s in the gate run, and 44 s at 26-05. The flush limit of 1 in `cut_window_scenario` makes each new request flush all trees. 26-11 ran in parallel, so the load was high. If the test binary becomes the long pole of the workspace run, the post-cut `cutmap` set is the part to trim.
- The validator is in the test file only. A scrub-like check over a real store would need the same rules without the model. It is not built.
- The random schedule is single-threaded, so it cannot find flush-window races (the settle mutation does not fail it). The per-tree hook test covers those windows.
- Not covered: the cost of the five `SPT1` reads at load (26-10 note). No test here counts reads.

## Known Stubs

None.

## Threat Flags

None beyond the plan's register:

- T-26-35: the five-tree validator runs at every fault point, in 32 random schedules (200 in release), and in the per-tree flush fault and flush-window tests.
- T-26-36: the seed is fixed and printed, and each case seed is printed. One case can be run again with `PMAP08_SEED` and `PMAP08_CASES=1`.
- T-26-SC: no package was added.

## Self-Check: PASSED

The gate log and this SUMMARY exist. Commits `06be99c` and `7b9b324` are ancestors of HEAD (`plan_head_after` is the HEAD before the summary commit). The gate log has 6 `rc=0` lines, no `rc=[1-9]` line, and no home, user or scratch paths.
