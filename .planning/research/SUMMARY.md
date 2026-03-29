# Project Research Summary

**Project:** SliceFS v2.0 — Streaming Writes & Hardening Milestone
**Domain:** Deduplicating POSIX FUSE Filesystem — incremental Merkle write path
**Researched:** 2026-03-29
**Confidence:** HIGH

## Executive Summary

SliceFS v2.0 is a focused internal hardening milestone with a single dominant goal: replace the in-memory buffer write path (`Vec<u8>` accumulate-then-flush) with a streaming Merkle tree path (`State::push_bytes` per FUSE `write()` callback). This removes the hard file-size-equals-RAM ceiling that makes v1.0 unusable for large files. The streaming API already exists in the `blockset` crate — the work is integration, not invention. No new external dependencies are required; every change is an internal refactor or a stdlib primitive swap.

Alongside streaming writes, v2.0 removes compression from the write path so that dedup hashes on raw content bytes. This is architecturally correct — hashing after compression means two files with identical raw content but different compressors do not deduplicate, which defeats the purpose of a CAS filesystem. The compression removal touches the same source file as the streaming write integration (`filesystem.rs`), so the two should be implemented together or in immediate sequence. Three additional correctness bugs — refcount integer overflow, statfs hardcoded values, and O(n) snapshot scan — are independent and can ship in any order.

The critical risks are all implementation-time correctness hazards, not design unknowns. The biggest hazard is the append-only nature of `State::push_bytes`: the FUSE `write()` callback can receive non-sequential writes (pwrite, truncate, writeback_cache reordering), and naive streaming of these into the Merkle tree produces silent corruption. A sequential-write detection guard with fallback to the v1.0 buffer model must be the first implementation decision. The second major hazard is the mixed-version block store: after compression removal, old compressed blocks and new raw blocks coexist, and per-block heuristic decompression detection is unreliable. The mitigation is `store_version` gating, not content sniffing.

---

## Key Findings

### Recommended Stack

The v2.0 milestone adds no new crates. All required APIs already exist in the workspace. The `blockset` crate exposes `Tree::push_bytes(&mut self, storage, &[u8])` and `Tree::end(self, storage) -> Digest256` on the `Tree` trait — these are the two new call sites replacing the current single `State::push_all()` call. Inode counting uses `AtomicU64` from `std::sync::atomic`. Snapshot indexing uses `HashMap` from `std::collections`. Refcount overflow protection uses `u64::saturating_add` from `std`.

**Core technology changes (delta from v1.0):**
- `Tree::push_bytes` / `Tree::end` (blockset, local): wire into FUSE write/release path — replaces `push_all`
- `u64::saturating_add` (std): prevents refcount wrap-to-zero data loss
- `AtomicU64 inode_count` (std): feeds accurate `f_files` to `statfs()`
- `HashMap<u64, SnapshotEntry>` + `HashMap<String, u64>` (std): replaces `Vec<SnapshotEntry>` linear scan

**APIs to retire from the write path:**
- `State::push_all` on the large-file path (keep for symlink targets, xattr)
- `compress_block` / `to_wire_bytes` (move to read-path legacy fallback only)
- `SliceFsFilesystem.compressor` field (demote to Option or remove)

### Expected Features

**Must have (table stakes) — v2.0 ships these:**
- Streaming write via `push_bytes` per FUSE `write()` — eliminates RAM ceiling
- Non-sequential write fallback to buffer model — correctness for pwrite, mmap, databases
- Correct `inode.size` tracking via `byte_count: u64` in `OpenFileState`
- Correct `fsync()` during streaming — materialise `end()` at fsync, reset state
- Remove write-path compression — `to_wire_bytes` removed; raw bytes into Merkle tree
- Backward-compatible read of v1.0 compressed blocks via `store_version` gate
- Refcount overflow protection (`saturating_add`) — prevent silent data loss
- Realistic `statfs` reporting — `inode_count` atomic, `f_bfree` from logical_bytes

**Should have (P2 — correctness improvement):**
- Accurate `getattr` size during open write session (return `byte_count` from `OpenFileState`)
- `slicefs stats` exposes both logical and physical block counts

**Defer to v2.1+:**
- Snapshot O(1) indexed lookup (O(n) fine until snapshot count exceeds ~50; low-effort, do as time permits in v2.0)
- Segment-level post-dedup compression (architecturally correct; significant complexity)
- Per-chunk dedup during streaming writes (requires Chunker trait at write time; v3.0 scope)
- WAL checkpointing of partial streaming state (over-engineered; truncate-on-crash is acceptable)

### Architecture Approach

The write path restructuring is confined to `OpenFileState` and its three callers (`write()`, `flush_buffer_to_cas()`, `flush_buffer_for_fsync()`). No downstream components change: `DictMetadataStore`, `blockset::State`, `blockset::Dictionary`, `GetBytes`, WAL, GC, and all FUSE lifecycle callbacks retain their current signatures. The key change is that the dict lock, previously held once at flush time for the entire `push_all` call, is now acquired and released once per `write()` callback for the incremental `push_bytes` call — same per-operation locking granularity, different distribution across time.

**Major components and responsibilities:**

1. `OpenFileState` (crates/slicefs-cli/src/filesystem.rs) — Replace `buf: Vec<u8>` + `cas_committed` with `state: blockset::State` + `byte_count: u64` + `cas_committed`. Owns all in-progress write state; `state` grows O(log N) instead of O(N).
2. `flush_buffer_to_cas` / `flush_buffer_for_fsync` (filesystem.rs) — Replace `to_wire_bytes + push_all` with `state.end(&mut dict) + dict.end(&root256)`. Produce `Digest224` from accumulated streaming state.
3. `DictMetadataStore` (crates/metadata/src/store.rs) — Add `inode_count: AtomicU64`; replace `snapshots: Mutex<Vec<SnapshotEntry>>` with two `HashMap`s; fix `increment_refcount` to use `saturating_add`.
4. `statfs()` handler (filesystem.rs) — Consume `meta.inode_count()` for `f_files`; remove hardcoded sentinel values.

**Build order (from ARCHITECTURE.md):**

- Step 1: Remove compression from write/read path (prerequisite for streaming, not consequence of it)
- Step 2: Add `byte_count` to `OpenFileState`
- Step 3: Replace `Vec<u8>` with `blockset::State`
- Step 4: Fix read-after-write for open write handles (clone+end materialisation)
- Step 5: Handle non-sequential writes with sequential detection and fallback
- Step 6: Handle truncate with in-progress streaming State
- Step 7: Remove `compressor` field / dependency from write path

### Critical Pitfalls

1. **`State::push_bytes` is append-only — random writes silently corrupt the Merkle root.** Track `next_expected_offset: u64` per file handle. If `offset != next_expected_offset`, fall back to the v1.0 buffer model for that handle. Never call `push_bytes` with data at an arbitrary offset.

2. **`writeback_cache` delivers FUSE write callbacks out of sequential order** even for logically sequential writes. Do not trust that write callbacks arrive in offset order. The sequential-write guard must validate `offset == next_expected_offset` before every `push_bytes` call.

3. **Mixed-version block store: per-block heuristic decompression detection causes silent corruption.** A raw block whose first byte matches a valid `AlgorithmId` will be silently mis-decoded. Use `store_version` gating exclusively — do not try to auto-detect block format from content bytes.

4. **Inode size and Merkle root must commit atomically.** Never write `inode.size` to durable metadata during an in-progress stream. Only commit `(manifest, size)` together at `fsync`/`release`. Keep `byte_count` in-memory only until finalisation.

5. **`cas_committed` guard must be preserved in streaming path.** If `release()` calls `flush_buffer_to_cas` on an empty `State` (already committed), `set_manifest(ino, &[])` overwrites the manifest with an empty one — silent data erasure. The guard condition changes from `buf.is_empty() && cas_committed` to `byte_count == 0 && cas_committed`.

6. **Refcount overflow with `+= 1` wraps to zero in release builds** — GC frees a still-live block. Replace with `saturating_add(1)`. This fix must precede streaming writes, which increase dedup hit rates and accelerate the path to overflow.

---

## Implications for Roadmap

The dependency graph from FEATURES.md and ARCHITECTURE.md drives a natural four-phase order. Bug fixes that are independent and low-risk ship first, then the structural write-path changes proceed in dependency order.

### Phase 1: Bug Fixes — Refcount, statfs, Snapshot Index

**Rationale:** These three changes are independent of each other and of the streaming write work. They share no state with `OpenFileState` or the write path. Shipping them first eliminates the data-loss risk (refcount overflow) and establishes the `inode_count` infrastructure that `statfs` needs — before streaming writes change how and when inodes are created. They are the lowest-risk changes: a two-line fix, adding an atomic counter, and replacing a Vec with two HashMaps.

**Delivers:** Refcount overflow protection; accurate `df` output; O(1) snapshot lookup.

**Features from FEATURES.md:** Refcount `saturating_add`; `inode_count` AtomicU64; `HashMap` snapshot indexes.

**Avoids:** Pitfall 5 (refcount overflow must be fixed before streaming increases dedup hit rates). Pitfall 6 (statfs must be accurate before streaming changes block count patterns at flush time).

**Research flag:** Standard patterns — no phase research needed. All three changes are fully specified with exact file locations and line numbers in STACK.md.

---

### Phase 2: Compression Removal from Write Path

**Rationale:** ARCHITECTURE.md explicitly calls compression removal a prerequisite for streaming, not a consequence. Both features touch `flush_buffer_to_cas` and `OpenFileState`. Doing compression removal first isolates the risk: if raw-block reads regress, the cause is unambiguously the compression change, not the streaming change. The streaming write integration then starts with a clean `push_bytes(raw_data)` call from the first commit.

**Delivers:** Raw bytes into Merkle tree on write; `store_version` bumped to 3; backward-compatible read of v1.0 compressed blocks via `store_version` gate.

**Features from FEATURES.md:** Remove `to_wire_bytes()`; gate `from_wire_bytes()` on `store_version`; keep `slicefs-compression` as read-path legacy fallback.

**Avoids:** Pitfall 3 (hash identity incompatibility v1.0 vs v2.0 — locking this in before streaming avoids hybrid write-path logic). Pitfall 4 (per-block heuristic detection — `store_version` gating established here is the only safe approach).

**Research flag:** Well-documented pattern. `store_version` gating approach is fully specified. No phase research needed.

---

### Phase 3: Streaming Write Path (Core)

**Rationale:** This is the milestone-defining change. It comes after prerequisites (bug fixes, compression removal) so the change is isolated to `OpenFileState` restructuring and the write/flush callbacks. Corresponds to Steps 2–4 of the build order: add `byte_count`, replace `Vec<u8>` with `blockset::State`, fix read-after-write via clone+end materialisation.

**Delivers:** O(log N) memory per open file handle; files of arbitrary size; correct `inode.size` tracking; correct fsync semantics; correct read-after-write on open write handles.

**Features from FEATURES.md:** Streaming write via `push_bytes` per FUSE write callback; `byte_count` size tracking; materialise-on-demand for read-after-write.

**Architecture components:** `OpenFileState` restructured; `flush_buffer_to_cas` rewritten; read-after-write path updated.

**Avoids:** Pitfall 2 (inode size desync — commit `(manifest, size)` atomically at flush only). Anti-Pattern 4 from ARCHITECTURE.md (chunked buffers as streaming proxy). Pitfall 8 (no intermediate manifests — one manifest per file-open lifetime).

**Research flag:** Well-documented patterns. Streaming API and data flow fully specified in ARCHITECTURE.md with exact call sequences. No phase research needed.

---

### Phase 4: Non-Sequential Write Handling and Edge Cases

**Rationale:** The core streaming path in Phase 3 handles the common case (sequential writes from offset 0). Phase 4 adds the correctness envelope for edge cases: non-sequential writes (pwrite, mmap-write), writeback_cache out-of-order delivery, and truncate with an in-progress streaming State. These are Steps 5–6 of the build order. They require the Phase 3 core to exist before the edge-case routing logic can be tested against real streaming behaviour.

**Delivers:** Correct behaviour for pwrite, sparse files, memory-mapped writes, and truncate-during-write; compatibility with `writeback_cache` enabled.

**Features from FEATURES.md:** Non-sequential write fallback (`next_expected_offset` detection); truncate materialise-and-rebuild; `mode: WriteMode` enum in `OpenFileState`.

**Avoids:** Pitfall 1 (the most severe correctness hazard in v2.0 — append-only State with random writes). Pitfall 7 (writeback_cache out-of-order delivery — must be tested explicitly in this phase with `writeback_cache` enabled).

**Research flag:** Testing effort is high but pattern is well-specified. No research needed. Explicit test cases required: `pwrite(2)` at non-sequential offsets, `vim`/`emacs` write patterns, `cp --sparse`, 10GB sequential write with `writeback_cache` enabled, kill-9 mid-stream crash test.

---

### Phase Ordering Rationale

- **Bug fixes before structural changes:** Refcount, statfs, and snapshot index are independent and low-risk. Shipping them first shrinks the risk surface before the invasive write-path restructuring begins.
- **Compression removal before streaming:** Both touch `flush_buffer_to_cas`. Compression first means the streaming integration always works with semantically correct raw bytes — no hybrid where compression state and streaming state are entangled.
- **Core streaming before edge cases:** Edge cases (non-sequential writes, writeback_cache) require the core streaming path to be stable and testable before routing logic can be validated.
- **No WAL checkpointing in v2.0:** The "no intermediate manifests" invariant is simpler, correct, and sufficient. Streaming `State` is O(log N) in memory — the effective file-size limit is terabytes, not gigabytes of RAM. Checkpointing is a v3.0+ concern.

### Research Flags

All four phases use well-documented patterns with no deep unknowns. Research-phase is not needed for any phase. The gaps below require implementation attention, not research.

Standard patterns (skip research-phase):
- **Phase 1:** All three changes fully specified with exact file locations and line numbers. Two-line fix, one atomic counter, two HashMaps.
- **Phase 2:** `store_version` gating pattern established in v1.0 codebase. Extend it to v3.
- **Phase 3:** Streaming API, data flow, and lock sequences fully documented in ARCHITECTURE.md.
- **Phase 4:** Edge case patterns specified in FEATURES.md and PITFALLS.md. This is a testing problem, not a research problem.

---

## Confidence Assessment

| Area | Confidence | Notes |
|------|------------|-------|
| Stack | HIGH | All changes verified against actual source files; exact line numbers cited; no new dependencies required |
| Features | HIGH | Existing codebase fully inspected; cross-checked against bup/restic/borg/bcachefs patterns |
| Architecture | HIGH | Based on direct inspection of all relevant source files with exact struct layouts and call sequences |
| Pitfalls | HIGH | v1.0 codebase read directly; cross-referenced with FUSE mailing lists, Linux refcount docs, USENIX FAST papers |

**Overall confidence: HIGH**

### Gaps to Address

- **`writeback_cache` integration test:** No existing test exercises write-callback out-of-order delivery under `writeback_cache`. Must be written before declaring Phase 4 complete. The fallback logic cannot be validated without it.
- **Mixed-version store migration path:** `store_version` gating leaves v1.0 stores permanently in a mixed-version state with no cross-epoch dedup. If operators eventually need unified dedup, a `slicefs migrate-store` command will be required. Not in scope for v2.0 but should be noted in the store format spec.
- **Dict lock ordering during `flush_buffer_to_cas`:** ARCHITECTURE.md notes that `SliceFsFilesystem.dict` and `DictMetadataStore.dict` are two separate `Mutex<Dictionary>` clones sharing underlying data via `Arc`. The exact lock acquisition sequence during finalisation needs a single canonical implementation to prevent lock inversion. Verify in Phase 3.

---

## Sources

### Primary (HIGH confidence)

- `crates/data-id/blockset/src/tree.rs` — `push_bytes` and `end` API confirmed on `Tree` trait
- `crates/data-id/blockset/src/content_dependant_tree.rs` — `State = Vec<Level>`; `Clone` via `Vec`; append-only invariant confirmed
- `crates/slicefs-cli/src/filesystem.rs` — `OpenFileState`, `flush_buffer_to_cas`, `to_wire_bytes` call sites, `statfs` hardcoded values confirmed
- `crates/metadata/src/store.rs` — `increment_refcount` overflow at line 136; `snapshots: Mutex<Vec<SnapshotEntry>>` linear scan confirmed
- `crates/slicefs-compression/src/lib.rs` — `AlgorithmId` byte header format; `compress_block` / `decompress_block` confirmed
- `.planning/PROJECT.md` — v2.0 milestone definition and known bug list
- statfs(2) man page — `f_files` / `f_ffree` / `f_bfree` semantics
- Rust Reference — integer overflow wrapping in release mode; `saturating_add` semantics
- Linux kernel `refcount.h` — saturation semantics on overflow

### Secondary (MEDIUM confidence)

- USENIX FAST 2017: "To FUSE or Not to FUSE" — `writeback_cache` behavior and write reordering
- libfuse GitHub discussions/868 — `writeback_cache` inode size staleness
- FUSE kernel mailing list — write vs getattr/lookup file size race in FUSE kernel module
- MinIO blog: "Myths about Deduplication and Compression" — dedup on compressed data yields worse ratios
- Zcash incrementalmerkletree — append-only Merkle tree design and invariants

### Tertiary (LOW confidence)

- bup/restic community comparison discussions — streaming model tradeoffs (community consensus, not official docs)

---

*Research completed: 2026-03-29*
*Ready for roadmap: yes*
