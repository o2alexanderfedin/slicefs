# Pitfalls Research

**Domain:** SliceFS v2.0 — Streaming Writes, Compression Removal, Bug Fixes
**Researched:** 2026-03-29
**Confidence:** HIGH (v1.0 codebase read directly; FUSE kernel mailing lists; Linux refcount subsystem docs; USENIX FAST papers; production CAS backup tool analysis)

---

## Scope

This document covers pitfalls specific to the v2.0 milestone additions:

1. Replacing the buffered write path with streaming writes via `State::push_bytes()` (incremental Merkle tree)
2. Removing compression from the write path so dedup hashes on raw bytes
3. Maintaining backward compatibility with v1.0 stores that contain compressed blocks
4. Fixing the refcount overflow bug and statfs reporting
5. Improving snapshot lookup from O(n) to indexed

The foundational pitfalls (GC races, crash consistency, FUSE context-switch overhead, dedup index memory) remain valid from the v1.0 research and are not repeated here.

---

## Critical Pitfalls

### Pitfall 1: State::push_bytes() Is Append-Only — Random Writes and Truncation Require Full Rebuild

**What goes wrong:**
The `State` (content-dependent Merkle tree) accumulates bytes via `push_bytes()` in forward-only order. It has no concept of "overwrite bytes at offset N" or "truncate to size M." When the FUSE layer receives a random-offset write (`write(fd, buf, len)` at offset != current EOF) or a `setattr`/`ftruncate` call, naively calling `push_bytes()` with only the new data produces a completely wrong tree root. The inode's manifest will point to content that represents only the new bytes, discarding all earlier content.

**Why it happens:**
The buffered write path (v1.0) sidesteps this by keeping a `Vec<u8>` buffer in memory for the entire file lifetime and only calling `State::push_all()` at flush time. The buffer supports random writes and truncation trivially. When a developer migrates to streaming pushes without reading the `State` API contract, they assume `push_bytes()` is equivalent to `write(2)` semantics, but it is only equivalent to sequential `write(2)` from offset 0 with no gaps or overwrites.

**How to avoid:**
- Accept that `State` is a streaming digest accumulator, not a random-access buffer. The FUSE write path must maintain a per-handle `Vec<u8>` buffer **until it is certain all writes are sequential from offset 0 with no gaps**.
- Alternatively, implement a "write pipeline" that sorts all write fragments by offset and detects gaps/overwrites before deciding whether streaming is possible. Only invoke `State::push_bytes()` if writes are provably sequential.
- For the common case (sequential file creation, `cp`, `dd`), detect that `offset == current_buf_len` before pushing. Fall back to buffer otherwise.
- Truncation (`setattr size < current`) can never be streamed — it always requires reading back and re-pushing the first `new_size` bytes, or discarding the in-progress `State` and starting fresh.
- **Key invariant to codify in a type or comment:** A `State` that has had any bytes pushed to it cannot be rewound. If you need to truncate-and-continue, you must construct a new `State` and replay the kept bytes.

**Warning signs:**
- Test with `vim`, `emacs`, or `cp --sparse` through the mount. These tools write non-sequentially (overwrite header bytes after seeking to EOF, or write the last byte first to pre-allocate).
- A write-intensive test that uses `pwrite(2)` (positioned write) at random offsets produces files that read back garbage.
- `ftruncate(fd, 0)` followed by new writes produces a file whose content starts with the old content rather than the new content.

**Phase to address:** v2.0 streaming write phase — the streaming/sequential-write detection logic must be the first implementation decision. Do not write a single `push_bytes()` call in FUSE callbacks without this guard.

---

### Pitfall 2: The Inode Size Stored in Metadata Desynchronizes from the Merkle Tree Root

**What goes wrong:**
In the buffered write path, `inode.size = buf.len()` is set atomically with `set_manifest(digest)` at flush time. In the streaming path, `inode.size` must be updated incrementally as bytes are pushed, but the Merkle tree root is only finalized when `State::end()` is called. There is a window where `inode.size > 0` but the manifest is still the empty/old root from the previous flush. If a crash occurs or the FUSE daemon is killed between incrementing `inode.size` and finalizing the manifest, reads will return data from the old manifest but `stat(2)` will report the new (larger) size — producing apparent data truncation or garbage reads.

**Why it happens:**
Streaming writes update size on every push (or batch of pushes) to keep `getattr` responses accurate during the write, but the digest is only computable when the stream ends. The size and the digest are two separate invariants that v1.0 always kept in sync via a single flush-time update. The streaming path breaks this synchronization guarantee unless explicitly designed to handle it.

**How to avoid:**
- Never write `inode.size` to durable metadata during an in-progress stream. Keep size as in-memory state on the `OpenFileState`.
- Only commit `(manifest, size)` atomically as a unit when `State::end()` is called (at `fsync`, `release`, or at deliberate checkpoint intervals).
- Use the WAL (already present) to journal both the manifest digest and the new size in a single atomic operation, so crash recovery either sees the complete update or the complete absence of it.
- If intermediate `getattr` accuracy is required (so `ls -l` shows a growing file during a large write), track size in-memory per file handle and return the in-memory size from FUSE `getattr` while deferring the durable update.

**Warning signs:**
- `stat(2)` shows `size=1GB` but reading the file returns 0 bytes after a crash.
- Any code that calls `update_inode(size=N)` in a loop during streaming without a paired `set_manifest`.
- `getattr` reads `inode.size` from durable metadata (redb/WAL) rather than from in-memory `OpenFileState`.

**Phase to address:** v2.0 streaming write phase — write the crash recovery test for streaming writes before implementing streaming.

---

### Pitfall 3: Compression Removal Breaks Dedup Hash Identity — v1.0 and v2.0 Blocks Appear as Different Content

**What goes wrong:**
In v1.0, the write path is: `raw_bytes → compress_block(raw) → State::push_all(wire_bytes)`. The Merkle root (Digest224) is computed over the **compressed** bytes. In v2.0, the target write path is: `raw_bytes → State::push_all(raw_bytes)`. The Merkle root is computed over the **raw** bytes. These two roots are structurally incompatible: the same file content produces a different Digest224 in v1.0 vs v2.0. This means:

1. A file written in v1.0 and re-written in v2.0 gets stored as two distinct blocks — zero dedup between v1.0 and v2.0 writes, even for identical content.
2. The v1.0 manifest digest stored in metadata cannot be used to read the v2.0 block store.
3. Dedup ratio drops at store migration time because the logical block identity changes.

**Why it happens:**
The correct ordering is always: **hash on raw bytes, then optionally compress the stored block as a storage optimization**. v1.0 violated this ordering by hashing compressed bytes, which broke cross-compressor dedup. v2.0 fixes the ordering, but fixing it creates a two-epoch block store where the two epochs cannot share dedup state.

**How to avoid:**
- Add a per-block metadata tag (`raw` vs `compressed:algo`) that the read path consults when fetching a block. The manifest stores the Digest224 of the **raw** bytes; the block file on disk is stored with a header indicating whether it is compressed. This separates the identity (raw hash) from the storage format (compressed payload).
- For the v2.0 migration: treat v1.0 blocks as read-only legacy. New writes go to the v2.0 store. Old blocks remain readable via the v1.0 decompression path. No migration of existing blocks is required at mount time.
- The `store_version` field already in `SliceFsFilesystem` gates read-path behavior. Extend it to gate write-path behavior: version 1 writes hashed-compressed, version 2 writes hashed-raw.
- Document that stores created with v1.0 and stores created with v2.0 cannot share dedup identity. Users who need unified dedup must migrate (read all files from v1.0 store, write to v2.0 store).

**Warning signs:**
- `slicefs stats` on a store that was first populated in v1.0 and then updated in v2.0 shows near-zero dedup ratio even when the content is identical across old and new writes.
- Any test that writes the same bytes in v1.0 mode and v2.0 mode and expects the same Digest224.
- Code that calls `State::push_all(wire_bytes)` regardless of whether `wire_bytes` are compressed or raw.

**Phase to address:** v2.0 compression-removal phase — the block identity design decision (hash raw vs hash compressed) must be locked before touching the write path, and it must be documented in the store format spec.

---

### Pitfall 4: Mixed-Version Stores — Decompression Required for Old Blocks, Absent for New Blocks

**What goes wrong:**
After compression is removed from the write path (v2.0), the store contains a mix of:
- Old blocks: stored as `[AlgorithmId byte][compressed payload]` (v1.0 format)
- New blocks: stored as `[raw bytes]` with no header

The read path must detect which format each block is in. The existing fallback in `from_wire_bytes()` (try decompress; on error, return raw) works for blocks that decompress cleanly, but is **unreliable for raw blocks whose first byte happens to be a valid `AlgorithmId`**. A raw block whose first byte is `0x01` (the Zstd ID) will be passed to the Zstd decompressor, which will either return an error (detectable) or — in pathological cases — successfully decompress garbage into garbage (silent corruption).

**Why it happens:**
The fallback approach `try decompress, on error return raw` is correct only if decompression failure is guaranteed for unformatted input. In practice, raw binary data can begin with any byte value, and a decompressor may accept malformed input and return a non-error result. For Zstd specifically, the frame magic is `0xFD2FB528` (4 bytes), so a raw block is unlikely to accidentally satisfy the full Zstd header — but for the `AlgorithmId::None` and `AlgorithmId::Raw` IDs (which are passthroughs), any block whose first byte is `0x00` or `0x02` will appear as a "valid" no-compression block and return the remaining bytes as content, dropping the first byte.

**How to avoid:**
- Add an explicit format sentinel to the block store, not just a runtime heuristic. Options:
  - **Preferred:** Store a `format_version` field in the block store index or in the block's directory manifest. The read path consults the version field to decide whether to attempt decompression, not the first byte of the block.
  - **Acceptable:** Use the existing `store_version` field in `SliceFsFilesystem` consistently. If `store_version < 2`, all blocks are in v1.0 compressed format. If `store_version >= 2`, all blocks are in v2.0 raw format. No mixed-version store at the individual block level.
  - **Avoid:** Per-block heuristic decompression detection. It is fragile and will corrupt files whose first byte accidentally matches an `AlgorithmId`.
- Implement a `slicefs migrate` command that converts all blocks in a v1.0 store to v2.0 format atomically. After migration, `store_version` is bumped to 2 and no legacy path is needed.

**Warning signs:**
- Any file whose first byte is `0x00` (e.g., a binary format with a null-padded header, a zero-filled sparse file, or a TIFF image whose magic starts with `0x49 0x49`) reads back with the first byte stripped or with corrupted content.
- The `from_wire_bytes` fallback is triggered in production (add a log/metric counter to detect this).
- No `slicefs migrate` command exists for v1.0 → v2.0 store conversion.

**Phase to address:** v2.0 compression-removal phase — resolve block format versioning before implementing the raw write path. The block-level format detection approach must be decided and documented as part of the store format spec.

---

### Pitfall 5: Refcount Overflow — Silent Wrap to Zero Frees a Still-Referenced Block

**What goes wrong:**
If a block's reference count is stored as a fixed-width unsigned integer (e.g., `u16`, `u32`) and the count reaches the maximum value, an arithmetic increment wraps to zero. The GC then sees `refcount == 0` and frees the block. All files referencing that block now have a dangling manifest entry pointing to a deleted block. Reads return `ENOENT` from the block store (best case) or return bytes from a different block that happened to reuse the same storage address (worst case, silent data corruption).

In the v1.0 codebase, the PROJECT.md identifies this as a known bug. The existing implementation uses a simple numeric refcount that is incremented on dedup hit without overflow protection.

**Why it happens:**
Refcounts wrap silently in Rust with `u32::wrapping_add(1)` or when using `+` on debug builds without overflow checks, and silently in release builds. A block that is duplicated more than `u32::MAX` (4 billion) times — plausible for a block of all zeros or a common file header in a large dataset — will wrap to zero and be freed. The bug is latent: it only triggers when a single block is referenced by an extraordinary number of files, but for a filesystem in daily use over years, this threshold is reachable.

**How to avoid:**
- Replace all refcount increments with `saturating_add(1)`. A saturated refcount means "this block has more references than we can count; never free it." This is the correct invariant: it is always safe to keep a block that might be referenced; it is never safe to free a block that is still referenced.
- Add a test that creates `u32::MAX + 1` references to the same block (using a mock refcount store) and verifies the count saturates rather than wraps.
- Consider `u64` for refcounts if the extra 4 bytes per block is acceptable in the index. At u64::MAX the wrap-to-zero risk is negligible in practice, but `saturating_add` is still the correct semantic.
- Add an `fsck` check that reports any block with `refcount == u32::MAX` (the saturation sentinel) for operator visibility.

**Warning signs:**
- Any `refcount += 1` or `refcount.fetch_add(1, Ordering::Relaxed)` without overflow handling.
- No test that verifies refcount behavior at the maximum value.
- `increment_refcount` implementation that uses plain integer arithmetic.

**Phase to address:** v2.0 bug-fix phase — this is a correctness bug that can cause data loss. Fix it before streaming writes, as streaming writes may increase dedup hit rates and accelerate the path to overflow.

---

### Pitfall 6: statfs Reports Incorrect Free Space — Applications Abort Writes or Over-Provision

**What goes wrong:**
If `f_bfree` and `f_bavail` are hardcoded or computed incorrectly, applications that check free space before writing will either:
- Refuse to write (if the reported free space is lower than actual), causing `ENOSPC` errors on operations that would succeed
- Fail to detect a full filesystem (if reported free space is higher than actual), causing silent truncation or kernel-level write failures

For a deduplicating filesystem, the "correct" answer for free space is ambiguous: the logical free space (based on total capacity minus logical file sizes) is always much larger than the physical free space (based on actual blocks on disk). Applications like `rsync`, `df`, `du`, and package managers all query `statfs`. Misleading values break them in hard-to-diagnose ways.

**Why it happens:**
The v1.0 `statfs` uses hardcoded `f_files = 1_000_000` (inode count) and estimates `f_bfree` without tracking actual physical block usage. This is marked as a known bug in PROJECT.md. The correct implementation requires tracking total physical blocks stored in the CAS store, which is a metadata operation not yet connected to the `statfs` response path.

**How to avoid:**
- Implement a `stats()` call on the metadata store that returns `(physical_blocks_used, total_capacity_blocks, inode_count)`. These are the source of truth for `statfs`.
- Physical capacity (`f_blocks`) should reflect the underlying storage device capacity, obtained via `statvfs(2)` on the store directory.
- Physical used (`f_bfree = f_blocks - used_blocks`) reflects CAS-deduplicated physical storage.
- Expose both logical and physical in `slicefs stats --json` for operator visibility.
- Add a test that writes known-size data, queries `statfs`, and verifies `f_bfree` decreases by the expected physical amount.

**Warning signs:**
- Hardcoded `f_files` or `f_blocks` in the `statfs` callback.
- `df -h /mnt/slicefs` reports a wildly incorrect value compared to the backing store's actual disk usage.
- No test that validates `statfs` values against actual written data.

**Phase to address:** v2.0 bug-fix phase — fix before streaming writes, since streaming writes change the physical block count as writes proceed rather than only at flush.

---

### Pitfall 7: Streaming Write Breaks writeback_cache Inode Size Tracking

**What goes wrong:**
When `writeback_cache` is enabled in fuser (which SliceFS relies on for acceptable small-write throughput), the FUSE kernel module maintains its own view of inode size in the page cache. Writes that extend the file update the kernel's cached `i_size` **without notifying the FUSE daemon**. When the FUSE daemon eventually receives the write data (batched), the `offset + len` of the write may exceed the daemon's internally tracked size. The daemon must accept any `offset` as valid and not reject writes beyond its current tracked size as out-of-range.

In the streaming write path, if the implementation uses `offset == current_stream_position` as a guard to decide between streaming and buffering, a write delivered out-of-order by the kernel (due to writeback batching) will misclassify a sequential write as a random write, triggering the fallback to full buffering and defeating the purpose of streaming.

**Why it happens:**
FUSE `writeback_cache` defers delivery of write data to the daemon until either the dirty page limit is reached or `fsync`/`close` is called. The kernel may reorder and coalesce writes before delivering them. A 1GB sequential write may arrive at the daemon as a single 128KB-aligned call, or as several calls, not necessarily in order. The daemon must handle all orderings.

**How to avoid:**
- Do not assume FUSE write callbacks arrive in `offset` order, even for a single file. The streaming write path must handle out-of-order delivery.
- Maintain a `next_expected_offset` per file handle. If a write arrives at an unexpected offset, buffer it and sort rather than pushing to the streaming `State`.
- Alternatively, disable `writeback_cache` for the streaming write path (revert to synchronous mode where writes arrive in order at the cost of throughput). Document this tradeoff explicitly.
- Reference: FUSE kernel mailing list documents `writeback_cache` behavior — the cached writes beyond EOF extend local `i_size` without keeping the userspace server in sync. Trust `offset + len` from the write callback as the new minimum size; do not rely on the daemon's previous `i_size` being correct.

**Warning signs:**
- Write test with `writeback_cache` enabled that writes 10MB sequentially but delivers writes to the daemon in non-sequential callback order — streaming path produces incorrect Merkle root.
- Any streaming implementation that assumes `write(fd, buf, N)` callbacks arrive with `offset == prev_offset + prev_len`.
- No test specifically covering `writeback_cache` + large sequential write + correct read-back.

**Phase to address:** v2.0 streaming write phase — test with `writeback_cache` explicitly before declaring streaming writes complete.

---

### Pitfall 8: Partial Streaming State Is Not Crash-Safe Without WAL Integration

**What goes wrong:**
The v1.0 write path is crash-safe because the buffer is held in memory until `fsync` or `release`, at which point the entire content is atomically committed to CAS via WAL. The streaming path by design commits blocks incrementally — as bytes are pushed to `State`, new blocks are stored in the Dictionary. If the process crashes between the first `push_bytes` call and the final `State::end()` call, the Dictionary contains partial block data that is not referenced by any manifest. These are orphaned blocks that the GC must clean up, which is acceptable. However:

1. If the streaming path calls `set_manifest(partial_root)` at any intermediate checkpoint (to provide crash recovery of partial progress), and the file is then modified further, the old partial manifest must be dereferenced before the new manifest is committed. Failing to decrement the old manifest's refcount creates a permanent refcount leak.
2. If `set_manifest(partial_root)` is called at intermediate checkpoints, a crash between the last checkpoint and `State::end()` leaves the inode pointing to partial content with `size` smaller than the final intended size — content truncation on crash.

**Why it happens:**
Streaming writes tempt developers to checkpoint periodically (e.g., every 64MB) to bound memory usage while also bounding crash-recovery data loss. This looks safe but introduces a refcount leak when the checkpoint manifests are replaced by the final manifest.

**How to avoid:**
- For v2.0, the simplest correct approach is: **no intermediate manifests**. Keep partial streaming state purely in memory. The stream is only finalized (manifest set, refcount incremented) at `fsync` or `release`. This is equivalent to the v1.0 behavior except that instead of a `Vec<u8>` accumulating all bytes, you accumulate the streaming `State` struct (which is O(log N) in memory).
- If checkpointing is required (files larger than available memory), design a checkpointing protocol using the WAL: write a WAL record that says "inode X at checkpoint C has partial root R with refcount pending finalization." GC must honor pending-finalization refcounts as live.
- The "no intermediate manifests" approach avoids all refcount complexity and is the correct starting point for v2.0.

**Warning signs:**
- Any code that calls `set_manifest` more than once per file-open lifetime without decrementing the previous manifest's refcount.
- Integration test: `fsync()` mid-file, then continue writing, then `close()` — verify only one manifest exists and refcount == 1.
- GC running after a crash finds refcount == 2 for a block that is referenced by only one manifest.

**Phase to address:** v2.0 streaming write phase — establish the "one manifest per file lifetime" invariant as a rule before writing any streaming code.

---

### Pitfall 9: O(n) Snapshot Lookup Becomes a Latency Cliff on Mount

**What goes wrong:**
An O(n) snapshot scan at `slicefs snapshot list` or `slicefs snapshot switch` is tolerable when n < 10. As n grows (automated daily snapshots = 365/year), the scan becomes slow. If the mount command itself must scan all snapshots to find the active version (the common case), every mount adds latency proportional to snapshot count. With 1,000 snapshots, mount may take seconds. With 10,000 snapshots, it may time out.

**Why it happens:**
The initial snapshot implementation stores snapshots in a list and iterates to find by name or version number. Indexing is deferred. The list structure grows monotonically as snapshots accumulate. Developers notice the problem in production when automated snapshot creation runs for months.

**How to avoid:**
- Store snapshots in a `HashMap<version_number, snapshot_root>` and `HashMap<name, version_number>` for O(1) lookup by version and O(1) lookup by name.
- Alternatively, use the existing metadata store (redb/B-tree) with a snapshot index table keyed by version number. Lookups are O(log n) in the B-tree, which is acceptable for 10,000+ snapshots.
- The active version pointer (the "current mount" snapshot) should be a single record in the metadata store, not derived by scanning.
- Add a test that creates 1,000 snapshots and verifies that `snapshot switch --version 500` completes in under 100ms.

**Warning signs:**
- Snapshot lookup code that iterates a Vec or list structure.
- No performance test for snapshot operations with large n.
- `slicefs mount` reads all snapshots on startup to find the latest version.

**Phase to address:** v2.0 snapshot-indexing phase — implement indexed lookup alongside the streaming write work.

---

## Technical Debt Patterns

| Shortcut | Immediate Benefit | Long-term Cost | When Acceptable |
|----------|-------------------|----------------|-----------------|
| Keep buffered `Vec<u8>` for all writes, call `push_bytes` only at flush | Zero risk of streaming API misuse; v1.0 behavior preserved | File size still limited by RAM; the v2.0 goal is not achieved | Never — this is what v2.0 exists to change |
| Stream all writes naively without sequential-write guard | Simpler code | Random writes and sparse files produce silently corrupt content | Never — must have the sequential detection guard |
| Per-block heuristic decompression detection for mixed-version stores | No format migration needed | Silent corruption when raw block's first byte matches a valid AlgorithmId | Never — use store_version gating instead |
| Delay refcount overflow fix until after streaming writes | Fewer concurrent changes | Streaming increases dedup hit rate, accelerating the path to overflow | Never — overflow fix must precede streaming writes |
| Checkpoint intermediate manifests during streaming | Bounded crash-recovery loss | Refcount leaks if checkpoint manifests are not explicitly dereferenced | Only if checkpointing is explicitly required for >RAM files, with proper WAL protocol |
| Keep O(n) snapshot scan | Zero migration work | Mount time grows with snapshot count; breaks automated rotation workflows | Acceptable only until snapshot count exceeds 50; fix before any automated snapshot workflow |

---

## Integration Gotchas

| Integration | Common Mistake | Correct Approach |
|-------------|----------------|------------------|
| `State::push_bytes()` | Calling `push_bytes` with only new write data at arbitrary offsets | `State` is append-only from byte 0; maintain `OpenFileState.buf` until sequential guarantee is established |
| `State::end()` | Calling `end()` mid-stream to get a checkpoint digest and then calling `push_bytes()` on the consumed `State` | `end()` consumes `self`; you must construct a new `State` after `end()` — there is no "reset and continue" |
| `writeback_cache` + streaming | Trusting that FUSE write callbacks arrive in offset order | Writes may arrive coalesced, reordered, or with gaps under writeback_cache; always validate offset continuity |
| `store_version` gating | Reading `store_version` from the in-memory struct without verifying it was loaded from the on-disk store | `store_version` must be persisted in the store metadata (e.g., a `version` key in redb), not derived from mount flags |
| `increment_refcount` | Using `+= 1` or `wrapping_add` | Always `saturating_add(1)` to prevent overflow-to-zero data loss |
| `set_manifest` + `increment_refcount` ordering | Calling `set_manifest` before `increment_refcount` | Increment refcount first; if the process crashes between the two calls, the orphaned block (refcount > 0, no manifest) is handled safely by the next GC pass |

---

## Performance Traps

| Trap | Symptoms | Prevention | When It Breaks |
|------|----------|------------|----------------|
| Streaming fallback to full buffer on any non-sequential write | Memory usage identical to v1.0 for workloads with any `pwrite` | Detect truly-sequential writes early and stream; buffer only for mixed patterns | Any tool that pre-seeks or writes metadata headers first (e.g., MP4 muxers) |
| Lock contention on `dict: Arc<Mutex<Dictionary>>` during streaming push | Dictionary lock held for the entire streaming duration; concurrent reads block | Stream into a thread-local `Dictionary` accumulator; merge into the global dict at flush | Any filesystem with concurrent read + write on different inodes |
| Re-reading and re-hashing v1.0 blocks during migration to compute raw digest | Migration takes O(N × block size) time and reads all blocks | Only re-hash blocks that are actually being re-written; leave untouched blocks in v1.0 format | Stores with > 100GB of v1.0 data |
| O(n) snapshot scan on every mount | Mount latency grows with snapshot count; automated snapshots make this worse | Index snapshots by version number in the metadata store | > 50 snapshots |
| `statfs` computing free space by reading all block refcounts | `df` becomes an O(blocks) operation | Maintain a running physical_blocks_used counter; update on each CAS write/delete | Stores with > 1M blocks |

---

## "Looks Done But Isn't" Checklist

- [ ] **Streaming writes:** Writes sequential content and reads it back correctly — verify with `pwrite(2)` at non-sequential offsets (e.g., write tail first, then head) and confirm content is correct.
- [ ] **Streaming writes:** Works for a 10GB file — verify process memory stays below 100MB during the write (O(log N) guarantee).
- [ ] **Compression removal:** New writes produce raw blocks — verify with `hexdump` on the block store that new blocks have no AlgorithmId header byte.
- [ ] **Compression removal:** v1.0 blocks still read correctly — verify by mounting a v1.0 store with v2.0 binary and reading all files.
- [ ] **Mixed-version store:** No silent corruption — write a raw block whose first byte is `0x01` (the Zstd AlgorithmId) and verify it reads back correctly, byte-for-byte.
- [ ] **Refcount overflow:** `saturating_add` is used — verify by searching for any `+= 1` or `wrapping_add` on refcount fields.
- [ ] **Refcount overflow:** Saturation does not cause premature GC — verify a block at refcount saturation is never freed even when GC runs.
- [ ] **statfs:** `df` reports plausible values — verify `f_bfree` decreases after writing data and increases after deleting data.
- [ ] **Snapshot indexing:** 1,000 snapshots — verify `snapshot switch` completes in under 100ms regardless of snapshot count.
- [ ] **Streaming + crash:** Process killed mid-stream — verify inode content on next mount is either the pre-stream content (clean rollback) or a complete post-stream content (complete commit), never a partial write.

---

## Recovery Strategies

| Pitfall | Recovery Cost | Recovery Steps |
|---------|---------------|----------------|
| Random-write silent corruption (wrong Merkle root) | HIGH | Offline fsck: compare expected file hash against stored manifest; identify corrupted inodes; attempt recovery from backup or re-write |
| Inode size / manifest desync after crash | MEDIUM | `slicefs fsck --verify-manifests`: walk all inodes, compare `inode.size` to the byte count obtainable by walking the manifest's Merkle tree; flag discrepancies |
| Mixed-version block corruption (first byte stripped) | HIGH | No automatic recovery; must restore from backup. Prevention is the only viable strategy. |
| Refcount wrap-to-zero data loss | HIGH | Same as v1.0: offline fsck, recompute refcounts from inode scan, identify freed-but-referenced blocks |
| statfs incorrect | LOW | Rebuild the physical block count from the block store: `slicefs stats --rebuild-index` |
| O(n) snapshot scan timeout on mount | MEDIUM | Rebuild snapshot index: `slicefs snapshot reindex` — walk all snapshot records and write indexed form |

---

## Pitfall-to-Phase Mapping

| Pitfall | Prevention Phase | Verification |
|---------|------------------|--------------|
| State is append-only; random writes require buffer | v2.0 streaming write | `pwrite(2)` test at non-sequential offsets; sparse file write; truncate-then-write |
| Inode size desync from Merkle root during stream | v2.0 streaming write | Crash test mid-stream; verify size and content consistency on remount |
| Compression removal breaks hash identity v1.0 vs v2.0 | v2.0 compression-removal | Write same content in v1.0 and v2.0 mode; verify Digest224 differs (as expected); verify both read correctly |
| Mixed-version store: first byte false-positive decompression | v2.0 compression-removal | Write raw block with AlgorithmId-valued first byte; read back without corruption |
| Refcount overflow to zero | v2.0 bug-fix (before streaming) | Saturating increment test at u32::MAX; verify GC does not free saturated-refcount block |
| statfs incorrect f_bfree | v2.0 bug-fix | `df` test before/after write/delete cycle; assert monotonic decrease/increase |
| writeback_cache + out-of-order write delivery | v2.0 streaming write | Large sequential write with `writeback_cache` enabled; verify correct read-back |
| Partial streaming state not crash-safe | v2.0 streaming write | Kill -9 during large streaming write; verify clean rollback on remount |
| O(n) snapshot scan on mount | v2.0 snapshot-indexing | Create 1,000 snapshots; verify mount + snapshot switch under 100ms |

---

## Sources

- PROJECT.md v2.0 milestone description — known bugs: refcount overflow, statfs inaccuracy, O(n) snapshot scan
- SliceFS v1.0 codebase: `crates/slicefs-cli/src/filesystem.rs` — `OpenFileState`, `flush_buffer_to_cas`, `from_wire_bytes` fallback logic
- SliceFS v1.0 codebase: `crates/data-id/blockset/src/content_dependant_tree.rs` — `State = Vec<Level>`, `push_digest`, `end()`
- SliceFS v1.0 codebase: `crates/data-id/blockset/src/tree.rs` — `Tree::push_bytes`, `Tree::end` (append-only, consumes self)
- SliceFS v1.0 codebase: `crates/slicefs-compression/src/lib.rs` — `compress_block`, `decompress_block`, 1-byte AlgorithmId header format
- FUSE kernel mailing list: write vs getattr/lookup file size update race in FUSE kernel module — https://fuse-devel.narkive.com/5k4cf7XX/write-vs-getattr-lookup-file-size-update-race-in-fuse-kernel-module-test-proposed-fix
- libfuse GitHub: writeback_cache inode size staleness discussion — https://github.com/libfuse/libfuse/discussions/868
- LWN.net: "Avoiding page reference-count overflows" — https://lwn.net/Articles/786044/
- Linux kernel `refcount.h`: saturation semantics on overflow — https://github.com/torvalds/linux/blob/master/linux/include/linux/refcount.h
- MinIO blog: "Myths about Deduplication and Compression" — dedup on compressed data yields worse ratios — https://blog.min.io/myths-about-deduplication-and-compression/
- Quest Community blog: "Backup Compression and Deduplication" — hash before compress ordering — https://www.quest.com/community/blogs/b/en/posts/backup-compression-and-deduplication-good-or-bad-part-i
- ACM: "Performance and Resource Utilization of FUSE User-Space File Systems" — memory copy overhead, splicing — https://dl.acm.org/doi/fullHtml/10.1145/3310148
- USENIX FAST 2017: "To FUSE or Not to FUSE" — writeback_cache behavior — https://www.usenix.org/system/files/conference/fast17/fast17-vangoor.pdf
- Zcash incrementalmerkletree: append-only Merkle tree design — https://github.com/zcash/incrementalmerkletree

---
*Pitfalls research for: SliceFS v2.0 — Streaming Writes & Hardening*
*Researched: 2026-03-29*
