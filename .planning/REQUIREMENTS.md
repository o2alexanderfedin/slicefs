# Requirements: DedupFS

**Defined:** 2026-03-27
**Core Value:** High-ratio data deduplication that works transparently as a real, daily-driver POSIX filesystem

## v1 Requirements

Requirements for initial release. Each maps to roadmap phases.

### CAS & Deduplication

- [x] **CAS-01**: Block-level content-addressable storage with pluggable hash function trait
- [x] **CAS-02**: Pluggable chunking/block-splitting trait interface (concrete algorithms provided by owner's existing technology)
- [x] **CAS-03**: Pluggable storage backend trait for CAS blocks with local disk implementation
- [x] **CAS-04**: Reference counting per block with atomic increment/decrement
- [x] **CAS-05**: Integrity verification on read (re-hash block, compare to stored hash, configurable on/off)
- [x] **CAS-06**: Dedup-aware space reporting (logical size vs physical size via statfs)
- [x] **CAS-07**: On-disk dedup index with bounded memory usage (no full DDT in RAM)

### Garbage Collection

- [x] **GC-01**: Crash-safe garbage collection of zero-refcount blocks
- [x] **GC-02**: Two-phase mark-and-sweep or WAL-based refcount with deferred physical deletion
- [x] **GC-03**: Snapshot-aware GC (blocks reachable from any snapshot are live)

### POSIX Filesystem

- [x] **POSIX-01**: File read/write/create/delete operations
- [x] **POSIX-02**: Directory create/delete/list (readdir with . and .. entries)
- [x] **POSIX-03**: Atomic rename (rename(2)) for editors, package managers
- [x] **POSIX-04**: Symbolic links (symlink/readlink)
- [x] **POSIX-05**: Hard links (link(2)) with correct inode-level reference counting
- [x] **POSIX-06**: File permissions (chmod/chown, uid/gid)
- [x] **POSIX-07**: Timestamps (mtime, ctime; noatime by default)
- [x] **POSIX-08**: Extended attributes (xattr) for macOS Finder metadata, SELinux labels
- [x] **POSIX-09**: Truncate/ftruncate with correct partial block handling
- [x] **POSIX-10**: Stable inode numbers across mount cycles
- [x] **POSIX-11**: fsync/fdatasync correctness (guaranteed durability)
- [x] **POSIX-12**: POSIX locking (fcntl locks, flock)
- [x] **POSIX-13**: Correct errno values for all operations
- [x] **POSIX-14**: pjdfstest pass rate >95%
- [x] **POSIX-15**: All POSIX operations that FUSE frontend allows on each platform

### Metadata & Crash Safety

- [x] **META-01**: Atomic metadata commits (crash-safe root pointer update)
- [x] **META-02**: Clean mount/unmount with graceful SIGTERM handling and pending write flush
- [x] **META-03**: Metadata storage separated from block storage (independent stores)

### Snapshots & Versioning

- [x] **SNAP-01**: Read-only point-in-time snapshots (frozen metadata tree, shared CAS blocks)
- [x] **SNAP-02**: Filesystem version history with ability to switch between historical versions
- [x] **SNAP-03**: Efficient version switching at block level (leveraging CAS architecture)

### Compression

- [x] **COMP-01**: Compression of stored blocks (pluggable compressor, e.g., LZ4 for speed, Zstd for ratio)
- [x] **COMP-02**: Dedup-first-then-compress ordering (hash original content, store compressed)

### Cross-Platform

- [x] **PLAT-01**: macOS support via FUSE-T + fuser
- [x] **PLAT-02**: Linux support via libfuse + fuser
- [ ] **PLAT-03**: Windows support via WinFSP (note: GPL-3 license implications)
- [ ] **PLAT-04**: Platform-specific POSIX compliance testing on each target

### CLI & Operations

- [x] **CLI-01**: Mount command with configurable options (backing store path, mount options)
- [x] **CLI-02**: Unmount command with clean shutdown
- [x] **CLI-03**: Stats command (dedup ratio, logical/physical bytes, block count, reference distribution)
- [x] **CLI-04**: Scrub command (walk all blocks, re-verify hashes, report corruption)
- [x] **CLI-05**: Structured JSON output from all CLI commands for tooling integration
- [x] **CLI-06**: Mount options for performance tuning (noatime, writeback cache, cache size)

## v2 Requirements

Deferred to future milestone. Tracked but not in current roadmap.

### Security

- **SEC-01**: Encryption at rest (dedup-then-encrypt; AES-256-GCM per block)
- **SEC-02**: Key management (passphrase-derived via Argon2/scrypt)

### Distributed

- **DIST-01**: Remote/distributed storage backend via pluggable trait
- **DIST-02**: Cross-machine deduplication
- **DIST-03**: Decentralized topology support (P2P or federated)

### Advanced

- **ADV-01**: Snapshot writeable clones (branch-on-write)
- **ADV-02**: Quota management (per-directory or per-user)
- **ADV-03**: Online compaction/repack of block store

## Out of Scope

| Feature | Reason |
|---------|--------|
| Encrypt-before-dedup | Destroys dedup ratio — encrypted blocks of identical data produce different ciphertext |
| Full DDT in RAM (ZFS-style) | Memory explosion at scale (~320 bytes/block); use on-disk index with LRU cache |
| Online defragmentation | Meaningless for CAS — blocks stored by hash, physical adjacency irrelevant |
| Per-file dedup ratio tracking | Blocks are shared pool resources; per-file attribution is misleading and expensive |
| GUI management interface | CLI-first; structured JSON output enables third-party GUIs |
| Specific hash/chunking algorithm selection | Owner will provide existing technology; trait interfaces are in scope, concrete algorithms are not |

## Traceability

Which phases cover which requirements. Updated during roadmap creation.

| Requirement | Phase | Status |
|-------------|-------|--------|
| CAS-01 | Phase 1 | Complete |
| CAS-02 | Phase 1 | Complete |
| CAS-03 | Phase 1 | Complete |
| CAS-04 | Phase 4 | Complete |
| CAS-05 | Phase 1 | Complete |
| CAS-06 | Phase 4 | Complete |
| CAS-07 | Phase 1 | Complete |
| GC-01 | Phase 5 | Complete |
| GC-02 | Phase 5 | Complete |
| GC-03 | Phase 5 | Complete |
| POSIX-01 | Phase 4 | Complete |
| POSIX-02 | Phase 4 | Complete |
| POSIX-03 | Phase 4 | Complete |
| POSIX-04 | Phase 4 | Complete |
| POSIX-05 | Phase 4 | Complete |
| POSIX-06 | Phase 2 | Complete |
| POSIX-07 | Phase 2 | Complete |
| POSIX-08 | Phase 2 | Complete |
| POSIX-09 | Phase 4 | Complete |
| POSIX-10 | Phase 2 | Complete |
| POSIX-11 | Phase 5 | Complete |
| POSIX-12 | Phase 4 | Complete |
| POSIX-13 | Phase 3 | Complete |
| POSIX-14 | Phase 4 | Complete |
| POSIX-15 | Phase 3 | Complete |
| META-01 | Phase 5 | Complete |
| META-02 | Phase 3 | Complete |
| META-03 | Phase 2 | Complete |
| SNAP-01 | Phase 6 | Complete |
| SNAP-02 | Phase 6 | Complete |
| SNAP-03 | Phase 6 | Complete |
| COMP-01 | Phase 6 | Complete |
| COMP-02 | Phase 6 | Complete |
| PLAT-01 | Phase 7 | Complete |
| PLAT-02 | Phase 3 | Complete |
| PLAT-03 | Phase 7 | Pending |
| PLAT-04 | Phase 7 | Pending |
| CLI-01 | Phase 3 | Complete |
| CLI-02 | Phase 3 | Complete |
| CLI-03 | Phase 7 | Complete |
| CLI-04 | Phase 7 | Complete |
| CLI-05 | Phase 7 | Complete |
| CLI-06 | Phase 3 | Complete |

**Coverage:**
- v1 requirements: 43 total
- Mapped to phases: 43
- Unmapped: 0

---
*Requirements defined: 2026-03-27*
*Last updated: 2026-03-27 after roadmap creation — all 43 v1 requirements mapped*
