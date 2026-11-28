---
phase: 26-prolly-maps-in-commit-read-and-gc
plan: 04
subsystem: metadata
tags: [prolly, manifest-data, overlay, commit, gc-mark, rehash, pmap-03, pmap-06]
requires: ["26-02", "26-03"]
provides:
  - "pmap.rs: TreeIo, TreeKey/TreeValue, ticketed Overlay, Tree (working root + generation), MapTree with the protocol as module doc"
  - "manifest_data as a prolly tree (key ino 8 bytes BE, value 33-byte manifest key) through overlay, flush, commit, load, GC mark, rehash and the bench"
  - "apply_lock + prepared: no flush applies between the cut and the install"
  - "Ref::Tree (tree nodes straight to the live set), store::tree_nodes"
  - "IN-01: next_ino read after the takes"
  - "rehash rebuilds the manifest tree (Kind::ManifestTree = 8, RehashOptions::tree_salt)"
  - "bench: is_tree, serialized_bytes over the flat fields, <field>_entries on the load line"
affects: [26-05, 26-06, 26-07, 26-08, 26-09, 26-14]
tech-stack:
  added: []
  patterns:
    - "overlay first, then the working root (never the root first)"
    - "install the root before the settle; an entry leaves the overlay only with its current ticket"
    - "hot-path flush: apply_lock.try_lock, skip while prepared"
key-files:
  created:
    - crates/metadata/src/pmap.rs
    - crates/metadata/tests/gc_prolly_tests.rs
    - .planning/phases/26-prolly-maps-in-commit-read-and-gc/runs/gate-26-04.log
  modified:
    - crates/metadata/Cargo.toml
    - crates/metadata/src/lib.rs
    - crates/metadata/src/store.rs
    - crates/metadata/src/store_io.rs
    - crates/metadata/src/gc/mod.rs
    - crates/metadata/src/rehash/mod.rs
    - crates/metadata/src/rehash/map.rs
    - crates/slicefs-cli/src/rehash.rs
    - crates/slicefs-cli/tests/hash_change_tests.rs
    - crates/slicefs-cli/tests/rehash_tests.rs
    - benchmarks/prolly-bench/src/main.rs
    - benchmarks/prolly-bench/src/whole.rs
    - Cargo.lock
key-decisions:
  - "Overlay flush at TREE_FLUSH = 16,384 dirty entries per tree (8 under cfg(test))"
  - "Trees reuse the lock Site of their map (Site::ManifestData for the overlay shards and the root lock); apply_lock is Rank::Store, untimed"
  - "StoreIo node cache default 64 MiB (NODE_CACHE_BYTES) until plan 26-06 adds the option; tree config Config::DEFAULT"
  - "manifest_digest keeps its Option signature and logs a read error"
  - "A tree is always rebuilt by rehash, also when every node already has the target tag"
status: complete
metrics:
  duration: "about 1 h 30 min"
  completed: 2026-11-28
actuals:
  tokens: 13400
  tasks: 3
  commits: 2
plan_head_before: 49aa2d1938c32352c7235e8ce75d57fbe4efc220
plan_head_after: 61c136e0b095d2caf7dcb51d6cd01fa5ebb9494b
---

# Phase 26 Plan 04: manifest_data as a prolly tree (tracer) Summary

The `manifest_data` map is now a prolly tree in the live store: writes go to a sharded, ticketed overlay, reads check the overlay and then the tree through the 64 MiB node cache, a commit applies only the taken edits to the cut root, the GC mark sends tree nodes straight to the live set, rehash rebuilds the tree under the target hash, and the bench `whole` mode reads a tree field. The other four maps stay flat.

## What was built

- `crates/metadata/src/pmap.rs` (new): `TreeIo` (chunk store, node cache, config; `ctx()`), `TreeKey` for `u64` (8 bytes big endian), `TreeValue` for `ObjectKey` (33 bytes, length checked), `Overlay` (32 shards, `Option<V>` edits so a delete is a tombstone until its settle, tickets as in `Live`), `Tree` (working root and generation; install sets the root, then the generation + 1), `MapTree` (`read`, `read_with_generation`, `write`, `dirty`, `flush`, `take_for_commit`, `apply_batch` returning the new root and `prolly::ApplyStats`, `install_and_settle`, `requeue`). The protocol is the module doc. Unit tests: overlay then tree, a change after the take survives the settle, requeue, generation, a bad stored value is corrupt.
- `store.rs`: field `manifest: MapTree<u64, ObjectKey>` replaces the `BTreeMap`; `trees: TreeIo`; `apply_lock` (Rank::Store, untimed) and `prepared` (`AtomicBool`, written under `apply_lock`); `TREE_FLUSH`; `flush_trees` on the hot path after `set_manifest_digest` and `delete_inode` (try_lock, skip while prepared, apply error kept in `put_error`, nodes written added to `unsynced_puts`). `begin_commit`: wait out a flush and set `prepared`, release, cut (a cut error ends `prepared`), take dirs, inodes and the manifest overlay, then read `next_ino` (IN-01). `finish_inner`: apply to the cut root, the root record holds the new tree root, `BeforeSync` fault point after the record put, then under `apply_lock` install, settle and end `prepared`. `Drop` of an uninstalled commit requeues the manifest overlay and ends `prepared`. `load_from_root` takes the field as the working root and reads no manifest. `Ref::Tree`, `walk_tree` (uncached `prolly::visit`, one path; leaf entries length-checked), `pub fn tree_nodes`; `referenced_roots` keeps its file-value contract; `referenced_estimate` counts the manifest tree with `prolly::count`. Lock-order comment extended.
- `store_io.rs`: `NODE_CACHE_BYTES` (64 MiB), `node_cache()`, `tree_config()`.
- `gc/mod.rs`: `mark` sends `Ref::Tree` to `add` at once (still counted for the stop check); doc updated. The streamed-mark unit test adds the tree nodes of each root.
- `tests/gc_prolly_tests.rs` (new): `manifest_tree_end_to_end` (3,000 files, height >= 2, commit, reload, 100 changes and 50 deletes, root B, full GC frees at least one node only A reached and keeps every node of B, reload B) and `an_empty_store_keeps_its_trees`.
- Rehash: `manifest_tree` rebuild (cursor over an uncached `Ctx`, manifest list translation per entry, `prolly::Builder` with `Config::DEFAULT.with_salt(tree_salt)`, dry run writes nothing and counts tree nodes of another tag), `RehashOptions::tree_salt`, `Kind::ManifestTree = 8` (4 retired with the flat manifest map), module doc. New test `a_manifest_tree_rehashes_under_the_target_tag` (1,500 files, height >= 2); `assert_one_tag`, `all_objects` and `live_ids` include `tree_nodes`. The CLI passes `tree_salt: None` in both places (plan 26-06 passes the header value).
- Bench: `is_tree` (`prolly::NODE_MAGIC`), `MAP_NAMES`, `serialized_bytes` over the root record and the flat fields; the `whole` load line adds `<field>_entries=` for each tree field (now `manifest_data_entries=`) and opens no tree field as a file value.

## Commits

- `d50f5cd` feat(26-04): Tasks 1-3 in one commit (rule 2: the rehash tests and the bench smoke are red until all three are in).
- `61c136e` chore(26-04): gate log.

`commits: 2` counts these two; the SUMMARY commit that follows is not in the count.

## Decisions

The discretion choices of the plan are in the frontmatter. Also:
- The overlay stores `Option<V>` per key, so a delete stays in the overlay as a tombstone until a root without the key is installed. `delete_inode` writes a delete for every inode, also one with no manifest (a delete of an absent key is a no-op in `apply`); this saves a tree read on each delete.
- A failed hot-path flush puts its error into the existing `put_error`, so the next commit fails the same way it does for a hot-path put error. The taken entries are requeued.
- Flush and commit both take entries only under `apply_lock` (or `prepared`), so a requeue of every in-flight entry is correct for both.
- `referenced_estimate` now counts 5 root-record keys (the manifest tree root goes to the live set, not to the sorter).
- Rehash keeps `Kind` 4 unused and adds `ManifestTree = 8`. The kind byte is only in rehash work files.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 - Blocking] `gen` is a reserved word in edition 2024**
- **Found during:** Task 1. The metadata crate is edition 2024. The tree generation is named `generation` and `read_with_gen` is `read_with_generation`. Same protocol.

**2. [Rule 3 - Blocking] The end-to-end test needs real content objects**
- **Found during:** Task 1. The GC mark reads every content key, so `ObjectKey::example` manifests made the mark fail with an unknown hash tag. The test stores one real value per manifest.

**3. [Rule 3 - Blocking] GC unit test**
- `gc::tests::the_streamed_mark_gives_the_live_set_of_the_key_lists` now expects the tree nodes of each root in the live set and in the add count. The file is in `files_modified`.

**4. [Files] `Cargo.lock`**
- One line (`prolly` in the deps of `metadata`) from the new path dependency. It is not in `files_modified`.

No change was made outside `files_modified` except `Cargo.lock`.

## Verification

`runs/gate-26-04.log`: 7 gates, all `rc=0`. These are `cargo test -p metadata`, `cargo test -p prolly-bench` (`smoke_end_to_end` ok), the slicefs-cli rehash, hash change, online GC and write log replay tests, `lock_stats_report` with `--features lock-stats`, clippy `-D warnings` for metadata and prolly-bench, `unsafe-count.sh` and `cargo test --workspace`. `retry_tests` did not fail. Earlier runs: `write_log_replay_tests` and `e2e_filesystem_tests` passed with no `lock rank violation`.

Acceptance greps: the `prolly` path dependency is in metadata's Cargo.toml once. No `manifest_data: sync::Mutex<BTreeMap` is left. `Ref::Tree` is in gc/mod.rs and `apply_lock` is in store.rs more than 3 times. `pub(crate) struct MapTree` is in pmap.rs once. `tree_salt` is in rehash/mod.rs 3 or more times and in the CLI rehash.rs 2 times. `Blob::ManifestMap` is gone. `fn is_tree` and `NODE_MAGIC` are in the bench main.rs.

## For later plans

- Plans 26-07 to 26-09 copy the manifest wiring for one map each. They add a `MapTree` field, add it to `take_for_commit`/`apply_batch`/`install_and_settle` in `begin_commit`/`finish_inner`, `Drop` and `flush_trees`, and add the field to `walk_tree` users (`for_each_referenced`, `tree_nodes`, `referenced_estimate`) and to the rehash `Field` list.
- `flush_trees` checks only the manifest tree; each new tree goes under the same one `apply_lock` hold.
- `apply_batch` returns `prolly::ApplyStats`, but `finish_inner` drops it today (`let (manifest_root, _)`). Plan 26-14 records it.
- `tree_salt` is `None` everywhere, and `StoreIo::tree_config()` is `Config::DEFAULT` with no salt. Plan 26-06 adds the header salt and the cache size option.
- `load_from_root` does not check that the tree field starts with `SPT1` (IN-02 is a later plan). A flat-map store from an earlier build fails at the first manifest read with a `manifest_data tree:` corrupt error.
- Integration tests build the library without `cfg(test)`, so `TREE_FLUSH` is 16,384 there: no slicefs-cli test and no `gc_prolly_tests` test reaches the hot-path flush. Only the unit tests in `store.rs` and `pmap.rs` do (limit 8). Crash windows (26-05) must write more than `TREE_FLUSH` entries, or get a test knob, to cover a flush.
- PMAP-03 and PMAP-06 are done for one map of five; the other four maps are still flat and still cloned in `begin_commit`.
- `manifest_digest` can now read a tree node under the fs namespace stripes (`reclaim_inode`); this is in its doc comment.

## Known Stubs

None. `tree_salt: None` in `crates/slicefs-cli/src/rehash.rs` is planned for 26-06; the comment names the plan.

## Threat Flags

None beyond the plan's register. T-26-09: tree errors map to `MetaError::Corrupted`/`Io`, with no unwrap on store data, and leaf entries are length-checked in `walk_tree` and rehash. T-26-10: covered by `manifest_tree_end_to_end`. T-26-11: covered by the ticketed overlay and by `a_flush_does_not_run_while_a_commit_is_prepared` and `a_change_after_the_take_survives_the_settle`. T-26-12: the lock order comment and the lock-stats gate.

## Self-Check: PASSED

The files `pmap.rs`, `gc_prolly_tests.rs` and `gate-26-04.log` exist. Commits d50f5cd and 61c136e are ancestors of HEAD.
