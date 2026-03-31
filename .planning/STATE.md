---
gsd_state_version: 1.0
milestone: v2.0
milestone_name: Streaming Writes & Hardening
status: completed
stopped_at: Phase 12 context gathered
last_updated: "2026-03-31T23:05:45.160Z"
last_activity: "2026-03-30 — Plan 02 complete: 10 STRM-02 integration tests covering fallback, gap zero-fill, overlapping writes, buffered fsync/truncate/read, out-of-order delivery"
progress:
  total_phases: 13
  completed_phases: 12
  total_plans: 35
  completed_plans: 35
  percent: 100
---

# Project State

## Project Reference

See: .planning/PROJECT.md (updated 2026-03-29)

**Core value:** High-ratio data deduplication that works transparently as a real, daily-driver POSIX filesystem
**Current focus:** v2.0 — Streaming Writes & Hardening (Phase 7.1: FileStorage Migration)

## Current Position

Phase: 11 (Non-Sequential Write Handling)
Plan: 02 of 2 complete
Status: Complete
Last activity: 2026-03-30 — Plan 02 complete: 10 STRM-02 integration tests covering fallback, gap zero-fill, overlapping writes, buffered fsync/truncate/read, out-of-order delivery

Progress: [██████████] 100%

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
| Phase 09-compression-removal P01 | 14 | 2 tasks | 6 files |
| Phase 09-compression-removal P02 | 7 | 2 tasks | 9 files |
| Phase 10-streaming-writes-core P01 | 12 | 2 tasks | 6 files |
| Phase 10 P02 | 4 | 2 tasks | 2 files |
| Phase 10-streaming-writes-core P03 | 2 | 2 tasks | 2 files |
| Phase 11-non-sequential-write-handling P01 | 1 | 2 tasks | 3 files |
| Phase 11-non-sequential-write-handling P02 | 2 | 2 tasks | 1 files |

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
- [Phase 09-01]: v3 store format: raw bytes pushed directly to State::push_all, no compression header, no backward compat
- [Phase 09-01]: StoreStats.compressor field kept for JSON API stability; value changed to none (v3 raw)
- [Phase 09-02]: compression_tests.rs deleted entirely — tests v1/v2 wire format behavior that no longer exists; no migration tests needed
- [Phase 09-02]: cli.rs compressor unit tests removed — CLI flags (--compressor, --compressor-level) were removed in Plan 01
- [Phase 10-01]: FSA Drop flush + disk-read extend for streaming writes — FileStorageAdd now flushes pending internal nodes on drop and reads from disk in extend() when entries missing from map, enabling push_bytes/end across separate FSA sessions
- [Phase 10-01]: flush_buffer_for_fsync uses clone+end pattern — clone State, end the clone for CAS commit, keep original alive; no State reset on fsync
- [Phase 10-01]: Digest224 re-exported from blockset crate — was private module, now publicly accessible
- [Phase 10-01]: 2 non-sequential offset tests ignored for Phase 11 (STRM-02) — test_write_with_gap_zero_pads, test_file_write_at_offset_zero_pads
- [Phase 10]: [Phase 10-02]: flush_buffer_for_fsync tracks last_committed_root for decrement-on-overwrite refcount lifecycle
- [Phase 10]: [Phase 10-02]: test_release skips redundant manifest write when final digest matches last_committed_root (dedup optimization)
- [Phase 10]: [Phase 10-02]: test_read scans open_files values for matching ino (O(n) acceptable for typical handle counts)
- [Phase 10]: [Phase 10-02]: FUSE read() delegates entirely to test_read -- single code path for committed and uncommitted reads
- [Phase 10]: [Phase 10-02]: flush_buffer_to_cas removed -- release path handles State.end() directly
- [Phase 10-streaming-writes-core]: Truncate to 0 is a fast path: State::default() reset without materialization
- [Phase 10-streaming-writes-core]: Truncate to N>0 materializes via clone+end, resizes Vec, pushes into fresh State
- [Phase 10-streaming-writes-core]: last_committed_root.take() on truncate prevents refcount leaks from prior fsyncs

- [Phase 11-01]: WriteMode enum variants embed mode-specific data (State in Streaming, Vec<u8> in Buffered) making illegal states unrepresentable
- [Phase 11-01]: Fallback transition uses lock-release-reacquire pattern: clone State under open_files, release, materialize under io, re-acquire open_files to swap
- [Phase 11-01]: byte_count == 0 fallback loads committed manifest content to prevent existing file data loss (Research Pitfall 1)
- [Phase 11-01]: Buffered read serves directly from buf clone (no CAS roundtrip needed)
- [Phase 11-01]: Buffered truncate uses simple buf.resize() instead of materialize+repush
- [Phase 11-02]: test_pwrite_existing_file_preserves_content skipped (no test_open helper for reopening committed files); new-file pwrite variant covers zero-fill path
- [Phase 11-02]: Out-of-order writes test (STRM-02i) writes first chunk at offset 30 to trigger immediate fallback on brand new file

### Roadmap Evolution

- Phase 7.1 inserted after Phase 7: FileStorage Migration (URGENT) — switch DictMetadataStore from in-memory Dictionary to file-backed FileStorageAdd/file_storage_get. Eliminates ~67 GB RAM for 1 TB stores. Structurally solves FIX-03/FIX-04. Runs before Phase 8 correctness fixes.
- Phase 12 added: Add SMB/FSKit backend support for FUSE-T — FUSE-T NFS backend has confirmed macOS kernel bug (Issue #45) causing cp hangs. SMB backend (1.0.35+) and FSKit backend (macOS 26, 1.2.0) bypass this.

### Pending Todos

None yet.

### Blockers/Concerns

- Phase 10: Dict lock acquisition sequence during flush_buffer_to_cas needs canonical ordering to prevent lock inversion — verify in Phase 10 planning (two separate Mutex<Dictionary> clones sharing underlying Arc)
- Post-v2.0: Mixed-version stores (v1/v2/v3 blocks) have no cross-epoch dedup path; a slicefs migrate-store command may be needed in v2.1+

## Session Continuity

Last session: 2026-03-31T23:05:45.152Z
Stopped at: Phase 12 context gathered
Resume file: .planning/phases/12-add-smb-fskit-backend-support-for-fuse-t/12-CONTEXT.md
