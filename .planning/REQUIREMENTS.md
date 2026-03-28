# Requirements: DedupFS

**Defined:** 2026-03-27
**Core Value:** High-ratio data deduplication that works transparently as a real, daily-driver POSIX filesystem

## v1 Requirements

Requirements for initial release. Each maps to roadmap phases.

### CAS & Deduplication

- [x] **CAS-01**: Block-level content-addressable storage with pluggable hash function trait
- [x] **CAS-02**: Pluggable chunking/block-splitting trait interface (concrete algorithms provided by owner's existing technology)
- [x] **CAS-03**: Pluggable storage backend trait for CAS blocks with local disk implementation
- [ ] **CAS-04**: Reference counting per block with atomic increment/decrement
- [x] **CAS-05**: Integrity verification on read (re-hash block, compare to stored hash, configurable on/off)
- [ ] **CAS-06**: Dedup-aware space reporting (logical size vs physical size via statfs)
- [x] **CAS-07**: On-disk dedup index with bounded memory usage (no full DDT in RAM)

### Garbage Collection

- [ ] **GC-01**: Crash-safe garbage collection of zero-refcount blocks
- [ ] **GC-02**: Two-phase mark-and-sweep or WAL-based refcount with deferred physical deletion
- [ ] **GC-03**: Snapshot-aware GC (blocks reachable from any snapshot are live)

### POSIX Filesystem

- [ ] **POSIX-01**: File read/write/create/delete operations
- [ ] **POSIX-02**: Directory create/delete/list (readdir with . and .. entries)
- [ ] **POSIX-03**: Atomic rename (rename(2)) for editors, package managers
- [ ] **POSIX-04**: Symbolic links (symlink/readlink)
- [ ] **POSIX-05**: Hard links (link(2)) with correct inode-level reference counting
- [ ] **POSIX-06**: File permissions (chmod/chown, uid/gid)
- [ ] **POSIX-07**: Timestamps (mtime, ctime; noatime by default)
- [ ] **POSIX-08**: Extended attributes (xattr) for macOS Finder metadata, SELinux labels
- [ ] **POSIX-09**: Truncate/ftruncate with correct partial block handling
- [ ] **POSIX-10**: Stable inode numbers across mount cycles
- [ ] **POSIX-11**: fsync/fdatasync correctness (guaranteed durability)
- [ ] **POSIX-12**: POSIX locking (fcntl locks, flock)
- [ ] **POSIX-13**: Correct errno values for all operations
- [ ] **POSIX-14**: pjdfstest pass rate >95%
- [ ] **POSIX-15**: All POSIX operations that FUSE frontend allows on each platform

### Metadata & Crash Safety

- [ ] **META-01**: Atomic metadata commits (crash-safe root pointer update)
- [ ] **META-02**: Clean mount/unmount with graceful SIGTERM handling and pending write flush
- [ ] **META-03**: Metadata storage separated from block storage (independent stores)

### Snapshots & Versioning

- [ ] **SNAP-01**: Read-only point-in-time snapshots (frozen metadata tree, shared CAS blocks)
- [ ] **SNAP-02**: Filesystem version history with ability to switch between historical versions
- [ ] **SNAP-03**: Efficient version switching at block level (leveraging CAS architecture)

### Compression

- [ ] **COMP-01**: Compression of stored blocks (pluggable compressor, e.g., LZ4 for speed, Zstd for ratio)
- [ ] **COMP-02**: Dedup-first-then-compress ordering (hash original content, store compressed)

### Cross-Platform

- [ ] **PLAT-01**: macOS support via FUSE-T + fuser
- [ ] **PLAT-02**: Linux support via libfuse + fuser
- [ ] **PLAT-03**: Windows support via WinFSP (note: GPL-3 license implications)
- [ ] **PLAT-04**: Platform-specific POSIX compliance testing on each target

### CLI & Operations

- [ ] **CLI-01**: Mount command with configurable options (backing store path, mount options)
- [ ] **CLI-02**: Unmount command with clean shutdown
- [ ] **CLI-03**: Stats command (dedup ratio, logical/physical bytes, block count, reference distribution)
- [ ] **CLI-04**: Scrub command (walk all blocks, re-verify hashes, report corruption)
- [ ] **CLI-05**: Structured JSON output from all CLI commands for tooling integration
- [ ] **CLI-06**: Mount options for performance tuning (noatime, writeback cache, cache size)

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
| CAS-04 | Phase 4 | Pending |
| CAS-05 | Phase 1 | Complete |
| CAS-06 | Phase 4 | Pending |
| CAS-07 | Phase 1 | Complete |
| GC-01 | Phase 5 | Pending |
| GC-02 | Phase 5 | Pending |
| GC-03 | Phase 5 | Pending |
| POSIX-01 | Phase 4 | Pending |
| POSIX-02 | Phase 4 | Pending |
| POSIX-03 | Phase 4 | Pending |
| POSIX-04 | Phase 4 | Pending |
| POSIX-05 | Phase 4 | Pending |
| POSIX-06 | Phase 2 | Pending |
| POSIX-07 | Phase 2 | Pending |
| POSIX-08 | Phase 2 | Pending |
| POSIX-09 | Phase 4 | Pending |
| POSIX-10 | Phase 2 | Pending |
| POSIX-11 | Phase 5 | Pending |
| POSIX-12 | Phase 4 | Pending |
| POSIX-13 | Phase 3 | Pending |
| POSIX-14 | Phase 4 | Pending |
| POSIX-15 | Phase 3 | Pending |
| META-01 | Phase 5 | Pending |
| META-02 | Phase 3 | Pending |
| META-03 | Phase 2 | Pending |
| SNAP-01 | Phase 6 | Pending |
| SNAP-02 | Phase 6 | Pending |
| SNAP-03 | Phase 6 | Pending |
| COMP-01 | Phase 6 | Pending |
| COMP-02 | Phase 6 | Pending |
| PLAT-01 | Phase 7 | Pending |
| PLAT-02 | Phase 3 | Pending |
| PLAT-03 | Phase 7 | Pending |
| PLAT-04 | Phase 7 | Pending |
| CLI-01 | Phase 3 | Pending |
| CLI-02 | Phase 3 | Pending |
| CLI-03 | Phase 7 | Pending |
| CLI-04 | Phase 7 | Pending |
| CLI-05 | Phase 7 | Pending |
| CLI-06 | Phase 3 | Pending |

**Coverage:**
- v1 requirements: 43 total
- Mapped to phases: 43
- Unmapped: 0

---
*Requirements defined: 2026-03-27*
*Last updated: 2026-03-27 after roadmap creation — all 43 v1 requirements mapped*
