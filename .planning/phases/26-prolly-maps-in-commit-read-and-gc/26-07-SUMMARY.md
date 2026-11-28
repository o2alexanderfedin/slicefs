---
phase: 26-prolly-maps-in-commit-read-and-gc
plan: 07
subsystem: metadata
tags: [prolly, refcounts, read-modify-write, gc-mark, rehash, pmap-03, pmap-06]
requires: ["26-05", "26-06"]
provides:
  - "refcounts as a prolly tree: MapTree<ObjectKey, u64> (key 33-byte content key, value count 8 bytes LE), Site::Refcounts"
  - "pmap: TreeKey for ObjectKey, TreeValue for u64, MapTree::modify (one RMW step under the shard lock), MapTree::for_each_value (cursor plus overlay copy)"
  - "store: change_refcount RMW loop (read_with_generation, modify, retry); refcount tree in commit, requeue_all, flush_trees, AfterApply, TreeStats, GC walk (Ref::Tree), tree_nodes"
  - "saturated_refcount_count() -> Result<usize, MetaError> (scrub-time scan)"
  - "rehash: refcount_tree rebuild (cursor, translate, extsort by new key, saturating merge, prolly::Builder); Kind::RefcountTree = 9, kind 6 retired"
affects: [26-08, 26-09, 26-12, 26-15]
tech-stack:
  added: []
  patterns:
    - "RMW on a map tree: modify(k, None) -> miss -> read_with_generation (no lock) -> modify(k, Some(value, generation)) -> retry while the generation moved"
    - "flush_trees flushes every tree over the limit under one apply_lock hold"
key-files:
  created:
    - .planning/phases/26-prolly-maps-in-commit-read-and-gc/runs/gate-26-07.log
  modified:
    - crates/metadata/src/store.rs
    - crates/metadata/src/pmap.rs
    - crates/metadata/src/rehash/mod.rs
    - crates/metadata/src/rehash/map.rs
    - crates/metadata/tests/refcount_tests.rs
    - crates/metadata/tests/gc_prolly_tests.rs
    - crates/slicefs-cli/src/scrub.rs
key-decisions:
  - "Refcount count is u64 little endian, 8 bytes (discretion choice of the plan)"
  - "Refcount overlay shard = first two digest bytes (key bytes 1..3, LE u16) mod 32; the tag byte is not used, since most keys share it (discretion choice of the plan)"
  - "increment/decrement/get keep their signatures (many slicefs-cli callers). A tree read error is logged and goes to put_error (the next commit fails); get_refcount logs and gives 0"
  - "saturated_refcount_count returns Result, so scrub reports a corrupt refcount tree instead of a silent 0"
  - "The scan of saturated_refcount_count reads the tree with an uncached cursor (does not fill the node cache), and is exact only when no request changes the store meanwhile (scrub)"
  - "The refcount tree gives only its nodes to the GC mark (Ref::Tree); its leaf keys are not live references (the manifests give the live content); leaf entries are length-checked"
status: complete
metrics:
  duration: "about 25 min"
  completed: 2026-11-28
actuals:
  tokens: 6440
  tasks: 2
  commits: 2
plan_head_before: a3494e8ad417544f6bf4a532592dbe0bd7a6e040
plan_head_after: 56531a0666c921edd38deb195f9764ae9e19beb3
---

# Phase 26 Plan 07: refcounts as a prolly tree Summary

The `refcounts` map is now a prolly tree with an overlay. Increments and decrements use the read-modify-write of `pmap` (overlay hit in place; else a tree read with its generation, then a recheck under the shard lock, with a retry), the commit and the GC mark cover the tree, and rehash rebuilds it under the target hash. Two of five maps are trees now (manifest, refcounts). `xattr_data` was not touched.

## What was built

- `pmap.rs`: `TreeKey for ObjectKey` (33 bytes; shard from the digest bytes), `TreeValue for u64` (8 bytes LE, length checked). `MapTree::modify(k, tree, f)` is one RMW step under the shard lock. The current value is the pending edit; if there is none, it is the given tree value, but only while the generation is the same. It returns `None` (retry) otherwise. A change acts as `put` does (dirty, the in-flight ticket is dropped), and "no change" writes nothing. `MapTree::for_each_value` reads the working root with one uncached cursor, skips the keys that have a pending edit, then adds the pending edits. `Overlay::pending` copies the overlay. New unit test `modify_retries_after_an_install`.
- `store.rs`: the `refcounts: MapTree<ObjectKey, u64>` field, set in `new` and `load_from_root` (the root-record field is the working root). `change_refcount` runs the RMW loop and calls `flush_trees` after a change. Increment saturates at `u64::MAX` and warns when a count reaches it. Decrement does nothing for a saturated count, an absent key or a tombstone, and deletes a count that reaches 0.
  - `get_refcount` is a plain read. `saturated_refcount_count` is the scrub-time scan.
  - `PreparedCommit` gets `refcount_root` and `refcounts: Taken<..>`. `begin_commit` takes them with `take_for_commit`.
  - `finish_inner` applies both trees before `AfterApply` and pushes a `TreeStats` for `refcounts`. Both trees are installed under one `apply_lock` hold.
  - `requeue_all` requeues the refcount overlay before the install. `flush_trees` flushes each tree over the limit under one `apply_lock` hold.
  - GC walk: `tree_nodes` and `for_each_referenced` walk the manifest tree, then the refcount tree, so both give the same order. `REFCOUNTS` is no longer a `Ref::Node`. `referenced_estimate` starts at 4 file-value nodes.
  - Removed: the flat `intern_key_u64_map` and `load_key_u64_map` (rule 14), and their line in `map_decode_tests`.
  - The lock-order comment now names the RMW (one overlay shard, no lock across the tree read).
- `rehash/mod.rs`: `refcount_tree` replaces the flat `refcounts` translation. It reads the old tree with a cursor (length-checked entries) and translates each key through `value("file content")`. A stale row (missing content) is dropped and counted. The rows are sorted with the external sorter, merged in one pass (saturating sum) and pushed into `prolly::Builder` with the header salt. A dry run writes nothing and counts the tree nodes of another tag with `prolly::visit`. The module doc is updated. `rehash/map.rs`: `Kind::RefcountTree = 9`; 6 is retired.
- `scrub.rs`: a refcount scan error is a scrub error ("refcount tree corrupt").

## Tests

- `refcount_tests.rs`: `refcounts_from_threads_lose_nothing`. Each of 8 threads makes 2,000 increments and 1,000 decrements over 64 shared keys, and a thread decrements only a key it already incremented. A commit thread runs in a loop, and a flush runs at each change (`test_set_tree_flush(1)`). The counts are exact before and after a reload.
- `store.rs` unit tests:
  - The three saturation tests now write to the overlay. `test_saturated_refcount_count` also covers tree entries after a commit, a tree entry hidden by a delete or by a normal count, and a count that saturates only in the overlay.
  - New `refcount_changes_follow_the_tree`: a decrement of an absent key writes no edit; the stored value is 8 bytes LE; a count of 0 leaves the tree; a tombstone reads as 0; a saturated count in the tree writes no edit.
  - Both fault tests (before and after the install) now check the refcount overlay and the counts after a reload.
  - The windowed-walk test adds refcounts. It checks that the refcount root is in the tree nodes and is not a file value.
- The rehash tests `two_old_keys_of_one_content_merge_their_counts` and `a_stale_refcount_is_dropped_and_counted` pass on the tree, and so do all other rehash tests.
- Mutation check: I removed the generation recheck in `modify`. `refcounts_from_threads_lose_nothing` still passed 6 times out of 6, because the race window is too small to hit. `modify_retries_after_an_install` failed at once, so the deterministic unit test is the guard for the recheck.

## Commits

- `cc06f0a` feat(26-07): refcounts as a prolly tree (Task 1)
- `56531a0` chore(26-07): gate log (Task 2)

## Deviations from Plan

**1. [Rule 3 - Blocking] `crates/metadata/src/pmap.rs` changed (not in `files_modified`)**
- The overlay shard locks are private to `pmap`, so the recheck under the shard lock (`modify`), the overlay copy for the scan (`for_each_value`) and the key and value impls for `ObjectKey -> u64` must be there. Two pmap tests now call `TreeKey::encode` and `TreeValue::encode` by trait name, because `u64` and `ObjectKey` now implement both traits.

**2. [Rule 3 - Blocking] `crates/metadata/tests/gc_prolly_tests.rs`**
- `manifest_tree_end_to_end` expected `tree_nodes` to be the manifest tree only. It now expects the manifest tree plus the refcount tree (one offset constant and a set union).

**3. [Rule 2 - Correctness] `saturated_refcount_count` returns `Result`; `crates/slicefs-cli/src/scrub.rs` changed**
- A tree scan can fail, and a silent 0 on the scrub path would hide damage. The change in scrub is one `match`.

**4. Concurrency test flush limit**
- The plan says "while flushes and commits run". The integration build has `TREE_FLUSH = 16,384`, so the test sets the limit to 1 with `test_set_tree_flush`.

**5. Gate commands**
- `cargo test -p metadata` and the CLI suites ran with `--no-fail-fast`. Clippy ran for `-p slicefs-cli -p metadata` (a superset of the plan's `-p metadata`, because `scrub.rs` changed).

## Verification

`runs/gate-26-07.log` has 6 sections, all `rc=0`: metadata, the five slicefs-cli suites, `lock_stats_report` with `--features lock-stats`, prolly-bench, clippy `-D warnings`, and `cargo test --workspace`. All gates ran on the committed tree. `retry_tests` did not fail.

Acceptance greps: `refcounts: sync::Mutex<BTreeMap<ObjectKey, u64>>` 0; `MapTree<ObjectKey, u64>` at least 1; `fn refcounts_from_threads_lose_nothing` 1.

## Notes for later plans (26-15 next)

- **Pattern:** the RMW pattern for a tree is `change_refcount` in `store.rs` with `MapTree::modify`. An xattr set or remove by name (26-15) is a plain `write`, with no RMW, unless it needs create-only or replace-only semantics. Those can use `modify` (`f` gets the current value and returns `None` to refuse).
- **Wiring:** every new tree must go into `PreparedCommit`, `begin_commit`, `finish_inner` (apply before `AfterApply`, `TreeStats`, install under the same `apply_lock` hold), `requeue_all`, `flush_trees` (a new `if dirty >= limit` arm), `tree_nodes` and `for_each_referenced`. These last two must keep the same order (manifest, refcounts, then new trees). The windowed-walk test asserts it. `referenced_estimate` drops 1 for each flat map that becomes a tree (now 4).
- **Test hooks:** `test_fail_next_flush` and the flush hook still act on the manifest tree only (26-12 widens them).
- **Bench:** `is_tree` in the bench already treats the refcount field as a tree, and the `whole` load line will print `refcounts_entries=`.
- **Commit stats:** `CommitStats.trees` has two entries (`manifest_data`, `refcounts`). Every converted tree costs two more root loads per commit for `height` and `entries`.
- **Concurrency:** `saturated_refcount_count` is exact only with no concurrent writes. It is fine for scrub (an offline load), not for a live mount.

## Known Stubs

None.

## Threat Flags

None beyond the plan's register. T-26-21: covered by the RMW with the generation recheck, by `refcounts_from_threads_lose_nothing`, by `modify_retries_after_an_install`, and by the saturation tests. Tree values from disk are length-checked in the read path (`TreeValue::decode`), the GC walk (`refcount_leaf`) and rehash. T-26-SC: no package added.

## Self-Check: PASSED

`gate-26-07.log` exists with 6 `rc=0` lines and no `rc=[1-9]`; commits `cc06f0a` and `56531a0` are ancestors of HEAD.
