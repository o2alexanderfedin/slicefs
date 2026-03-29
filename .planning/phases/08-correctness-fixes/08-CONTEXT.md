# Phase 8: Correctness Fixes - Context

**Gathered:** 2026-03-29
**Status:** Ready for planning

<domain>
## Phase Boundary

Eliminate known v1.0 correctness bugs: refcount overflow risk (FIX-01) and statfs reporting inaccuracy (FIX-02). FIX-03/FIX-04 (snapshot O(1) lookup) are deferred — they will be structurally solved by the FileStorage migration phase being inserted before this phase.

**Note:** Phase numbering will shift. A new FileStorage Migration phase is being inserted as the new Phase 8. This correctness phase becomes Phase 9. See deferred ideas.

</domain>

<decisions>
## Implementation Decisions

### statfs — Three-Tier Space Reporting
- statfs must report **three tiers** of space information:
  1. **Logical bytes** — sum of all file sizes as users see them (already tracked via `logical_bytes()`)
  2. **CAS bytes** — total size of deduplicated block content stored in the CAS
  3. **Host disk bytes** — actual bytes consumed on the underlying OS filesystem
- **All three tiers encoded in statfs fields** for `df` visibility:
  - `blocks/bsize` = host disk capacity (total from underlying volume)
  - `bfree/bavail` = remaining host disk free space
  - `files` = actual inode count (not hardcoded 1M)
  - Used blocks derived from host disk consumption
- **Host disk capacity/free** determined by calling OS `statvfs()` on the backing store path at mount time and on each statfs call
- **Inode count** tracked via `AtomicU64` counter — increment on inode create, decrement on delete, persisted via segment WAL replay on mount
- **CAS bytes** tracked via `AtomicU64` counter — increment on block store put, decrement on GC delete. Chosen for future-proofing: survives the FileStorage migration without rework
- **Logical + CAS bytes** also exposed in `slicefs stats` CLI command for detailed dedup ratio analysis

### Refcount Saturation (FIX-01)
- Replace `+= 1` with `saturating_add` — refcount caps at `u64::MAX`
- **Log a warning** when saturation is hit — block becomes immortal (can never be GC'd)
- **Decrement is a no-op at `u64::MAX`** — once saturated, block is permanently pinned. Guarantees no data loss; GC skips it
- **`slicefs scrub` reports saturated refcounts** as warnings — "N blocks have saturated refcounts (immortal)". Gives operators visibility without treating it as corruption

### Snapshot Lookup (FIX-03/FIX-04) — DEFERRED
- O(1) snapshot lookup will be structurally solved by the FileStorage migration (filesystem-as-hashtable makes lookup inherently O(1))
- No interim HashMap fix needed — the Vec linear scan is adequate for the snapshot counts expected before FileStorage lands

### Claude's Discretion
- Exact AtomicU64 initialization strategy during segment replay (scan vs accumulate during replay)
- Whether statvfs is called on every statfs or cached with periodic refresh
- Warning log format for saturated refcounts

</decisions>

<specifics>
## Specific Ideas

- User wants all three levels of storage visibility: "total file size in slicefs, total size in our blocks, total size in the host storage"
- The existing `dict.len() * 92` formula for physical bytes is explicitly rejected — must be actual tracked values
- AtomicU64 tracking chosen specifically because it survives the FileStorage migration without rework

</specifics>

<code_context>
## Existing Code Insights

### Reusable Assets
- `logical_bytes: AtomicU64` already exists in `DictMetadataStore` (store.rs:66) — pattern for CAS bytes and inode count tracking
- `FileStorageAdd` and `file_storage_get` already exist in data-id (`blockset/src/file_storage.rs`) — implements `StorageAdd`/`StorageGet` via filesystem-as-hashtable
- `LocalDiskStore` uses hash-as-filename with 256-way directory sharding — established pattern for disk-backed hash lookup

### Established Patterns
- `Dictionary` = `BTreeMap<Digest224, Branches>` — currently held entirely in-memory in `DictMetadataStore.dict: Mutex<Dictionary>`
- `StorageAdd`/`StorageGet` traits in data-id — pluggable interface that enables Dictionary-to-FileStorage swap
- Segment replay on mount populates in-memory state — AtomicU64 counters fit this pattern naturally

### Integration Points
- `filesystem.rs:1138-1164` — statfs implementation, currently hardcodes `files = 1_000_000` and uses `dict.len() * 92`
- `store.rs:134-137` — `increment_refcount` uses `+= 1`, needs `saturating_add`
- `store.rs:139-150` — `decrement_refcount` needs guard for `u64::MAX`
- `scrub.rs` — needs new check for saturated refcount reporting

### Memory Concern (informing FileStorage priority)
- For 1 TB unique data at 4KB chunks: ~500M Dictionary entries = ~67 GB RAM
- Plus MemDedupIndex (~11 GB) and refcounts (~15 GB) = ~93 GB total
- This is the "Full DDT in RAM" anti-pattern called out in PROJECT.md out-of-scope
- FileStorage migration eliminates the Dictionary memory cost entirely

</code_context>

<deferred>
## Deferred Ideas

### FileStorage Migration (NEW PHASE — INSERT BEFORE THIS PHASE)
- **Priority: HIGHEST** — user explicitly requested this be inserted as new Phase 8
- Switch `DictMetadataStore` from in-memory `Dictionary` to file-backed `FileStorageAdd`/`file_storage_get` from data-id
- Eliminates ~67 GB RAM for 1 TB stores (Dictionary moves to disk, OS page cache handles hot nodes)
- Structurally solves FIX-03/FIX-04 (snapshot O(1) lookup via filesystem)
- data-id already has the implementation (`FileStorageAdd`, `file_storage_get`, `Io` trait)
- Requires: reworking segment replay, commit path, WAL strategy, GC, scrub, stats consumers
- **Roadmap impact:** Current Phase 8 becomes Phase 9, all subsequent phases shift by +1

</deferred>

---

*Phase: 08-correctness-fixes*
*Context gathered: 2026-03-29*
