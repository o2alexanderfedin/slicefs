# Phase 11: Non-Sequential Write Handling - Research

**Researched:** 2026-03-29
**Domain:** FUSE write-path dual dispatch (streaming vs buffered), pwrite semantics, writeback_cache
**Confidence:** HIGH

## Summary

Phase 11 adds non-sequential write detection and a fallback from the O(log N) streaming `State` accumulator to a `Vec<u8>` buffer model. The core mechanic is a `next_expected_offset` tracker on `OpenFileState` that triggers a one-way transition from `Streaming` to `Buffered` mode when any write arrives at a non-sequential offset. The buffered path provides standard pwrite semantics (random writes, gap zero-fill, overlapping writes) at the cost of O(N) memory for that file handle.

The implementation is contained entirely within `filesystem.rs` (the `OpenFileState` struct and the 5 dispatch points: `test_write`, `test_read`, `test_release`, `flush_buffer_for_fsync`, `test_setattr_size`) plus new integration tests. The blockset crate is untouched unless the sparse/hole node optimization is included. Two `#[ignore]` tests in `write_path_tests.rs` and `posix_compliance_tests.rs` are waiting to be un-ignored.

**Primary recommendation:** Implement the WriteMode enum with dual dispatch in filesystem.rs, keeping the streaming path completely unchanged and adding the buffered path alongside it. The sparse/hole optimization should be deferred unless scope permits -- CAS block-level dedup of all-zero chunks is acceptable interim behavior.

<user_constraints>
## User Constraints (from CONTEXT.md)

### Locked Decisions
- **Strict offset tracking**: add `next_expected_offset: u64` to OpenFileState, initialized to 0
- On each write: if `offset != next_expected_offset`, trigger fallback -- zero tolerance for gaps, overlaps, or backward seeks
- After a sequential write: `next_expected_offset += data.len()`
- No gap-filling in streaming mode -- any non-sequential access means fall back
- **One-way transition**: once a non-sequential write is detected, the file handle stays in Buffered mode for its entire remaining lifetime -- never reverse back to Streaming
- Add `WriteMode` enum (`Streaming` | `Buffered`) to OpenFileState
- On transition: materialize current State via clone+end+file_storage_get into a Vec<u8>, then apply the non-sequential write to the buffer
- Drop the State after materialization -- no need to keep both representations
- Decrement last_committed_root refcount if one exists (consistent with Phase 10 refcount lifecycle)
- Standard pwrite semantics: write data at arbitrary offset into Vec<u8>, extending with zeros if offset > buf.len()
- On release: commit entire buffer via `State::push_all(&mut fsa, &buf)` then `state.end()` -- same finalization path as Phase 10 but from materialized buffer
- **Dual dispatch on WriteMode**: test_write, test_read, test_release, flush_buffer_for_fsync, and test_setattr_size all check WriteMode and dispatch to Streaming or Buffered code path
- fsync in Buffered mode: push_all + end the buffer content, same clone+end pattern but from Vec<u8>
- truncate in Buffered mode: standard buf.resize() -- simpler than Streaming truncate
- read in Buffered mode: serve directly from buf (no clone+end materialization needed)
- **writeback_cache not enabled by default** -- sequential writes are the common case
- When writeback_cache IS enabled: out-of-order FUSE write callbacks trigger immediate fallback to Buffered mode
- Detection check (offset vs next_expected_offset) happens with only open_files lock held
- If fallback triggered: acquire io lock for materialization (open_files -> io, consistent with Phase 10 canonical ordering)
- In Buffered mode: writes to Vec<u8> need only open_files lock (no io access needed until flush/release)

### Claude's Discretion
- Exact WriteMode enum placement (in OpenFileState struct vs separate type)
- Whether to log/trace when fallback triggers (useful for debugging but not user-visible)
- Error handling for materialization failures during transition
- Whether flush_buffer_for_fsync in Buffered mode reuses existing infrastructure or has its own path

### Deferred Ideas (OUT OF SCOPE)
- If tree-level sparse representation proves too invasive for blockset crate during this phase, defer to a dedicated "Sparse File Optimization" phase -- CAS block-level dedup provides acceptable (not optimal) interim behavior
</user_constraints>

<phase_requirements>
## Phase Requirements

| ID | Description | Research Support |
|----|-------------|-----------------|
| STRM-02 | Non-sequential writes (pwrite at arbitrary offset) detected and fall back to Vec<u8> buffer mode with no regression | WriteMode enum + dual dispatch across 5 methods; 2 existing #[ignore] tests to un-ignore; new tests for fallback transition, overlapping writes, fsync in buffered mode, read-during-write in buffered mode, writeback_cache out-of-order delivery |
</phase_requirements>

## Standard Stack

### Core
| Library | Version | Purpose | Why Standard |
|---------|---------|---------|--------------|
| blockset (crate) | local | State, Tree, push_bytes, push_all, end, file_storage_get, FileStorageAdd | Project's own Merkle tree accumulator -- the streaming/buffered finalization target |
| fuser | current | FUSE bindings, WriteFlags, writeback_cache capability | Project's FUSE frontend |
| metadata (crate) | local | DictMetadataStore, set_manifest, increment_refcount, decrement_refcount | Project's metadata layer |

### Supporting
| Library | Version | Purpose | When to Use |
|---------|---------|---------|-------------|
| tracing | current | Logging fallback transitions | Already in deps; use for debug-level logging of Streaming->Buffered transitions |
| tempfile | current | Integration test temp dirs | Already used by streaming_tests.rs |

## Architecture Patterns

### WriteMode Enum and Dual Dispatch

The central pattern is a `WriteMode` enum stored in `OpenFileState`:

```rust
enum WriteMode {
    Streaming {
        state: State,
        next_expected_offset: u64,
    },
    Buffered {
        buf: Vec<u8>,
    },
}
```

**Recommendation:** Embed the mode-specific data directly in the enum variants rather than keeping `state` and `buf` as separate fields alongside a mode flag. This makes illegal states unrepresentable -- you cannot have a Streaming mode with a buf, or a Buffered mode with a State.

The `OpenFileState` struct becomes:

```rust
struct OpenFileState {
    ino: u64,
    write_mode: WriteMode,
    byte_count: u64,
    cas_committed: bool,
    last_committed_root: Option<Digest224>,
}
```

### Dispatch Points

Five methods need dual dispatch:

1. **test_write** -- Streaming: check offset, push_bytes; Buffered: pwrite into buf
2. **test_read** -- Streaming: clone+end+file_storage_get (existing); Buffered: slice from buf
3. **test_release** -- Streaming: state.end() (existing); Buffered: push_all+end from buf
4. **flush_buffer_for_fsync** -- Streaming: clone+end (existing); Buffered: push_all+end from buf (similar but source is Vec not State clone)
5. **test_setattr_size** -- Streaming: materialize+resize+repush (existing); Buffered: buf.resize()

### Fallback Transition Pattern

```rust
// In test_write, when offset != next_expected_offset:
// 1. Materialize current State to bytes (clone+end+file_storage_get)
// 2. Create Vec<u8> from materialized bytes
// 3. Apply the current write to the buffer
// 4. Replace WriteMode::Streaming with WriteMode::Buffered
// 5. Decrement last_committed_root if Some
```

The materialization needs both `open_files` lock (to clone State) and `io` lock (to run end+file_storage_get). Lock ordering: release open_files, acquire io, materialize, re-acquire open_files, swap WriteMode.

**Critical detail:** Between releasing open_files and re-acquiring it, another write could arrive on the same fh. Since FUSE serializes writes per-fh (even with writeback_cache -- writes to the same fh are serialized by the kernel), this is safe. But the code must handle the case where the state was already swapped by checking WriteMode after re-acquiring the lock.

### Buffered Release Finalization

```rust
// In test_release, Buffered path:
let digest: Digest224 = {
    let mut io = self.io.lock().unwrap();
    let mut fsa = FileStorageAdd::new(&mut *io);
    let mut fresh = State::default();
    fresh.push_bytes(&mut fsa, &buf);
    let d256 = fresh.end(&mut fsa);
    fsa.end(&d256)
};
// Then same refcount lifecycle as Streaming release
```

Note: `State::push_all` is a trait method on `Tree` that creates a fresh State, calls push_bytes, then end+fsa.end. For release we need the Digest224, so we use push_bytes+end+fsa.end directly.

### Open on Existing Files

**Important finding:** When an existing file is opened for writing (without O_TRUNC), the current code initializes `OpenFileState` with `State::default()` and `byte_count: 0`. This means:

- If the first write is at offset 0, `next_expected_offset` starts at 0, so it matches -- streaming mode works.
- If the first write is at a non-zero offset (e.g., appending to an existing file via pwrite), `next_expected_offset` is 0, `offset` is non-zero, so it immediately falls back to Buffered.
- In the fallback, `byte_count` is 0 so there's nothing to materialize from the State. The buf starts empty, gets zero-extended to the write offset, and the write is applied.

This is correct behavior: the existing file's committed content is read via `test_read` from the manifest, not from the in-flight state. The write handle only tracks new/modified content. **However**, this means a Buffered write at offset 100 creates a 104-byte buf with zeros in [0..100], even though the committed file may have data at [0..100].

**This is a correctness issue.** When the file is released, the entire buf is pushed to CAS, but the buf only has zeros where the original file had data. The fix: on transition to Buffered mode (or on first non-sequential write from empty state), load the existing committed content into the buffer first.

For the transition case (Streaming had some bytes, now fallback), the materialization already captures the current content. But for the case where `byte_count == 0` and the first write is at a non-zero offset on an existing file, we need to load the committed manifest content into the buffer.

**Recommended approach:** When transitioning to Buffered mode with `byte_count == 0`, check if the inode has a committed manifest and load it:

```rust
// If byte_count == 0 and first write is non-sequential:
let existing_content = match self.meta.get_manifest(ino) {
    Ok(m) if !m.is_empty() => {
        let mut io = self.io.lock().unwrap();
        file_storage_get(&mut *io, &m[0]).unwrap_or_default()
    }
    _ => Vec::new(),
};
let mut buf = existing_content;
// Now apply the non-sequential write to buf
```

### Anti-Patterns to Avoid
- **Two-phase lock release/re-acquire without checking state:** After releasing and re-acquiring `open_files`, always check that the fh still exists and the WriteMode hasn't changed.
- **Forgetting to update byte_count in Buffered mode:** After every buf modification (write, truncate), `byte_count = buf.len() as u64`.
- **Materializing empty State:** When `byte_count == 0` in Streaming mode, don't call clone+end -- just create an empty Vec.
- **Double refcount decrement:** When transitioning, only decrement `last_committed_root` once. The transition clears it; subsequent operations in Buffered mode won't see it again.

## Don't Hand-Roll

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|-------------|-----|
| Merkle tree finalization | Custom CAS commit logic | State::push_bytes + end + fsa.end (for Buffered release) | Established pattern from Phase 10; handles internal node flushing and Digest224 generation |
| Reorder buffer for writeback_cache | Write queue with offset sorting | Immediate fallback to Buffered mode | Zero tolerance is simpler and correct; reorder buffers are complex and error-prone |
| Sparse file optimization | Custom hole tracking in Vec<u8> | CAS block-level dedup of zero blocks (interim) | All-zero blocks share one physical block via CAS; acceptable until tree-level holes are implemented |

## Common Pitfalls

### Pitfall 1: Existing File Content Loss on Non-Sequential Write
**What goes wrong:** Opening an existing 1MB file, writing 10 bytes at offset 500000, releasing -- file ends up as 500010 bytes of zeros + 10 bytes of data instead of preserving the original 1MB content.
**Why it happens:** OpenFileState starts with byte_count=0 and empty State. Buffered fallback creates buf from materialized State (which is empty), not from committed content.
**How to avoid:** On fallback to Buffered when byte_count==0, load committed manifest content into buf before applying the write.
**Warning signs:** Tests with pwrite on existing files fail; file shrinks or loses prior content.

### Pitfall 2: Refcount Leak During Transition
**What goes wrong:** State materialization creates CAS entries during clone+end, but the resulting digest is never tracked for decrement.
**Why it happens:** The clone+end during transition is a temporary materialization for reading bytes, not a commit. The entries go into CAS but may not be reachable from any manifest.
**How to avoid:** The transition materialization is transient -- the entries in CAS are only reachable during the file_storage_get call. Since FileStorageAdd flushes on drop and the result is read immediately, this is fine. The final release will create new CAS entries from the buffer. Old committed root (last_committed_root) should be decremented during transition since the streaming state is being discarded.
**Warning signs:** Increasing physical store size with zero-refcount blocks after many fallback transitions.

### Pitfall 3: Lock Ordering Violation During Fallback
**What goes wrong:** Deadlock when materializing State during fallback.
**Why it happens:** Trying to hold open_files lock while acquiring io lock for materialization, or vice versa.
**How to avoid:** Follow canonical lock ordering: open_files -> io. For fallback: (1) clone State under open_files, (2) release open_files, (3) acquire io for materialization, (4) release io, (5) re-acquire open_files to swap WriteMode. Between steps 4 and 5, another write cannot arrive on the same fh because FUSE serializes per-fh writes.
**Warning signs:** Deadlock in multi-threaded FUSE operation; test hangs.

### Pitfall 4: byte_count Drift in Buffered Mode
**What goes wrong:** byte_count doesn't match buf.len() after writes that extend beyond current size.
**Why it happens:** In Buffered mode, writes can extend the buffer via resize. If byte_count isn't updated to buf.len() after every write, inode size will be wrong.
**How to avoid:** After every Buffered write: `state.byte_count = buf.len() as u64`. This is simpler than tracking increments.
**Warning signs:** Inode size doesn't match file content length; read returns fewer bytes than expected.

### Pitfall 5: fsync in Buffered Mode Creates Orphan Blocks
**What goes wrong:** Each fsync in Buffered mode does push_all+end from the full buffer, creating a new CAS tree. Previous fsync's tree becomes unreferenced if refcount lifecycle isn't maintained.
**Why it happens:** Buffered fsync is "commit from scratch" each time (push_all the entire buffer), not incremental.
**How to avoid:** Same refcount lifecycle as Streaming fsync: track last_committed_root, decrement old, increment new. This is already the pattern in flush_buffer_for_fsync.
**Warning signs:** Orphan CAS entries accumulating between fsyncs.

## Code Examples

### WriteMode Enum Definition

```rust
// Source: Design from CONTEXT.md decisions
enum WriteMode {
    Streaming {
        state: State,
        next_expected_offset: u64,
    },
    Buffered {
        buf: Vec<u8>,
    },
}
```

### test_write with Dual Dispatch

```rust
// Source: Adaptation of existing test_write (filesystem.rs:263)
pub fn test_write(&self, fh: u64, offset: u64, data: &[u8]) -> Result<u32, i32> {
    let mut open_files = self.open_files.lock().unwrap();
    let file_state = open_files.get_mut(&fh).ok_or(libc::EBADF)?;

    match &mut file_state.write_mode {
        WriteMode::Streaming { state, next_expected_offset } => {
            if offset != *next_expected_offset {
                // Fallback to Buffered mode
                let snapshot = state.clone();
                let byte_count = file_state.byte_count;
                let ino = file_state.ino;
                let old_root = file_state.last_committed_root.take();
                drop(open_files); // Release before io

                // Materialize current streaming content (or load from committed)
                let mut buf = if byte_count > 0 {
                    let digest = {
                        let mut io = self.io.lock().unwrap();
                        let mut fsa = FileStorageAdd::new(&mut *io);
                        let d256 = snapshot.end(&mut fsa);
                        fsa.end(&d256)
                    };
                    let mut io = self.io.lock().unwrap();
                    file_storage_get(&mut *io, &digest).unwrap_or_default()
                } else {
                    // No streaming content yet -- load from committed manifest
                    match self.meta.get_manifest(ino) {
                        Ok(m) if !m.is_empty() => {
                            let mut io = self.io.lock().unwrap();
                            file_storage_get(&mut *io, &m[0]).unwrap_or_default()
                        }
                        _ => Vec::new(),
                    }
                };

                // Apply pwrite semantics
                let end = offset as usize + data.len();
                if end > buf.len() {
                    buf.resize(end, 0);
                }
                buf[offset as usize..end].copy_from_slice(data);

                // Swap to Buffered mode
                let mut open_files = self.open_files.lock().unwrap();
                if let Some(s) = open_files.get_mut(&fh) {
                    s.write_mode = WriteMode::Buffered { buf };
                    s.byte_count = /* buf.len() */ end.max(s.byte_count as usize) as u64;
                    s.cas_committed = false;
                }

                // Decrement old committed root
                if let Some(old) = old_root {
                    self.meta.decrement_refcount(&old);
                }

                return Ok(data.len() as u32);
            }

            // Sequential write -- continue streaming
            let mut io = self.io.lock().unwrap();
            let mut fsa = FileStorageAdd::new(&mut *io);
            state.push_bytes(&mut fsa, data);
            drop(fsa);
            drop(io);
            *next_expected_offset += data.len() as u64;
            file_state.byte_count += data.len() as u64;
            file_state.cas_committed = false;
            Ok(data.len() as u32)
        }
        WriteMode::Buffered { buf } => {
            // Standard pwrite into buffer
            let end = offset as usize + data.len();
            if end > buf.len() {
                buf.resize(end, 0);
            }
            buf[offset as usize..end].copy_from_slice(data);
            file_state.byte_count = buf.len() as u64;
            file_state.cas_committed = false;
            Ok(data.len() as u32)
        }
    }
}
```

### Buffered Release Finalization

```rust
// Source: Adaptation of existing test_release (filesystem.rs:292)
// In the Buffered arm of test_release:
WriteMode::Buffered { buf } => {
    if buf.is_empty() {
        if !cas_committed {
            self.meta.set_manifest(ino, &[]).map_err(|_| libc::EIO)?;
        }
        return Ok(());
    }
    let new_digest: Digest224 = {
        let mut io = self.io.lock().unwrap();
        let mut fsa = FileStorageAdd::new(&mut *io);
        let mut fresh = State::default();
        fresh.push_bytes(&mut fsa, &buf);
        let d256 = fresh.end(&mut fsa);
        fsa.end(&d256)
    };
    // Same refcount lifecycle as Streaming release...
}
```

### Buffered Read (Direct from Buffer)

```rust
// In test_read, when finding an open writer in Buffered mode:
WriteMode::Buffered { buf } => {
    let raw_bytes = buf.clone(); // Clone under open_files lock
    // (buf is already materialized -- no CAS roundtrip needed)
    let start = (offset as usize).min(raw_bytes.len());
    let end_pos = (start + size as usize).min(raw_bytes.len());
    Ok(raw_bytes[start..end_pos].to_vec())
}
```

## State of the Art

| Old Approach | Current Approach | When Changed | Impact |
|--------------|------------------|--------------|--------|
| Vec<u8> buffer for all writes | State streaming for sequential, Vec<u8> fallback for non-sequential | Phase 10/11 | O(log N) memory for common case (sequential), O(N) only for random writes |
| Ignore offset in test_write | Strict offset tracking with fallback | Phase 11 | Correct pwrite semantics for all access patterns |

## Open Questions

1. **Sparse/Hole Node Optimization**
   - What we know: The user wants tree-level sparse representation (hole nodes encoding zero-byte sequences without physical storage). CAS already deduplicates identical all-zero blocks.
   - What's unclear: How invasive the blockset crate changes would be (new node types in content_dependant_tree.rs, new Part3 variant for holes, changes to GetBytes/GetData).
   - Recommendation: Defer unless the planner assesses it as low-risk. CAS dedup of zero blocks is adequate for correctness.

2. **Existing File Open Without O_TRUNC**
   - What we know: Current code initializes OpenFileState with empty State and byte_count=0. For sequential appends starting at offset 0, this is fine (the file is being rewritten). For non-sequential writes on existing files, the committed content must be loaded.
   - What's unclear: Whether any tool (vim, sqlite) opens a file without O_TRUNC and writes at non-zero offsets as first write.
   - Recommendation: Handle it defensively -- always load committed content on fallback when byte_count==0.

3. **FUSE Per-fh Write Serialization**
   - What we know: The Linux kernel FUSE module serializes writes to the same file handle even with writeback_cache. FUSE-T on macOS should behave similarly.
   - What's unclear: Whether FUSE-T guarantees the same serialization.
   - Recommendation: The lock-release-reacquire pattern during fallback is safe regardless because we check state after re-acquisition. No extra synchronization needed.

## Validation Architecture

### Test Framework
| Property | Value |
|----------|-------|
| Framework | cargo test (Rust built-in) |
| Config file | Cargo.toml (workspace) |
| Quick run command | `cargo test -p slicefs-cli --test streaming_tests --test write_path_tests --test posix_compliance_tests` |
| Full suite command | `cargo test` |

### Phase Requirements -> Test Map
| Req ID | Behavior | Test Type | Automated Command | File Exists? |
|--------|----------|-----------|-------------------|-------------|
| STRM-02a | pwrite at non-sequential offset falls back to Buffered | integration | `cargo test -p slicefs-cli --test streaming_tests test_nonseq -x` | No - Wave 0 |
| STRM-02b | Fallback materializes Streaming content correctly | integration | `cargo test -p slicefs-cli --test streaming_tests test_fallback_materializes -x` | No - Wave 0 |
| STRM-02c | Buffered write at arbitrary offset produces correct file | integration | `cargo test -p slicefs-cli --test write_path_tests test_write_with_gap_zero_pads` | Yes - currently #[ignore] |
| STRM-02d | Buffered write on existing file preserves prior content | integration | `cargo test -p slicefs-cli --test streaming_tests test_pwrite_existing_file -x` | No - Wave 0 |
| STRM-02e | Read in Buffered mode returns correct content | integration | `cargo test -p slicefs-cli --test streaming_tests test_read_buffered -x` | No - Wave 0 |
| STRM-02f | fsync in Buffered mode commits correctly | integration | `cargo test -p slicefs-cli --test streaming_tests test_fsync_buffered -x` | No - Wave 0 |
| STRM-02g | Truncate in Buffered mode works | integration | `cargo test -p slicefs-cli --test streaming_tests test_truncate_buffered -x` | No - Wave 0 |
| STRM-02h | Sequential writes remain in Streaming mode (no regression) | integration | Existing streaming_tests.rs (all 12 tests) | Yes |
| STRM-02i | Out-of-order FUSE writes (writeback_cache simulation) produce correct file | integration | `cargo test -p slicefs-cli --test streaming_tests test_out_of_order_writes -x` | No - Wave 0 |
| STRM-02j | pwrite at non-zero offset on new file zero-pads correctly | integration | `cargo test -p slicefs-cli --test posix_compliance_tests test_file_write_at_offset_zero_pads` | Yes - currently #[ignore] |
| STRM-02k | Overlapping writes produce correct content | integration | `cargo test -p slicefs-cli --test streaming_tests test_overlapping_writes -x` | No - Wave 0 |
| STRM-02l | Mixed sequential then non-sequential writes on same handle | integration | `cargo test -p slicefs-cli --test streaming_tests test_mixed_seq_then_nonseq -x` | No - Wave 0 |

### Sampling Rate
- **Per task commit:** `cargo test -p slicefs-cli --test streaming_tests --test write_path_tests --test posix_compliance_tests`
- **Per wave merge:** `cargo test`
- **Phase gate:** Full suite green before `/gsd:verify-work`

### Wave 0 Gaps
- [ ] New test functions in `streaming_tests.rs` -- covers STRM-02a/b/d/e/f/g/i/k/l (non-sequential fallback, buffered operations, out-of-order writes)
- [ ] Un-ignore `test_write_with_gap_zero_pads` in `write_path_tests.rs` -- covers STRM-02c
- [ ] Un-ignore `test_file_write_at_offset_zero_pads` in `posix_compliance_tests.rs` -- covers STRM-02j

## Sources

### Primary (HIGH confidence)
- Codebase inspection: `filesystem.rs` (OpenFileState struct lines 47-61, test_write lines 263-281, test_release lines 292-346, test_read lines 371-402, flush_buffer_for_fsync lines 416-476, test_setattr_size lines 481-558)
- Codebase inspection: `content_dependant_tree.rs` (State = Vec<Level>, Tree trait with push_bytes/push_all/end)
- Codebase inspection: `streaming_tests.rs` (12 existing tests covering STRM-01/03/04/05)
- Codebase inspection: `write_path_tests.rs` and `posix_compliance_tests.rs` (#[ignore] tests for Phase 11)
- CONTEXT.md decisions (all implementation choices locked by user)

### Secondary (MEDIUM confidence)
- FUSE per-fh write serialization: Linux kernel FUSE documentation states writes to the same fh are serialized even with writeback_cache

### Tertiary (LOW confidence)
- FUSE-T per-fh serialization guarantee on macOS: not explicitly documented; assumed from NFS translation layer behavior

## Metadata

**Confidence breakdown:**
- Standard stack: HIGH - all code is in the project codebase, fully inspected
- Architecture: HIGH - dual dispatch pattern is straightforward; all dispatch points identified and code examples verified against existing patterns
- Pitfalls: HIGH - identified from code inspection (existing file content loss is the most critical finding)

**Research date:** 2026-03-29
**Valid until:** Indefinite (project-internal code, no external dependency changes)
