---
gsd_state_version: 1.0
milestone: v1.0
milestone_name: milestone
status: planning
stopped_at: Completed 01-cas-foundation-03-PLAN.md
last_updated: "2026-03-28T06:42:03.354Z"
last_activity: 2026-03-27 — Roadmap created; ready for Phase 1 planning
progress:
  total_phases: 7
  completed_phases: 1
  total_plans: 3
  completed_plans: 3
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
- [Phase 01-cas-foundation]: 01-01: dedupfs-traits depends only on thiserror — adapter crates implement traits without pulling cas-local deps
- [Phase 01-cas-foundation]: 01-01: DedupIndex exposes bloom_check() separately from lookup() — explicit two-phase design from day one per CAS-07
- [Phase 01-cas-foundation]: 01-02: MemBlockStore write-time integrity check on put() — catches caller bugs where hash and data diverge before any storage occurs
- [Phase 01-cas-foundation]: 01-02: FixedChunker strategy_id() uses match on block_size for &'static str — trait requires &'static str; named constants cover 4096/8192; 'fixed-custom' fallback for others
- [Phase 01-cas-foundation]: 01-02: Empty input in FixedChunker returns Ok(vec\![]) — zero-length files are valid, no chunk emitted
- [Phase 01-cas-foundation]: AtomicBloomFilter used for DedupIndex insert() to satisfy &self trait requirement without Mutex wrapping
- [Phase 01-cas-foundation]: Atomic writes via .tmp + rename prevent partial block writes from appearing as valid CAS blocks
- [Phase 01-cas-foundation]: Bloom serialization via HashSet + rebuild: fastbloom serde not enabled, HashSet persisted and bloom rebuilt on load

### Pending Todos

None yet.

### Blockers/Concerns

- Phase 2 planning: Owner's CDC algorithm interface must be clarified before the Chunker trait is finalized — it is the primary differentiator
- Phase 2 planning: redb 3.x API surface for inode table, manifest store, and chunk index requires validation against redb 3.x docs (breaking changes from 2.x)
- Phase 6 planning: WAL epoch-based deletion and two-phase commit for CAS refcounting are subtle; re-read USENIX FAST 2013 concurrent deletion paper before planning
- Phase 7 planning: fuser marks macOS as "untested" in README — practical validation with FUSE-T on macOS Sequoia required; FSKit (macOS 15+) may be better long-term path
- Phase 7 planning: winfsp-rs is GPL-3 — distribution license decision must precede any Windows implementation work

## Session Continuity

Last session: 2026-03-28T06:42:03.351Z
Stopped at: Completed 01-cas-foundation-03-PLAN.md
Resume file: None
