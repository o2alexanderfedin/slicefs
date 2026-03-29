# Architecture Research

**Domain:** Streaming writes integration — SliceFS v2.0
**Researched:** 2026-03-29
**Confidence:** HIGH (based on direct codebase inspection of all relevant source files)

---

## Scope

This document answers the specific question for the v2.0 milestone: how does streaming
writes (`State::push_bytes`) integrate with the existing FUSE write callback,
flush/release lifecycle, compression removal, and read-after-write consistency? What
changes to `OpenFileState`, `flush_buffer_to_cas`, and the read path?

All source locations are in `crates/slicefs-cli/src/filesystem.rs` unless noted.

---

## System Overview (as-built, v1.0)

```
Kernel VFS
    │ FUSE opcodes
    ▼
SliceFsFilesystem (filesystem.rs)
    │  Implements fuser::Filesystem
    │  open_files: Mutex<HashMap<u64, OpenFileState>>
    │
    ├─── write()  ─────────────────────────────────────────────────────────────┐
    │    appends data into OpenFileState.buf (Vec<u8>)                        │
    │    sets cas_committed = false                                            │
    │                                                                          │
    ├─── flush() / fsync() ────────────────────────────────────────────────── │
    │    calls flush_buffer_for_fsync(ino, fh)                                │
    │      ├── takes buf out of open_files with mem::take                     │
    │      ├── calls to_wire_bytes(buf) → compress_block (if store_version≥2) │
    │      ├── State::push_all(&mut dict, &wire_bytes) → Digest224            │
    │      ├── set_manifest(ino, &[digest]) + increment_refcount              │
    │      └── puts empty buf back; sets cas_committed = true                 │
    │                                                                          │
    ├─── release() ─────────────────────────────────────────────────────────  │
    │    calls test_release(ino, fh)                                           │
    │      ├── if buf.is_empty() && cas_committed → skip (guard)              │
    │      └── else flush_buffer_to_cas(ino, buf)  (same pipeline)            │
    │                                                                          │
    └─── read() ────────────────────────────────────────────────────────────  │
         if fh is a write handle → serve from OpenFileState.buf (RAW)        │
         else: get_manifest → GetData/GetBytes → from_wire_bytes (decompress) │
                                                                               │
DictMetadataStore (metadata/src/store.rs)                                     │
    │  dict: Mutex<Dictionary>                                                 │
    │  manifest_data: Mutex<BTreeMap<u64, Digest224>>                         │
    │  refcounts: Mutex<BTreeMap<Digest224, u64>>                             │
    │                                                                          │
blockset::Dictionary (data-id/blockset/src/dictionary.rs)                     │
    │  BTreeMap<Digest224, Branches>                                           │
    │  StorageAdd: add(left, right) → Digest256                               │
    │              end(x) → Digest224                                          │
    │                                                                          │
blockset::State = Vec<Level> (content_dependant_tree.rs)                      │
    │  Level = (MerkleTreeState, Digest256)                                    │
    │  push_digest() — O(log N) tree insertion                                 │
    │  push_bytes(storage, &[u8]) — feeds bytes one-by-one into push_digest   │
    │  push_all(storage, &[u8]) → Digest224 — full-buffer convenience         │
    │  end(storage) → Digest256 — finalises partial state                     │
```

---

## Current Write Path (v1.0) — What Exists

### Buffer Accumulation

`write()` FUSE callback appends into `OpenFileState.buf: Vec<u8>`. The entire file
content is accumulated in-memory before any CAS interaction. This is the root cause of
the file-size-equals-RAM limitation.

### Flush (CAS commit)

`flush_buffer_to_cas(ino, buf)` and the non-closing variant `flush_buffer_for_fsync`
both execute:

```
raw_buf
  → to_wire_bytes()               # compress_block if store_version >= 2
  → State::push_all(&mut dict, &wire_bytes)  # whole buffer at once → Digest224
  → set_manifest(ino, [digest])
  → increment_refcount(digest)
  → update inode size/mtime
```

`State::push_all` is a convenience wrapper:

```rust
// tree.rs — Tree trait
fn push_all(storage: &mut impl StorageAdd, v: &[u8]) -> Digest224 {
    let mut state = Self::default();
    state.push_bytes(storage, v);  // feeds bytes into incremental State
    let root = state.end(storage); // finalises Merkle tree
    storage.end(&root)             // wraps into Digest224
}
```

So `push_all` already calls `push_bytes` internally. The issue is that `v` is the
entire already-buffered `Vec<u8>` — nothing is streamed.

### Read-After-Write

`read()` inspects `fh`: if it is a write handle (non-zero, present in `open_files`),
data is served directly from `OpenFileState.buf` — raw, uncompressed bytes. This is
correct for v1.0 because the buffer holds the canonical version. If the handle is
read-only (fh == 0) the manifest → dictionary path is taken.

### Compression Involvement

`to_wire_bytes()` / `from_wire_bytes()` gate on `store_version`:

- `< 2`: no-op (raw bytes, pre-v1.0 format)
- `>= 2`: `compress_block` wraps payload with 1-byte `AlgorithmId` header; `decompress_block` strips it

The compressor is stored in `SliceFsFilesystem.compressor: Arc<dyn Compressor>`. A
`NoneCompressor` effectively makes the write path raw already.

---

## What v2.0 Changes

### Goal 1 — Streaming Writes (O(log N) memory)

Instead of buffering the entire file then calling `State::push_all`, maintain a live
`State` (the incremental Merkle tree) in `OpenFileState`. Each `write()` call feeds its
chunk of bytes directly into the tree via `State::push_bytes`. On flush/release the
tree is finalised with `state.end(dict)` then `dict.end(root)`.

### Goal 2 — Remove Compression from Write/Read Path

Remove the `to_wire_bytes` / `from_wire_bytes` calls. Raw bytes go straight into the
Merkle tree. Deduplication operates on raw content, so the same bytes from different
files deduplicate regardless of what compressor was in use at any given time. The
`slicefs-compression` crate and the compressor field on `SliceFsFilesystem` become
unused on the active write path (may be kept for backward-compatible reads of v1.0
blocks, or removed entirely).

---

## Component Changes — New vs Modified

### Modified: `OpenFileState`

**Current:**
```rust
struct OpenFileState {
    ino: u64,
    buf: Vec<u8>,
    cas_committed: bool,
}
```

**v2.0:**
```rust
struct OpenFileState {
    ino: u64,
    state: blockset::State,   // replaces buf: Vec<u8>
    byte_count: u64,          // tracks logical size (inode.size)
    cas_committed: bool,
}
```

`State = Vec<Level>` where `Level = (MerkleTreeState, Digest256)`. It grows
O(log N) in the number of distinct content-defined chunks, not O(N) in bytes.

`byte_count` is needed because the old path derived `inode.size` from `buf.len()`.
With streaming, the buffer no longer exists at flush time — size must be tracked
incrementally as bytes arrive in `write()`.

`cas_committed` semantics are unchanged: set to `true` after a successful
flush, cleared to `false` when new writes arrive.

### Modified: `write()` callback and `test_write()`

**Current:** `buf.resize(end, 0); buf[offset..end].copy_from_slice(data)`

**v2.0:** `state.push_bytes(&mut dict, data); byte_count += data.len() as u64`

The offset parameter becomes irrelevant for append-only streaming. For non-sequential
writes (pwrite at arbitrary offsets) this needs careful handling — see Integration
Considerations below.

### Modified: `flush_buffer_to_cas()` and `flush_buffer_for_fsync()`

**Current:**
```
buf → to_wire_bytes → State::push_all → Digest224
```

**v2.0:**
```
state.end(&mut dict) → Digest256 → dict.end(&root) → Digest224
```

The `State` already holds the partially-built Merkle tree. Finalising it requires only:

```rust
// Acquire dict lock
let mut dict = self.dict.lock().unwrap();
let root256 = state.end(&mut *dict);
let digest = dict.end(&root256);
// release lock
self.meta.set_manifest(ino, &[digest]);
self.meta.increment_refcount(&digest);
```

`inode.size` is set to `byte_count` (not `buf.len()`).

The `to_wire_bytes` call is removed entirely. No `compress_block` on the write path.

### Modified: `read()` callback and `test_read()`

**Current read-after-write:** serves from `state.buf` (the raw Vec<u8>).

**v2.0:** The `State` is not directly readable — it is a write-accumulation structure.
Two options:

1. **Materialise on demand** — when `read()` is called on an open write handle, call
   `state.end()` on a clone of the state to produce a temporary root, then use
   `GetBytes` to reconstruct the bytes. This is correct but has cost proportional to
   file size on every read-during-write.

2. **Shadow buffer** — keep a small shadow `Vec<u8>` in `OpenFileState` for reads
   during the write session, separate from the streaming `State`. This duplicates
   memory but avoids materialise cost for interactive read-after-write workloads.

3. **Flush-then-read** — on `read()` for an open write handle, flush the streaming
   state to the dictionary first, then read from the CAS path. Safe, correct, but
   adds a CAS commit on every read.

**Recommendation:** Option 1 (materialise on demand) is simplest and correct for the
common case (read-after-write is rare in bulk-write workloads). A clone of `State` is
cheap (small Vec) and `GetBytes` reconstruction has cost proportional to file size —
acceptable.

**Read path compression removal:** `from_wire_bytes` is removed for new blocks. For
backward-compatible reads of v1.0 (store_version < 2) blocks, keep the
`store_version`-gated decompress path or migrate old blocks on first read. If backward
compatibility is not required, the decompression path can be deleted entirely.

### Unchanged: `DictMetadataStore`

No changes needed. `set_manifest`, `increment_refcount`, `get_manifest`, `get_inode`,
`update_inode` all have the same signatures. The dict lock is still held only during
the finalise step, not during the streaming push_bytes phase.

### Unchanged: `blockset::State`, `Tree::push_bytes`, `Dictionary`

The incremental API already exists and works correctly. `push_bytes` and `end` are the
two calls that replace the single `push_all` call.

### Unchanged: FUSE lifecycle callbacks (`open`, `flush`, `fsync`, `release`)

The FUSE-level callbacks (`flush`, `fsync`, `release`) delegate to the internal
helpers. Their signatures and semantics are unchanged — they call the same helper
functions, which now operate on `State` instead of `Vec<u8>`.

### Unchanged: `setattr` truncate path

`test_setattr_size` reads from CAS (manifest → GetBytes) to reconstruct content, then
resizes it. With compression removal this path simplifies (no `from_wire_bytes`). The
core CAS-read-then-rewrite logic remains structurally identical.

---

## Data Flow: v2.0 Streaming Write Path

```
Application write(fd, buf, offset)
    ↓
fuser::Filesystem::write()
    ↓
test_write(fh, offset, data)
    acquire open_files lock
    state.push_bytes(&mut dict, data)   ← feeds raw bytes into Merkle tree
    byte_count += data.len()
    cas_committed = false
    return bytes_written
    (dict lock held only during push_bytes call)

Application close(fd) / fsync(fd)
    ↓
flush_buffer_for_fsync(ino, fh)  [or flush_buffer_to_cas for release]
    acquire open_files lock
    take state out of open_files (mem::take equivalent)
    acquire dict lock
    root256 = state.end(&mut dict)      ← finalises content-defined Merkle tree
    digest224 = dict.end(&root256)      ← wraps to addressable Digest224
    release dict lock
    meta.set_manifest(ino, &[digest224])
    meta.increment_refcount(&digest224)
    meta.update_inode(ino, size=byte_count, mtime=now)
    put empty State back; cas_committed = true
```

### Key difference from v1.0

In v1.0, the dict lock was held for the entire `push_all` call (which includes
`push_bytes` over all bytes). In v2.0, the dict lock is held during each `push_bytes`
call in `write()` AND during the finalise in flush. This is the same locking granularity
per-call — the semantic difference is that the CAS work is distributed across all
`write()` calls rather than deferred to flush.

**Locking implication:** The dict lock in `SliceFsFilesystem` is separate from the dict
lock inside `DictMetadataStore`. The filesystem holds `self.dict: Arc<Mutex<Dictionary>>`
as a clone for content reads. The store has its own `dict: Mutex<Dictionary>`. These
two must remain consistent — write path pushes into `self.dict` (the filesystem's
copy), and at commit time `set_manifest` records the digest against the store's copy.
This is existing v1.0 behaviour; streaming does not change this boundary.

---

## Data Flow: v2.0 Read Path

```
Application read(fd, offset, size)
    ↓
fuser::Filesystem::read()
    ↓
(if fh is a write handle)
    acquire open_files lock
    clone current state   ← cheap: State = Vec<Level>, small
    acquire dict lock
    root256 = cloned_state.end(&mut dict)
    release dict lock
    GetBytes::new(GetData::new(&dict, &root256)).collect() → Vec<u8>
    return raw_bytes[offset..offset+size]

(if fh == 0, read-only handle)
    meta.get_manifest(ino) → [digest224]
    from_digest224(digest224) → root256
    acquire dict lock
    GetBytes::new(GetData::new(&dict, &root256)).collect() → Vec<u8>
    release dict lock
    return raw_bytes[offset..offset+size]   ← no decompression (v2.0)
```

---

## Integration Considerations

### Non-Sequential Writes (pwrite)

FUSE `write()` can be called with arbitrary offsets. The current v1.0 implementation
handles this via `buf.resize(end, 0); buf[offset..end].copy_from_slice(data)` — sparse
fills with zeros.

`State::push_bytes` is append-only — it does not support seeking or overwriting. The
v2.0 streaming approach only works cleanly when writes arrive in sequential order from
offset 0 upward.

**Mitigation options:**

1. **Detect sequential writes** — track `next_expected_offset: u64` in `OpenFileState`.
   If `offset == next_expected_offset`, push directly. If offset is non-sequential,
   fall back to a buffer (revert to v1.0 behaviour for that file handle).

2. **Require O_APPEND semantics from callers** — acceptable for the streaming write
   use case (cp, tar extract, etc.) but breaks random-write workloads (database files,
   mmap-write).

3. **Accept in-order-only streaming, fallback otherwise** — the pragmatic choice:
   most writes from standard tools (cp, rsync, cat) are sequential. Add a
   `mode: WriteMode` enum to `OpenFileState` and switch between streaming and
   buffered modes at first non-sequential write.

**Recommendation:** Implement option 3. Start with streaming mode. On first
non-sequential write, materialise the partially-streamed bytes into a fallback `Vec<u8>`
(by calling `GetBytes` on the partial state), then continue in buffered mode for the
remainder of the file's open session.

### Truncate with Open Write Handle

`test_setattr_size(ino, Some(fh), new_size)` currently resizes `state.buf` in-place.
With streaming `State`, truncation of an in-progress write session requires:

- If `new_size >= byte_count`: extend with zero padding (append zeros via `push_bytes`).
- If `new_size < byte_count`: there is no way to "unwind" a `State`. Must flush the
  partial state, read back the bytes, truncate, and rebuild a fresh `State` from the
  truncated content.

This case (truncate-then-write) is uncommon in normal use but must be handled for POSIX
correctness.

### Read-After-Write Consistency

The current FUSE `read()` callback in v1.0 serves from `OpenFileState.buf` for write
handles — raw bytes, no dictionary lookup needed. In v2.0, serving from a partially-
built `State` via clone+end is correct but requires holding the dict lock during the
read. This is fine for correctness; for throughput, avoid concurrent reads during
large writes.

### Inode Size During an Open Write Session

v1.0: `inode.size` is only updated at flush. Any `getattr()` during an active write
session returns the last-committed size (or 0 for a new file).

v2.0 streaming maintains `byte_count` in `OpenFileState`. To provide accurate
`getattr()` during open sessions, `getattr()` can check `open_files` for the inode's
fh and return `byte_count` as the live size. This is an improvement over v1.0 but is
optional — POSIX does not require stable size until the next `stat()` after `close()`.

---

## Architectural Patterns

### Pattern 1: Incremental Merkle Tree Insertion (State::push_bytes)

**What:** `blockset::State` (= `Vec<Level>`) is a streaming write accumulator.
`push_bytes(storage, &[u8])` feeds raw bytes one-by-one into a content-defined
Merkle tree. The tree is finalised with `state.end(storage)` at flush time. Memory
usage grows O(log N) in the number of distinct chunk boundaries, not O(N) in bytes.

**When to use:** Any sequential write of file content into the Merkle tree. Replace
`State::push_all(dict, &full_buf)` with `state.push_bytes(dict, chunk)` called
incrementally from `write()`, then `state.end(dict)` at flush.

**Key API:**
```rust
// Incremental (v2.0)
let mut state: blockset::State = State::default();
state.push_bytes(&mut dict, chunk);     // call for each write() chunk
let root256 = state.end(&mut dict);    // finalise at flush
let digest224 = dict.end(&root256);    // wrap for manifest storage

// Batch convenience (v1.0 — replaces above)
let digest224 = State::push_all(&mut dict, &full_buf);
```

### Pattern 2: Separated Write Accumulation from Read Path

**What:** `OpenFileState` owns the in-progress write `State`. Reads during an open
write session clone the `State` and call `end()` on the clone — the original
accumulator remains live. The dictionary grows monotonically; reads reconstruct bytes
from it via `GetBytes`.

**When to use:** Any read-after-write within the same file handle's open session.

### Pattern 3: Compression Removal — NoneCompressor as Default

**What:** In v1.0, compression is applied in `to_wire_bytes` before bytes enter the
Merkle tree: `compress_block(compressor, raw) → wire_bytes`. In v2.0, raw bytes
go directly into the tree: `state.push_bytes(dict, raw_data)`. The `NoneCompressor`
path already makes `to_wire_bytes` a no-op, so the code change is simply removing the
call entirely and removing the `compressor` field from `SliceFsFilesystem`.

**Why removing compression improves dedup:** With compression in the write path,
two files with identical raw content but written with different compressors produce
different `Digest224` values — they do not deduplicate. With raw-byte hashing, identical
content always produces an identical `Digest224` regardless of any future compression
layer.

---

## Anti-Patterns to Avoid

### Anti-Pattern 1: Holding dict Lock During Entire Write Session

**What people do:** Lock the dict for the entire duration of streaming push, from
first `write()` to `flush()`.

**Why it's wrong:** Blocks all other dict operations (reads, GC, metadata commits)
for the full write duration of every open file. For large files, this creates
multi-second lock contention.

**Do this instead:** Acquire the dict lock only for each individual `push_bytes` call
in `write()` (or for each FUSE-level write callback). Release it after each call.
The dict is a `BTreeMap` with synchronous `add()` semantics — fine-grained locking
is correct.

### Anti-Pattern 2: Cloning State as a Read Path Shortcut

**What people do:** Store a full `Vec<u8>` shadow buffer alongside the `State` for
fast read-after-write, reasoning that the extra memory is "just temporary."

**Why it's wrong:** For a 10GB file write, the shadow buffer holds 10GB of RAM — the
exact problem streaming writes are meant to solve. The point of streaming is to keep
memory at O(log N), not O(N).

**Do this instead:** Read-after-write during an open write session reconstructs bytes
by cloning the State and calling `end()`. This has per-call cost but bounded memory.
If read performance during writes is critical, flush to CAS first (making the bytes
available via GetBytes from the dictionary) then serve from the committed manifest.

### Anti-Pattern 3: Removing `cas_committed` Guard

**What people do:** Simplify `release()` to always call `flush_buffer_to_cas`, even
when the buffer is empty and data was already committed.

**Why it's wrong:** Without the guard, `release()` calls
`set_manifest(ino, &[])` on an empty `State`, which overwrites the just-committed
manifest with an empty one — silently erasing all written data. This is a pre-existing
v1.0 bug that was fixed with `cas_committed`. Streaming writes must preserve this guard.

### Anti-Pattern 4: Implementing Streaming via Chunked Buffers

**What people do:** Divide the write buffer into fixed-size chunks (e.g., 4MB), flush
each chunk as a separate CAS object, and store multiple Digest224s in the manifest.

**Why it's wrong:** This creates a manifest that varies with write patterns (flush
frequency), breaking deduplication for files that contain the same content but were
written in different chunk sizes. It also complicates the read path (must reassemble
across manifest entries) and the truncate path.

**Do this instead:** Use `State::push_bytes` as designed — a single `State` accumulates
all bytes across all `write()` calls for the file's open session, producing exactly one
`Digest224` per file on flush, matching v1.0 manifest format.

---

## Integration Points

### Internal Boundaries

| Boundary | Communication | Notes for v2.0 |
|----------|---------------|----------------|
| `filesystem.rs::write()` → `blockset::State::push_bytes` | Direct call on `OpenFileState.state` | Acquire dict lock for each call; dict must be passed as `&mut impl StorageAdd` |
| `filesystem.rs::flush_buffer_*()` → `blockset::State::end` | Direct call, returns `Digest256` | Then call `dict.end(&root256)` for the `Digest224` |
| `filesystem.rs::read()` → `blockset::State::clone().end()` | Clone state for non-destructive materialise | Lock ordering: open_files then dict |
| `SliceFsFilesystem.dict` → `DictMetadataStore.dict` | Two separate `Mutex<Dictionary>` clones | Write path populates filesystem's dict; manifest stores the resulting Digest224; both dicts must stay in sync (currently achieved by sharing the same Dictionary at construction time via Arc) |
| `write()` / `flush()` → `inode.size` | `byte_count` in OpenFileState | Update inode size at flush; optionally expose live size via getattr |

### What is NOT Changed

| Component | Status | Reason |
|-----------|--------|--------|
| `DictMetadataStore` | Unchanged | Same `set_manifest`, `increment_refcount`, `get_manifest` API |
| `blockset::State`, `Tree::push_bytes`, `Tree::end` | Unchanged | Already has the full incremental API |
| `blockset::GetBytes`, `GetData` | Unchanged | Read path reconstruction unchanged |
| `blockset::Dictionary` | Unchanged | BTreeMap, same `StorageAdd` trait |
| FUSE lifecycle (`open`, `flush`, `fsync`, `release`) | Unchanged signatures | Delegate to same helpers |
| WAL, GC, refcount logic | Unchanged | Operate on `Digest224`; indifferent to how it was produced |
| `simulate_*` helpers (mkdir, unlink, rename, etc.) | Unchanged | Do not touch write buffer |

---

## Suggested Build Order for v2.0

Dependencies drive the order.

```
Step 1 — Remove compression from write path
    - Remove to_wire_bytes() call in flush_buffer_to_cas and flush_buffer_for_fsync
    - Remove from_wire_bytes() call in read(), test_read(), test_setattr_size, simulate_readlink
    - Set store_version check to pass-through (or remove the field)
    - Tests: verify write-then-read round-trip produces identical bytes
    - NOTE: This is a prerequisite for streaming, not a consequence of it.
      Raw bytes into push_bytes is the correct semantic.

Step 2 — Add byte_count to OpenFileState
    - Add byte_count: u64 field
    - Increment in test_write on each write call
    - Use byte_count as inode.size source at flush (replacing buf.len())
    - Tests: verify getattr size matches bytes written

Step 3 — Replace Vec<u8> buf with blockset::State
    - Change OpenFileState.buf: Vec<u8> → state: blockset::State
    - Change test_write to call state.push_bytes(&mut dict, data)
    - Change flush_buffer_to_cas / flush_buffer_for_fsync to call state.end() + dict.end()
    - Update flush guard: replace buf.is_empty() check with byte_count == 0
    - Tests: write / flush / read round-trip for files of various sizes

Step 4 — Fix read-after-write for open write handles
    - Replace "serve from buf" with clone+end materialisation
    - Tests: write then read within same open session returns correct bytes

Step 5 — Handle non-sequential writes
    - Track next_expected_offset in OpenFileState
    - On sequential write: push_bytes as normal
    - On non-sequential write: fallback to materialise-and-buffer
    - Tests: pwrite at offset > 0, interspersed read/write patterns

Step 6 — Handle truncate with in-progress streaming State
    - test_setattr_size: if open write handle, handle new_size < byte_count by
      materialising, truncating, rebuilding State
    - Tests: truncate during write, extend during write

Step 7 — Remove compression crate dependency from write path
    - Remove compressor field from SliceFsFilesystem (or make it read-only for
      backward-compatible reads of old blocks if needed)
    - Remove slicefs-compression from slicefs-cli write-path imports
    - Tests: existing tests pass without compression dependency on write path
```

---

## Sources

- Direct inspection: `crates/slicefs-cli/src/filesystem.rs` (all 1,500+ lines)
- Direct inspection: `crates/data-id/blockset/src/content_dependant_tree.rs` (State impl)
- Direct inspection: `crates/data-id/blockset/src/tree.rs` (Tree trait, push_bytes, push_all)
- Direct inspection: `crates/data-id/blockset/src/merkle_tree.rs` (MerkleTreeState)
- Direct inspection: `crates/data-id/blockset/src/dictionary.rs` (StorageAdd impl)
- Direct inspection: `crates/data-id/blockset/src/get_data.rs` (GetBytes, GetData)
- Direct inspection: `crates/metadata/src/store.rs` (DictMetadataStore structure)
- Direct inspection: `crates/slicefs-compression/src/lib.rs` (compress_block, decompress_block)
- Direct inspection: `crates/slicefs-cli/tests/write_path_tests.rs` (integration test patterns)
- `.planning/PROJECT.md` (v2.0 milestone goals)

---

*Architecture research for: SliceFS v2.0 streaming writes integration*
*Researched: 2026-03-29*
