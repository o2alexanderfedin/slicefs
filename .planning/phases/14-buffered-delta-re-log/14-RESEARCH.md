# Phase 14: Bounded-memory random writes - Research

**Researched:** 2026-11-18
**Domain:** FUSE write path (overlay on a content-defined chunk tree), FastCDC re-chunk and resync, write-log relog, online GC
**Confidence:** HIGH for the code inventory and the FastCDC behavior (read in source and measured); MEDIUM for the cost and memory numbers (estimates, to be measured in Wave 0 and the Docker run)

<user_constraints>
## User Constraints (from CONTEXT.md)

### Locked Decisions

Direction set by the user (2026-11-18), verbatim:
- No full-file buffer anywhere. `WriteMode::Buffered { buf: Vec<u8> }` and every `handle_content` call that reads a whole file into RAM go away. — **Reversibility:** costly — core write path of the filesystem
- Memory of an open file is bounded by a fixed limit (dirty bytes plus about one chunk of work space), independent of the file size. Tested in Docker with a 1 GiB and a 4 GiB random-write workload under the same VmHWM cap. — **Reversibility:** costly

Proposed design from #98 (the planner confirms or improves it after research), verbatim:
- Patch overlay: base = manifest (chunk list) of the last committed version, kept in the chunk store; dirty extents = sparse map offset -> bytes; reads merge both; truncate/extend change only size and extents.
- Rebase at a dirty-bytes limit (for example 64 MiB), at fsync and at close: re-chunk only near the dirty extents (start at the chunk boundary before an extent, feed base + dirty bytes through FastCDC until a new boundary meets an old boundary after the extent), then replace that range of chunk references in the manifest. Unchanged chunks keep their IDs.
- Write log: dirty extents are already `Write` records; a commit re-logs only the dirty extents since the last rebase (fixes #93).
- GC: a rebase outside the commit gate is protected from online GC (GC root or rebase/GC lock), with a test that runs GC during a rebase.
- Open questions: can the manifest replace a range of chunk references without a full rewrite of the chunk list; is a global limit over all open handles also needed.

SUPERSEDED (pre-2026-11-18 scope), verbatim:
> The "Decided autonomously" items below were written for the old scope. Keep only what still applies (replay rules, shrink-then-extend zeros, write aliases on the owner state, test_write taking the state out of open_files, GC race). Ignore anything that keeps a full-file buffer.

Items of the old "Decided autonomously" list that still apply (the rest assume a full-file buffer and are void):
- Record format: no new Op. `Op::Write{ino,offset,data}` (tag 12) and `Op::Setattr{size:Some(..)}` (tag 9) on top of the base manifest in the root; oplog.rs encode/decode unchanged.
- Record size limit: split each relogged range (MAX_PAYLOAD = 64 MiB, segment/mod.rs:186). This research recommends 1 MiB pieces, not 16 MiB (see Pitfall 6).
- Shrink then extend: log `Setattr size=<smallest size since the base>` before the final-size Setattr and the writes, so zeros come back on replay.
- Write aliases (`write_alias`, `owner_fh`): the overlay sits on the owner state.
- `test_write` takes the state out of `open_files` (filesystem.rs:723).
- Refcounts: install of a base increments the new root and decrements the old one if it differs (pattern at filesystem.rs:901-912).
- GC race: a base root that only the handle knows must be a GC root.
- Tests: extend write_log_replay_tests.rs (a_commit_keeps_the_times_of_a_buffered_file at 290, fsynced_buffered_bytes_survive_a_commit_and_a_crash at 331), a test hook for relogged bytes.

### Claude's Discretion
- Test names, helper layout and plan split are at Claude's discretion.

### Deferred Ideas (OUT OF SCOPE)
Verbatim from CONTEXT.md:
> Spill of large Buffered files to an on-disk chunked form; an incremental CDC re-chunk or splice API in chunk-store (FileWriter has no edit or patch operation; file.rs:99-205); a re-log reduction for Streaming pending out-of-order writes (usually small, bounded by REORDER_WINDOW = 64 MiB at filesystem.rs:41). Note: a full pending window can itself exceed MAX_PAYLOAD only per record; each pending entry is at most one FUSE write, so it is fine. A CLI flag for the rebase threshold is also deferred.

**STALE DEFERRAL (flag for the planner):** this list was written for the old scope. The user's 2026-11-18 direction ("re-chunk only near the dirty extents ... replace that range of chunk references in the manifest") *is* the incremental re-chunk / splice API in chunk-store. Under the new scope it is in scope; without it, a rebase costs O(file size) CPU. "Spill of large Buffered files" is also replaced by the overlay itself (the base is already on disk). The CLI flag and the Streaming-pending relog reduction stay deferred.
</user_constraints>

<phase_requirements>
## Phase Requirements

| ID | Description | Research Support |
|----|-------------|------------------|
| BUFR-01 | A commit with a large open file that is written at random writes to the write log an amount that depends on the bytes changed since the last commit, not on the file size. | Q4: relog only dirty extents since the last rebase, in 1 MiB pieces, after `meta.commit()`; base is always in the manifest. Bound: at most the dirty limit per handle. Gap vs the strict reading: Open Question 2. |
| BUFR-02 | Replay after a crash still gives the same content, size and times (tests in `write_log_replay_tests`, for PerOp and FlushOnFsync). | Q4: record order `Setattr(visible)`, `Setattr(size)`, `Write` pieces, ctime Setattr; replay paths verified in oplog.rs:276-306. |
| BUFR-03 | The commit gate is not held for the hash of a whole file. Online GC that runs during a rebase loses no chunk (test). | Q5/Q6: rebase runs inside FUSE callbacks under `gate.read()`, never in `commit_cut`; base in manifest + `file_roots`; deterministic GC-interleave test. |
| BUFR-04 | A random write never loads the whole file into RAM. The memory of an open file is bounded by a fixed limit: a 1 GiB and a 4 GiB random-write test in Docker stay below the same VmHWM cap. | Q1 inventory (every O(file) site), overlay with a cost-accounted dirty limit, streaming rebase, counting-allocator test, Docker test design with the index caveat. |
| BUFR-05 | After a small change in a large file, the store grows by about the changed chunks only (unchanged chunks keep their IDs). | Q2/Q3: canonical splice (same cuts as a full re-chunk; measured), unchanged leaves dedup by ID; tolerance must include path nodes. |
</phase_requirements>

## Project Constraints (from CLAUDE.md and project memory)

There is no `./CLAUDE.md` in the repo. The user's global CLAUDE.md and the project memory give these directives:
- Avoid work with little or no value (global CLAUDE.md).
- No macOS mounts in automated tests. Linux FUSE tests run in Docker (OrbStack) through `scripts/docker-linux-test/run.sh`. FSKit is the only macOS backend.
- Keep test and CI waits short. Run the Docker `verify` only when the change touches mount, unmount or crash paths. Long runs (Boost, and here the 4 GiB run) run by hand. Merge on CI green.
- No store backward compatibility is needed (no deployments). A format change needs no migration.
- Global/decentralized goal: full BLAKE3-256 IDs; assume content from untrusted peers in design choices (dedup, GC, IDs). This favors canonical chunking (the same bytes give the same chunks on every node).
- Never mention licensing in public GitHub text.
- Do not commit this research file (task instruction).

## Summary

Today a handle switches to `WriteMode::Buffered { buf: Vec<u8> }` on the first non-sequential write, and `handle_content` reads the whole file into that `Vec` (filesystem.rs:591-602, 785). The research found eleven places that hold or build O(file) memory (Q1 table). They include the setattr truncate of a Streaming handle (1284-1292), the full-buffer relog clone at each commit (1078), and `put_all`, which copies its whole input into `FileWriter.buf` before the first cut (file.rs:122-128). All of them go away if the Buffered state becomes an **overlay**: a base root that is already in the manifest, a sorted extent map of written bytes, a `visible` length (base bytes that still show after a shrink) and a logical `size`. Reads merge base, extents and zeros. A **rebase** re-chunks only around the extents and splices the new chunk references into the manifest tree.

FastCDC v2020 in this repo (4/8/32 KiB, Level 1) decides each cut from the bytes since the chunk start, and the byte *at* the cut is hashed (fastcdc v2020/mod.rs:322-331). So a rebase must restart at the largest old boundary **strictly before** the first changed byte, and it may stop at the first new cut at or past the change end that equals an old boundary. From there on the result is the same as a full re-chunk. The oracle run below confirms this. On random data resync comes within 55 KiB in every measured trial. On zeros or periodic data, about half the trials never resync before EOF. So the resync distance has **no bound**, and the rebase must stream with fixed memory. A canonical zero-run fast path removes the zero case. The manifest is a tree whose nodes are cut by entry ID, so the same resync idea works one level up. No splice API exists: `Entry`, `Node`, `push` and the node cursor are private. A new splice module in `chunk-store/src/file.rs` is the core new code.

Placement: run the rebase **synchronously in the FUSE callback** that crosses the dirty limit, and at release. Do not run it in `commit_cut` or in a background thread. fuser runs one thread (`n_threads.unwrap_or(1)`, vendor/fuser/src/session.rs:257), and each callback holds `commit_gate.read()`. So a rebase can never straddle `gc_begin`. Only a synchronous rebase in the write path bounds memory. The GC hazard that remains is a base root that only the handle knows. Fix it by installing every new base in the manifest at once, and by adding overlay bases to `file_roots` in `gc_start`.

**Primary recommendation:** replace `WriteMode::Buffered` with `WriteMode::Overlay(Overlay)`. Add a streaming, canonical `chunk_store::file::splice` (a leaf cursor and a writer that accepts existing entries). Rebase in the callback at a cost-accounted dirty limit (start at 32 MiB, set from measurement) and at close. Relog only the extents since the last rebase, in 1 MiB pieces, after `meta.commit()`. Ship a tracer first (overlay plus a naive full re-stream rebase), then swap in the splice.

## Architectural Responsibility Map

| Capability | Primary Tier | Secondary Tier | Rationale |
|------------|-------------|----------------|-----------|
| Overlay state (extents, visible, size, cost) | Daemon RAM: new `slicefs-cli/src/overlay.rs` (pure data structure) | filesystem.rs (owner state in `open_files`) | Pure, no I/O, unit-testable against a `Vec<u8>` model |
| Merged read (base + extents + zeros) | filesystem.rs `test_read` | content.rs `Reader` (base range) | Lock held only to copy the extents of the range (#21 rule) |
| Re-chunk and manifest splice | chunk-store `file.rs` (new `splice`) | content.rs (thin wrapper) | Needs private `Entry`/`Node`/`push`; owns the canonical-tree invariant (file.rs:13-15) |
| Rebase trigger and install (refcounts, manifest, stamp) | filesystem.rs (write, release, setattr) | metadata `set_manifest`, refcounts | Same pattern as finalize_write 901-912 |
| Relog at commit | filesystem.rs `commit_cut` | oplog.rs (unchanged) | Existing records; only the content of the relog changes |
| GC safety | filesystem.rs `gc_start` (file_roots) | chunk-store `gc_begin` touched set | No new lock needed under one FUSE thread |
| Bounded-memory proof | tests: counting allocator (in-process), Docker VmHWM (Linux FUSE) | scripts/docker-linux-test | No macOS mounts in automation |

## Standard Stack

Nothing new. The phase uses what the workspace already has.

### Core
| Library | Version | Purpose | Why Standard |
|---------|---------|---------|--------------|
| fastcdc | 3.2.1 (Cargo.lock:272-273; `fastcdc = "3.2"` at Cargo.toml:29) | v2020 content-defined chunking through `chunk_store::chunk_ranges` | Already defines every chunk in the store; the splice must give the same cuts |
| chunk-store (in repo) | workspace | `FileWriter`, `FileReader`, node format, `put`/`read`, GC touched set | Owns the canonical tree; the splice goes in its `file.rs` |
| fuser (vendored) | 0.18 patched (`vendor/fuser`, Cargo.toml:27, 36-38) | FUSE session, one thread by default | Unchanged |

### Supporting
| Library | Version | Purpose | When to Use |
|---------|---------|---------|-------------|
| std `BTreeMap` | std | Extent map of the overlay | Always (no new crate; #21 option B with `im`/`rpds` is later) |
| std `alloc::System` + `GlobalAlloc` | std | Counting allocator in its own test binary | BUFR-04 in-process test |
| tempfile | workspace | Test stores | Tests |
| python3 | 3.11.2 in the `slicefs-fusetest` image | Seeded random-write workload and reference file | Docker run |

### Alternatives Considered
See "Alternatives considered" under Architecture Patterns.

**Installation:** none.

## Q1. Every full-file (O(file)) buffer today

All lines below were read this session.

| # | Site | What holds O(file) | Evidence |
|---|------|--------------------|----------|
| 1 | `WriteMode::Buffered` | `buf: Vec<u8>` of the whole file, until the last close | filesystem.rs:57-59 `Buffered { buf: Vec<u8>, },` |
| 2 | `handle_content` | `content::get_all` of the stream finish, or of `m[0]`, into one Vec | filesystem.rs:591-602; get_all at content.rs:247-251 reads `r.read(cs, 0, n)` |
| 3 | Streaming to Buffered switch in `test_write` | `handle_content` + `buf.resize(min_size)` + pwrites; also drops the refcount of `last_committed_root` although the manifest still points at it | filesystem.rs:781-801 (785 `let mut buf = self.handle_content(...)`, 786-788 resize, 794-800 decrement) |
| 4 | Buffered pwrite, sparse write far past the end (#23) | `pwrite_into` resizes the Vec to `offset+len`: a write at 900 GiB allocates 900 GiB (capped only by MAX_FILE_SIZE = 1 TiB) | filesystem.rs:264-271, 804-810; MAX_FILE_SIZE at 33-37 says "Out-of-order writes and truncates still hold the whole file in memory" |
| 5 | Read of a Buffered file | served from `buf` (range copy only, but needs the whole buf to exist) | filesystem.rs:961-966 |
| 6 | `finalize_write` (release, destroy) | `content::put_all(&buf)`; `put_all` pushes the whole slice into `FileWriter.buf` before the first cut, so a 1 GiB buffer is copied once more (2x peak) | filesystem.rs:892; content.rs:239-243; file.rs:122-128 (`self.buf.extend_from_slice(data)` then cut) |
| 7 | `flush_handle` Buffered branch | `buf.clone()` + `put_all`; dead code today (every caller passes `include_buffered = false`: 1039, 1052) | filesystem.rs:1136-1148, 1162 |
| 8 | `commit_cut` relog | `buf.clone()` into one `Op::Write` of the whole file at every commit (#93); a record over MAX_PAYLOAD (64 MiB) stops the reader, so it and every later record are lost on replay | filesystem.rs:1076-1079; segment/mod.rs:186 `pub(crate) const MAX_PAYLOAD: usize = 64 << 20;`; reader.rs:60 `if payload_len > super::MAX_PAYLOAD \|\| seq != self.next_seq { return None; }` |
| 9 | `test_setattr_size`, Buffered | `buf.resize(new_size)`: `truncate -s 1T` on a Buffered handle allocates 1 TiB | filesystem.rs:1254-1256 |
| 10 | `test_setattr_size`, Streaming, non-zero size that is not a pure extension | `handle_content` + `content.resize` + `fresh.push(&content)` (the push copies it once more, see 6) | filesystem.rs:1276-1294 |
| 11 | `gc_start` | not O(file), but a Buffered handle gets no file root (`WriteMode::Buffered { .. } => None`); correct today only because Buffered bytes are in RAM | filesystem.rs:502-507 |

Not O(file), confirmed bounded:
- Case B of `test_setattr_size` (no open handle) streams in 1 MiB pieces (filesystem.rs:1347-1368). Memory is fine, but CPU is O(new size). The splice makes it O(log N).
- `fill_stream` and the sparse jump past `REORDER_WINDOW` stream zeros with O(1) memory (filesystem.rs:241-261, 739-749).
- `end_padded` / `FileWriter::write_zeros` use a 64 KiB zero buffer (file.rs:131-140). Memory is fine, CPU is O(zeros).
- `readlink` uses `get_all` on a symlink target (filesystem.rs:1847), and symlink creation uses `put_all` (1824); both are small by nature.
- Whole-crate grep for `get_all|put_all|read_to_end|fs::read(` over `crates/*/src`: the remaining hits are offline tooling or not file content. `slicefs seed` reads each host file whole (`std::fs::read(path)` then `put_all`, seed.rs:118-122); this is offline tooling, not the daemon, so it is out of this phase (a follow-up could stream it). `pack_io.rs:161` reads a pack-side file and `cas-local/disk_block_store.rs:109` is the legacy block store; neither holds file content in the daemon write path.
- The write at open of an existing file is not a separate path. `test_open` makes a Streaming state (filesystem.rs:1903-1924). The first write is "not fresh" when `has_content` (734), so it goes to site 3.
- Write aliases share one owner state (filesystem.rs:201-205, 1910-1914); the heir logic in `test_release` moves it (835-856). Nothing is O(file).

Other RAM holders that grow with **written bytes**, not with file size (out of the strict BUFR-04 scope; see Open Questions 4-5):
- `FlushOnFsyncWal` and `PeriodicWal` keep every `WalEntry` (with the Write bytes) in a `Vec` until `flush_and_sync` (wal/flush_on_fsync.rs:13-15, 53-56; wal/periodic.rs:16, 48-49). The default is per-op (mount.rs:191-196, `_ => WalConfig::PerOp`).
- Replay at mount loads every op record after the last root into `Vec<Vec<u8>>` (segment/mod.rs:114-137), then `oplog::replay` (mount.rs:358-363).
- The chunk index `HashMap<ObjectId, Loc>` (chunk-store lib.rs:401) grows with the object count. `Loc` is `pack: u64, offset: u64, stored: u32, raw: u32, flags: u8` (lib.rs:169-175). The design note says "about 57 bytes per object, so 57 MB for one million objects" [CITED: docs/design/chunk-store.md:160-161]. With hashbrown overhead it is more like 72 B per entry plus slack [ASSUMED]. So a 4 GiB run has a larger RSS than a 1 GiB run for a reason outside this phase (Phase 17, #85).
- Each FUSE write is copied 3-4 times on its way to the log: `data.to_vec()` (handlers.rs:327), `rec.encode()`, `wal_entry_to_segment` clone (wal/per_op.rs:69-80), `frame_record` (segment/mod.rs:190-198). Per write this is bounded by max_write.

## Q2. Manifest format: can a range of chunk refs be replaced?

Verbatim constants (crates/chunk-store/src/file.rs:21-29, read this session):
```
pub const LEAF_ENTRIES: usize = 4096;
pub const NODE_MIN: usize = 64;
pub const NODE_CUT_ONE_IN: u32 = 512;
```
Node format (file.rs:4-7): magic `SFM1`, level `u8`, count `u32 LE`, entries of `ObjectId` (33 B) + `len u64 LE` (41 B per entry; `const ENTRY: usize = ID_LEN + 8;` at file.rs:39).

Findings:
1. **It is a tree with content-defined node cuts** [VERIFIED: file.rs:33-36, 157-169]. A node ends after entry `e` when `n == LEAF_ENTRIES || (n >= NODE_MIN && is_cut(&e.id))`. `is_cut` looks at the last 4 digest bytes of the entry's ID. Index nodes (level k > 0) hold `(child id, bytes covered)` and are cut the same way. Single-child tops collapse (file.rs:186-189).
2. **The tree is canonical**: "The tree depends only on the chunk sequence, so the key of a file does not depend on the sizes of the writes that made it." [VERIFIED: file.rs:13-15]. This is an invariant to keep (see Q3, Open Question 1).
3. **No splice API exists.** `Entry`, `Node`, `decode_node`, `load_node`, `FileWriter::push` and `put_node` are all private (file.rs:41-92, 157-174). `FileWriter` can only append bytes (`write`, `write_zeros`). `FileReader::read` descends from the root for each chunk (file.rs:261-310), with a cache of 16 nodes (`const NODE_CACHE: usize = 16;` file.rs:214). No API lists the entries of a range.
4. **Splice cost.** With canonical node cuts, the same resync argument as for chunks holds one level up: from an old node boundary, the same entry sequence gives the same nodes. Proof that this is local: the existing test `a_shift_at_the_start_changes_few_nodes` asserts at most 6 new nodes for an insert or delete at the start of 200,000 entries (file.rs:372-393); the design note reports 2 [CITED: docs/design/chunk-store.md:67-72]. So a splice is O(depth x node size + changed entries) when the entry IDs give natural cuts.
5. **Exception (forced cuts at 4096):** a run of identical entry IDs (for example the zero chunk) is cut only by count. A change in the entry count before the run shifts the phase of every forced cut in the run, so every leaf of the run is rebuilt. These leaves are all identical (4096 x zero entry), so they dedup by ID and cost only hashing (about 168 KiB per leaf). Memory stays fixed.
6. **What a reader needs for a byte range:** the root key and a descent by `partition_point` on `starts` per level (file.rs:265-287). A splice needs a **leaf cursor**: seek to the entry that holds offset x (the same descent, keeping the path stack), then iterate entries forward across leaves. Memory: depth x one decoded node (at most 4096 x 41 B = 168 KiB per level).
7. **Never build a flat entry list.** At MAX_FILE_SIZE (1 TiB) with about 10.9 KB per chunk (measured average below), that is about 100 M entries x 41 B, which is about 4 GB. The cursor and the writer must stream.

Answer to the CONTEXT open question: yes. With a cursor plus a writer that accepts existing entries (and, in the optimized form, whole existing subtrees when the lower levels of the writer are empty and the cursor is at an old node boundary), a range of chunk references is replaced without a rewrite of the chunk list. The output tree is the same tree that a full rebuild would make.

## Q3. FastCDC: restart, resync, and its bound

Verbatim constants (crates/chunk-store/src/lib.rs:78-80, 84-88, read this session):
```
pub const CHUNK_MIN: u32 = 4 * 1024;
pub const CHUNK_AVG: u32 = 8 * 1024;
pub const CHUNK_MAX: u32 = 32 * 1024;
pub fn chunk_ranges(data: &[u8]) -> Vec<Range<usize>> {
    fastcdc::v2020::FastCDC::new(data, CHUNK_MIN, CHUNK_AVG, CHUNK_MAX)
```
`fastcdc` 3.2.1 (Cargo.lock:272-273; workspace `fastcdc = "3.2"` at Cargo.toml:29). `FastCDC::new` uses `Normalization::Level1` (fastcdc v2020/mod.rs:454-455). The masks are `MASKS[bits ± 1]` with bits = 13: mask_s = MASKS[14] = `0x0000d90313530000`, mask_l = MASKS[12] = `0x0000d90103530000` (v2020/mod.rs:74-100, 490-492). The bits of mask_l are a subset of the bits of mask_s.

How a cut is decided (v2020/mod.rs:298-350, read this session):
- Hashing starts at `index = min_size / 2`, that is byte `min_size` of the chunk, with `hash = 0`. It runs `hash = (hash << 2) + gear_ls[source[a]]` and tests `return (hash, a)`. So **the byte at the cut position is hashed**, and the chunk is `[start, a)`. A cut at p depends on bytes `[p-63, p]` (64-bit shift register) and on `p - start` (min, avg and max thresholds).
- Near EOF: `remaining <= min_size` returns the rest as one chunk. Natural cuts before EOF are the same as in an endless stream. Only the **last** chunk is EOF-forced.
- After a cut, the chunker state is just the new start position. So two chunkings that share a cut at e and the bytes after e give the same cuts from e on.
- `FileWriter::cut` keeps the last range in `buf` until more bytes arrive (file.rs:142-155), so streaming in any piece size gives the same (canonical) cuts as one pass over the whole file.

Rules that follow:
- **Restart** at the largest old boundary **strictly less than** the first changed byte d. An old cut at exactly d was decided by byte d, so it is invalid. The proposed wording "start at the chunk boundary before an extent" must use `<`, not `<=`.
- **Resync** at the first new cut e with e >= the end of the changed region and e equal to a usable old cut. A usable old cut is below `min(base_len, visible)`, or equals `base_len` when the new size is `base_len` and the tail did not change. The old EOF cut is not usable when the file grows.
- **Merge regions:** if another extent starts before resync, keep chunking (one region).

Measured with the repo's exact parameters (scratch program, 64 MiB inputs, xorshift data; output pasted):
```
random: write 4096 B, trials 2000: old chunks 6167 (avg 10881 B); resync dist past extent p50 7753 p99 39106 max 54775; new chunks p50 2 p99 5 max 6; never resynced 0
random: write 1 B, trials 2000: old chunks 6167 (avg 10881 B); resync dist past extent p50 5341 p99 22045 max 32043; new chunks p50 1 p99 1 max 3; never resynced 0
random: write 1048576 B, trials 300: old chunks 6167 (avg 10881 B); resync dist past extent p50 9995 p99 39778 max 52410; new chunks p50 98 p99 105 max 107; never resynced 0
zeros: write 4096 B, trials 300: old chunks 2048 (avg 32768 B); resync dist past extent p50 32509 p99 66359406 max 66817292; new chunks p50 2 p99 2027 max 2041; never resynced 0
zeros: write 100 B, trials 300: old chunks 2048 (avg 32768 B); resync dist past extent p50 16060 p99 37889134 max 63851395; new chunks p50 1 p99 1158 max 1950; never resynced 0
periodic: write 4096 B, trials 300: old chunks 2048 (avg 32768 B); resync dist past extent p50 5267692 p99 66359406 max 66817292; new chunks p50 162 p99 2027 max 2041; never resynced 0
  random/4096: 0 of 2000 trials resynced more than 1 MiB past the extent (EOF counts)
  zeros/4096: 150 of 300 trials resynced more than 1 MiB past the extent (EOF counts)
  zeros/100: 13 of 300 trials resynced more than 1 MiB past the extent (EOF counts)
  periodic/4096: 153 of 300 trials resynced more than 1 MiB past the extent (EOF counts)
```
("never resynced 0" counts EOF as a resync; the 66 MB maxima are runs to EOF.)

Oracle check of the restart rule (spliced cuts compared with `cuts(whole modified data)`, 16 MiB random base, 3000 edits, half of the overwrites start exactly on an old boundary):
```
restart at largest old cut <= change start: mismatches vs full re-chunk: overwrite 374/750, append 750/750, shrink 0/750, extend-zeros 750/750
restart at largest old cut < change start: mismatches vs full re-chunk: overwrite 0/750, append 0/750, shrink 0/750, extend-zeros 0/750
```
So with the strict rule and the usable-cut rule below, the spliced cuts are **identical** to a full re-chunk in all 3000 edits [VERIFIED: run this session]. With `<=`, half the overwrites (those on a boundary) and every append or extend give different cuts.

Conclusions:
- Random data: resync within about 2 chunks (p99 39 KiB, max 55 KiB past the extent). A 4 KiB write rewrites 2-6 chunks.
- Zeros and periodic data: in about half the trials the resync point is **EOF**. FastCDC gives **no bound** [VERIFIED: measured above]. The rebase must stream with fixed memory however far it runs.
- Zeros cut only at CHUNK_MAX (64 MiB of zeros gave 2048 chunks of 32768 B, measured above; the reader's flyweight relies on the same fact, file.rs:216-225). So a **zero-run fast path** is exact and canonical: at a chunk start, if the next CHUNK_MAX bytes of the merged view are known zeros (base entries with the zero ID and length CHUNK_MAX, or the implicit zero area `[visible, size)`), and no extent overlaps them, push the zero entry without reading or hashing. This makes extends, `truncate -s 4G` followed by writes, and zero-filled disk images cheap.
- Non-zero periodic data can still re-chunk to EOF. CPU is O(run) and memory is fixed. A forced cut at an old boundary would bound the CPU, but it breaks the canonical-key invariant (Open Question 1).

## Q4. Write-log records, relog, replay

Records today (oplog.rs:22-49; tags at 152-160, read this session): `Setattr` tag 9 (`ino, mode, uid, gid, size, atime, mtime`), `Write { ino, offset, data }` tag 12, `Truncate { ino }` tag 13, `Release` 14, `Flush` 15. `Enc::bytes` writes a `u32` length (oplog.rs:99).

How writes are logged now: `handle_write` logs `Op::Write { ino, offset, data: data[..n].to_vec() }` after a successful `test_write` (handlers.rs:316-329). The clock is frozen per request (`logged()`, handlers.rs:106-125). PerOp appends without a sync for Op records (wal/per_op.rs:39-47). `flush_wal` syncs at fsync.

What `commit_cut` relogs today (filesystem.rs:1046-1106): under the exclusive gate, `flush_handle(.., false)` for each handle (Streaming prefix into the manifest; Buffered returns early at 1136-1143), then it builds `Vec<Record>`. For Streaming it adds the pending writes (1071-1075). For Buffered it adds `Truncate` + `Write{offset 0, whole buf}` (1076-1079), plus a ctime `Setattr` when ctime > time (1081-1090). Then `meta.commit()`, then `log_record` each, then `flush_wal` (1093-1104).

Minimal relog for an overlay handle (base always in the manifest, see Q5):
1. Before `meta.commit()`: collect only descriptors `(fh, ino, time, base_len, visible, size, extent ranges)`. Do not clone bytes. The exclusive gate keeps the state stable until step 3.
2. `meta.commit()`.
3. For each overlay handle, log in this order, with `time = s.modified`:
   - `Setattr { size: Some(visible) }` if `visible < base_len` (shrink: the zeros come back),
   - `Setattr { size: Some(size) }` if `size != visible` (extend),
   - one `Write` per extent, split into **1 MiB** pieces, each built, logged and dropped before the next (Pitfall 6),
   - the existing ctime `Setattr` (keep 1081-1090).
4. `flush_wal` (keep 1101-1104).

Replay correctness (paths read this session):
- `Setattr` replays through `handle_setattr` with the replay handle of the inode if one exists, else `fh = None` (oplog.rs:276-284). With no write handle, `test_setattr_size` goes to Case B and rebuilds the committed content to the new size (filesystem.rs:1333-1369). Content is correct; with the splice it becomes cheap.
- `Write` opens `O_RDWR` if needed (oplog.rs:295-306). On a file with content the first write is not fresh (filesystem.rs:734), so it becomes an overlay on the manifest root, which is the base. No file read. This depends on `has_content` (filesystem.rs:584-587: `m.size > 0` and a non-empty manifest), so every base install must also set the inode size (Q7 switch recipe, Pitfall 12). The existing test `a_commit_keeps_the_times_of_a_buffered_file` (write_log_replay_tests.rs:289-327: create, stream 50,000 B, backward write, chmod, commit, crash, replay) is exactly this case and must stay green.
- If the file had no committed content (`base = None`), the writes go through Streaming, pending and settle on replay. Same content, because the zeros are implicit in both paths.
- Times: `test_setattr_size` stamps mtime and ctime with `now_pair()`, which is the frozen record time `s.modified` (filesystem.rs:298-302, 1319-1328). The final `Release` of each replay handle (oplog.rs:333-335) stamps `modified` through `stamp_content` (1025-1036). The ctime `Setattr` puts back a later ctime.
- `Truncate` (tag 13) is not needed for overlays: a resize to 0 clears the manifest and the base (`clear_manifest`, 455-467), and the relog then logs `size` and the extents only.

BUFR-01 bound: the relog is at most the cost-accounted dirty limit per open overlay handle, and it holds only bytes changed since the last rebase. With a rebase at close and at the limit, that is "changed bytes" in the common cases. It is not strictly "changed since the last commit" (Open Question 2).

## Q5. Online GC interaction

Read this session:
- `gc_begin` seals the active pack and starts `touched = Some(HashSet::new())` (chunk-store lib.rs:608-616). `put` touches the ID also on a dedup hit (`inner.touch(id)` at lib.rs:544 and 562). `read` touches (lib.rs:810-812). `gc_finish` keeps `live.contains(id) || touched.contains(id)` (lib.rs:628-640) for packs up to `boundary`.
- `gc_start` takes `commit_gate.write()`, calls `gc_begin`, adds a root for each **Streaming** state only (`WriteMode::Buffered { .. } => None`), then `commit_cut` + `flush_wal`, then drops the gate (filesystem.rs:491-537). Mark and sweep (`gc_content`) run **without** the gate (553-571).
- The mark walks `file_objects` of every root and file root (metadata gc/mod.rs:42 `live_objects`; chunk-store file.rs:313-327).

Analysis for an overlay rebase that runs inside a FUSE callback (holding `gate.read()`), with one FUSE thread:
- A rebase cannot straddle `gc_begin`, because `gc_begin` runs under `gate.write()`.
- A rebase after `gc_begin` (during mark or sweep): every new object it puts is touched, so it is kept. The old subtrees it reuses by ID (not read, not put) belong to the base B. B is live for this GC if B was in the manifest at the `gc_start` commit, or in `file_roots`.
- **The real hazard:** a base root that is only in the handle state (for example the stream finish at the switch, or a rebase result not yet in the manifest). Its objects were put before `gc_begin`, so they are not touched. `gc_start` skips it (502-507), so the sweep deletes it.
- **Fix (both, cheap):** (a) install each new base in the manifest at once: at the switch, and right after each rebase (`set_manifest`, refcount +1/-1, `last_committed_root = Some(new)`, `stamp_content`). The relog needs this anyway, because replay starts from the manifest. (b) In `gc_start`, add `Overlay { base: Some(k), .. } => Some(k)` to `file_roots`.
- No rebase/GC mutex is needed while fuser runs one thread. If FUSE becomes multi-threaded (#22), this argument still holds as long as every rebase runs inside a callback that holds `gate.read()`. A rebase outside a callback (background thread) would need the mutex in the old CONTEXT item.

## Q6. Concurrency

Read this session:
- `commit_gate: Arc<RwLock<()>>` (filesystem.rs:134-135). Every FUSE callback takes `gate.read()` (fuse_callbacks.rs, for example read 150-151, write 454-455, setattr 747-748, fsync 309-310). The commit timer takes `gate.write()` and calls `commit_cut` (commit_timer.rs:47-56).
- fuser: `let n_threads = config.n_threads.unwrap_or(1);` (vendor/fuser/src/session.rs:257). mount.rs builds `Config::default()` and does not set `n_threads` (mount.rs:177-183). So there is **one** FUSE thread on Linux and on macOS.
- `test_write` takes the state out of `open_files`, works without the lock, and puts it back (filesystem.rs:722-727, 813-815). `settle_pending`, `finalize_write` and `test_setattr_size` use the same pattern or short locks.
- #21 rules (gh issue 21, open): hold a lock only for an in-memory change; never hold one during disk I/O, a hash or a wait. Per-inode extent overlays and a frozen layer for the committer are the target model.

Where a rebase can run:
- **In the callback that needs it** (write over the limit, release, setattr), with the state taken out of `open_files` (the existing pattern), under the callback's `gate.read()`. No global mutex is held during hashing. This matches the current model and #21's rule for the `open_files` lock.
- **Not in `commit_cut`**: that would be hashing under the exclusive gate (BUFR-03 spirit, #93).
- **Not in a `prepare_cut` on the commit-timer thread**: it would run at the same time as the FUSE thread. Taking the state out would make a concurrent `test_write` return `EBADF` (filesystem.rs:723-726). Not taking it out needs a frozen layer and a generation check (#21 work). A background rebase also cannot bound memory, because only the writer can stop the dirty bytes from growing.
- Pre-existing race to note, not fix: while a state is out of the map, a read from another thread would miss it. That cannot happen with one FUSE thread.

## Architecture Patterns (Q7: design recommendation)

### System Architecture Diagram

```
 FUSE write(fh, off, data)  [gate.read held by the callback]
        |
        v
 owner state (owner_fh) taken out of open_files
        |
        +-- Streaming, in order / in reorder window --> Stream::push (unchanged)
        |
        +-- not fresh / out of window ------------------------------+
        |                                                          v
        |                                   switch: base = stream finish or manifest root
        |                                   install base: set_manifest + refcount + stamp_content(size>0)
        |                                   pending writes --> extents (moved)
        v                                                          |
 Overlay { base, base_len, visible, size, extents, cost } <--------+
        |  write: merge into extents; truncate: clip extents, visible=min(visible,n)
        |
        +-- cost > DIRTY_LIMIT, or release, or setattr Case B --+
        |                                                         v
        |                     rebase = splice(base tree, regions, size)  [streams; ~2-3 MiB]
        |                       leaf cursor --> copy old entries --> restart at old cut < region
        |                       --> FastCDC over (base bytes + extents + zeros) --> resync at old cut
        |                       --> FileWriter (new chunks put; old entries/subtrees reused by ID)
        |                                                         |
        |                     install new root in manifest (refcount +1/-1, stamp) <--+
        v
 state put back into open_files --> reply

 FUSE read(ino, off, len): lock, copy extent parts in range + base/visible/size, unlock
        --> base range via readers cache --> zeros in [visible, size) --> overlay extents --> reply

 commit timer: gate.write --> commit_cut: Streaming prefix into manifest (unchanged);
        overlays: collect descriptors only --> meta.commit() --> relog per overlay:
        Setattr(visible) if shrunk, Setattr(size) if changed, Write pieces of 1 MiB, ctime Setattr
        --> flush_wal

 online GC: gc_start (gate.write): gc_begin --> file_roots += Streaming finishes + overlay bases
        --> commit_cut --> drop gate --> mark + sweep (no gate; a rebase may run now:
        its puts are touched, its reused IDs belong to a root)
```

### Data structures

```rust
// crates/slicefs-cli/src/overlay.rs (new, pure, no I/O)
pub struct Overlay {
    pub base: Option<ObjectKey>, // == manifest root of the inode (always installed)
    pub base_len: u64,           // length of base content (Reader::len of the root)
    pub visible: u64,            // base bytes [0, visible) still show; <= base_len
    pub size: u64,               // logical file size
    extents: BTreeMap<u64, Vec<u8>>, // non-overlapping, merged when they touch; all < size
    cost: usize,                 // sum(len) + EXTENT_OVERHEAD * extents.len()
}
// write(off, data): merge overlapping or adjacent extents into one Vec; size = max(size, end)
// truncate(n):    drop extents >= n, cut the one across n; visible = min(visible, n); size = n
// read_into(off, len) -> Vec<(u64, Vec<u8>)>: copies of the extent parts in range (lock held only for this)
// changed_regions(): the extents, plus a tail region from min(visible, base_len) to size
//   whenever size != base_len or visible < base_len (a pure shrink gives an EMPTY [visible, size)
//   range, but the tail chunk must still be re-chunked: the oracle used change start = end = new size)
enum WriteMode { Streaming { .. }, Overlay(Overlay) }
```
- `EXTENT_OVERHEAD`: about 64 B, charged per extent, so many 1-byte writes cannot exceed the budget through map overhead. [ASSUMED value; measure with the counting allocator.]
- Dirty limit: an internal constant `DIRTY_LIMIT`, plus a test hook to set it small. Start at **32 MiB** and set the final value from the counting-allocator test and the Docker measurement (see Validation). The issue's "for example 64 MiB" is a suggestion. The cap must also leave room for the relog piece, the rebase working set and the chunk index. [ASSUMED value.]
- Per-handle limit only; no global limit (Open Question 3).
- **Real per-handle peak:** at the switch, the pending writes (up to `REORDER_WINDOW` = 64 MiB, `pub const REORDER_WINDOW: usize = 64 << 20;` at filesystem.rs:41) move into the extents. So the bound is `max(REORDER_WINDOW, DIRTY_LIMIT)` + one write + the rebase working set. A `DIRTY_LIMIT` below 64 MiB does not lower the peak unless `REORDER_WINDOW` is lowered too (or the switch rebases at once when the moved pending bytes exceed `DIRTY_LIMIT`, which then costs one rebase of up to 64 MiB).

### Switch from Streaming (replaces filesystem.rs:781-801)
- Base: if `next_expected_offset > 0`, then `state.clone().finish()` (no zero padding: `visible = base_len = streamed`, `size = max(byte_count, min_size)`; the zeros are implicit). Else the manifest root `m[0]` if `has_content`, with `base_len = Reader::len`.
- Install the base in the manifest at once if it differs (refcount +1 new, -1 old). Keep `last_committed_root = Some(base)`; do not decrement-and-drop as 794-800 does.
- **Also `stamp_content(ino, base_len, modified)` at the install** when the inode size is 0 or differs. `has_content` needs `inode.size > 0` *and* a non-empty manifest (filesystem.rs:584-587). A handle that switches before its first flush still has size 0 in the inode. If the root keeps size 0, the first replayed `Write` is "fresh" (734), streams from scratch, and the base bytes are lost (Pitfall 12).
- Move the pending writes into the extents (no copy), then apply the triggering write. If `cost > DIRTY_LIMIT`, rebase before the reply.
- Keep the log line text `switches to Buffered`, or change the Phase 13 Docker check together with it: 13-02-PLAN.md greps `"switches to Buffered"` in mount.log.

### Rebase (one function; tracer first, splice second)
```
rebase(cs, ov: &Overlay) -> (root: Option<ObjectKey>, len: u64)
  tracer : Stream::push of the merged view in 1 MiB pieces from 0 to size (O(file) CPU, fixed memory,
           canonical, so unchanged chunks dedup by ID and BUFR-05 already holds)
  splice : chunk_store::file::splice(store, base, regions, size, read_merged) (Q2/Q3)
install : set_manifest(ino,[root]); +1 root, -1 old base if different; last_committed_root;
          stamp_content(ino, size, modified); then ov = Overlay::clean(root, size)
```
Triggers: write or setattr when `cost > DIRTY_LIMIT`; release (`finalize_write`) when dirty or `size != base_len`; destroy (through `finalize_write`). **Not at fsync** (recommended change from the direction): fsync is durable through the write log already. A rebase at every fsync costs tens of KiB of path nodes per 4 KiB write, and it conflicts with #3 and `fsync_tests.rs:154` (`fsync_time_on_buffered_handle_does_not_grow_with_file_size`). This is Open Question 2.

Splice algorithm (`chunk_store::file::splice`, streaming):
1. Leaf cursor on the old tree: seek, then iterate `(offset, id, len)`; path stack only.
2. Writer = `FileWriter` with a new `pub(crate)` method that pushes an existing level-0 entry (requires `buf` empty). Optimized form: push a whole old node at level k+1 when levels 0..k of the writer are empty and the cursor is at an old level-k boundary that the change does not touch.
3. For each region: copy old entries that end at or before the restart point (the largest old cut < region start). Re-chunk from there with a local loop: a buffer of at most 2 x CHUNK_MAX + one piece, `chunk_ranges`, emit all ranges but the last (as `FileWriter::cut` does). Read base bytes with a `FileReader` in pieces, overlay the extents, use zeros at or past `visible`, and take the zero-run fast path. After each emitted chunk end e: if e >= region end and e is a usable old cut (Q3 rules), drop the buffer and go back to copying old entries from e.
4. At `size`: cut all, then `finish()`. If `size != base_len` (shrink, extend, append) the tail region always exists, also when no extent is near the end.
Memory: about 2 x 32 KiB + one piece (1 MiB) + depth x 168 KiB (cursor) + depth x 168 KiB (writer levels). That is about 2-3 MiB, with no dependence on file size.

### Read merge (replaces filesystem.rs:961-966)
Under `open_files`: find the overlay of the inode, copy the extent parts for `[offset, offset+size)`, copy `base`, `visible`, `size`; unlock. Then read the base part `[offset, min(end, visible))` through the per-inode `readers` cache (989-999), zero-fill up to `min(end, size)`, and lay the extent copies over it. Memory O(size of the request).

### setattr size
- Overlay: `Overlay::truncate(n)` (n = 0 also clears the manifest and the base as today, 1242-1253).
- Streaming, not a pure extension (1276-1315): switch to an overlay as at a write, then truncate. No materialization.
- Case B, no handle (1333-1369): either keep it (bounded memory) or call `splice` with no extents and `visible = size = n` (O(log N)). Recommended in the optimization wave.

### Relog: see Q4. GC: see Q5.

### Alternatives considered

| Instead of | Could use | Why not chosen |
|------------|-----------|----------------|
| Inline rebase in callbacks | Background rebase + frozen layer + rebase/GC mutex (old CONTEXT `prepare_cut`) | Cannot bound memory (only the writer can stop the growth); concurrency with the take-out pattern gives EBADF (filesystem.rs:723-726); needs #21 per-state locks first |
| Relog of extents at the cut | Rebase inside `commit_cut` (log nothing for overlays) | Hashes under the exclusive gate (#93 complaint); but it meets BUFR-01 in the strict reading. Kept as fallback in Open Question 2 |
| Canonical splice | Forced cut at an old boundary after a resync limit | Bounds CPU on periodic data, but breaks file.rs:13-15 (key independent of write history) and cross-node dedup (global goal) |
| Splice with subtree reuse | Re-stream every old entry through `FileWriter::push` | Simpler, canonical, fixed memory, but O(file/chunk) node work per rebase (about 16 MiB of nodes for 4 GiB). Fine as an intermediate step |
| Extent map | Fixed pages (for example 64 KiB) with a valid-bitmap | Simpler accounting, but needs a base read per first touch or a bitmap, and relogs whole pages |
| `BTreeMap<u64, Vec<u8>>` | Persistent map (`im`/`rpds`, #21 option B) | New crate; only useful with lock-free readers (#21) |

## Don't Hand-Roll

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|-------------|-----|
| Chunk boundaries | Own rolling hash or own cut loop | `chunk_store::chunk_ranges` (fastcdc 3.2.1 v2020) on a bounded buffer, "all ranges but the last" as in `FileWriter::cut` | Keeps the exact canonical cuts; the EOF rule and normalization are subtle |
| Node cuts and tree shape | A second tree builder | `FileWriter::push` / `put_node` / `finish` (made `pub(crate)` or wrapped in a splice type inside file.rs) | The canonical-tree invariant lives in one place |
| Zero chunk ID | Hashing zeros again | `zero_chunk()` (file.rs:219-225) | Already computed per hasher |
| Range reads of the base | Own node descent | `FileReader` / `content::Reader` and the `readers` cache | Has the corruption checks (file.rs:277-285) |
| Durable relog | New record types | `Op::Write`, `Op::Setattr` | Replay already handles them |
| Random-op correctness | Ad hoc cases only | A seeded differential test against a `Vec<u8>` model (no new crate), in the style of the existing `data(seed, n)` helpers | fsx-style coverage of overlay read/write/truncate/rebase |

**Key insight:** every hard property (canonical cuts, canonical tree, zero flyweight, corruption checks) already exists in `chunk-store/src/file.rs`. The splice must reuse those internals, not copy them into slicefs-cli.

## Runtime State Inventory

Not a rename phase. The format of stored objects does not change. Checked:
- Stored data: none. Manifests, nodes and chunks keep the SFM1 format. Old stores need no migration (and none is required by project rules).
- Live service config: none.
- OS-registered state: none.
- Secrets/env vars: none.
- Build artifacts: none.
- Log text: `switches to Buffered` is grepped by the Phase 13 Docker check (13-02-PLAN.md). Keep it or update that check in the same change.

## Common Pitfalls

### Pitfall 1: restart at "the boundary before the extent" with `<=`
**What goes wrong:** a write that starts exactly on an old boundary keeps the old chunk that ends there, but that cut was decided by the first byte of the write. The tree is then not canonical, and the key of the file depends on write history.
**How to avoid:** restart at the largest old cut **strictly less** than the first changed byte. In the oracle run, `<=` gave 374 of 750 overwrites and 750 of 750 appends wrong; `<` gave 0.
**Warning signs:** the oracle test (splice root equals the `FileWriter` root over the same bytes) fails on writes that start on boundaries.

### Pitfall 2: treating the old EOF chunk as reusable
**What goes wrong:** the last old chunk ended at EOF, not at a content cut. If the file grows (append, extend, write past the end), it must be re-chunked.
**How to avoid:** a usable old cut is below `min(base_len, visible)`, or equals `base_len` only if the new size equals `base_len`.

### Pitfall 3: no bound on resync
**What goes wrong:** on zero or periodic data, the re-chunk runs to EOF (measured: about 50% of trials). Any code that collects the re-chunked range in a Vec is O(file).
**How to avoid:** stream every stage; use the zero-run fast path; never build a flat entry list.

### Pitfall 4: base root only in the handle
**What goes wrong:** online GC sweeps the objects of a stream finish or a rebase result that no root names (gc_start 502-507).
**How to avoid:** install every base in the manifest at once; add overlay bases to `file_roots`.

### Pitfall 5: dropping the refcount of a live base
**What goes wrong:** the current switch decrements `last_committed_root` while the manifest still points at it (794-800).
**How to avoid:** the overlay keeps `last_committed_root == base`; decrement only when a new root replaces it.

### Pitfall 6: copies during relog
**What goes wrong:** building `Vec<Record>` with `data.clone()` before `meta.commit()` doubles the dirty bytes. Each record is then copied 3 more times (encode, `wal_entry_to_segment`, `frame_record`). With FlushOnFsync, all of them wait in the WAL buffer until `flush_wal`. A 16 MiB piece costs about 64 MiB of transient RAM.
**How to avoid:** collect descriptors; after `meta.commit()`, build, log and drop one 1 MiB piece at a time.

### Pitfall 7: bytes-only dirty accounting
**What goes wrong:** 1-byte random writes create one extent each. 32 Mi extents x about 80 B of map and Vec overhead is gigabytes.
**How to avoid:** charge `EXTENT_OVERHEAD` per extent in `cost`; merge adjacent extents.

### Pitfall 8: `put_all` / `FileWriter::write` with a big slice
**What goes wrong:** `FileWriter::write` appends the whole slice to `buf` before it cuts (file.rs:122-128), and `chunk_ranges` then returns one `Range` per chunk of it.
**How to avoid:** all new callers push pieces of at most 1 MiB. Optionally make `FileWriter::write` loop over pieces itself.

### Pitfall 9: VmHWM differences that are not the overlay
**What goes wrong:** the 4 GiB run has about 4x the chunk-index entries of the 1 GiB run (lib.rs:401). Random rewrites add new chunks until GC, and a hashbrown resize briefly holds the old and new tables. The FlushOnFsync WAL buffer and replay at mount also hold written bytes.
**How to avoid:** pin `--wal-strategy per-op`, use a fresh daemon per size, write the same random-write **volume** to both sizes (not "rewrite the whole file"), and set the cap from measured runs (the Phase 13 rule). Report the index size (`slicefs stats` object count) next to VmHWM.

### Pitfall 10: zero-filled base in the Docker test
**What goes wrong:** with a sparse base and 4 KiB random writes (the fio default), a rebase re-chunks to EOF in about half the cases (Q3), unless the zero-run fast path exists.
**How to avoid:** the main Docker run uses a random base (urandom) with 1 MiB in-place blocks plus 4 KiB blocks. A sparse-base run comes only after the zero-run fast path is in.

### Pitfall 11: tests that assert Buffered mode
**What goes wrong:** tests use `test_is_streaming(fh) == false` as "Buffered" (fsync_tests.rs:149, write_log_replay_tests.rs:303/340, coverage_boost_tests.rs ~1661). They still hold for an overlay (it is not streaming). The two relog tests (write_log_replay_tests.rs:289-327 and 330-348) check content and times after replay, not the shape of the relog, so they stay valid unchanged and are the regression tests for Pitfall 12. Only comments and names that say "Buffered" become stale.

### Pitfall 12: base installed without the inode size
**What goes wrong:** replay of the first `Write` takes the Streaming "fresh" path when the root has `inode.size == 0` (filesystem.rs:584-587, 734), so the base content is dropped.
**How to avoid:** every base install (switch, rebase) calls `stamp_content` with the base length or the current size.
**Warning signs:** `a_commit_keeps_the_times_of_a_buffered_file` fails with the backward-written bytes only.

## Code Examples

Restart and resync rule (the oracle model, from the scratch program used above):
```rust
// keep old cuts < b0, re-chunk d[b0..] until a new cut e >= change_end is a usable old cut
let i = old.partition_point(|&c| c < change_start);          // strictly less (Pitfall 1)
let b0 = if i == 0 { 0 } else { old[i - 1] };
let usable = |c: usize| c < old_len.min(new_len) || (c == old_len && old_len == new_len); // Pitfall 2
for c in fastcdc::v2020::FastCDC::new(&d[b0..], 4096, 8192, 32768) {
    let e = b0 + c.offset + c.length;
    out.push(e);
    if e >= change_end && usable(e) { /* copy old entries after e */ break; }
}
```
The production version streams (bounded buffer, `chunk_ranges`, all ranges but the last), not `&d[b0..]`.

Relog piece loop (after `meta.commit()`, under the exclusive gate):
```rust
const RELOG_PIECE: usize = 1 << 20; // well under MAX_PAYLOAD (segment/mod.rs:186)
for (off, bytes) in ov.extents() {
    for (i, piece) in bytes.chunks(RELOG_PIECE).enumerate() {
        self.log_record(Record { time, op: Op::Write { ino, offset: off + (i * RELOG_PIECE) as u64, data: piece.to_vec() } });
    }
}
```

## State of the Art

| Old Approach | Current Approach | When Changed | Impact |
|--------------|------------------|--------------|--------|
| Full-file `Vec` on random writes | Overlay on a content-defined chunk tree, splice re-chunk | This phase | O(dirty) memory |
| Relog the whole buffer per commit (#93) | Relog extents since the last rebase | This phase | Log bytes do not depend on file size |
| Hash the whole buffer at close | Re-chunk only near extents, reuse unchanged chunk and node IDs | This phase | CPU O(change + resync + depth) except on periodic data |

## Package Legitimacy Audit

No new packages. The phase uses `fastcdc` 3.2.1, `blake3` and `zstd`, which are already in the workspace (Cargo.toml:20-45, Cargo.lock:272-273). The counting allocator for the memory test wraps `std::alloc::System` (std only). The Docker workload uses `python3` 3.11.2 from the existing image (checked: `docker run slicefs-fusetest python3 --version`). `fio` is not in the image and is not needed.

| Package | Registry | Age | Downloads | Source Repo | Verdict | Disposition |
|---------|----------|-----|-----------|-------------|---------|-------------|
| (none new) | — | — | — | — | — | — |

**Packages removed due to [SLOP] verdict:** none
**Packages flagged as suspicious [SUS]:** none

## Assumptions Log

| # | Claim | Section | Risk if Wrong |
|---|-------|---------|---------------|
| A1 | Start `DIRTY_LIMIT` at 32 MiB; final value from measurement. The real per-handle peak is `max(REORDER_WINDOW, DIRTY_LIMIT)` + one write (Q7) | Q7 | Cap too tight or rebases too often; a constant change only |
| A2 | `EXTENT_OVERHEAD` about 64 B per extent is a fair charge | Q7 | Many tiny writes exceed the real budget; measure with the counting allocator |
| A3 | The chunk index costs about 72 B per object plus hashbrown slack (design note says 57 B) | Q1, Pitfall 9 | The VmHWM cap for 4 GiB must be set from measurement anyway |
| A4 | Rebase CPU for 32 MiB of 4 KiB random writes is under about 1-2 s (about 8 K extents x 2-6 chunks each) | Q7 | Long write latency at the limit; then lower the limit or add the background rebase later (#21) |
| A5 | Users accept O(run) CPU (not memory) for rebases on non-zero periodic data, to keep canonical keys | Q3, Open Question 1 | If not, a forced cut that breaks file.rs:13-15 is needed |
| A6 | Rebase at close and at the limit (not at fsync) is acceptable | Q7, Open Question 2 | If the user wants fsync rebases, the fsync time test (#3) must be redefined as O(dirty) |

## Open Questions

1. **Canonical keys vs bounded rebase CPU.**
   - What we know: file.rs:13-15 makes the key independent of write history. A forced cut after a resync limit would break that. Zeros get an exact fast path.
   - What's unclear: whether O(run) CPU on non-zero periodic data is acceptable.
   - Recommendation: keep canonical; fixed memory in all cases; zero-run fast path; revisit only if a real workload shows long rebases.
2. **Rebase at fsync, and the strict reading of BUFR-01.**
   - What we know: the direction says "at fsync and at close". Fsync is durable through the log already. With rebase at close and at the limit, a commit relogs up to `DIRTY_LIMIT` of bytes changed since the last rebase, which can include bytes from before the last commit.
   - Recommendation: no rebase at fsync; the BUFR-01 test asserts relog <= changed bytes since the last rebase, and much less than the file size. If the strict reading is required, add "rebase in `commit_cut` when the relog would exceed a small limit" (hash under the gate, bounded).
3. **Global dirty limit over all handles.**
   - What we know: N open overlay handles can hold N x `DIRTY_LIMIT`; Streaming pending windows have the same N x 64 MiB property today (REORDER_WINDOW, filesystem.rs:41).
   - Recommendation: per-handle only in this phase (it meets "per open file"); a follow-up issue for one global budget that covers both overlays and pending windows.
4. **Buffered WAL strategies (FlushOnFsync, Periodic) hold written bytes until a sync.**
   - Recommendation: out of scope (O(write volume), not O(file)); pin per-op in the Docker test; follow-up issue.
5. **Replay at mount loads every pending record** (segment/mod.rs:114-137).
   - Recommendation: out of scope; follow-up issue (stream records into replay).
6. **Phase 13 is not executed yet** (only 13-0x-PLAN.md files exist). It edits write_log_replay_tests.rs, verify-phases.sh and write-log.md. Run Phase 13 first, as the roadmap says.

## Environment Availability

| Dependency | Required By | Available | Version | Fallback |
|------------|------------|-----------|---------|----------|
| cargo / rustc | build, tests | yes (`~/.cargo/bin`, not on the default PATH of the agent shell) | cargo 1.95.0-nightly | — |
| fastcdc crate | splice | yes (registry cache, offline build worked) | 3.2.1 | — |
| Docker (OrbStack) | BUFR-04 VmHWM run | yes | 29.8.2, context orbstack | — |
| Image `slicefs-fusetest` | Docker run | yes | 32 CPUs, 16 GiB RAM, 661 GiB free disk | — |
| python3 in image | random-write workload | yes | 3.11.2 | — |
| fio in image | — | no | — | python3 workload |

A 4 GiB run needs the file, a reference copy and the store: about 12+ GiB of container disk (available).

## Validation Architecture

### Test Framework
| Property | Value |
|----------|-------|
| Framework | Rust built-in test harness (`cargo test`), tempfile 3 |
| Config file | none |
| Quick run command | `cargo test -p chunk-store file::` and `cargo test -p slicefs-cli --test write_log_replay_tests` |
| Full suite command | `cargo test --workspace` (once per merge); Docker: `scripts/docker-linux-test/run.sh verify`; 4 GiB run by hand |

### Phase Requirements → Test Map
| Req ID | Behavior | Test Type | Automated Command | File Exists? |
|--------|----------|-----------|-------------------|-------------|
| BUFR-02 / splice | `splice` root == `FileWriter` root over the same bytes (random edits: overwrites on and off boundaries, append, pure shrink, shrink then extend, extend, multi-extent, base = None); content equal; zero-run fast path gives the same root | unit (oracle) | `cargo test -p chunk-store file::splice` | no, Wave 0 |
| BUFR-04 | Overlay model: seeded random write/truncate/extend/read vs a `Vec<u8>` model; cost accounting | unit | `cargo test -p slicefs-cli overlay::` | no, Wave 0 |
| BUFR-04 | Peak heap (counting `#[global_allocator]`, own test binary) for an 8 MiB and a 32 MiB file, same random-write volume, small test dirty limit: peaks differ by less than a small margin and stay under limit x k | integration | `cargo test -p slicefs-cli --test memory_bound_tests` | no, Wave 0 |
| BUFR-01 | Large file, close, open, small write, commit: a test hook `test_last_relog_bytes()` reports about the change size; after a rebase the next cut relogs only newer extents | integration | `cargo test -p slicefs-cli --test write_log_replay_tests relog` | partly (file exists) |
| BUFR-02 | Crash and replay (PerOp and FlushOnFsync): a switch before the first flush, then commit and crash (existing test at 289, Pitfall 12); shrink then extend (zeros back), sparse write far past the end (#23), a rebase between commits, times kept; an overlay with more than 64 MiB of relog over its life replays (MAX_PAYLOAD regression) | integration | `cargo test -p slicefs-cli --test write_log_replay_tests` | partly |
| BUFR-03 | GC during rebase, deterministic: rebase, `gc_start()`, more writes and a rebase, `gc_content()`, release, remount, compare bytes; plus a pause hook in the rebase with a GC mark+sweep in the pause | integration | `cargo test -p slicefs-cli --test gc_online_tests rebase` | partly |
| BUFR-03 | `commit_cut` does not hash overlay content (counter hook, or timing in the style of fsync_tests.rs:154) | integration | `cargo test -p slicefs-cli --test fsync_tests` | partly |
| BUFR-05 | 64 MiB random file, close; 4 KiB change; close: new objects <= about 6 chunks + 2 x depth nodes, stored-bytes growth < 1 MiB; chunk ID sets differ only near the change | integration | `cargo test -p slicefs-cli --test write_path_tests store_growth` | no, Wave 0 |
| BUFR-04 | Docker: 1 GiB and 4 GiB urandom base (cp), then the same 512 MiB of random in-place writes (1 MiB and 4 KiB blocks, seeded, one python3 process), cmp vs a reference, remount, cmp; VmHWM <= the same cap; per-op WAL; fresh daemon per size; release build | e2e (manual run) | `scripts/docker-linux-test/run.sh bigwrite` (new mode) | no, Wave 0 |

### Sampling Rate
- **Per task commit:** the targeted `cargo test -p ... --test ...` command of the touched area
- **Per wave merge:** `cargo test --workspace`
- **Phase gate:** full suite green, plus one `run.sh verify` and one `run.sh bigwrite` by hand

### Wave 0 Gaps
- [ ] `crates/chunk-store/src/file.rs` tests: splice oracle (root equality)
- [ ] `crates/slicefs-cli/src/overlay.rs` with a model test
- [ ] `crates/slicefs-cli/tests/memory_bound_tests.rs`: counting allocator (own binary)
- [ ] test hooks: `test_set_dirty_limit`, `test_last_relog_bytes`, a rebase pause hook
- [ ] `scripts/docker-linux-test/bigwrite.sh` + `run.sh bigwrite` (release build)

## Security Domain

| ASVS Category | Applies | Standard Control |
|---------------|---------|-----------------|
| V2 Authentication | no | — |
| V3 Session Management | no | — |
| V4 Access Control | no (unchanged; FUSE DefaultPermissions) | — |
| V5 Input Validation | yes | Bounds from untrusted trees: keep the `FileReader` checks (child len and level, chunk len, file.rs:277-285, 298-300) in the cursor; keep MAX_FILE_SIZE / EFBIG checks (filesystem.rs:719-721, 1200-1202) |
| V6 Cryptography | no new crypto | BLAKE3-256 IDs through `ObjectHasher`; never hand-roll |

| Pattern | STRIDE | Standard Mitigation |
|---------|--------|---------------------|
| Memory exhaustion through many tiny writes or a huge sparse offset | Denial of service | Cost-accounted dirty limit; implicit zeros; no allocation by offset |
| Crafted manifest tree from a peer (bad lengths, levels) | Tampering / DoS | The cursor validates as `FileReader::read` does; errors, never panics (test `a_malformed_tree_is_an_error_not_a_panic`, file.rs:347-364) |
| GC sweep of in-flight objects | Tampering (data loss) | Base in the manifest + `file_roots`; touched set after `gc_begin` |

## Suggested Plan Split

Files are disjoint inside each wave where possible. filesystem.rs work is serial.

**Wave 1 (parallel, disjoint files)**
- 14-01 Tracer: overlay end to end with the naive rebase. Files: `crates/slicefs-cli/src/overlay.rs` (new), `crates/slicefs-cli/src/filesystem.rs`, `crates/slicefs-cli/src/lib.rs`. Content: `WriteMode::Overlay`, the switch without `handle_content` (base install with `stamp_content`, Pitfall 12), merged read, setattr, the cost-accounted `DIRTY_LIMIT` with `EXTENT_OVERHEAD` and the `test_set_dirty_limit` hook, a **limit-triggered** rebase in the write path, and a rebase at release. The rebase is the naive full re-stream in 1 MiB pieces. Also: base installed in the manifest, overlay bases in `file_roots`, relog of extents in 1 MiB pieces after `meta.commit()`. Delete `handle_content` and `pwrite_into`. The limit must be in this plan: without it the extents can grow to the file size and the interim state breaks the hard rule. Existing tests stay green (write_log_replay_tests.rs:289-348 unchanged, see Pitfall 11). With the limit, this plan alone meets the memory bound (BUFR-04) and BUFR-05 (canonical cuts plus dedup). CPU is O(file) per rebase, so the 4 GiB Docker run waits for 14-04.
- 14-02 Splice in chunk-store: `crates/chunk-store/src/file.rs` (leaf cursor, `pub(crate)` entry push, `splice`, zero-run fast path, oracle tests). No slicefs-cli changes.
- 14-03 Docker bigwrite script: `scripts/docker-linux-test/bigwrite.sh`, `scripts/docker-linux-test/run.sh` (new mode, release build), measure mode only (prints VmHWM). Depends on Phase 13's `verify-phases.sh` helpers only by copy, not by edit.

**Wave 2 (serial on filesystem.rs)**
- 14-04 Swap the rebase body for `splice` (no change to triggers or limits), and use `splice` for setattr Case B. Files: `filesystem.rs`, `content.rs`.
- 14-05 Tests: `tests/write_log_replay_tests.rs` (relog size, replay matrix for PerOp and FlushOnFsync), `tests/gc_online_tests.rs` (GC during rebase), `tests/memory_bound_tests.rs` (new), `tests/fsync_tests.rs` (no hash in `commit_cut`), store-growth test. Test files are disjoint from 14-04 if the hooks land in 14-04.

**Wave 3**
- 14-06 Docker measurement at 1 GiB and 4 GiB, set the cap by the Phase 13 rule, enable the check; update `docs/design/write-log.md` (part 2 overlay) and `docs/design/chunk-store.md` (splice); a follow-up issue for Open Questions 3-5. Checkpoint: the user confirms the cap and Open Questions 1-2.

Tracer slice first: 14-01. It removes every full-file buffer with the least new code, before the splice exists.

## Sources

### Primary (HIGH confidence, read or run this session)
- crates/slicefs-cli/src/filesystem.rs (lines cited above), content.rs, oplog.rs, handlers.rs, fuse_callbacks.rs, commit_timer.rs, mount.rs
- crates/chunk-store/src/file.rs, lib.rs (CHUNK_*, put/read/touch, gc_begin/gc_finish)
- crates/metadata/src/segment/{mod.rs,reader.rs}, wal/{per_op,flush_on_fsync,periodic}.rs, gc/mod.rs
- vendor/fuser/src/session.rs:257 (n_threads default)
- fastcdc 3.2.1 source in the cargo registry: v2020/mod.rs (MASKS, cut_gear, FastCDC::new)
- Scratch measurement program (fastcdc 3.2.1, repo parameters), output pasted in Q3
- gh issues #98, #93, #21; .planning/REQUIREMENTS.md, ROADMAP.md, 14-CONTEXT.md, 13-02-PLAN.md

### Secondary (MEDIUM)
- docs/design/chunk-store.md (index size estimate, node-cut rationale), docs/design/write-log.md

### Tertiary (LOW)
- Cost estimates for rebase CPU and hashbrown overhead (Assumptions A3-A4)

## Metadata

**Confidence breakdown:**
- Q1 inventory: HIGH (every site read)
- FastCDC restart/resync rules: HIGH (source read, oracle and measurement run)
- Splice design: MEDIUM-HIGH (follows from the verified invariants; not built yet)
- Memory and cap numbers: MEDIUM (estimates; to be measured)

**Research date:** 2026-11-18
**Valid until:** 2026-12-18 (stable code base; re-check if Phase 13 changes commit_cut or the tests first)
