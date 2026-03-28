# Phase 1: CAS Foundation - Context

**Gathered:** 2026-03-27
**Status:** Ready for planning

<domain>
## Phase Boundary

Define filesystem-oriented trait interfaces for content-addressable storage (ContentHasher, Chunker, BlockStore, Dedup Index) and build stub/test implementations. The owner's existing Rust algorithm crates will be integrated in a separate dedicated phase — Phase 1 produces the trait contracts and test harness, not the production implementations.

Requirements: CAS-01, CAS-02, CAS-03, CAS-05, CAS-07

</domain>

<decisions>
## Implementation Decisions

### Block storage layout
- Owner has existing Rust crates that provide the CAS API, block storage, addressing, hashing, chunking, and dedup index
- These algorithms are in separate published Rust crates
- Phase 1 does NOT integrate the owner's algorithms — it defines filesystem-side trait interfaces with stub/test implementations
- The actual storage layout, addressing scheme, and dedup index design will come from the owner's algorithms via adapters in a later dedicated phase

### Integration strategy
- Dedicated phase(s) will be added to the roadmap for reviewing the owner's algorithms and designing/building adapters to fit them behind the filesystem traits
- This is a deliberate "traits first, integration second" approach — the trait interfaces are designed for filesystem needs, and adapters bridge to the owner's CAS algorithms

### Crate organization
- Cargo workspace with subcrates per component
- Initial subcrates: cas-traits, cas-local (stub/test implementations), metadata, fuse-frontend, plus more as needed
- Crate naming convention: Claude's discretion (recommend dedupfs-* prefix for clarity)

### Trait API shape
- Sync traits first — owner's existing algorithms are synchronous, and fuser uses sync callbacks (thread-per-request model)
- Async traits/wrappers will be added when distributed backends (v2) introduce network I/O
- Buffered I/O (whole block as Vec<u8> or &[u8]) — driven by fuser's byte-slice callback model
- No premature async refactoring of owner's algorithms

### Claude's Discretion
- Error handling strategy (recommend thiserror for typed errors + anyhow for application code)
- Crate naming convention (recommend dedupfs-* prefix)
- Stub implementation details (in-memory HashMap-based BlockStore for testing)
- Test harness design and property-based testing approach

</decisions>

<code_context>
## Existing Code Insights

### Reusable Assets
- Owner has separate published Rust crates for CAS algorithms (hashing, chunking, block storage, dedup index, addressing)
- These crates are sync and may need adapters to fit filesystem-specific trait interfaces

### Established Patterns
- No existing codebase patterns yet (greenfield project)
- Owner's crates establish the pattern of separate, focused Rust crates — workspace approach is consistent

### Integration Points
- Trait interfaces defined in cas-traits crate will be the integration surface for owner's algorithms
- Adapter crate(s) will bridge owner's CAS crate APIs to filesystem trait interfaces

</code_context>

<specifics>
## Specific Ideas

- Owner emphasized: "Do not make assumptions on chunking and hashing — I'll give you repository with algorithms later"
- The algorithms "potentially might not be suitable as they are to fit into the file system" — adapters are expected, not direct use
- Owner's CAS covers more than just hashing/chunking — it includes the storage layout, addressing scheme, and dedup detection
- Phase 1 should produce trait interfaces that are flexible enough to accommodate the owner's algorithms without having seen them yet

</specifics>

<deferred>
## Deferred Ideas

- **Roadmap adjustment needed:** Add dedicated phase(s) for reviewing owner's existing algorithm crates and building filesystem adapters. This should be inserted between Phase 1 (traits) and the phase that first needs production implementations. Suggested: Phase 1.5 or replace current Phase 1 scope and shift numbering.
- Owner's algorithms may need async refactoring for distributed backends — defer to v2 milestone

</deferred>

---

*Phase: 01-cas-foundation*
*Context gathered: 2026-03-27*
