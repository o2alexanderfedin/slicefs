---
gsd_state_version: 1.0
milestone: v1.0
milestone_name: milestone
status: planning
stopped_at: Completed 07-03-PLAN.md - CI pipeline and benchmark infrastructure
last_updated: "2026-03-29T20:52:00.714Z"
last_activity: 2026-03-27 — Roadmap created; ready for Phase 1 planning
progress:
  total_phases: 7
  completed_phases: 7
  total_plans: 24
  completed_plans: 24
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
| Phase 04-full-posix-write-path P01 | 249 | 2 tasks | 4 files |
| Phase 04-full-posix-write-path P03 | 273 | 2 tasks | 2 files |
| Phase 04-full-posix-write-path P02 | 15 | 2 tasks | 3 files |
| Phase 04-full-posix-write-path P04 | 20 | 2 tasks | 5 files |
| Phase 05-crash-safety-and-gc P01 | 8min | 2 tasks | 12 files |
| Phase 05-crash-safety-and-gc P03 | 20min | 2 tasks | 7 files |
| Phase 05-crash-safety-and-gc P02 | 45min | 2 tasks | 12 files |
| Phase 05-crash-safety-and-gc P04 | 6min | 2 tasks | 7 files |
| Phase 06-compression-and-snapshots P01 | 202s | 2 tasks | 8 files |
| Phase 06-compression-and-snapshots P03 | 8min | 2 tasks | 14 files |
| Phase 06-compression-and-snapshots P04 | 14min | 2 tasks | 9 files |
| Phase 06-compression-and-snapshots P02 | 863s | 2 tasks | 12 files |
| Phase 07-cross-platform-and-production-hardening P01 | 149s | 2 tasks | 4 files |
| Phase 07-cross-platform-and-production-hardening P02 | 421s | 2 tasks | 7 files |
| Phase 07-cross-platform-and-production-hardening P03 | 155s | 2 tasks | 7 files |

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
- [Phase 04-full-posix-write-path]: Root record expanded from 156 to 184 bytes: adds refcount_data_digest as 7th Digest224; 156-byte records accepted as backward-compat (empty refcounts)
- [Phase 04-full-posix-write-path]: OpenFlags.acc_mode() used for write-mode detection — fuser 0.17 OpenFlags is newtype i32 with no bitfield methods
- [Phase 04-full-posix-write-path]: MountOption::RO removed: filesystem mounts read-write; destroy() persists dictionary.bin + root.bin to store_path
- [Phase 04-full-posix-write-path]: simulate_rename uses raw u32 flags bits (0/1/2) — RENAME_NOREPLACE/EXCHANGE are linux-only; raw bits portable for macOS test env
- [Phase 04-full-posix-write-path]: simulate_mkdir delegates to DictMetadataStore::create_directory — it handles nlinks/dot-entries/parent-update internally
- [Phase 04-full-posix-write-path]: Integration tests in separate tests/*.rs files compile independently — 04-02 RED phase errors do not block 04-03 tests
- [Phase 04-full-posix-write-path]: test_* helpers bypass FUSE request/reply machinery — public impl methods on SliceFsFilesystem enable integration tests without mounting
- [Phase 04-full-posix-write-path]: flush_buffer_to_cas() shared helper: dict lock dropped before meta.* calls to prevent deadlock; called by both test_release and FUSE release()
- [Phase 04-full-posix-write-path]: setattr size: in-flight buffer resize for open handles; CAS read-resize-push for closed files with refcount update
- [Phase 04-full-posix-write-path]: logical_bytes uses AtomicU64 for lock-free counter maintenance; update_inode reads old size before overwriting digest for delta tracking
- [Phase 04-full-posix-write-path]: fuser 0.17 getlk/setlk already return ENOSYS by default — no explicit stubs needed for POSIX locking
- [Phase 04-full-posix-write-path]: statfs bfree = u64::MAX/4 — dedup filesystem is effectively unlimited; physical = dict.len() * 92 bytes
- [Phase 05-crash-safety-and-gc]: SegmentEntry defined in segment/mod.rs (not reader.rs) — single source of truth for segment types
- [Phase 05-crash-safety-and-gc]: PerOpWal uses Mutex<SegmentWriter> — WalStrategy requires Send+Sync; Mutex provides interior mutability
- [Phase 05-crash-safety-and-gc]: PeriodicWal structurally identical to FlushOnFsyncWal — background timer wiring deferred to Plan 04 GC thread
- [Phase 05-crash-safety-and-gc]: GC mark_reachable uses blockset::to_digest224 for Digest256→Digest224 child conversion, filtering data leaves automatically
- [Phase 05-crash-safety-and-gc]: current_root() uses Mutex<Option<Digest224>> last_root field in DictMetadataStore, updated by commit()
- [Phase 05-crash-safety-and-gc]: GcHandle::drop sets shutdown flag but does not join — avoids blocking in drop(); explicit shutdown() joins
- [Phase 05-crash-safety-and-gc]: Snapshot-delta WAL logging: BTreeSet snapshot of keys before intern_*, log new entries after — no need to modify intern_* functions
- [Phase 05-crash-safety-and-gc]: flush_buffer_for_fsync uses mem::take to atomically remove buffer content while keeping fh in open_files
- [Phase 05-crash-safety-and-gc]: destroy() calls shutdown_wal() replacing dictionary.bin write — WAL segment is the persistence mechanism
- [Phase 05-crash-safety-and-gc]: set_wal() bootstraps WAL with all existing dict entries: DictMetadataStore::new() creates initial ino=1 dict entries before WAL is set; without bootstrap, crash before first explicit commit loses these entries
- [Phase 05-crash-safety-and-gc]: Offline GC checks mount.lock before running to prevent concurrent modification with an active mount
- [Phase 05-crash-safety-and-gc]: Background GC: GcHandle stored for FUSE session duration, shutdown() called after mount2 returns but before MountLock drops
- [Phase 06-compression-and-snapshots]: AlgorithmId::Raw for incompressible data: blocks never inflated even when compressor is active
- [Phase 06-compression-and-snapshots]: compress_block falls back to Raw on compress error: wire format always writable
- [Phase 06-compression-and-snapshots]: NoneCompressor rejects Zstd/Lz4 in decompress: explicit error for cross-compressor reads
- [Phase 06-compression-and-snapshots]: SnapshotRecord as SegmentEntry variant (0x03): crash-safe via WAL, no new file format
- [Phase 06-compression-and-snapshots]: load_store_from_segments returns 3-tuple (dict, root, snapshots): reconstructed on replay
- [Phase 06-compression-and-snapshots]: snapshot_roots() for GC multi-root anchoring: all snapshot roots + current live root
- [Phase 06-compression-and-snapshots]: commit_root() on DictMetadataStore writes RootUpdate WAL entry without full re-commit: enables snapshot switch to redirect live root cheaply
- [Phase 06-compression-and-snapshots]: to_wire_bytes/from_wire_bytes helpers centralise store_version gating: fix v1 write path that incorrectly added compression header byte
- [Phase 06-compression-and-snapshots]: Background GC uses snapshot_roots() replacing current_root(): single-line change; snapshot_roots() returns current root + all snapshot roots
- [Phase 06-compression-and-snapshots]: store_version gates both write and read paths: <2 = raw, >=2 = compression header
- [Phase 06-compression-and-snapshots]: to_wire_bytes/from_wire_bytes helpers centralize store_version gating across all write/read sites
- [Phase 07-cross-platform-and-production-hardening]: SessionACL::All used for allow_other (fuser 0.17 has no MountOption::AllowOther)
- [Phase 07-cross-platform-and-production-hardening]: direct_io via MountOption::CUSTOM on macOS only to fix FUSE-T NFS page cache staleness (issue #45)
- [Phase 07-cross-platform-and-production-hardening]: SHA-224 verification in scrub uses SHA224.compress directly: blockset::compress does data concatenation for small inputs, not SHA-224; all dictionary keys are always SHA-224 hashes
- [Phase 07-cross-platform-and-production-hardening]: Stats works on mounted stores (read-only scan, warns on mount.lock): allows monitoring live filesystems without lock refusal
- [Phase 07-cross-platform-and-production-hardening]: pjdfstest uses -p PATH CLI flag; no --skip; root-only tests auto-skip; pass rate denominator excludes skipped tests
- [Phase 07-cross-platform-and-production-hardening]: macos-14 pinned in CI (not macos-latest) to prevent FUSE-T breakage on macOS 15
- [Phase 07-cross-platform-and-production-hardening]: Benchmark targets documented (not CI-gated): 200 MB/s sequential write, hardware variance precludes automation

### Pending Todos

None yet.

### Blockers/Concerns

- Phase 2 planning: Owner's CDC algorithm interface must be clarified before the Chunker trait is finalized — it is the primary differentiator
- Phase 2 planning: redb 3.x API surface for inode table, manifest store, and chunk index requires validation against redb 3.x docs (breaking changes from 2.x)
- Phase 6 planning: WAL epoch-based deletion and two-phase commit for CAS refcounting are subtle; re-read USENIX FAST 2013 concurrent deletion paper before planning
- Phase 7 planning: fuser marks macOS as "untested" in README — practical validation with FUSE-T on macOS Sequoia required; FSKit (macOS 15+) may be better long-term path
- Phase 7 planning: winfsp-rs is GPL-3 — distribution license decision must precede any Windows implementation work

## Session Continuity

Last session: 2026-03-29T20:52:00.711Z
Stopped at: Completed 07-03-PLAN.md - CI pipeline and benchmark infrastructure
Resume file: None
