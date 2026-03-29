# Feature Research

**Domain:** Deduplicating POSIX FUSE Filesystem — v2.0 Streaming Writes & Hardening Milestone
**Researched:** 2026-03-29
**Confidence:** HIGH (existing codebase fully inspected; patterns cross-checked against bup/restic/borg/bcachefs documentation)

---

## Milestone Context

v1.0 is complete: full POSIX write path, WAL crash safety, GC, Zstd/LZ4/None compression, snapshots, stats/scrub CLI, macOS FUSE-T + Linux support.

v2.0 goal: remove the file-size-equals-RAM limitation, remove write-path compression (so dedup operates on raw content), and fix three known correctness bugs.

Existing write path (v1.0): all writes accumulate in `OpenFileState.buf: Vec<u8>` in memory, then flush atomically to `State::push_all()` on release/fsync. This is the buffer-flush model — correct, simple, but limited to files that fit in available RAM.

Target write path (v2.0): replace the single-buffer model with incremental `State::push_bytes()` streaming through the Merkle tree, bounded O(log N) memory per open file handle.

---

## Feature Landscape

### Table Stakes (Users Expect These)

Features that the v2.0 milestone must deliver. Missing any = the milestone is incomplete or introduces data-loss risk.

| Feature | Why Expected | Complexity | Notes |
|---------|--------------|------------|-------|
| Unbounded file writes (no RAM ceiling) | Current limit blocks any file > available free RAM; unusable for large video, disk images, databases | HIGH | Replace `OpenFileState.buf: Vec<u8>` with `(State, Dictionary)` stream pair; `push_bytes()` per FUSE write call |
| Streaming write: correct final digest on release | `State::end()` must be called once at release/fsync time; manifest must store the resulting `Digest224` | MEDIUM | Already exists in blockset API; need to call `end()` correctly at release, not at each `write()` |
| Streaming write: correct inode.size tracking | `inode.size` must reflect logical bytes written, not tree size | MEDIUM | Accumulate byte count in `OpenFileState` alongside the `State`; update on release |
| Streaming write: fsync mid-file | FUSE may call `fsync()` before `release()`; streaming state must be checkpointable or serialisable | HIGH | Most systems buffer to disk and re-load; simplest approach: materialise `State::end()` + snapshot manifest at fsync, continue streaming from empty state after |
| Remove write-path compression | Dedup must operate on raw content bytes; compressor currently applied before `State::push_all()` means `Digest224` is over compressed bytes — cross-compressor dedup impossible | MEDIUM | Remove `to_wire_bytes()` call from `flush_buffer_to_cas()`; store raw bytes in Merkle tree |
| Remove read-path decompression for new blocks | Read path calls `from_wire_bytes()` which decompresses; new raw blocks must return without decompression attempt | MEDIUM | Gate decompression on a per-block header or a new store_version value (e.g. v3) |
| Backward compat: old compressed blocks still readable | Existing v1.0 stores have compressed blocks; migration must be seamless | MEDIUM | Keep `from_wire_bytes()` as a fallback; check `store_version` to decide whether to attempt decompression |
| Refcount overflow protection | `*rc.entry(*digest).or_insert(0) += 1` wraps on overflow (`u64`); silent wrap to 0 = GC deletes live data | MEDIUM | Change to `saturating_add(1)` or `checked_add(1).unwrap_or(u64::MAX)`; add a saturated-refcount warning log |
| Realistic statfs reporting | `f_files` is hardcoded to 1,000,000; `bfree` is unrealistic — tools like `df` and POSIX compliance tests check these | MEDIUM | Track inode count via `AtomicU64 inode_count` in `DictMetadataStore`; compute `f_bfree` from `logical_bytes` vs estimated capacity |
| Snapshot indexed lookup | `snapshots: Vec<SnapshotEntry>` is O(n) scan by version and by name; becomes visible latency at thousands of snapshots | LOW | Add `HashMap<u64, usize>` (version → index) and `HashMap<String, usize>` (name → index) rebuilt on `set_snapshots()` and appended on `create_snapshot()` |

### Differentiators (Competitive Advantage Beyond Table Stakes)

Features that go beyond the mandatory fixes and would make v2.0 a materially stronger product. Not required for the milestone definition but worth noting as future-phase candidates.

| Feature | Value Proposition | Complexity | Notes |
|---------|-------------------|------------|-------|
| Streaming writes with zero-copy FUSE splice | FUSE `write()` can receive kernel buffer pointers with splice(2) on Linux; avoids user-space memcpy for large sequential writes | HIGH | Requires `fuser` splice support; not in scope for v2.0 but architecturally enabled by streaming model |
| Per-chunk incremental dedup during write | Instead of one `Digest224` per file, chunk file during streaming writes, store one `Digest224` per chunk — enables partial-file dedup | HIGH | Needs `Chunker` trait integration at write time; current architecture pushes raw bytes byte-by-byte to `State`; chunking is currently only applied at seed time |
| Write coalescing: group small writes before push | FUSE delivers writes in 128KB–4MB pages; multiple `write()` calls per file in rapid succession; coalescing into larger push calls reduces Merkle tree height | MEDIUM | Could batch `push_bytes()` calls per-handle; already implicit if streaming is per-FUSE-write |
| WAL entries for in-flight streaming state | If process crashes mid-stream, the partial bytes are lost; WAL could checkpoint streaming state so large writes survive crash | VERY HIGH | Over-engineered for v2.0; truncate-on-open is acceptable semantics for crash mid-write |

### Anti-Features (Commonly Requested, Often Problematic)

| Feature | Why Requested | Why Problematic | Alternative |
|---------|---------------|-----------------|-------------|
| Re-introduce write-path compression | Reduces disk I/O for incompressible data | Dedup operates on compressed bytes — `Digest224` changes per-compressor; same raw content compressed with Zstd vs LZ4 produces different digests = no cross-tool dedup; also requires decompression on every read | Store raw in Merkle tree; apply post-dedup segment-level compression as a v2.1 option (already in PROJECT.md out-of-scope list) |
| Random-write in-place update without re-materialise | Users ask "why must the whole file re-stream on a single-byte change?" | CAS is structurally append-only; in-place mutation requires either copy-on-write of affected tree nodes (complex, like btrfs B-tree COW) or a write-ahead log of deltas (complex, like BTRFS extent tree); both require significant redesign of the Merkle tree internals | Accept whole-file re-stream on modify for v2.0; per-chunk dedup in v3.0 reduces waste |
| Streaming write with mid-stream seek | `write(fd, buf, offset)` where offset < current position requires rewriting already-pushed tree nodes | Merkle tree is a write-once, append-only structure; seeks backwards break the invariant | Buffer writes for files where non-sequential writes are detected; fall back to whole-buffer model for that handle |
| Segment-level compression at write time | Compress groups of blocks together for higher ratio | Requires second-pass over already-written data; adds latency spike at segment boundary; blocks read access until segment closed | Out of scope per PROJECT.md; defer to v2.1 |
| Changing compressor mid-store without full re-write | Users want to switch from Zstd to LZ4 or None after migration | Old blocks keep their existing header; new blocks get new compressor; `get_refcount` / GC logic must handle mixed-format store; dedup across mixed-format blocks is lost | Document "one compressor per store" policy; provide a `slicefs migrate-compressor` command in v3.0 |

---

## Feature Dependencies

```
[Streaming write path]
    requires  --> [State + Dictionary per open handle in OpenFileState]
    requires  --> [State::push_bytes() called per FUSE write()]
    requires  --> [State::end() called on release()/fsync()]
    enables   --> [Unbounded file sizes]

[Remove write-path compression]
    requires  --> [Streaming write path completed first] (both touch flush_buffer_to_cas)
    requires  --> [store_version bump to v3]
    requires  --> [Backward compat read path for old compressed blocks]

[Refcount overflow fix]
    independent -- no dependencies

[Realistic statfs]
    requires  --> [inode_count AtomicU64 in DictMetadataStore]
    enhances  --> [statfs() FUSE callback accuracy]

[Snapshot indexed lookup]
    independent -- no dependencies
    enhances  --> [snapshot list/switch CLI commands]
```

### Dependency Notes

- **Streaming write requires compression removal to be done last (or together):** Both features touch `flush_buffer_to_cas()` and `OpenFileState`. Doing them in separate phases on the same struct avoids a double-rewrite — implement streaming first with compression still present, then remove compression as a follow-on diff, OR implement both together.
- **store_version bump required for compression removal:** Existing stores are v2 (compressed blocks). Removing write-path compression must bump to v3 so the read path knows new blocks are raw. Without a version bump, `from_wire_bytes()` will attempt to decompress raw blocks and corrupt reads.
- **Backward compat is mandatory:** v1.0 stores (version < 2) have raw blocks; v2.0 stores have compressed blocks; v3.0 stores (after compression removal) have raw blocks again. The read path must handle all three cases cleanly.
- **Refcount fix and statfs fix are independent:** No shared state; either can ship in any order.
- **Snapshot indexing is low risk:** `Vec<SnapshotEntry>` → `(Vec, HashMap<u64, usize>, HashMap<String, usize>)`; purely additive change.

---

## How Streaming CAS Writes Work (Research Findings)

### The blockset `State` API (HIGH confidence — source code inspected)

`blockset::State` (alias for `Vec<Level>`) already implements the streaming Merkle tree. The write API is:

```rust
state.push_bytes(&mut dictionary, &chunk);   // O(log N) memory per call
let root = state.end(&mut dictionary);       // finalise tree, returns Digest224 (via push_all)
```

`push_bytes()` processes bytes through a content-dependent tree (CDT) algorithm: bytes are accumulated into `MerkleTreeState` levels; when a level's rolling threshold fires, a new parent node is emitted. Tree height grows as O(log N) of total bytes pushed, so memory usage is bounded by the number of levels, not the file size.

The existing `flush_buffer_to_cas()` already calls `State::push_all()`, which internally calls `push_bytes()` + `end()`. The only change needed is to split these across the `write()` and `release()` FUSE callbacks:

- `write()`: call `push_bytes()` with the incoming data slice
- `release()` / `fsync()`: call `end()` to get the root, store as manifest

### The Random-Write Fallback Problem (MEDIUM confidence — inference from architecture + restic/bup patterns)

CAS Merkle trees are write-once, append-only. A `write(fd, buf, 0)` after data has been pushed to a `State` cannot retroactively modify already-committed tree nodes. Three approaches exist in production systems:

1. **Whole-buffer fallback (bup/restic model):** Detect non-sequential writes (offset != current stream position) and fall back to accumulating the full file content in a scratch buffer, then re-stream on release. Simple; correct; accepts memory cost for files written non-sequentially. This is the recommended approach for v2.0.

2. **Copy-on-write tree nodes (btrfs/bcachefs model):** Walk the Merkle tree to the affected leaf nodes, copy and replace them, rebuild parent hashes up to the root. Correct and memory-efficient but requires addressable tree node storage and is a major architectural addition — not appropriate for v2.0.

3. **Write-ahead delta log (ZFS intent log model):** Buffer random writes as a delta log, replay on read, merge periodically. Complex; adds read-path complexity; not appropriate for a CAS-only store.

**Recommendation for v2.0:** Use approach 1. `OpenFileState` tracks `write_pos: u64`. On `write(offset, data)`, if `offset == write_pos`, call `push_bytes()` and advance `write_pos`. If `offset != write_pos` (seek or non-sequential write), set a `fallback: bool` flag and accumulate into a `Vec<u8>` buffer. On release, if `fallback = true`, re-stream the buffer from scratch; if `fallback = false`, call `end()` directly.

In practice, the FUSE kernel buffer manager delivers writes in sequential pages for most workloads (cp, cat, editors writing via rename-on-save). Non-sequential writes occur mainly with memory-mapped writes and database files that do partial updates — these are the cases that fall back to the buffer model.

### Incremental Merkle Tree Construction: Table Stakes vs Differentiators (HIGH confidence — blockset source + academic literature)

**Table stakes (must work correctly):**
- Final `Digest224` produced by streaming `push_bytes()` + `end()` must be identical to the `Digest224` produced by `push_all()` on the same bytes. This is guaranteed by the blockset API — both paths use the same CDT algorithm.
- Tree height stays O(log N) regardless of file size. Verified in blockset source: `Vec<Level>` length is the tree height; each level fires when its content-dependent threshold triggers.
- `end()` can only be called once; subsequent pushes after `end()` would create a new tree. This is correct — `release()` consumes the state.

**Differentiators (not required for v2.0):**
- Content-dependent chunking at write time (rolling hash CDC): instead of pushing raw bytes one-at-a-time through the CDT algorithm, chunk the stream with Rabin/FastCDC first, push chunk digests instead of byte digests. This enables cross-file dedup at the chunk level. The blockset `State` already supports `push_digest()` for this pattern — deferred to v3.0 per PROJECT.md scope.
- Incremental tree update on partial rewrite: reuse unchanged subtrees. Requires storing the tree structure addressably. Not implemented in blockset; deferred.

---

## MVP Definition for v2.0 Milestone

### Ship in v2.0

- [ ] **Streaming write via `State::push_bytes()` per FUSE `write()` callback** — eliminates file size = RAM ceiling; core milestone goal
- [ ] **Non-sequential write fallback to buffer model** — correctness for O_RDWR, mmap, database workloads
- [ ] **Correct `inode.size` tracking during streaming** — byte counter in `OpenFileState`, not tree size
- [ ] **Correct `fsync()` during streaming** — materialise `end()` at fsync, reset state for continuation writes (simplest approach: accept that fsync breaks streaming; resume as new stream after fsync)
- [ ] **Remove write-path compression** — `to_wire_bytes()` returns raw bytes; store_version bumped to 3
- [ ] **Backward-compat read path for v1/v2 compressed blocks** — `from_wire_bytes()` checks header presence based on store_version
- [ ] **Refcount overflow protection (`saturating_add`)** — prevents silent data loss on overflow
- [ ] **Realistic `statfs` reporting** — `inode_count` atomic, `f_bfree` computed from logical_bytes

### Defer to v2.1 or Later

- [ ] **Segment-level post-dedup compression** — in PROJECT.md out-of-scope; adds significant complexity
- [ ] **Per-chunk dedup during streaming writes** — requires Chunker trait at write time; v3.0
- [ ] **Snapshot indexed lookup** — O(n) is fine until thousands of snapshots; add to v2.0 if trivially low-effort, else v2.1

---

## Feature Prioritization Matrix

| Feature | User Value | Implementation Cost | Priority |
|---------|------------|---------------------|----------|
| Streaming write path | HIGH — removes hard RAM ceiling for large files | HIGH — touches OpenFileState, flush_buffer_to_cas, fsync path | P1 |
| Remove write-path compression | HIGH — dedup on raw content; cross-tool correctness | MEDIUM — touches to_wire_bytes, store_version | P1 |
| Refcount overflow fix | HIGH — data loss risk | LOW — two-line change in increment_refcount | P1 |
| Realistic statfs | MEDIUM — affects tooling (df, quota checks, pjdfstest) | MEDIUM — add inode_count AtomicU64, update statfs callback | P2 |
| Non-sequential write fallback | MEDIUM — correctness for database / mmap writes | MEDIUM — flag in OpenFileState, routing logic in write() | P1 (correctness) |
| Snapshot indexed lookup | LOW — only relevant at thousands of snapshots | LOW — add two HashMaps to DictMetadataStore | P3 |

**Priority key:**
- P1: Must have for v2.0 — milestone-defining
- P2: Should have — observable correctness improvement
- P3: Nice to have — performance optimization

---

## Competitor Feature Analysis

| Feature | bup | restic | ZFS dedup | Our Approach |
|---------|-----|--------|-----------|--------------|
| Streaming write model | Split file into chunks via rolling hash, stream chunk digests | Pack files into chunks, write whole packfile before indexing | Fixed-size block level, in-kernel write path | `State::push_bytes()` CDT; one Merkle node per logical byte initially (v2.0); per-chunk in v3.0 |
| Random write handling | Re-process file from scratch on backup run | Re-pack changed chunks on next backup | Block-level COW in kernel; ZFS manages it at extent level | Non-sequential write fallback to buffer; re-stream on release |
| Compression | Write-time, per-object | Write-time, per-pack chunk | Write-time, LZ4/ZSTD per-block | Remove from write path in v2.0; deferred segment-level post-dedup compression in v2.1 |
| Dedup scope | Cross-file, cross-backup, content-defined chunks | Cross-file, cross-backup, content-defined packs | Block-level, per-pool, online dedup | Cross-file, whole-file (v2.0); cross-file per-chunk (v3.0) |
| Refcount model | Git-like object ref counting via pack index | Pack reference counted; GC via `forget` + `prune` | In-kernel DDT with overflow saturation | `BTreeMap<Digest224, u64>` with saturating_add (v2.0 fix) |

---

## Sources

- blockset source code: `/Volumes/Unitek-B/Projects/file-systems/crates/data-id/blockset/src/` (HIGH confidence — direct inspection)
- slicefs-cli filesystem.rs: `/Volumes/Unitek-B/Projects/file-systems/crates/slicefs-cli/src/filesystem.rs` (HIGH confidence — direct inspection)
- metadata store.rs: `/Volumes/Unitek-B/Projects/file-systems/crates/metadata/src/store.rs` (HIGH confidence — direct inspection)
- PROJECT.md v2.0 milestone definition: `/Volumes/Unitek-B/Projects/file-systems/.planning/PROJECT.md` (HIGH confidence)
- [bup streaming model — GitHub restic/others comparison](https://github.com/restic/others/issues/21) (MEDIUM confidence — community discussion)
- [bcachefs snapshot btree indexed lookup](https://bcachefs.org/Snapshots/) (MEDIUM confidence — official bcachefs docs)
- [Linux kernel refcount_t overflow protection — LWN](https://lwn.net/Articles/728675/) (HIGH confidence — kernel documentation)
- [statfs(2) man page — f_files / f_ffree semantics](https://man7.org/linux/man-pages/man2/statfs.2.html) (HIGH confidence — official POSIX man page)
- [Content-defined Merkle Trees for Container Delivery — arxiv](https://arxiv.org/pdf/2104.02158) (MEDIUM confidence — academic paper on streaming CDT construction)

---
*Feature research for: SliceFS v2.0 Streaming Writes & Hardening milestone*
*Researched: 2026-03-29*
