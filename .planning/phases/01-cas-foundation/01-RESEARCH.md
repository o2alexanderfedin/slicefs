# Phase 1: CAS Foundation - Research

**Researched:** 2026-03-27
**Domain:** Rust trait interface design for content-addressable storage (CAS)
**Confidence:** HIGH

<user_constraints>
## User Constraints (from CONTEXT.md)

### Locked Decisions
- Owner has existing Rust crates that provide the CAS API, block storage, addressing, hashing, chunking, and dedup index — these are separate published Rust crates
- Phase 1 does NOT integrate the owner's algorithms — it defines filesystem-side trait interfaces with stub/test implementations only
- The actual storage layout, addressing scheme, and dedup index design will come from the owner's algorithms via adapters in a later dedicated phase
- Dedicated phase(s) will be added to the roadmap for reviewing the owner's algorithms and designing/building adapters to fit them behind the filesystem traits
- This is a deliberate "traits first, integration second" approach
- Cargo workspace with subcrates per component; initial subcrates: cas-traits, cas-local (stub/test implementations), metadata, fuse-frontend, plus more as needed
- Sync traits first — owner's existing algorithms are synchronous, and fuser uses sync callbacks (thread-per-request model)
- Async traits/wrappers will be added when distributed backends (v2) introduce network I/O
- Buffered I/O (whole block as Vec<u8> or &[u8]) — driven by fuser's byte-slice callback model
- No premature async refactoring of owner's algorithms
- Do not make assumptions on chunking and hashing — the owner will provide the real algorithms later
- Owner emphasized: adapters are expected, not direct use

### Claude's Discretion
- Error handling strategy (recommend thiserror for typed errors + anyhow for application code)
- Crate naming convention (recommend dedupfs-* prefix)
- Stub implementation details (in-memory HashMap-based BlockStore for testing)
- Test harness design and property-based testing approach

### Deferred Ideas (OUT OF SCOPE)
- Roadmap adjustment: Adding dedicated phase(s) for reviewing owner's existing algorithm crates and building filesystem adapters — this is out of scope for Phase 1
- Owner's algorithms may need async refactoring for distributed backends — defer to v2 milestone
</user_constraints>

<phase_requirements>
## Phase Requirements

| ID | Description | Research Support |
|----|-------------|-----------------|
| CAS-01 | Block-level content-addressable storage with pluggable hash function trait | ContentHasher trait design; BLAKE3 stub impl; see Standard Stack and Architecture Patterns |
| CAS-02 | Pluggable chunking/block-splitting trait interface (concrete algorithms provided by owner's existing technology) | Chunker trait design; fixed-size stub for tests; owner's real algorithm integrated in later phase |
| CAS-03 | Pluggable storage backend trait for CAS blocks with local disk implementation | BlockStore trait design; LocalDiskStore flat-file impl with 2-byte directory sharding |
| CAS-05 | Integrity verification on read (re-hash block, compare to stored hash, configurable on/off) | Re-hash on get() in LocalDiskStore; configurable via BlockStoreConfig; see Code Examples |
| CAS-07 | On-disk dedup index with bounded memory usage (no full DDT in RAM) | DedupIndex trait; in-memory HashMap stub for Phase 1; bloom filter (fastbloom) as pre-filter; bloom filter serialized to disk for persistence boundary |
</phase_requirements>

---

## Summary

Phase 1 delivers the trait contracts that every subsequent component depends on — NOT the production implementations. The owner has unpublished Rust crates for hashing, chunking, block storage, addressing, and dedup index; those plug in as adapter impls of these traits in a dedicated later phase. Phase 1's job is to define clean, filesystem-oriented trait surfaces and prove they work via stub/test implementations.

The architecture is trait-driven dependency injection throughout. `ContentHasher`, `Chunker`, `BlockStore`, and `DedupIndex` are all abstract traits defined in a single `dedupfs-traits` (or `cas-traits`) crate. The `cas-local` crate provides stub/test implementations: an in-memory `HashMap`-backed `BlockStore`, a fixed-size `Chunker` stub, a `Blake3Hasher`, and an in-memory `DedupIndex` with a serializable bloom filter front-end. Unit tests exercise all five success criteria against the stub implementations.

The critical design decision is to make all traits sync (matching fuser's thread-per-request model and the owner's existing sync algorithms), use buffered I/O (`&[u8]` / `Vec<u8>`) rather than streaming, and ensure the `DedupIndex` trait explicitly separates the bloom-filter fast path from the on-disk index lookup — both of which the owner's real algorithm will satisfy when it is integrated.

**Primary recommendation:** Design traits first in `dedupfs-traits`. Implement in-memory stubs in `cas-local`. Write test harness that proves all five success criteria. Do not expose any implementation detail of storage layout or chunking strategy through the trait interface — the owner's algorithms define those.

---

## Standard Stack

### Core (Phase 1 specific)

| Library | Version | Purpose | Why Standard |
|---------|---------|---------|--------------|
| `thiserror` | 2.0.18 | Typed domain errors per crate | Zero-cost derive macro for `std::error::Error`; 609M downloads; error types stay out of public API |
| `blake3` | 1.8.x | Stub `ContentHasher` implementation | 80M+ downloads; fastest cryptographic hash; SIMD-accelerated; designed for CAS; pluggable behind trait |
| `fastbloom` | 0.14.1 | Bloom filter pre-filter for `DedupIndex` | Fastest Bloom filter in Rust; concurrent support; compatible with any hasher; 2-20x faster than alternatives |
| `proptest` | 1.x | Property-based unit tests | Randomized input generation; finds edge cases in round-trip correctness; used across all CAS trait tests |
| `tempfile` | 3.x | Temporary directories for LocalDiskStore tests | Auto-cleanup; cross-platform; standard for filesystem integration tests |

### Supporting (available but Phase 1 uses minimally)

| Library | Version | Purpose | When to Use |
|---------|---------|---------|-------------|
| `anyhow` | 1.x | Application-level error aggregation | Binary crates / test harness that don't need typed errors |
| `criterion` | 0.5.x | Micro-benchmarks | Hash throughput, chunk throughput baselines — write in Phase 1, run for verification |
| `serde` + `bincode` | serde 1.x, bincode 2.x | Serialize bloom filter state to disk | Needed to persist bloom filter across process restarts; hold off on full use until disk-backed index phase |
| `tracing` | 0.1.x | Structured logging | Wire up in stubs so integration picks it up automatically |

### Alternatives Considered

| Instead of | Could Use | Tradeoff |
|------------|-----------|----------|
| `fastbloom` 0.14.1 | `bloomfilter` crate | bloomfilter is older, slower, less accurate; fastbloom preferred |
| `fastbloom` 0.14.1 | `growable-bloom-filter` | growable-bloom uses more memory for the growth mechanism; standard fixed-size bloom filter is correct for Phase 1 |
| `blake3` stub hasher | `sha2` stub hasher | BLAKE3 is faster and matches what the real workload needs; sha2 is available as alternate impl |
| Fixed stub `Chunker` | `fastcdc` 3.2.1 | fastcdc would be the default chunker if owner's algorithm were not being integrated separately; use fixed-size stub only for Phase 1 |

**Installation (Phase 1 workspace):**
```toml
# Cargo.toml (workspace root)
[workspace]
members = [
    "crates/dedupfs-traits",   # All CAS trait definitions; zero deps except std
    "crates/cas-local",        # Stub/test implementations (HashMap BlockStore, etc.)
]
resolver = "2"

[workspace.dependencies]
thiserror  = "2"
blake3     = "1"
fastbloom  = "0.14"
proptest   = "1"
tempfile   = "3"
serde      = { version = "1", features = ["derive"] }
bincode    = "2"
tracing    = "0.1"
criterion  = { version = "0.5", features = ["html_reports"] }

# Not used in Phase 1 but wire up workspace entry now for later phases:
fuser      = "0.17"
redb       = "3.1"
fastcdc    = "3.2"
tokio      = { version = "1", features = ["full"] }
libc       = "0.2"
anyhow     = "1"
```

---

## Architecture Patterns

### Recommended Crate Structure

```
crates/
├── dedupfs-traits/            # The trait contract crate (Phase 1 primary output)
│   ├── Cargo.toml             # Deps: thiserror only (keep dependency-free)
│   └── src/
│       ├── lib.rs             # Re-exports all public traits and types
│       ├── hash.rs            # ContentHasher trait, ChunkHash newtype
│       ├── chunk.rs           # Chunker trait, Chunk struct
│       ├── block_store.rs     # BlockStore trait, BlockStoreConfig
│       ├── dedup_index.rs     # DedupIndex trait (bloom + on-disk abstraction)
│       └── error.rs           # CasError enum (thiserror)
│
├── cas-local/                 # Stub/test implementations (Phase 1 secondary output)
│   ├── Cargo.toml             # Deps: dedupfs-traits, blake3, fastbloom, thiserror
│   └── src/
│       ├── lib.rs
│       ├── blake3_hasher.rs   # Blake3Hasher: implements ContentHasher
│       ├── fixed_chunker.rs   # FixedChunker: implements Chunker (fixed block size)
│       ├── mem_block_store.rs # MemBlockStore: HashMap<ChunkHash, Vec<u8>> + integrity check
│       ├── disk_block_store.rs# LocalDiskStore: flat files keyed by hash path (2-byte sharding)
│       └── mem_dedup_index.rs # MemDedupIndex: bloom filter + HashMap<ChunkHash, ()>
│
# Later phases add crates:
# crates/dedupfs-meta/         # Metadata engine (Phase 2)
# crates/dedupfs-fuse/         # FUSE integration (Phase 3)
# crates/dedupfs-cli/          # CLI binary (Phase 3)
# crates/dedupfs-owner-adapter/# Owner's algorithm adapters (dedicated adapter phase)
```

### Pattern 1: Trait Definition in Zero-Dependency Crate

**What:** All trait definitions live in `dedupfs-traits` with no external crate dependencies beyond `std` and `thiserror`. Every other crate in the workspace depends on `dedupfs-traits`.

**When to use:** Always — this is the architectural boundary that enables adapter crates for the owner's algorithms to exist independently.

**Why:** Keeping `dedupfs-traits` dependency-free (or nearly so) means the owner's adapter crate can implement the traits without pulling in every library that `cas-local` uses. It also makes the trait contract auditable without noise.

```rust
// Source: Rust API Guidelines + project design
// crates/dedupfs-traits/src/hash.rs

use std::fmt;
use crate::error::CasError;

/// Opaque content hash. Wraps raw bytes; size is implementation-defined.
/// Equality, hashing, and display are derived or implemented on the newtype.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChunkHash(Vec<u8>);

impl ChunkHash {
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Display for ChunkHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in &self.0 {
            write!(f, "{:02x}", b)?;
        }
        Ok(())
    }
}

/// Pluggable hash function interface (CAS-01).
/// Implementations must be Send + Sync for use in fuser's thread-per-request model.
pub trait ContentHasher: Send + Sync {
    /// Hash a complete block (buffered I/O: caller owns the slice).
    fn hash(&self, data: &[u8]) -> ChunkHash;

    /// Identifier string for this hasher (used in block store metadata, collision detection).
    fn algorithm_id(&self) -> &'static str;
}
```

### Pattern 2: BlockStore Trait with Integrity Check Hook

**What:** The `BlockStore` trait explicitly separates existence check from retrieval, and the local disk implementation re-hashes on read when integrity verification is enabled (CAS-05).

**When to use:** Use this pattern from day one — the `verify_on_read` flag must be in the interface so the owner's adapter can implement the same contract.

```rust
// crates/dedupfs-traits/src/block_store.rs

use crate::{hash::ChunkHash, error::CasError};

#[derive(Debug, Clone)]
pub struct BlockStoreConfig {
    /// Re-hash block on every get() and compare against stored hash. CAS-05.
    pub verify_on_read: bool,
}

impl Default for BlockStoreConfig {
    fn default() -> Self {
        Self { verify_on_read: true }
    }
}

/// Pluggable storage backend trait for CAS blocks (CAS-03).
/// Sync — matches fuser thread-per-request model and owner's existing sync algorithms.
pub trait BlockStore: Send + Sync {
    /// Persist a block keyed by its hash. Idempotent: writing the same hash twice is a no-op.
    fn put(&self, hash: &ChunkHash, data: &[u8]) -> Result<(), CasError>;

    /// Retrieve a block by its hash.
    /// When config.verify_on_read is true, re-hash the bytes and return
    /// CasError::IntegrityFailure if they do not match. (CAS-05)
    fn get(&self, hash: &ChunkHash) -> Result<Vec<u8>, CasError>;

    /// Check existence without reading the block content (used by DedupIndex).
    fn exists(&self, hash: &ChunkHash) -> Result<bool, CasError>;

    /// Remove a block. Only called by GC engine (Phase 5). Idempotent.
    fn delete(&self, hash: &ChunkHash) -> Result<(), CasError>;
}
```

### Pattern 3: DedupIndex with Explicit Bloom Pre-Filter

**What:** The `DedupIndex` trait exposes both `bloom_check` (fast probabilistic path) and `lookup` (authoritative path). The stub implementation keeps both in memory. The owner's on-disk implementation will provide the same interface.

**When to use:** Use this two-method interface from day one (CAS-07). The bloom pre-filter is mandatory — it prevents the ZFS DDT memory explosion pitfall.

```rust
// crates/dedupfs-traits/src/dedup_index.rs

use crate::{hash::ChunkHash, error::CasError};

/// Result of a dedup index lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DedupResult {
    /// Block definitely not present (bloom filter said no). No disk read needed.
    DefinitelyAbsent,
    /// Block is present in the index (confirmed via on-disk lookup).
    Present,
    /// Bloom filter said "maybe present" but on-disk lookup found nothing (false positive).
    Absent,
}

/// On-disk dedup index with bounded memory usage (CAS-07).
/// Provides a bloom filter fast-path before committing to an on-disk index lookup.
pub trait DedupIndex: Send + Sync {
    /// Fast probabilistic check. If Absent, definitely not in index. If MaybePresent,
    /// caller must call `lookup` to confirm. Never returns false negatives.
    fn bloom_check(&self, hash: &ChunkHash) -> bool;

    /// Authoritative on-disk lookup. Returns Present or Absent.
    fn lookup(&self, hash: &ChunkHash) -> Result<DedupResult, CasError>;

    /// Record a new hash in both the bloom filter and the on-disk index.
    fn insert(&self, hash: &ChunkHash) -> Result<(), CasError>;

    /// Remove a hash from the on-disk index. Bloom filter is not updated (false positives
    /// are tolerated; they cause an unnecessary on-disk lookup, not data corruption).
    fn remove(&self, hash: &ChunkHash) -> Result<(), CasError>;
}
```

### Pattern 4: Chunker Trait (Flexible for Owner's Algorithm)

**What:** The `Chunker` trait operates on a complete `&[u8]` buffer (buffered I/O) and returns a `Vec<Chunk>`. This matches fuser's byte-slice write model and the owner's existing sync algorithms.

**When to use:** The stub `FixedChunker` divides the buffer into equal-sized chunks. The owner's CDC algorithm will produce variable-length chunks — the trait accommodates both.

```rust
// crates/dedupfs-traits/src/chunk.rs

use crate::error::CasError;

/// A single chunk produced by a Chunker.
#[derive(Debug, Clone)]
pub struct Chunk {
    /// Byte offset of this chunk within the original buffer.
    pub offset: usize,
    /// The chunk data.
    pub data: Vec<u8>,
}

/// Pluggable chunking/block-splitting interface (CAS-02).
/// Takes a complete buffer and returns the ordered list of chunks.
/// Sync — matches fuser model and owner's existing algorithms.
pub trait Chunker: Send + Sync {
    /// Split `data` into an ordered list of chunks.
    /// Implementations MUST be deterministic: same input always produces same chunks.
    fn chunk(&self, data: &[u8]) -> Result<Vec<Chunk>, CasError>;

    /// Human-readable identifier for this chunking strategy (e.g., "fixed-4096", "cdc-fastcdc").
    fn strategy_id(&self) -> &'static str;
}
```

### Pattern 5: Local Disk Block Store with 2-Byte Directory Sharding

**What:** `LocalDiskStore` stores blocks as flat files at `<root>/<xx>/<rest-of-hash>` where `xx` is the first 2 hex characters of the hash. This matches the layout used by casync, Borg, and git object stores.

**When to use:** This is the Phase 1 production-ready local implementation. It satisfies CAS-01 and CAS-03.

```rust
// crates/cas-local/src/disk_block_store.rs (sketch)
// Source: casync design, git object store convention

// Hash "abcdef..." → stored at root/ab/cdef.../
// 256 subdirs max; each holds average (total_blocks / 256) files.
// No index needed for existence check — use filesystem stat().
fn hash_to_path(root: &Path, hash: &ChunkHash) -> PathBuf {
    let hex = hash.to_string();
    let (prefix, rest) = hex.split_at(2);
    root.join(prefix).join(rest)
}
```

### Anti-Patterns to Avoid

- **Putting implementation detail in traits:** The `BlockStore` trait must not reference file paths, directory layout, or storage format. Those are implementation details of `LocalDiskStore` and the owner's adapter.
- **Making traits async in Phase 1:** fuser is sync. Owner's algorithms are sync. Adding `async fn` now forces the owner's adapter to deal with async machinery it was not built for. Leave async for v2 distributed backends.
- **One big crate:** Do not put traits and implementations in the same crate. The owner's adapter crate must be able to implement `ContentHasher` without depending on `blake3`.
- **Fixed ChunkHash size at compile time:** `ChunkHash(Vec<u8>)` accommodates any hash size. A `[u8; 32]` newtype would break when the owner's algorithm uses a different output width.
- **Missing `algorithm_id()` on ContentHasher:** The stored hash must be interpretable without the hasher present. The algorithm ID allows integrity verification to select the correct hasher on read.

---

## Don't Hand-Roll

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|-------------|-----|
| Bloom filter | Custom bit-array probabilistic set | `fastbloom` 0.14.1 | Correct false-positive math, thread-safe, serializable; bloom filter edge cases are subtle |
| Typed error propagation | Manual `Box<dyn Error>` or custom error enums with manual impls | `thiserror` 2.0 | Zero-cost derive; source chaining; Display auto-derive; no boilerplate |
| Cryptographic hashing | Hand-rolled hash | `blake3` 1.8.x | SIMD acceleration; constant-time; security-reviewed; correct padding/finalization |
| Hex encoding for hash paths | Manual `format!("{:02x}", b)` in hot path | `hex` crate or `ChunkHash::Display` impl | Correctness (leading-zero preservation); no allocation surprise |
| Temporary test directories | Manual `std::fs::create_dir_all` + cleanup | `tempfile::TempDir` | Guaranteed cleanup on drop; works on all platforms; prevents test pollution |
| Property-based tests | Exhaustive case enumeration | `proptest` 1.x | Finds boundary cases automatically; shrinks failing inputs; much better coverage |

**Key insight:** The traits themselves are the hand-rolled work — every *implementation* of those traits should reach for a crate rather than a custom solution.

---

## Common Pitfalls

### Pitfall 1: ChunkHash Equality Bug in Stub DedupIndex

**What goes wrong:** `MemDedupIndex` uses `HashMap<ChunkHash, ()>`. If `ChunkHash` derives `Hash` incorrectly (e.g., hashing only the pointer, not the content), two identical hashes will not be found as duplicates.

**Why it happens:** `Vec<u8>` derives `Hash` correctly in Rust, so as long as `ChunkHash` derives `Hash` rather than implementing it manually, this is safe. The bug occurs when developers try to optimize and implement `Hash` manually.

**How to avoid:** Derive `Hash` on `ChunkHash(Vec<u8>)`. Test: write block A, write block A again, verify DedupIndex returns `Present` on the second write. This is a mandatory unit test.

**Warning signs:** `DedupIndex::lookup` returns `Absent` for a hash that was just inserted.

---

### Pitfall 2: Missing Integrity Check Path (CAS-05)

**What goes wrong:** `LocalDiskStore::get` reads bytes from disk but does not re-hash them. CAS-05 requires that a corrupted block is detected on read.

**Why it happens:** Developers store the block and assume the filesystem guarantees integrity. FUSE filesystems are often used on top of unreliable or network-backed storage where silent corruption is possible.

**How to avoid:** Always re-hash in `get()` when `config.verify_on_read == true`. Return `CasError::IntegrityFailure { expected: hash.clone(), actual: computed_hash }`. Test by writing a block, manually corrupting the on-disk file, and asserting `get()` returns an error.

**Warning signs:** `LocalDiskStore::get` reads bytes and returns them without any hash comparison.

---

### Pitfall 3: Bloom Filter Not Persisted

**What goes wrong:** `MemDedupIndex` stores the bloom filter only in memory. After process restart, the bloom filter is empty, causing every lookup to miss the bloom pre-filter and hit the on-disk index directly — or worse, causing double-writes of already-stored blocks if the on-disk index is also in-memory.

**Why it happens:** Phase 1 is "stub implementations only," which tempts developers to skip persistence entirely.

**How to avoid:** The stub `MemDedupIndex` is acceptable as memory-only for unit tests. However, `LocalDiskStore` combined with `MemDedupIndex` in any integration test must document that the dedup index is not crash-persistent in Phase 1. The `DedupIndex` trait must include a comment noting that production implementations MUST persist state. The bloom filter from `fastbloom` supports serialization via `serde` — wire this up as part of the Phase 1 stub.

**Warning signs:** Integration test writes 10 blocks, restarts process, writes same 10 blocks — does the second run detect all as duplicates? If not, the index is not persisted.

---

### Pitfall 4: Chunker Returns Empty Vec on Zero-Length Input

**What goes wrong:** `FixedChunker::chunk(&[])` returns `vec![]`. Upstream code that assumes `chunk().len() >= 1` panics or produces a corrupted empty file manifest.

**Why it happens:** Edge case not tested. Zero-byte files are valid POSIX files.

**How to avoid:** Define behavior explicitly: an empty input returns an empty chunk list. The `BlockStore` and `FileManifest` layers must handle files with zero chunks (zero-length files). Unit test: `assert_eq!(chunker.chunk(&[]).unwrap().len(), 0)`.

---

### Pitfall 5: ChunkHash Display Does Not Preserve Leading Zeros

**What goes wrong:** `format!("{:x}", byte)` for byte value `0x0a` produces `"a"` not `"0a"`. File path `ab/cdef...` becomes `ab/cdef...` for hash `0xabcdef` but `b/cdef...` for hash `0x0bcdef`. On case-insensitive filesystems, directory lookup silently succeeds with wrong data.

**Why it happens:** Developers use `format!("{:x}", b)` instead of `format!("{:02x}", b)` in hex encoding.

**How to avoid:** `ChunkHash::Display` must use `{:02x}` for every byte. Property-based test: any hash with a leading-zero byte must round-trip through `to_string()` → path → `from_hex()` correctly.

---

### Pitfall 6: Blocking I/O in FUSE Callbacks (Future Phase Concern — Design Now)

**What goes wrong:** `LocalDiskStore::get` does blocking file I/O. In Phase 1 this is fine — Phase 1 is unit tests only. But if the trait is designed without this consideration, Phase 3 (FUSE integration) will face synchronous blocking on the fuser callback thread.

**Why it happens:** Sync traits in Phase 1 are correct but the integration path must be planned.

**How to avoid:** Document on the `BlockStore` trait: "All implementations use synchronous I/O. FUSE integration (Phase 3+) must wrap calls in `tokio::task::spawn_blocking`. Do not add `async fn` to this trait until v2 distributed backend milestone." This note prevents future developers from inadvertently mixing sync and async at the trait level.

---

## Code Examples

### ContentHasher — Blake3 Stub Implementation

```rust
// crates/cas-local/src/blake3_hasher.rs
use blake3::Hasher;
use dedupfs_traits::{hash::{ChunkHash, ContentHasher}, error::CasError};

pub struct Blake3Hasher;

impl ContentHasher for Blake3Hasher {
    fn hash(&self, data: &[u8]) -> ChunkHash {
        let hash = blake3::hash(data);
        ChunkHash::from_bytes(hash.as_bytes().to_vec())
    }

    fn algorithm_id(&self) -> &'static str {
        "blake3"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_input_produces_same_hash() {
        let h = Blake3Hasher;
        assert_eq!(h.hash(b"hello"), h.hash(b"hello"));
    }

    #[test]
    fn different_input_produces_different_hash() {
        let h = Blake3Hasher;
        assert_ne!(h.hash(b"hello"), h.hash(b"world"));
    }
}
```

### MemBlockStore — In-Memory Stub with Integrity Check

```rust
// crates/cas-local/src/mem_block_store.rs
use std::collections::HashMap;
use std::sync::RwLock;
use dedupfs_traits::{
    hash::{ChunkHash, ContentHasher},
    block_store::{BlockStore, BlockStoreConfig},
    error::CasError,
};

pub struct MemBlockStore {
    store: RwLock<HashMap<ChunkHash, Vec<u8>>>,
    config: BlockStoreConfig,
    hasher: Box<dyn ContentHasher>,
}

impl MemBlockStore {
    pub fn new(config: BlockStoreConfig, hasher: Box<dyn ContentHasher>) -> Self {
        Self { store: RwLock::new(HashMap::new()), config, hasher }
    }
}

impl BlockStore for MemBlockStore {
    fn put(&self, hash: &ChunkHash, data: &[u8]) -> Result<(), CasError> {
        // Verify hash matches data before storing (prevents silent corruption at write time)
        let computed = self.hasher.hash(data);
        if &computed != hash {
            return Err(CasError::IntegrityFailure {
                expected: hash.clone(),
                actual: computed,
            });
        }
        self.store.write().unwrap().entry(hash.clone()).or_insert_with(|| data.to_vec());
        Ok(())
    }

    fn get(&self, hash: &ChunkHash) -> Result<Vec<u8>, CasError> {
        let store = self.store.read().unwrap();
        let data = store.get(hash).ok_or_else(|| CasError::NotFound(hash.clone()))?.clone();
        if self.config.verify_on_read {
            let computed = self.hasher.hash(&data);
            if &computed != hash {
                return Err(CasError::IntegrityFailure {
                    expected: hash.clone(),
                    actual: computed,
                });
            }
        }
        Ok(data)
    }

    fn exists(&self, hash: &ChunkHash) -> Result<bool, CasError> {
        Ok(self.store.read().unwrap().contains_key(hash))
    }

    fn delete(&self, hash: &ChunkHash) -> Result<(), CasError> {
        self.store.write().unwrap().remove(hash);
        Ok(())
    }
}
```

### LocalDiskStore — Path Sharding

```rust
// crates/cas-local/src/disk_block_store.rs (key excerpt)
// Source: casync design, git object store convention

use std::path::{Path, PathBuf};
use dedupfs_traits::hash::ChunkHash;

fn hash_to_path(root: &Path, hash: &ChunkHash) -> PathBuf {
    // "ab" prefix directory + remainder as filename
    // Guarantees: always 2-char prefix (leading zeros preserved via {:02x})
    let hex = hash.to_string(); // uses ChunkHash::Display with {:02x}
    let (prefix, rest) = hex.split_at(2);
    root.join(prefix).join(rest)
}
```

### DedupIndex — Bloom Filter + HashMap Stub

```rust
// crates/cas-local/src/mem_dedup_index.rs (sketch)
use fastbloom::BloomFilter;
use std::sync::RwLock;
use std::collections::HashSet;
use dedupfs_traits::{
    hash::ChunkHash,
    dedup_index::{DedupIndex, DedupResult},
    error::CasError,
};

pub struct MemDedupIndex {
    bloom: RwLock<BloomFilter>,
    present: RwLock<HashSet<ChunkHash>>,
}

impl MemDedupIndex {
    pub fn new(expected_items: usize, false_positive_rate: f64) -> Self {
        Self {
            bloom: RwLock::new(BloomFilter::with_false_pos(false_positive_rate)
                .expected_items(expected_items)),
            present: RwLock::new(HashSet::new()),
        }
    }
}

impl DedupIndex for MemDedupIndex {
    fn bloom_check(&self, hash: &ChunkHash) -> bool {
        self.bloom.read().unwrap().contains(hash.as_bytes())
    }

    fn lookup(&self, hash: &ChunkHash) -> Result<DedupResult, CasError> {
        if !self.bloom_check(hash) {
            return Ok(DedupResult::DefinitelyAbsent);
        }
        if self.present.read().unwrap().contains(hash) {
            Ok(DedupResult::Present)
        } else {
            Ok(DedupResult::Absent) // bloom false positive
        }
    }

    fn insert(&self, hash: &ChunkHash) -> Result<(), CasError> {
        self.bloom.write().unwrap().insert(hash.as_bytes());
        self.present.write().unwrap().insert(hash.clone());
        Ok(())
    }

    fn remove(&self, hash: &ChunkHash) -> Result<(), CasError> {
        self.present.write().unwrap().remove(hash);
        // Bloom filter is not updated — false positives on removed hashes are tolerated
        Ok(())
    }
}
```

### CasError — Typed Error Enum

```rust
// crates/dedupfs-traits/src/error.rs
use thiserror::Error;
use crate::hash::ChunkHash;

#[derive(Debug, Error)]
pub enum CasError {
    #[error("block not found: {0}")]
    NotFound(ChunkHash),

    #[error("integrity failure: expected {expected}, got {actual}")]
    IntegrityFailure { expected: ChunkHash, actual: ChunkHash },

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("chunker error: {0}")]
    Chunker(String),

    #[error("index error: {0}")]
    Index(String),
}
```

---

## State of the Art

| Old Approach | Current Approach | When Changed | Impact |
|--------------|------------------|--------------|--------|
| Single-crate CAS library | Trait crate + implementation crates | Became standard in Rust ecosystem ~2021 | Owner's adapters slot in without modifying trait crate |
| `async fn` in traits required `async-trait` macro | Native async fn in traits (RPITIT, Rust 1.75+) | Rust 1.75 (Dec 2023) | Phase 1 uses sync traits; no concern for Phase 1; note for v2 |
| `sled` as embedded KV for metadata | `redb` 3.x (ACID, stable format) | redb 1.0 release 2023 | sled is pre-1.0 with unstable format — do not use |
| `bincode` 1.x | `bincode` 2.x (incompatible wire format) | bincode 2.0 release 2024 | Start with 2.x; do not mix versions |
| `thiserror` 1.x | `thiserror` 2.x | 2024 | Additive change; 2.x is backward compatible with 1.x usage patterns |

**Deprecated/outdated:**
- `sled` crate: pre-1.0, on-disk format unstable, last release 2021 — use `redb` for any metadata needs
- `fuse-rs` (zargony): archived, unmaintained — use `fuser` (cberner)
- `bincode` 1.x: soundness issues, incompatible wire format with 2.x — use `bincode` 2.x from day one

---

## Open Questions

1. **ChunkHash size from owner's algorithm**
   - What we know: Owner's existing crates may use a fixed-size hash (e.g., `[u8; 32]` for Blake3/SHA256) or a different output size
   - What's unclear: Whether `ChunkHash(Vec<u8>)` introduces unnecessary heap allocation vs. `[u8; N]` generic parameter
   - Recommendation: Use `ChunkHash(Vec<u8>)` for Phase 1 flexibility. After owner's algorithm is reviewed, consider adding a `type Hash = [u8; 32]` associated type to `ContentHasher` for zero-allocation paths. This is a non-breaking addition.

2. **DedupIndex persistence in Phase 1**
   - What we know: Phase 1 is unit tests only; `MemDedupIndex` is memory-only
   - What's unclear: Whether the success criterion "duplicate block write is detected via bloom filter + on-disk index" requires actual disk persistence or just that the trait interface supports it
   - Recommendation: Implement `MemDedupIndex` with a `save_to_path()` / `load_from_path()` method pair (not part of the trait) that serializes bloom filter state via `serde` + `bincode`. The phase success criterion can be satisfied with in-memory detection; on-disk persistence is the owner's adapter's job.

3. **Error type for collision detection**
   - What we know: Pitfall 11 in PITFALLS.md warns about hash collision handling
   - What's unclear: Whether Phase 1 should include a `CasError::HashCollision` variant (distinct from `IntegrityFailure`)
   - Recommendation: Add `CasError::HashCollision { hash: ChunkHash }` to the error enum in Phase 1. The `put()` implementation can verify that stored bytes match when a hash already exists. This prevents silent data corruption from implementation bugs or weak hash selection.

---

## Validation Architecture

### Test Framework

| Property | Value |
|----------|-------|
| Framework | Rust built-in (`cargo test`) + `proptest` 1.x |
| Config file | None needed — standard Rust test infrastructure |
| Quick run command | `cargo test -p dedupfs-traits -p cas-local` |
| Full suite command | `cargo test --workspace` |

### Phase Requirements to Test Map

| Req ID | Behavior | Test Type | Automated Command | File Exists? |
|--------|----------|-----------|-------------------|--------------|
| CAS-01 | Write a block keyed by hash; retrieve same block by same hash | unit | `cargo test -p cas-local block_store::tests` | No — Wave 0 |
| CAS-01 | Alternative ContentHasher (sha2 stub) swappable without other code changes | unit | `cargo test -p cas-local hasher::tests::swap_hasher` | No — Wave 0 |
| CAS-02 | Alternative Chunker (large block stub) swappable without other code changes | unit | `cargo test -p cas-local chunker::tests::swap_chunker` | No — Wave 0 |
| CAS-03 | LocalDiskStore: write block → appears on disk; read block → matches original bytes | unit | `cargo test -p cas-local disk_block_store::tests` | No — Wave 0 |
| CAS-05 | LocalDiskStore: corrupt stored bytes → get() returns CasError::IntegrityFailure | unit | `cargo test -p cas-local disk_block_store::tests::corruption_detected` | No — Wave 0 |
| CAS-07 | Duplicate block write: DedupIndex returns Present before any disk write occurs | unit | `cargo test -p cas-local mem_dedup_index::tests::dedup_prevents_write` | No — Wave 0 |
| CAS-07 | Bloom filter: definitely-absent hash returns DefinitelyAbsent without on-disk lookup | unit | `cargo test -p cas-local mem_dedup_index::tests::bloom_fast_path` | No — Wave 0 |

### Sampling Rate

- **Per task commit:** `cargo test -p dedupfs-traits -p cas-local`
- **Per wave merge:** `cargo test --workspace`
- **Phase gate:** All workspace tests green before `/gsd:verify-work`

### Wave 0 Gaps

- [ ] `crates/dedupfs-traits/src/` — entire crate (traits not yet created)
- [ ] `crates/cas-local/src/` — entire crate (implementations not yet created)
- [ ] `Cargo.toml` (workspace root) — workspace not yet initialized
- [ ] `crates/dedupfs-traits/Cargo.toml`
- [ ] `crates/cas-local/Cargo.toml`
- [ ] All test files under `crates/cas-local/src/*/tests` — covers all REQ IDs above
- [ ] Framework install: `cargo add` is built-in; no additional tooling install needed
- [ ] `cargo nextest` install optional: `cargo install cargo-nextest` for faster test runs

---

## Sources

### Primary (HIGH confidence)
- Project ARCHITECTURE.md — CAS trait patterns, system design, build order
- Project STACK.md — library versions, alternatives considered, what not to use
- Project PITFALLS.md — pitfalls 1, 3, 5, 11, 12 directly apply to Phase 1
- [Rust API Guidelines — Naming](https://rust-lang.github.io/api-guidelines/naming.html) — trait naming, associated type conventions
- [thiserror 2.0.18 — docs.rs](https://docs.rs/thiserror/latest/thiserror/) — current version confirmed

### Secondary (MEDIUM confidence)
- [fastbloom GitHub](https://github.com/tomtomwombat/fastbloom) — version 0.14.1, thread-safe, fastest Bloom filter in Rust — verified via WebSearch cross-referenced with crates.io
- [blake3 crates.io](https://crates.io/crates/blake3) — version 1.8.x, 80M downloads — per STACK.md (prior research)
- [proptest lib.rs](https://lib.rs/crates/proptest) — version 1.1.3 confirmed active in 2025 — WebSearch verified

### Tertiary (LOW confidence)
- WebSearch: "fastbloom 0.14.1 is the latest version" — not directly verified via docs.rs; treat as MEDIUM pending confirmation during implementation

---

## Metadata

**Confidence breakdown:**
- Standard stack: HIGH — libraries confirmed from STACK.md prior research plus current WebSearch verification
- Architecture: HIGH — trait design patterns are well-established Rust idiom; confirmed against project ARCHITECTURE.md
- Pitfalls: HIGH — sourced from PITFALLS.md which cites USENIX FAST papers and real post-mortems; Phase 1-specific pitfalls independently verified

**Research date:** 2026-03-27
**Valid until:** 2026-04-27 (stable Rust ecosystem; 30-day refresh)
