# Phase 8: Correctness Fixes - Context

**Gathered:** 2026-03-29
**Updated:** 2026-03-30 (post Phase 7.1 completion)
**Status:** Ready for planning

<domain>
## Phase Boundary

Eliminate known v1.0 correctness bugs: refcount overflow risk (FIX-01) and statfs reporting inaccuracy (FIX-02).

FIX-03/FIX-04 (snapshot O(1) lookup) were solved in Phase 7.1 via dual HashMap indexes. They are complete and no longer in scope for this phase.

</domain>

<decisions>
## Implementation Decisions

### statfs — Three-Tier Space Reporting (FIX-02)
- statfs must report **three tiers** of space information:
  1. **Logical bytes** — sum of all file sizes as users see them (already tracked via `logical_bytes()`)
  2. **CAS bytes** — total size of deduplicated block content (Phase 7.1 already computes this via `dir_size(&sp.join("vt0"))`)
  3. **Host disk bytes** — actual bytes consumed on the underlying OS filesystem
- **All three tiers encoded in statfs fields** for `df` visibility:
  - `blocks/bsize` = host disk capacity (total from underlying volume)
  - `bfree/bavail` = remaining host disk free space
  - `files` = actual inode count (not hardcoded 1M)
  - Used blocks derived from host disk consumption
- **Host disk capacity/free** determined by calling OS `statvfs()` on the backing store path at mount time and on each statfs call
- **Inode count** tracked via `AtomicU64` counter — increment on inode create, decrement on delete, reconstructed during mount from inode_map
- **CAS bytes** tracked via `AtomicU64` counter — increment on block store put, decrement on GC delete
- **Logical + CAS bytes** also exposed in `slicefs stats` CLI command for detailed dedup ratio analysis

### Refcount Saturation (FIX-01)
- Replace `+= 1` with `saturating_add` — refcount caps at `u64::MAX`
- **Log a warning** when saturation is hit — block becomes immortal (can never be GC'd)
- **Decrement is a no-op at `u64::MAX`** — once saturated, block is permanently pinned. Guarantees no data loss; GC skips it
- **`slicefs scrub` reports saturated refcounts** as warnings — "N blocks have saturated refcounts (immortal)". Gives operators visibility without treating it as corruption

### Claude's Discretion
- Exact AtomicU64 initialization strategy during mount (scan inode_map vs accumulate during replay)
- Whether statvfs is called on every statfs or cached with periodic refresh
- Warning log format for saturated refcounts
- How CAS bytes AtomicU64 is kept in sync (track at put/delete vs derive from dir_size on demand)

</decisions>

<specifics>
## Specific Ideas

- User wants all three levels of storage visibility: "total file size in slicefs, total size in our blocks, total size in the host storage"
- The old `dict.len() * 92` formula is already gone — Phase 7.1 replaced it with `dir_size(&sp.join("vt0"))` which is better but still not ideal (scans directory on every statfs call)
- AtomicU64 tracking chosen specifically because it's efficient and doesn't require directory scanning

</specifics>

<code_context>
## Existing Code Insights

### Reusable Assets (post Phase 7.1)
- `logical_bytes: AtomicU64` already exists in `DictMetadataStore` (store.rs) — established pattern for CAS bytes and inode count tracking
- `StoreIo` in metadata crate — wraps backing store path, could provide `statvfs()` access
- `dir_size()` helper already used in `statfs` — computes vt0/ directory size (current CAS bytes approach)

### Established Patterns (post Phase 7.1)
- `DictMetadataStore` now holds `io: Arc<Mutex<StoreIo>>` instead of `Mutex<Dictionary>`
- `SliceFsFilesystem` holds `io: Arc<Mutex<StoreIo>>` (shared with metadata store)
- Snapshots use dual HashMap indexes (`snapshots_by_version`, `snapshots_by_name`) — FIX-03/FIX-04 complete
- Refcounts still stored in `Mutex<HashMap<Digest224, u64>>` — unchanged from v1.0

### Integration Points (current line numbers approximate after Phase 7.1)
- `filesystem.rs` statfs — still hardcodes `files = 1_000_000`, uses `dir_size()` for physical, `bfree = u64::MAX / 4`
- `store.rs` `increment_refcount` — still uses `+= 1`, needs `saturating_add`
- `store.rs` `decrement_refcount` — needs guard for `u64::MAX`
- `scrub.rs` — needs new check for saturated refcount reporting
- `stats.rs` — needs to expose three-tier reporting in CLI output

</code_context>

<deferred>
## Deferred Ideas

None — FileStorage Migration (previously deferred) completed as Phase 7.1. FIX-03/FIX-04 resolved there.

</deferred>

---

*Phase: 08-correctness-fixes*
*Context gathered: 2026-03-29, updated 2026-03-30*
