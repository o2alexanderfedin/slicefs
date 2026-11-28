---
phase: 26-prolly-maps-in-commit-read-and-gc
plan: 09
subsystem: metadata, slicefs-cli
tags: [prolly, directories, readdir, bounded-memory, gc-mark, rehash, pmap-03, pmap-06]
requires: ["26-08", "26-16"]
provides:
  - "dir_data as a prolly tree, one entry per name: key dir ino (8 bytes BE) || name; value child ino (8 bytes LE) || type byte (DT_* = (mode & S_IFMT) >> 12, 0 if no inode)"
  - "dirent.rs: DirKey, DirValue, entry_key, dir_range, name_of, kind_of, KIND_DIR"
  - "store: dirs: MapTree<DirKey, DirValue> (Site::DirLive shards, Site::DirData root); pub for_each_dir_entry (streamed), pub dir_is_empty"
  - "dir tree in commit (TreeStats dir_data, 5 entries in CommitStats.trees), load, flush_trees, requeue_all, GC walk (Ref::Tree), tree_nodes, referenced_estimate (starts at 1)"
  - "rehash: copy_tree (inode and dir trees), Kind::DirTree = 12; Kind::ValueMap = 3 retired"
  - "fs layer: SliceFsFilesystem::readdir_entries (streamed readdir from an entry index); rmdir and rename use dir_is_empty"
  - "mt-linux.sh steps readdir-many, rmdir-notempty, rmdir"
affects: [26-10, 26-12]
tech-stack:
  added: []
  patterns:
    - "Streamed readdir: the FUSE reply is filled from the store scan (ControlFlow::Break when reply.add reports full)"
    - "Value scan without a pmap change: scan_keys, then one read per key with no lock held (a key removed meanwhile is skipped)"
key-files:
  created:
    - crates/metadata/src/dirent.rs
    - crates/metadata/tests/dir_prolly_tests.rs
    - .planning/phases/26-prolly-maps-in-commit-read-and-gc/runs/gate-26-09.log
  modified:
    - crates/metadata/src/store.rs
    - crates/metadata/src/lib.rs
    - crates/metadata/src/rehash/mod.rs
    - crates/metadata/src/rehash/map.rs
    - crates/metadata/tests/gc_prolly_tests.rs
    - crates/slicefs-cli/src/filesystem.rs
    - crates/slicefs-cli/src/fuse_callbacks.rs
    - crates/slicefs-cli/tests/gc_online_tests.rs
    - scripts/docker-linux-test/mt-linux.sh
  deleted:
    - crates/metadata/src/directory.rs
key-decisions:
  - "The dir overlay is MapTree<DirKey, DirValue> sharded by shard_of(dir ino): per-name pending edits, so one directory with many new names flushes at TREE_FLUSH"
  - "Overlay shards timed Site::DirLive, root lock Site::DirData (the lock-stats report expects both labels)"
  - "The type byte is the Linux DT_* number, (mode & S_IFMT) >> 12; 0 when the child has no inode at link time"
  - "Paged readdir by name cookie is not built (stretch item); readdir streams from an entry index, and list_directory keeps returning a Vec"
  - "Values in a scan come from scan_keys plus one read per key; pmap.rs is unchanged"
  - "link, unlink and create_directory refuse . and .. (InvalidName), as DirList did; an empty name is InvalidName at link and mkdir, NotFound at lookup and unlink"
  - "delete_inode scans the dir range only for a directory (the mode comes from the inode read-modify-write)"
  - "CommitStats.trees order: manifest_data, refcounts, xattr_data, inode_data, dir_data; tree_nodes and for_each_referenced use the same order"
status: complete
metrics:
  completed: 2026-11-28
actuals:
  tokens: 35500
  tasks: 2
  commits: 2
plan_head_before: 7d38a5f138af8caa6d15f9db072c2db96ef74f14
plan_head_after: 1672e509b7aaf39ae9b6c7421948903c04cf5b86
---

# Phase 26 Plan 09: directory entries by name in the dir_data prolly tree Summary

`dir_data` is now a prolly tree with one entry per name. The key is the directory ino (8 bytes, big endian) followed by the name bytes. The value is the child ino (8 bytes, little endian) and one type byte (the Linux `DT_*` number). Every directory stores `.` and `..`. A readdir is a range scan merged with the pending edits, in name byte order. An emptiness check stops at the first name other than `.` and `..`. All five metadata maps are prolly trees now (PMAP-03). No flat map, no per-directory list object and no directory cache is left.

## What was built

- `dirent.rs` (new, private module):
  - `DirKey { dir, name }`: a `TreeKey` sharded by `shard_of(dir)`, so all names of one directory are in one overlay shard.
  - `DirValue { ino, kind }`: 9 bytes. A decode of any other length is an error, which becomes `Corrupted`.
  - `entry_key`, `dir_range` (`dir BE .. (dir + 1) BE`, no end for `u64::MAX`), `name_of` (`Corrupted` on a short key or bad UTF-8), `kind_of`, `KIND_DIR`.
- `store.rs`:
  - The field `dirs: MapTree<DirKey, DirValue>` (`with_sites("dir_data", Site::DirLive, Site::DirData, ..)`) replaces the directory cache, the dir digest map and its helpers. The following are removed: `Live`, `Sharded`, `Shard`, `DIRTY_FLUSH`, `DIR_CACHE_MAX`, `MAP_ENTRY`, `for_each_map_value`, `intern_u64_digest_map`, `load_u64_digest_map`, `map_size`, `map_decode_tests`, `cached_dir`, `with_dir` and `flush_dirs`.
  - `is_dir` is one read of `ino || "."`. `check_dir` and `missing_entry` give `NotADirectory` or `NotFound(0)`, with the `.` probe only on a miss.
  - `lookup`: one read. A name above 255 bytes is `NameTooLong`.
  - `link`: `check_name` (now also refuses an empty name) and refuses `.` and `..`. The parent must be a directory. The child mode is read before the read-modify-write. `add_entry` is an insert-if-absent read-modify-write, and a taken name (pending or in the tree) is `AlreadyExists(ino)`.
  - `unlink`: refuses `.` and `..`. It is a read-modify-write to a pending delete, and an absent name is `NotFound(0)` (`NotADirectory` for a file).
  - `set_parent`: a read-modify-write of `..`. An absent `..` is `NotADirectory`.
  - `make_directory`: the inode first, then `.` and `..`, then the name in the parent (a lookup that finds the name finds a directory). On `AlreadyExists`, `delete_inode` removes the inode and its entries again.
  - `for_each_dir_entry(ino, f)` (pub) is a streamed scan (`scan_keys`), with each entry read with no lock held. `list_directory` and `dir_is_empty` (pub) are built on it.
  - `delete_inode`: for a directory, `delete_dir_entries` sends deletes in scans of `tree_flush` keys, with a flush between them (the `delete_xattrs` scheme).
  - `new`: the root `.` and `..` -> 1 go into the overlay.
  - Commit wiring: `PreparedCommit` has `dir_root` and `dirs`. `begin_commit` takes the dir overlay after the inode overlay and before the `next_ino` read. `finish_inner` applies the tree, pushes `TreeStats` "dir_data" and installs the tree under the same `apply_lock` hold. `requeue_all` and `flush_trees` (the early-return test and an arm) cover it. `load_from_root` takes the field as the working root. The lock-order comment covers the dir overlay.
  - GC: `for_each_referenced` gives only the root record as `Ref::Node`, then walks the dir tree last (`Ref::Tree`; `dir_leaf` checks a key of at least 9 bytes and a 9-byte value). `tree_nodes` has the same order. `referenced_estimate` starts at 1 and no longer opens `DIR_DATA` as a file.
- `rehash`:
  - `copy_tree(kind, what, ok, old)` replaces `inode_tree`. It rebuilds the inode tree (`Kind::InodeTree`) and the dir tree (`Kind::DirTree = 12`) with identity values.
  - `Blob` and `Kind::ValueMap` are gone. Number 3 is now marked "not used".
  - `manifest_list` is the old `blob` with only the manifest-list path left. The docs are updated.
- `filesystem.rs`:
  - rmdir and `rename_held` call `dir_is_empty`.
  - `readdir_entries(ino, offset, f)` streams the entries from an entry index and gives each entry with its next offset. The type comes from the stored byte (`dt_file_type`); for a byte of 0 the inode is read.
  - `test_readdir` builds its `Vec` from `readdir_entries`.
- `fuse_callbacks.rs`: the `readdir` callback fills the reply inside `serve` from `readdir_entries` and stops when `reply.add` reports full. The whole directory is never listed into RAM.
- `mt-linux.sh` (Docker smoke):
  - `readdir-many`: 3,000 files in one directory. `ls -f` gives 3,002 entries and `ls` gives 3,000 unique names, so the kernel reads many readdir replies.
  - `rmdir-notempty`: rmdir must fail while the directory has names.
  - `rmdir`: remove all names, then rmdir.

## Tests

- New store unit tests:
  - `listing_merges_pending_edits`: committed a, b, c, then `unlink(b)` and `link(d)` pending. The listing is `. .. a c d`, also after a flush and after a reload.
  - `dot_and_dotdot_are_entries`: `.` and `..` of a new directory, the root's `..` is 1, `set_parent`, `is_dir`, `set_parent` of a file, and `InvalidName` for `.` and `..` in link and unlink. All are checked again after a reload.
  - `link_refuses_a_taken_name_and_unlink_a_missing_one`: pending only, tree only (overlay empty), a pending delete, and `NotADirectory` for a file parent in lookup, unlink, link, list and `dir_is_empty`.
  - `the_entry_type_is_the_child_mode`: file 8, directory 4, symlink 10, no inode 0, from the overlay and from the tree.
  - `deleting_a_directory_deletes_its_entries`: an empty directory and a 32-name one (committed and pending, past `TREE_FLUSH` = 8). After the commit the tree range is empty, and the other directory keeps its entries.
  - `an_empty_name_is_refused`: empty-name errors, a 255-byte name links and looks up across a reload, and 256 bytes is `NameTooLong(256)`.
  - `dir_is_empty_stops_at_the_first_other_name`: the names `+x`, `-a` and `!` sort before `.`, and a pending delete hides a tree name.
- `dirent.rs` unit tests: key order, the value length check, and the type bytes.
- Ported tests:
  - `cached_dirs_and_inodes_survive_eviction_commit_and_reload` is now `dirs_and_inodes_survive_flushes_commit_and_reload`. It bounds the dir overlay by `TREE_FLUSH`, because there is no cache to evict.
  - `cold_dir_load_does_not_block_other_inodes` is now `cold_dir_read_does_not_block_other_inodes`. 2,000 names in `a` give a dir tree of at least two levels. The reload uses a fresh `StoreIo`, which has an empty node cache. A held pack read in a lookup in `a` does not block B (a cached inode change, a create, and a lookup in `b`).
  - The windowed-walk test: no flat map is left. The dir tree comes last in the tree nodes, with 42 entries. `dir_leaf` checks.
  - Both fault tests check the dir overlay. It is dirty again before the install and clean after it.
- `tests/dir_prolly_tests.rs`:
  - `a_large_directory`: 20,000 names, past `TREE_FLUSH` = 16,384 in this build. The listing has 20,002 names in byte order, 100 seeded lookups hit, and the directory is not empty. All of this is checked before and after a reload. Then every name is unlinked and deleted, `dir_is_empty` is true, the rmdir path runs, and a reload follows.
  - `hard_links_are_separate_entries`.
  - `rename_across_directories_moves_dotdot`.
- `filesystem.rs`: `readdir_streams_from_an_offset_until_the_reply_is_full` reads pages of two, each resumed at its last offset, and the pages equal the full listing.
- `gc_prolly_tests.rs` `manifest_tree_end_to_end`: the dir tree nodes are in the expected `tree_nodes` set.

## Decisions

- The dir overlay is `MapTree<DirKey, DirValue>`, sharded by the directory ino. The shards are timed as `Site::DirLive` and the root as `Site::DirData`. The type byte is the `DT_*` number. Paged readdir by name cookie is not built. (These are the discretion choices of the plan, as written there.)
- Readdir streams from an entry index (this is from the orchestrator's binding note). It is not the paged-readdir stretch item: offsets are still indexes, and each call scans and skips the entries before its offset.
- Values come from `scan_keys` plus one read per key, so `pmap.rs` (not in `files_modified`) is unchanged.
- `.` and `..` are refused in `link`, `unlink` and `create_directory`. Without this, `unlink(dir, ".")` would turn a directory into a non-directory.
- `make_directory` writes `.` and `..` before the name in the parent. The plan said "as today, then `.`/`..`". The new order means a lookup never finds a directory without its `.` entry.
- `CommitStats.trees` has 5 entries: manifest_data, refcounts, xattr_data, inode_data, dir_data.
- The dir value has no ino check against the child inode, and the type byte is not updated after the link. A mode's type bits never change: setattr keeps `S_IFMT`.

## Deviations from Plan

**1. Tasks 1 and 2 are one code commit.** After Task 1 alone, the fs layer and the gates are not done. This follows the 26-08 and 26-15 precedent. The commits are the code (Tasks 1 and 2), then the gate log.

**2. Streamed readdir (orchestrator note) touches `fuse_callbacks.rs`.** That file is not in `files_modified`. The callback no longer uses `handle_readdir` and `ReaddirResult`, so those two imports were removed. `handlers::handle_readdir` stays for `mount.rs` and the test suites.

**3. [Rule 3, blocking] `crates/metadata/tests/gc_prolly_tests.rs`.** A `DIR_DATA` offset constant was added, and the dir tree nodes were added to the expected `tree_nodes` (the same change 26-08 made for inodes).

**4. [Rule 1, test] `crates/slicefs-cli/tests/gc_online_tests.rs` `reads_during_rewrite_and_gc_never_fail`.** This is outside the "order change" allowance of step 3, so here is the evidence:
- At a machine load average of 18 to 30 (other agents were running), the whole `gc_online_tests` binary failed about half the time. One GC ran in the 1.5 s window and removed 0 objects.
- A probe ran one GC right after the setup commit, on a copy of the base tree (7d38a5f, `git archive` into the scratch folder, with its own target directory). On base it removed 2 objects that the old flat directory map and lists left from the store setup. On this tree it removed 0.
- With the probe taking those leftovers first, the base tree also failed under the same load (`GCs 1, removed 0`).
- So the old test passed on setup garbage and not on "old roots' objects while reads ran". The product behaves the same.
- The fix: the writer and the reader run until the GC thread stops. The GC thread runs for at least 1.5 s and until `removed > 0`, with a hard cap of 30 s. The assertion is unchanged. With no load the behavior is unchanged.
- After the fix, the whole binary passed 6 of 6 runs under load, then passed again in the gate run.

**5. `mt-linux.sh` (not in `files_modified`).** It has three new Docker steps for the streamed readdir and rmdir through the kernel. Docker `run.sh all` was added as a gate. No host mount was used.

**6. Gate commands.** `cargo test -p metadata` and the CLI suites ran with `--no-fail-fast`. `lock_stats_report` ran with `--nocapture`: `dir_live n=82` and `dir_data n=13`. The Docker section of the log lists the mount-step lines of the full output before its last 40 lines.

**7. Clippy `enum_variant_names`.** The local `Field` enum of `rehash::root` has short variant names (`Manifest`, ..., `Dir`). This was found by the first gate run, which was not committed.

## Verification

`runs/gate-26-09.log` has 7 sections and 7 `rc=0` lines, with no `rc=[1-9]` line. All ran on the tree of `ada8976`:

- `cargo test -p metadata` (lib: 197 passed);
- the CLI suites: dir_link, rename_ancestor, posix_compliance, e2e_filesystem, integration, rehash, hash_change, gc_online, write_log_replay, snapshot_switch_gc;
- `lock_stats_report` with `--features lock-stats`;
- `prolly-bench`;
- clippy `-D warnings` for metadata and slicefs-cli;
- `cargo test --workspace` (92 test binaries, all ok; `retry_tests` did not fail);
- Docker `run.sh all`: the Linux workspace, the fuser lib and the mount smoke. Every step is ok, including readdir-many, rmdir-notempty and rmdir.

Plan acceptance greps:

| Grep | Count | Expected |
|------|-------|----------|
| `pub(crate) fn entry_key` in dirent.rs | 1 | 1 |
| `DirList`, `dir_live`, `DIR_CACHE_MAX`, `fn cached_dir`, `fn with_dir` or `fn flush_dirs` in store.rs | 0 | 0 |
| `pub fn dir_is_empty` in store.rs | 1 | 1 |
| `dir_is_empty` in filesystem.rs | 2 | at least 2 |
| `Check emptiness: list_directory` in filesystem.rs | 0 | 0 |
| directory.rs exists | no (exists=1) | exists=1 |
| `^rc=0$` in the gate log | 7 | at least 6 |
| `^rc=[1-9]` in the gate log | 0 | 0 |

Key link note: `list_directory` and `dir_is_empty` reach `prolly::range` through `MapTree::scan_keys` (pmap.rs). `store.rs` itself names `prolly::range` only in a test helper.

## Commits

- `ada8976` feat(26-09): directory entries by name in the dir_data prolly tree (Tasks 1, 2)
- `1672e50` chore(26-09): gate log

`commits: 2` counts the commits from `7d38a5f` to `1672e50`. The docs commit of this SUMMARY is not in the count.

## Notes for 26-10

- All five maps are trees. `root_rec` still has the same layout (197 bytes), and `DIR_DATA` is a tree root now. `referenced_roots` gives only the root record, the manifests and the spilled xattr values.
- `docs/design/store-format.md` still describes `dir_data` as `ino -> 33-byte ObjectKey`, and it does not describe the xattr tree. The doc comments of `Site::DirLive` and `Site::DirData` in `crates/chunk-store/src/lock_stats.rs` still name the old directory cache. None of these files were in this plan.
- Readdir offsets are entry indexes. Each readdir call scans from the start of the directory and reads the value of each skipped entry, so a full listing through the kernel costs O(n^2 / page) entry reads, but bounded RAM. Two cheaper fixes:
  - Move the skip before the value read (an index-aware `for_each_dir_entry`).
  - Use a name cookie (paged readdir, the stretch item).
- `stats.rs` `content_stats` still calls `list_directory` (one `Vec` per directory, pre-existing). It could stream with `for_each_dir_entry`.
- Test hooks `test_fail_next_flush` and `test_set_flush_hook` still act on the manifest tree only.
- `reads_during_rewrite_and_gc_never_fail` now waits for a GC that removes objects. Other timed tests that passed on leftover setup objects may show up under load. A GC right after store creation now finds 0 objects to remove.

## Known Stubs

None.

## Threat Flags

None beyond the plan's register:

- T-26-25: the fixed 8-byte prefix; an empty name is refused (`an_empty_name_is_refused`); `NAME_MAX` 255 gives a key of at most 263 bytes.
- T-26-26: `..` is a stored entry, updated by the `set_parent` read-modify-write. Covered by `rename_ancestor_tests` (7 passed), `rename_across_directories_moves_dotdot` and `dot_and_dotdot_are_entries`.
- T-26-27: the per-name overlay with flushes at `TREE_FLUSH`. Emptiness reads a few entries. Readdir streams into the reply. `list_directory`'s `Vec` is the pre-existing API.
- T-26-28: the 9-byte length check (`DirValue::decode`, `dir_leaf`, the rehash `dir_ok`). A bad key or bad UTF-8 is `Corrupted`.
- T-26-SC: no package was added.

## Self-Check: PASSED

`dirent.rs`, `dir_prolly_tests.rs`, the gate log and this SUMMARY exist, and `directory.rs` is gone. Commits `ada8976` and `1672e50` are ancestors of HEAD. The gate log has 7 `rc=0` lines, no `rc=[1-9]` line, and no home, user or scratch paths.
