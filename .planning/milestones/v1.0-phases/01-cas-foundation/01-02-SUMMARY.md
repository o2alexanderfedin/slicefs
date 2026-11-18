---
phase: 01-cas-foundation
plan: 02
subsystem: cas
tags: [rust, blake3, chunker, mem-store, proptest, content-addressable-storage, integrity, tdd]

# Dependency graph
requires:
  - phase: 01-cas-foundation
    plan: 01
    provides:
      - ContentHasher trait (CAS-01)
      - Chunker trait (CAS-02)
      - BlockStore trait + BlockStoreConfig (CAS-03, CAS-05)
      - ChunkHash newtype
      - CasError enum
      - cas-local crate skeleton with placeholder modules
provides:
  - Blake3Hasher (ContentHasher) — stateless BLAKE3 impl, 32-byte ChunkHash, algorithm_id="blake3"
  - FixedChunker (Chunker) — configurable block size, Default=4096, strategy_id="fixed-4096"
  - MemBlockStore (BlockStore) — RwLock<HashMap> store with write/read-time integrity verification
  - Trait pluggability proven via swap tests for &dyn ContentHasher and &dyn Chunker (CAS-01, CAS-02)
  - Integrity verification code path exercised for both write (IntegrityFailure) and read (verify_on_read=true) (CAS-05)
affects:
  - 01-03 (disk implementations build on same trait contracts and can use MemBlockStore as reference)
  - all subsequent phases (these are the reference implementations for testing)

# Tech tracking
tech-stack:
  added: []  # All dependencies already wired in cas-local/Cargo.toml by Plan 01
  patterns:
    - TDD: tests co-located in #[cfg(test)] modules within each implementation file
    - Swap tests: minimal stub structs (AltHasher, LargeBlockChunker) in test modules prove &dyn dispatch
    - Property-based tests via proptest: round-trip and structural invariant coverage
    - Internal synchronization pattern: RwLock<HashMap> for &self trait methods enabling Arc<dyn BlockStore>
    - Write-time integrity: hash(data) compared to provided hash before insert in put()
    - Idempotent delete: store.remove() without NotFound check (HashMap::remove is safe to call on missing keys)

key-files:
  created: []
  modified:
    - crates/cas-local/src/blake3_hasher.rs — Blake3Hasher impl + 6 unit tests including swap test
    - crates/cas-local/src/fixed_chunker.rs — FixedChunker impl + 7 unit tests + 2 proptest tests
    - crates/cas-local/src/mem_block_store.rs — MemBlockStore impl + 11 unit tests + 1 proptest test

key-decisions:
  - "MemBlockStore write-time integrity check on put() — catches caller bugs where hash and data diverge before any storage occurs"
  - "FixedChunker strategy_id() uses match on block_size for &'static str — trait requires &'static str; named constants cover 4096/8192; 'fixed-custom' fallback for others"
  - "Empty input in FixedChunker returns Ok(vec![]) not an error — zero-length files are valid; no chunk emitted"
  - "HashCollision detection in put() compares stored bytes to incoming bytes when hash already exists — distinct from IntegrityFailure which checks hash against data"

patterns-established:
  - "Pattern 5 (Co-located tests with swap proofs): #[cfg(test)] modules contain both unit tests and minimal swap-struct helpers; no separate test files needed for unit coverage"
  - "Pattern 6 (Write-time integrity before insert): always hash(data) and compare to provided hash at the top of put(); prevents garbage entering the store"

requirements-completed: [CAS-01, CAS-02, CAS-05]

# Metrics
duration: 7min
completed: 2026-03-28
---

# Phase 1 Plan 02: Stub Implementations Summary

**Blake3Hasher, FixedChunker, and MemBlockStore implemented with 40 passing tests (unit + proptest + swap + concurrency), proving CAS trait pluggability and integrity verification code paths**

## Performance

- **Duration:** 7 min
- **Started:** 2026-03-28T06:31:48Z
- **Completed:** 2026-03-28T06:38:56Z
- **Tasks:** 2 of 2
- **Files modified:** 3

## Accomplishments

- Blake3Hasher: stateless ContentHasher producing 32-byte ChunkHash, deterministic, swap-testable via &dyn ContentHasher
- FixedChunker: configurable block size (Default=4096), empty-safe, round-trip verified, swap-testable via &dyn Chunker, with proptest coverage
- MemBlockStore: RwLock<HashMap> store with write-time integrity check, read-time re-hash option (verify_on_read), hash collision detection, idempotent put/delete, and thread-safety verified via std::thread::scope
- Full workspace green: 40 tests pass across all three modules (plus disk_block_store tests that already existed)

## Task Commits

Each task was committed atomically:

1. **Task 1: Implement Blake3Hasher and FixedChunker with tests** - `60ec921` (feat)
2. **Task 2: Implement MemBlockStore with integrity verification and tests** - `893f6b9` (feat)

**Plan metadata:** (pending — final docs commit)

## Files Created/Modified

- `crates/cas-local/src/blake3_hasher.rs` — Blake3Hasher: ContentHasher impl + 6 tests (determinism, empty, size, algorithm_id, swap)
- `crates/cas-local/src/fixed_chunker.rs` — FixedChunker: Chunker impl + 7 unit tests + 2 proptest tests (round-trip, offsets)
- `crates/cas-local/src/mem_block_store.rs` — MemBlockStore: BlockStore impl + 11 unit tests + 1 proptest test (round-trip)

## Decisions Made

- **Write-time integrity in put()** — hash(data) compared against provided hash before insert; returns IntegrityFailure immediately on mismatch. Catches the class of bugs where caller computes hash on different bytes than what they store.
- **FixedChunker strategy_id() match on block_size** — trait requires `&'static str`; a `match` on common sizes (4096, 8192) covers the normal cases with readable labels; "fixed-custom" fallback handles arbitrary sizes without unsafe code.
- **Empty input in FixedChunker is Ok(vec![])** — zero-length files are valid inputs; the defined behavior is "no chunks", not an error.
- **HashCollision vs IntegrityFailure distinction** — HashCollision: same hash, different data already stored (catastrophic, data cannot be de-duplicated safely). IntegrityFailure: hash does not match data (caller error or corruption). Both cases tested.

## Deviations from Plan

None - plan executed exactly as written.

## Issues Encountered

None.

## User Setup Required

None — no external service configuration required.

## Next Phase Readiness

- Blake3Hasher, FixedChunker, and MemBlockStore all compiled and tested
- `cargo test --workspace` passes with zero errors (40 tests)
- cas-local module structure ready for Plan 03 (disk implementations: LocalDiskStore, MemDedupIndex with bloom)
- No blockers for Plan 03

---
*Phase: 01-cas-foundation*
*Completed: 2026-03-28*

## Self-Check: PASSED

All 3 modified files verified present on disk. Both task commits confirmed in git log:
- 60ec921 (Task 1: Blake3Hasher + FixedChunker)
- 893f6b9 (Task 2: MemBlockStore)
