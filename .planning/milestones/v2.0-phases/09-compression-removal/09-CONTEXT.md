# Phase 9: Compression Removal - Context

**Gathered:** 2026-03-30
**Status:** Ready for planning

<domain>
## Phase Boundary

Remove compression from the write path entirely. Raw bytes go directly into the Merkle tree with no compression header. Clean break — no v1/v2 backward compatibility (no production stores exist). Remove `compress_block`/`decompress_block` and the `slicefs-compression` crate dependency from the write/read path.

</domain>

<decisions>
## Implementation Decisions

### Write Path (DECOMP-01)
- `to_wire_bytes()` is removed or becomes identity — raw bytes go directly to FileStorageAdd
- No `compress_block` call anywhere in the write path
- Digest224 is computed on raw content (already the case — hash before compress was the v1.0 design)

### Store Format (DECOMP-02)
- Store format version is v3 (raw blocks, no compression header)
- `store_version` field removed or fixed at 3 — no version gating needed
- No `AlgorithmId` header byte in stored blocks

### Read Path (DECOMP-03) — CLEAN BREAK
- **No v1/v2 backward compatibility** — no production stores exist
- `from_wire_bytes()` is removed or becomes identity — stored bytes are raw content
- `decompress_block` calls removed entirely
- Old stores must be re-seeded (acceptable — no production data)

### Cross-Dedup (DECOMP-04)
- Digest224 identity computed on raw content — cross-file dedup works naturally since there's only one format now

### Compression Crate
- `slicefs-compression` crate can be removed from workspace or kept as dead code for future segment-level compression (v2.1)
- Remove `compressor` field from `SliceFsFilesystem` and `--compressor` CLI flags
- Remove `slicefs-compression` dependency from `slicefs-cli/Cargo.toml`

### Claude's Discretion
- Whether to keep `slicefs-compression` crate in workspace for future segment-level compression or remove entirely
- How to handle `store_version` field — remove, hardcode 3, or keep as config
- Cleanup of `AlgorithmId` references across the codebase

</decisions>

<specifics>
## Specific Ideas

- No production instances exist — clean break is safe and simplest
- This is preparation for Phase 10 (streaming writes) which needs raw bytes flowing through push_bytes
- Segment-level compression (Option C from v2.0 planning) is deferred to v2.1 — keep the crate if useful later

</specifics>

<code_context>
## Existing Code Insights

### Integration Points (post Phase 7.1 + Phase 8)
- `filesystem.rs` — `to_wire_bytes()` / `from_wire_bytes()` methods with `store_version` gating
- `filesystem.rs` — `compressor: Arc<dyn Compressor>` field in `SliceFsFilesystem`
- `mount.rs` — `--compressor` and `--compressor-level` CLI flags, passes compressor to filesystem
- `slicefs-compression` crate — `compress_block()`, `decompress_block()`, `Lz4Compressor`, `ZstdCompressor`, `NoneCompressor`
- `slicefs-traits` — `Compressor` trait, `AlgorithmId` enum
- `seed.rs` — may use compressor during seeding

### Established Patterns
- `store_version` currently gates behavior: `< 2` = raw (v1), `>= 2` = compressed header (v2)
- After Phase 7.1: `DictMetadataStore` uses `StoreIo` (file-backed), not in-memory Dictionary
- Write path: raw bytes → `to_wire_bytes()` → store in CAS
- Read path: CAS bytes → `from_wire_bytes()` → raw bytes to user

</code_context>

<deferred>
## Deferred Ideas

None — discussion stayed within phase scope. Segment-level compression noted as v2.1 item in PROJECT.md.

</deferred>

---

*Phase: 09-compression-removal*
*Context gathered: 2026-03-30*
