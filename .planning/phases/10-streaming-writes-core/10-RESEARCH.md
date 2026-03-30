# Phase 10: Streaming Writes Core - Research

**Researched:** 2026-03-29
**Domain:** Rust / FUSE / blockset Merkle tree streaming writes
**Confidence:** HIGH

---

<user_constraints>
## User Constraints (from CONTEXT.md)

### Locked Decisions

**OpenFileState Redesign**
- Replace `buf: Vec<u8>` with `state: State` (from blockset's tree.rs) — created immediately on open/create via `State::default()`
- One code path: every write goes through `state.push_bytes()` from byte 0 — no threshold-based promotion
- Add `byte_count: u64` field to track total bytes written (incremented on each `push_bytes` call) — no State introspection needed
- Keep `cas_committed: bool` guard, adapted to State-based flow — after fsync commits and State is reset, release() must not overwrite manifest with empty tree

**fsync Mid-Stream**
- Clone + end pattern: clone the in-progress State, call `end()` on the clone to get Digest224, set manifest, keep original State alive for further writes
- Requires adding `#[derive(Clone)]` to `State` in data-id — State is a struct of Digest256 values and Vec-like accumulator, all cloneable
- After fsync, the original State continues accumulating bytes — no reset, no fresh State
- Refcount on fsync: increment refcount immediately when fsync commits a root
- Track last committed root: store last committed Digest224 in OpenFileState; when a new root is committed (next fsync or release), decrement the old one — prevents refcount leaks

**Read-During-Write (STRM-03)**
- Clone + end + file_storage_get: clone the writer's in-progress State, call end() on clone, read bytes from materialized tree via `file_storage_get`
- No caching of materialized result — materialize fresh on each read(). Always correct, writes between reads automatically reflected
- Cross-handle reads: read() on a DIFFERENT file handle (read-only) on the same inode also sees uncommitted writes — look up all open write handles for the inode, clone+end the writer's State. Requires inode-to-fh lookup
- Temporary tree nodes: left for GC mark-and-sweep to reclaim as unreachable — zero extra cleanup code

**Truncate on Open Streaming Handle (STRM-05)**
- new_size == 0: drop State, reset to `State::default()` with `byte_count = 0`
- new_size > 0: materialize current content via clone+end+file_storage_get, truncate/extend the Vec, then `push_all` into a fresh State
- Materialize from in-progress State (clone+end), not from last fsync'd manifest — captures ALL bytes written so far, including uncommitted
- Defer manifest update to next fsync/release — consistent with normal write buffering semantics. Crash after truncate but before fsync reverts to pre-truncate state
- Decrement old committed root if one exists from a prior fsync — consistent with fsync refcount tracking

### Claude's Discretion
- Exact inode-to-fh reverse lookup structure (HashMap<u64, Vec<u64>> or scan open_files)
- Whether State::default() needs explicit initialization or is already usable
- Error handling details for clone+end materialization failures
- Lock ordering for io Mutex during clone+end+read operations

### Deferred Ideas (OUT OF SCOPE)
None — discussion stayed within phase scope.
</user_constraints>

---

<phase_requirements>
## Phase Requirements

| ID | Description | Research Support |
|----|-------------|-----------------|
| STRM-01 | Sequential file writes use State::push_bytes() incrementally — O(log N) memory regardless of file size | State type is `Vec<Level>` where Level = `(MerkleTreeState, Digest256)`, both are Vec-based, total accumulator size grows O(log N) with file size — confirmed by source inspection |
| STRM-03 | Read-during-write on an open streaming file handle returns correct content (clone+end materialization) | State is `Vec<Level>` = `Vec<(Vec<Digest256>, Digest256)>`, all fields are Clone — derive(Clone) is trivially addable; clone+end+file_storage_get is the proven read path |
| STRM-04 | fsync() mid-stream commits current State, resets streaming state for subsequent writes | Current `flush_buffer_for_fsync` uses `mem::take` on buf and resets to empty Vec; new version clones State, ends the clone, keeps original alive — no structural barrier |
| STRM-05 | Truncate on an open streaming file handle resets State and adjusts inode size atomically | `test_setattr_size` currently does `buf.resize(new_size, 0)`; new version must do clone+end+file_storage_get+resize+push_all_into_fresh_State |
</phase_requirements>

---

## Summary

Phase 10 replaces the flat `Vec<u8>` write buffer in `OpenFileState` with blockset's incremental `State` type, enabling the filesystem to write arbitrarily large files with O(log N) memory. The `State` type is `Vec<Level>` where `Level = (MerkleTreeState, Digest256)` and `MerkleTreeState = Vec<Digest256>`, making all fields trivially cloneable. The central mechanism used by all three advanced operations — fsync mid-stream, read-during-write, and truncate on open handle — is the clone+end pattern: `state.clone().end(&mut fsa)` produces a `Digest256` snapshot without disturbing the live accumulator.

The key prerequisite change is adding `#[derive(Clone)]` to the `State` type alias in `content_dependant_tree.rs`. Since `State = Vec<Level>` and `Level = (MerkleTreeState, Digest256)` and `MerkleTreeState = Vec<Digest256>`, and all `Digest256` values are `[u32; 8]` arrays (which are Copy+Clone), this derive is a one-line change with no risk. All three plans have clear, well-bounded implementation surface: Plan 01 (OpenFileState redesign), Plan 02 (flush/fsync/read rewrite), Plan 03 (truncate + integration tests).

The single most important concurrency concern is lock ordering: `open_files: Mutex<HashMap>` must always be released before acquiring `io: Arc<Mutex<StoreIo>>`. The clone+end pattern makes this natural — clone the State while holding `open_files` lock, then drop the lock, then acquire `io` lock for the `FileStorageAdd` operations. The existing `flush_buffer_for_fsync` already demonstrates the correct pattern (`mem::take` under `open_files` lock, then IO operations outside it).

**Primary recommendation:** Add `#[derive(Clone)]` to `State` in Plan 01 as the very first commit, then rewrite `OpenFileState` and all flush/read/truncate paths in the three plans as planned.

---

## Standard Stack

### Core

| Library | Version | Purpose | Why Standard |
|---------|---------|---------|--------------|
| `blockset::State` | workspace | Incremental Merkle tree accumulator for streaming | The existing production accumulator; push_bytes already flushes tree nodes to disk as they complete, O(log N) total live memory |
| `blockset::FileStorageAdd` | workspace | Batch file-backed StorageAdd for tree node writes | Already used in all flush paths; named as the standard I/O adapter |
| `blockset::file_storage_get` | workspace | Read content by Digest224 from file storage | Already used in `test_read`; proven correct for content retrieval |
| `blockset::Tree` | workspace | Trait providing push_bytes, push_all, end | All push_bytes/end calls require this trait in scope |
| `std::sync::{Arc, Mutex}` | std | Shared mutable state across FUSE callbacks | Already established pattern throughout filesystem.rs |

### Supporting

| Library | Version | Purpose | When to Use |
|---------|---------|---------|-------------|
| `std::collections::HashMap` | std | Inode-to-fh reverse index | For cross-handle read-during-write lookup (Claude's discretion) |
| `blockset::digest224::Digest224` | workspace | Root digest type returned by `end()` on FSA | Stored in `last_committed_root: Option<Digest224>` for refcount tracking |

### Alternatives Considered

| Instead of | Could Use | Tradeoff |
|------------|-----------|----------|
| Clone + end (read snapshot) | Separate committed-only read path | Committed-only misses uncommitted bytes written since last fsync — violates STRM-03 |
| Inode-to-fh HashMap | Scan open_files for matching ino | Scan is O(n open handles), acceptable for typical handle counts; HashMap is O(1) but adds update complexity on open/release |
| Defer manifest update on truncate | Immediate manifest write | Deferred is consistent with normal write semantics; immediate would require a full push_all that may fail mid-write |

**Installation:** No new dependencies — all libraries are already in the workspace.

---

## Architecture Patterns

### Recommended Project Structure

No structural file additions needed. All changes are within:

```
crates/
├── data-id/blockset/src/
│   └── content_dependant_tree.rs   # Add #[derive(Clone)] to State
├── slicefs-cli/src/
│   └── filesystem.rs               # OpenFileState, write/fsync/read/truncate paths
└── slicefs-cli/tests/
    └── streaming_tests.rs          # New: STRM-01/03/04/05 integration tests (Plan 03)
```

### Pattern 1: Clone + End Snapshot

**What:** Snapshot an in-progress `State` accumulator without disturbing it by cloning and calling `end()` on the clone with a temporary `FileStorageAdd`.

**When to use:** Whenever the current streaming content must be materialized (read-during-write, fsync, truncate materialization) without stopping further writes.

**Lock ordering rule:** Always clone the State while holding the `open_files` Mutex, then release `open_files` before acquiring the `io` Mutex for `FileStorageAdd`.

```rust
// Source: derived from existing flush_buffer_for_fsync pattern + CONTEXT.md decisions

// Step 1: clone state while holding open_files lock
let state_snapshot = {
    let open_files = self.open_files.lock().unwrap();
    open_files.get(&fh).map(|s| s.state.clone())
};

// Step 2: materialize outside open_files lock (io lock acquired here)
if let Some(snapshot) = state_snapshot {
    let content_digest: Digest224 = {
        let mut io = self.io.lock().unwrap();
        let mut fsa = FileStorageAdd::new(&mut *io);
        let d256 = snapshot.end(&mut fsa);
        fsa.end(&d256)          // produces Digest224 (top-level file node)
    };
    // use content_digest for manifest, read, or refcount
}
```

Note: `State::end()` returns `Digest256` (internal tree root), but `FileStorageAdd::end(&d256)` finalizes the top-level file node and returns `Digest224` (the manifest key). The existing `flush_buffer_for_fsync` calls `State::push_all(&mut fsa, &buf)` which internally does both steps. With the clone+end pattern, these two steps must be explicit.

### Pattern 2: OpenFileState with Streaming Fields

**What:** Redesigned struct holding streaming accumulator, byte count, committed root tracking.

**When to use:** All file write handles in `open_files`.

```rust
// Source: CONTEXT.md decisions + existing OpenFileState structure

struct OpenFileState {
    ino: u64,
    state: State,               // replaces: buf: Vec<u8>
    byte_count: u64,            // total bytes pushed via push_bytes
    cas_committed: bool,        // true after fsync commits; false after new write
    last_committed_root: Option<Digest224>, // for decrement-on-overwrite
}
```

### Pattern 3: fsync Mid-Stream with Refcount Lifecycle

**What:** Commit current streaming state without stopping writes; track previous root for safe decrement.

**When to use:** `flush_buffer_for_fsync` rewrite and `test_fsync`.

```rust
// Source: CONTEXT.md fsync decisions

fn flush_streaming_for_fsync(&self, ino: u64, fh: u64) -> Result<(), i32> {
    // 1. Clone state snapshot (hold open_files lock minimally)
    let (state_snapshot, byte_count, old_root) = {
        let open_files = self.open_files.lock().unwrap();
        let s = open_files.get(&fh).ok_or(libc::EBADF)?;
        (s.state.clone(), s.byte_count, s.last_committed_root)
    };

    // 2. Materialize without holding open_files lock
    let new_digest = {
        let mut io = self.io.lock().unwrap();
        let mut fsa = FileStorageAdd::new(&mut *io);
        let d256 = state_snapshot.end(&mut fsa);
        fsa.end(&d256)
    };

    // 3. Decrement old committed root (if any)
    if let Some(old) = old_root {
        self.meta.decrement_refcount(&old);
    }

    // 4. Update manifest, increment new root refcount
    self.meta.set_manifest(ino, &[new_digest]).map_err(|_| libc::EIO)?;
    self.meta.increment_refcount(&new_digest);

    // 5. Update tracking fields
    {
        let mut open_files = self.open_files.lock().unwrap();
        if let Some(s) = open_files.get_mut(&fh) {
            s.cas_committed = true;
            s.last_committed_root = Some(new_digest);
        }
    }

    // 6. Update inode size
    let mut inode = self.meta.get_inode(ino).map_err(|_| libc::EIO)?;
    inode.size = byte_count;
    // ... update timestamps, call update_inode
    Ok(())
}
```

### Pattern 4: Read-During-Write (STRM-03)

**What:** `test_read` checks open write handles for the inode before falling back to manifest; materializes uncommitted content via clone+end.

**When to use:** All read paths — both same-handle and cross-handle reads.

```rust
// Source: CONTEXT.md STRM-03 decisions

pub fn test_read(&self, ino: u64, offset: u64, size: u32) -> Result<Vec<u8>, i32> {
    // Check for open write handles on this inode
    let writer_state: Option<(State, u64)> = {
        let open_files = self.open_files.lock().unwrap();
        open_files.values()
            .find(|s| s.ino == ino && s.byte_count > 0)
            .map(|s| (s.state.clone(), s.byte_count))
    };

    let raw_bytes: Vec<u8> = if let Some((snapshot, _byte_count)) = writer_state {
        // Materialize uncommitted content
        let mut io = self.io.lock().unwrap();
        let mut fsa = FileStorageAdd::new(&mut *io);
        let d256 = snapshot.end(&mut fsa);
        let digest224 = fsa.end(&d256);
        drop(io);
        let mut io2 = self.io.lock().unwrap();
        file_storage_get(&mut *io2, &digest224).ok_or(libc::EIO)?
    } else {
        // Fall back to committed manifest
        let manifest = self.meta.get_manifest(ino).map_err(|_| libc::EIO)?;
        if manifest.is_empty() { return Ok(vec![]); }
        let mut io = self.io.lock().unwrap();
        file_storage_get(&mut *io, &manifest[0]).ok_or(libc::EIO)?
    };

    let start = (offset as usize).min(raw_bytes.len());
    let end = (start + size as usize).min(raw_bytes.len());
    Ok(raw_bytes[start..end].to_vec())
}
```

Note on the io lock above: `FileStorageAdd::new(&mut *io)` borrows `io` mutably for the lifetime of `fsa`. When `fsa.end()` writes the top node, the borrow ends with `drop(fsa)` (implicit at end of block). The `file_storage_get` call then needs a fresh lock. This can be collapsed if the Digest224 is captured before dropping — verify during implementation.

### Pattern 5: Truncate on Open Streaming Handle

**What:** For `new_size == 0` reset to fresh State; for `new_size > 0` materialize, resize, push_all into fresh State.

**When to use:** `test_setattr_size` when open write handle exists.

```rust
// Source: CONTEXT.md STRM-05 decisions

// new_size == 0: reset (fast path)
if new_size == 0 {
    let mut open_files = self.open_files.lock().unwrap();
    if let Some(s) = open_files.get_mut(&fh_val) {
        if let Some(old_root) = s.last_committed_root.take() {
            self.meta.decrement_refcount(&old_root);  // NB: needs open_files released first
        }
        s.state = State::default();
        s.byte_count = 0;
        s.cas_committed = false;
    }
}

// new_size > 0: materialize, resize, re-push
// (1) clone state under lock, (2) materialize under io lock, (3) resize Vec,
// (4) push_all into fresh State, (5) update OpenFileState fields
```

### Anti-Patterns to Avoid

- **Holding open_files and io locks simultaneously:** Deadlock risk. Always release `open_files` before acquiring `io`. The clone+end pattern makes this natural.
- **Calling `State::end()` on the live state (not a clone):** Consumes the accumulator, terminating the stream. Only call `end()` on clones for snapshots.
- **Using `byte_count` as the State's internal byte count:** State does not expose a byte count — `byte_count` in OpenFileState is the only source of truth for inode size during streaming.
- **Resetting State on fsync:** After fsync the original State must continue accumulating. Only the clone is consumed by `end()`.
- **Setting manifest to `&[]` on release when cas_committed=true and byte_count > 0:** The `cas_committed` guard prevents this but must be verified for the streaming case: the guard must check `last_committed_root.is_some()` not just `state.is_empty()`.

---

## Don't Hand-Roll

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|-------------|-----|
| Streaming content-addressed Merkle tree | Custom incremental hasher | `State::push_bytes()` | Handles content-dependent chunking, tree balancing, and node serialization — thousands of lines of battle-tested code |
| File content reading from CAS | Custom tree walker | `file_storage_get` | Already handles Part2/Part3 node format, internal vs top-level node routing, and lazy tree traversal |
| Snapshot of in-progress tree | Rolling checkpoint structure | `state.clone().end(&mut fsa)` | Clone is O(log N) — just copy the accumulator; end() is the single-pass finalizer |
| Refcount lifecycle | Custom reference counting | `meta.increment_refcount()` / `meta.decrement_refcount()` | Already handles saturation at u64::MAX and all persistence concerns |

**Key insight:** The entire complexity of streaming writes is already encoded in the `State` type. Phase 10 is wiring, not building.

---

## Common Pitfalls

### Pitfall 1: State is Vec<Level> — Clone Must Be Derived, Not Implemented

**What goes wrong:** Attempting to implement `Clone` manually or copying fields individually instead of adding `#[derive(Clone)]`.

**Why it happens:** `State` is a type alias (`pub type State = Vec<Level>`) defined in `content_dependant_tree.rs`. Since it is a type alias (not a newtype), `#[derive(Clone)]` cannot be applied to the alias itself — but `Vec<Level>` already implements Clone if `Level: Clone`. Level is `(MerkleTreeState, Digest256)` = `(Vec<Digest256>, Digest256)`, which is Clone. So `State` (= `Vec<Level>`) already implements Clone without any derive.

**How to avoid:** Verify at compile time that `let _: State = state.clone();` compiles. No derive annotation needed — Vec<T: Clone> is already Clone.

**Warning signs:** Compiler error "the method `clone` exists but the following trait bounds are not satisfied" — would mean Level is not Clone; check Digest256 type.

### Pitfall 2: end() Requires a Mutable StorageAdd — Cannot Reuse FSA Borrow

**What goes wrong:** Trying to call both `snapshot.end(&mut fsa)` and `file_storage_get(&mut *io, ...)` within the same borrowed scope of `io`.

**Why it happens:** `FileStorageAdd` borrows `io` mutably for its lifetime. `file_storage_get` also requires a mutable `Io` reference. Both cannot hold `&mut io` simultaneously.

**How to avoid:** Drop `fsa` (end its borrow) before calling `file_storage_get`. Capture the `Digest224` result before releasing `fsa`. In practice: end the block containing `fsa` to drop it, then re-acquire or reuse `io` for `file_storage_get`.

**Warning signs:** Rust borrow checker error "cannot borrow `*io` as mutable because it is also borrowed as mutable".

### Pitfall 3: release() Writes Empty Manifest Over Committed Data

**What goes wrong:** After the last fsync before release, `byte_count > 0` but `state` is still a live accumulator (not empty). release() calls `state.end(&mut fsa)` and gets the same digest as the last fsync — OR worse, if state accumulated more bytes since fsync, it produces a new tree that was never refcounted.

**Why it happens:** The `cas_committed` guard was designed for the Vec<u8> world where the buffer was emptied on fsync. In the streaming world, state is never reset — `cas_committed=true` plus `state` being non-default is the normal post-fsync state.

**How to avoid:** On release, always call `state.clone().end(&mut fsa)` (or `state.end()` since state is consumed on release) to produce the final Digest224. Then: if `last_committed_root == Some(new_digest)` (i.e., state hasn't changed since last fsync), skip the manifest write but still ensure refcount is correct. If different, write new manifest, increment new, decrement old.

**Warning signs:** Read after release returns empty file despite data being written. Refcount leak when multiple fsyncs occur before release.

### Pitfall 4: Lock Inversion Between open_files and io

**What goes wrong:** Code path A holds `open_files` lock then acquires `io` lock; code path B holds `io` lock then acquires `open_files` lock — classic deadlock.

**Why it happens:** FUSE callbacks can arrive concurrently. If GC or a read callback acquires `io` first and then tries to inspect open_files, while a write callback holds open_files and is materializing via io, both block forever.

**How to avoid:** Establish canonical ordering: always `open_files` before `io`, never the reverse. Clone state while holding `open_files`, release `open_files`, then acquire `io`. Never hold both simultaneously.

**Warning signs:** Intermittent deadlock under concurrent write+read or write+gc load. Hangs during FUSE stress tests.

### Pitfall 5: inode-to-fh Scan Race During read-During-Write

**What goes wrong:** The scan of `open_files` to find write handles for a given inode may miss handles opened after the scan begins, or may clone state from a handle that is concurrently being released.

**Why it happens:** Without inode-to-fh reverse index, the implementation must scan `open_files` under lock. The lock protects the scan but does not prevent a handle from being released between the scan and the materialization.

**How to avoid:** Clone the State under the lock in the same critical section as the scan. By the time the lock is released and `io` is acquired for materialization, the clone is independent — the original handle may be released without affecting the snapshot.

**Warning signs:** Panic from unwrapping a None handle during read; stale/missing data in read-during-write tests.

---

## Code Examples

Verified patterns from source inspection:

### State Type Resolution (content_dependant_tree.rs)

```rust
// Source: crates/data-id/blockset/src/content_dependant_tree.rs (verified)
// State is a Vec<Level>, Level = (MerkleTreeState, Digest256)
// MerkleTreeState = Vec<Digest256> (from merkle_tree.rs)
// Digest256 = sha2_compress::Hash<u32> = [u32; 8] (Copy + Clone)
// Therefore: State = Vec<(Vec<[u32;8]>, [u32;8])> — fully Clone without any derive
pub type State = Vec<Level>;
type Level = (MerkleTreeState, Digest256);
// MerkleTreeState = Vec<Digest256>;
```

### Current flush_buffer_to_cas (filesystem.rs:376 — to be replaced)

```rust
// Source: crates/slicefs-cli/src/filesystem.rs (verified)
fn flush_buffer_to_cas(&self, ino: u64, buf: Vec<u8>) -> Result<(), i32> {
    let content_digest = {
        let mut io = self.io.lock().unwrap();
        let mut fsa = FileStorageAdd::new(&mut *io);
        let digest = State::push_all(&mut fsa, &buf);   // push_all = push_bytes + end + fsa.end
        drop(fsa);
        digest
    };
    self.meta.set_manifest(ino, &[content_digest]).map_err(|_| libc::EIO)?;
    self.meta.increment_refcount(&content_digest);
    // ... update inode size to buf.len()
}
```

### FileStorageAdd::end() — How Digest256 Becomes Digest224

```rust
// Source: crates/data-id/blockset/src/file_storage.rs (verified)
// fsa.end(digest: &Digest256) -> Digest224
// SHA224 compresses digest+EMPTY, takes first 7 bytes as Digest224
// Writes the top-level file node to vt0/<base32(name)>
impl<'a, T: Io> StorageAdd for FileStorageAdd<'a, T> {
    fn end(&mut self, digest: &Digest256) -> Digest224 {
        // name = SHA224(digest || EMPTY)[..7]
        // write top node to vt0/<base32(name)>
        // returns name as Digest224
    }
}
```

### State::push_all (tree.rs) — Shows Composition

```rust
// Source: crates/data-id/blockset/src/tree.rs (verified)
fn push_all(storage: &mut impl StorageAdd, v: &[u8]) -> Digest224 {
    let i = Self::push_all_internal(storage, v);  // push_bytes + end -> Digest256
    storage.end(&i)                                 // fsa.end -> Digest224
}
```

For the clone+end pattern, replicate this manually:
```rust
let d256: Digest256 = state_clone.end(&mut fsa);   // Tree::end on Vec<Level>
let d224: Digest224 = fsa.end(&d256);              // FileStorageAdd::end -> top node
```

### Existing OpenFileState (filesystem.rs:47 — to be replaced)

```rust
// Source: crates/slicefs-cli/src/filesystem.rs (verified)
struct OpenFileState {
    ino: u64,
    buf: Vec<u8>,
    cas_committed: bool,
}
// Current write: state.buf[offset..end].copy_from_slice(data);
// Current fsync: mem::take(&mut state.buf) -> push_all -> reset
```

---

## State of the Art

| Old Approach | Current Approach | When Changed | Impact |
|--------------|------------------|--------------|--------|
| In-memory `Vec<u8>` buffer | `State::push_bytes()` incremental Merkle accumulator | Phase 10 (this phase) | O(log N) memory vs O(N); 10 GB file on 512 MB RAM becomes possible |
| `flush_buffer_for_fsync` resets buffer to `Vec::new()` | Clone+end on fsync, keep State alive | Phase 10 | Subsequent writes after fsync continue correctly without re-push |
| read() serves only committed manifest | read() checks open write handles first | Phase 10 | read-during-write returns current bytes, not just last fsync snapshot |
| truncate resizes `buf: Vec<u8>` directly | truncate materializes+resizes+repushes into fresh State | Phase 10 | Truncate works on handles mid-stream |

**Deprecated/outdated after Phase 10:**
- `buf: Vec<u8>` in OpenFileState: replaced by `state: State`
- `flush_buffer_for_fsync` in its current form: rewritten to clone+end
- `flush_buffer_to_cas` taking a `Vec<u8>`: replaced by State-based release path

---

## Open Questions

1. **FileStorageAdd borrow scope in clone+end+file_storage_get**
   - What we know: `FileStorageAdd<'a, T>` borrows `T: Io` mutably for lifetime `'a`. `file_storage_get` also takes `&mut impl Io`. Both need `&mut StoreIo`.
   - What's unclear: Whether the compiler will allow creating `fsa`, calling `fsa.end()`, then calling `file_storage_get` on the same `io` within a single locked region — or whether the borrow must be released between the two calls.
   - Recommendation: Structure as two sub-blocks: `let d224 = { let mut fsa = FSA::new(&mut *io); ... fsa.end(&d256) };` then immediately `file_storage_get(&mut *io, &d224)` within the same outer `io.lock()` scope. The borrow of `fsa` ends when the inner block closes, before `file_storage_get` begins. This should compile.

2. **inode-to-fh reverse lookup: scan vs HashMap**
   - What we know: Both approaches work. Scan is O(n open handles), HashMap is O(1) but adds a second data structure to keep consistent with open_files.
   - What's unclear: Typical concurrent handle count during FUSE stress tests.
   - Recommendation: Start with scan (simpler, zero additional state). Switch to HashMap if profiling shows open handle counts exceeding ~100 during tests. For a local filesystem, scan is acceptable.

3. **release() correctness when State was never pushed (zero-byte file)**
   - What we know: `State::default()` is `Vec::new()`. `Vec::new().end(&mut fsa)` returns `EMPTY` (verified from test_state_empty). `fsa.end(&EMPTY)` behavior not explicitly verified.
   - What's unclear: Whether `fsa.end(&EMPTY)` writes a zero-byte file node or errors.
   - Recommendation: Keep the existing empty-file fast path: `if byte_count == 0 { set_manifest(ino, &[]) }` — skip CAS entirely for empty files, matching the current `flush_buffer_to_cas` empty check.

---

## Validation Architecture

### Test Framework

| Property | Value |
|----------|-------|
| Framework | Rust built-in test harness (`cargo test`) |
| Config file | `crates/slicefs-cli/Cargo.toml` (test integration via `[[test]]` entries) |
| Quick run command | `cargo test -p slicefs-cli --test write_path_tests 2>&1 \| tail -20` |
| Full suite command | `cargo test -p slicefs-cli 2>&1 \| tail -40` |

### Phase Requirements → Test Map

| Req ID | Behavior | Test Type | Automated Command | File Exists? |
|--------|----------|-----------|-------------------|-------------|
| STRM-01 | Writing 10 GB file completes on 512 MB RAM — no OOM | integration | `cargo test -p slicefs-cli --test streaming_tests test_large_file_sequential_no_oom -- --nocapture` | ❌ Wave 0 |
| STRM-01 | Sequential writes use push_bytes O(log N) memory | unit | `cargo test -p slicefs-cli --test streaming_tests test_push_bytes_memory_bounded` | ❌ Wave 0 |
| STRM-03 | Same-handle read-during-write returns current bytes | integration | `cargo test -p slicefs-cli --test streaming_tests test_read_during_write_same_handle` | ❌ Wave 0 |
| STRM-03 | Cross-handle read sees uncommitted writes | integration | `cargo test -p slicefs-cli --test streaming_tests test_read_during_write_cross_handle` | ❌ Wave 0 |
| STRM-04 | fsync mid-stream commits bytes; subsequent writes correct | integration | `cargo test -p slicefs-cli --test streaming_tests test_fsync_midstream_then_write` | ❌ Wave 0 |
| STRM-04 | Multiple fsyncs do not leak refcounts | integration | `cargo test -p slicefs-cli --test streaming_tests test_fsync_refcount_no_leak` | ❌ Wave 0 |
| STRM-05 | Truncate to 0 on open handle resets State | integration | `cargo test -p slicefs-cli --test streaming_tests test_truncate_to_zero_on_open_handle` | ❌ Wave 0 |
| STRM-05 | Truncate to N>0 on open handle materializes correctly | integration | `cargo test -p slicefs-cli --test streaming_tests test_truncate_midstream_nonzero` | ❌ Wave 0 |
| STRM-05 | Truncate decrements old committed root | integration | `cargo test -p slicefs-cli --test streaming_tests test_truncate_decrements_refcount` | ❌ Wave 0 |

### Sampling Rate

- **Per task commit:** `cargo test -p slicefs-cli 2>&1 | tail -30`
- **Per wave merge:** `cargo test -p slicefs-cli 2>&1 | tail -30`
- **Phase gate:** Full suite green before `/gsd:verify-work`

### Wave 0 Gaps

- [ ] `crates/slicefs-cli/tests/streaming_tests.rs` — covers STRM-01, STRM-03, STRM-04, STRM-05 (all 9 tests above)

*(Framework and existing test infrastructure are already in place — only the new test file is missing.)*

---

## Sources

### Primary (HIGH confidence)

- `crates/data-id/blockset/src/content_dependant_tree.rs` — State type definition, push_digest implementation, test_state_empty confirming Vec::new().end() = EMPTY
- `crates/data-id/blockset/src/merkle_tree.rs` — MerkleTreeState = Vec<Digest256>, confirming Clone derivability
- `crates/data-id/blockset/src/tree.rs` — Tree trait: push_bytes, end, push_all source
- `crates/data-id/blockset/src/file_storage.rs` — FileStorageAdd implementation, file_storage_get, end() producing Digest224
- `crates/slicefs-cli/src/filesystem.rs` — OpenFileState, flush_buffer_to_cas, flush_buffer_for_fsync, test_read, test_setattr_size (full implementation read)
- `crates/slicefs-cli/tests/fsync_tests.rs` — existing test patterns for fsync lifecycle
- `crates/slicefs-cli/tests/write_path_tests.rs` — existing test helper patterns (fresh_fs, read_content)
- `.planning/phases/10-streaming-writes-core/10-CONTEXT.md` — all locked decisions and architecture choices

### Secondary (MEDIUM confidence)

- `.planning/STATE.md` — accumulated decision log confirming lock ordering concern and no-intermediate-manifests invariant
- `.planning/REQUIREMENTS.md` — STRM-01/03/04/05 requirement text

### Tertiary (LOW confidence)

None — all findings are from direct source inspection.

---

## Metadata

**Confidence breakdown:**
- Standard stack: HIGH — all types verified by direct source read; no external libraries needed
- Architecture: HIGH — clone+end pattern derived from existing flush_buffer_for_fsync structure plus CONTEXT.md decisions; lock ordering confirmed by existing code pattern
- Pitfalls: HIGH — borrow scope pitfall confirmed by understanding of FileStorageAdd lifetime; lock inversion derived from established Rust mutex safety analysis
- Test map: HIGH — test infrastructure exists; streaming_tests.rs is the only gap

**Research date:** 2026-03-29
**Valid until:** 2026-04-28 (stable internal codebase; no external dependencies added)
