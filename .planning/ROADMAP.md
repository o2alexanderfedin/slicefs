# Roadmap: DedupFS

## Overview

DedupFS is built bottom-up along its dependency graph. The CAS block store and chunking traits are the foundation — every other component depends on them and nothing else. The metadata engine is built in parallel isolation, then the FUSE layer is mounted read-only to validate all three pillars under real kernel interaction before any write complexity is introduced. Full POSIX write semantics with inline deduplication completes the functional core. Crash safety, reference counting, and garbage collection are co-developed as a single correctness layer because GC correctness depends entirely on refcount invariants holding. Compression and snapshots arrive next — both are natural CAS capabilities unlocked by the foundation, not bolt-ons. The roadmap closes with cross-platform expansion to macOS and Windows and production hardening that earns the "daily-driver" label.

## Phases

**Phase Numbering:**
- Integer phases (1, 2, 3): Planned milestone work
- Decimal phases (2.1, 2.2): Urgent insertions (marked with INSERTED)

Decimal phases appear between their surrounding integers in numeric order.

- [x] **Phase 1: CAS Foundation** - Block store, hash, chunking traits, and dedup index with bloom filter (completed 2026-03-28)
- [ ] **Phase 2: Metadata Engine** - Inode table, directory structure, file manifests, xattrs — separate from block store
- [ ] **Phase 3: Read-Only FUSE** - Mount a real filesystem read-only; validate the kernel interface before write complexity
- [ ] **Phase 4: Full POSIX Write Path** - Complete read/write POSIX with inline deduplication; pjdfstest >95% on Linux
- [ ] **Phase 5: Crash Safety and GC** - WAL, refcount correctness, garbage collection, and crash recovery — the correctness layer
- [ ] **Phase 6: Compression and Snapshots** - Block compression and point-in-time versioning enabled by the CAS architecture
- [ ] **Phase 7: Cross-Platform and Production Hardening** - macOS, Windows, CLI completeness, scrub, and production benchmarks

## Phase Details

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
**Plans:** 2/3 plans executed
Plans:
- [ ] 02-01-PLAN.md — data-id submodule, slicefs-traits redesign, InodeMeta serialization
- [ ] 02-02-PLAN.md — DictMetadataStore with inode CRUD, directory ops, and manifest storage
- [ ] 02-03-PLAN.md — Xattr storage and persistence round-trip with inode stability proof

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
**Plans:** 1/3 plans executed
Plans:
- [ ] 03-01-PLAN.md — CLI crate scaffold, StoreIo, clap subcommands, and SliceFsFilesystem FUSE adapter
- [ ] 03-02-PLAN.md — Seed command: import directory tree into CAS store via State CDC
- [ ] 03-03-PLAN.md — Mount and unmount commands with FUSE lifecycle and end-to-end verification

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
**Plans**: TBD

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
**Plans**: TBD

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
**Plans**: TBD

### Phase 7: Cross-Platform and Production Hardening
**Goal**: The filesystem runs on macOS and Windows in addition to Linux; the CLI is complete with stats, scrub, and structured output; benchmark baselines confirm daily-driver performance; the system is validated as production-ready
**Depends on**: Phase 6
**Requirements**: PLAT-01, PLAT-03, PLAT-04, CLI-03, CLI-04, CLI-05
**Success Criteria** (what must be TRUE):
  1. The filesystem mounts and passes the macOS-specific POSIX test suite (mmap writes, flock, large directory) via FUSE-T
  2. The filesystem mounts and passes basic POSIX conformance on Windows via WinFSP
  3. The stats command outputs dedup ratio, logical bytes, physical bytes, and block count in both human-readable and JSON formats
  4. The scrub command walks all stored blocks, re-verifies their hashes, and reports any corrupted blocks without modifying the store
  5. Sequential write throughput on NVMe meets or exceeds 200 MB/s as measured by fio with 4K writes; dedup index memory stays below the configured cap under 100 GB unique data load
**Plans**: TBD

## Progress

**Execution Order:**
Phases execute in numeric order: 1 -> 2 -> 3 -> 4 -> 5 -> 6 -> 7

| Phase | Plans Complete | Status | Completed |
|-------|----------------|--------|-----------|
| 1. CAS Foundation | 3/3 | Complete   | 2026-03-28 |
| 2. Metadata Engine | 2/3 | In Progress|  |
| 3. Read-Only FUSE | 1/3 | In Progress|  |
| 4. Full POSIX Write Path | 0/TBD | Not started | - |
| 5. Crash Safety and GC | 0/TBD | Not started | - |
| 6. Compression and Snapshots | 0/TBD | Not started | - |
| 7. Cross-Platform and Production Hardening | 0/TBD | Not started | - |
