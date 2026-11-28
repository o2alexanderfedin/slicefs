---
phase: 26-prolly-maps-in-commit-read-and-gc
plan: 08
subsystem: metadata
tags: [prolly, inodes, inline-values, read-modify-write, gc-mark, rehash, pmap-03, pmap-06]
requires: ["26-15"]
provides:
  - "inode_data as a prolly tree: key ino (8 bytes BE), value the serialize_inode record inline (56 or 72 bytes, no tag); no inode object"
  - "inode.rs: impl TreeValue for InodeMeta over serialize_inode/deserialize_inode; intern_inode and load_inode removed"
  - "pmap: MapTree::with_sites (one Site for the overlay shards, one for the root lock), MapTree::read_modify_write (the whole RMW loop), a cfg(test) read hook (set_read_hook)"
  - "store: inodes: MapTree<u64, InodeMeta> (Site::InodeLive shards, Site::InodeData root); load_meta, modify_inode, delete_inode and insert_inode on the tree"
  - "inode tree in commit (TreeStats inode_data, 4 entries in CommitStats.trees), requeue_all, flush_trees, load_from_root (root inode by one read), GC walk (Ref::Tree), tree_nodes"
  - "rehash: inode_tree rebuild, Kind::InodeTree = 11"
affects: [26-09, 26-12, 26-16]
tech-stack:
  added: []
  patterns:
    - "MapTree::read_modify_write: modify with the pending edit; on a miss, read_with_generation (no lock), then modify again; retry while the generation moved"
    - "create-at probe: read_with_generation with no lock, then the allocator lock and MapTree::modify with the probe (refuse a value, accept a pending delete, retry on a moved generation)"
key-files:
  created:
    - .planning/phases/26-prolly-maps-in-commit-read-and-gc/runs/gate-26-08.log
  modified:
    - crates/metadata/src/store.rs
    - crates/metadata/src/inode.rs
    - crates/metadata/src/pmap.rs
    - crates/metadata/src/rehash/mod.rs
    - crates/metadata/src/rehash/map.rs
    - crates/metadata/tests/gc_prolly_tests.rs
key-decisions:
  - "MapTree<u64, InodeMeta> replaces the inode cache with dirty flags: one overlay scheme for every map; the node cache holds the clean inodes; INODE_CACHE_MAX goes (discretion choice of the plan)"
  - "Overlay shards timed Site::InodeLive, root lock Site::InodeData (the lock-stats report expects both labels; discretion choice of the plan)"
  - "delete_inode is a read-modify-write: the size is read and the pending delete written in one shard hold, so of two racing deletes one counts and the other gets NotFound"
  - "Inode records keep the ino in bytes 0..8 (serialize_inode as it is); the key is the ino BE"
  - "The read hook of read_modify_write is cfg(test) only: no lock cost on the hot path"
  - "Kind::InodeTree = 11 (next free number after XattrTree = 10)"
  - "CommitStats.trees order: manifest_data, refcounts, xattr_data, inode_data"
status: complete
metrics:
  completed: 2026-11-28
actuals:
  tokens: 20770
  tasks: 2
  commits: 2
plan_head_before: ef4fb9ed4b8b55488d81bf734165cf935da80be4
plan_head_after: 7b802bb395244ee610f049f7a02c8b5ae55ab3d7
---

# Phase 26 Plan 08: inodes inline in the inode_data prolly tree Summary

`inode_data` is now a prolly tree. The key is the ino (8 bytes big endian). The value is the inline inode record (`serialize_inode`, 56 or 72 bytes, with no tag byte). No inode is stored as an object of its own, and no inode map is in RAM. The overlay of `pmap` replaces the inode cache with its dirty flags. The node cache holds the clean inodes. Four of the five maps are trees now (manifest, refcounts, xattrs, inodes). Only `dir_data` is still flat (26-09).

## What was built

- `inode.rs`:
  - `impl TreeValue for InodeMeta`: encode is `serialize_inode`, and decode is `deserialize_inode`, which accepts only 56 or 72 bytes.
  - `intern_inode`, `load_inode` and their object-store test are removed. The encoding tests stay.
- `pmap.rs`:
  - `MapTree::with_sites(name, shards, root_site, root)`. `new` calls it with one site.
  - `MapTree::read_modify_write(io, k, f)` is the whole RMW loop. It calls `modify` with the pending edit. On a miss it runs `read_with_generation` with no lock held, then `modify` again with that value, and retries while the generation moved. `f` runs once, under the shard lock.
  - A `cfg(test)` read hook (`set_read_hook`). It runs once, between the tree read and the recheck, and is taken out of its slot and called with no lock held.
  - The protocol doc names the helper.
- `store.rs`:
  - The field `inodes: MapTree<u64, InodeMeta>` (shards `Site::InodeLive`, root `Site::InodeData`) replaces the inode cache, the inode digest map, `cached_inode`, `trim_inodes`, `flush_inodes`, `flush_inodes_between` and `INODE_CACHE_MAX`.
  - `load_meta` is a tree read.
  - `modify_inode` keeps the D-05 contract through `read_modify_write`. `f` runs under the shard lock, and `logical_bytes` follows the size in the same hold.
  - `delete_inode` is an RMW: it reads the size and writes the pending delete in one hold.
  - `insert_inode`:
    - A new number is allocated, and its value written, under `inode_alloc`.
    - For `Some(at)`, the store probes the tree with `read_with_generation` and no lock held. It then takes `inode_alloc` and runs `modify` with the probe: a value gives `AlreadyExists`, a pending delete is accepted, and a moved generation drops the allocator lock and probes again.
    - The allocator, the count and the size change only on success.
  - `new` writes the root inode into the empty tree through the overlay, and the first commit writes it.
  - Commit:
    - `PreparedCommit` has `inode_root` and `inodes: Taken<..>`.
    - `begin_commit` takes the inode overlay before the `next_ino` read (IN-01).
    - `finish_inner` applies the tree before `AfterApply`, pushes `TreeStats` "inode_data", and installs the tree under the same `apply_lock` hold.
    - The `INODE_DATA` field is the tree root.
  - `requeue_all` covers the tree. So do `flush_trees` (in the early-return test and in its own arm) and the lock-order comment.
  - `load_from_root` takes the field as the working root and checks the root inode with one tree read. `inode_count` and `logical_bytes` come from the root record, with no walk.
  - GC:
    - `INODE_DATA` is no longer in the `Ref::Node` list or in the flat-map walk.
    - `for_each_referenced` walks the inode tree after the xattr tree, with its nodes as `Ref::Tree`. Its leaves are checked by `inode_leaf`: an 8-byte key and a 56- or 72-byte value.
    - `tree_nodes` uses the same order: manifest, refcounts, xattrs, inodes.
    - `referenced_estimate` starts at 2 and reads only the directory map.
- `rehash/mod.rs`, `rehash/map.rs`:
  - `inode_tree` rebuilds the tree in key order with an uncached cursor. It checks each entry (an 8-byte key and a 56- or 72-byte value) and copies it, so only the node IDs change.
  - A dry run writes nothing and counts the nodes of another tag.
  - The `INODE_DATA` field is no longer a flat value map. `Kind::InodeTree = 11`. The docs are updated.

## Tests

- New unit tests in `store.rs`:
  - `a_cold_inode_read_racing_a_flush_is_not_stale`. The read hook changes the inode and flushes between the tree read and the recheck of a `modify_inode`. The change lands on the newest value (+10 then +1 gives +11), before and after a reload.
  - `create_inode_at_refuses_a_number_in_the_tree`. After a commit, with the overlay empty, `create_inode_at` of an existing ino (and of ino 1) gives `AlreadyExists`. After `delete_inode` and before any flush, it succeeds. The count, the size and a reload are checked.
  - `an_inode_value_is_56_or_72_bytes`. The leaf values are 56 and 72 bytes. Lengths 0, 55, 57, 60, 71 and 73 give `MetaError::Corrupted` from a tree read, and `inode_leaf` refuses them.
- Ported race tests (they use the flush hook of `pmap.rs` on `store.inodes`):
  - `flush_install_does_not_resurrect_a_deleted_inode`. The hook deletes X between the install and the settle. X stays deleted, the next flush removes it from the tree, and the reload has no X.
  - `late_older_install_does_not_overwrite_a_newer_change` (reshaped, see Deviations). In the first case, the commit cut takes size 1, the inode changes to size 2, and then the commit installs: the read gives 2 and the edit stays pending. In the second case, the hook changes X to size 3 between a flush install (size 4) and the settle: 3 survives the settle and the reload, and `logical_bytes` is 3.
  - `atomic_modify_inode_loses_no_change`, `missing_inode_modify_inode_is_not_found_and_creates_nothing`, `size_change_in_modify_inode_moves_logical_bytes`, `create_inode_at_takes_the_number_and_raises_the_allocator` and `cached_dirs_and_inodes_survive_eviction_commit_and_reload` pass. The last one now bounds the inode overlay by `TREE_FLUSH`.
- Both fault unit tests check the inode overlay. Before the install it is dirty again; after the install it is clean.
- The windowed-walk unit test:
  - It expects only the directory map as a flat map.
  - The inode tree has 41 entries and is a tree node after the xattr tree, not a file value.
- `tests/gc_prolly_tests.rs` `manifest_tree_end_to_end` adds the inode tree nodes to its expected `tree_nodes` set.
- The rehash test `a_root_rehashes_to_the_same_tree` checks that every tree node has the target tag (`tree_nodes` now lists the inode tree) and that the tree of inodes and contents is the same after a reload.

## Mutation checks

- I removed the generation recheck in `MapTree::modify` (a tree value is used with any generation). `a_cold_inode_read_racing_a_flush_is_not_stale` failed and `create_inode_at_refuses_a_number_in_the_tree` still passed. The code was restored from a copy before the gates ran.

## Decisions

- `MapTree<u64, InodeMeta>` replaces the inode cache with dirty flags. The clean inodes are in the node cache, and `INODE_CACHE_MAX` is gone. The overlay shards are timed as `Site::InodeLive` and the root lock as `Site::InodeData`. (These are the discretion choices of the plan, as written there.)
- `delete_inode` reads the size and writes the delete in one shard hold (an RMW). The plan reads the size and then writes the delete. The RMW is stricter: of two racing deletes, only one changes the counts.
- The inode record keeps the ino in its first 8 bytes. `serialize_inode` is used unchanged, and the key is also the ino.
- The read hook is `cfg(test)` only (so the RMW has no hook lock in a release build). The flush hook stays as it was. 26-12 widens the test hooks.
- `Kind::InodeTree = 11`. The order of `CommitStats.trees` is manifest_data, refcounts, xattr_data, inode_data.
- I added no `ino` check (record ino == key) on the read path. It is not in the plan.

## Deviations from Plan

**1. Tasks 1 and 2 are one code commit**
- After Task 1 alone, GC and rehash would read the inode tree root as a flat map, so the workspace cannot pass (rule 2; the 26-05 and 26-15 precedent). The commits are the code (Tasks 1+2), then the gate log.

**2. [Rule 3 - Blocking] `crates/metadata/tests/gc_prolly_tests.rs` (not in `files_modified`)**
- `manifest_tree_end_to_end` lists the tree nodes of the root. The inode tree is a tree now, so its nodes are in `tree_nodes`. The test has a new `INODE_DATA` offset constant and one more `extend`.

**3. `late_older_install_does_not_overwrite_a_newer_change` is reshaped**
- The old test ran a commit's install inside an inode cache flush. With pmap that order cannot happen: `flush_trees` skips while a commit is prepared. The flush hook also runs with `apply_lock` held, so `finish_commit` cannot run inside it.
- The test checks the same outcome in the two orders that can happen:
  - a change after the cut and before the install;
  - a change between a flush install and its settle (the hook).

**4. Gate commands**
- `cargo test -p metadata` and the CLI suites ran with `--no-fail-fast`.
- The `lock_stats_report` run used `--nocapture`, so the log shows the lines `inode_live n=88` and `inode_data n=12`.
- `unsafe-count.sh` ran as an extra gate (`files=34 tokens=144`, the same as 26-15).

No other file outside `files_modified` was changed.

## Verification

`runs/gate-26-08.log` has 7 sections, all `rc=0`, and all ran on the tree of `cdcddc6`:

- `cargo test -p metadata` (the lib run alone: 202 passed);
- the slicefs-cli suites (rehash, hash_change, gc_online, write_log_replay, posix_compliance, e2e_filesystem, crash_recovery);
- `lock_stats_report` with `--features lock-stats`;
- `prolly-bench`;
- clippy `-D warnings` for metadata;
- `unsafe-count.sh`;
- `cargo test --workspace` (91 test binaries, all ok).

`retry_tests` did not fail.

The acceptance greps give the expected counts:

| Grep | Count | Expected |
|------|-------|----------|
| `MapTree<u64, InodeMeta>` in store.rs | 2 | at least 1 |
| `inode_live`, `INODE_CACHE_MAX` or `fn flush_inodes` in store.rs | 0 | 0 |
| `pub fn intern_inode` or `pub fn load_inode` in inode.rs | 0 | 0 |
| `Blob::ValueMap("inode node")` in rehash/mod.rs | 0 | 0 |
| `^rc=0$` in the gate log | 7 | at least 6 |
| `^rc=[1-9]` in the gate log | 0 | 0 |

## Commits

- `cdcddc6` feat(26-08): inodes inline in the inode_data prolly tree (Tasks 1, 2)
- `7b802bb` chore(26-08): gate log

`commits: 2` counts the commits from `ef4fb9e` to `7b802bb`. The docs commit of this SUMMARY is not in the count.

## Notes for 26-09 (dir_data tree)

- **Last flat map:** `dir_data` is now the only flat map. `referenced_estimate` starts at 2 (the root record and the dir map) and becomes 1 when the dir map is a tree. `for_each_map_value`, `load_u64_digest_map`, `intern_u64_digest_map`, `MAP_ENTRY`, `Blob::ValueMap`, `Kind::ValueMap`, `Sharded`, `Live` and `DIRTY_FLUSH` are used only by the directories now, so 26-09 can remove them (and `map_decode_tests`).
- **Wiring:** add the dir tree everywhere the inode tree is listed:
  - `PreparedCommit` and `begin_commit` (the take before the `next_ino` read);
  - `finish_inner` (the apply before `AfterApply`, `TreeStats`, the install under the same `apply_lock` hold);
  - `requeue_all`;
  - `flush_trees` (the early-return test and an arm);
  - `tree_nodes` and `for_each_referenced`, in the order manifest, refcounts, xattrs, inodes, then dirs;
  - the windowed-walk test, `manifest_tree_end_to_end` and both fault unit tests.
  - `CommitStats.trees` has 4 entries now.
- **Sites:** use `MapTree::with_sites(.., Site::DirLive, Site::DirData, ..)`. The lock-stats report expects `dir_live` and `dir_data`.
- **RMW:** use `MapTree::read_modify_write` for a change that must see the current value (for example a create-only add of a name). A blind put is `write`. For a lookup and then an add that must be one step, `modify` with a probe works the same way as `insert_inode` does.
- **Scans:** `MapTree::scan_keys` needs every key of the range in one shard. Shard a dir key by `shard_of(dir_ino)`. `make_directory` still uses `is_dir` and `with_dir`. A cold `is_dir` can become one read of the `.` entry of the directory.
- **Test hooks:** the `cfg(test)` read hook (`set_read_hook`) works on any `MapTree`. `test_fail_next_flush` and `test_set_flush_hook` still act on the manifest tree only.
- **For 26-16 and the fs layer:** the inode calls keep their signatures and their results. `create_inode_at` of a deleted number succeeds as before. Two racing `delete_inode` calls of one ino now give one `Ok` and one `NotFound`.

## Known Stubs

None.

## Threat Flags

None beyond the plan's register. The register items are covered as follows:

- T-26-22: `read_modify_write` with the generation recheck; `a_cold_inode_read_racing_a_flush_is_not_stale`, plus the mutation check; the ported race tests; `create_inode_at_refuses_a_number_in_the_tree`.
- T-26-23: `TreeValue::decode` takes only 56 or 72 bytes, and so do `inode_leaf` (GC) and the rehash rebuild; `an_inode_value_is_56_or_72_bytes`.
- T-26-24: the allocator and `next_ino` are unchanged (read after the take). `create_inode_at` probes the overlay and the tree.
- T-26-SC: no package was added.

## Self-Check: PASSED

The gate log and this SUMMARY exist. Commits `cdcddc6` and `7b802bb` are ancestors of HEAD. The gate log has 7 `rc=0` lines, no `rc=[1-9]` line, and no home or user paths.
