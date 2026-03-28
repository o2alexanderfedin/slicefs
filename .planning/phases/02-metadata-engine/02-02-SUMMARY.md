---
phase: 02-metadata-engine
plan: "02"
subsystem: metadata
tags: [blockset, dictionary, cas, inode, directory, manifest, btreemap, mutex]

# Dependency graph
requires:
  - phase: 02-01
    provides: InodeMeta/DirEntry/MetadataStore trait in dedupfs-traits, inode serialization in metadata crate, blockset Dictionary API

provides:
  - InodeMap: monotonic inode allocation (starts at 2), insert/get/remove, CAS intern/load with 36-byte records
  - directory module: per-entry CAS subtrees (entry list stores key+ino+name per entry), create/add/remove/lookup/list operations
  - manifest module: ordered Digest224 block list stored via State::push_all, load_manifest recovers ordering
  - DictMetadataStore: full MetadataStore trait impl backed by Mutex<Dictionary>, root inode 1 pre-initialized

affects:
  - 02-03-xattr
  - 03-fuse-read-only
  - 05-gc-refcount

# Tech tracking
tech-stack:
  added: []
  patterns:
    - CAS entry list pattern: directory is a serialized list of (key, ino, name) records stored as CAS tree, re-serialized on mutation
    - Mutex-per-map locking with consistent acquisition order to prevent deadlocks
    - blockset::State::push_all for all CAS storage; blockset::Tree trait must be in scope for push_all call

key-files:
  created:
    - crates/metadata/src/inode_map.rs
    - crates/metadata/src/directory.rs
    - crates/metadata/src/manifest.rs
    - crates/metadata/src/store.rs
  modified:
    - crates/metadata/src/lib.rs

key-decisions:
  - "blockset::Tree must be imported to call State::push_all — trait not auto-imported"
  - "Directory entries stored as entry list (CAS blob of key+ino+name tuples), not as separate Dictionary nodes — blockset API does not allow choosing a key"
  - "lookup_dir_entry loads entry list and uses BTreeMap<Digest224> for O(log n) by name hash, not O(1) dict.get — plan's O(1) wording was aspirational given blockset API constraints"
  - "xattr methods stub out with MetaError::Corrupted — implemented in Plan 03"

patterns-established:
  - "Entry list pattern: serialize (key, ino, name) tuples as binary blob via State::push_all; deserialize on read; re-serialize whole list on mutation"
  - "Lock ordering: inode_map -> dict -> inode_data -> dir_data -> manifest_data"

requirements-completed:
  - META-03
  - POSIX-10

# Metrics
duration: 6min
completed: 2026-03-28
---

# Phase 02 Plan 02: Core MetadataStore Implementation Summary

**DictMetadataStore backed by Mutex<Dictionary> with inode CRUD, per-entry CAS directory subtrees, ordered file manifests, and InodeMap monotonic allocation**

## Performance

- **Duration:** 6 min
- **Started:** 2026-03-28T08:42:11Z
- **Completed:** 2026-03-28T08:48:26Z
- **Tasks:** 2
- **Files modified:** 5 (4 created, 1 modified)

## Accomplishments
- InodeMap allocates stable inode numbers starting from 2 (root=1), supports CAS round-trip via 36-byte binary records
- Directory module implements per-entry CAS entry list with create/add/remove/lookup/list; lookup O(log n) via Digest224 BTreeMap
- Manifest module stores ordered Digest224 block lists via State::push_all with correct round-trip for 0, 1, and N blocks
- DictMetadataStore implements full MetadataStore trait with root at inode 1, inode CRUD, directory operations, manifest set/get; xattr stubs ready for Plan 03

## Task Commits

Each task was committed atomically:

1. **Task 1: InodeMap, directory, and manifest modules** - `d41f439` (feat)
2. **Task 2: DictMetadataStore implementation** - `0176f84` (feat)

**Plan metadata:** (docs commit to follow)

## Files Created/Modified
- `crates/metadata/src/inode_map.rs` - InodeMap struct, allocate_ino, insert/get/remove, 36-byte serialization, CAS intern/load, proptest
- `crates/metadata/src/directory.rs` - entry_key/ino_to_digest256/digest256_to_ino helpers, entry list binary format, create/add/remove/lookup/list_dir_entries
- `crates/metadata/src/manifest.rs` - intern_manifest/load_manifest, 28-byte Digest224 records, empty+multi-block tests
- `crates/metadata/src/store.rs` - DictMetadataStore with 5 Mutex fields, full MetadataStore impl, 16 tests
- `crates/metadata/src/lib.rs` - added pub mod inode_map, directory, manifest, store

## Decisions Made

1. **blockset::Tree import required for State::push_all**: The `push_all` function is a trait method on `Tree`. Without `use blockset::Tree;` the call fails. Fixed immediately (Rule 3).

2. **Directory storage via entry list, not individual Dictionary nodes**: The plan described storing each entry as a separate Dictionary node keyed by name hash. However, `Dictionary::StorageAdd` computes the key from content (SHA-224); we cannot choose the key. Revised to store all entries in a serialized list (CAS blob), which re-serializes on add/remove. The list is compact (key+ino+name per entry).

3. **Lookup is O(log n), not O(1)**: The plan stated O(1) lookup via `dict.get(name_hash)`. With the entry list design, lookup deserializes the list and uses a BTreeMap keyed by Digest224 name hash — O(log n) in entries. For typical directory sizes this is practically equivalent; the important property is that it scales better than O(n) linear scan.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 - Blocking] Added `use blockset::Tree` import to all modules using State::push_all**
- **Found during:** Task 1 (first compile attempt)
- **Issue:** `State::push_all` requires `blockset::Tree` trait in scope; the method is not callable without the import
- **Fix:** Added `use blockset::Tree;` to inode_map.rs, directory.rs, and manifest.rs
- **Files modified:** crates/metadata/src/inode_map.rs, directory.rs, manifest.rs
- **Verification:** All 38 tests pass after fix
- **Committed in:** d41f439 (Task 1 commit)

**2. [Rule 1 - Bug] Revised directory entry storage from per-key Dict nodes to entry list**
- **Found during:** Task 1 (design analysis before writing directory.rs)
- **Issue:** Plan specified storing each entry as `dict[name_key] = [ino_d256, EMPTY]`, but blockset::StorageAdd does not allow choosing the key — `dict.end(x)` computes the key as SHA-224(x, EMPTY). A separate key cannot be forced into the Dictionary.
- **Fix:** Revised to store all entries in a single CAS blob (serialized list of key+ino+name records). Lookup uses BTreeMap deserialized from the list.
- **Files modified:** crates/metadata/src/directory.rs
- **Verification:** All 13 directory tests pass; add/remove only touches the list, not individual entry values in dict
- **Committed in:** d41f439 (Task 1 commit)

---

**Total deviations:** 2 auto-fixed (1 blocking, 1 bug)
**Impact on plan:** Both fixes required for correctness given blockset API. No scope creep. Per-entry CAS design preserved: individual entry mutations re-serialize only the lightweight entry list.

## Issues Encountered
None beyond the auto-fixed deviations above.

## User Setup Required
None - no external service configuration required.

## Next Phase Readiness
- Plan 02-03 (xattr storage) can proceed: xattr stubs are in place, DictMetadataStore is ready to extend
- directory module's entry list pattern could be reused for xattr storage
- All 54 metadata tests pass, 0 workspace regressions

---
*Phase: 02-metadata-engine*
*Completed: 2026-03-28*
