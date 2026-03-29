# Stack Research

**Domain:** SliceFS v2.0 — Streaming writes, write-path compression removal, bug fixes
**Researched:** 2026-03-29
**Confidence:** HIGH — all changes are internal refactors or stdlib arithmetic; no new external crates required

---

## Executive Summary

This milestone adds **no new external dependencies**. Every change is either:

- A refactor within existing crates (streaming writes, compression removal)
- Stdlib arithmetic primitives (`saturating_add`, `checked_add`)
- Stdlib collection swap (`Vec` → `HashMap` for snapshot index)
- An `AtomicU64` counter already used in the metadata store

The v1.0 STACK.md (`STACK.md` dated 2026-03-27) remains fully valid. This document records only the delta — which APIs to use, which to stop using, and why.

---

## What Changes (and Why)

### 1. Streaming Writes — `State::push_bytes()` replaces `State::push_all()`

**Current:** `OpenFileState.buf: Vec<u8>` accumulates the entire file in memory. On `release()` / fsync, `State::push_all(&mut dict, &wire_bytes)` pushes the entire buffer at once. For large files this is an unbounded memory allocation.

**New:** `OpenFileState` holds a live `blockset::State` (type alias: `Vec<Level>`, where `Level = (MerkleTreeState, Digest256)`). Each FUSE `write()` call feeds incoming data directly into `state.push_bytes(&mut dict, data)` and discards the raw bytes immediately. On `release()`, `state.end(&mut dict)` finalises the Merkle root.

**API already exists.** The `push_bytes` method is defined on the `Tree` trait in `crates/data-id/blockset/src/tree.rs` (lines 36-38). It is not new — it just has not been wired into the FUSE write path yet. No crate version bump needed.

**Memory profile after change:** `O(log N)` — `State` holds at most one `(MerkleTreeState, Digest256)` entry per tree level; each level is ~72 bytes; at 4-byte leaf granularity a 1 GiB file produces ~7-8 levels.

**Integration point:** `crates/slicefs-cli/src/filesystem.rs`

- `OpenFileState` struct: replace `buf: Vec<u8>` + `cas_committed: bool` with `state: blockset::State` + a `dict` reference or lock handle.
- `write()` FUSE handler: remove `buf.resize/copy_from_slice`, call `state.push_bytes(&mut dict, data)`.
- `flush_buffer_to_cas()` and `flush_buffer_for_fsync()`: call `state.clone().end(&mut dict)` to finalise without consuming (fsync must allow further writes); or consume on `release()`.
- `test_write()` / `test_release()`: update to match.

**Constraint:** `State` is `Vec<Level>` which is `Clone` — cloning it for a non-consuming fsync is safe and cheap (log-depth vector).

| API | Location | Status |
|-----|----------|--------|
| `Tree::push_bytes(&mut self, storage, v: &[u8])` | `blockset::tree` | Exists — wire it in |
| `Tree::end(self, storage) -> Digest256` | `blockset::tree` | Exists — wire it in |
| `State::push_all(storage, v) -> Digest224` | `blockset::content_dependant_tree` | Keep for small writes (symlink target, xattr); remove from large file path |

---

### 2. Remove Write-Path Compression

**Current:** `to_wire_bytes()` wraps every block with `compress_block()` (1-byte `AlgorithmId` header + compressed payload). All four write sites in `filesystem.rs` (lines 372, 410, 491, 832) call this. The `store_version >= 2` gate switches it on.

**New:** Raw bytes go directly into the Merkle tree. Dedup operates on original content, which means two identical files compressed with different algorithms will now correctly deduplicate.

**Changes:**
- Delete `to_wire_bytes()` from `SliceFsFilesystem` (or replace body with `raw.to_vec()` as no-op stub during transition).
- Delete `from_wire_bytes()` or leave as read-path fallback for old blocks only.
- Remove `compressor` field from `SliceFsFilesystem` (or demote to `Option<Arc<dyn Compressor>>`).
- Remove `store_version` field (or keep it only for backward-compatible reads of old compressed blocks).
- The `slicefs-compression` crate stays in the workspace — it may be used for future segment-level compression (v2.1). Do not delete it.

**No version changes needed.** `zstd`, `lz4_flex`, and `slicefs-compression` remain in Cargo.toml — just unused on the write path.

---

### 3. Refcount Overflow Fix — `saturating_add` / `checked_add`

**Current:** `increment_refcount()` in `crates/metadata/src/store.rs` (line 136):

```rust
*rc.entry(*digest).or_insert(0) += 1;
```

`+= 1` on a `u64` wraps to 0 on overflow in release builds (Rust integer overflow is defined as wrapping in release mode). A file with `u64::MAX` references would silently drop its refcount to 0, making the block eligible for GC — silent data loss.

**Fix:** Use `saturating_add(1)`:

```rust
let count = rc.entry(*digest).or_insert(0);
*count = count.saturating_add(1);
```

`saturating_add` clamps at `u64::MAX` instead of wrapping. In practice no real file will ever reach `u64::MAX` references, so this is both safe and correct.

**Alternative considered:** `checked_add` returning `Err`. Rejected — the caller has no reasonable error recovery path for "too many references"; saturating is the correct semantic for a reference-counted store where the count is a safety floor, not an exact accounting figure.

**No new dependencies.** `u64::saturating_add` is `std`.

---

### 4. Realistic `statfs` Reporting

**Current:** `statfs()` in `filesystem.rs` hardcodes `f_files = 1_000_000` and `bfree = u64::MAX / 4` (lines 1158-1164). This makes `df` display nonsense.

**Needed:**
- `f_files` (total inodes): track the live inode count. The metadata store already has `logical_bytes: AtomicU64` as a precedent. Add `inode_count: AtomicU64` to `DictMetadataStore`, incremented in `create_inode()` and decremented in `delete_inode()`.
- `f_ffree` (free inodes): `u64::MAX - inode_count` (CAS filesystem has no hard inode limit; this is the honest answer).
- `f_blocks` (total blocks): derive from physical bytes — `dict.len() * 92 / bsize` (already computed for `blocks_used`).
- `bfree` / `bavail`: CAS filesystem is append-only (old blocks are GC'd, new blocks always storable). A realistic answer is `u64::MAX / bsize` — "effectively unlimited" — which is more truthful than the current sentinel value, and matches what ZFS reports on pools with no hard quota.

**Changes:**
- `crates/metadata/src/store.rs`: add `inode_count: AtomicU64` field. Increment in `create_inode`, decrement in `delete_inode`. Expose via `fn inode_count(&self) -> u64`.
- `crates/slicefs-cli/src/filesystem.rs`: use `self.meta.inode_count()` for `f_files` in `statfs()`.

**No new dependencies.** `AtomicU64` is `std::sync::atomic`.

---

### 5. Snapshot Indexed Lookup — `HashMap` Indexes

**Current:** `snapshots: Mutex<Vec<SnapshotEntry>>` (line 75 of `store.rs`). `find_snapshot()` does `snaps.iter().find(...)` — O(n) linear scan. For typical snapshot counts (tens to hundreds) this is not a bottleneck, but the PROJECT.md explicitly targets O(1).

**New:** Replace the single `Vec` with two indexes:

```rust
snapshots_by_version: Mutex<HashMap<u64, SnapshotEntry>>,
snapshots_by_name:    Mutex<HashMap<String, u64>>,   // name → version key
```

`find_snapshot(ref)`:
- If `ref` parses as `u64`: `snapshots_by_version.get(&version)` — O(1).
- Otherwise: `snapshots_by_name.get(name).and_then(|v| snapshots_by_version.get(v))` — O(1).

`list_snapshots()`: collect values from `snapshots_by_version`, sort by version — O(n log n), same as before.

`create_snapshot()` and `set_snapshots()`: insert into both maps.

**Why `HashMap` not `BTreeMap`:** `BTreeMap` would give O(log n) lookup and in-order iteration without the sort step in `list_snapshots`. For a small number of snapshots (expected: < 1000) the difference is negligible. `HashMap` is used because the PROJECT.md specifies O(1) lookup, not O(log n), and `HashMap` is more idiomatic for keyed lookup. If sorted iteration performance matters more than O(1) lookup a `BTreeMap<u64, SnapshotEntry>` + `HashMap<String, u64>` hybrid is also valid.

**No new dependencies.** `std::collections::HashMap` is `std`.

---

## Recommended Stack (Delta Only)

No new crates. Existing crates unchanged.

### APIs to Start Using

| API | Crate | Where | Purpose |
|-----|-------|-------|---------|
| `Tree::push_bytes(&mut self, storage, &[u8])` | `blockset` (local) | `filesystem.rs` write path | Stream bytes into Merkle tree without buffering full file |
| `Tree::end(self, storage) -> Digest256` | `blockset` (local) | `filesystem.rs` release/fsync | Finalise in-progress Merkle state to root digest |
| `u64::saturating_add(1)` | `std` | `store.rs::increment_refcount` | Refcount overflow protection |
| `AtomicU64` | `std::sync::atomic` | `store.rs` | Track live inode count for statfs |
| `HashMap<u64, SnapshotEntry>` | `std::collections` | `store.rs` | O(1) snapshot version lookup |
| `HashMap<String, u64>` | `std::collections` | `store.rs` | O(1) snapshot name lookup |

### APIs to Stop Using (on write path)

| API | Crate | Reason |
|-----|-------|--------|
| `State::push_all(storage, &[u8]) -> Digest224` | `blockset` (local) | Buffers entire file; replace with `push_bytes` + `end` on large-file path |
| `compress_block(&compressor, &[u8])` | `slicefs-compression` | Write-path compression removed; dedup on raw content |
| `SliceFsFilesystem.to_wire_bytes()` | `slicefs-cli` | Wrapper around `compress_block`; delete |

### Crates That Stay But Change Role

| Crate | v1.0 Role | v2.0 Role |
|-------|-----------|-----------|
| `slicefs-compression` | Write + read path | Read path only (decompress legacy blocks); keep for v2.1 segment compression |
| `blockset` (local) | `push_all` for writes | `push_bytes` + `end` for writes; `push_all` kept for small payloads (xattr, symlinks) |

---

## What NOT to Add

| Do Not Add | Why |
|------------|-----|
| New streaming I/O crate (`bytes`, `tokio::io::AsyncWrite`, etc.) | Overkill — `push_bytes` already accepts `&[u8]` slices; FUSE write callbacks deliver data in fixed-size chunks natively |
| A write buffer crate (`crossbeam-channel`, ring buffer, etc.) | The `State` (Merkle stack) is already the buffer; no secondary buffer needed |
| A new indexing/KV crate for snapshots | `std::collections::HashMap` is sufficient; `redb` is appropriate if snapshots need to persist independently of WAL, but WAL-backed persistence is already working |
| Any new async machinery | `push_bytes` is synchronous; no async required for this milestone |
| Separate inode counter storage | `AtomicU64` on `DictMetadataStore` is sufficient; does not need separate DB table |

---

## Version Compatibility Notes

All changes are within existing dependency versions already in `Cargo.lock`:

| Package | Locked Version | Notes |
|---------|---------------|-------|
| `blockset` (local) | 0.1.0 | `push_bytes` and `end` already on `Tree` trait — no bump needed |
| `fuser` | 0.17.0 | `write()` callback signature unchanged |
| `blake3` | 2.11.0 (resolved) | No change |
| `thiserror` | 3.27.0 (resolved) | No change |
| `libc` | 0.2.x | No change |

---

## Integration Points Summary

| Change | File | What to Modify |
|--------|------|----------------|
| Streaming writes | `crates/slicefs-cli/src/filesystem.rs` | `OpenFileState`, `write()`, `flush_buffer_to_cas()`, `flush_buffer_for_fsync()`, `test_write()`, `test_release()` |
| Remove write compression | `crates/slicefs-cli/src/filesystem.rs` | Delete `to_wire_bytes()`, remove `compressor` field or demote to read-path only |
| Refcount overflow | `crates/metadata/src/store.rs` | `increment_refcount()` line 136 |
| Inode count tracking | `crates/metadata/src/store.rs` | Add `inode_count: AtomicU64`, update `create_inode`, `delete_inode`, expose getter |
| Statfs improvement | `crates/slicefs-cli/src/filesystem.rs` | `statfs()` handler, consume `meta.inode_count()` |
| Snapshot index | `crates/metadata/src/store.rs` | Replace `snapshots: Mutex<Vec<SnapshotEntry>>` with two HashMaps |

---

## Sources

- `crates/data-id/blockset/src/tree.rs` — `push_bytes` and `end` API confirmed present (lines 36-49); no external crate needed
- `crates/data-id/blockset/src/content_dependant_tree.rs` — `State = Vec<Level>` type confirmed; `Clone` via `Vec` derive
- `crates/slicefs-cli/src/filesystem.rs` — `State::push_all` call sites confirmed at lines 375, 414, 494, 835; `compress_block` call sites confirmed
- `crates/metadata/src/store.rs` — `refcounts: Mutex<BTreeMap<Digest224, u64>>` and `+= 1` overflow confirmed at line 136; `snapshots: Mutex<Vec<SnapshotEntry>>` linear scan confirmed
- `crates/slicefs-cli/src/filesystem.rs` — `statfs()` hardcoded values confirmed at lines 1158-1164
- Rust Reference: integer overflow in release mode is defined as wrapping (`wrapping_add`) — `saturating_add` is the correct fix [HIGH confidence]
- `std::collections::HashMap` — O(1) average lookup [HIGH confidence, stdlib]

---

*Stack research for: SliceFS v2.0 — Streaming writes and bug fixes*
*Researched: 2026-03-29*
