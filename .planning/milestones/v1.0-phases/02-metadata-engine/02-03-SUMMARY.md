---
phase: 02-metadata-engine
plan: 03
subsystem: database
tags: [blockset, cas, xattr, persistence, inode-stability, posix]

# Dependency graph
requires:
  - phase: 02-metadata-engine/02-02
    provides: DictMetadataStore with inode/dir/manifest ops, blockset Dictionary CAS storage
provides:
  - xattr set/get/list/remove on any inode via CAS-backed name=value pair storage
  - commit() serializing full filesystem state (inodes, dirs, manifests, xattrs) into a 156-byte root Digest224
  - load_from_root(dict, root) reconstructing DictMetadataStore with all data intact
  - serialize_dictionary/deserialize_dictionary for correct Dictionary persistence
  - Inode number stability across commit/reload cycle (POSIX-10 proven)
affects: [03-fuse-layer, 04-chunk-engine, 05-refcount-gc]

# Tech tracking
tech-stack:
  added: []
  patterns:
    - "Xattr list stored as flat byte stream: [name_len u32][name][value_len u32][value] concatenated"
    - "Root record is a 156-byte blob: 7 Digest224 fields + 2 u64 scalars all serialized LE"
    - "BTreeMap<u64, Digest224> serialized as [count u64][ino u64 + Digest224 28 bytes...] via intern_u64_digest_map"
    - "Dictionary persistence uses our own 92-byte-per-entry format (blockset::serialize broken for small payloads)"

key-files:
  created:
    - crates/metadata/src/xattr.rs
  modified:
    - crates/metadata/src/store.rs
    - crates/metadata/src/inode_map.rs
    - crates/metadata/src/lib.rs

key-decisions:
  - "Xattr storage: load-mutate-re-intern pattern (same as directory entries) — load existing xattr list from CAS, modify in memory, intern updated list"
  - "blockset::serialize / blockset::deserialize broken for small payloads: deserialize recomputes compress(left, right) and calls to_digest224 which returns None for inline entries produced by end() — implemented own serialize_dictionary/deserialize_dictionary (92 bytes per entry: 28-byte key + 64-byte branches)"
  - "Added InodeMap::set_next_ino() to restore exact counter from root record without O(n) loop"
  - "Root record expanded from 44 bytes to 156 bytes to include inode_data/dir_data/manifest_data/xattr_data map digests"

patterns-established:
  - "All per-inode state maps (inode_data, dir_data, manifest_data, xattr_data) serialized via intern_u64_digest_map and referenced from root record"
  - "load_from_root is the inverse of commit: reads root record, loads all digests, reconstructs complete in-memory state"

requirements-completed: [POSIX-08, POSIX-10]

# Metrics
duration: 10min
completed: 2026-03-27
---

# Phase 2 Plan 03: Xattr Storage, Full Persistence Round-Trip, and Inode Stability Summary

**CAS-backed xattr storage plus full commit/reload cycle proving POSIX inode number stability via 156-byte root record capturing all metadata maps**

## Performance

- **Duration:** 10 min
- **Started:** 2026-03-27T16:12:04Z
- **Completed:** 2026-03-27T16:22:00Z
- **Tasks:** 2
- **Files modified:** 4

## Accomplishments

- Xattr storage module with intern/load CAS-backed pair lists and in-memory set/get/list/remove helpers
- DictMetadataStore xattr methods fully implemented: load-mutate-re-intern pattern, empty list returns `[]`
- Expanded commit() to serialize all state maps as a 156-byte root record
- load_from_root() reconstructing full DictMetadataStore from Dictionary + root Digest224
- serialize_dictionary/deserialize_dictionary bypassing broken blockset serialize (see deviations)
- 88 tests passing (34 new), all workspace tests pass — POSIX-08 and POSIX-10 satisfied

## Task Commits

1. **Task 1: Xattr storage module and store integration** - `1ba9984` (feat)
2. **Task 2: Persistence round-trip and inode stability across reload** - `243712f` (feat)

**Plan metadata:** (created after summary)

## Files Created/Modified

- `/Volumes/Unitek-B/Projects/file-systems/crates/metadata/src/xattr.rs` - Xattr storage: intern_xattrs, load_xattrs, set/get/list/remove_xattr_entry
- `/Volumes/Unitek-B/Projects/file-systems/crates/metadata/src/store.rs` - Full xattr methods, revised commit(), load_from_root(), serialize/deserialize_dictionary, intern/load_u64_digest_map
- `/Volumes/Unitek-B/Projects/file-systems/crates/metadata/src/inode_map.rs` - Added set_next_ino() method
- `/Volumes/Unitek-B/Projects/file-systems/crates/metadata/src/lib.rs` - Added pub mod xattr

## Decisions Made

- Xattr storage uses load-mutate-re-intern pattern (same as directory entries): load existing xattr list from CAS by digest, modify in-memory, intern updated list, store new digest. Returns empty `Vec` for list_xattrs on inodes with no xattrs (no error).
- Root record expanded from 44 bytes (Plan 02 stub) to 156 bytes: inode_map_digest (28) + root_dir_ino (8) + next_ino (8) + inode_data_digest (28) + dir_data_digest (28) + manifest_data_digest (28) + xattr_data_digest (28).
- Added InodeMap::set_next_ino() to restore exact counter, avoiding O(n) allocation loop that would be needed otherwise.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 1 - Bug] blockset::serialize / blockset::deserialize broken for small payloads**
- **Found during:** Task 2 (test_dictionary_persistence_round_trip)
- **Issue:** blockset's deserialize recomputes `to_digest224(&compress(left, right))` for every entry. For entries stored via State::push_all on small data (< 248 bytes), the Dictionary stores `end()` nodes where `key = SHA224.compress(x, EMPTY)[..7]` but the branches are `[x, EMPTY]`. When deserializing, `compress(x, EMPTY)` may return an inline Digest256 (not a hash), so `to_digest224` returns None and `unwrap()` panics.
- **Fix:** Implemented `serialize_dictionary(dict) -> Vec<u8>` and `deserialize_dictionary(bytes) -> Result<Dictionary, MetaError>` in store.rs. Format: 92 bytes per entry (28-byte Digest224 key + 64-byte Branches), iterating over BTreeMap entries. Does not recompute hashes, just stores/restores key+branches verbatim.
- **Files modified:** crates/metadata/src/store.rs
- **Verification:** test_dictionary_persistence_round_trip passes; all 88 tests pass
- **Committed in:** 243712f (Task 2 commit)

**2. [Rule 2 - Missing Critical] Added InodeMap::set_next_ino() method**
- **Found during:** Task 2 (load_from_root implementation)
- **Issue:** InodeMap::deserialize_inode_map reconstructs next_ino as max(keys)+1 which may be too low if inodes were deleted. load_from_root needs to restore the exact counter from the root record.
- **Fix:** Added `pub fn set_next_ino(&mut self, value: u64)` to InodeMap. Replaces original O(n) workaround via allocate_ino() loop.
- **Files modified:** crates/metadata/src/inode_map.rs
- **Verification:** test_inode_stability_across_reload passes
- **Committed in:** 243712f (Task 2 commit)

---

**Total deviations:** 2 auto-fixed (1 bug, 1 missing critical)
**Impact on plan:** Both essential for correctness. The blockset serialize bug would have blocked POSIX-10 proof; the set_next_ino omission would have caused inode counter regression after reload.

## Issues Encountered

- blockset::serialize/deserialize has a latent panic bug for small payloads. Filed as known limitation; our own serialization is correct. The blockset submodule is not our codebase to patch.

## Next Phase Readiness

- Phase 2 metadata engine is complete: inode CRUD, directory operations, file manifests, xattrs, commit/reload cycle all working
- POSIX-08 (xattr set/get/list/remove) and POSIX-10 (inode stability across restart) satisfied
- Phase 3 FUSE layer can use DictMetadataStore directly via MetadataStore trait
- Persistence story: serialize_dictionary + commit root digest = complete filesystem snapshot

---
*Phase: 02-metadata-engine*
*Completed: 2026-03-27*

## Self-Check: PASSED

- xattr.rs: FOUND
- 02-03-SUMMARY.md: FOUND
- Task 1 commit 1ba9984: FOUND
- Task 2 commit 243712f: FOUND
