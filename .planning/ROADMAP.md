# Roadmap: SliceFS

## Milestones

- ✅ **v1.0 Foundation** - Phases 1-7 (shipped 2026-03-29)
- 🚧 **v2.0 Streaming Writes & Hardening** - Phases 8-11 (in progress)

## Phases

<details>
<summary>✅ v1.0 Foundation (Phases 1-7) - SHIPPED 2026-03-29</summary>

### Phase 1: CAS Foundation
**Goal**: The immutable block store, pluggable hash interface, pluggable chunking interface, and dedup index exist and can be exercised in unit tests — every subsequent component has a foundation to build on
**Depends on**: Nothing (first phase)
**Requirements**: CAS-01, CAS-02, CAS-03, CAS-05, CAS-07
**Success Criteria** (what must be TRUE):
  1. A block can be written to the local disk store keyed by its hash and retrieved by that hash
  2. An alternative hash function can be swapped in by implementing the ContentHasher trait without changing any other code
  3. An alternative chunking strategy can be swapped in by implementing the Chunker trait without changing any other code
  4. A duplicate block write is detected via the bloom filter + on-disk index before any disk write occurs
  5. A retrieved block fails verification if its stored bytes have been corrupted (integrity check on read)
**Plans:** 3/3 plans complete

Plans:
- [x] 01-01-PLAN.md — Cargo workspace setup and CAS trait definitions in slicefs-traits
- [x] 01-02-PLAN.md — Blake3Hasher, FixedChunker, and MemBlockStore stub implementations
- [x] 01-03-PLAN.md — LocalDiskStore and MemDedupIndex with bloom filter

### Phase 2: Metadata Engine
**Goal**: The inode table, directory tree, file manifests, and xattr store exist as an ACID-backed metadata layer completely separated from the block store — FUSE can be wired on top of it
**Depends on**: Phase 1
**Requirements**: META-03, POSIX-06, POSIX-07, POSIX-08, POSIX-10
**Success Criteria** (what must be TRUE):
  1. An inode can be created, read, updated, and deleted via the MetadataStore interface
  2. A directory entry can be created, listed, and removed; . and .. are always present
  3. A file manifest linking inode to ordered list of block hashes can be created and retrieved
  4. Extended attributes can be stored and retrieved on an inode
  5. Inode numbers are stable — the same inode number is assigned to the same file across process restarts
**Plans:** 3/3 plans complete

Plans:
- [x] 02-01-PLAN.md — data-id submodule, slicefs-traits redesign, InodeMeta serialization
- [x] 02-02-PLAN.md — DictMetadataStore with inode CRUD, directory ops, and manifest storage
- [x] 02-03-PLAN.md — Xattr storage and persistence round-trip with inode stability proof

### Phase 3: Read-Only FUSE
**Goal**: The filesystem can be mounted and browsed read-only by the OS; a human can ls, cat, and stat files through the mount point using pre-populated content — the kernel interface is validated before write complexity is introduced
**Depends on**: Phase 2
**Requirements**: POSIX-13, POSIX-15, PLAT-02, CLI-01, CLI-02, CLI-06, META-02
**Success Criteria** (what must be TRUE):
  1. Running the mount command produces a mountable filesystem at the specified path on Linux
  2. Files and directories pre-seeded into the backing store appear correctly under ls, stat, and cat through the mount point
  3. The filesystem unmounts cleanly via the unmount command with no kernel errors; SIGTERM triggers a graceful flush
  4. All POSIX operations return correct errno values for read-only violations (e.g., EROFS on write attempt)
  5. Mount options (noatime, cache size) are accepted and applied
**Plans:** 3/3 plans complete

Plans:
- [x] 03-01-PLAN.md — CLI crate scaffold, StoreIo, clap subcommands, and SliceFsFilesystem FUSE adapter
- [x] 03-02-PLAN.md — Seed command: import directory tree into CAS store via State CDC
- [x] 03-03-PLAN.md — Mount and unmount commands with FUSE lifecycle and end-to-end verification

### Phase 4: Full POSIX Write Path
**Goal**: Files can be created, written, modified, renamed, deleted, and linked through the mount point with inline deduplication active; real tools (editors, package managers, build systems) work correctly; pjdfstest passes >95% on Linux
**Depends on**: Phase 3
**Requirements**: POSIX-01, POSIX-02, POSIX-03, POSIX-04, POSIX-05, POSIX-09, POSIX-12, POSIX-14, CAS-04, CAS-06
**Success Criteria** (what must be TRUE):
  1. A file written through the mount point is stored as deduplicated blocks; writing the same file twice results in one physical copy
  2. Atomic rename works correctly — editors (vim, emacs) can save files without corruption
  3. Hard links share inode reference counts; unlinking one hard link does not remove the file until all links are removed
  4. statfs reports both logical and physical byte counts, showing the dedup ratio
  5. pjdfstest passes >95% of applicable tests on Linux
**Plans:** 4/4 plans complete

Plans:
- [x] 04-01-PLAN.md — Refcount infrastructure, write state types, RW mount, destroy persistence
- [x] 04-02-PLAN.md — Core file write path: create, write, release with CAS flush, setattr/truncate
- [x] 04-03-PLAN.md — Directory ops, rename, symlinks, hard links, unlink with nlinks lifecycle
- [x] 04-04-PLAN.md — Dedup-aware statfs, POSIX locking stubs, custom POSIX compliance test suite

### Phase 5: Crash Safety and GC
**Goal**: The filesystem survives crashes and power loss without data loss or block leaks; garbage collection reclaims orphaned blocks safely without racing against active writes or snapshots
**Depends on**: Phase 4
**Requirements**: META-01, GC-01, GC-02, GC-03, POSIX-11
**Success Criteria** (what must be TRUE):
  1. A kill -9 during an active write followed by remount produces a consistent filesystem — no dangling block references and no data corruption
  2. fsync and fdatasync guarantee durability — data written before the call is on disk before the call returns, verified after crash
  3. Orphaned blocks from an interrupted write are reclaimed by GC and do not grow the store unboundedly
  4. A block referenced by any snapshot is never deleted by GC, even when its refcount reaches zero in the live tree
  5. WAL replay on dirty mount restores the last committed state without manual intervention
**Plans:** 4/4 plans complete

Plans:
- [x] 05-01-PLAN.md — Segment file I/O layer and WAL strategy trait with per-op and no-op implementations
- [x] 05-02-PLAN.md — DictMetadataStore segment integration, dirty mount detection, fsync callback, --wal-strategy CLI
- [x] 05-03-PLAN.md — Mark-and-sweep GC engine with segment compaction and background thread
- [x] 05-04-PLAN.md — Offline GC CLI command, mount with background GC, crash recovery integration tests

### Phase 6: Compression and Snapshots
**Goal**: Stored blocks are compressed to reduce physical footprint; point-in-time snapshots can be created and the filesystem state can be switched between historical versions — both capabilities are natural expressions of the CAS architecture already in place
**Depends on**: Phase 5
**Requirements**: COMP-01, COMP-02, SNAP-01, SNAP-02, SNAP-03
**Success Criteria** (what must be TRUE):
  1. Blocks written to the store are compressed before storage; the physical size on disk is smaller than the logical size for compressible data
  2. An alternative compressor (e.g., LZ4 vs. Zstd) can be swapped in via a pluggable compressor trait without changing the store interface
  3. A snapshot command creates a read-only point-in-time view; files in the snapshot are readable and match their state at snapshot time
  4. Switching to a historical version makes the live filesystem reflect that version's file contents
  5. Two snapshots sharing blocks do not double-count physical storage; shared blocks appear once in physical usage
**Plans:** 4/4 plans complete

Plans:
- [x] 06-01-PLAN.md — Compressor trait in slicefs-traits + Zstd/LZ4/None implementations in slicefs-compression crate
- [x] 06-02-PLAN.md — Compression wired into FUSE write/read path + CLI flags (--compressor, --compressor-level)
- [x] 06-03-PLAN.md — SnapshotRecord segment entry + DictMetadataStore snapshot methods (create/list/find/roots)
- [x] 06-04-PLAN.md — Snapshot CLI commands (create/list/switch) + snapshot mount flags + snapshot-aware GC

### Phase 7: Cross-Platform and Production Hardening
**Goal**: The filesystem runs on macOS via FUSE-T with full POSIX compliance; the CLI is complete with stats, scrub, and structured JSON output; benchmark baselines confirm daily-driver performance; Windows support is deferred to v2
**Depends on**: Phase 6
**Requirements**: PLAT-01, PLAT-03, PLAT-04, CLI-03, CLI-04, CLI-05
**Success Criteria** (what must be TRUE):
  1. The filesystem mounts and passes the macOS-specific POSIX test suite (mmap writes, flock, large directory) via FUSE-T
  2. The filesystem mounts and passes basic POSIX conformance on Windows via WinFSP
  3. The stats command outputs dedup ratio, logical bytes, physical bytes, and block count in both human-readable and JSON formats
  4. The scrub command walks all stored blocks, re-verifies their hashes, and reports any corrupted blocks without modifying the store
  5. Sequential write throughput on NVMe meets or exceeds 200 MB/s as measured by fio with 4K writes; dedup index memory stays below the configured cap under 100 GB unique data load
**Plans:** 3/3 plans complete

Plans:
- [x] 07-01-PLAN.md — FUSE-T write path fix + macOS build ergonomics (direct_io, .cargo/config.toml, build.rs)
- [x] 07-02-PLAN.md — Global --json flag + stats and scrub CLI commands with human and JSON output
- [x] 07-03-PLAN.md — GitHub Actions CI (Linux + macOS), benchmark infrastructure, Windows deferral documentation

</details>

### v2.0 — Streaming Writes & Hardening

**Milestone Goal:** Remove the file size = RAM limitation via incremental Merkle streaming writes; remove write-path compression so dedup hashes raw content; fix known correctness bugs from v1.0 operation.

### Phase 07.1: FileStorage Migration (INSERTED)

**Goal**: Switch DictMetadataStore from in-memory Dictionary (BTreeMap) to file-backed FileStorageAdd/file_storage_get from data-id — eliminates ~67 GB RAM for 1 TB stores by using the host filesystem as a hash table; structurally solves FIX-03/FIX-04 (snapshot O(1) lookup)
**Depends on:** Phase 7
**Requirements**: FIX-03, FIX-04 (solved structurally)
**Success Criteria** (what must be TRUE):
  1. DictMetadataStore uses file-backed storage (FileStorageAdd/file_storage_get via Io trait) instead of in-memory BTreeMap<Digest224, Branches>
  2. Memory usage for the Merkle tree is O(1) regardless of store size — hot nodes served by OS page cache, not application heap
  3. Snapshot lookup by version or name is O(1) via filesystem path resolution
  4. All existing tests pass with no regression — seed, mount, read, write, GC, scrub, stats, snapshots all work identically
  5. Segment replay on mount populates file-backed storage instead of in-memory Dictionary
**Plans**: 3 plans

Plans:
- [x] 07.1-01-PLAN.md — Export blockset FileStorage API, generalize intern/load helpers, O(1) snapshot indexes (FIX-03/FIX-04)
- [x] 07.1-02-PLAN.md — Replace Dictionary with StoreIo in DictMetadataStore, remove DictEntry from WAL/segments
- [x] 07.1-03-PLAN.md — Update all CLI consumers (filesystem, mount, seed, GC, scrub, stats) to use file-backed storage

#### Phase 8: Correctness Fixes
**Goal**: Known v1.0 correctness bugs are eliminated before the invasive write-path restructuring begins — refcount overflow risk is closed and statfs reports real numbers with three-tier space reporting (logical, CAS, host disk)
**Depends on**: Phase 7.1
**Requirements**: FIX-01, FIX-02, FIX-03, FIX-04
**Success Criteria** (what must be TRUE):
  1. Incrementing a block's refcount at u64::MAX produces u64::MAX (saturating), not 0 — no silent data loss under extreme dedup load
  2. `df` on a mounted SliceFS volume reports the actual number of inodes in use, not a hardcoded 1,000,000
  3. Physical bytes reported by statfs reflect the real store occupancy, not an arithmetic approximation based on dict entry count
  4. Looking up a snapshot by version number or name executes in O(1) time regardless of how many snapshots exist
**Plans**: 1 plan

Plans:
- [ ] 08-01-PLAN.md — Saturating refcount fix, inode_count AtomicU64, three-tier statfs, scrub saturated reporting, FIX-03/FIX-04 verification

#### Phase 9: Compression Removal
**Goal**: The write path pushes raw bytes directly into the Merkle tree with no compression header; compression infrastructure is fully removed (clean break — no v1/v2 backward compatibility; no production stores exist); raw content dedup works naturally
**Depends on**: Phase 8
**Requirements**: DECOMP-01, DECOMP-02, DECOMP-03, DECOMP-04
**Success Criteria** (what must be TRUE):
  1. Writing a file to a mounted store produces no compression header bytes — raw content is stored in the Merkle tree verbatim
  2. Two files with identical raw content deduplicate correctly via same Digest224 on raw bytes
  3. Reading a stored file returns raw bytes unchanged — no decompress call in the read path
  4. Stats reports compressor as "none (v3 raw)"; CLI has no --compressor flags
**Plans**: 2 plans

Plans:
- [ ] 09-01-PLAN.md — Remove compression from production code (filesystem.rs, mount.rs, cli.rs, stats.rs, Cargo.toml)
- [ ] 09-02-PLAN.md — Update test files, delete compression_tests.rs, create v3 store validation tests

#### Phase 10: Streaming Writes Core
**Goal**: Sequential file writes use the incremental push_bytes API so that open file handle memory is O(log N) in file size — arbitrarily large files can be written without hitting a RAM ceiling; fsync mid-stream and truncate on an open handle both work correctly
**Depends on**: Phase 9
**Requirements**: STRM-01, STRM-03, STRM-04, STRM-05
**Success Criteria** (what must be TRUE):
  1. Writing a 10 GB file sequentially through the mount point completes successfully on a machine with 512 MB of RAM available to the FUSE process — no OOM kill
  2. Reading a file while it is still open for writing (before release) returns the bytes written so far, consistent with what a second process would see after the write completes
  3. Calling fsync mid-stream commits all bytes written up to that point durably; subsequent writes to the same file handle continue correctly and produce a consistent final file
  4. Truncating an open file handle to a smaller size atomically resets the streaming state and adjusts inode size — the file is correct after release with no leftover bytes beyond the truncation point
**Plans**: 3 plans

Plans:
- [ ] 10-01-PLAN.md — OpenFileState: replace Vec<u8> buf with blockset::State + byte_count tracking
- [ ] 10-02-PLAN.md — flush_buffer_to_cas rewritten to use state.end(); fsync mid-stream; read-after-write materialisation
- [ ] 10-03-PLAN.md — Truncate with in-progress streaming State; cas_committed guard update; integration tests

#### Phase 11: Non-Sequential Write Handling
**Goal**: pwrite at arbitrary offsets, memory-mapped writes, and writeback_cache out-of-order delivery all produce correct results — the streaming path handles sequential writes and falls back to the v1.0 buffer model for non-sequential writes with no regression
**Depends on**: Phase 10
**Requirements**: STRM-02
**Success Criteria** (what must be TRUE):
  1. pwrite(2) at a non-sequential offset on an open file handle produces a correct file after release — no corruption, no silent data loss
  2. Enabling writeback_cache on a mount with in-flight writes produces correct files — out-of-order FUSE write callbacks do not corrupt the Merkle root
  3. A file written via a tool that uses non-sequential access patterns (vim, sqlite, cp --sparse) is byte-identical to the source after release
**Plans**: TBD

Plans:
- [ ] 11-01-PLAN.md — next_expected_offset tracking + WriteMode enum; fallback to Vec<u8> on non-sequential offset
- [ ] 11-02-PLAN.md — writeback_cache integration test; pwrite and sparse-file correctness tests

## Progress

**Execution Order:**
Phases execute in numeric order: 7.1 -> 8 -> 9 -> 10 -> 11

| Phase | Milestone | Plans Complete | Status | Completed |
|-------|-----------|----------------|--------|-----------|
| 1. CAS Foundation | v1.0 | 3/3 | Complete | 2026-03-28 |
| 2. Metadata Engine | v1.0 | 3/3 | Complete | 2026-03-28 |
| 3. Read-Only FUSE | v1.0 | 3/3 | Complete | 2026-03-28 |
| 4. Full POSIX Write Path | v1.0 | 4/4 | Complete | 2026-03-28 |
| 5. Crash Safety and GC | v1.0 | 4/4 | Complete | 2026-03-29 |
| 6. Compression and Snapshots | v1.0 | 4/4 | Complete | 2026-03-29 |
| 7. Cross-Platform and Production Hardening | v1.0 | 3/3 | Complete | 2026-03-29 |
| 7.1. FileStorage Migration (INSERTED) | 3/3 | Complete |  | - |
| 8. Correctness Fixes | 1/1 | Complete   | 2026-03-30 | - |
| 9. Compression Removal | 1/2 | In Progress|  | - |
| 10. Streaming Writes Core | v2.0 | 0/3 | Not started | - |
| 11. Non-Sequential Write Handling | v2.0 | 0/2 | Not started | - |
