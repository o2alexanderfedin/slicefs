---
phase: 02-metadata-engine
verified: 2026-03-27T00:00:00Z
status: passed
score: 5/5 must-haves verified
re_verification: false
---

# Phase 2: Metadata Engine Verification Report

**Phase Goal:** The inode table, directory tree, file manifests, and xattr store exist as an ACID-backed metadata layer completely separated from the block store — FUSE can be wired on top of it
**Verified:** 2026-03-27
**Status:** passed
**Re-verification:** No — initial verification

## Goal Achievement

### Observable Truths (from ROADMAP.md Success Criteria)

| #   | Truth                                                                                              | Status     | Evidence                                                                                              |
| --- | -------------------------------------------------------------------------------------------------- | ---------- | ----------------------------------------------------------------------------------------------------- |
| 1   | An inode can be created, read, updated, and deleted via the MetadataStore interface                | VERIFIED | `store.rs`: `create_inode`, `get_inode`, `update_inode`, `delete_inode` all implemented; `test_inode_crud` passes |
| 2   | A directory entry can be created, listed, and removed; . and .. are always present                 | VERIFIED | `directory.rs`: `create_dir_entries` always adds `.` and `..`; `test_create_dir_entries_has_dot_and_dotdot` and `test_new_has_root_dir` pass |
| 3   | A file manifest linking inode to ordered list of block hashes can be created and retrieved         | VERIFIED | `manifest.rs`: `intern_manifest`/`load_manifest`; `test_manifest_round_trip` and `test_ordering_preserved` pass |
| 4   | Extended attributes can be stored and retrieved on an inode                                        | VERIFIED | `xattr.rs` + `store.rs`: full `set_xattr`/`get_xattr`/`list_xattrs`/`remove_xattr` implemented; `test_xattr_set_get`, `test_xattr_list`, `test_xattr_remove`, `test_xattr_overwrite`, `test_xattr_on_directory` all pass |
| 5   | Inode numbers are stable — the same inode number is assigned to the same file across process restarts | VERIFIED | `store.rs`: `commit()` + `load_from_root()` preserve exact inode counter via `InodeMap::set_next_ino`; `test_inode_stability_across_reload` and `test_dictionary_persistence_round_trip` pass |

**Score:** 5/5 truths verified

---

### Required Artifacts

| Artifact | Expected | Status | Details |
| -------- | -------- | ------ | ------- |
| `crates/data-id/blockset/Cargo.toml` | blockset crate as git submodule | VERIFIED | Submodule initialized at commit `7054519`; `blockset/Cargo.toml` exists |
| `crates/dedupfs-traits/src/digest.rs` | Re-exports Digest224, Digest256, Branches | VERIFIED | Type aliases `Digest224 = [u32; 7]`, `Digest256 = [u32; 8]`, `Branches = [Digest256; 2]` declared; helper functions re-exported from blockset |
| `crates/dedupfs-traits/src/storage.rs` | Re-exports StorageAdd, StorageGet | VERIFIED | Traits declared independently (blockset storage module is private); structurally identical |
| `crates/dedupfs-traits/src/metadata.rs` | MetadataStore trait, InodeId, DirEntry, MetaError, InodeMeta | VERIFIED | All types and 16-method trait defined; `InodeMeta` has uid/gid/mode/mtime/ctime fields |
| `crates/metadata/src/inode.rs` | InodeMeta 56-byte binary serialization | VERIFIED | `serialize_inode` produces exactly 56 bytes; `intern_inode`/`load_inode` for Dictionary CAS; 9 tests including proptest pass |
| `crates/metadata/src/inode_map.rs` | InodeMap: monotonic allocation, CAS intern/load | VERIFIED | `allocate_ino()` starts at 2; 36-byte record serialization; `set_next_ino()` for reload |
| `crates/metadata/src/directory.rs` | Per-entry directory CAS operations | VERIFIED | `create_dir_entries`, `add_dir_entry`, `remove_dir_entry`, `lookup_dir_entry`, `list_dir_entries` implemented; dot/dotdot protection in place |
| `crates/metadata/src/manifest.rs` | Ordered Digest224 block list storage | VERIFIED | `intern_manifest`/`load_manifest`; 28-byte per Digest224; empty and multi-block round-trips pass |
| `crates/metadata/src/store.rs` | DictMetadataStore implementing MetadataStore trait | VERIFIED | All 16 trait methods implemented; `commit()` produces 156-byte root record; `load_from_root()` reconstructs full state; `serialize_dictionary`/`deserialize_dictionary` for persistence |
| `crates/metadata/src/xattr.rs` | Xattr storage: intern_xattrs, load_xattrs, set/get/list/remove helpers | VERIFIED | Full implementation with CAS-backed xattr list; large value (>31 bytes) CAS tree path tested |

---

### Key Link Verification

| From | To | Via | Status | Details |
| ---- | -- | --- | ------ | ------- |
| `crates/dedupfs-traits/src/digest.rs` | `blockset` | `pub use blockset::from_digest224` etc. | WIRED | Helper functions re-exported from blockset; type aliases declared locally due to private blockset modules |
| `crates/metadata/src/store.rs` | `blockset::Dictionary` | `Mutex<Dictionary>` field | WIRED | `dict: Mutex<Dictionary>` at line 44; all operations lock this |
| `crates/metadata/src/store.rs` | `crates/metadata/src/directory.rs` | function calls | WIRED | `use crate::directory::{create_dir_entries, add_dir_entry, ...}` at line 22; called in `create_directory`, `list_directory`, `lookup`, `link`, `unlink` |
| `crates/metadata/src/store.rs` | `crates/metadata/src/inode_map.rs` | `InodeMap` field | WIRED | `inode_map: Mutex<InodeMap>` at line 46; `allocate_ino()`, `insert()`, `set_next_ino()` called throughout |
| `crates/metadata/src/store.rs` | `crates/metadata/src/xattr.rs` | function calls | WIRED | `use crate::xattr::{intern_xattrs, load_xattrs, ...}` at line 25; called in all four xattr methods |
| `crates/metadata/src/store.rs` | `serialize_dictionary`/`deserialize_dictionary` | Dictionary persistence | WIRED | Both functions implemented in `store.rs`; used in `test_dictionary_persistence_round_trip` — bypasses broken `blockset::serialize` |

---

### Requirements Coverage

| Requirement | Source Plans | Description | Status | Evidence |
| ----------- | ------------ | ----------- | ------ | -------- |
| META-03 | 02-01, 02-02 | Metadata storage separated from block storage | SATISFIED | `DictMetadataStore` in `crates/metadata` is entirely separate from `crates/cas-local`; no block store dependency |
| POSIX-06 | 02-01 | File permissions (chmod/chown, uid/gid) | SATISFIED | `InodeMeta` has `mode`, `uid`, `gid` fields; stored and round-tripped via 56-byte serialization; `update_inode` enables chmod/chown semantics |
| POSIX-07 | 02-01 | Timestamps (mtime, ctime; noatime by default) | SATISFIED | `InodeMeta` has `mtime_sec`, `mtime_nsec`, `ctime_sec`, `ctime_nsec` fields; stored and round-tripped |
| POSIX-08 | 02-03 | Extended attributes (xattr) | SATISFIED | Full `set_xattr`/`get_xattr`/`list_xattrs`/`remove_xattr` in `DictMetadataStore`; CAS-backed via `xattr.rs`; all xattr tests pass |
| POSIX-10 | 02-02, 02-03 | Stable inode numbers across mount cycles | SATISFIED | `commit()` encodes `next_ino` in 156-byte root record; `load_from_root()` calls `set_next_ino()` to restore exact counter; `test_inode_stability_across_reload` and `test_dictionary_persistence_round_trip` prove this end-to-end |

**Orphaned requirements:** None. All 5 phase-2 requirements appear in at least one plan's `requirements` field.

---

### Anti-Patterns Found

No anti-patterns detected.

Scanned: `store.rs`, `xattr.rs`, `directory.rs`, `inode.rs`, `inode_map.rs`, `manifest.rs`, `crates/dedupfs-traits/src/metadata.rs`

- No `TODO`, `FIXME`, `XXX`, `HACK`, or `PLACEHOLDER` comments in any file
- No stub returns (`return null`, `return {}`, etc.)
- All xattr methods previously stubbed with `Err(MetaError::Corrupted("xattr not yet implemented"))` in plan 02-02 were replaced with full implementations in plan 02-03

---

### Human Verification Required

None. All phase-2 success criteria are verifiable programmatically. The metadata engine has no FUSE kernel interface (Phase 3) or external service dependencies at this stage.

---

### Summary

All three plans executed successfully. The metadata engine is complete:

- **Plan 02-01** established the type foundation: `data-id` git submodule wired as `blockset` path dependency, `Digest224`/`Digest256`/`MetadataStore`/`InodeMeta` types in `dedupfs-traits`, metadata crate with 56-byte inode serialization and Dictionary CAS round-trip.
- **Plan 02-02** built the core store: `InodeMap` with monotonic allocation, per-entry directory CAS operations, file manifest storage, and `DictMetadataStore` implementing all 16 `MetadataStore` trait methods backed by `Mutex<Dictionary>`.
- **Plan 02-03** completed xattr storage and proved the persistence story: `load_from_root()` reconstructs complete filesystem state from a Dictionary + root `Digest224`, with inode numbers stable across the cycle.

Two notable deviations (both auto-fixed) affect design but not correctness:
1. `blockset::serialize`/`deserialize` panics on small payloads — replaced with own 92-byte-per-entry `serialize_dictionary`/`deserialize_dictionary`.
2. Directory entries stored as a serialized entry list (CAS blob) rather than individual Dictionary nodes, because `StorageAdd` does not allow choosing a key.

88 tests pass across all metadata modules. 0 workspace regressions. FUSE can be wired on top of `DictMetadataStore` via the `MetadataStore` trait — Phase 3 is unblocked.

---

_Verified: 2026-03-27_
_Verifier: Claude (gsd-verifier)_
