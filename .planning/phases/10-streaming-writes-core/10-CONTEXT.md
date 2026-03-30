# Phase 10: Streaming Writes Core - Context

**Gathered:** 2026-03-29
**Status:** Ready for planning

<domain>
## Phase Boundary

Replace the `Vec<u8>` write buffer in `OpenFileState` with blockset's incremental `State::push_bytes()` API so that sequential file writes use O(log N) memory regardless of file size. Also: fsync mid-stream commits, truncate on open streaming handle, and read-during-write correctness. Non-sequential write fallback is Phase 11.

</domain>

<decisions>
## Implementation Decisions

### OpenFileState Redesign
- Replace `buf: Vec<u8>` with `state: State` (from blockset's tree.rs) — created immediately on open/create via `State::default()`
- One code path: every write goes through `state.push_bytes()` from byte 0 — no threshold-based promotion
- Add `byte_count: u64` field to track total bytes written (incremented on each `push_bytes` call) — no State introspection needed
- Keep `cas_committed: bool` guard, adapted to State-based flow — after fsync commits and State is reset, release() must not overwrite manifest with empty tree

### fsync Mid-Stream
- **Clone + end pattern**: clone the in-progress State, call `end()` on the clone to get Digest224, set manifest, keep original State alive for further writes
- Requires adding `#[derive(Clone)]` to `State` in data-id — State is a struct of Digest256 values and Vec-like accumulator, all cloneable
- After fsync, the original State continues accumulating bytes — no reset, no fresh State
- **Refcount on fsync**: increment refcount immediately when fsync commits a root
- **Track last committed root**: store last committed Digest224 in OpenFileState; when a new root is committed (next fsync or release), decrement the old one — prevents refcount leaks

### Read-During-Write (STRM-03)
- **Clone + end + file_storage_get**: clone the writer's in-progress State, call end() on clone, read bytes from materialized tree via `file_storage_get`
- No caching of materialized result — materialize fresh on each read(). Always correct, writes between reads automatically reflected
- **Cross-handle reads**: read() on a DIFFERENT file handle (read-only) on the same inode also sees uncommitted writes — look up all open write handles for the inode, clone+end the writer's State. Requires inode-to-fh lookup
- **Temporary tree nodes**: left for GC mark-and-sweep to reclaim as unreachable — zero extra cleanup code

### Truncate on Open Streaming Handle (STRM-05)
- **Discard State, start fresh**:
  - `new_size == 0`: drop State, reset to `State::default()` with `byte_count = 0`
  - `new_size > 0`: materialize current content via clone+end+file_storage_get, truncate/extend the Vec, then `push_all` into a fresh State
- **Materialize from in-progress State** (clone+end), not from last fsync'd manifest — captures ALL bytes written so far, including uncommitted
- **Defer manifest update** to next fsync/release — consistent with normal write buffering semantics. Crash after truncate but before fsync reverts to pre-truncate state
- **Decrement old committed root** if one exists from a prior fsync — consistent with fsync refcount tracking

### Claude's Discretion
- Exact inode-to-fh reverse lookup structure (HashMap<u64, Vec<u64>> or scan open_files)
- Whether State::default() needs explicit initialization or is already usable
- Error handling details for clone+end materialization failures
- Lock ordering for io Mutex during clone+end+read operations

</decisions>

<specifics>
## Specific Ideas

- User chose safety and precision over optimization: no caches, no thresholds, no deferred refcounting
- The clone+end pattern is the central mechanism — used by fsync, read-during-write, AND truncate materialization
- State must derive Clone — this is a prerequisite change in data-id's blockset crate
- Lock ordering concern from STATE.md: "Dict lock acquisition sequence during flush_buffer_to_cas needs canonical ordering to prevent lock inversion" — must be verified during planning

</specifics>

<code_context>
## Existing Code Insights

### Reusable Assets
- `State` (data-id/blockset/src/tree.rs) — implements `Tree` trait with `push_bytes()`, `end()`, `push_all()`. Needs `#[derive(Clone)]`
- `FileStorageAdd` (data-id/blockset/src/file_storage.rs) — batched file storage, already used in flush_buffer_to_cas
- `file_storage_get` (data-id/blockset/src/file_storage.rs) — reads content by Digest224, used for read path
- `StoreIo` (metadata/src/store_io.rs) — file-backed Io implementation, shared via `Arc<Mutex<StoreIo>>`

### Established Patterns
- `flush_buffer_to_cas()` (filesystem.rs:376) — current write-commit logic: `State::push_all(&mut fsa, &buf)`. Will be rewritten to use `state.clone().end()`
- `flush_buffer_for_fsync()` (filesystem.rs:316) — current fsync: takes Vec, pushes via push_all, resets. Will use clone+end instead
- `cas_committed` bool guard — prevents release() from overwriting committed manifest with empty data
- Refcount increment on commit, decrement on overwrite — established in existing flush paths

### Integration Points
- `OpenFileState` (filesystem.rs:47) — struct to be redesigned: `buf: Vec<u8>` → `state: State`, add `byte_count: u64`
- `test_write()` (filesystem.rs:244) — `buf[offset..end].copy_from_slice(data)` → `state.push_bytes(&mut fsa, data)` (offset validation for sequential-only in Phase 10)
- `test_release()` (filesystem.rs:259) — `flush_buffer_to_cas(ino, buf)` → `state.end(&mut fsa)` based flow
- `test_setattr_size()` (filesystem.rs:409) — truncate on open handle: `buf.resize(new_size, 0)` → materialize+re-push or reset
- FUSE `read()` — currently serves from CAS manifest only; must check for open write handles on same inode

</code_context>

<deferred>
## Deferred Ideas

None — discussion stayed within phase scope.

</deferred>

---

*Phase: 10-streaming-writes-core*
*Context gathered: 2026-03-29*
