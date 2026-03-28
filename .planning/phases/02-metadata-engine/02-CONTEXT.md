# Phase 2: Metadata Engine - Context

**Gathered:** 2026-03-28
**Status:** Ready for planning

<domain>
## Phase Boundary

Inode table, directory tree, file manifests, and xattr store as a metadata layer built on top of data-id's CAS Dictionary. Everything (metadata + content) lives in one shared Dictionary. No separate database. FUSE can be wired on top of it.

Requirements: META-03, POSIX-06, POSIX-07, POSIX-08, POSIX-10

</domain>

<decisions>
## Implementation Decisions

### data-id integration
- Adapt slicefs-traits to fit data-id's native types (Digest224, Digest256, StorageAdd/StorageGet, Tree/State) — not an adapter layer over existing traits
- data-id (https://github.com/o2alexanderfedin/data-id.git) cloned as a git submodule — not a crates.io dependency — to allow future optimizations
- SHA-224 is the hash algorithm: Digest224 `[u32; 7]` is the actual 224-bit hash used as Dictionary keys; Digest256 `[u32; 8]` is a tagged union container where word[7] is either `0xFFFFFFFF` (hash marker) or encodes inline data bit-length
- Keep `&self` + internal sync pattern from current slicefs-traits — wrap data-id's `&mut self` (StorageAdd) with Mutex/RwLock internally for Arc<dyn Trait> sharing across FUSE threads
- Dedup is implicit via content addressing — no separate bloom filter or dedup index needed (data-id's model: same content = same hash = same tree node)

### Storage architecture
- One shared Dictionary for everything — metadata and content stored as CAS tree nodes in the same Dictionary instance
- No separate embedded database (redb/sled/SQLite) — data-id's Dictionary IS the storage layer
- Filesystem state at any point in time = a single root Digest224
- Time travel / snapshots = a journal of root Digest224 values with timestamps/sequence numbers
- Switching temporal slices = traversing from a different root digest — all old nodes remain in Dictionary (CAS never overwrites)

### Inode model
- Persistent inode-number-to-digest map: a u64 inode number maps to the current Digest224 of the inode's subtree root. The map itself is stored as a CAS tree. Inode numbers survive renames and content changes (satisfies POSIX-10)
- Per-entry directory tree nodes: each directory entry is a separate subtree node keyed by name hash. Changes to one entry don't require re-serializing the whole directory. Supports large directories efficiently

### GC considerations
- No GC hooks or design decisions needed in Phase 2 — GC operates at the Dictionary/storage level (Phase 5)
- The CAS-everything model naturally supports GC via root reachability: traverse from live roots (current + pinned snapshots), mark reachable nodes, sweep unreachable
- SSD-optimized GC (segment-based compaction, batch deletes, TRIM) is a storage layout concern deferred to Phase 5

### Claude's Discretion
- Inode field serialization format (fixed-size binary struct recommended — pack mode, uid, gid, size, timestamps, nlinks into a compact byte buffer that fits in a small number of Digest256 leaf nodes)
- Xattr storage model (recommended: subtree under inode node — xattr names as entry keys, values as leaf data — handles both small Finder tags and large SELinux policies without size limits)
- . and .. directory entry handling
- Error type design for metadata-specific failures

</decisions>

<code_context>
## Existing Code Insights

### data-id API surface (from git submodule)
- `Digest256 = [u32; 8]` — tagged union: hash (word[7] = 0xFFFFFFFF) or inline data (word[7] top byte = bit-length)
- `Digest224 = [u32; 7]` — pure 224-bit hash, used as Dictionary key
- `Branches = [Digest256; 2]` — tree node value (left + right children)
- `Dictionary = BTreeMap<Digest224, Branches>` — in-memory storage, implements StorageAdd + StorageGet
- `StorageAdd { fn add(&mut self, left: &Digest256, right: &Digest256) -> Digest256; fn end(&mut self, x: &Digest256) -> Digest224; }`
- `StorageGet { fn get(&self, key: &Digest224) -> Option<Branches>; }`
- `Tree` trait with `State` (CDC) and `MerkleTreeState` implementations
- `GetData<S: StorageGet>` / `GetBytes<S: StorageGet>` for tree traversal and byte iteration
- `compress(a: &Digest256, b: &Digest256) -> Digest256` — SHA-224 when overflow, inline concat otherwise
- `FileStorageAdd` — file-backed storage implementation
- All sync, no serde, hand-rolled binary serialization

### Reusable Assets in slicefs
- `slicefs-traits` crate — needs redesign to align with data-id types (replace ChunkHash(Vec<u8>) with Digest224/Digest256)
- `cas-local` crate — stub implementations (MemBlockStore, Blake3Hasher, FixedChunker, MemDedupIndex) — most will be replaced by data-id adapters
- Existing trait pattern: `&self`, `Send + Sync`, `Result<T, CasError>` — keep this ergonomic pattern

### Integration Points
- `slicefs-traits` must be refactored to use data-id's Digest224/Digest256 instead of ChunkHash(Vec<u8>)
- BlockStore, DedupIndex traits may be collapsed into StorageAdd/StorageGet adapters
- ContentHasher trait becomes thin wrapper around data-id's compress() function
- Chunker trait maps to data-id's Tree/State CDC implementation

</code_context>

<specifics>
## Specific Ideas

- SHA-224 chosen deliberately: 224-bit hash fits in 7 x u32 words, leaving the 8th word (32 bits) as a metadata/tag field in the Digest256 container
- data-id's inline data optimization: short data (up to 31 bytes) stored directly inside the Digest256 — no separate tree node needed. This is a key efficiency feature to preserve
- The Dictionary model gives free deduplication across time slices — unchanged subtrees share nodes between filesystem versions
- data-id's CDC (`State` type) uses monotone increasing digest comparison for chunk boundaries — not Rabin/Gear fingerprinting

</specifics>

<deferred>
## Deferred Ideas

- **Phase 1 trait refactoring**: slicefs-traits needs redesign to align with data-id. This may be a Phase 1.5 or rolled into Phase 2 planning
- **GC and SSD optimization**: Segment-based compaction, batch deletes, TRIM/discard — deferred to Phase 5
- **Snapshot pinning protocol**: How to mark roots as live for GC — deferred to Phase 5/6
- **Async wrappers**: For distributed backends in v2 milestone
- **data-id submodule optimizations**: Future performance work on the algorithms themselves

</deferred>

---

*Phase: 02-metadata-engine*
*Context gathered: 2026-03-28*
