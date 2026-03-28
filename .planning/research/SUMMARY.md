# Project Research Summary

**Project:** DedupFS — Deduplicating FUSE Filesystem in Rust
**Domain:** CAS-backed, cross-platform, POSIX-compatible deduplicating filesystem
**Researched:** 2026-03-27
**Confidence:** MEDIUM-HIGH

## Executive Summary

DedupFS is a daily-driver POSIX filesystem that transparently deduplicates file content at the block level using content-addressable storage (CAS). Experts build systems like this in layers: a thin FUSE translation layer sits atop a metadata engine (inodes, directories, manifests) and a separate CAS block store, connected via a write path that chunks incoming data, hashes each chunk, deduplicates against an index, and persists only novel blocks. The reference implementations — ZFS, Btrfs, Borg, rdedup — all converge on this layered design, and the Rust ecosystem has mature, purpose-built crates (`fuser` 0.17, `redb` 3.1, `blake3`, `fastcdc`) for each layer.

The recommended approach is to build bottom-up: CAS block store first, then chunking integration (plugging in the owner's CDC algorithm via a `Chunker` trait), then metadata engine, then the FUSE layer on top. Inline deduplication (dedup happens before blocks reach persistent storage) is the correct default for a daily-driver filesystem — it avoids the "storage balloon" problem of post-process dedup. The dedup index must be backed by an on-disk B-tree with a bloom filter pre-filter from the start; keeping the index in memory only is the primary cause of the ZFS DDT memory explosion problem.

The key risks fall into two categories. The first is correctness: reference count corruption under concurrent deletion and GC races can cause silent data loss, and crash inconsistency (writing blocks before committing metadata references) is the most common cause of dangling pointers. Both require a WAL and an epoch-aware two-phase GC from day one — these cannot be retrofitted. The second category is performance: FUSE's context-switch overhead degrades small writes to ~20% of native speed unless `writeback_cache` is enabled, and inline dedup must run in `spawn_blocking` to avoid starving the Tokio executor. Addressing these in the correct phase prevents rearchitecting later.

---

## Key Findings

### Recommended Stack

The Rust ecosystem provides a clean, pure-Rust stack for all critical layers. `fuser` 0.17 is the only actively maintained FUSE implementation for Rust (2,100+ dependents, Feb 2026 release) and runs on Linux natively and macOS via FUSE-T. `redb` 3.1 is the correct metadata store: pure Rust, ACID, MVCC, stable format — it replaces both `sled` (pre-1.0, format-unstable, abandoned in practice) and SQLite (C dependency, write-serialized WAL). `blake3` is the default hash (80M downloads, SIMD-accelerated, designed for CAS). `fastcdc` 3.2 provides the default chunking algorithm while the owner's proprietary CDC implementation plugs in via a `Chunker` trait.

The workspace should be structured as four crates: `dedupfs-core` (CAS engine, traits), `dedupfs-meta` (redb-backed inode table), `dedupfs-fuse` (fuser integration), and `dedupfs-cli` (mount/umount/stats). Windows support via `winfsp` 0.12 is a separate, later-phase concern with a GPL-3 license implication that must be resolved before distribution.

**Core technologies:**
- `fuser` 0.17: FUSE filesystem interface — only actively maintained pure-Rust FUSE implementation
- `redb` 3.1: Metadata store — pure Rust, ACID, MVCC, stable format; replaces sled (avoid) and SQLite (avoid)
- `blake3` 1.8: Primary hash function — fastest cryptographic hash, SIMD-accelerated, designed for CAS
- `fastcdc` 3.2: Default chunking — FastCDC v2020 reference implementation; async-capable; default until owner's algorithm is wired in
- `tokio` 1.x: Async runtime — required for spawn_blocking, background GC, future distributed work
- `serde` + `bincode` 2.x: Metadata serialization — compact binary records for B-tree storage

**What NOT to use:** `sled` (pre-1.0, format changes), `fuse-rs`/zargony (archived), `macfuse` kext (broken on Apple Silicon), `bincode` 1.x (soundness issues), in-memory-only chunk index.

### Expected Features

The feature set is well-bounded. Full POSIX compliance is load-bearing: without atomic rename, stable inodes, hard links, xattr, correct fsync, and correct statfs, real tools (editors, package managers, build systems) will break immediately. The dedup value proposition requires CAS with pluggable hash and chunking, reference counting with crash-safe GC, and dedup-aware space reporting — these are co-equal to the POSIX features, not optional.

**Must have (table stakes — v1):**
- Full POSIX: read/write/create/delete/stat/rename/symlink/hardlinks/xattr — real tools break without these
- Stable inode numbers across mount cycles — build systems and editors depend on them
- fsync/fdatasync correctness — databases and editors; wrong behavior silently corrupts data
- Block-level CAS deduplication with pluggable hash (default: BLAKE3) — core value proposition
- Pluggable chunking interface (owner's CDC + fastcdc fallback) — required by architecture
- Reference counting with crash-safe GC — without this, blocks leak
- Atomic metadata commits — required for fsync correctness and crash safety
- Correct statfs (physical vs. logical) — users need to see how full the disk is
- CLI: mount, unmount, stats (dedup ratio, logical/physical bytes, block count)
- Integrity verification on read (configurable) — natural with CAS; builds user trust
- pjdfstest pass rate target: >95%

**Should have (differentiators — v1.x):**
- Content-defined chunking (CDC) if not in owner's algorithm already
- Block compression (LZ4/Zstd) — stacks with dedup; apply after dedup, before storage
- noatime mount option — performance; most modern Linux deployments default to this
- Integrity scrub CLI command — proactive corruption detection
- Read-only snapshots — COW metadata required first; enables safe backups

**Defer (v2+):**
- Encryption at rest — dedup-then-encrypt architecture; key management complexity
- Distributed/remote storage backend — requires clean BlockStore trait established in v1
- Snapshot writeable clones, quota management, online compaction

**Anti-features to explicitly reject:** encrypt-before-dedup (destroys dedup ratio), in-memory-only DDT (ZFS problem), per-file dedup ratio attribution (misleading with shared blocks), distributed filesystem in v1.

### Architecture Approach

The architecture is a strict layered system with five independent concerns: (1) FUSE translation layer, (2) VFS adapter mapping kernel inode numbers to internal IDs, (3) metadata engine owning inodes/directories/manifests, (4) write path engine orchestrating chunk/hash/dedup/store, and (5) CAS block store as an immutable, purely content-addressed blob store. These must be kept completely separate — mixing metadata and block storage is the primary cause of entangled lifetime management and performance issues. The chunk index (hash → block address) is a sixth, separate structure with a bloom filter pre-filter in front of an on-disk redb tree.

The build order is dictated by dependencies: CAS foundation first, then chunking integration, then metadata engine, then read-only FUSE, then full read/write POSIX, then reference counting + WAL, then GC, then read path optimization, then production hardening.

**Major components:**
1. **FuseHandler** — thin translation of kernel FUSE opcodes to internal calls; must contain zero business logic
2. **VFS Adapter** — inode number mapping, open file handle table, POSIX semantics enforcement (unlink-while-open, nlookup lifecycle)
3. **Metadata Engine** — inodes, directory entries, file manifests, xattrs, link counts; backed by redb; completely separate from block store
4. **Write Path Engine** — chunks incoming data, hashes chunks, deduplicates via chunk index, persists novel blocks; dispatches via `Chunker` and `ContentHasher` traits
5. **CAS Block Store** — immutable blob store keyed by hash; `trait BlockStore { put, get, delete, exists }`; local disk first, swappable backend
6. **Chunk Index** — bloom filter + on-disk redb tree; hot path for dedup lookups
7. **RefCount + WAL** — transactional refcount mutations logged before commit; crash recovery via WAL replay
8. **GC Engine** — background mark-and-sweep; mark phase walks all manifests; sweep phase deletes zero-refcount blocks; never on the hot path

### Critical Pitfalls

1. **Reference count corruption under concurrent deletion** — Use a WAL that records reference intentions before they are counted as live. Never decrement a refcount and free a block without a lock preventing concurrent writers from adding references to the same block. Model refcount state as an explicit state machine, not a bare AtomicU64. Prevention phase: core CAS + metadata (foundational).

2. **GC race — new write lost during mark phase** — Maintain a "pending references" protected set: any block referenced by an in-progress write is shielded from the sweep. A readers-writer lock on the GC pass (write lock for sweep, read lock held by active write transactions) is correct for single-node. Prevention phase: GC implementation; ship with formal correctness argument or exhaustive concurrency test.

3. **Crash inconsistency (orphaned blocks and dangling references)** — Write blocks to CAS store first, flush, then commit the metadata reference. This produces orphans (recoverable via GC) rather than dangling references (unrecoverable). Journal the metadata reference with write-intent semantics. Prevention phase: storage + metadata layer, co-developed; write the recovery path before shipping the write path.

4. **FUSE context-switch overhead destroying small-write performance** — Enable `writeback_cache` in fuser mount options (batches small writes into 128KB requests). Run hash computation in `spawn_blocking`, not in the FUSE callback thread. Establish `fio` benchmarks (4K sequential writes >200MB/s on NVMe) as phase acceptance criteria. Prevention phase: FUSE integration and write path.

5. **Dedup index memory explosion (the ZFS DDT lesson)** — Implement a bloom filter pre-filter before the main index lookup from day one. Design the index as a pluggable trait; the in-memory HashMap is fine for development but the production implementation requires a disk-backed B-tree with a configurable memory cap. Prevention phase: core dedup engine design (first phase); memory budget is a first-class design constraint.

6. **Inline dedup blocking the write hot path** — Hash computation and index lookup must run in `tokio::task::spawn_blocking`. Never hold a `std::sync::Mutex` across an `.await` in the dedup pipeline. Prevention phase: write path architecture, before implementing the write path.

---

## Implications for Roadmap

Based on the combined research, the build order is strictly dictated by component dependencies. Lower layers must exist and be testable before upper layers can be built. Correctness concerns (crash safety, refcount integrity) must be addressed at foundation level — they cannot be retrofitted. The suggested phase structure is nine phases, matching the architecture research's build order.

### Phase 1: CAS Foundation

**Rationale:** Every other component depends on the block store and hash abstraction. Build the immutable foundation first; all later layers reference blocks but blocks reference nothing. Bloom filter and memory-bounded index must be in this phase — retrofitting them later risks the ZFS DDT problem at scale.

**Delivers:** `BlockStore` trait + `LocalDiskStore` impl; `ContentHasher` trait + BLAKE3 default; `ChunkIndex` with bloom filter + redb backend; collision detection in block store contract.

**Addresses features:** Block-level CAS deduplication, integrity verification on read (hash verification is free here), pluggable hash function.

**Avoids pitfalls:** Dedup index memory explosion (bloom filter from day one), hash collision absent handling (part of BlockStore contract), metadata store bottleneck (separate from block store from day one).

### Phase 2: Chunking Integration

**Rationale:** The owner has existing CDC technology that is the primary differentiator. Plugging it in early validates the `Chunker` trait design before anything depends on it. `fastcdc` provides an immediate test of the trait.

**Delivers:** `Chunker` trait; owner's CDC implementation as the default; `fastcdc` as the fallback; `WritePathEngine` (chunk → hash → store, no dedup yet); integration test: write file, verify block contents.

**Addresses features:** Pluggable chunking interface, content-defined chunking.

**Avoids pitfalls:** Block size selection locking in wrong tradeoffs (CDC from the start, not fixed-size).

### Phase 3: Metadata Engine

**Rationale:** The FUSE layer cannot be built without a working metadata store. Separating this from block storage ensures the clean boundary between content-addressed immutable data and structured mutable metadata.

**Delivers:** `Inode` struct, `FileManifest`, `DirEntry`, `xattr` store; `MetadataStore` trait + redb implementation; inode CRUD; directory listing and path resolution.

**Addresses features:** Stable inode numbers, file permissions, timestamps, extended attributes.

**Avoids pitfalls:** Metadata store bottleneck (redb from the start, not sled/SQLite), mixing metadata and block storage (enforced by separation).

### Phase 4: Read-Only FUSE Layer

**Rationale:** Mount and verify the filesystem is visible to the OS before adding write complexity. Read-only first isolates read path bugs from write path bugs. Validates the FuseHandler and VFS Adapter design under real kernel interaction.

**Delivers:** `FuseHandler` implementing `fuser::Filesystem`; VFS Adapter inode number mapping; `lookup()`, `getattr()`, `read()`, `readdir()`; mount and unmount of a read-only filesystem on Linux.

**Addresses features:** stat/fstat/lstat, directory list, CLI mount/unmount.

**Avoids pitfalls:** FUSE context-switch overhead (writeback_cache and spawn_blocking patterns established here; benchmark baseline set).

### Phase 5: Full Read/Write POSIX

**Rationale:** Full POSIX compliance is table stakes. This phase completes the write path with inline deduplication and POSIX operations. pjdfstest is the phase exit criterion — >95% pass rate before proceeding.

**Delivers:** Write path with inline dedup; `create()`, `unlink()`, `mkdir()`, `rmdir()`, `rename()`, `symlink()`, `link()`, `chmod()`, `chown()`, `utimens()`, `xattr` ops; `fsync()`/`fdatasync()`; `statfs()` with physical vs. logical reporting; pjdfstest >95%.

**Addresses features:** All P1 POSIX features, atomic rename, hard links, fsync correctness, correct statfs.

**Avoids pitfalls:** Inline dedup blocking write hot path (spawn_blocking from the start), FUSE context-switch overhead (writeback_cache enabled).

### Phase 6: Reference Counting + WAL

**Rationale:** Without correct refcounting and crash safety, the filesystem will leak blocks and corrupt data on crash. This phase makes the system production-safe. The WAL and refcount invariants must be provably correct before GC is built on top of them.

**Delivers:** `RefCountStore`; WAL with crash recovery; correct refcount maintenance through create/unlink/rename; crash test suite (kill -9 during write, verify clean restart).

**Addresses features:** Crash-safe GC prerequisite, fsync correctness completion.

**Avoids pitfalls:** Reference count corruption (WAL + two-phase intent), crash inconsistency (write blocks before committing metadata reference, recovery path written before write path ships).

### Phase 7: Garbage Collection

**Rationale:** Without GC, storage fills with orphaned blocks. GC must be built after the WAL/refcount layer is correct — it depends on refcount invariants holding. The GC race (new write lost during mark phase) must be addressed before shipping.

**Delivers:** `GcEngine` mark phase (walk manifests → live set); sweep phase (delete zero-refcount blocks); GC CLI command; WAL checkpoint after successful GC; concurrency test (GC chaos mode between every write step).

**Addresses features:** Garbage collection of orphan blocks.

**Avoids pitfalls:** GC race — new write lost during mark phase (pending-references protected set or RW lock).

### Phase 8: Read Path Optimization

**Rationale:** After correctness is established, make the filesystem fast enough for daily use. Chunk cache and bloom filter are the two highest-leverage optimizations for read-heavy workloads.

**Delivers:** `ChunkCache` (LRU in-memory by ChunkHash); bloom filter operationalized in ChunkIndex; persistent ChunkIndex (redb backend); read-ahead prefetch for sequential access; benchmark suite (fio: read throughput before and after cache).

**Addresses features:** Read performance, dedup-aware space reporting accuracy.

**Avoids pitfalls:** Read fragmentation accumulation (locality hint interface added to BlockStore even if not fully implemented yet).

### Phase 9: Production Hardening + Cross-Platform

**Rationale:** The system is functionally complete; this phase validates it under real conditions, adds macOS support, and establishes the benchmark baselines that define "production ready."

**Delivers:** macOS FUSE-T integration with macOS-specific POSIX test suite (mmap writes, flock, large directory, atime/mtime); Linux pjdfstest full pass; fsck/repair tool (orphan GC, dangling reference detection); comprehensive integration test suite; benchmark suite (dedup ratio, read/write throughput, index memory under 100GB unique data).

**Addresses features:** Cross-platform support (Linux + macOS), integrity scrub command.

**Avoids pitfalls:** FUSE-T macOS semantic gaps (explicit macOS test suite as phase exit criterion), memory bounds (100GB unique data write test).

### Phase Ordering Rationale

- **Bottom-up dependency order:** Phases 1-3 build the three independent storage layers (CAS, chunking, metadata) before any FUSE integration. The FUSE layer in Phase 4 depends on all three.
- **Correctness before features:** Reference counting and WAL (Phase 6) come before GC (Phase 7) because GC correctness depends on refcount invariants. Both come before production hardening.
- **Inline dedup from the start:** Phase 5 includes inline dedup in the write path. This avoids the "storage balloon" problem of post-process dedup and ensures the dedup pipeline is stress-tested throughout all subsequent phases, not retrofitted at the end.
- **Windows deferred:** Windows (WinFSP) is not in the 9-phase core roadmap. The `BlockStore` and `MetadataStore` traits established in Phase 1 and Phase 3 create the platform-abstracted boundary that enables Windows support as a separate subsequent milestone with no core changes.

### Research Flags

Phases needing deeper research during planning:
- **Phase 6 (Reference Counting + WAL):** Epoch-based deletion and two-phase commit protocols for CAS refcounting are subtle; the USENIX FAST 2013 concurrent deletion paper should be re-read at planning time.
- **Phase 7 (GC):** The GC race scenario (new write lost during mark phase) requires formal reasoning about the specific locking strategy; worth a focused research pass before implementation.
- **Phase 9 (macOS FUSE-T):** FUSE-T's NFS semantic gaps (mmap write visibility, flock bypass, attribute caching) are partially documented but require hands-on testing against the actual macOS version in use; FSKit (macOS 15+) may be a better long-term path.

Phases with standard, well-documented patterns (skip research-phase):
- **Phase 1 (CAS Foundation):** CAS with bloom filter + on-disk B-tree is a canonical pattern with multiple reference implementations (rdedup, casync, Borg).
- **Phase 2 (Chunking Integration):** Trait-based dependency injection is straightforward Rust; fastcdc API is well-documented.
- **Phase 4 (Read-Only FUSE):** fuser crate has example implementations; read-only FUSE is the simplest FUSE case.
- **Phase 8 (Read Path Optimization):** LRU chunk cache is a standard data structure; bloom filter integration is well-understood.

---

## Confidence Assessment

| Area | Confidence | Notes |
|------|------------|-------|
| Stack | HIGH | All core crates verified with release dates, download counts, and GitHub sources. fuser 0.17 confirmed Feb 2026 release. redb 3.1.1 confirmed Mar 2026 release. Only gap: macOS fuser marked "untested" in README but works with FUSE-T via API compatibility. |
| Features | MEDIUM-HIGH | Core POSIX requirements are HIGH confidence (well-specified). Dedup-specific features (dedup ratio, GC behavior) are MEDIUM — derived from ZFS/Btrfs documentation and academic research, not from a production Rust CAS filesystem codebase. |
| Architecture | HIGH | FUSE layer, CAS patterns, and ZFS/Btrfs reference systems are well-documented. GC strategies and WAL specifics are MEDIUM — design recommendations are from USENIX FAST papers and production systems, but Rust-specific implementation details require validation. |
| Pitfalls | HIGH | Sourced from USENIX FAST 2013/2015/2017 papers, ZFS post-mortems, and production crash consistency research (CrashMonkey). The pitfalls are well-documented failure modes, not speculation. |

**Overall confidence:** MEDIUM-HIGH

### Gaps to Address

- **fuser macOS support:** The fuser README marks macOS as "untested," but FUSE-T provides libfuse API compatibility. Practical validation of fuser on macOS Sequoia with FUSE-T should be done in Phase 9 planning, not assumed.
- **Owner's CDC algorithm interface:** The `Chunker` trait design (Phase 2) depends on understanding the interface of the owner's existing chunking technology. This must be clarified before Phase 2 planning — it is the primary differentiator and the central integration point.
- **redb 3.x API surface for filesystem patterns:** redb 3.x has breaking changes from 2.x. The specific API patterns for the inode table, manifest store, and chunk index need validation against the redb 3.x documentation before Phase 3 implementation.
- **Windows distribution license:** winfsp-rs is GPL-3. If the project intends commercial or proprietary distribution with Windows support, this is a blocking decision that must be resolved before any Windows work begins.
- **FUSE-T and FSKit (macOS 15+):** Apple's FSKit (available macOS 15+) may replace FUSE-T as the recommended macOS userspace filesystem mechanism. The architecture's FUSE interface abstraction should be designed to accommodate a future FSKit adapter without core changes.

---

## Sources

### Primary (HIGH confidence)
- [fuser on GitHub (cberner/fuser)](https://github.com/cberner/fuser) — version 0.17.0, platform support, Feb 2026 release
- [redb on GitHub (cberner/redb)](https://github.com/cberner/redb) — version 3.1.1, ACID/MVCC, Mar 2026 release
- [blake3 on crates.io](https://crates.io/crates/blake3) — version 1.8.x, 80M downloads
- [fastcdc on GitHub (nlfiedler/fastcdc-rs)](https://github.com/nlfiedler/fastcdc-rs) — version 3.2.1, async API
- USENIX FAST 2017: "To FUSE or Not to FUSE" — FUSE overhead quantification
- USENIX FAST 2017: "The Logic of Physical Garbage Collection in Deduplicating Storage" — GC correctness
- USENIX FAST 2013: "Concurrent Deletion in a Distributed CAS System" — refcount race conditions
- CrashMonkey (Microsoft Research 2021) — crash consistency bugs in filesystems

### Secondary (MEDIUM confidence)
- [Introducing OpenZFS Fast Dedup — Klara Systems](https://klarasystems.com/articles/introducing-openzfs-fast-dedup/) — DDT memory design
- [OpenZFS dedup is good now — despairlabs](https://despairlabs.com/blog/posts/2024-10-27-openzfs-dedup-is-good-dont-use-it/) — DDT practical memory costs
- [Borg Backup Internals](https://borgbackup.readthedocs.io/en/stable/internals/data-structures.html) — manifest pattern, rolling hash chunker
- [rdedup on GitHub](https://github.com/dpc/rdedup) — pluggable chunker/hasher/compressor traits in Rust
- [FUSE-T on GitHub](https://github.com/macos-fuse-t/fuse-t) — known NFS semantic gaps
- [winfsp-rs on GitHub](https://github.com/SnowflakePowered/winfsp-rs) — version 0.12.4, GPL-3, POSIX gaps
- [pjdfstest](https://github.com/saidsay-so/pjdfstest) — POSIX filesystem conformance suite

### Tertiary (MEDIUM-LOW confidence)
- [DFUSE: Strongly Consistent Write-Back Kernel Caching — arXiv 2025](https://arxiv.org/html/2503.18191v1) — writeback cache tradeoffs
- [POSIX Compatibility comparison — JuiceFS Blog](https://juicefs.com/en/blog/engineering/posix-compatibility-comparison-among-four-file-system-on-the-cloud) — practical POSIX gap analysis
- WebSearch: RocksDB Rust crate 0.24.0 — alternatives considered
- WebSearch: sha2 0.10.x RustCrypto — pluggable hash alternative

---
*Research completed: 2026-03-27*
*Ready for roadmap: yes*
