# Architecture Research

**Domain:** Deduplicating FUSE Filesystem (Rust)
**Researched:** 2026-03-27
**Confidence:** HIGH (FUSE layer, CAS patterns, ZFS/Btrfs reference systems); MEDIUM (GC strategies, WAL specifics)

## Standard Architecture

### System Overview

```
┌──────────────────────────────────────────────────────────────────┐
│                        FUSE LAYER                                │
│  Kernel VFS ←→ /dev/fuse ←→ fuser crate ←→ FuseHandler          │
│  (lookup, getattr, read, write, create, unlink, rename, xattr)  │
└──────────────────────────┬───────────────────────────────────────┘
                           │ POSIX ops (ino, fh, offset, size)
┌──────────────────────────▼───────────────────────────────────────┐
│                   VFS ADAPTER / ROUTER                           │
│  Maps FUSE inode numbers → internal InodeId                     │
│  Owns open file handle table (fh → FileState)                   │
│  Enforces POSIX semantics (unlink-while-open, nlookup lifecycle) │
└───────────┬─────────────────────────────────┬────────────────────┘
            │                                 │
┌───────────▼──────────┐         ┌────────────▼──────────────────┐
│  METADATA ENGINE     │         │     WRITE PATH ENGINE         │
│                      │         │                               │
│  Inode store         │         │  Chunker (pluggable trait)    │
│  Directory tree      │         │  Hash function (pluggable)    │
│  Timestamps/perms    │         │  Dedup lookup (chunk index)   │
│  xattrs              │         │  Block writer                 │
│  Hard link refcount  │         │  File manifest builder        │
│  (sled / sqlite)     │         │                               │
└───────────┬──────────┘         └────────────┬──────────────────┘
            │                                 │
            │         ┌───────────────────────▼──────────────────┐
            │         │         CHUNK INDEX                      │
            │         │  hash → block address mapping            │
            │         │  In-memory hot layer (LRU/bloom filter)  │
            │         │  Persistent layer (sled / rocksdb)       │
            │         └───────────────┬──────────────────────────┘
            │                         │
┌───────────▼─────────────────────────▼──────────────────────────┐
│                   BLOCK STORE (CAS)                             │
│  trait BlockStore { put(hash, data); get(hash) → data; del; }  │
│                                                                 │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐          │
│  │  LocalDisk   │  │  ObjectStore │  │  Future:     │          │
│  │  (files by   │  │  (S3/GCS     │  │  Distributed │          │
│  │   hash path) │  │   shim)      │  │  P2P backend │          │
│  └──────────────┘  └──────────────┘  └──────────────┘          │
└─────────────────────────────────────────────────────────────────┘
            │
┌───────────▼─────────────────────────────────────────────────────┐
│                REFERENCE COUNTER + GC ENGINE                    │
│  Block refcount table (hash → u64)                              │
│  Orphan detection (mark phase: walk all manifests)              │
│  Sweep phase: delete blocks with refcount == 0                  │
│  WAL / journal: staged refcount mutations for crash safety      │
└─────────────────────────────────────────────────────────────────┘
```

### Component Responsibilities

| Component | Responsibility | Typical Implementation |
|-----------|----------------|------------------------|
| **FuseHandler** | Translate kernel FUSE opcodes into internal ops; reply with FileAttr/data | Implements `fuser::Filesystem` trait |
| **VFS Adapter** | Map FUSE inode numbers to internal InodeId; manage open file handles; enforce nlookup lifecycle | HashMap<u64, InodeId>, HashMap<u64, FileState> |
| **Metadata Engine** | Own all filesystem metadata: inodes, directory entries, permissions, xattrs, link counts | Embedded KV store (sled/sqlite). Separate from block store. |
| **Write Path Engine** | Accept byte stream on write(), chunk it, hash chunks, deduplicate, persist new blocks, update file manifest | Trait-dispatched chunker + hasher; calls chunk index |
| **Chunk Index** | Map content hash → block store address + refcount. The deduplication lookup table. | In-memory LRU + bloom filter; persistent sled/rocksdb layer |
| **Block Store (CAS)** | Immutable content-addressed storage of raw chunk bytes. Keyed by hash. | `trait BlockStore` with local disk impl; swappable backend |
| **File Manifest** | Per-file ordered list of chunk hashes that reconstruct file content | Stored as inode attribute in metadata engine |
| **Reference Counter** | Track how many file manifests reference each chunk hash | Maintained transactionally during writes and unlinks |
| **GC Engine** | Reclaim blocks with zero references (two-phase: mark live hashes, sweep dead blocks) | Background task; runs on demand or scheduled |
| **WAL / Journal** | Ensure crash safety: refcount mutations and metadata updates logged before application | Append-only log; replayed on mount |
| **Read Path / Cache** | Reassemble file content from chunk hashes; cache hot chunks and reassembled regions | Chunk cache (LRU by hash); page-aligned read-ahead |

## Recommended Project Structure

```
src/
├── main.rs                    # Mount entrypoint, CLI args, signal handling
├── fuse/
│   ├── handler.rs             # Implements fuser::Filesystem trait (FuseHandler)
│   ├── adapter.rs             # VFS adapter: inode number mapping, file handle table
│   └── reply.rs               # Helper builders for fuser reply types
├── metadata/
│   ├── mod.rs
│   ├── inode.rs               # Inode struct: ino, mode, uid, gid, size, times, link_count
│   ├── dir.rs                 # Directory entries: parent ino → [(name, child ino)]
│   ├── manifest.rs            # File manifest: ino → Vec<ChunkHash>
│   ├── xattr.rs               # Extended attributes store
│   └── store.rs               # trait MetadataStore + sled/sqlite implementation
├── cas/
│   ├── mod.rs
│   ├── block_store.rs         # trait BlockStore { put, get, delete, exists }
│   ├── local.rs               # LocalDiskStore: blocks/ab/cd/<full-hash>
│   └── chunk_index.rs         # ChunkIndex: hash → (address, refcount), bloom filter
├── dedup/
│   ├── mod.rs
│   ├── chunker.rs             # trait Chunker { chunk(reader) → Iterator<Chunk> }
│   ├── hasher.rs              # trait ContentHasher { hash(data) → ChunkHash }
│   └── write_path.rs          # WritePathEngine: orchestrates chunk + hash + dedup + store
├── refcount/
│   ├── mod.rs
│   ├── counter.rs             # RefCountStore: hash → u64, atomic increment/decrement
│   └── journal.rs             # WAL entries for staged refcount mutations
├── gc/
│   ├── mod.rs
│   └── sweep.rs               # GcEngine: mark phase + sweep phase
├── cache/
│   ├── mod.rs
│   └── chunk_cache.rs         # LRU cache: ChunkHash → Arc<[u8]>
└── error.rs                   # Unified error types
```

### Structure Rationale

- **fuse/:** Thin translation layer. Must not contain business logic — it only maps kernel opcodes to internal calls and vice versa. Keeping it thin makes the rest testable without FUSE.
- **metadata/:** Completely separate from block/CAS storage. Metadata (inodes, directories, manifests) is structured and transactional; block data is immutable and content-addressed. Mixing these causes the ZFS DDT problem: metadata and data lifetime management become entangled.
- **cas/:** The content-addressable block store is the stable core. Everything else references blocks; blocks reference nothing. This makes CAS the foundation to build first.
- **dedup/:** The chunker and hasher are pluggable traits. The write path engine wires them together but does not own them — it receives them via dependency injection. This is the integration point for the owner's existing chunking technology.
- **refcount/:** Reference counting is separated from the chunk index because its mutation pattern is transactional (must be atomic with metadata changes), while the chunk index is primarily a lookup structure.
- **gc/:** Isolated as a background concern. GC reads refcounts and block lists; it does not need to be on the write path.

## Architectural Patterns

### Pattern 1: Content-Addressable Storage with Two-Level Index

**What:** Raw block bytes are stored immutably keyed by their cryptographic hash. A separate index maps each known hash to its physical storage address. The in-memory layer uses a bloom filter to avoid disk I/O for definitely-absent hashes.

**When to use:** Always — this is the core deduplication primitive. ZFS, Borg, rdedup, casync all use this pattern.

**Trade-offs:** Lookup is O(1) amortized; storage is perfectly deduplicated for identical blocks; but index size grows linearly with unique block count and must eventually be managed.

**Example (Rust sketch):**
```rust
pub trait BlockStore: Send + Sync {
    fn put(&self, hash: &ChunkHash, data: &[u8]) -> Result<()>;
    fn get(&self, hash: &ChunkHash) -> Result<Bytes>;
    fn exists(&self, hash: &ChunkHash) -> Result<bool>;
    fn delete(&self, hash: &ChunkHash) -> Result<()>;
}

// Bloom filter fast-path before hitting disk
pub struct ChunkIndex {
    bloom: BloomFilter,          // probabilistic "definitely not present"
    index: sled::Tree,           // hash bytes → address + refcount
}
```

### Pattern 2: Inode-to-Manifest Indirection (Thin Inode)

**What:** Inodes do not directly contain file data or even block pointers. Each inode stores a reference to a file manifest (an ordered list of chunk hashes). The inode contains only metadata (size, times, mode, uid/gid, link count). This mirrors how Borg stores items as a stream of chunk references.

**When to use:** Essential for FUSE deduplicating filesystems. Decouples logical file identity (inode) from physical storage (chunks). Enables hard links, reflinks, and future snapshotting without data duplication.

**Trade-offs:** One extra indirection on read (inode → manifest → chunks); justified because it enables atomic file replacement (swap the manifest reference), cheap snapshotting (copy the manifest), and clean hard link semantics (multiple inodes reference the same manifest).

**Example (Rust sketch):**
```rust
pub struct Inode {
    pub ino: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub atime: SystemTime,
    pub mtime: SystemTime,
    pub ctime: SystemTime,
    pub nlink: u32,
    pub manifest_id: Option<ManifestId>,  // None for dirs/symlinks
}

pub struct FileManifest {
    pub id: ManifestId,
    pub chunks: Vec<ChunkHash>,   // ordered; reconstruct by concatenation
}
```

### Pattern 3: Staged Write Buffer with Inline Deduplication

**What:** On `write()`, data is accumulated in a per-file write buffer until a flush boundary (close, fsync, explicit flush, or buffer full). At flush time, the buffer is chunked, hashed, deduplicated against the chunk index, and committed atomically. This is inline deduplication: dedup happens before blocks reach persistent storage.

**When to use:** Preferred for a daily-driver filesystem. Post-process deduplication is simpler but wastes disk I/O on writes that will later be deduplicated. Inline deduplication reduces write amplification and storage usage from the first write.

**Trade-offs:** Inline dedup adds latency to the flush path proportional to chunk index lookup time. For workloads with large write bursts, the write buffer smooths this into batches. Post-process dedup would be simpler to implement first and can be replaced later.

**Recommendation:** Build inline dedup from the start. The write buffer absorbs latency variance. Post-process dedup is a reasonable MVP shortcut but creates user-visible storage "balloons" before dedup runs — unacceptable for a daily-driver filesystem.

### Pattern 4: Transactional Refcount Mutations via WAL

**What:** When a chunk's reference count changes (block added or block dereferenced on file delete), the refcount mutation is written to a WAL entry before the metadata is updated. On crash recovery, the WAL is replayed to restore consistent refcounts before mounting.

**When to use:** Any system where chunk deletion must be safe. Incorrect refcounts lead to premature GC (data loss) or leaked blocks (storage leak). ZFS uses transaction groups; Borg uses a transaction-safe key-value store.

**Trade-offs:** Adds a WAL write on every file create/delete that changes chunk references. Sequential WAL writes are fast. The alternative (in-place refcount updates) risks inconsistency on crash.

**Example WAL entry types:**
```rust
pub enum WalEntry {
    ChunkRefIncrement { hash: ChunkHash, delta: u64 },
    ChunkRefDecrement { hash: ChunkHash, delta: u64 },
    InodeCreate { ino: u64, manifest_id: ManifestId },
    InodeDelete { ino: u64 },
    ManifestReplace { manifest_id: ManifestId, chunks: Vec<ChunkHash> },
}
```

### Pattern 5: Mark-and-Sweep Garbage Collection

**What:** Two-phase GC. Mark phase: walk all file manifests, collect the set of all referenced chunk hashes. Sweep phase: iterate all stored blocks; delete any block whose hash is not in the live set.

**When to use:** Triggered manually by user command or periodically in a background task. Not on the hot path. Borg's `check --repair` and `compact` commands follow this pattern.

**Trade-offs:** Mark phase requires walking all metadata (can be slow on large filesystems). For the local MVP, this is acceptable. Future optimization: maintain a live/dead bloom filter incrementally via refcount transitions.

## Data Flow

### Write Path (Inline Deduplication)

```
Application write(fd, buf, offset)
    ↓
[FuseHandler.write()] — receives raw bytes from kernel
    ↓
[VFS Adapter] — looks up FileState by file handle (fh)
    ↓
[Write Buffer] — accumulates bytes; triggers flush at boundary
    ↓ (on flush)
[Chunker] — splits buffer using pluggable chunking strategy
    ↓ (per chunk)
[ContentHasher] — computes chunk hash (e.g., BLAKE3)
    ↓
[ChunkIndex.lookup(hash)] — bloom filter fast path
    ├── PRESENT: increment refcount, record hash in manifest (no I/O)
    └── ABSENT:
            ↓
        [BlockStore.put(hash, bytes)] — persist new block to CAS
            ↓
        [ChunkIndex.insert(hash, address)] — update index
            ↓
        [RefCountStore.increment(hash)] — record new reference
    ↓
[FileManifest.append(hash)] — update ordered chunk list for this file
    ↓
[WAL.append(ManifestReplace + refcount deltas)] — durability
    ↓
[MetadataStore.update_inode(size, mtime)] — update inode metadata
```

### Read Path

```
Application read(fd, offset, size)
    ↓
[FuseHandler.read()] — receives offset + size from kernel
    ↓
[VFS Adapter] — looks up FileState → InodeId → ManifestId
    ↓
[MetadataStore.get_manifest(manifest_id)] — retrieve chunk hash list
    ↓
[Manifest resolver] — compute which chunks cover [offset, offset+size)
    ↓ (per needed chunk)
[ChunkCache.get(hash)] — in-memory LRU cache
    ├── HIT: return cached bytes
    └── MISS:
            ↓
        [BlockStore.get(hash)] — fetch from CAS
            ↓
        [ChunkCache.insert(hash, bytes)] — populate cache
    ↓
[Reassembler] — stitch chunk bytes, slice to requested [offset, size]
    ↓
[fuser reply_data(bytes)] → kernel → application
```

### Unlink / Delete Path

```
Application unlink(path) or rmdir
    ↓
[FuseHandler.unlink(parent_ino, name)]
    ↓
[VFS Adapter] — resolve name → InodeId
    ↓
[MetadataStore.get_inode(ino)] — check nlink
    ├── nlink > 1 (hard link): decrement nlink, update inode only
    └── nlink == 1 (last reference):
            ↓
        [MetadataStore.get_manifest(manifest_id)] — get chunk list
            ↓
        [WAL.append(refcount decrements for all chunks)]
            ↓
        [RefCountStore.decrement_batch(chunks)] — update refcounts
            ↓
        [MetadataStore.delete_inode(ino) + delete_manifest(id)]
    ↓
(Zero-refcount blocks remain until GC sweep — safe lazy cleanup)
```

### Garbage Collection Path

```
User runs: dedupfs gc (or background scheduler triggers)
    ↓
[GcEngine.run()]
    ↓
[Mark phase]
    Scan all inodes → collect all manifests → union all chunk hashes
    Result: HashSet<ChunkHash> (live set)
    ↓
[Sweep phase]
    Iterate ChunkIndex entries
    For each hash: if hash NOT in live set AND refcount == 0:
        BlockStore.delete(hash)
        ChunkIndex.remove(hash)
    ↓
[WAL.truncate()] — checkpoint WAL after successful GC
```

## Scaling Considerations

This is a local single-node filesystem for daily use. Scaling here means "handles large working sets without degrading" not "handles many concurrent users."

| Concern | At 10K files (small) | At 1M files (large) | At 10M+ files (extreme) |
|---------|----------------------|---------------------|--------------------------|
| Chunk index memory | Fits in memory easily | Bloom filter + sled on-disk index required | Tiered index; memory-mapped hot region |
| Metadata store | SQLite/sled trivially sufficient | sled with careful key design | RocksDB or similar LSM |
| GC mark phase | Fast full scan | Incremental mark needed | Reference graph optimization (FGC approach) |
| Read latency | In-memory chunk cache dominates | Cache miss → disk; SSD essential | Larger chunk cache; read-ahead prefetch |
| Write throughput | Chunker CPU-bound | Parallel chunk processing | Concurrent write paths per file |

### Scaling Priorities

1. **First bottleneck:** Chunk index memory pressure on large repos. Mitigation: bloom filter in front of on-disk sled tree so most lookups skip disk for definitely-absent chunks.
2. **Second bottleneck:** GC mark phase duration on file deletion. Mitigation: lazy GC (accumulate dead chunks, sweep periodically) rather than GC on every delete.
3. **Third bottleneck:** Metadata store write contention. Mitigation: sled or sqlite with WAL mode handles concurrent reads well; writes are serialized through the VFS adapter.

## Anti-Patterns

### Anti-Pattern 1: Mixing Metadata and Block Storage

**What people do:** Store inodes and block data in the same key-value store or database, using different key prefixes.

**Why it's wrong:** Metadata and block data have very different access patterns. Metadata is read on every filesystem operation; block data is read only when file content is accessed. Mixing them in one store causes metadata operations to contend with large block I/O. GC becomes harder (how do you scan only blocks?). The boundary between "metadata schema" and "storage format" becomes unclear.

**Do this instead:** Maintain completely separate stores. MetadataStore owns inodes, directories, manifests. BlockStore owns raw chunk bytes. ChunkIndex is a third, separate structure. Each has its own read/write patterns and can be independently tuned or swapped.

### Anti-Pattern 2: Eager Refcount Decrement on Unlink

**What people do:** When a file is deleted, immediately decrement all chunk refcounts and delete zero-refcount blocks in the same transaction.

**Why it's wrong:** Deleting a large file with many chunks requires touching many index entries synchronously on the hot path. On a file with 100K chunks, this serializes 100K block deletions into the unlink handler. Worse, if the system crashes mid-delete, the filesystem may be in an inconsistent state (some blocks deleted, some not).

**Do this instead:** Decrement refcounts in the WAL but defer physical block deletion to the background GC engine. The unlink path becomes: log refcount decrements → update metadata → return. GC later cleans up zero-refcount blocks safely.

### Anti-Pattern 3: Fixed Block Size Without Pluggable Chunking Boundary

**What people do:** Hard-code a fixed block size (e.g., 4KB) as the chunking granularity.

**Why it's wrong:** Fixed-size chunking destroys deduplication across data that has been offset-shifted (inserting one byte at the start of a file changes every subsequent block boundary). The owner has existing rolling-hash/content-defined chunking technology for exactly this reason. Fixed-size chunking is only appropriate as a fallback or for append-only workloads.

**Do this instead:** Abstract chunking behind a `trait Chunker`. Ship with the owner's existing CDC implementation as the default. Allow fixed-size as an alternative strategy selectable at mount time.

### Anti-Pattern 4: In-Memory-Only Chunk Index

**What people do:** Keep the entire hash-to-address index in memory as a HashMap.

**Why it's wrong:** The index grows proportionally to the number of unique chunks. A 1TB filesystem with 4MB average chunk size has 250K unique chunks. A 1MB average chunk size has 1M unique chunks. At 64 bytes per index entry, 1M chunks = 64MB just for the index — before accounting for HashMap overhead. For a daily-driver filesystem, this is unacceptable long-term.

**Do this instead:** Use a bloom filter as a probabilistic first layer (fast "definitely not present" check) backed by an on-disk sled tree. Hot entries naturally stay in sled's block cache. This is the BloomStore pattern from FAST'12 research.

### Anti-Pattern 5: Implementing POSIX Semantics in the Block Store

**What people do:** Teach the block store about files, directories, and permissions.

**Why it's wrong:** The block store should be purely content-addressed: put/get/delete by hash. Any POSIX knowledge in the block store makes it impossible to swap backends (e.g., replace local disk with S3). It also makes testing much harder.

**Do this instead:** All POSIX semantics live in the VFS Adapter and Metadata Engine. The block store is a dumb blob store. This is the boundary that enables future distributed/P2P backends without changes to the POSIX layer.

## Integration Points

### External Services

| Service | Integration Pattern | Notes |
|---------|---------------------|-------|
| FUSE kernel module | fuser crate session loop; /dev/fuse fd | On macOS: FUSE-T via NFS; on Linux: libfuse kernel module |
| FUSE-T (macOS) | fuser auto-detects via mount flags | No kext required; uses NFS transport internally |
| WinFSP (Windows) | fuser WinFSP backend (future) | Separate mount mechanism; same Filesystem trait |
| OS page cache | `direct_io` flag controls bypass; writeback vs write-through mode | Write-through by default is safest; write-back doubles throughput but requires careful cache invalidation |

### Internal Boundaries

| Boundary | Communication | Notes |
|----------|---------------|-------|
| FuseHandler → VFS Adapter | Direct method calls (same process) | VFS Adapter holds mutable state; must be Arc<Mutex> or use message passing |
| VFS Adapter → Metadata Engine | Synchronous read/write via MetadataStore trait | Trait allows swapping sled for sqlite or future distributed metadata |
| VFS Adapter → Write Path Engine | Synchronous call on flush boundary | Write engine is stateless; VFS Adapter owns the write buffer per file handle |
| Write Path Engine → Chunker | Trait method call (pluggable) | Owner's existing chunking tech plugs in here via the Chunker trait |
| Write Path Engine → ChunkIndex | Lookup and insert per chunk | Hot path; must be fast; bloom filter in front |
| Write Path Engine → BlockStore | put() only (immutable once written) | Trait abstraction; local disk impl first |
| RefCountStore → WAL | WAL writes before refcount mutations commit | Crash safety boundary |
| GC Engine → BlockStore | delete() for zero-refcount blocks | Only path that deletes from CAS; must check refcount atomically |
| GC Engine → MetadataStore | Read-only during mark phase | GC must not modify metadata during scan |

## Suggested Build Order

Dependencies drive this order: lower layers must exist before upper layers can be tested.

```
Phase 1 — CAS Foundation
    BlockStore trait + LocalDiskStore impl
    ContentHasher trait + BLAKE3 impl
    ChunkIndex (in-memory only, no bloom filter yet)
    Basic unit tests: put/get/exists round-trip

Phase 2 — Chunking Integration
    Chunker trait
    Plug in owner's existing CDC implementation
    WritePathEngine: chunk → hash → store (no dedup yet)
    Integration test: write file, verify block contents

Phase 3 — Metadata Engine
    Inode struct, FileManifest, DirEntry
    MetadataStore trait + sled implementation
    Inode create/read/update/delete
    Directory listing and path resolution

Phase 4 — FUSE Layer (minimal read-only first)
    FuseHandler implementing fuser::Filesystem
    VFS Adapter: inode number mapping
    lookup(), getattr(), read(), readdir()
    Mount and unmount a read-only filesystem

Phase 5 — Full Read/Write POSIX
    Write path: write(), flush(), fsync()
    Inline deduplication in write path
    create(), unlink(), mkdir(), rmdir(), rename()
    symlink(), link(), chmod(), chown(), utimens()
    xattr operations
    POSIX compliance test suite

Phase 6 — Reference Counting + WAL
    RefCountStore
    WAL with crash recovery
    Correct refcount maintenance through create/unlink/rename

Phase 7 — Garbage Collection
    Mark phase: walk manifests to live set
    Sweep phase: delete dead blocks
    GC CLI command

Phase 8 — Read Path Optimization
    ChunkCache (LRU in-memory)
    Bloom filter in ChunkIndex
    Persistent ChunkIndex (sled backend)
    Read-ahead prefetch for sequential access

Phase 9 — Production Hardening
    Error recovery and fsck-style repair
    Comprehensive integration tests
    Benchmark suite (dedup ratio, read/write throughput)
    Cross-platform validation (macOS FUSE-T, Linux libfuse)
```

## Reference Systems

| System | Type | Key Lessons |
|--------|------|-------------|
| **ZFS** | Kernel filesystem | DDT (dedup table) as central hash→address map; inline dedup; transaction groups for atomicity; DDT must fit in ARC RAM for performance |
| **Btrfs** | Kernel filesystem | Extent tree for block reference counting; COW as foundation for dedup; out-of-band dedup via ioctl; refcount in extent_item struct |
| **Borg** | Backup archiver | Repository = low-level KV store; Manifest = root of all archives; rolling hash chunker (Buzhash); global dedup across all backups; FUSE mount for restore |
| **casync** | Image sync tool | Removes file boundaries before chunking; chunk store as directory tree; SHA256 + xz per chunk; index file separate from chunk store |
| **Perkeep** | Personal storage | blobpacked: small blobs merged into large zip files for efficient access; metadata index separate from blob store; recovery via index rebuild |
| **rdedup** | Dedup engine (Rust) | Recursive index (index-of-index until single hash); pluggable chunker/hasher/compressor via traits; immutable conflict-free store design; incremental GC |

## Sources

- [ZFS Deduplication — TrueNAS Documentation](https://www.truenas.com/docs/references/zfsdeduplication/)
- [Introducing OpenZFS Fast Dedup — Klara Systems](https://klarasystems.com/articles/introducing-openzfs-fast-dedup/)
- [Btrfs Design — BTRFS Documentation](https://btrfs.readthedocs.io/en/latest/dev/dev-btrfs-design.html)
- [Btrfs: how reference counting works — Josef Bacik](https://josefbacik.github.io/kernel/btrfs/2021/12/16/btrfs-extent-reference-counting.html)
- [Btrfs Deduplication — BTRFS Documentation](https://btrfs.readthedocs.io/en/latest/Deduplication.html)
- [Borg Internals — Data Structures and File Formats](https://borgbackup.readthedocs.io/en/stable/internals/data-structures.html)
- [casync — Content-Addressable Data Synchronization Tool](https://github.com/systemd/casync)
- [Perkeep blobpacked package](https://pkg.go.dev/perkeep.org/pkg/blobserver/blobpacked)
- [rdedup — Data Deduplication Engine (Rust)](https://github.com/dpc/rdedup)
- [fuser — Filesystem in Userspace for Rust](https://docs.rs/fuser/latest/fuser/trait.Filesystem.html)
- [fuser crate on crates.io](https://crates.io/crates/fuser)
- [FUSE Inode Lifecycle — libfuse low-level ops](https://libfuse.github.io/doxygen/structfuse__lowlevel__ops.html)
- [The Logic of Physical Garbage Collection in Deduplicating Storage — USENIX FAST'17](https://www.usenix.org/system/files/conference/fast17/fast17-douglis.pdf)
- [Sparse Indexing: Large Scale Inline Deduplication — USENIX FAST'09](https://www.usenix.org/legacy/events/fast09/tech/full_papers/lillibridge/lillibridge_html/index.html)
- [Scalable Filesystem Metadata Services with RocksDB — Alluxio](https://www.alluxio.io/resources/presentations/scalable-filesystem-metadata-services-with-rocksdb/)
- [Inline vs Post-processing Deduplication — TechTarget](https://www.techtarget.com/searchdatabackup/tutorial/Inline-deduplication-vs-post-processing-Data-dedupe-best-practices)
- [FUSE Caching Overview — Google Cloud Storage FUSE](https://docs.cloud.google.com/storage/docs/cloud-storage-fuse/caching)
- [Linux FUSE Performance — Medium](https://medium.com/@xiaolongjiang/linux-fuse-file-system-performance-learning-efb23a1fb83f)

---
*Architecture research for: Deduplicating FUSE Filesystem (Rust / DedupFS)*
*Researched: 2026-03-27*
