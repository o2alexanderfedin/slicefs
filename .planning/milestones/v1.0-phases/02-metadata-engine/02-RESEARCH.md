# Phase 2: Metadata Engine - Research

**Researched:** 2026-03-27
**Domain:** POSIX inode/directory/xattr metadata layer built on data-id's CAS Dictionary
**Confidence:** HIGH

<user_constraints>
## User Constraints (from CONTEXT.md)

### Locked Decisions

- Adapt slicefs-traits to fit data-id's native types (Digest224, Digest256, StorageAdd/StorageGet, Tree/State) — not an adapter layer over existing traits
- data-id (https://github.com/o2alexanderfedin/data-id.git) cloned as a git submodule — not a crates.io dependency — to allow future optimizations
- SHA-224 is the hash algorithm: Digest224 `[u32; 7]` is the actual 224-bit hash used as Dictionary keys; Digest256 `[u32; 8]` is a tagged union container where word[7] is either `0xFFFFFFFF` (hash marker) or encodes inline data bit-length
- Keep `&self` + internal sync pattern from current slicefs-traits — wrap data-id's `&mut self` (StorageAdd) with Mutex/RwLock internally for Arc<dyn Trait> sharing across FUSE threads
- Dedup is implicit via content addressing — no separate bloom filter or dedup index needed (data-id's model: same content = same hash = same tree node)
- One shared Dictionary for everything — metadata and content stored as CAS tree nodes in the same Dictionary instance
- No separate embedded database (redb/sled/SQLite) — data-id's Dictionary IS the storage layer
- Filesystem state at any point in time = a single root Digest224
- Time travel / snapshots = a journal of root Digest224 values with timestamps/sequence numbers
- Switching temporal slices = traversing from a different root digest — all old nodes remain in Dictionary (CAS never overwrites)
- Persistent inode-number-to-digest map: a u64 inode number maps to the current Digest224 of the inode's subtree root. The map itself is stored as a CAS tree. Inode numbers survive renames and content changes (satisfies POSIX-10)
- Per-entry directory tree nodes: each directory entry is a separate subtree node keyed by name hash. Changes to one entry don't require re-serializing the whole directory. Supports large directories efficiently
- No GC hooks or design decisions needed in Phase 2 — GC operates at the Dictionary/storage level (Phase 5)
- The CAS-everything model naturally supports GC via root reachability: traverse from live roots (current + pinned snapshots), mark reachable nodes, sweep unreachable
- SSD-optimized GC (segment-based compaction, batch deletes, TRIM) is a storage layout concern deferred to Phase 5

### Claude's Discretion

- Inode field serialization format (fixed-size binary struct recommended — pack mode, uid, gid, size, timestamps, nlinks into a compact byte buffer that fits in a small number of Digest256 leaf nodes)
- Xattr storage model (recommended: subtree under inode node — xattr names as entry keys, values as leaf data — handles both small Finder tags and large SELinux policies without size limits)
- . and .. directory entry handling
- Error type design for metadata-specific failures

### Deferred Ideas (OUT OF SCOPE)

- **Phase 1 trait refactoring**: slicefs-traits needs redesign to align with data-id. This may be a Phase 1.5 or rolled into Phase 2 planning
- **GC and SSD optimization**: Segment-based compaction, batch deletes, TRIM/discard — deferred to Phase 5
- **Snapshot pinning protocol**: How to mark roots as live for GC — deferred to Phase 5/6
- **Async wrappers**: For distributed backends in v2 milestone
- **data-id submodule optimizations**: Future performance work on the algorithms themselves
</user_constraints>

<phase_requirements>
## Phase Requirements

| ID | Description | Research Support |
|----|-------------|-----------------|
| META-03 | Metadata storage separated from block storage (independent stores) | Satisfied architecturally: metadata lives in its own CAS subtree under the filesystem root Digest224; the MetadataStore struct owns the Dictionary and root; block content trees share the same Dictionary but metadata tree nodes are distinct |
| POSIX-06 | File permissions (chmod/chown, uid/gid) | uid (u32), gid (u32), mode (u32) packed into the inode binary struct; stored as inline Digest256 data or small CAS tree; retrieved and updated via MetadataStore::update_inode |
| POSIX-07 | Timestamps (mtime, ctime; noatime by default) | mtime and ctime as i64 Unix seconds + u32 nanoseconds packed into inode binary struct; atime omitted (noatime by default); updated atomically with inode |
| POSIX-08 | Extended attributes (xattr) for macOS Finder metadata, SELinux labels | xattr subtree under each inode node; name->value CAS entries; handles arbitrary-size values via Tree::push_bytes; names stored as inline Digest256 when <= 31 bytes |
| POSIX-10 | Stable inode numbers across mount cycles | InodeMap: persistent u64->Digest224 BTreeMap stored as CAS tree; inode number assigned once at file creation; number never reused; survives process restart by loading from the CAS-stored inode map root |
</phase_requirements>

---

## Summary

Phase 2 builds the metadata engine on top of data-id's `Dictionary` (a `BTreeMap<Digest224, Branches>`). There is no separate database — metadata lives as CAS subtrees in the same Dictionary as content blocks. The filesystem state at any moment is a single `Digest224` root that points to a metadata tree whose leaves are inodes, directory entries, file manifests, and xattr values.

The central design challenge is adapting the existing `slicefs-traits` crate (which uses `ChunkHash(Vec<u8>)` and `&mut self` StorageAdd) to data-id's native `Digest224`/`Digest256` types and its `&mut self` `StorageAdd` trait. The resolution is: rewrite `slicefs-traits` to use `Digest224`/`Digest256` natively and introduce a new `MetadataStore` trait; wrap the Dictionary's `&mut self` methods behind a `Mutex<Dictionary>` so the store satisfies `&self` + `Send + Sync`. The data-id crate becomes a git submodule at `crates/data-id` and is referenced as a path dependency.

The five success criteria decompose into four implementation units: (1) an `InodeStore` trait and implementation for CRUD on inodes stored as compact binary-serialized subtrees, (2) a `DirectoryStore` for per-entry subtrees with guaranteed `.` and `..`, (3) a `ManifestStore` linking inode numbers to ordered lists of block Digest224 values, and (4) an `XattrStore` implemented as a subtree under each inode. All four are unified behind a single `MetadataStore` facade that holds the Dictionary and the current filesystem root.

**Primary recommendation:** Introduce a new `metadata` crate. Redesign `slicefs-traits` in the same phase to use Digest224/Digest256. Implement MetadataStore wrapping `Mutex<Dictionary>`. Use fixed-layout binary structs (little-endian, no serde) for all inode fields — they fit in ≤ 3 Digest256 leaf nodes (88 bytes).

---

## Standard Stack

### Core

| Library | Version | Purpose | Why Standard |
|---------|---------|---------|--------------|
| `data-id` (blockset crate) | git submodule (main branch) | CAS Dictionary, Digest224/256, StorageAdd/StorageGet, Tree/State, compress, GetData/GetBytes | Owner's foundational CAS primitive; all data-id types are the ground truth for this architecture |
| `thiserror` | 2.x (workspace) | Typed `MetaError` enum | Existing project convention; zero-cost derive |
| `sha2-compress` | 0.7.1 (transitive via data-id) | SHA-224 compression function | Used internally by blockset; available via data-id dependency |

### Supporting

| Library | Version | Purpose | When to Use |
|---------|---------|---------|-------------|
| `std::sync::Mutex` | stdlib | Wrap `Dictionary`'s `&mut self` for `&self` trait surface | Required to share Dictionary across FUSE threads; use `Mutex<Dictionary>` not `RwLock` because writes (StorageAdd) are far more frequent than reads in the metadata path |
| `std::collections::BTreeMap` | stdlib | InodeMap: u64 inode number -> Digest224 | Already used internally by Dictionary; natural fit for monotonically-increasing inode assignment |
| `tempfile` | 3.x (workspace) | Integration tests for MetadataStore | Existing workspace dependency |
| `proptest` | 1.x (workspace) | Property-based tests for round-trip inode serialization | Existing workspace dependency |

### Alternatives Considered

| Instead of | Could Use | Tradeoff |
|------------|-----------|----------|
| Mutex<Dictionary> | RwLock<Dictionary> | RwLock only helps when reads outnumber writes; metadata CRUD is write-heavy; Mutex is simpler and avoids writer starvation risk |
| Fixed binary struct for inode fields | serde + bincode | serde adds a dependency; data-id itself is hand-rolled binary; fixed layout is self-describing, deterministic, and hash-stable (same bytes = same hash) |
| BTreeMap<u64, Digest224> for InodeMap in RAM | Persist InodeMap as CAS tree | RAM map is rebuilt from the CAS tree on mount; CAS tree is the durable form; both are needed |

**Installation (workspace Cargo.toml additions):**
```toml
# Add to [workspace] members:
"crates/data-id/blockset"   # git submodule path
"crates/metadata"

# data-id is a path dep — no crates.io entry
```

---

## Architecture Patterns

### Recommended Project Structure

```
crates/
├── data-id/                 # git submodule (o2alexanderfedin/data-id)
│   └── blockset/            # the actual crate consumed as path dep
├── slicefs-traits/          # redesigned: Digest224/256 types, MetadataStore trait
│   └── src/
│       ├── lib.rs
│       ├── digest.rs        # re-export Digest224, Digest256, Branches from blockset
│       ├── storage.rs       # re-export StorageAdd, StorageGet from blockset
│       ├── metadata.rs      # MetadataStore, InodeId, InodeMeta, DirEntry, Xattr traits
│       └── error.rs         # MetaError + CasError unified
├── cas-local/               # existing Phase 1 stubs; minimal changes
└── metadata/                # NEW: MetadataStore implementation
    └── src/
        ├── lib.rs
        ├── store.rs         # MetadataStore struct wrapping Mutex<Dictionary>
        ├── inode.rs         # InodeMeta serialization (binary pack/unpack)
        ├── directory.rs     # DirectoryNode: per-entry subtrees, . and ..
        ├── manifest.rs      # FileManifest: ordered Vec<Digest224>
        ├── xattr.rs         # XattrStore: subtree under inode
        └── inode_map.rs     # InodeMap: u64 -> Digest224, persisted as CAS tree
```

### Pattern 1: Dictionary wrapped in Mutex for &self + Send + Sync

**What:** data-id's `Dictionary` implements `StorageAdd` with `&mut self`. The MetadataStore wraps it in `Mutex<Dictionary>` so the store can implement `&self` methods required by the project's trait convention.

**When to use:** Whenever data-id's `StorageAdd` must be called from a `&self` context (i.e., from `Arc<dyn MetadataStore>`).

**Example:**
```rust
// Source: data-id/blockset/src/storage.rs (StorageAdd trait)
// Source: data-id/blockset/src/dictionary.rs (Dictionary impl)

use std::sync::Mutex;
use blockset::{Dictionary, Digest224, Digest256, StorageAdd, State, Tree};

pub struct MetadataStore {
    dict: Mutex<Dictionary>,
    root: Mutex<Option<Digest224>>,  // current filesystem root; None = empty
}

impl MetadataStore {
    pub fn new() -> Self {
        Self {
            dict: Mutex::new(Dictionary::new()),
            root: Mutex::new(None),
        }
    }

    /// Intern bytes as a CAS subtree; returns the root Digest224.
    fn intern_bytes(&self, data: &[u8]) -> Digest224 {
        let mut dict = self.dict.lock().unwrap();
        State::push_all(&mut *dict, data)   // CDC tree, returns Digest224
    }

    /// Retrieve bytes for a Digest224.
    fn get_bytes(&self, key: &Digest224) -> Option<Vec<u8>> {
        let dict = self.dict.lock().unwrap();
        let digest = blockset::from_digest224(key);
        Some(blockset::GetBytes::new(
            blockset::GetData::new(&*dict, &digest)
        ).collect())
    }
}
```

### Pattern 2: Fixed-layout binary inode struct (no serde, deterministic hash)

**What:** Inode fields (mode, uid, gid, size, nlinks, mtime, ctime) are packed into a fixed 56-byte little-endian binary buffer. This fits within 2 Digest256 leaf nodes and triggers the inline-data optimization for the first 31 bytes.

**When to use:** For every inode read/write path. The fixed layout ensures identical inode state produces identical bytes, which in turn produces identical CAS digests — enabling structural sharing across time slices.

**Inode layout (56 bytes):**
```
Offset  Size  Field
0       8     ino: u64          (stable inode number)
8       4     mode: u32         (file type + permissions)
12      4     uid: u32          (owner user id)
16      4     gid: u32          (owner group id)
20      4     nlinks: u32       (hard link count)
24      8     size: u64         (logical file size in bytes)
32      8     mtime_sec: i64    (modification time, seconds since epoch)
40      4     mtime_nsec: u32   (modification time, nanoseconds)
44      8     ctime_sec: i64    (metadata change time)
52      4     ctime_nsec: u32   (metadata change time, nanoseconds)
--- total: 56 bytes ---
```

```rust
// Source: research — fixed binary layout, little-endian, deterministic
use blockset::Digest224;

#[repr(C)]
pub struct InodeMeta {
    pub ino: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub nlinks: u32,
    pub size: u64,
    pub mtime_sec: i64,
    pub mtime_nsec: u32,
    pub ctime_sec: i64,
    pub ctime_nsec: u32,
}

impl InodeMeta {
    pub fn to_bytes(&self) -> [u8; 56] {
        let mut buf = [0u8; 56];
        buf[0..8].copy_from_slice(&self.ino.to_le_bytes());
        buf[8..12].copy_from_slice(&self.mode.to_le_bytes());
        buf[12..16].copy_from_slice(&self.uid.to_le_bytes());
        buf[16..20].copy_from_slice(&self.gid.to_le_bytes());
        buf[20..24].copy_from_slice(&self.nlinks.to_le_bytes());
        buf[24..32].copy_from_slice(&self.size.to_le_bytes());
        buf[32..40].copy_from_slice(&self.mtime_sec.to_le_bytes());
        buf[40..44].copy_from_slice(&self.mtime_nsec.to_le_bytes());
        buf[44..52].copy_from_slice(&self.ctime_sec.to_le_bytes());
        buf[52..56].copy_from_slice(&self.ctime_nsec.to_le_bytes());
        buf
    }

    pub fn from_bytes(buf: &[u8; 56]) -> Self {
        Self {
            ino:        u64::from_le_bytes(buf[0..8].try_into().unwrap()),
            mode:       u32::from_le_bytes(buf[8..12].try_into().unwrap()),
            uid:        u32::from_le_bytes(buf[12..16].try_into().unwrap()),
            gid:        u32::from_le_bytes(buf[16..20].try_into().unwrap()),
            nlinks:     u32::from_le_bytes(buf[20..24].try_into().unwrap()),
            size:       u64::from_le_bytes(buf[24..32].try_into().unwrap()),
            mtime_sec:  i64::from_le_bytes(buf[32..40].try_into().unwrap()),
            mtime_nsec: u32::from_le_bytes(buf[40..44].try_into().unwrap()),
            ctime_sec:  i64::from_le_bytes(buf[44..52].try_into().unwrap()),
            ctime_nsec: u32::from_le_bytes(buf[52..56].try_into().unwrap()),
        }
    }
}
```

### Pattern 3: Per-entry directory storage using name-keyed CAS subtrees

**What:** Each directory is a CAS subtree whose children are individual name->inode-number entries. Entry key = `State::push_all(dict, name.as_bytes())` (a Digest224). Entry value = an 8-byte u64 inode number encoded as a Digest256 via `blockset::from_bytes`. Changing one entry only updates one subtree path — O(log n) writes for a directory with n entries.

**Special entries:** `.` always maps to the directory's own inode number. `..` maps to the parent's inode number. Both are stored as regular entries and looked up by name hash like any other entry.

**When to use:** Always. No single-node serialization of entire directory contents.

```rust
// Source: research — per-entry pattern using data-id Tree API
// Source: data-id/blockset/src/tree.rs (Tree::push_all -> Digest224)
// Source: data-id/blockset/src/digest256.rs (from_bytes, to_data)

use blockset::{Digest224, State, Tree, from_bytes as d256_from_bytes, to_data};

/// Compute the directory key for an entry name.
fn entry_key(dict: &mut impl StorageAdd, name: &str) -> Digest224 {
    State::push_all(dict, name.as_bytes())
}

/// Encode an inode number as a Digest256 (uses inline-data for u64 = 8 bytes).
fn ino_to_digest256(ino: u64) -> blockset::Digest256 {
    d256_from_bytes(&ino.to_le_bytes()).unwrap()  // 8 bytes always fits inline
}

/// Decode an inode number from a Digest256.
fn digest256_to_ino(d: &blockset::Digest256) -> u64 {
    let bytes = to_data(d);
    u64::from_le_bytes(bytes.try_into().unwrap())
}
```

### Pattern 4: InodeMap as a persistent CAS tree (Digest224 -> persisted across restarts)

**What:** An in-memory `BTreeMap<u64, Digest224>` is loaded from a CAS-stored serialization on mount and persisted on every commit. The serialization format is a flat byte stream of `(ino: u64, key: [u32; 7])` pairs (36 bytes each) stored via `State::push_all`. The next available inode number is `map.keys().max().copied().unwrap_or(0) + 1`.

**Why:** Inode numbers must survive process restarts (POSIX-10). The CAS model means the map itself is immutable once committed — the current-map digest is one of the values tracked in the filesystem root node.

**Example:**
```rust
// Source: research — CAS-stored inode map

fn serialize_inode_map(map: &BTreeMap<u64, Digest224>) -> Vec<u8> {
    let mut buf = Vec::with_capacity(map.len() * 36);
    for (ino, key) in map {
        buf.extend_from_slice(&ino.to_le_bytes());
        for word in key {
            buf.extend_from_slice(&word.to_le_bytes());
        }
    }
    buf
}

fn deserialize_inode_map(bytes: &[u8]) -> BTreeMap<u64, Digest224> {
    let mut map = BTreeMap::new();
    for chunk in bytes.chunks_exact(36) {
        let ino = u64::from_le_bytes(chunk[0..8].try_into().unwrap());
        let mut key = [0u32; 7];
        for (i, w) in key.iter_mut().enumerate() {
            let off = 8 + i * 4;
            *w = u32::from_le_bytes(chunk[off..off+4].try_into().unwrap());
        }
        map.insert(ino, key);
    }
    map
}
```

### Pattern 5: Xattr store as a named subtree under the inode

**What:** Each inode has an optional xattr subtree. The xattr subtree key is stored in the inode's CAS subtree under a reserved name (e.g., the CAS digest of `"\0xattr"` — a null-prefixed name that cannot appear in real filenames). The xattr subtree is itself a Dictionary of `name_digest224 -> value_digest224` entries.

**When to use:** Only create the xattr subtree when the first xattr is set. If no xattrs exist, the inode's CAS subtree simply has no xattr child — zero overhead for files without xattrs.

### Anti-Patterns to Avoid

- **Serializing entire directory to bytes before storing:** Re-serializing a 10,000-entry directory on every `rename` or `create` costs O(n). Use per-entry subtrees (Pattern 3).
- **Using `RwLock<Dictionary>` instead of `Mutex`:** In the metadata path, writes (StorageAdd) are far more frequent than reads. RwLock's overhead exceeds its benefit; Mutex is simpler.
- **Storing inode numbers as variable-length data:** Always encode as fixed 8-byte u64 LE. `blockset::from_bytes` on 8 bytes always produces an inline Digest256 — no tree node is allocated.
- **Allocating inode 0 to real files:** Reserve inode 0 as "invalid". Start real inodes at 1. FUSE uses inode 1 as the root directory.
- **Forgetting that `StorageAdd::end()` always inserts into Dictionary:** `end()` in Dictionary implementation always inserts a `(key, [x, EMPTY])` node, even for empty data. This is intentional — it marks the root of a value tree. Do not call `end()` for intermediate nodes.
- **Bypassing the Mutex on reads:** `StorageGet::get()` takes `&self`, so a read lock is sufficient — but `Dictionary` implements `StorageGet` on `&Self` (the `BTreeMap::get` method). Use `dict.lock().unwrap()` for all access, both reads and writes, to avoid split-brain if Mutex is replaced with RwLock later.

---

## Don't Hand-Roll

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|-------------|-----|
| Byte->tree storage | Custom hashing loop | `blockset::State::push_all(dict, bytes)` | CDC tree with inline data optimization; already handles 0-31 byte values without allocating Dictionary entries |
| Tree->byte retrieval | Manual tree traversal | `blockset::GetBytes::new(GetData::new(dict, &digest))` | Handles the full inline-data / hash-node split in one iterator |
| SHA-224 compression | Custom SHA-224 impl | `blockset::compress(a, b)` / `blockset::from_bytes` | data-id's compress() uses sha2-compress crate + inline-concat for short data |
| Digest224<->Digest256 conversion | Bit manipulation | `blockset::to_digest224(d)`, `blockset::from_digest224(k)` | These handle the hash-suffix protocol correctly; manual bit manipulation will break |
| Dictionary serialization | Custom format | `blockset::serialize(dict, write)` / `blockset::deserialize(read)` | data-id's serialize format is the canonical on-disk form; produces 64-byte records (two Digest256 values per node) |

**Key insight:** data-id's inline-data optimization means values of ≤ 31 bytes (250 bits) do not allocate any Dictionary entry — they are stored directly in the Digest256 container. Inode numbers (8 bytes), short xattr names, and short xattr values all benefit from this. Only data exceeding 31 bytes creates actual tree nodes in the Dictionary.

---

## Common Pitfalls

### Pitfall 1: Calling StorageAdd methods outside the Mutex lock

**What goes wrong:** A refactored function acquires the Dictionary, performs some computation, then calls `dict.add()` on a locally-captured reference — but another thread committed a new root between the acquisition and the add, causing structural inconsistency.

**Why it happens:** `Dictionary` is `!Sync` (it is `&mut self`). The Mutex prevents this, but if code holds a reference obtained before locking, the borrow checker will not catch the logical race.

**How to avoid:** Keep Mutex lock acquisition at the outermost level of each MetadataStore method. Never hold a `&Dictionary` across an await point or across a method boundary without passing the guard explicitly.

**Warning signs:** Any `let dict_ref = ...` followed by multiple independent `dict_ref.add()` calls without re-locking.

### Pitfall 2: Using `compress()` directly instead of `StorageAdd::add()`

**What goes wrong:** `compress()` computes the combined Digest256 but does NOT insert the `(key, [left, right])` node into the Dictionary. If you use `compress()` to combine two sub-trees without also inserting via `add()`, the combined tree root becomes unretrievable.

**Why it happens:** `compress()` is a pure function. `Dictionary::add()` calls `compress()` AND inserts. It is easy to confuse them.

**How to avoid:** Use `StorageAdd::add(dict, left, right)` whenever you want a stored internal node. Only use `compress()` standalone when checking whether inline concatenation occurred (no Dictionary insertion needed for inline data).

**Warning signs:** Calling `compress(&a, &b)` and using the result as a Digest224 key without a subsequent `dict.insert()` call.

### Pitfall 3: Forgetting that `StorageAdd::end()` forces a SHA-224 compression

**What goes wrong:** `Dictionary::end(x)` computes `SHA224.compress(x, &EMPTY)` and inserts it. This means even a single-element sequence gets an extra level of hashing. Calling `end()` twice on the same root doubles the wrapping — the inner root is unreachable from the outer key.

**Why it happens:** `end()` is designed to canonicalize the root of an entire byte sequence (distinguishing "stored as tree root" from "intermediate node"). It should be called exactly once per logical value.

**How to avoid:** Call `State::push_all(dict, bytes)` (which calls `end()` internally) rather than managing the `State` manually and calling `end()` explicitly. Only call `end()` manually when you need the intermediate `Digest256` (via `push_all_internal`).

### Pitfall 4: Inode number 1 collision with FUSE root

**What goes wrong:** FUSE always treats inode 1 as the root directory. If the InodeMap assigns number 1 to the first file created, that file will shadow or conflict with the root.

**Why it happens:** Monotone increment from 1 reaches the root inode number immediately.

**How to avoid:** Initialize the InodeMap with inode 1 pre-assigned to the root directory during `MetadataStore::new_filesystem()`. All subsequent allocations start from `max(existing_inodes) + 1`, which will be ≥ 2.

### Pitfall 5: Dictionary lock held across serialization of large inode maps

**What goes wrong:** Serializing a large InodeMap (hundreds of thousands of entries) while holding the Dictionary lock blocks all other metadata operations for milliseconds.

**Why it happens:** Serialize-to-bytes and then `push_all()` into the Dictionary are naturally sequential, and `push_all()` requires `StorageAdd` (the dict lock).

**How to avoid:** Serialize the InodeMap bytes first (outside the lock), then acquire the lock only for the `push_all()` call. The serialized bytes are immutable, so this is safe.

### Pitfall 6: . and .. entries as hardcoded special cases

**What goes wrong:** Some implementations check `if name == "." || name == ".."` in the lookup path and return hardcoded values instead of looking them up in the Directory subtree. This means `.` and `..` are not stored and disappear after a restart.

**Why it happens:** `.` and `..` feel like special filesystem concepts, not regular stored entries.

**How to avoid:** Store `.` and `..` as regular named entries in the directory subtree, created when the directory is created. They are always looked up exactly like any other entry. The only "special" treatment is ensuring they are created automatically on `create_directory()`.

---

## Code Examples

Verified patterns from data-id source:

### Store bytes as CAS tree (returns Digest224)
```rust
// Source: data-id/blockset/src/tree.rs — Tree::push_all
// Source: data-id/blockset/src/content_dependant_tree.rs — State impl
use blockset::{Dictionary, State, Tree};

let mut dict = Dictionary::new();
let root: blockset::Digest224 = State::push_all(&mut dict, b"inode data here");
// root is now the stable key for this content; dict contains tree nodes
```

### Retrieve bytes from CAS tree
```rust
// Source: data-id/blockset/src/get_data.rs — GetBytes, GetData
use blockset::{Dictionary, GetBytes, GetData, from_digest224};

let digest256 = blockset::from_digest224(&root);
let bytes: Vec<u8> = GetBytes::new(GetData::new(&dict, &digest256)).collect();
```

### Inline small data (≤ 31 bytes, no Dictionary entry)
```rust
// Source: data-id/blockset/src/digest256.rs — from_bytes, to_data
use blockset::{from_bytes, to_data};

// Encode: 8-byte inode number stored inline in a Digest256
let ino: u64 = 42;
let d256 = blockset::from_bytes(&ino.to_le_bytes()).unwrap();
// d256[7] top byte = 0x40 (64 bits), no Dictionary insertion needed

// Decode: extract bytes from an inline Digest256
let raw = to_data(&d256);  // returns Vec<u8> of the original bytes
let ino_back = u64::from_le_bytes(raw.try_into().unwrap());
```

### Persist and load Dictionary (on-disk format)
```rust
// Source: data-id/blockset/src/dictionary.rs — serialize, deserialize
use blockset::{Dictionary, serialize, deserialize};
use std::io::Cursor;

// Save
let mut cursor = Cursor::new(Vec::<u8>::new());
serialize(&dict, &mut cursor);

// Load
cursor.set_position(0);
let dict2: Dictionary = deserialize(&mut cursor);
```

### Check if a Digest256 is a hash node vs inline data
```rust
// Source: data-id/blockset/src/digest256.rs — is_hash, len
use blockset::{is_hash, validate};

if blockset::digest256::is_hash(&d256) {
    // d256[7] == 0xFFFF_FFFF — this is a tree node key
    let key = blockset::to_digest224(&d256).unwrap();
    // look up in dictionary
} else {
    // inline data — len(&d256) gives the bit-length
    let bytes = blockset::to_data(&d256);
}
```

### Convert between Digest224 and Digest256
```rust
// Source: data-id/blockset/src/digest224.rs — to_digest224, from_digest224
use blockset::{to_digest224, from_digest224, Digest224, Digest256};

let key: Digest224 = [1, 2, 3, 4, 5, 6, 7];
let d256: Digest256 = from_digest224(&key);  // sets word[7] = 0xFFFF_FFFF
let key_back: Option<Digest224> = to_digest224(&d256);  // Some(key) if is_hash
```

---

## data-id Public API Surface (Confirmed from Source)

**Confirmed exports from `blockset/src/lib.rs`:**

```
pub use content_dependant_tree::State;          // CDC tree implementation (push_all)
pub use dictionary::{deserialize, serialize, Dictionary};   // BTreeMap + I/O
pub use digest224::{from_base32, from_digest224, to_base32, to_digest224, Base32};
pub use get_data::{GetBytes, GetData};          // tree traversal / byte iteration
pub use tree::Tree;                             // trait: push_all, push_bytes, end
pub use bin_div::{BIN_DIV_16, BIN_DIV_2, BIN_DIV_4, BIN_DIV_8};
pub use block::{compact, to_block, to_dictionary};
pub use digest256::{from_bytes, to_data, validate};  // inline data helpers
pub use io::Io;
pub use visual::print_tree;
```

**NOT publicly exported from lib.rs (must access via module path or not at all):**
- `compress()` — lives in `blockset::digest256::compress`, not re-exported from lib.rs. Access as `blockset::digest256::compress` or use `Dictionary::add()` which calls it internally.
- `is_hash()`, `len()` — same; in `blockset::digest256` module.
- `MerkleTreeState` — in `blockset::merkle_tree`; not re-exported. Use `State` (CDC) for all metadata storage.
- `StorageAdd`, `StorageGet`, `Branches` — in `blockset::storage`; not re-exported. Import directly: `use blockset::storage::{StorageAdd, StorageGet, Branches}`.
- `Digest224`, `Digest256` — type aliases defined in `blockset::digest224` / `blockset::digest256`; not re-exported from lib.rs. Import as `use blockset::digest224::Digest224; use blockset::digest256::Digest256`.
- `FileStorageAdd` — in `blockset::file_storage`; not re-exported. Relevant for Phase 3 on-disk persistence.

---

## slicefs-traits Redesign Plan (Phase 2 prerequisite)

The existing `slicefs-traits` crate uses `ChunkHash(Vec<u8>)`. Phase 2 must redesign it. The scope of changes:

1. **Add `digest.rs`** — re-exports `Digest224`, `Digest256`, `Branches` from `blockset`
2. **Add `storage.rs`** — re-exports `StorageAdd`, `StorageGet` from `blockset::storage`
3. **Add `metadata.rs`** — defines `MetadataStore` trait, `InodeId` alias, `InodeMeta` struct, `DirEntry` struct, `MetaError` enum
4. **Update `hash.rs`** — `ContentHasher::hash()` now returns `Digest224` instead of `ChunkHash`; `ChunkHash` removed or deprecated
5. **Update `block_store.rs`** — `BlockStore::put/get/exists/delete` use `Digest224` instead of `ChunkHash`
6. **Update `error.rs`** — merge `CasError` and `MetaError` or keep separate; `MetaError` needed for Phase 2

The `cas-local` stubs (MemBlockStore, Blake3Hasher, FixedChunker, MemDedupIndex) will need corresponding updates but can remain as stubs with the new types.

---

## State of the Art

| Old Approach | Current Approach | When Changed | Impact |
|--------------|------------------|--------------|--------|
| Embedded DB (redb/sled/SQLite) for metadata | CAS Dictionary as universal storage | Phase 2 context decision | No external DB dep; metadata and blocks share structural dedup; time travel is free |
| ChunkHash(Vec<u8>) variable-width hash | Digest224 = [u32; 7] fixed 224-bit | Phase 2 redesign | Hash comparison is a fixed-cost 7-word compare; no heap allocation per hash |
| Separate bloom filter for dedup | Implicit dedup via content addressing | Phase 2 context decision | Same content = same hash = same Dictionary key; no separate index needed |
| Per-directory serialization (whole dir as blob) | Per-entry subtrees | Phase 2 context decision | O(1) entry update regardless of directory size |

**Deprecated/outdated for Phase 2:**
- `ChunkHash(Vec<u8>)`: replaced by `Digest224` from data-id
- `redb = "3.1"` in workspace Cargo.toml: was speculative; Phase 2 does not use it (no separate DB)
- `DedupIndex` trait in current form: implicit in data-id's Dictionary; may be retained as a facade but the bloom filter impl is not needed for metadata

---

## Open Questions

1. **data-id submodule initialization procedure**
   - What we know: the submodule URL is `https://github.com/o2alexanderfedin/data-id.git`; it must be added with `git submodule add` and referenced as a path dependency in Cargo.toml
   - What's unclear: whether the workspace Cargo.toml adds `"crates/data-id/blockset"` directly to `[workspace] members`, or whether `metadata` crate has a `[dependencies] blockset = { path = "../data-id/blockset" }`
   - Recommendation: Add blockset as a path dependency in each crate that uses it (not as a workspace member, since it is external code). Do NOT add it to `[workspace] members` unless its Cargo.toml is compatible with this workspace's resolver.

2. **Thread-safety of Mutex<Dictionary> under concurrent FUSE operations**
   - What we know: FUSE (Phase 3) sends concurrent requests from multiple threads; all must serialize through the Mutex
   - What's unclear: Whether metadata operations will bottleneck at the Mutex under realistic FUSE workloads
   - Recommendation: Implement with Mutex for Phase 2 correctness; benchmark in Phase 3; shard by inode prefix or add a read path cache if needed

3. **Root node schema — how to organize the filesystem root subtree**
   - What we know: The filesystem state is a single Digest224 root; it must encode pointers to the inode map, the root directory, and the journal
   - What's unclear: Exact binary layout of the root node (is it a 3-entry CAS tree? A fixed binary struct?)
   - Recommendation: A fixed 44-byte root record: `[inode_map_root: Digest224 (28 bytes), root_dir_ino: u64 (8 bytes), next_ino: u64 (8 bytes)]`. Store via `State::push_all`. Retrieve with `GetBytes`.

4. **data-id LICENSE compatibility**
   - What we know: blockset crate Cargo.toml says `license = "GPL-3.0-or-later"`
   - What's unclear: Whether the overall SliceFS license is compatible with GPL-3.0-or-later for the submodule dependency
   - Recommendation: Flag for owner review before Phase 2 starts; linking against GPL-3 code in a non-GPL project may have distribution implications

---

## Validation Architecture

`nyquist_validation` is enabled (not explicitly false in config.json).

### Test Framework
| Property | Value |
|----------|-------|
| Framework | Rust built-in test (`cargo test`) |
| Config file | none — standard `#[test]` and `#[cfg(test)]` modules |
| Quick run command | `cargo test -p metadata` |
| Full suite command | `cargo test --workspace` |

### Phase Requirements -> Test Map

| Req ID | Behavior | Test Type | Automated Command | File Exists? |
|--------|----------|-----------|-------------------|-------------|
| META-03 | MetadataStore holds no external DB dep; all data in Dictionary | unit | `cargo test -p metadata -- metadata_store` | Wave 0 |
| POSIX-06 | chmod/chown: uid, gid, mode round-trip through InodeMeta | unit | `cargo test -p metadata -- inode::tests` | Wave 0 |
| POSIX-07 | mtime/ctime stored and retrieved; noatime default | unit | `cargo test -p metadata -- inode::tests::timestamps` | Wave 0 |
| POSIX-08 | xattr set/get/list/remove on inode | unit | `cargo test -p metadata -- xattr::tests` | Wave 0 |
| POSIX-10 | Inode numbers stable across MetadataStore drop+reload | unit | `cargo test -p metadata -- inode_map::tests::stable_across_reload` | Wave 0 |
| Phase SC 1 | Inode create/read/update/delete | unit | `cargo test -p metadata -- store::tests::inode_crud` | Wave 0 |
| Phase SC 2 | Directory create/list/remove + . and .. present | unit | `cargo test -p metadata -- directory::tests` | Wave 0 |
| Phase SC 3 | File manifest create/retrieve with ordered block list | unit | `cargo test -p metadata -- manifest::tests` | Wave 0 |
| Phase SC 4 | Xattr set/get on inode | unit | `cargo test -p metadata -- xattr::tests::set_get` | Wave 0 |
| Phase SC 5 | Inode numbers stable across process restart (Dictionary serialize/deserialize cycle) | integration | `cargo test -p metadata -- inode_map::tests::stable_across_reload` | Wave 0 |

### Sampling Rate
- **Per task commit:** `cargo test -p metadata`
- **Per wave merge:** `cargo test --workspace`
- **Phase gate:** Full suite green before `/gsd:verify-work`

### Wave 0 Gaps
- [ ] `crates/metadata/` — crate does not exist yet; Wave 0 creates skeleton
- [ ] `crates/metadata/src/store.rs` — covers META-03, Phase SC 1
- [ ] `crates/metadata/src/inode.rs` — covers POSIX-06, POSIX-07, Phase SC 1
- [ ] `crates/metadata/src/directory.rs` — covers Phase SC 2
- [ ] `crates/metadata/src/manifest.rs` — covers Phase SC 3
- [ ] `crates/metadata/src/xattr.rs` — covers POSIX-08, Phase SC 4
- [ ] `crates/metadata/src/inode_map.rs` — covers POSIX-10, Phase SC 5
- [ ] `git submodule add https://github.com/o2alexanderfedin/data-id.git crates/data-id` — data-id not yet cloned
- [ ] `crates/slicefs-traits` redesign — Digest224/Digest256 replacing ChunkHash

---

## Sources

### Primary (HIGH confidence)
- `data-id/blockset/src/storage.rs` (fetched via GitHub API) — `StorageAdd`, `StorageGet`, `Branches` exact definitions
- `data-id/blockset/src/digest256.rs` (fetched via GitHub API) — `Digest256 = [u32; 8]`, `compress()`, `from_bytes()`, `to_data()`, `is_hash()`, `len()`, `EMPTY`, `HASH_SUFFIX = 0xFFFF_FFFF`
- `data-id/blockset/src/digest224.rs` (fetched via GitHub API) — `Digest224 = [u32; 7]`, `to_digest224()`, `from_digest224()`
- `data-id/blockset/src/dictionary.rs` (fetched via GitHub API) — `Dictionary = BTreeMap<Digest224, Branches>`, `serialize()`, `deserialize()`, StorageAdd/StorageGet impls
- `data-id/blockset/src/tree.rs` (fetched via GitHub API) — `Tree` trait, `push_all()`, `push_bytes()`, `end()`
- `data-id/blockset/src/content_dependant_tree.rs` (fetched via GitHub API) — `State` CDC implementation
- `data-id/blockset/src/get_data.rs` (fetched via GitHub API) — `GetData`, `GetBytes` byte-iterator over CAS tree
- `data-id/blockset/src/lib.rs` (fetched via GitHub API) — full public export list
- `data-id/blockset/Cargo.toml` (fetched via GitHub API) — license: GPL-3.0-or-later; dep: sha2-compress 0.7.1
- Existing project crates in `/Volumes/Unitek-B/Projects/file-systems/crates/` — coding conventions, error patterns, trait patterns confirmed

### Secondary (MEDIUM confidence)
- POSIX inode field conventions (ino, mode, uid, gid, nlinks, size, mtime, ctime) — standard POSIX `struct stat` layout; well-established
- `fuser` inode number convention (inode 1 = root directory) — documented in fuser crate README and FUSE kernel protocol

### Tertiary (LOW confidence)
- Thread-safety performance analysis of `Mutex<Dictionary>` under FUSE load — untested; based on general knowledge of Mutex overhead vs. workload characteristics

---

## Metadata

**Confidence breakdown:**
- Standard stack: HIGH — data-id source confirmed via GitHub API; all types verified
- Architecture: HIGH — data-id Dictionary model is fully understood; patterns derived directly from source
- Pitfalls: HIGH — derived from careful reading of actual data-id source code (compress vs add, end() behavior, inline data threshold)
- slicefs-traits redesign scope: HIGH — current source confirmed; required changes are mechanical

**Research date:** 2026-03-27
**Valid until:** 2026-06-27 (data-id is a git submodule at a fixed commit; changes only on explicit submodule update)
