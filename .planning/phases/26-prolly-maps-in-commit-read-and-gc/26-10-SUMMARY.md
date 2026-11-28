---
phase: 26-prolly-maps-in-commit-read-and-gc
plan: 10
subsystem: metadata, slicefs-cli, docs
tags: [prolly, store-format, in-02, m19, docs, pmap-03, pmap-08]
requires: ["26-09"]
provides:
  - "metadata::store::FLAT_MAPS_REFUSED; load_from_root checks that the root object of each of the five map fields starts with prolly::NODE_MAGIC (one verified read per field)"
  - "header::msg_interim_flat_maps (M19); mount::load_error maps the refusal to M19 in load_store, load_store_read_only and the snapshot mount"
  - "store-format.md: final key grammar and value encodings of the five trees, readdir order, root record fields as tree roots, M19 row"
  - "metadata-maps.md: the tree protocol as built (overlay, flush, commit, failure rules, node cache, readdir cost, xattr flag gap, GC mark, rehash)"
  - "limits.md matches the code (Phase 26 items are true now)"
affects: [26-11, 26-12]
tech-stack:
  added: []
  patterns:
    - "Format refusal by magic: a load checks the first object of each root record field before any tree read"
key-files:
  created:
    - .planning/phases/26-prolly-maps-in-commit-read-and-gc/runs/gate-26-10.log
  modified:
    - crates/metadata/src/store.rs
    - crates/slicefs-cli/src/header.rs
    - crates/slicefs-cli/src/mount.rs
    - crates/slicefs-cli/tests/store_header_tests.rs
    - crates/prolly/src/lib.rs
    - crates/chunk-store/src/lock_stats.rs
    - docs/design/store-format.md
    - docs/design/metadata-maps.md
    - docs/design/limits.md
key-decisions:
  - "The refusal reads the first object of each of the five fields (five small reads at mount, O(1)), before the root inode read"
  - "M19 is the next free message number"
  - "An empty flat map (a value of 0 bytes) is refused with the same FLAT_MAPS_REFUSED prefix (no slice of 4 bytes is assumed)"
  - "A missing or damaged field object keeps the normal Corrupted error (node_err), not M19"
  - "Only the three mount paths map to M19; stats, scrub, gc and the snapshot commands keep 'failed to reconstruct metadata store: root record: the metadata maps are not prolly trees: ...'"
status: complete
metrics:
  completed: 2026-11-27
actuals:
  tokens: 15100
  tasks: 3
  commits: 5
plan_head_before: 799349c6da1446b0006ef66ac2c08c47b7137962
plan_head_after: 5ef08c9e0488cb8f0b90011a8976b20393c3b264
---

# Phase 26 Plan 10: final format docs and the M19 refusal of interim flat-map stores Summary

A store with a format 8 header whose root record fields are flat maps (a store of the interim build) is now refused at mount with M19. Nothing is written to its segments or packs (a mount with `--hash` records that hash in `FORMAT` first, as for any store). `load_from_root` reads the root object of each of the five fields. Each one must start with `SPT1`, else the result is `MetaError::Corrupted` that starts with `FLAT_MAPS_REFUSED`. `docs/design/store-format.md` now has the final key grammar and value encodings of the five trees and the M19 row. `docs/design/metadata-maps.md` and `docs/design/limits.md` describe the trees as built. Docker `all`, pjdfstest with 4 workers and `verify` pass on prolly stores (ROADMAP criterion 1).

## What was built

- **Task 1 (flat code):** already done by 26-04 to 26-09. The helpers named in the plan, `MAP_ENTRY`, `map_decode_tests`, `Blob` and the flat `Kind` variants (3 to 6 are marked "not used") were gone at the base. `put_digest` and `for_each_window` are still used (the root record write and `for_each_manifest_key`), so they stay. Only the store module doc still described in-RAM maps. It now says that the five maps are prolly trees with overlays. Two stale comments ("every inode object", "one inode map") are fixed.
- **Task 2 (IN-02):**
  - `store.rs`:
    - `pub const FLAT_MAPS_REFUSED`.
    - `check_tree_fields` runs in `load_from_root` after `check_root_len` and before the root inode read. It does one verified `ChunkStore::read` per field and requires `prolly::NODE_MAGIC`. The error is `"{FLAT_MAPS_REFUSED}: field <name> starts with \"<hex>\""`.
  - `header.rs`: `pub fn msg_interim_flat_maps` (M19), one `format!` literal.
  - `mount.rs`: `load_error(store_path, what, e)` gives M19 for that error, and `what: <error>` for every other error (the old text). `load_store_with`, `load_store_read_only_with` and the snapshot load in `start` use it.
- **Task 3 (docs):**
  - `store-format.md`:
    - Layout: the key grammar table, the inode record byte table (56 or 72 bytes, no tag), the `dir_data` entries, and the `xattr_data` entries.
      - `dir_data`: the 9-byte value, the type byte `(mode & S_IFMT) >> 12` (0 when the child had no inode), `.` and `..`, hard links, name byte order, and node boundaries set by the names.
      - `xattr_data`: tag 0 inline or tag 1 spilled, the inline rule, and listxattr by range.
    - The "room for a name" paragraph is replaced: entries are stored by name.
    - Root record: the fields are tree roots, the magic check, and M19.
    - "This build and the prolly maps": the trees are used in commit, read, GC and rehash; the overlay and `--meta-cache-size`.
    - Messages: the M19 row, with the same text as the literal.
  - `metadata-maps.md`: rewritten for the trees. It covers:
    - the five trees, the overlay and working root, the flush at `TREE_FLUSH`, and the node cache;
    - the commit steps and the failure rules;
    - readdir, with its known O(n^2 / page) cost (offsets are entry indexes; paged readdir by name cookie is later work);
    - xattrs, with the known `XATTR_CREATE`/`XATTR_REPLACE` gap;
    - the GC mark (tree nodes straight to the live set, manifests and spilled values through the external sorter, every snapshot walked in full) and rehash by rebuild;
    - the O(files) memory of `slicefs stats`, which was there before this phase.
    - The Phase 24 decision text is kept and marked as history.
  - `limits.md`: the "Phase 26" marks and the "Before Phase 26" sentences are gone. The inode record is 56 or 72 bytes (it said about 100), and the dir value is 9 bytes. There are new rows for the xattr name limit, the readdir cost and the xattr flag gap.
  - `prolly/src/lib.rs` "# Status": SliceFS keeps its five maps in this crate's trees.
  - `lock_stats.rs`: the `Site` docs give the overlay shards (`InodeLive`, `DirLive`) and root locks (`InodeData`, `DirData`), and one site for both on `ManifestData`, `XattrData` and `Refcounts`. The labels are unchanged.

## Tests

- `an_interim_flat_map_root_is_refused` (store unit test):
  - All five fields flat is refused, and the error names `inode_data`, the first field checked.
  - Each single flat field with four tree fields is refused and names that field.
  - An empty flat map (0-byte value) in `dir_data` is refused.
  - The real root still loads.
- `root_record_layout`: after a commit, every field object starts with `NODE_MAGIC`.
- `a_230_byte_root_record_is_refused`: unchanged, passes.
- `an_interim_flat_map_store_is_refused_with_m19` (store_header_tests):
  - The test makes a seeded format 8 store, puts a 197-byte root record whose five fields are plain values, syncs it, and logs it with `commit_root`.
  - `load_store`, `load_store_read_only` and `start` then each return exactly the M19 text with the store path.
  - The whole store tree (names, sizes and blake3 of every file) is byte-equal before and after, and no `mount.lock` is left.

## Decisions

- The refusal reads the first object of each of the five fields (five small reads at mount, O(1)). M19 is the next free message number. Both are discretion choices of the plan.
- The check runs before any tree read (the root inode read), so no tree code ever decodes a flat map.
- The error is built with `starts_with` on the read bytes, not `bytes[..4]`, because an empty interim map is shorter than 4 bytes.
- A field object that cannot be read (missing, damaged) keeps the normal `node_err` text. That is store damage, not an interim store.
- M19 is given on the three mount paths only, as the plan says. The offline commands (`stats`, `scrub`, `gc`, `snapshot`) give the generic text, which still contains "the metadata maps are not prolly trees".

## Deviations from Plan

**1. Task 1 had no code to delete.** The flat-map code was removed by 26-04 to 26-09 (26-09 removed the last part). The verify grep was 0 at the base. The doc fixes of Task 1 are in the Task 2 commit, so Task 1 has no commit of its own.

**2. One commit for code, one for docs.** The code commit holds Tasks 1 and 2. The docs commit holds the Task 3 doc and comment edits (`lock_stats.rs` and `prolly/src/lib.rs` are comment-only). Then comes the gate log commit. `cargo test --workspace` ran on the tree with all edits, before both commits.

**3. Docker volume.** The plan gave `SLICEFS_TARGET_VOL=slicefs-target-26-10`. EXEC-RULES rule 7 and the orchestrator say to use the default volume `slicefs-target`, so `SLICEFS_TARGET_VOL` was not set.

**4. `docs/design/limits.md` is not in `files_modified`.** The orchestrator asked for it: it contradicted the code (the "Phase 26" marks, and an inode record of "about 100 bytes").

**5. Gate log.** It has 10 gate sections, all `rc=0`. It adds `store_header_tests` and `cargo test -p metadata` with `--no-fail-fast` to the plan's list. The `^== ` count is 16, because the `verify` output has its own `== ` section lines. The Docker `all` section lists the mount smoke step lines before its last 40 lines.

## Verification

All gates ran on the tree of `280e506` (`runs/gate-26-10.log`, 10 `rc=0` lines, no `rc=[1-9]`):

- `cargo test -p metadata` (lib: 198 passed);
- `cargo test -p prolly`;
- `store_header_tests` (11 passed);
- `lock_stats_report` with `--features lock-stats` (`dir_live n=82`, `inode_live n=87`, `dir_data n=13`);
- clippy `-D warnings` for metadata, slicefs-cli, prolly and chunk-store;
- `unsafe-count.sh` (`files=34 tokens=144 result=ok`);
- `cargo test --workspace` (92 test result lines, all ok; `retry_tests` did not fail);
- Docker `run.sh all`: the Linux workspace, the fuser lib and the mount smoke, with every step ok, including the xattr, readdir-many and rmdir steps;
- Docker `SLICEFS_WORKERS=4 run.sh pjdfstest`: `Files=237, Tests=8792, Result: PASS`, `Workers: 4 (set)`;
- Docker `run.sh verify`: `RESULT verify-phases: PASS` (snapshots, remount, a 1 GiB copy with daemon VmHWM 97 MiB under the 512 MiB cap).

Plan acceptance greps:

| Grep | Count | Expected |
|------|-------|----------|
| flat helper names in store.rs and rehash/mod.rs (code lines) | 0 | 0 |
| `Blob::ValueMap`, `Blob::ManifestMap` in rehash/mod.rs | 0 | 0 |
| `fn msg_interim_flat_maps` in header.rs | 1 | 1 |
| `FLAT_MAPS_REFUSED` in mount.rs | 1 | at least 1 |
| `NODE_MAGIC` in store.rs | 3 | at least 1 |
| `M19`, `type (1 byte`, `tag 0 then` in store-format.md | 6 | at least 3 |
| `key of a serialized flat map in this build` in store-format.md | 0 | 0 |
| `do not use this crate` in prolly/src/lib.rs | 0 | 0 |
| `^rc=0$` in the gate log | 10 | at least 9 |
| `^rc=[1-9]` in the gate log | 0 | 0 |

## Commits

- `9c2e1d0` feat(26-10): refuse an interim flat-map store at mount with M19 (IN-02)
- `280e506` docs(26-10): final key grammar and value encodings, M19, tree protocol
- `fc04fc2` chore(26-10): gate log

- `0821bca` docs(26-10): summary
- `5ef08c9` docs(26-10): M19 and --hash, scale table grammar note (docs only: the M19 text about `--hash`, and a note that the 1B-inode scale table used the Phase 25 grammar)

`commits: 5` counts the commits from `799349c` to `5ef08c9` (the first summary commit included). The commit that updates this SUMMARY is not in the count. `tokens` is chars/4 of the plan's diff.

## Notes for 26-11 and 26-12 (they run next, in parallel)

- `load_from_root` now does five more `ChunkStore::read` calls, one per field, before the root inode read. A test that counts the reads of a load, or that puts a fault on the first node read of a load, sees them. These reads do not go through the node cache.
- A test that builds a root record by hand must put real tree roots in all five fields. A plain value in any field is refused with `FLAT_MAPS_REFUSED`.
- `metadata-maps.md` "GC mark" says that plan 26-11 records the measured cost of the full snapshot walk. 26-11 can add the number there, or in its own run file.
- `metadata-maps.md` "Commit" lists the steps and failure rules as built. If 26-12 widens the crash matrix or the test hooks (`test_fail_next_flush`, the flush hook) to all trees, the text needs no change unless a rule changes.
- The "Scale at 1,000,000,000 inodes" table of `store-format.md` was measured with the Phase 25 key grammar (one `dir_data` entry per directory). The doc now says so. If 26-11 measures tree sizes on the product code, it can replace the table.
- Not fixed, and outside this plan's files:
  - `crates/metadata/src/inode.rs`: the doc of `serialize_inode` still says "56 or 60 bytes", and says the 56-byte form keeps old "CAS digests". The code and the format doc say 56 or 72 bytes.
  - `docs/user/upgrade.md` covers format 7 only. M19 points to it, and its copy steps (issue #155) also apply to an interim store.
- Known costs, documented and not changed: readdir offsets are entry indexes (O(n^2 / page) entry reads for a full listing); setxattr ignores `XATTR_CREATE`/`XATTR_REPLACE`; `content_stats` keeps O(files) sets.

## Known Stubs

None.

## Threat Flags

None beyond the plan's register:

- T-26-29: the magic check of all five fields at load and M19 on the three mount paths. The CLI test shows that the whole store tree is unchanged with no `--hash`. With `--hash`, `select_hash` records the hash in `FORMAT` before the load: this is the existing order of `load_store_with`, it is not tested here, and the M19 row says so.
- T-26-30: the docs were written from `inode.rs`, `dirent.rs`, `xattr.rs`, `pmap.rs`, `store.rs` and `gc/mod.rs` as they are at `799349c`.
- T-26-31: Docker `all`, pjdfstest with 4 workers and `verify` passed.
- T-26-SC: no package was added.

## Self-Check: PASSED

The gate log and this SUMMARY exist. Commits `9c2e1d0`, `280e506`, `fc04fc2`, `0821bca` and `5ef08c9` are ancestors of HEAD. The gate log has 10 `rc=0` lines, no `rc=[1-9]` line, and no home, user or scratch paths.
