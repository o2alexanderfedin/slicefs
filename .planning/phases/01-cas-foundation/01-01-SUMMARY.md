---
phase: 01-cas-foundation
plan: 01
subsystem: cas
tags: [rust, cargo, workspace, traits, cas, content-addressable-storage, blake3, fastbloom, thiserror]

# Dependency graph
requires: []
provides:
  - ContentHasher trait (CAS-01) — pluggable hash function interface with Send+Sync and &self
  - Chunker trait (CAS-02) — buffered block-splitting interface, determinism-guaranteed
  - BlockStore trait (CAS-03, CAS-05) — CAS block storage with verify_on_read integrity hook
  - DedupIndex trait (CAS-07) — bloom pre-filter + on-disk index with bounded memory
  - CasError enum — typed errors: NotFound, IntegrityFailure, HashCollision, Io, Chunker, Index
  - ChunkHash newtype — variable-width, hex-displayable, Hash+Eq on content not pointer
  - Cargo workspace with resolver=2 and all Phase 1 workspace dependencies locked
  - cas-local crate skeleton with five module stubs for Phase 2/3 implementations
affects:
  - 01-02 (stub implementations depend on these traits)
  - 01-03 (disk implementations depend on these traits)
  - all subsequent phases (every component implements or consumes these traits)

# Tech tracking
tech-stack:
  added:
    - thiserror 2.0.18 — typed domain errors via derive macro
    - blake3 1.8.3 — stub ContentHasher (wired in cas-local, not yet implemented)
    - fastbloom 0.14.1 — bloom filter for DedupIndex (wired in cas-local, not yet implemented)
    - proptest 1.x — property-based testing (dev-dep in cas-local, used in later plans)
    - tempfile 3.x — temp directories for disk store tests (dev-dep in cas-local)
  patterns:
    - Trait-driven dependency injection: all CAS operations behind abstract traits
    - Zero-dependency traits crate: dedupfs-traits has only thiserror, enabling adapter crates without pulling in implementation deps
    - Variable-width ChunkHash newtype (Vec<u8>): accommodates any hash output width
    - &self trait methods with internal synchronization pattern for Arc<dyn Trait> sharing
    - Bloom pre-filter + authoritative lookup two-phase DedupIndex design (prevents ZFS DDT memory explosion)

key-files:
  created:
    - Cargo.toml — workspace root, resolver=2, all workspace dependencies
    - .cargo/config.toml — project-level rustc override (fixes rust-fv-driver toolchain mismatch)
    - crates/dedupfs-traits/Cargo.toml — traits crate manifest, thiserror only
    - crates/dedupfs-traits/src/lib.rs — pub mod declarations + pub use re-exports
    - crates/dedupfs-traits/src/error.rs — CasError enum with 6 variants
    - crates/dedupfs-traits/src/hash.rs — ChunkHash newtype + ContentHasher trait
    - crates/dedupfs-traits/src/chunk.rs — Chunk struct + Chunker trait
    - crates/dedupfs-traits/src/block_store.rs — BlockStoreConfig + BlockStore trait
    - crates/dedupfs-traits/src/dedup_index.rs — DedupResult enum + DedupIndex trait
    - crates/cas-local/Cargo.toml — stub impl crate, depends on dedupfs-traits via path
    - crates/cas-local/src/lib.rs — module declarations for 5 stub modules
    - crates/cas-local/src/blake3_hasher.rs — placeholder
    - crates/cas-local/src/fixed_chunker.rs — placeholder
    - crates/cas-local/src/mem_block_store.rs — placeholder
    - crates/cas-local/src/disk_block_store.rs — placeholder
    - crates/cas-local/src/mem_dedup_index.rs — placeholder
  modified: []

key-decisions:
  - "ChunkHash uses Vec<u8> not [u8; 32] — variable width accommodates owner's unknown hash output size"
  - "All traits use &self not &mut self — enables Arc<dyn Trait> sharing without external locking"
  - "dedupfs-traits depends only on thiserror — adapter crates can implement traits without cas-local deps"
  - "DedupIndex exposes bloom_check() separately from lookup() — two-phase design from day one per CAS-07"
  - "verify_on_read=true by default in BlockStoreConfig — secure-by-default for CAS-05 integrity"
  - "Project-level .cargo/config.toml added to override broken global rust-fv-driver setting"

patterns-established:
  - "Pattern 1 (Trait Definition in Zero-Dependency Crate): all trait definitions live in dedupfs-traits with no external deps beyond thiserror; every other crate depends on dedupfs-traits"
  - "Pattern 2 (Variable-Width Hash Newtype): ChunkHash(Vec<u8>) not fixed-size array — never break owner adapter for different hash widths"
  - "Pattern 3 (Bloom Pre-Filter + Authoritative Lookup): DedupIndex exposes bloom_check() for fast path, lookup() for authoritative path; two separate methods from day one"
  - "Pattern 4 (&self methods with internal sync): all trait methods take &self; implementations use Mutex/RwLock; trait objects sharable via Arc<dyn Trait>"

requirements-completed: [CAS-01, CAS-02, CAS-03, CAS-05, CAS-07]

# Metrics
duration: 12min
completed: 2026-03-27
---

# Phase 1 Plan 01: CAS Foundation Summary

**Four sync CAS trait interfaces (ContentHasher, Chunker, BlockStore, DedupIndex) defined in a zero-dependency dedupfs-traits crate with ChunkHash newtype, CasError enum, and cas-local skeleton wired for Phase 2/3 stub implementations**

## Performance

- **Duration:** 12 min
- **Started:** 2026-03-27T22:33:52Z
- **Completed:** 2026-03-27T22:46:00Z
- **Tasks:** 2 of 2
- **Files modified:** 16

## Accomplishments

- Cargo workspace initialized with resolver=2 and all Phase 1 library versions locked (thiserror 2, blake3 1.8.3, fastbloom 0.14.1, proptest, tempfile, serde, bincode, tracing, criterion)
- Four trait interfaces defined with complete doc comments: ContentHasher (CAS-01), Chunker (CAS-02), BlockStore (CAS-03/CAS-05), DedupIndex (CAS-07)
- CasError enum with all six variants including HashCollision (catastrophic) and IntegrityFailure (corruption detection)
- cas-local crate skeleton created with five placeholder modules ready for Plan 02/03 implementations

## Task Commits

Each task was committed atomically:

1. **Task 1: Initialize workspace and dedupfs-traits with all trait definitions** - `41ececf` (feat)
2. **Task 2: Create cas-local skeleton with dependency wiring** - `07f8175` (feat)

**Plan metadata:** (pending)

## Files Created/Modified

- `Cargo.toml` — workspace root, resolver=2, all workspace dependency versions
- `.cargo/config.toml` — project-level rustc override for toolchain fix
- `crates/dedupfs-traits/Cargo.toml` — minimal traits crate (thiserror only)
- `crates/dedupfs-traits/src/lib.rs` — pub mod + pub use re-exports for ergonomic imports
- `crates/dedupfs-traits/src/error.rs` — CasError with NotFound, IntegrityFailure, HashCollision, Io, Chunker, Index
- `crates/dedupfs-traits/src/hash.rs` — ChunkHash newtype + ContentHasher trait
- `crates/dedupfs-traits/src/chunk.rs` — Chunk struct + Chunker trait
- `crates/dedupfs-traits/src/block_store.rs` — BlockStoreConfig (verify_on_read=true default) + BlockStore trait
- `crates/dedupfs-traits/src/dedup_index.rs` — DedupResult enum + DedupIndex trait with two-phase bloom design
- `crates/cas-local/Cargo.toml` — depends on dedupfs-traits path, blake3, fastbloom, thiserror
- `crates/cas-local/src/lib.rs` — module declarations for 5 stub modules
- `crates/cas-local/src/blake3_hasher.rs` — placeholder (Plan 02)
- `crates/cas-local/src/fixed_chunker.rs` — placeholder (Plan 02)
- `crates/cas-local/src/mem_block_store.rs` — placeholder (Plan 02)
- `crates/cas-local/src/disk_block_store.rs` — placeholder (Plan 03)
- `crates/cas-local/src/mem_dedup_index.rs` — placeholder (Plan 03)

## Decisions Made

- **ChunkHash uses Vec<u8>** — variable-width accommodates owner's unknown hash output size; fixed `[u8; 32]` would break when owner's algorithm uses a different width
- **All traits use &self** — enables `Arc<dyn Trait>` sharing; implementations handle sync internally with Mutex/RwLock
- **dedupfs-traits depends only on thiserror** — adapter crates for the owner's algorithms can implement traits without pulling in cas-local's blake3/fastbloom deps
- **DedupIndex exposes bloom_check() separately from lookup()** — explicit two-phase design from day one per CAS-07; prevents ZFS DDT memory explosion
- **verify_on_read=true by default** — secure-by-default; callers opt-out explicitly for performance-critical paths

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 - Blocking] Overrode broken global rust-fv-driver via project-level .cargo/config.toml**
- **Found during:** Task 1 (first cargo check attempt)
- **Issue:** Global `~/.cargo/config.toml` sets `rustc = "rust-fv-driver"` (formal verification tool) which was compiled against rustc 1.93.0; toolchain update to 1.94.1 broke the dylib link, causing all cargo commands to SIGABRT
- **Fix:** Created `/Users/alexanderfedin/Projects/file-systems/.cargo/config.toml` with `rustc = "rustc"` to override the global setting for this project
- **Files modified:** `.cargo/config.toml` (created)
- **Verification:** `cargo check -p dedupfs-traits` and `cargo check --workspace` both passed after fix
- **Committed in:** 41ececf (Task 1 commit)

---

**Total deviations:** 1 auto-fixed (1 blocking environment issue)
**Impact on plan:** Necessary to unblock all cargo operations. No scope creep. The project-local override is the correct fix — it does not affect other projects using the global setting.

## Issues Encountered

- `rust-fv-driver` binary at `~/.cargo/bin/rust-fv-driver` linked against old rustc dylib `librustc_driver-f9453740c55d2f61.dylib` (1.93.0). After `rustup update stable` to 1.94.1, only `librustc_driver-f9c8b388b33f1d3d.dylib` exists. Project-level cargo config override was the correct fix.

## User Setup Required

None — no external service configuration required.

## Next Phase Readiness

- All four trait interfaces compiled and verified
- `cargo check --workspace` passes with zero errors
- cas-local module structure in place for Plan 02 (in-memory stub implementations: Blake3Hasher, FixedChunker, MemBlockStore)
- cas-local module structure in place for Plan 03 (disk implementations: LocalDiskStore, MemDedupIndex with bloom)
- No blockers for Plan 02

---
*Phase: 01-cas-foundation*
*Completed: 2026-03-27*

## Self-Check: PASSED

All 15 created files verified present on disk. Both task commits confirmed in git log:
- 41ececf (Task 1: workspace + traits)
- 07f8175 (Task 2: cas-local skeleton)
