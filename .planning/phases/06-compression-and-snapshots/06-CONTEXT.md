# Phase 6: Compression and Snapshots - Context

**Gathered:** 2026-03-29
**Status:** Ready for planning

<domain>
## Phase Boundary

Stored blocks are compressed to reduce physical footprint; point-in-time snapshots capture filesystem state and allow switching between historical versions. Both capabilities are natural expressions of the CAS architecture already in place.

Requirements: COMP-01, COMP-02, SNAP-01, SNAP-02, SNAP-03

</domain>

<decisions>
## Implementation Decisions

### Compression layer design
- **Pluggable Compressor trait** in slicefs-traits with compress()/decompress()/algorithm_id() — consistent with ContentHasher/Chunker pattern
- **Both Zstd and LZ4** ship as concrete implementations behind the trait
- **Per-mount changeable** compressor — each block records its compression method (1-byte header). Different mounts can use different compressors. Old blocks remain readable regardless of current compressor. Blocks compressed with different algorithms coexist in the same store
- **Compression level configurable** — expose native level parameter (Zstd: 1-22, LZ4: acceleration factor) via CLI flag (e.g., --zstd-level 3). Sane defaults for each
- **Optional compression** — mount with `--compressor none` to disable entirely. Matches `--wal-strategy` pattern from Phase 5
- **Incompressible block detection** — if compressed output >= original size, store block uncompressed with a "raw" flag. Avoids wasting CPU on already-compressed data (JPEG, ZIP, encrypted)

### Compression + dedup ordering (COMP-02)
- **Dedup-first-then-compress** — hash original (uncompressed) content for dedup, store compressed. Dedup decisions are based on content identity, compression is a storage optimization
- Content hash is always computed on raw bytes — compression is transparent to the dedup layer

### Snapshot metadata model
- **Both name + auto-version** — each snapshot gets an auto-incrementing version number (u64) AND an optional user-provided name/tag
- **Minimal metadata** — SnapshotEntry { version: u64, name: Option<String>, root: Digest224, created_at: u64 }
- **Immutable / append-only** — snapshots can never be deleted, only new ones created. Storage grows but GC root set management is simplified (roots only grow, never shrink)

### Version switching UX
- **CLI snapshot create** — `slicefs snapshot create <store> [--name "before-upgrade"]`. Works while mounted (live snapshot) or unmounted. Returns version number and name
- **Auto-snapshot on unmount** — optional `--auto-snapshot` flag on mount creates a snapshot on every clean unmount
- **Read-only snapshot mount** — `slicefs mount <store> <mountpoint> --snapshot <version|name>` mounts a specific snapshot read-only. Multiple snapshots can be mounted simultaneously at different paths
- **Live FS switch** — `slicefs snapshot switch <store> <version|name>` switches the live root pointer. Requires unmount first for safety
- **Auto-snapshot before switch** — before switching to a historical version, automatically create a snapshot of the current state. Guarantees no data loss — user can always switch back
- **Basic list** — `slicefs snapshot list <store>` shows version number, name, timestamp, root digest. Simple table output. Storage sharing analysis deferred to Phase 7 stats command

### Claude's Discretion
- Compression placement in data path (block-level vs segment-level)
- Snapshot table persistence mechanism (in segments vs separate file)
- Compression header format and magic bytes
- Default compression levels for each algorithm
- Snapshot version numbering implementation (counter in store metadata)
- How live snapshot interacts with in-flight writes (likely: commit current state, then record root)

</decisions>

<code_context>
## Existing Code Insights

### Reusable Assets
- `collect_live_set(dict, &[Digest224])` in gc/mod.rs — already accepts multiple roots for snapshot-aware GC
- `GarbageCollector::run_gc()` takes `roots: &[Digest224]` — snapshot roots plug directly in
- Background GC in gc/background.rs has placeholder: "Phase 6 will add snapshot root discovery"
- `WalStrategy` trait pattern — Compressor trait follows the same pluggable architecture
- `SegmentEntry` in segment/mod.rs — can be extended with compression flag or new variant for snapshot entries
- `DictMetadataStore::commit()` returns root Digest224 — snapshot creation = save this root + metadata
- `DictMetadataStore::current_root()` — GC already uses this; snapshot roots extend the root list

### Established Patterns
- Pluggable traits in slicefs-traits (ContentHasher, Chunker, BlockStore, WalStrategy) — Compressor follows this pattern
- Mount-time configuration via CLI flags (--wal-strategy) — --compressor follows this pattern
- 92-byte Dictionary entries (Digest224 key + Branches value) — compression changes physical representation
- Log-structured segments with WAL — snapshot entries can be a new segment entry type

### Integration Points
- New `Compressor` trait in slicefs-traits
- Concrete LZ4 + Zstd implementations (new crate or in cas-local)
- Segment writer/reader modified to handle compressed entries
- New `slicefs snapshot` CLI subcommand group (create/list/switch)
- Mount command gets `--compressor`, `--compressor-level`, `--auto-snapshot`, `--snapshot` flags
- Background GC updated to discover snapshot roots (currently hardcoded to single current_root)
- Offline GC (`slicefs gc`) updated to walk snapshot roots

</code_context>

<specifics>
## Specific Ideas

- Immutable snapshots are a deliberate choice — simplifies GC root management and matches the CAS append-only philosophy. Snapshot deletion can be added in a future phase if storage pressure demands it
- Auto-snapshot before version switch ensures users can never lose data by switching — the current state is always recoverable
- Per-mount changeable compressor with per-block headers enables gradual migration between algorithms without store recreation
- The `--compressor none` option parallels `--wal-strategy no-wal` — both are escape hatches for specific workloads

</specifics>

<deferred>
## Deferred Ideas

- **Snapshot deletion** — if storage pressure becomes an issue, add `slicefs snapshot delete` in a future phase. Requires GC root set shrinking logic
- **Storage sharing analysis** — per-snapshot unique block count for `snapshot list`. Expensive tree walking; defer to Phase 7 stats command
- **Snapshot diff** — show what changed between two snapshots. Useful but complex tree comparison; future phase
- **Compression ratio stats** — per-file or per-store compression ratio reporting. Defer to Phase 7 stats command
- **Writable snapshot clones** — ADV-01 in v2 requirements. Branch-on-write from a snapshot. Already tracked

</deferred>

---

*Phase: 06-compression-and-snapshots*
*Context gathered: 2026-03-29*
