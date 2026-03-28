---
gsd_state_version: 1.0
milestone: v1.0
milestone_name: milestone
status: planning
stopped_at: Phase 4 context gathered
last_updated: "2026-03-28T19:56:16.329Z"
last_activity: 2026-03-27 — Roadmap created; ready for Phase 1 planning
progress:
  total_phases: 7
  completed_phases: 3
  total_plans: 9
  completed_plans: 9
  percent: 0
---

# Project State

## Project Reference

See: .planning/PROJECT.md (updated 2026-03-27)

**Core value:** High-ratio data deduplication that works transparently as a real, daily-driver POSIX filesystem
**Current focus:** Phase 1 — CAS Foundation

## Current Position

Phase: 1 of 7 (CAS Foundation)
Plan: 0 of TBD in current phase
Status: Ready to plan
Last activity: 2026-03-27 — Roadmap created; ready for Phase 1 planning

Progress: [░░░░░░░░░░] 0%

## Performance Metrics

**Velocity:**
- Total plans completed: 0
- Average duration: —
- Total execution time: —

**By Phase:**

| Phase | Plans | Total | Avg/Plan |
|-------|-------|-------|----------|
| - | - | - | - |

**Recent Trend:**
- Last 5 plans: —
- Trend: —

*Updated after each plan completion*
| Phase 01-cas-foundation P01 | 12 | 2 tasks | 16 files |
| Phase 01-cas-foundation P02 | 7 | 2 tasks | 3 files |
| Phase 01-cas-foundation P03 | 6 | 2 tasks | 2 files |
| Phase 02-metadata-engine P01 | 25 | 2 tasks | 11 files |
| Phase 02-metadata-engine P02 | 6min | 2 tasks | 5 files |
| Phase 02-metadata-engine P03 | 10min | 2 tasks | 4 files |
| Phase 03-read-only-fuse P01 | 402 | 2 tasks | 6 files |
| Phase 03-read-only-fuse P02 | 8 | 2 tasks | 3 files |
| Phase 03-read-only-fuse P03 | 3min | 1 tasks | 3 files |

## Accumulated Context

### Decisions

Decisions are logged in PROJECT.md Key Decisions table.
Recent decisions affecting current work:

- Roadmap: Build bottom-up along dependency graph — CAS and chunking traits before metadata, metadata before FUSE, read-only FUSE before write path
- Roadmap: Refcount + WAL + GC co-developed in Phase 5 — GC correctness depends on refcount invariants; splitting them forces two correction cycles
- Roadmap: Compression and snapshots grouped in Phase 6 — both are natural CAS capabilities, not bolt-ons
- Roadmap: Windows deferred to Phase 7 — GPL-3 license implications of winfsp-rs must be resolved before distribution work begins
- [Phase 01-cas-foundation]: 01-01: ChunkHash uses Vec<u8> not [u8;32] — variable-width accommodates owner's unknown hash algorithm output size
- [Phase 01-cas-foundation]: 01-01: All traits use &self — enables Arc<dyn Trait> sharing; implementations handle sync internally
- [Phase 01-cas-foundation]: 01-01: slicefs-traits depends only on thiserror — adapter crates implement traits without pulling cas-local deps
- [Phase 01-cas-foundation]: 01-01: DedupIndex exposes bloom_check() separately from lookup() — explicit two-phase design from day one per CAS-07
- [Phase 01-cas-foundation]: 01-02: MemBlockStore write-time integrity check on put() — catches caller bugs where hash and data diverge before any storage occurs
- [Phase 01-cas-foundation]: 01-02: FixedChunker strategy_id() uses match on block_size for &'static str — trait requires &'static str; named constants cover 4096/8192; 'fixed-custom' fallback for others
- [Phase 01-cas-foundation]: 01-02: Empty input in FixedChunker returns Ok(vec\![]) — zero-length files are valid, no chunk emitted
- [Phase 01-cas-foundation]: AtomicBloomFilter used for DedupIndex insert() to satisfy &self trait requirement without Mutex wrapping
- [Phase 01-cas-foundation]: Atomic writes via .tmp + rename prevent partial block writes from appearing as valid CAS blocks
- [Phase 01-cas-foundation]: Bloom serialization via HashSet + rebuild: fastbloom serde not enabled, HashSet persisted and bloom rebuilt on load
- [Phase 02-metadata-engine]: blockset StorageAdd/StorageGet are private traits — intern_inode/load_inode use blockset::Dictionary directly
- [Phase 02-metadata-engine]: Digest224/Digest256/Branches redeclared as type aliases in slicefs-traits (not re-exported from private blockset modules)
- [Phase 02-metadata-engine]: InodeMeta defined in slicefs-traits as plain data struct — serialization lives in metadata crate
- [Phase 02-metadata-engine]: blockset::Tree must be in scope to call State::push_all (trait method not auto-imported)
- [Phase 02-metadata-engine]: Directory entry list stores (key, ino, name) tuples as CAS blob — blockset API does not allow choosing dictionary keys
- [Phase 02-metadata-engine]: DictMetadataStore xattr methods stub out for Plan 03; all other MetadataStore ops fully implemented
- [Phase 02-metadata-engine]: blockset::serialize/deserialize broken for small payloads — implemented own serialize_dictionary/deserialize_dictionary (92 bytes/entry: key+branches verbatim)
- [Phase 02-metadata-engine]: Root record expanded from 44 to 156 bytes: adds inode_data/dir_data/manifest_data/xattr_data digests enabling full state reconstruction via load_from_root
- [Phase 02-metadata-engine]: Xattr storage: load-mutate-re-intern pattern; list_xattrs returns empty vec for inodes with no xattrs
- [Phase 02-metadata-engine]: InodeMap::set_next_ino() added for O(1) counter restoration after reload
- [Phase 03-read-only-fuse]: fuser macos-no-mount feature: compiles without macFUSE install, provides full Filesystem API for unit tests
- [Phase 03-read-only-fuse]: All write FUSE callbacks return EROFS (not ENOSYS) — signals read-only filesystem per POSIX
- [Phase 03-read-only-fuse]: SliceFsFilesystem dual Arc: meta Arc<DictMetadataStore> + dict Arc<Mutex<Dictionary>> to avoid deadlock with DictMetadataStore's internal mutex
- [Phase 03-read-only-fuse]: dict() accessor exposes shared Dictionary for seed content ops; deadlock warning prevents misuse
- [Phase 03-read-only-fuse]: Seed uses same Dictionary for file content and metadata to avoid desync on persist
- [Phase 03-read-only-fuse]: fuser 0.17 Config is #[non_exhaustive] — use Config::default() + field mutation, not struct literal
- [Phase 03-read-only-fuse]: Dictionary cloned before load_from_root: load_from_root consumes dict; clone provides content_dict for SliceFsFilesystem
- [Phase 03-read-only-fuse]: Task 2 (end-to-end FUSE mount) deferred: requires Linux or macFUSE; not available on macOS dev machine

### Pending Todos

None yet.

### Blockers/Concerns

- Phase 2 planning: Owner's CDC algorithm interface must be clarified before the Chunker trait is finalized — it is the primary differentiator
- Phase 2 planning: redb 3.x API surface for inode table, manifest store, and chunk index requires validation against redb 3.x docs (breaking changes from 2.x)
- Phase 6 planning: WAL epoch-based deletion and two-phase commit for CAS refcounting are subtle; re-read USENIX FAST 2013 concurrent deletion paper before planning
- Phase 7 planning: fuser marks macOS as "untested" in README — practical validation with FUSE-T on macOS Sequoia required; FSKit (macOS 15+) may be better long-term path
- Phase 7 planning: winfsp-rs is GPL-3 — distribution license decision must precede any Windows implementation work

## Session Continuity

Last session: 2026-03-28T19:56:16.326Z
Stopped at: Phase 4 context gathered
Resume file: .planning/phases/04-full-posix-write-path/04-CONTEXT.md
