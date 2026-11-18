# Phase 6: Compression and Snapshots - Research

**Researched:** 2026-03-29
**Domain:** Rust block compression (zstd/lz4_flex), CAS-layer pluggable traits, snapshot metadata persistence
**Confidence:** HIGH

## Summary

Phase 6 adds two capabilities that are natural expressions of the existing CAS architecture. Compression sits between the caller and the blockset Dictionary: raw bytes are hashed for dedup identity, then compressed before storage, with a 1-byte algorithm header prepended so blocks remain self-describing. Snapshots are even simpler — a snapshot is just a named, time-stamped record of a root `Digest224` plus version counter, persisted as a new `SegmentEntry` variant alongside existing `DictEntry` and `RootUpdate` records.

Both capabilities follow established patterns already in the codebase. The `Compressor` trait mirrors `WalStrategy` (pluggable behind a trait object, selected by CLI flag). The snapshot table mirrors how `last_root` is tracked in `DictMetadataStore` — extend it to a `Vec<SnapshotEntry>` and add a new `SnapshotRecord` segment entry type for persistence.

The critical architectural insight is that snapshot-aware GC already works: `collect_live_set(dict, &[Digest224])` accepts a slice of roots, and `GarbageCollector::run_gc` passes `roots: &[Digest224]`. The Phase 6 change is just supplying all snapshot roots instead of only `current_root()`.

**Primary recommendation:** Implement `Compressor` trait + zstd/lz4_flex in a new `slicefs-compression` crate; add `SnapshotRecord` variant to `SegmentEntry`; extend `DictMetadataStore` with a `snapshots: Mutex<Vec<SnapshotEntry>>` field; add `slicefs snapshot` CLI subcommand group.

---

<user_constraints>
## User Constraints (from CONTEXT.md)

### Locked Decisions

**Compression layer design:**
- Pluggable `Compressor` trait in `slicefs-traits` with `compress()`/`decompress()`/`algorithm_id()` — consistent with `ContentHasher`/`Chunker` pattern
- Both Zstd and LZ4 ship as concrete implementations behind the trait
- Per-mount changeable compressor — each block records its compression method (1-byte header). Different mounts can use different compressors. Old blocks remain readable regardless of current compressor. Blocks compressed with different algorithms coexist in the same store
- Compression level configurable — expose native level parameter (Zstd: 1-22, LZ4: acceleration factor) via CLI flag (e.g., `--zstd-level 3`). Sane defaults for each
- Optional compression — mount with `--compressor none` to disable entirely. Matches `--wal-strategy` pattern from Phase 5
- Incompressible block detection — if compressed output >= original size, store block uncompressed with a "raw" flag. Avoids wasting CPU on already-compressed data (JPEG, ZIP, encrypted)

**Compression + dedup ordering (COMP-02):**
- Dedup-first-then-compress — hash original (uncompressed) content for dedup, store compressed. Dedup decisions are based on content identity, compression is a storage optimization
- Content hash is always computed on raw bytes — compression is transparent to the dedup layer

**Snapshot metadata model:**
- Both name + auto-version — each snapshot gets an auto-incrementing version number (u64) AND an optional user-provided name/tag
- Minimal metadata — `SnapshotEntry { version: u64, name: Option<String>, root: Digest224, created_at: u64 }`
- Immutable / append-only — snapshots can never be deleted, only new ones created. Storage grows but GC root set management is simplified (roots only grow, never shrink)

**Version switching UX:**
- CLI `snapshot create` — `slicefs snapshot create <store> [--name "before-upgrade"]`. Works while mounted (live snapshot) or unmounted. Returns version number and name
- Auto-snapshot on unmount — optional `--auto-snapshot` flag on mount creates a snapshot on every clean unmount
- Read-only snapshot mount — `slicefs mount <store> <mountpoint> --snapshot <version|name>` mounts a specific snapshot read-only. Multiple snapshots can be mounted simultaneously at different paths
- Live FS switch — `slicefs snapshot switch <store> <version|name>` switches the live root pointer. Requires unmount first for safety
- Auto-snapshot before switch — before switching to a historical version, automatically create a snapshot of the current state. Guarantees no data loss — user can always switch back
- Basic list — `slicefs snapshot list <store>` shows version number, name, timestamp, root digest. Simple table output. Storage sharing analysis deferred to Phase 7 stats command

### Claude's Discretion
- Compression placement in data path (block-level vs segment-level)
- Snapshot table persistence mechanism (in segments vs separate file)
- Compression header format and magic bytes
- Default compression levels for each algorithm
- Snapshot version numbering implementation (counter in store metadata)
- How live snapshot interacts with in-flight writes (likely: commit current state, then record root)

### Deferred Ideas (OUT OF SCOPE)
- **Snapshot deletion** — if storage pressure becomes an issue, add `slicefs snapshot delete` in a future phase. Requires GC root set shrinking logic
- **Storage sharing analysis** — per-snapshot unique block count for `snapshot list`. Expensive tree walking; defer to Phase 7 stats command
- **Snapshot diff** — show what changed between two snapshots. Useful but complex tree comparison; future phase
- **Compression ratio stats** — per-file or per-store compression ratio reporting. Defer to Phase 7 stats command
- **Writable snapshot clones** — ADV-01 in v2 requirements. Branch-on-write from a snapshot. Already tracked
</user_constraints>

---

<phase_requirements>
## Phase Requirements

| ID | Description | Research Support |
|----|-------------|-----------------|
| COMP-01 | Compression of stored blocks (pluggable compressor, e.g., LZ4 for speed, Zstd for ratio) | `Compressor` trait + zstd 0.13.x + lz4_flex 0.13.x; 1-byte algorithm header enables coexistence |
| COMP-02 | Dedup-first-then-compress ordering (hash original content, store compressed) | Hash on raw bytes before `compress()` call; compression is below the dedup layer |
| SNAP-01 | Read-only point-in-time snapshots (frozen metadata tree, shared CAS blocks) | `SnapshotRecord` segment entry stores root `Digest224`; mount with `--snapshot N` sets `MountOption::RO` |
| SNAP-02 | Filesystem version history with ability to switch between historical versions | `snapshot list` reads all `SnapshotRecord` entries from segments; `snapshot switch` updates live root pointer |
| SNAP-03 | Efficient version switching at block level (leveraging CAS architecture) | CAS blocks are already shared; switching root pointer is O(1); no block copying needed |
</phase_requirements>

---

## Standard Stack

### Core
| Library | Version | Purpose | Why Standard |
|---------|---------|---------|--------------|
| zstd | 0.13.3 | Zstd compression binding | Industry-standard ratio compressor; levels 1-22; `encode_all`/`decode_all` one-shot API |
| lz4_flex | 0.13.0 | Pure-Rust LZ4 implementation | Fastest LZ4 in Rust; no C deps; safe by default; `block::compress`/`block::decompress` for in-memory buffers |

### Supporting
| Library | Version | Purpose | When to Use |
|---------|---------|---------|-------------|
| (none new) | — | Snapshot persistence reuses existing segment infrastructure | Extend `SegmentEntry` with `SnapshotRecord` variant |

### Alternatives Considered
| Instead of | Could Use | Tradeoff |
|------------|-----------|----------|
| lz4_flex (pure Rust) | lz4 (FFI binding) | lz4 FFI has C build dep; lz4_flex is pure Rust, no unsafe by default, actively maintained |
| zstd (FFI binding) | zstd-rs (pure Rust) | zstd-rs is immature; zstd crate wraps official libzstd which is the reference impl |

**Installation:**
```bash
# In slicefs-compression crate (new) or cas-local
cargo add zstd@0.13
cargo add lz4_flex@0.13

# In workspace Cargo.toml:
# zstd = "0.13"
# lz4_flex = "0.13"
```

---

## Architecture Patterns

### Recommended Project Structure

```
crates/
├── slicefs-traits/src/
│   ├── compressor.rs        # NEW: Compressor trait + AlgorithmId enum
│   └── lib.rs               # re-export Compressor
├── slicefs-compression/     # NEW crate
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs
│       ├── zstd_compressor.rs
│       ├── lz4_compressor.rs
│       └── none_compressor.rs
├── metadata/src/
│   ├── segment/mod.rs       # Add SnapshotRecord variant to SegmentEntry
│   ├── store.rs             # Add snapshots field, create_snapshot(), list_snapshots(), switch_root()
│   └── snapshot.rs          # NEW: SnapshotEntry struct, snapshot table helpers
└── slicefs-cli/src/
    ├── cli.rs               # Add Snapshot subcommand group + mount flags
    ├── snapshot.rs          # NEW: run_snapshot_create/list/switch
    └── mount.rs             # Add --compressor, --compressor-level, --auto-snapshot, --snapshot flags
```

### Pattern 1: Compressor Trait (mirrors WalStrategy)

**What:** A `&self` trait with three methods — compress, decompress, algorithm_id. Implementations are stateless or hold only configuration (level).

**When to use:** Called in the block write path after hashing but before storing bytes.

```rust
// In slicefs-traits/src/compressor.rs
// Source: mirrors WalStrategy pattern in metadata/src/wal/mod.rs

/// Algorithm discriminant — stored as 1-byte header in every compressed block.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlgorithmId {
    None  = 0x00,  // uncompressed (original bytes)
    Zstd  = 0x01,
    Lz4   = 0x02,
    Raw   = 0x03,  // incompressible — stored uncompressed even when compressor != None
}

/// Pluggable block compressor.
///
/// All implementations must be Send + Sync to allow use behind Arc.
/// compress() and decompress() operate on owned byte vectors for simplicity.
pub trait Compressor: Send + Sync {
    /// Compress `input` and return compressed bytes.
    /// Returns `(AlgorithmId::Raw, input.to_vec())` when compressed >= original.
    fn compress(&self, input: &[u8]) -> Result<(AlgorithmId, Vec<u8>), CompressorError>;

    /// Decompress `input` using the given algorithm.
    fn decompress(&self, algorithm: AlgorithmId, input: &[u8]) -> Result<Vec<u8>, CompressorError>;

    /// Algorithm this compressor produces (None for passthrough).
    fn algorithm_id(&self) -> AlgorithmId;
}
```

### Pattern 2: Block Wire Format with Compression Header

**What:** Each block stored to the blockset Dictionary is prefixed with a 1-byte algorithm_id. Decompression reads the byte first, then dispatches.

**When to use:** Applied at the boundary where bytes enter/leave the blockset (inside `flush_buffer_to_cas` or equivalent, after refcount bookkeeping).

```rust
// Compression write path (applied AFTER hash computed, BEFORE dict push)
// Source: derived from existing flush_buffer_to_cas pattern in slicefs-cli/src/filesystem.rs

fn compress_block(compressor: &dyn Compressor, raw: &[u8]) -> Vec<u8> {
    let (algo_id, compressed) = compressor.compress(raw)
        .unwrap_or_else(|_| (AlgorithmId::Raw, raw.to_vec()));
    let mut wire = Vec::with_capacity(1 + compressed.len());
    wire.push(algo_id as u8);
    wire.extend_from_slice(&compressed);
    wire
}

fn decompress_block(compressor: &dyn Compressor, wire: &[u8]) -> Result<Vec<u8>, CompressorError> {
    let algo = AlgorithmId::from_u8(wire[0])?;
    compressor.decompress(algo, &wire[1..])
}
```

### Pattern 3: Incompressible Block Detection

**What:** Inside `compress()`, if `compressed.len() >= input.len()`, return `(AlgorithmId::Raw, input.to_vec())` — store original bytes with a Raw header. On decompress, `AlgorithmId::Raw` means return slice verbatim.

**When to use:** Automatic — every compressor implements this internally. Callers never need to check.

```rust
// Inside ZstdCompressor::compress — incompressible detection
fn compress(&self, input: &[u8]) -> Result<(AlgorithmId, Vec<u8>), CompressorError> {
    let compressed = zstd::encode_all(input, self.level)?;
    if compressed.len() >= input.len() {
        return Ok((AlgorithmId::Raw, input.to_vec()));
    }
    Ok((AlgorithmId::Zstd, compressed))
}
```

### Pattern 4: Snapshot Record as Segment Entry (preferred over separate file)

**What:** A `SnapshotRecord` variant added to `SegmentEntry` in `metadata/src/segment/mod.rs`. The payload is: `version: u64` (8 bytes) + `root: Digest224` (28 bytes) + `created_at: u64` (8 bytes) + `name_len: u32` (4 bytes) + `name: [u8; name_len]` (variable). Total minimum: 48 bytes.

**When to use:** Snapshot creation appends a `SnapshotRecord` to the active WAL segment. On replay, `load_store_from_segments` collects all `SnapshotRecord` entries into a `Vec<SnapshotEntry>`.

**Why segments over separate file:** Atomicity is free — the WAL segment is already crash-safe. A snapshot is durably committed when the segment write returns. No new file format needed.

```rust
// Extended SegmentEntry in metadata/src/segment/mod.rs
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegmentEntry {
    DictEntry      { key: Digest224, branches: Branches },
    RootUpdate     { root: Digest224 },
    SnapshotRecord {            // NEW in Phase 6
        version:    u64,
        root:       Digest224,
        created_at: u64,        // Unix timestamp (seconds)
        name:       Option<String>,
    },
}
```

### Pattern 5: Snapshot-Aware GC (minimal change)

**What:** `background.rs` currently calls `store.current_root()` to get a single root. Change to `store.snapshot_roots()` which returns all snapshot roots plus the current live root.

**When to use:** Drop-in replacement — `GarbageCollector::run_gc` already accepts `&[Digest224]`.

```rust
// In DictMetadataStore — new helper for GC
pub fn snapshot_roots(&self) -> Vec<Digest224> {
    let mut roots = Vec::new();
    if let Some(root) = self.current_root() {
        roots.push(root);
    }
    let snapshots = self.snapshots.lock().unwrap();
    for snap in snapshots.iter() {
        roots.push(snap.root);
    }
    roots
}
```

### Pattern 6: Snapshot Mount (read-only variant of existing mount path)

**What:** `slicefs mount <store> <mountpoint> --snapshot <version|name>` loads the store, resolves the snapshot root by version or name, calls `DictMetadataStore::load_from_root(dict, &snapshot_root)`, then adds `MountOption::RO` to the FUSE config.

**When to use:** Snapshot mounts. The regular (live) mount is unchanged.

```rust
// In mount.rs — snapshot mount path
if let Some(snap_ref) = snapshot_ref {
    let snap_root = resolve_snapshot(&snapshots, snap_ref)?;
    meta = DictMetadataStore::load_from_root(content_dict.clone(), &snap_root)?;
    cfg.mount_options.push(MountOption::RO);
}
```

### Anti-Patterns to Avoid

- **Compressing before hashing:** Violates COMP-02. Hash is always on raw content; compression happens after identity is established.
- **Storing algorithm in inode or manifest:** The 1-byte header is self-contained in the wire bytes, which is the correct place. The metadata layer has no business knowing about compression.
- **Separate snapshot file (snapshot.json, etc.):** Bypasses WAL crash safety. A crash after writing snapshot data but before the segment sync loses the snapshot. Segment entries are the right persistence unit.
- **Mutable snapshot roots (snapshot deletion in Phase 6):** Out of scope per decisions. GC root management is simplified precisely because roots only grow.
- **Compressor stored in DictMetadataStore:** The metadata layer is compression-unaware. The compressor lives at the filesystem (FUSE) layer where raw bytes are assembled, exactly where `flush_buffer_to_cas` currently lives.

---

## Don't Hand-Roll

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|-------------|-----|
| Zstd frame format | Custom Zstd implementation | `zstd::encode_all` / `zstd::decode_all` | Reference implementation; handles all edge cases including dictionary context, streaming, levels |
| LZ4 block format | Custom LZ4 | `lz4_flex::block::compress` / `lz4_flex::block::decompress_size_prepended` | Handles all LZ4 framing edge cases; pure Rust, no unsafe by default |
| Incompressible detection | Custom ratio check | Return `AlgorithmId::Raw` inside `compress()` when `compressed.len() >= input.len()` | One line; already the right abstraction boundary |
| Snapshot version counter | Custom monotonic counter in a separate file | u64 field in last `SnapshotRecord` + 1 | Derived from segment replay; crash-safe by construction |
| Snapshot persistence format | Custom binary format or JSON | `SnapshotRecord` variant in existing `SegmentEntry` | Reuses WAL crash safety, segment reader/writer, and replay logic already proven in Phase 5 |

**Key insight:** Both compression and snapshots have near-zero incremental persistence complexity when built atop the existing WAL segment infrastructure.

---

## Common Pitfalls

### Pitfall 1: Compressor Placement — Wrong Layer
**What goes wrong:** Compressor is wired into `DictMetadataStore` or the blockset `Dictionary` rather than the FUSE filesystem layer.
**Why it happens:** It seems natural to compress at the "bottom" of the stack.
**How to avoid:** Compression belongs at the point where raw file bytes are assembled into blocks — inside `flush_buffer_to_cas()` in `filesystem.rs`. The metadata layer (inode structures, directory entries) is already small structured data; compressing it at a different layer creates two different compression granularities.
**Warning signs:** If `DictMetadataStore` grows a `Compressor` field, the architecture is wrong.

### Pitfall 2: Hash Computed on Compressed Bytes
**What goes wrong:** Dedup identity is broken — two files with identical content but compressed at different levels or with different algorithms produce different hashes and are not deduped.
**Why it happens:** Tempting to compress first as a pre-processing step.
**How to avoid:** The hash (content identity for dedup) must be computed on the raw, uncompressed bytes. Compression is applied to the payload after the hash is computed.
**Warning signs:** Unit test: same raw content, different compressors → same Digest224 key.

### Pitfall 3: Missing 1-Byte Header on Stored Blocks
**What goes wrong:** Old uncompressed blocks cannot be distinguished from new compressed blocks during read.
**Why it happens:** Forgetting that the store already contains uncompressed data from Phase 5.
**How to avoid:** `AlgorithmId::None = 0x00` means "uncompressed, read verbatim." Every new block gets the 1-byte header, but existing blocks do NOT have it. The read path must distinguish: if block was written pre-Phase-6 (no header), treat as raw bytes; if post-Phase-6, read header byte first.
**Warning signs:** Garbage output when reading files written before Phase 6.

**Migration strategy for existing blocks:** Store blocks currently in the Dictionary have no compression header. Two options: (a) on read, try header byte — if it's not a valid `AlgorithmId`, treat entire block as uncompressed (fragile); (b) add a store format version bump: blocks written with Phase 6 store version get headers; older blocks don't. Recommended: use the store format version. Write a `StoreVersion` record to the segment during the first Phase-6 mount, and use that to gate header presence.

### Pitfall 4: Snapshot Create Race with In-Flight Writes
**What goes wrong:** Snapshot root is captured mid-write-buffer flush; file content is partially committed.
**Why it happens:** Snapshot create can be called via CLI while the filesystem is mounted.
**How to avoid:** Snapshot create must call `meta.commit()` to flush all in-memory state to the Dictionary first, then capture the returned `Digest224` as the snapshot root. This matches the CONTEXT.md note: "commit current state, then record root."
**Warning signs:** Files in snapshot are incomplete (truncated) or missing.

### Pitfall 5: GC Deletes Snapshot-Only Blocks
**What goes wrong:** GC runs with only `current_root()` and collects blocks that are only reachable from snapshots.
**Why it happens:** `background.rs` currently hardcodes `vec![current_root]`.
**How to avoid:** Replace `store.current_root()` with `store.snapshot_roots()` in both background GC and offline GC CLI. This is the Phase 6 "snapshot-aware GC" hook already prepared in `gc/background.rs`.
**Warning signs:** Files in older snapshots return ENOENT or corrupt data after GC runs.

### Pitfall 6: lz4_flex Block vs Frame Format Confusion
**What goes wrong:** lz4_flex frame format is used for block-level storage, adding unnecessary framing overhead for fixed-size blocks.
**Why it happens:** lz4_flex README recommends the frame format for streaming; block format is presented as secondary.
**How to avoid:** Use `lz4_flex::block::compress_prepend_size` / `decompress_size_prepended` for fixed in-memory blocks. The block format is appropriate here because all blocks have known maximum sizes and there is no streaming.
**Warning signs:** Storage overhead larger than expected for LZ4.

---

## Code Examples

Verified patterns from official sources:

### Zstd One-Shot Compress/Decompress
```rust
// Source: https://docs.rs/zstd/latest/zstd/
use zstd;

// Compress
let compressed = zstd::encode_all(input_bytes.as_slice(), level)?;
// level: 1 (fastest) to 22 (best ratio); 0 = default (3)

// Decompress
let decompressed = zstd::decode_all(compressed.as_slice())?;
```

### LZ4 Block Compress/Decompress
```rust
// Source: https://docs.rs/lz4_flex/latest/lz4_flex/
use lz4_flex::block::{compress_prepend_size, decompress_size_prepended};

// Compress (prepends 4-byte original size)
let compressed = compress_prepend_size(input_bytes);

// Decompress
let decompressed = decompress_size_prepended(&compressed)
    .map_err(|e| CompressorError::Decompress(e.to_string()))?;
```

### Snapshot Create (conceptual write path)
```rust
// Source: derived from DictMetadataStore::commit() in metadata/src/store.rs

// 1. Force-commit all in-memory state to get a stable root
let root = meta.commit()?;

// 2. Record snapshot entry
let entry = SnapshotEntry {
    version: meta.next_snapshot_version(),
    name: name.map(|s| s.to_string()),
    root,
    created_at: SystemTime::now()
        .duration_since(UNIX_EPOCH).unwrap().as_secs(),
};
meta.create_snapshot(entry.clone())?;   // logs SnapshotRecord to WAL

// 3. Print result
println!("snapshot {} created: root={}", entry.version, hex_digest(&entry.root));
```

### Snapshot Switch (offline, unmounted)
```rust
// Source: derived from load_store_from_segments pattern in slicefs-cli/src/mount.rs

// 1. Resolve snapshot by version or name
let snap = snapshots.iter()
    .find(|s| s.version == version || s.name.as_deref() == Some(name_str))
    .ok_or("snapshot not found")?;

// 2. Write a RootUpdate segment entry pointing to snapshot root
// (creates new segment or appends to last open segment)
let mut writer = SegmentWriter::new(&new_seg_path, next_id)?;
writer.write_entry(&SegmentEntry::RootUpdate { root: snap.root })?;
writer.close()?;

// 3. Next mount replays segments in order and sees the new root last
```

---

## State of the Art

| Old Approach | Current Approach | When Changed | Impact |
|--------------|------------------|--------------|--------|
| No compression (Phase 5) | Block-level with 1-byte algorithm header | Phase 6 | Physical store size reduction for compressible data |
| Single live root (Phase 5) | Multiple roots: live + all snapshots | Phase 6 | GC must use `snapshot_roots()` slice not single root |
| `SegmentEntry` has 2 variants | 3 variants: DictEntry, RootUpdate, SnapshotRecord | Phase 6 | Reader/writer need new case; old segments remain valid (no migration needed) |

**Deprecated/outdated:**
- Single-root GC in `background.rs` line 86: `vec![root]` — replace with `store.snapshot_roots()`
- Segment `SEGMENT_VERSION: u32 = 1` — consider bumping to 2 to signal Phase-6 format with compression headers; enables clean migration

---

## Open Questions

1. **Migration path for pre-Phase-6 blocks (no compression header)**
   - What we know: Blocks currently stored have no 1-byte algorithm header. After Phase 6, new blocks will have it.
   - What's unclear: How to distinguish old (no-header) from new (has-header) blocks on read without walking all existing segments.
   - Recommendation: Write a `StoreFormatVersion` segment entry (new variant, value = 2) during the first Phase 6 mount. Read path: if store format version < 2, all blocks are raw (no header); if >= 2, all blocks have a header. This is a one-time write-once migration marker.

2. **Snapshot create while mounted — locking**
   - What we know: CONTEXT.md says "commit current state, then record root." `DictMetadataStore::commit()` holds `inode_map` + `dict` locks during commit.
   - What's unclear: Whether a CLI `slicefs snapshot create` while mounted needs to communicate through the FUSE layer (via a signal or IPC), or whether the CLI can open the store directly.
   - Recommendation: For Phase 6, require `slicefs snapshot create` to operate on the store directly (file-level), acquiring a store-level snapshot lock. The WAL segment is safe to append to even while the FUSE process has an open WAL writer, as long as the new record is in a new segment (atomically created). Alternatively: restrict live snapshot to `--auto-snapshot` on mount (triggered by the FUSE process itself), and require unmount for manual `snapshot create`. This avoids all cross-process locking. Recommend the simpler unmount-first restriction for Phase 6; live snapshot can be added in Phase 7.

3. **Snapshot version counter persistence**
   - What we know: Version is u64, auto-incrementing. Derived by replaying all `SnapshotRecord` entries from segments.
   - What's unclear: If the last snapshot version was 5 and then a `slicefs snapshot switch` writes a new `RootUpdate` but no snapshot, what version does the next `snapshot create` use?
   - Recommendation: `next_snapshot_version()` = `max(all SnapshotRecord.version) + 1`. Always derived from segments; never persisted separately. Safe across restarts.

---

## Validation Architecture

### Test Framework
| Property | Value |
|----------|-------|
| Framework | Rust built-in `#[test]` via `cargo test` |
| Config file | none (workspace uses `cargo test` directly) |
| Quick run command | `cargo test -p slicefs-compression 2>&1 \| tail -20` |
| Full suite command | `cargo test --workspace 2>&1 \| tail -30` |

### Phase Requirements → Test Map
| Req ID | Behavior | Test Type | Automated Command | File Exists? |
|--------|----------|-----------|-------------------|-------------|
| COMP-01 | Zstd compressor compresses compressible data to smaller size | unit | `cargo test -p slicefs-compression test_zstd_compressor_reduces_size -x` | Wave 0 |
| COMP-01 | LZ4 compressor compresses compressible data to smaller size | unit | `cargo test -p slicefs-compression test_lz4_compressor_reduces_size -x` | Wave 0 |
| COMP-01 | `--compressor none` stores blocks uncompressed | unit | `cargo test -p slicefs-compression test_none_compressor_passthrough -x` | Wave 0 |
| COMP-01 | Incompressible block detection: random data stored as raw | unit | `cargo test -p slicefs-compression test_incompressible_stored_raw -x` | Wave 0 |
| COMP-01 | Round-trip: compress then decompress returns original bytes | unit | `cargo test -p slicefs-compression test_compress_decompress_roundtrip -x` | Wave 0 |
| COMP-02 | Dedup: same content + different compressors → same Digest224 | unit | `cargo test -p metadata test_dedup_content_hash_independent_of_compressor -x` | Wave 0 |
| SNAP-01 | Snapshot create returns version number and stores root | unit | `cargo test -p metadata test_create_snapshot_returns_version -x` | Wave 0 |
| SNAP-01 | Snapshot root is readable after creation (files match state at snapshot time) | integration | `cargo test -p slicefs-cli test_snapshot_files_readable -x` | Wave 0 |
| SNAP-02 | Snapshot list shows all created snapshots in order | unit | `cargo test -p metadata test_list_snapshots_ordered -x` | Wave 0 |
| SNAP-02 | Version switch updates live root pointer | unit | `cargo test -p metadata test_switch_root_updates_live_pointer -x` | Wave 0 |
| SNAP-03 | Two snapshots sharing blocks do not double physical storage | unit | `cargo test -p metadata test_shared_blocks_not_double_counted -x` | Wave 0 |
| GC-03 | GC with multiple snapshot roots keeps snapshot-only blocks alive | unit | `cargo test -p metadata test_gc_preserves_snapshot_blocks -x` | Wave 0 |

### Sampling Rate
- **Per task commit:** `cargo test -p slicefs-compression && cargo test -p metadata 2>&1 | tail -20`
- **Per wave merge:** `cargo test --workspace 2>&1 | tail -30`
- **Phase gate:** Full suite green before `/gsd:verify-work`

### Wave 0 Gaps
- [ ] `crates/slicefs-compression/src/lib.rs` — new crate, covers COMP-01
- [ ] `crates/slicefs-compression/tests/compressor_tests.rs` — COMP-01, COMP-02
- [ ] `crates/metadata/tests/snapshot_tests.rs` — SNAP-01, SNAP-02, SNAP-03, GC-03

---

## Sources

### Primary (HIGH confidence)
- `crates/metadata/src/segment/mod.rs` — `SegmentEntry` enum, record format, `load_store_from_segments`
- `crates/metadata/src/store.rs` — `DictMetadataStore`, `commit()`, `load_from_root()`, `current_root()`
- `crates/metadata/src/gc/mod.rs` — `collect_live_set(dict, &[Digest224])`, `GarbageCollector::run_gc`
- `crates/metadata/src/gc/background.rs` — Phase 6 snapshot root hook comment
- `crates/metadata/src/wal/mod.rs` — `WalStrategy` trait pattern (Compressor mirrors this)
- `crates/slicefs-cli/src/cli.rs` — existing CLI structure (snapshot subcommand will be added here)
- `crates/slicefs-cli/src/mount.rs` — `load_store`, `parse_wal_config`, `build_mount_options`
- https://docs.rs/zstd/latest/zstd/ — zstd 0.13.3 API: `encode_all`, `decode_all`, levels 1-22
- https://docs.rs/lz4_flex/latest/lz4_flex/ — lz4_flex 0.13.0 API: `block::compress_prepend_size`, `decompress_size_prepended`

### Secondary (MEDIUM confidence)
- https://generalistprogrammer.com/tutorials/zstd-rust-crate-guide — zstd level range, DEFAULT_COMPRESSION_LEVEL constant
- https://github.com/PSeitz/lz4_flex — lz4_flex frame vs block format distinction, safe-encode/safe-decode defaults

### Tertiary (LOW confidence)
- None — all critical claims verified against official crate documentation.

---

## Metadata

**Confidence breakdown:**
- Standard stack: HIGH — zstd 0.13.3 and lz4_flex 0.13.0 verified against docs.rs
- Architecture: HIGH — patterns derived directly from existing codebase; Compressor mirrors WalStrategy exactly
- Pitfalls: HIGH — migration concern (no-header vs header) is a concrete engineering gap identified from reading Phase 5 code; all other pitfalls derived from codebase analysis
- Snapshot persistence: HIGH — segment-based approach verified as the right pattern by reading the WAL/segment code

**Research date:** 2026-03-29
**Valid until:** 2026-06-29 (zstd and lz4_flex are stable; codebase patterns are frozen until Phase 7)
