---
phase: 26-prolly-maps-in-commit-read-and-gc
plan: 15
subsystem: metadata
tags: [prolly, xattrs, bounded-memory, gc-mark, rehash, crash-tests, pmap-03, pmap-06, pmap-08]
requires: ["26-07"]
provides:
  - "xattr_data as a prolly tree, one entry per name: key ino (8 bytes BE) || name; value tag 0 + inline bytes, or tag 1 + 33-byte ObjectKey of a spilled value (FastCDC chunks + manifest)"
  - "xattr.rs: XattrKey, XattrValue, xattr_range, name_of, XATTR_NAME_MAX, check_name, XATTR_ENTRY_OVERHEAD, fits_inline, spill, value_len, read_range"
  - "pmap: TreeKey: Clone + Eq + Hash, shard_of(u64), MapTree::scan_keys (prefix range merged with the pending edits)"
  - "DictMetadataStore::{xattr_len, read_xattr, for_each_xattr_name} (inherent, pub), per-name set_xattr/remove_xattr, delete_xattrs in delete_inode"
  - "xattr tree in commit, load, flush_trees, requeue_all, TreeStats (xattr_data), GC walk (Ref::Tree nodes, Ref::Node spilled values), tree_nodes, referenced_estimate"
  - "rehash: xattr_tree rebuild, Kind::XattrTree = 10"
affects: [26-08, 26-09, 26-12, 26-16]
tech-stack:
  added: []
  patterns:
    - "A prefix scan of one inode on a map tree: copy the pending keys of the range under one overlay shard, then merge with a prolly::range cursor (MapTree::scan_keys)"
    - "A large value of a tree entry is stored like file content and read by range (FileReader::read)"
key-files:
  created:
    - crates/metadata/tests/xattr_prolly_tests.rs
    - crates/metadata/tests/xattr_memory_tests.rs
    - .planning/phases/26-prolly-maps-in-commit-read-and-gc/runs/gate-26-15.log
  modified:
    - crates/metadata/src/xattr.rs
    - crates/metadata/src/pmap.rs
    - crates/metadata/src/store.rs
    - crates/metadata/src/rehash/mod.rs
    - crates/metadata/src/rehash/map.rs
    - crates/metadata/tests/gc_prolly_tests.rs
    - crates/metadata/tests/commit_crash_tests.rs
    - docs/design/unsafe-review.md
key-decisions:
  - "Tags: 0 = inline, 1 = spilled (discretion choice of the plan)"
  - "Inline rule: 4 + 8 + name + 1 + value <= prolly::MAX_ENTRY (leaf entry header, ino, tag); depends only on the lengths"
  - "Names of 1 to 255 bytes (Linux XATTR_NAME_MAX); check_name runs in every xattr call, also the reads, so no key can exceed MAX_KEY"
  - "The xattr overlay shards by the ino (shard_of), so the names of one inode are in one shard; the pending copy of a scan holds keys only"
  - "A spilled value is metadata (Ref::Node), with no refcount row"
  - "get_xattr reads the entry once and then the value of that same entry (not xattr_len then read_xattr, which could straddle a replace)"
  - "Kind::XattrTree = 10 (next free number after RefcountTree = 9)"
status: complete
metrics:
  completed: 2026-11-28
actuals:
  tokens: 27700
  tasks: 3
  commits: 3
plan_head_before: 5a9611d134abc802009cd67e8a89acd33f676cf4
plan_head_after: ef5d1c56d2cf6bcd3c8c8c72c0b5e686a8b11850
---

# Phase 26 Plan 15: xattrs by name in the xattr_data prolly tree Summary

`xattr_data` is now a prolly tree with one entry per xattr name. A value that fits the leaf entry is stored inline (tag 0). A larger value is stored like file content (tag 1, chunks and a manifest) and is read by range. Every xattr call touches one entry or one range of chunks, and no call loads the xattr set of a file whole. Three of the five maps are now trees (manifest, refcounts, xattrs).

## What was built

- `xattr.rs` (rewritten): the module doc gives the key grammar, the tags, the inline rule and the memory bound. `XattrKey { ino, name }` (`TreeKey`: ino BE then the name; its shard is `shard_of(ino)`). `xattr_range`, `name_of`, `XattrValue { Inline, Spilled }` (`TreeValue`, tags 0 and 1, strict decode). `XATTR_NAME_MAX`, `check_name`, `XATTR_ENTRY_OVERHEAD`, `fits_inline`. `spill` uses `FileWriter`, `value_len` reads the manifest root only, and `read_range` uses `FileReader::read(offset, size)`. The set object and the list helpers are gone, and so are their tests.
- `pmap.rs`:
  - `TreeKey: Clone + Eq + Hash` (was `Copy`). The overlay now clones a key where it copied one.
  - `shard_of(u64)`.
  - `MapTree::scan_keys(io, shard, start, end, f)`. It copies the encoded keys of the range from the pending edits of one shard (keys only, with a put or delete flag), then merges that copy with a `prolly::range` cursor on the working root. A pending put gives its key once, also when the tree has it. A pending delete hides the tree key. `Break` stops the scan.
  - A "Scan" item in the protocol doc.
- `store.rs`:
  - The field `xattrs: MapTree<XattrKey, XattrValue>` (`Site::XattrData`) replaces the flat map, and the lock-order comment covers it.
  - `new` sets an empty root, and `load_from_root` takes the field as the working root.
  - `begin_commit` calls `take_for_commit`. `finish_inner` applies the tree before `AfterApply`, pushes `TreeStats` "xattr_data" (3 entries in `CommitStats.trees` now), and installs it under the same `apply_lock` hold.
  - `requeue_all` and `flush_trees` cover the tree (also in the early-return test).
  - `set_xattr` is a blind put. It spills with no lock held when the value does not fit.
  - `remove_xattr` reads the entry: `NotFound` if there is none, else a delete.
  - Inherent `pub` calls: `xattr_len`, `read_xattr(ino, name, offset, size)` and `for_each_xattr_name(ino, f)`.
  - `get_xattr` reads one entry, then its whole value. `list_xattrs` collects the scan.
  - `delete_inode` calls `delete_xattrs`: scans of at most `tree_flush` keys from a resume key (last key + `0x00`), the deletes written after each scan, then a flush.
  - GC: the `XATTR_DATA` key left the `Ref::Node` list and the flat-map walk. `for_each_referenced` walks the xattr tree after the refcount tree: the nodes as `Ref::Tree`, each spilled key as `Ref::Node` (`xattr_leaf` checks the key length and the tag). `tree_nodes` uses the same order (manifest, refcounts, xattrs). `referenced_estimate` starts at 3 and adds `prolly::count` of the xattr tree.
  - Removed: `change_xattrs` and the imports of the removed helpers.
- `rehash/mod.rs`, `rehash/map.rs`: `xattr_tree` rebuilds the tree in key order with an uncached cursor. An inline value is copied. A spilled value goes through `value("xattr value", ..)` (byte copy, `Kind::Value`, once per old key). The entries go into a `prolly::Builder` with the tree salt. A dry run writes nothing and counts the nodes of another tag. The `XATTR_DATA` field is no longer a flat value map. `Kind::XattrTree = 10`. The docs are updated.

## Tests

- Unit tests:
  - xattr codec (key, range, values including tag 2 and 32/34 key bytes, inline rule at the boundary, names, ranged reads);
  - `pending_edits_merge_into_scan_keys` (pmap);
  - `a_value_that_fits_is_inline_and_a_larger_one_spills`, `an_offset_read_reads_the_range` and `delete_inode_drops_pending_and_tree_xattrs` (store);
  - `an_xattr_tree_rehashes_with_its_spilled_values` (rehash).
  - The existing `test_xattr_*` and `test_delete_inode_drops_manifest_xattrs_and_dir_entries` pass.
  - The windowed-walk test now has spilled values and checks the order of the trees.
  - Both fault unit tests set an inline and a spilled xattr: the overlay is dirty again before the install, clean after it, and the values read back after the reload.
- `tests/xattr_prolly_tests.rs`: `many_xattrs_on_one_file` (20,000 names, crosses the 16,384 hot-path flush), `one_large_spilled_value` (3 MiB), `list_order_is_name_byte_order`, `remove_reports_a_missing_name`, `a_reused_inode_number_has_no_xattrs`, `xattrs_from_threads_lose_nothing`.
- `tests/gc_prolly_tests.rs`: `a_spilled_xattr_value_lives_while_referenced`. It covers a replaced value and a deleted inode.
- `tests/commit_crash_tests.rs`: `xattrs_survive_a_cut_commit` (all 5 fault points, `MemIo::crash()`, a reload to R1, then a good commit of the same edits, a crash and a reload to R2).
- `tests/xattr_memory_tests.rs`: `xattr_calls_do_not_grow_with_the_set`. Output: `xattr peak small=596366 large=1055907 set_small=840 set_large=840`. The large peak (about 1.0 MiB) is under 2 MiB, so the 4 MiB cap stays. The growth from small to large (about 460 KB) is the 1 MiB node cache: the whole small tree (about 440 KB of leaves) fits in it, and the large one fills it.

## Mutation checks

- Without `self.xattrs.requeue()` in `requeue_all`, `a_failed_commit_before_install_requeues_every_overlay` fails.
- With the spilled keys left out of the GC walk (`Some(_) => Ok(())`), `a_spilled_xattr_value_lives_while_referenced` fails ("kept while referenced").

## Decisions

- Tag 0 means inline and tag 1 means spilled. The inline rule is `4 + 8 + name + 1 + value <= MAX_ENTRY`. Names have 1 to 255 bytes. The overlay shard is that of the ino. The pending copy of a scan holds keys only. A spilled value is `Ref::Node` metadata. (These are the discretion choices of the plan, as written there.)
- `check_name` runs in the read calls too (`xattr_len`, `read_xattr`, `get_xattr`), not only in set and remove. An empty or over-long name gives `InvalidName` or `NameTooLong`. A name of 2,000 bytes can never become a key above `MAX_KEY`.
- `get_xattr` reads the entry once, then the value of that entry. Two lookups (`xattr_len`, then `read_xattr`) could straddle a replace.
- `Kind::XattrTree = 10`.

## Deviations from Plan

**1. Tasks 1 and 2 are one code commit**
- `gc_prolly_tests.rs` has both the Task 1 fix (below) and the Task 2 GC test. Rule 2 needs a workspace pass on each committed tree (the 26-05 precedent). The code commits are: Tasks 1+2, then Task 3, then the gate log.

**2. [Rule 3 - Blocking] `manifest_tree_end_to_end` (gc_prolly_tests) and the windowed-walk unit test**
- The empty xattr tree root is now a tree node of every root, so `tree_nodes` includes it. The GC test adds the xattr tree nodes to its expected set. The windowed-walk test no longer expects the `XATTR_DATA` key as a file value.

**3. The scan unit test is named `pending_edits_merge_into_scan_keys`, not `scan_keys_merges_pending_edits`**
- The acceptance grep `grep -c 'fn scan_keys' pmap.rs == 1` also counts a test named `scan_keys_...`. The name still matches the Task 1 verify filter `scan_keys`.

**4. Flush limits in tests**
- `xattrs_from_threads_lose_nothing` sets `test_set_tree_flush(16)` and `xattrs_survive_a_cut_commit` sets 64, so flushes run during the race and before the cut. The integration build flushes at 16,384.

**5. Gate commands**
- `cargo test -p metadata` and the CLI suites ran with `--no-fail-fast`. The CLI gate is the union of the Task 1 and Task 3 lists, without `integration_tests` (it ran in Task 1 and in the workspace run).

No file outside `files_modified` was changed.

## Verification

`runs/gate-26-15.log` has 7 sections, all `rc=0`, and all ran on the committed tree:

- `cargo test -p metadata`;
- the slicefs-cli suites (lib, rehash, hash_change, gc_online, write_log_replay, posix_compliance, dir_link, coverage_boost, e2e_filesystem);
- `lock_stats_report` with `--features lock-stats`;
- `prolly-bench`;
- clippy `-D warnings` for metadata;
- `unsafe-count.sh`;
- `cargo test --workspace`.

`retry_tests` did not fail. An earlier workspace run on the tree before the last test edits also passed.

The acceptance greps give the expected counts:

| Grep | Count |
|------|-------|
| the set object functions | 0 |
| `xattr_data: sync::Mutex<BTreeMap` | 0 |
| `fn change_xattrs` | 0 |
| `MapTree<XattrKey, XattrValue>` | 1 |
| the three bounded calls | 3 |
| `fn fits_inline` | 1 |
| `trait TreeKey: Copy` | 0 |
| `fn scan_keys` | 1 |
| `XattrTree` in map.rs | 1 |
| the rehash test | 1 |
| the six integration tests | 6 |
| the GC test | 1 |
| the crash test | 1 |
| the unsafe count line | 1 |

## Commits

- `3de779d` feat(26-15): xattrs by name in the xattr_data prolly tree (Tasks 1, 2)
- `43c9a43` test(26-15): xattr crash case and memory bound (Task 3)
- `ef5d1c5` chore(26-15): gate log

`commits: 3` counts the three commits from `5a9611d` to `ef5d1c5`. The docs commit of this SUMMARY is not in the count.

## Notes for 26-16 (fs layer) and 26-08 (inode tree), which run next in parallel

- **26-16:**
  - Use `xattr_len` (size query), `read_xattr(ino, name, offset, size)` (ERANGE checks, macOS position) and `for_each_xattr_name` (a streamed listxattr reply). Do not use `get_xattr` or `list_xattrs` on the hot path: they return a whole value or a whole list.
  - An absent name is `MetaError::NotFound(ino)`. `meta_error_to_errno` maps it to ENOENT, but getxattr and removexattr need ENODATA (ENOATTR on macOS). `test_getxattr` maps every error to ENODATA.
  - `NameTooLong` maps to ENAMETOOLONG and `InvalidName` (empty name) to EINVAL. Check what each xattr call must return for these, and Linux returns ERANGE for a name above 255 bytes.
  - Names list in byte order (they listed in insertion order before).
  - `set_xattr` is a blind put. The fs layer must keep the calls of one inode in order (its stripe). Create-only and replace-only (XATTR_CREATE/XATTR_REPLACE) can use `MapTree::modify` in the store if the fs needs them atomic.
  - `docs/design/store-format.md` is not updated by this plan (not in its files).
- **26-08 (inode tree):**
  - `TreeKey` is `Clone`, not `Copy`. Use `pmap::shard_of(ino)` for an inode key, so an inode key and the xattrs of that inode are in the same shard number.
  - Order of the trees in `tree_nodes` and `for_each_referenced`: manifest, refcounts, xattrs. Add the inode tree after them. The windowed-walk unit test and `manifest_tree_end_to_end` check this list, so extend both.
  - `referenced_estimate` starts at 3 now (root record, inode map, dir map), and becomes 2 when the inode map is a tree.
  - `CommitStats.trees` has 3 entries (`manifest_data`, `refcounts`, `xattr_data`).
  - `requeue_all`, `flush_trees` (the early-return test and an arm), `PreparedCommit`, `begin_commit` and `finish_inner` all list the xattr tree, so add the new tree beside it.
  - `MapTree::scan_keys` is generic. A directory tree (26-09) can use it for a range of one directory, if its keys shard by the 8-byte prefix.
  - Both fault unit tests in `store.rs` check each overlay. Add the inode overlay there.
  - `get_inode` is used by `xattrs_survive_a_cut_commit` (the xattr keys must be inodes). It keeps working as long as `get_inode` does.

## Known Stubs

None.

## Threat Flags

None beyond the plan's register. The register items are covered as follows:

- T-26-19: codec tests, `xattr_leaf`, `name_of`.
- T-26-20: `a_spilled_xattr_value_lives_while_referenced`, plus the mutation check.
- T-26-42: `xattr_calls_do_not_grow_with_the_set`.
- T-26-43: neighbor inodes in `many_xattrs_on_one_file` and `delete_inode_drops_pending_and_tree_xattrs`.
- T-26-44: `a_reused_inode_number_has_no_xattrs`.
- T-26-45: `flush_trees` covers the xattr tree, and `many_xattrs_on_one_file` crosses a hot-path flush.
- T-26-SC: no package was added.

## Self-Check: PASSED

The new test files, the gate log and this SUMMARY exist. Commits `3de779d`, `43c9a43` and `ef5d1c5` are ancestors of HEAD. The gate log has 7 `rc=0` lines, no `rc=[1-9]` line, and no home or user paths.
