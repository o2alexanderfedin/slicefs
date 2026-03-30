# Phase 11: Non-Sequential Write Handling - Context

**Gathered:** 2026-03-30
**Status:** Ready for planning

<domain>
## Phase Boundary

Detect non-sequential writes (pwrite at arbitrary offsets, writeback_cache reordering) and fall back from the streaming State accumulator to a Vec<u8> buffer model. The fallback must produce correct files for all access patterns: pwrite, vim, sqlite, cp --sparse. Sequential writes continue using the O(log N) streaming path from Phase 10.

</domain>

<decisions>
## Implementation Decisions

### Fallback Detection
- **Strict offset tracking**: add `next_expected_offset: u64` to OpenFileState, initialized to 0
- On each write: if `offset != next_expected_offset`, trigger fallback — zero tolerance for gaps, overlaps, or backward seeks
- After a sequential write: `next_expected_offset += data.len()`
- No gap-filling in streaming mode — any non-sequential access means the file isn't purely sequential, so fall back

### Fallback Transition (Streaming → Buffered)
- **One-way transition**: once a non-sequential write is detected, the file handle stays in Buffered mode for its entire remaining lifetime — never reverse back to Streaming
- Add `WriteMode` enum (`Streaming` | `Buffered`) to OpenFileState
- On transition: materialize current State via clone+end+file_storage_get into a Vec<u8>, then apply the non-sequential write to the buffer
- Drop the State after materialization — no need to keep both representations
- Decrement last_committed_root refcount if one exists (consistent with Phase 10 refcount lifecycle)

### Buffer Mode Behavior
- Standard pwrite semantics: write data at arbitrary offset into Vec<u8>, extending with zeros if offset > buf.len()
- On release: commit entire buffer via `State::push_all(&mut fsa, &buf)` then `state.end()` — same finalization path as Phase 10 but from materialized buffer
- **Dual dispatch on WriteMode**: test_write, test_read, test_release, flush_buffer_for_fsync, and test_setattr_size all check WriteMode and dispatch to Streaming or Buffered code path
- fsync in Buffered mode: push_all + end the buffer content, same clone+end pattern but from Vec<u8>
- truncate in Buffered mode: standard buf.resize() — simpler than Streaming truncate
- read in Buffered mode: serve directly from buf (no clone+end materialization needed)

### writeback_cache Policy
- **Not enabled by default** — sequential writes are the common case and should use the efficient streaming path
- When writeback_cache IS enabled: out-of-order FUSE write callbacks will trigger immediate fallback to Buffered mode (no attempt to reorder or tolerate small reorderings)
- This is correct behavior: writeback_cache trades ordering guarantees for throughput; our fallback ensures correctness at the cost of O(N) memory for that file handle

### Sparse File / Zero Block Optimization
- **Tree-level sparse representation**: investigate adding a "hole" node concept to the blockset Merkle tree
- A hole node encodes the length of the zero-byte sequence without storing any physical block
- Reading a hole range returns zeros without disk I/O
- No physical zero blocks stored in CAS — holes are virtual
- GC/scrub skip hole nodes (nothing to verify on disk)
- This requires changes in the blockset crate's tree/State node types
- **If feasible in scope**: implement in the blockset crate as part of this phase, so sparse files (cp --sparse, lseek+write gaps) are stored efficiently
- **If too invasive**: defer to a separate blockset enhancement phase; in the interim, zeros get stored as regular blocks (CAS block-level dedup means all-zero chunks share one physical block, which is acceptable but not optimal)

### Lock Ordering
- Detection check (offset vs next_expected_offset) happens with only open_files lock held
- If fallback triggered: acquire io lock for materialization (open_files → io, consistent with Phase 10 canonical ordering)
- In Buffered mode: writes to Vec<u8> need only open_files lock (no io access needed until flush/release)

### Claude's Discretion
- Exact WriteMode enum placement (in OpenFileState struct vs separate type)
- Whether to log/trace when fallback triggers (useful for debugging but not user-visible)
- Error handling for materialization failures during transition
- Whether flush_buffer_for_fsync in Buffered mode reuses existing infrastructure or has its own path

</decisions>

<specifics>
## Specific Ideas

- User preference: safety, precision, completeness over performance optimization — no backward compatibility concerns (no production instances exist)
- The sparse/hole representation is a user-requested optimization, not just an implementation detail — they specifically want zero-byte sequences to encode length and avoid physical storage
- Phase 10 established the clone+end pattern as the central mechanism — reuse it for the Streaming→Buffered materialization transition
- Two tests are already `#[ignore]` from Phase 10 waiting for Phase 11's STRM-02 implementation

</specifics>

<code_context>
## Existing Code Insights

### Reusable Assets
- `State` (blockset/src/tree.rs) — already has `push_bytes()`, `push_all()`, `end()`, `Clone` derive from Phase 10
- `FileStorageAdd` (blockset/src/file_storage.rs) — batched file storage with Drop impl and `extend()` disk fallback
- `file_storage_get` (blockset/src/file_storage.rs) — reads content by Digest224, used for materialization
- Clone+end+file_storage_get pattern — established in Phase 10 for fsync, read-during-write, and truncate

### Established Patterns
- `OpenFileState` (filesystem.rs:47) — State + byte_count + cas_committed + last_committed_root; will add next_expected_offset + write_mode
- `test_write()` (filesystem.rs:263) — currently always appends via push_bytes regardless of offset; comment at line 260 says "Phase 11 will add offset validation and fallback"
- `flush_buffer_for_fsync()` — clone+end pattern for Streaming mode; Buffered mode needs push_all+end equivalent
- `test_setattr_size()` — Streaming truncate with materialize+repush; Buffered truncate is simpler buf.resize()
- Lock ordering: open_files before io — established and must be maintained

### Integration Points
- `test_write()` — primary dispatch point: check WriteMode, route to push_bytes or buf write
- `test_read()` — needs Buffered path: serve from buf directly instead of clone+end
- `test_release()` — Buffered path: push_all(&buf) + end() instead of state.end()
- `flush_buffer_for_fsync()` — Buffered path: push_all + clone+end from buffer
- Two `#[ignore]` tests in streaming_tests.rs waiting for STRM-02

</code_context>

<deferred>
## Deferred Ideas

- If tree-level sparse representation proves too invasive for blockset crate during this phase, defer to a dedicated "Sparse File Optimization" phase — CAS block-level dedup provides acceptable (not optimal) interim behavior

</deferred>

---

*Phase: 11-non-sequential-write-handling*
*Context gathered: 2026-03-30*
