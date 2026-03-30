---
gsd_state_version: 1.0
milestone: v2.0
milestone_name: Streaming Writes & Hardening
status: executing
stopped_at: "Phase 08-01 complete: correctness fixes (refcount saturation + three-tier statfs + scrub saturated reporting)"
last_updated: "2026-03-30T03:54:51.619Z"
last_activity: "2026-03-29 — Plan 03 complete: all CLI consumers migrated to file-backed StoreIo; zero Dictionary references remain"
progress:
  total_phases: 12
  completed_phases: 9
  total_plans: 28
  completed_plans: 28
  percent: 0
---

# Project State

## Project Reference

See: .planning/PROJECT.md (updated 2026-03-29)

**Core value:** High-ratio data deduplication that works transparently as a real, daily-driver POSIX filesystem
**Current focus:** v2.0 — Streaming Writes & Hardening (Phase 7.1: FileStorage Migration)

## Current Position

Phase: 7.1 (FileStorage Migration — INSERTED, urgent)
Plan: 03 complete (phase COMPLETE)
Status: In progress
Last activity: 2026-03-29 — Plan 03 complete: all CLI consumers migrated to file-backed StoreIo; zero Dictionary references remain

Progress: [░░░░░░░░░░] 0%

## Performance Metrics

**Velocity (v1.0 history):**
- Total plans completed: 23
- Average duration: ~4 min/plan
- Total execution time: ~92 min

**v2.0 By Phase:**

| Phase | Plans | Total | Avg/Plan |
|-------|-------|-------|----------|
| 7.1. FileStorage Migration (INSERTED) | 0/? | - | - |
| 8. Correctness Fixes | 0/2 | - | - |
| 9. Compression Removal | 0/2 | - | - |
| 10. Streaming Writes Core | 0/3 | - | - |
| 11. Non-Sequential Write Handling | 0/2 | - | - |

*Updated after each plan completion*
| Phase 07.1-filestorage-migration P01 | 7 | 2 tasks | 7 files |
| Phase 07.1-filestorage-migration P02 | ~180 | 3 tasks | 11 files |
| Phase 07.1-filestorage-migration P03 | ~60 | 2 tasks | 19 files |
| Phase 08-correctness-fixes P01 | 20 | 2 tasks | 6 files |

## Accumulated Context

### Decisions

- v2.0 Roadmap: Bug fixes (Phase 8) before structural changes — closes data-loss risk before write-path invasive work
- v2.0 Roadmap: Compression removal (Phase 9) before streaming — both touch flush_buffer_to_cas; sequential isolation makes regressions unambiguous
- v2.0 Roadmap: Core streaming (Phase 10) before edge cases (Phase 11) — non-sequential fallback requires stable core to test against
- v2.0 Roadmap: No WAL checkpointing of partial streaming state — "no intermediate manifests" invariant; truncate-on-crash is sufficient; streaming State is O(log N) so terabyte files fit in memory
- [Phase 07.1-01]: directory.rs mixed read/write functions use S: StorageAdd + StorageGet combined bound (not split params) to avoid borrow conflict on same Dictionary
- [Phase 07.1-01]: Snapshot storage: dual HashMap indexes (by_version + by_name) for O(1) multi-key lookup vs prior O(N) Vec scan
- [Phase 07.1-02]: DictMetadataStore::new() requires Arc<Mutex<StoreIo>> — Default removed; callers must provide explicit storage backing
- [Phase 07.1-02]: Read-then-write pattern for directory ops: file_storage_get releases borrow before FileStorageAdd::new(io)
- [Phase 07.1-02]: GC background thread uses run_gc_roots_only() — Dictionary-free; live-set filtering from file storage deferred
- [Phase 07.1-02]: Crash-safe commit: FSA drop (flushes node files) THEN WAL RootUpdate write
- [Phase 07.1-03]: collect_live_set simplified to empty stub — FileStorage orphan file GC deferred (no DictEntry in segments to filter)
- [Phase 07.1-03]: Legacy store (dictionary.bin) → Re-seed error everywhere — no in-place migration
- [Phase 07.1-03]: statfs physical bytes from dir_size(vt0/) — replaces dict.len() * 92 formula
- [Phase 08-01]: saturating refcount: increment_refcount saturates at u64::MAX with tracing::warn; decrement is no-op at MAX (immortal blocks never GC'd)
- [Phase 08-01]: statfs three-tier: libc::statvfs for blocks/bfree/bavail; inode_count() for files; fallback to zeros when store_path is None
- [Phase 08-01]: compute_statfs extracted to regular impl block (not Filesystem trait) to allow pub visibility and test_statfs_values() helper

### Roadmap Evolution

- Phase 7.1 inserted after Phase 7: FileStorage Migration (URGENT) — switch DictMetadataStore from in-memory Dictionary to file-backed FileStorageAdd/file_storage_get. Eliminates ~67 GB RAM for 1 TB stores. Structurally solves FIX-03/FIX-04. Runs before Phase 8 correctness fixes.

### Pending Todos

None yet.

### Blockers/Concerns

- Phase 10: Dict lock acquisition sequence during flush_buffer_to_cas needs canonical ordering to prevent lock inversion — verify in Phase 10 planning (two separate Mutex<Dictionary> clones sharing underlying Arc)
- Phase 11: No existing test exercises writeback_cache out-of-order write delivery — must be written before Phase 11 is declared complete
- Post-v2.0: Mixed-version stores (v1/v2/v3 blocks) have no cross-epoch dedup path; a slicefs migrate-store command may be needed in v2.1+

## Session Continuity

Last session: 2026-03-30T03:54:51.616Z
Stopped at: Phase 08-01 complete: correctness fixes (refcount saturation + three-tier statfs + scrub saturated reporting)
Resume file: None
