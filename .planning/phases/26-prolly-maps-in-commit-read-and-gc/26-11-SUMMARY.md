---
phase: 26-prolly-maps-in-commit-read-and-gc
plan: 11
subsystem: metadata
status: complete
tags: [prolly, gc, scale, testkit, memory]
requires: ["26-10"]
provides:
  - "metadata::testkit::bulk_store (hidden test loader) and BulkSpec"
  - "five-tree counter test at 10,000 and 300,000 files; load counter"
  - "GC mark peak-heap test at 50,000 and 200,000 files"
  - "snapshot, online GC and snapshot walk cost tests on trees"
affects: ["26-13"]
tech-stack:
  added: []
  patterns: ["bulk stores through the store encoders", "one counting-allocator test per binary"]
key-files:
  created:
    - crates/metadata/src/testkit.rs
    - crates/metadata/tests/gc_prolly_memory_tests.rs
    - .planning/phases/26-prolly-maps-in-commit-read-and-gc/runs/gate-26-11.log
  modified:
    - crates/metadata/src/lib.rs
    - crates/metadata/tests/commit_counter_tests.rs
    - crates/metadata/tests/gc_prolly_tests.rs
    - docs/design/metadata-maps.md
    - docs/design/unsafe-review.md
key-decisions:
  - "Sizes 10,000 / 300,000 (counter) and 50,000 / 200,000 (memory); an ignored 1,000,000-file counter variant for release runs by hand"
  - "The memory test opens the chunk store with sort chunks of 4,096 keys, so the keys of both sizes spill and the fixed 4 MiB sort chunk does not hide growth"
  - "Absolute cap of the mark peak: 2 x the measured 50,000-file peak (2 x 2,086,967 bytes)"
  - "Total-work check of the five-tree test: slack of 2 nodes per tree (10), as the one-tree test has 2"
metrics:
  completed: 2026-11-28
actuals:
  tokens: 9500
  tasks: 2
  commits: 2
plan_head_before: 53eff1f808a3aa925b4426b25c376a3ce147179a
plan_head_after: c16a28ce161fd1db95aa6e0b740b36921fb78258
---

# Phase 26 Plan 11: Bulk store loader, five-tree counter, bounded GC mark Summary

A test-only bulk loader writes stores of up to 300,000 files through the
store encoders. Its root record is byte-equal to the record of the same
files made with the product calls. On these stores, tests show ROADMAP
criteria 2 and 3: one change in all five trees costs O(height) nodes per
tree, a load reads 3 tree nodes, and the heap of a full GC mark grows by
0.55 MB for 4 x the map.

## Commits

- 1a293fd feat(26-11): bulk store loader and the five-tree counter test
- c16a28c test(26-11): GC mark heap, snapshot and online GC on trees, walk cost

## What changed

- `crates/metadata/src/testkit.rs` (`#[doc(hidden)] pub mod testkit`):
  `BulkSpec { files, dirs, xattr_every, refcounts_per_file }` and
  `bulk_store(io, spec) -> Result<ObjectKey, MetaError>`. Each of the five
  trees comes from one stream of entries in key order into
  `prolly::Builder`, with the tree config (and salt) of the `StoreIo`.
  Keys and values go through `TreeKey`/`TreeValue`, `entry_key`/`DirValue`,
  `XattrKey`/`XattrValue`, the `InodeMeta` value (`serialize_inode`) and
  `intern_manifest`. The root record is filled at the `root_rec` offsets.
  There is one real manifest and one content object per 1,000 files,
  `.`/`..` in each directory, and refcount keys with a big-endian counter
  (tree order). Helpers for tests: `dir_ino`, `file_ino`, `parent_of`,
  `has_xattr`, `inode_meta`, `file_name`, `dir_name`, `group_of`,
  `content`, `xattr_value`, `refcount_key`.
- `commit_counter_tests.rs`:
  - `a_bulk_store_is_the_store_of_the_product_calls` (2,000 files): the
    bulk store loads, every file is listed, and the inode, manifest, xattr
    and refcount read back. The same files made with `create_directory`,
    `create_inode`, `link`, `set_manifest`, `set_xattr`,
    `increment_refcount` and `update_inode` give the same root record
    bytes (T-26-34).
  - `one_change_in_every_tree_grows_with_height`,
    `#[ignore] one_change_at_one_million_files`,
    `loading_a_large_store_reads_a_few_nodes`.
- `gc_prolly_memory_tests.rs`: one test with a counting `GlobalAlloc`. The
  sweep after each mark removes 0 objects, so the mark finds every object
  of the bulk store.
- `gc_prolly_tests.rs`: `a_node_shared_with_a_snapshot_survives_gc`,
  `online_gc_frees_unreached_tree_nodes`, `snapshot_roots_cost_one_walk_each`.
- `docs/design/metadata-maps.md` ("GC mark"): a table of the measured walk
  cost and mark peaks, and the sentence that the full walk per root is the
  accepted limit.
- `docs/design/unsafe-review.md`: the entry and
  `- count: crates/metadata/tests/gc_prolly_memory_tests.rs 9`.

## Measured numbers (debug build, macOS)

One change in every tree, then a commit (h = tree height; nodes read and written):

| Tree | 10,000 files: h / read / written | 300,000 files: h / read / written |
|------|------|------|
| manifest_data | 2 / 3 / 2 | 3 / 6 / 3 |
| refcounts | 3 / 6 / 3 | 3 / 6 / 3 |
| xattr_data | 2 / 3 / 2 | 3 / 6 / 3 |
| inode_data | 2 / 3 / 2 | 3 / 6 / 3 |
| dir_data | 2 / 3 / 2 | 3 / 6 / 3 |
| Total read + written | 29 (sum of heights 11) | 45 (sum of heights 15) |

- Load of the 300,000-file store on a new node cache: 3 misses, 0 hits.
  The highest tree is 3, so the limit is 5 x (3 + 1) = 20. With the 5
  direct root checks of the load added, the count is 8.
- Bulk store build: 2,000 files 0.1 s, 10,000 files 0.2 s, 300,000 files
  7.4 to 7.6 s.
- GC mark peak heap above its start: 50,000 files 2,086,967 bytes,
  200,000 files 2,641,095 bytes (sort chunks of 4,096 keys). With the
  default options (a 4 MiB sort chunk in RAM) the peaks were 8,810,710 and
  9,284,135 bytes.
- Snapshot walk, 3,000 files: 1 root visits 89 tree nodes; 1 root and 3
  snapshots visit 356 nodes, of which 95 are distinct.
- Test times: `commit_counter_tests` 8 to 25 s, `gc_prolly_memory_tests`
  about 35 s, `gc_prolly_tests` about 12 s.

## Deviations from Plan

1. **[Rule 1 - Bug in the plan check] Total-work slack of the five-tree
   test.** The plan check `total(300k) <= total(10k) x heights ratio + 5`
   fails on correct numbers: 45 > 29 x 15 / 11 + 5 = 44.5. The reads of a
   tree are about 3(h - 1), not a fixed share of h, so the ratio check
   needs slack for each tree. The slack is 2 nodes per tree (10), the same
   as the 2 of the existing one-tree test. The per-tree caps (2h + 1 and
   3h) do not change. Commit 1a293fd.
2. **[Rule 2] Format check of the loader.** Added
   `a_bulk_store_is_the_store_of_the_product_calls`. It does the
   2,000-file read-back of the plan, and it also compares the root record
   bytes with the same files made with the product calls (the T-26-34
   mitigation).
3. **TDD order.** Task 1 is `tdd="true"`, but rule 2 allows code commits
   only on a green workspace. Thus the tests and the code went into one
   commit, after the workspace run.
4. **Memory test options.** With the default options, most of the
   50,000-file peak (8.8 MB) is the fixed in-RAM sort chunk. The test opens
   the chunk store with `gc_sort_chunk_ids: 4_096`, so both sizes spill to
   temp files and a part that grows with the map becomes visible.

Not done: the "Scale at 1,000,000,000 inodes" table is in
`docs/design/store-format.md`, which is outside `files_modified`. It was
not measured, so the Phase 25 note stays. The ignored 1,000,000-file
counter test was not run.

## Gates

`runs/gate-26-11.log`: `unsafe-count.sh`, `cargo test -p metadata`, the
CLI GC suites (`gc_online_tests`, `snapshot_switch_gc_tests`,
`gc_sweep_tests`, `gc_cli_tests`), clippy of `metadata` and
`cargo test --workspace`: all `rc=0`. No `retry_tests` failure.

## Notes for 26-13

- `metadata::testkit::bulk_store(&io, &BulkSpec { .. })` gives a root
  key. Open it with `DictMetadataStore::load_from_root(io, &root)`. In a
  debug build it takes about 25 microseconds per file (300,000 files in
  7.5 s). It syncs the chunk store once at the end.
- The bulk inode records have uid 0, gid 0 and times 0. File `i` is inode
  `dirs + 2 + i`, with the name `f{i:010}` in directory `2 + i % dirs`
  (`spec.parent_of(i)`).
- A bulk store has no WAL and no snapshots. The first commit after a load
  writes only the changed paths (the counts above).
- A GC right after `bulk_store` removes 0 objects.

## Known Stubs

None.

## Threat Flags

None (test code and docs only; no new product surface).

## Self-Check: PASSED

- Files exist: `crates/metadata/src/testkit.rs`,
  `crates/metadata/tests/gc_prolly_memory_tests.rs`, `runs/gate-26-11.log`.
- Commits 1a293fd and c16a28c are on `exec/26-11`.
