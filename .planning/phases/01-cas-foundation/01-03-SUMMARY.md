---
phase: 01-cas-foundation
plan: 03
subsystem: cas
tags: [rust, cas, disk-store, bloom-filter, dedup, blake3, fastbloom, proptest, tempfile, integrity]

# Dependency graph
requires:
  - phase: 01-cas-foundation-01
    provides: "BlockStore/DedupIndex/ContentHasher traits, ChunkHash, CasError, Blake3Hasher (from plan 02 work)"
provides:
  - LocalDiskStore (CAS-03) — disk-backed BlockStore with 2-byte hex directory sharding and atomic writes
  - MemDedupIndex (CAS-07) — AtomicBloomFilter fast path + HashSet authoritative lookup dedup index
  - Integrity verification on read (CAS-05) — re-hash on get() detects on-disk corruption
affects:
  - Phase 2 (metadata layer will use LocalDiskStore for block persistence)
  - Phase 3 (FUSE layer will use LocalDiskStore via BlockStore trait)
  - Phase 5 (GC will call delete() on LocalDiskStore and remove() on DedupIndex)

# Tech tracking
tech-stack:
  added:
    - fastbloom::AtomicBloomFilter — lock-free concurrent bloom filter for DedupIndex insert/contains (&self)
    - tempfile::TempDir — temporary directories for disk store unit tests
    - proptest — property-based round-trip tests for both implementations
  patterns:
    - Atomic write pattern: write to .tmp file then fs::rename to final path (prevents partial writes)
    - 2-byte hex prefix sharding: hash.to_string()[..2] as directory name (256-way sharding, no truncation)
    - Two-tier dedup detection: AtomicBloomFilter fast path + RwLock<HashSet> authoritative lookup
    - Bloom-no-delete invariant: remove() only clears HashSet; false positives from removed hashes tolerated
    - Serialization via HashSet + bloom rebuild: bloom state reconstructed from authoritative set on load

key-files:
  created:
    - crates/cas-local/src/disk_block_store.rs — LocalDiskStore impl with sharding, atomic writes, integrity
    - crates/cas-local/src/mem_dedup_index.rs — MemDedupIndex impl with AtomicBloomFilter + HashSet
  modified: []

key-decisions:
  - "AtomicBloomFilter (not BloomFilter) chosen for &self DedupIndex::insert — trait requires &self for Arc<dyn DedupIndex> sharing"
  - "Bloom serialization deferred: fastbloom serde feature not enabled in workspace; HashSet serialized instead and bloom rebuilt on load"
  - "Atomic writes via .tmp + rename — prevents partially-written blocks from appearing as valid in concurrent scenarios"
  - "Idempotent put() via path.exists() check — no error on duplicate write, enables content-addressable deduplication at block store level"

patterns-established:
  - "Pattern 5 (Atomic Disk Write): all block writes go through .tmp + rename — no partial block state visible to concurrent readers"
  - "Pattern 6 (2-byte Shard Dir): hex[..2] as shard prefix, hex[2..] as filename — 256-way fan-out from day one"
  - "Pattern 7 (Bloom-No-Delete): DedupIndex.remove() never updates bloom — false positives from removed entries are acceptable cost"

requirements-completed: [CAS-03, CAS-05, CAS-07]

# Metrics
duration: 6min
completed: 2026-03-28
---

# Phase 1 Plan 03: CAS Foundation Summary

**LocalDiskStore with 2-byte hex sharded layout and integrity re-hashing on read, plus MemDedupIndex with AtomicBloomFilter fast-path and authoritative HashSet, completing Phase 1 CAS implementation**

## Performance

- **Duration:** 6 min
- **Started:** 2026-03-28T06:33:59Z
- **Completed:** 2026-03-28T06:40:18Z
- **Tasks:** 2 of 2
- **Files modified:** 2

## Accomplishments

- LocalDiskStore: atomic writes (.tmp + rename), 2-byte hex directory sharding, integrity re-hash on get(), idempotent put/delete, 13 unit tests including corruption detection and proptest round-trip
- MemDedupIndex: AtomicBloomFilter fast path (bloom_check), RwLock<HashSet> authoritative lookup, two-tier dedup detection, thread-safe, serialization round-trip via HashSet + bloom rebuild, 12 unit tests including bloom false positive demonstration and proptest
- Full workspace: 52 tests pass across all cas-local modules (blake3_hasher, fixed_chunker, mem_block_store, disk_block_store, mem_dedup_index)

## Task Commits

Each task was committed atomically:

1. **Task 1: Implement LocalDiskStore with sharded layout and integrity verification** - `67383eb` (feat)
2. **Task 2: Implement MemDedupIndex with bloom filter and dedup detection tests** - `605d9da` (feat)

**Plan metadata:** (pending)

## Files Created/Modified

- `crates/cas-local/src/disk_block_store.rs` — LocalDiskStore: 2-byte sharding, atomic writes, verify_on_read integrity, idempotent put/delete, 13 tests
- `crates/cas-local/src/mem_dedup_index.rs` — MemDedupIndex: AtomicBloomFilter fast path, HashSet authoritative lookup, serialization, 12 tests

## Decisions Made

- **AtomicBloomFilter for `&self` insert** — The DedupIndex trait requires `&self` to support `Arc<dyn DedupIndex>`. `fastbloom::BloomFilter` requires `&mut self` for insert; `AtomicBloomFilter` uses atomic operations and takes `&self`. Used `AtomicBloomFilter` to satisfy the trait contract without wrapping in `Mutex<BloomFilter>`.
- **Bloom serialization via HashSet + rebuild** — `fastbloom`'s serde feature is not enabled in the workspace Cargo.toml. Rather than adding an optional feature dependency, the authoritative `HashSet` is serialized and the bloom filter is rebuilt from it on `load_from_reader`. O(n) rebuild on startup is acceptable for Phase 1.
- **Atomic writes via .tmp + rename** — Prevents partially-written block files from being visible as valid blocks if the write is interrupted. `fs::rename` is atomic on POSIX filesystems.
- **Idempotent put() via path.exists()** — Content-addressable storage implies the same hash always maps to the same content; if the file already exists, we can safely skip the write. This enables deduplication at the block store level without coordinating with the DedupIndex.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 - Blocking] Blake3Hasher already implemented (prior session work)**
- **Found during:** Task 1 setup
- **Issue:** blake3_hasher.rs appeared to be a placeholder based on commit history, but was actually implemented (likely from a prior session executing Plan 02 work). No action needed — it was available for import.
- **Fix:** None required. Imported `Blake3Hasher` from `crate::blake3_hasher` as planned.
- **Files modified:** None
- **Verification:** `cargo check --workspace` passed; tests imported Blake3Hasher successfully.

---

**Total deviations:** 1 (pre-existing implementation, no corrective action required)
**Impact on plan:** None. The presence of Blake3Hasher enabled both implementations without modification to the plan's scope.

## Issues Encountered

- `fastbloom::BloomFilter` requires `&mut self` for `insert()` which conflicts with the `DedupIndex` trait's `&self` requirement. Resolved by using `AtomicBloomFilter` which provides `&self` insert via lock-free atomic operations. This is the correct solution and produces better thread safety properties than `Mutex<BloomFilter>`.

## User Setup Required

None — no external service configuration required.

## Next Phase Readiness

- Phase 1 CAS Foundation complete: all five cas-local modules implemented (Blake3Hasher, FixedChunker, MemBlockStore, LocalDiskStore, MemDedupIndex)
- 52 tests pass; `cargo test --workspace` green
- LocalDiskStore ready for Phase 2 metadata layer integration (BlockStore trait impl)
- MemDedupIndex ready for Phase 2 dedup pipeline integration (DedupIndex trait impl)
- No blockers for Phase 2

---
*Phase: 01-cas-foundation*
*Completed: 2026-03-28*

## Self-Check: PASSED

All created files verified present on disk. Both task commits confirmed in git log:
- 67383eb (Task 1: LocalDiskStore)
- 605d9da (Task 2: MemDedupIndex)
