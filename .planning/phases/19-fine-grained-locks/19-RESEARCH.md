# Phase 19: Fine-grained locks - Research

**Researched:** 2026-11-21
**Domain:** Rust FUSE daemon concurrency: lock structure of `slicefs-cli`, `metadata` and `chunk-store`; write-log order; POSIX multi-handle semantics
**Confidence:** HIGH for what the code does today (every site below was read this session); MEDIUM for the new designs; LOW for FUSE-T/FSKit `O_APPEND` behavior (no evidence exists)

<user_constraints>
## User Constraints (from CONTEXT.md)

### Locked Decisions

**Lock crate**
- Replace `std::sync::{Mutex, RwLock, Condvar}` with `parking_lot` 0.12 (feature `arc_lock`) in the hot crates (`slicefs-cli`, `metadata`, `chunk-store`), through one `crate::sync`-style shim that the `lock-stats` wrapper and the rank checker wrap. Use 0.12, not 0.13, so the tree keeps one copy with the vendored fuser. Add a `deadlock-detect` feature (`parking_lot/deadlock_detection`) for stress builds (user choice, 2026-11-21) — **Reversibility:** costly — about 59 lock call sites change, and the shim becomes the lock API of three crates.

**Read while write**
- A write does its rebase step (chunk, hash, store put) outside the file lock. Under the lock it copies the range to rebase. It releases the lock and does the I/O. It takes the lock again and publishes the result only if the file generation did not change; otherwise it drops the result and retries later. Readers hold the file lock only for memory copies (copy the overlay view or clone the stream), and then read with no lock. A read sees all of a write or none of it, because a write applies to the overlay under the file lock (user choice, 2026-11-21) — **Reversibility:** costly — it changes the write path and the overlay publish protocol that Phase 20's flusher reuses.

**Decided autonomously**
- [Locks] Q: "Per-file lock structure?" → Selected: "Each open inode has one `Arc<FileCell>` in a sharded `ino -> Arc<FileCell>` map. All handles of the inode share it, so `write_alias`, `owner_fh` and the heir swap go away. The last handle drop runs `finalize_write`." — internal structure, as in `.planning/research/ARCHITECTURE.md` Part 1.
- [Locks] Q: "Namespace consistency for inodes that are not open?" → Selected: "`NamespaceLocks`: a fixed striped lock array; multi-inode operations take their stripes in ascending stripe order; a rename mutex for cross-directory rename comes first" — research marks this reversible; the harness confirms it.
- [Locks] Q: "Single-inode metadata read-modify-write?" → Selected: "`DictMetadataStore::modify_inode(ino, FnOnce(&mut InodeMeta))` under the shard lock of `inode_live`" — internal API.
- [Locks] Q: "Lock rank?" → Selected: "1 commit gate (shared), 2 rename mutex, 3 namespace stripes (ascending), 4 `FileCell.state`, 5 short maps (`handles`, `orphans`, cell map shards, `readers` map), 6 store internals (`DictMetadataStore`, WAL, `ChunkStore`). Checker: thread-local stack of held ranks behind `debug_assertions`" — debug-only code.
- [Locks] Q: "`readers` map?" → Selected: "`HashMap<ino, Arc<Mutex<Reader>>>`; clone the Arc under the map lock and lock the per-inode reader only for the read" — internal.
- [Log] Q: "Log order?" → Selected: "The log record append (a memory copy plus an LSN) happens under the same locks as its apply; disk write and sync happen outside the locks. Replay creates an inode with the logged inode number" — owner decision from the milestone; only the code shape is decided here.
- [Log] Q: "Issue #21 body?" → Selected: "Update it in this phase to match the log-order rule (owner decision); no licensing text" — public text, but the owner already asked for it.
- [Tests] Q: "Per-write test counters?" → Selected: "Make `write_rebase_bytes`, `write_base_bytes`, `last_rebase_bytes` thread-local or test-only" — test code.
- [Tests] Q: "How are LOCK-05 and the POSIX rules shown?" → Selected: "Extend the Phase 18 harness: several handles per inode with read-only, read-write and `O_APPEND` modes; checks for visibility across handles, all-or-none reads, and appends that never overlap; lock wait rows show that readers do not wait for writer I/O; many-files runs show no cross-file contention" — test code.
- [Research] Q: "Append offset?" → Selected: "The phase research checks how the kernel FUSE client computes the `O_APPEND` offset in the SliceFS mount mode; the daemon takes the end-of-file offset from the current size under the file lock" — research item.

### Claude's Discretion
- Shard counts, stripe count, and the exact shim API, inside the rules above.

### Deferred Ideas (OUT OF SCOPE)
None — discussion stayed within phase scope. (Phase boundary: no dirty budget flusher and no `keep_budget` removal (Phase 20), no worker pool (Phase 21), no WAL group commit (deferred).)
</user_constraints>

<phase_requirements>
## Phase Requirements

| ID | Description | Research Support |
|----|-------------|------------------|
| LOCK-01 | No global lock held on the read or write path during disk I/O, a hash calculation, or a wait (store lock, `readers`, WAL append mutex, `dir_live`/`inode_live`) | Lock inventory (section "Lock Inventory"), cell protocol, WAL ticket design, metadata hot-path plan, chunk-store tails |
| LOCK-03 | One fixed lock rank; debug-build checker fails a test on a violation | `crate::sync` rank design, `NamespaceLocks` protocol, rank unit tests |
| LOCK-04 | Log record appended under the same locks as its apply; replay equals apply, creates by logged inode number; #21 body updated | Log-under-lock protocol, `create_inode_at`, replay changes, crash-replay harness |
| LOCK-05 | Parallel readers and writers on one file and on many files; several handles with read-only, read-write and `O_APPEND`; POSIX.1 2.9.7 rules | O_APPEND kernel finding, handle table, cell protocol, harness extension, deterministic park-in-I/O tests |
</phase_requirements>

## Project Constraints (from CLAUDE.md)

No `./CLAUDE.md` or `./.claude/CLAUDE.md` exists in the repo (checked). The binding project rules come from the user's memory notes and the orchestrator brief:
- Never hold a whole file in RAM; per-file daemon memory must be bounded, not O(file size) (memory note `no-whole-file-in-ram`, #98).
- Keep tests short; the Boost mount run is only by hand, on main or on `ci/**` (memory note `test-time-budget`). No local macOS mounts (macOS CI is allowed).
- FastCDC stays 4/8/32 KiB. No store backward compatibility needed (no deployments; the log format may change).
- Do not mention licensing in public GitHub text (relevant to the #21 body edit).
- `cargo` needs `export PATH="$HOME/.cargo/bin:$PATH"`.
- Reversible technical choices: decide and report; one-way doors need the owner.

## Summary

Phase 19 turns a design that is safe only with one request at a time into one that is correct and parallel. Today the safety comes from the single fuser dispatch thread: write, settle, setattr and `keep_budget` remove `OpenFileState` from `open_files` while they work (`filesystem.rs:1114`, `997`, `1847`, `1267`), so a second thread gets `EBADF`, a stale read or a no-op fsync. I ran the Phase 18 target `serialize_off_target` on the current code to confirm: it fails with `op=write errno 9` (`EBADF`) and `op=read ... bytes match no version in 1..=1` (stale read), with `max_inflight=4`. The fixes are structural: an `Arc<FileCell>` per open inode; a handle table that knows each handle's mode (including append); a copy → I/O-without-lock → publish-if-generation-unchanged protocol for every step that chunks, hashes or puts; a split of the WAL into an in-memory append (LSN ticket, under the apply locks) and a write-out after the locks; metadata-store locks that do not span disk reads or hashing; striped namespace locks; and a debug rank checker.

Three things in the brief need rulings the planner cannot infer, and this document makes them. (1) The locked "read while write" decision covers the overlay rebase only, but streaming writes (`state.push`, `to_overlay`, `finalize_write`, `fill_stream`) chunk, hash and put inline too; this research applies the same protocol to them. (2) "Store lock" in LOCK-01 means two locks: `ChunkStore.inner` (`Site::Store`) and the `Mutex<StoreIo>` (`Site::Io`). The first holds a seal join, an index-run write, pack creation, a KEEP_CAP wait and (error path) a durable write; the page-cache `pwrite` of one record stays under it as a documented exception. (3) The daemon never sees `O_APPEND` today (`test_open` drops the flag); on Linux the kernel computes the append offset from its cached `i_size`, and passes `O_APPEND` in the open and write requests, so the daemon must keep a per-handle append flag and take EOF from the current size under the cell lock.

The real size of the lock migration is not 59 sites: `grep` finds 375 lock call sites in `src/` of the three hot crates (including inline test modules; 60 of the chunk-store sites are in the in-memory `mem.rs`) plus 36 in `tests/`. `Arc<Mutex<StoreIo>>` is built at about 115 sites in 41 files and the commit gate is a public `Arc<RwLock<()>>` used by tests; both should stay `std` types in this phase (see Standard Stack).

**Primary recommendation:** Build in this order, each step green on `cargo test --workspace`: (1) `chunk_store::sync` shim + parking_lot + rank checker core; (2) metadata hot path (`modify_inode`, sharded `Live`, no `io` lock, `create_*_at`); (3) WAL append/write-out split; (4) chunk-store lock tails; (5) `FileCell` table and handle table (still pessimistic); (6) the optimistic copy→I/O→publish protocol, `O_APPEND`, write-under-lock logging; (7) `NamespaceLocks`, namespace logging, replay by logged inode; (8) harness extensions and un-ignore `serialize_off_target`. Plans 2, 3 and 4 touch disjoint files and can run in one wave.

## Architectural Responsibility Map

This is a single-process daemon; the tiers are the layers of the daemon.

| Capability | Primary Tier | Secondary Tier | Rationale |
|------------|-------------|----------------|-----------|
| Request admission and commit/GC exclusion (commit gate) | `slicefs-cli` callbacks / future worker | — | The gate must be shared for a whole request (GC invariant: a rebase never straddles `gc_begin`); it stays a `std` RwLock (S5) |
| Per-file data consistency (overlay, stream, size, mtime) | `FileCell` in `slicefs-cli/filesystem.rs` | `content`/`overlay` helpers | The file lock must cover only memory; chunk/hash/put happen outside it |
| Namespace consistency (create/unlink/rename/link, open-vs-unlink) | `NamespaceLocks` stripes in `slicefs-cli` | `DictMetadataStore` shard locks | Closed inodes have no cell, so a separate stripe array orders namespace changes |
| Single-inode metadata read-modify-write | `DictMetadataStore::modify_inode` | — | Atomic under the `inode_live` shard lock; no filesystem-level lock needed |
| Log order (append under apply locks) | `handlers.rs` + `filesystem.rs` (callbacks into the locked section) | `metadata` WAL (ticket) | The record must be appended while the apply locks are held, so the code that holds them must append |
| Log durability (write-out, fsync) | `metadata` WAL strategies | — | Outside all apply locks; ticket/LSN + `write_out(lsn)` |
| Directory/inode caches | `DictMetadataStore` (`Live`) | `chunk-store` for node reads | Shard by inode; load cold entries outside the shard lock |
| Object storage and sync | `chunk-store` | — | Hash/compress/lookup already outside `inner`; tails of `inner` still hold waits and index writes |
| Lock rank checking and lock timing | `chunk_store::sync` shim (debug and `lock-stats`) | — | Lowest crate shared by all three hot crates |

## Standard Stack

### Core
| Library | Version | Purpose | Why Standard |
|---------|---------|---------|--------------|
| `parking_lot` | `0.12` (locked: 0.12.5), features `arc_lock` (+ optional `deadlock_detection`) | `Mutex`, `RwLock`, `Condvar` without poisoning | Already in `Cargo.lock` via the vendored fuser (`cargo tree -i parking_lot` shows only fuser as dependent), so no new code compiles; `0.13` would add a second copy |
| `lock_api` | 0.4.14 (transitive) | Guard types behind `arc_lock` | Comes with parking_lot; no direct dependency needed |

`[VERIFIED: Cargo.lock]` parking_lot 0.12.5 and parking_lot_core 0.9.12 are present; `[VERIFIED: ~/.cargo/registry/src/*/parking_lot-0.12.5/Cargo.toml:14,53-54,62]` `rust-version = "1.71"`, `arc_lock = ["lock_api/arc_lock"]`, `deadlock_detection = ["parking_lot_core/deadlock_detection"]`, `send_guard = []`. `[VERIFIED: parking_lot-0.12.5/src/lib.rs:34-35]` `send_guard` and `deadlock_detection` cannot be combined (`compile_error!`). `[VERIFIED: parking_lot-0.12.5/src/raw_mutex.rs:74,101 and raw_rwlock.rs:1149-1156]` deadlock tracking hooks `Mutex` and `RwLock` acquisition; the only `Condvar` hook is at `condvar.rs:354`.

### Supporting
| Library | Version | Purpose | When to Use |
|---------|---------|---------|-------------|
| `hdrhistogram` | 7.6 (already optional in chunk-store, dev-dep in slicefs-cli) | Latency/lock rows | Existing; no change |
| `tempfile` | workspace | Tests | Existing |

No new crate is needed. Do not add `dashmap`, `crossbeam`, `shuttle` or `arc-swap` in this phase (shuttle is Phase 20's protocol model).

### Alternatives Considered
| Instead of | Could Use | Tradeoff |
|------------|-----------|----------|
| Shim in `chunk-store` (`chunk_store::sync`) | New tiny crate `slicefs-sync` | A new crate is cleaner but adds a workspace member; `lock_stats` already lives in `chunk-store`, which `metadata` and `slicefs-cli` already depend on. Recommended: `chunk-store`. |
| Stripes for namespace locks | Per-inode lock table | Stripes need no eviction logic (PITFALLS 6a); already decided |
| Own `ShardedTable` over the shim | `dashmap` | Hidden shard locks defeat rank checking and lock timing |

### Keep `std` in this phase (discretion, flagged)
- `Arc<std::sync::Mutex<StoreIo>>` in the public constructors (`DictMetadataStore::new`, `SliceFsFilesystem::new`, `load_from_root`, ~115 construction sites in 41 files, `grep` result). After plan 2 the hot path does not touch `io`; it remains only for commit-time bookkeeping. Changing its type is a mechanical but noisy edit that a later phase can do.
- The commit gate `Arc<RwLock<()>>` returned by `SliceFsFilesystem::commit_gate()` (used by `fuse_callbacks.rs` 33 times, `commit_timer.rs`, and tests in `gc_online_tests.rs`, `write_log_replay_tests.rs`, `stress_harness.rs`). S5 decided to keep the std RwLock `[VERIFIED: docs/research/multithreaded-request-handling.md "S5 result", Decides: step 7]`. Consequence: `parking_lot::deadlock` cannot see a cycle that goes through the gate (no `Condvar`/std lock tracking), so the rank checker must model the gate (see Rank design) and the stress watchdog must also have a wall-clock timeout (the harness already has `watchdog`, `stress_harness.rs:171`).
- `cas-local` and `slicefs-traits` (cold, in-memory).

**Installation:**
```toml
# workspace Cargo.toml [workspace.dependencies]
parking_lot = { version = "0.12", features = ["arc_lock"] }
# crates/chunk-store, metadata, slicefs-cli: parking_lot.workspace = true
# crates/slicefs-cli/Cargo.toml [features] (and forwarded in metadata, chunk-store):
# deadlock-detect = ["parking_lot/deadlock_detection"]
```

**Version verification:** `cargo search parking_lot --limit 1` printed `parking_lot = "0.13.0"` as latest on crates.io; the tree stays on 0.12.5 on purpose (one copy with fuser). `[ASSUMED]` that 0.13 needs a newer toolchain than 0.12 (STACK.md says Rust 1.95; local toolchain is `rustc 1.95.0-nightly (9e79395f9 2026-02-10)` `[VERIFIED: rustc --version]`).

## Package Legitimacy Audit

| Package | Registry | Age | Downloads | Source Repo | Verdict | Disposition |
|---------|----------|-----|-----------|-------------|---------|-------------|
| parking_lot | crates.io | since 2016-05 (10 yrs) | 23.3M/wk | github.com/Amanieu/parking_lot | OK | Approved |
| lock_api | crates.io | since 2018-06 | 23.0M/wk | github.com/Amanieu/parking_lot | OK | Approved (transitive) |

`[VERIFIED: gsd-tools package-legitimacy check --ecosystem crates parking_lot lock_api]` both returned `OK`; no postinstall; neither deprecated. Docs-level claims about the crate come from the 0.12.5 source in the local cargo registry (authoritative, read this session) and docs.rs.

**Packages removed due to [SLOP] verdict:** none
**Packages flagged as suspicious [SUS]:** none

## Architecture Patterns

### System Architecture Diagram (after Phase 19; N threads = the harness today, the pool in Phase 21)

```
request thread (harness / later: worker)
  |
  | 1. gate.read()  (std RwLock, shared, rank 1) -- held for the whole request
  v
handlers.rs  handle_*(fs, ...)
  |  builds Logged { frozen clock } ; passes a LOG CLOSURE into the locked section
  v
filesystem.rs  SliceFsFilesystem
  |
  +-- namespace ops (create/mkdir/unlink/rename/link/setattr/xattr/open)
  |     rename mutex (rank 2, cross-dir only) -> stripes asc (rank 3) -> apply via
  |     meta.modify_inode / link / unlink -> LOG APPEND (ticket) -> unlock
  |
  +-- file data ops (write/read/fsync/flush/release/truncate)
  |     cells.get_or_insert(ino) -> Arc<FileCell>         (rank 5 shard lock, released at once)
  |     cell.state.lock() (rank 4):  decide, copy a bounded piece, note gen   [memory only]
  |     ---- unlock ----
  |     chunk + hash + store put  (outside every SliceFS lock; inside the shared gate)
  |     cell.state.lock():  gen unchanged? publish (swap overlay/stream, install_root) + LOG APPEND (ticket)
  |     ---- unlock ----          else: drop result, retry (2nd try under the lock)
  |
  +-- read: cell lock only to copy RangeView / clone Stream; then read_root through per-inode reader
  |
  v
after all apply locks are dropped:  meta.log_write_out(ticket)   (PerOp: write() into the kernel;
                                    buffered: only if over the RAM cap)  -- still inside the gate
  v
reply

metadata::DictMetadataStore            chunk-store::ChunkStore
  sharded Live<inode>, Live<dir>         inner lock: memory + page-cache pwrite only
  (cold loads outside shard locks)       waits (seal join, KEEP_CAP) before the lock
  WAL: append(ticket) | write_out | fsync outside the writer mutex
commit timer: gate.write() -> commit_cut_begin (locks each cell, rank 4, after the gate) -> drop gate -> finish
```

### Recommended Project Structure (additions)
```
crates/chunk-store/src/
├── sync.rs            # NEW shim: Mutex/RwLock/Condvar over parking_lot + Rank + lock_stats hook (debug rank stack)
├── lock_stats.rs      # extend: Site::{FileCell, Stripe, CellMap, Handles, DirLive, InodeLive, WalQueue,...}; recorders sized by Site::ALL
crates/metadata/src/
├── store.rs           # modify_inode, sharded Live, create_inode_at/create_directory_at, no io lock on hot path
├── wal/               # append()/write_out()/Lsn; fsync outside the writer mutex
crates/slicefs-cli/src/
├── filesystem.rs      # FileCell, HandleTable, cell protocol; (consider splitting into file_cell.rs)
├── locks.rs           # NEW NamespaceLocks (stripes) + lock order helper + rename mutex
├── overlay.rs         # copy_step()/discard(), generation counter
crates/slicefs-cli/tests/
├── lock_free_io.rs        # NEW deterministic "parked in I/O" tests (default features)
├── multi_handle_posix.rs  # NEW (or modes in stress_harness.rs): POSIX 2.9.7 modes
└── common/mod.rs          # NEW shared dump()/reload_and_replay() lifted from write_log_replay_tests.rs
```

### Pattern 1: The cell protocol (copy → I/O without lock → publish if generation unchanged)

**What:** Every step that chunks, hashes or puts objects (streaming `push`, `to_overlay`, `rebase_step`, `flush_handle`, `finalize_write`, `settle_pending`) follows three phases. Phase A (cell locked, memory only): validate, pick the piece, clone or copy it, read `gen`. Phase B (no SliceFS lock, still inside the request's shared gate): FastCDC, BLAKE3, store put/splice; the test hook fires here. Phase C (cell locked): if `gen` is unchanged, publish the result and append the log record under the same lock hold; otherwise drop the result (objects are idempotent garbage until GC) and retry. After one failed attempt the next attempt runs Phase B under the lock, so a busy file always makes progress and memory stays bounded.

**What exists and what is missing today:** there is no generation. `OpenFileState` has `ino, write_mode, byte_count, cas_committed, last_committed_root, min_size, modified` `[VERIFIED: filesystem.rs:159-179]`. `Overlay::take` removes the extents from the overlay (`overlay.rs:66-91`; called at `filesystem.rs:925`), and the error path puts them back on top of whatever was written meanwhile (`filesystem.rs:959-962`), which would overwrite newer bytes. Readers lose the taken bytes while the step runs. So the split needs:

- `Overlay::copy_step(free) -> Vec<(u64, Vec<u8>)>`: copies (does not remove) extents from the sweep point (same selection as `take`, `overlay.rs:66`). Memory: at most about `max(free, MIN_TAKE)` extra bytes (`REBASE_STEP` is 1 MiB, `filesystem.rs:103`); never the whole overlay. Readers keep seeing the bytes in the live overlay during Phase B.
- `Overlay::discard(copied)`: after publish, removes exactly the copied ranges (heads of extents; same split logic as `take`). Valid only because `gen` is unchanged, so the extents are identical.
- `gen: u64` inside `Overlay`, incremented by every `&mut self` method that changes extents, `visible`, `size` (`put`, `write`, `truncate`, `take`, `discard`, and the constructors that replace the overlay: `Overlay::new` after publish must continue from the old `gen + 1`). Put it in `Overlay` so no caller can forget. Add a unit test that calls each mutating method and asserts `gen` moved. For the streaming mode keep `gen` in `WriteMode`/state (bumped by push, pending insert, `fill_stream`, `min_size`/`byte_count` change, mode switch, truncate to 0).
- Concurrent plain writes commute with a rebase only when they touch bytes outside the copied ranges, but the size, base and `visible` can change under any write. Start with the coarse rule: any change → drop. Cheap improvement, optional: publish anyway when the only changes since Phase A are puts whose ranges do not intersect the copied ranges (keep a small list of ranges written since `gen`); leave this to Phase 20 unless the harness shows starvation.
- At most one in-flight rebase per cell: a flag `rebasing: bool` in the state, set in Phase A, cleared in Phase C or on drop; a second writer over the limit skips its inline step (the dirty cost is above the limit only by what concurrent writers add, bounded by the hard cap below).
- Hard cap (memory rule #98): if the cell's cost exceeds `limit + 2 * REBASE_STEP` (discretion) after a failed publish, the writer runs its step under the lock (Phase B inside the lock; only this cell is blocked, never a global lock). This keeps per-file RAM bounded even under continuous invalidation.
- `rebase_hook` (`filesystem.rs:945-948`) must fire in Phase B with the cell unlocked. The unit test at `filesystem.rs:4259-4271` expects the hook to run after the new root is finished and before `install_root`; the tests in `gc_online_tests.rs:225-345` run a GC mark and sweep inside the hook while the test holds the gate shared. Both keep working if the hook fires between Phase B and Phase C, with no cell lock held. These tests are also the pin for the GC invariant (see Pitfall 3).
- `rebase_step` publish (Phase C) needs `old` root = `ov.base` (the manifest names the base by invariant: `to_overlay` installs it at `filesystem.rs:874`, each step installs its root at `:949`), so it does not need `get_manifest` (a disk read) under the cell lock. Prepare the manifest digest in Phase B (`intern_manifest` hashes and puts) and only swap the digest in Phase C: add `DictMetadataStore::prepare_manifest(&[ObjectKey]) -> Digest` and `set_manifest_digest(ino, Digest)`. (Discretion; without it, `set_manifest` hashes one small node under the cell lock.)
- Publish must use the state's `last_committed_root` at publish time (under the lock) for refcount decrements, never the value read in Phase A. Two concurrent `flush_handle`s (two fsyncs) would otherwise both decrement the same old root or install the older root last (see Pitfall 5).

**Streaming writes (ruling).** `test_write` pushes under no lock today only because the state was taken out (`filesystem.rs:1154`). With the state in the map, a push under `cell.state` would block readers for a FastCDC+hash+put per write, which breaks LOCK-05 ("readers do not wait for the I/O of writers"). Apply the same protocol: Phase A clones the `Stream` (`FileWriter` = `buf` under 2 × CHUNK_MAX + level entries; `chunk-store/file.rs:109-116`, `Stream` derives `Clone` at `content.rs:217`), checks order (`offset == next_expected_offset`, reorder window), Phase B pushes into the clone, Phase C swaps the clone in if `gen` is unchanged. A failed attempt is retried once under the lock. Cost: one memcpy of at most 64 KiB per write. `fill_stream`/`push_zeros` (`filesystem.rs:408-436`), `to_overlay` (`:855-885`: `state.clone().finish(cs)` at `:866`, `has_content`/`get_manifest` at `:867-869`) and the first-write `has_content` check (`:1127`) also move to Phase B. Reads that must settle pending out-of-order writes (`test_read` calls `settle_pending`, `:1457-1462`) become a writer-like step through the same protocol.

**Finalize (last handle).** `finalize_write` (`:1362-1423`) today consumes the state. New rule: the last write-handle drop loops Phase A/B/C in pieces of at most `FINAL_STEP` (discretion, 4-8 MiB; the old code did one `usize::MAX` step, `:889-891`) until the overlay is clean or a new writer handle appeared (`write_handles > 0`: the cell lives on, nothing to finalize); when clean and `write_handles == 0` it sets `retired = true` and removes the cell from the map in one cell-lock hold (rank 4 then 5, ascending). A concurrent `open` that finds a retired cell must retry its lookup (compare `Arc::ptr_eq` after locking); this is the identity bug of PITFALLS 6a `[VERIFIED: .planning/research/PITFALLS.md:102-114]`. Copying the whole overlay at once (32 MiB dirty limit, `filesystem.rs:98`) would double the RAM of the file; stepwise avoids it.

**Truncate of a closed file** (`test_setattr_size` case B, `:1877-1928`) splices committed content with no lock at all today. Route it through a transient cell (`get_or_insert`, `write_handles = 0`, run, retire) and hold the cell lock across the splice. This is the one accepted exception to "no I/O under the cell lock" (a rare size change of a closed file); it keeps stripes free of content I/O (see Pattern 4 rule).

### Pattern 2: Handle table and cell table

`handles: Mutex<HashMap<u64, u64>>` (fh → ino) cannot say whether a handle is writable or `O_APPEND`, and `close_handle`/`drop_last_link` scan all handles with `values().any(...)` (`filesystem.rs:589`, `609`). Replace it with `HashMap<fh, Handle { ino, write: bool, append: bool }>` plus a per-inode open count (`HashMap<ino, u32>` and the orphan flag) so the "is it open" check is O(1) and happens under the inode's stripe. `release` decides read-only vs write from the handle table (today from membership in `open_files`/`write_alias`, `test_release_full` `:2687-2694`; `handle_release` and `handle_flush` call `ino_of_fh(fh).is_some()` at `handlers.rs:186`, `201`).

`FileCell { ino, state: Mutex<CellState>, dirty: AtomicUsize }` with `CellState { file: OpenFileState, write_handles: usize, retired: bool, rebasing: bool, ... }`. The cell map is `[Mutex<HashMap<u64, Arc<FileCell>>>; SHARDS]` (rank 5); `get_or_insert` clones the Arc and drops the shard lock at once. `dirty` mirrors `dirty_cost` (`filesystem.rs:114-119`) so `keep_budget` can sum without nesting cells (see `keep_budget` below). `write_alias`, `owner_fh`, the heir swap (`:1326-1355`), `write_handle_of` (`:814-819`) and the `find(|s| s.ino == ino)` scans (`:1457-1466`, `:2410-2415`, `:2458`) become `cells.get(ino)`.

### Pattern 3: WAL ticket (LOCK-04)

Today `DictMetadataStore::log_wal_entry` locks `self.wal` (`Site::Wal`) across `w.log_mutation(entry)` (`store.rs:515-525`); PerOp's `log_mutation` writes the record into the segment and flushes it to the kernel under the `writer` mutex (`wal/per_op.rs:64-77`), and `flush_and_sync` holds `writer` across `sync()` (`:79-83`). `Op::Write` records carry the payload, so the hold includes a copy of up to the write size: Phase 18 measured `wal`/`wal_writer` hold p50 of 1.2-2.4 ms and a 21 ms max (fsync) `[VERIFIED: docs/research/multithreaded-request-handling.md "S3 result"; 18-02-SUMMARY.md:63]`.

New shape (the trait keeps `log_mutation` for roots and snapshots, and gets default implementations so metadata tests that call it keep compiling):
```rust
trait WalStrategy {
    fn append(&self, entry: &WalEntry) -> Result<Lsn, WalError>;   // memory only: push to a queue + next LSN; short mutex
    fn write_out(&self, upto: Lsn) -> Result<(), WalError>;        // drains the queue into the segment (+ flush to the kernel); idempotent
    fn log_mutation(&self, e: &WalEntry) -> Result<(), WalError> { let l = self.append(e)?; self.write_out(l) }
    // flush_and_sync: lock writer -> flush -> clone file (file_for_sync) -> unlock -> sync_all on the clone
}
```
- Lock order inside the WAL: `io` (writer) → `q` (queue). Appenders take only `q` and never wait for `io`. `DictMetadataStore::log_op` splits into `log_append(bytes) -> Lsn` (called under the apply locks) and `log_write_out(Lsn)` (called by the handler after every apply lock is dropped but before the reply, still inside the gate). PerOp keeps its promise ("a completed request survives a crash of the daemon") because write-out precedes the reply. The encoding (`Record::encode`) and the payload `to_vec` happen under the apply lock: that is the "memory copy".
- `cut()` (under the exclusive gate, `store.rs:686-693`), `install_root`, `rotate` and `shutdown` must drain the queue first. Because every request holds the shared gate through its write-out, no request is between append and write-out when `cut` runs. The same drain is what `BufferedLog::cut` already does with `write_out` (`wal/buffered.rs:133-146`).
- `flush_and_sync` must not hold `writer` across `sync_all`: use the same clone-the-file pattern as `install_root` (`SegmentWriter::file_for_sync`, `segment/writer.rs:116`) and keep the `unsynced`/`disk_syncs` bookkeeping correct (add `begin_sync() -> Option<File>` / `end_sync()`; the writer's `sync()` skips work when nothing is unsynced). `behind.settle()` (`per_op.rs:80`) also fsyncs under the `behind` mutex; take the old writers out under the mutex, sync outside. This is "sync outside the mutex", in scope. Leader/follower group commit and `durable_lsn` are deferred (CONTEXT).
- `BufferedLog` (`FlushOnFsync`, `Periodic`): `log()` (`buffered.rs:79-95`) writes out under `pending` when over the cap. Make `append` push only; `write_out` writes the held records when over `BUFFER_CAP_BYTES` (4 MiB, `wal/mod.rs`), with the order `writer` → `pending` for the drain (the current order is `pending` → `writer` everywhere, comment "Lock order everywhere: pending, then writer", `buffered.rs:102`; pick one order and document it; the periodic timer thread calls `flush_and_sync`, so it must follow it too).
- Failure semantics stay: a failed write-out bumps `wal_fail_seq` and `flush_wal` fails until the next commit (`store.rs:454-464`, `515-525`).
- `Release`/`Flush` records with several handles. `oplog::Op::Release` says "Release of a write handle: it finalizes the content and sets times" `[VERIFIED: oplog.rs:44-47]`. Replay keeps one handle per inode (`handles: HashMap<u64,u64>`, `oplog.rs:245`) and finalizes it at the first `Release`. In the live run only the last write-handle release finalizes. Final state is the same (a later `Write` reopens a handle), but to keep replay faithful, log `Release` only when the apply finalized (last write handle) and `Flush` only when it did work; add a two-handle replay test. `Truncate` (open with `O_TRUNC`) is appended inside the open's cell-lock hold.

### Pattern 4: NamespaceLocks and the stripe rules

`NamespaceLocks { stripes: [Mutex<()>; N] }` (N discretion; 256 recommended, one cache-line each with padding) in `slicefs-cli/src/locks.rs`; `stripe(ino) = (ino * 0x9E37_79B9_7F4A_7C15) >> (64 - 8)`. Lock a *sorted, deduplicated* set of stripe indexes (two inodes in one stripe lock once). Rules:
1. Stripes cover namespace metadata only (directory entries, `nlinks`, orphan/open-count checks, setattr/xattr ordering). A stripe is never held across content I/O, hashing or a splice. Closed-file truncate uses a transient cell (Pattern 1), not a stripe.
2. Lookup-then-lock revalidation (PITFALLS 6b `[VERIFIED: PITFALLS.md:102-114]`): unlink/rmdir/rename/link learn the child inode by reading the parent directory. Algorithm: lock stripes of the known inodes; read the child; if the child's stripe is not yet held (or is lower than the highest held), release, lock the full sorted set, re-read and compare; retry on mismatch. Write this once as `lock_namespace(&self, parent, name, ...) -> NsGuard` and use it from every op.
3. `create`/`mkdir`/`mknod`/`symlink` lock the parent's stripe, and check the parent directory is still live under it (PITFALLS 6c: a concurrent `rmdir` of the parent).
4. `open` locks `stripe(ino)` briefly, verifies the inode exists (`get_inode`), inserts the handle (rank 5) and gets/creates the cell (rank 4 inside the stripe, ascending). Today `test_open` does not check existence at all. `drop_last_link` (`:608-618`) and `close_handle` (`:584-593`) take the same stripe so the "last handle closed / last link removed" decision is atomic (today a real race: unlink sees open, close sees no orphan yet, the orphan leaks until unmount).
5. `setattr`/`setxattr`/`removexattr` take `stripe(ino)` and, if a cell exists, its lock (stripe → cell). A `Write` takes only the cell. So every op that changes the same inode's size/mtime/ctime shares at least the cell lock with writes, and log order equals apply order on it (ctime merge rules in `stamp_content` `:1527-1538` are not commutative with `setattr`'s unconditional ctime).
6. Cross-directory rename takes the rename mutex (rank 2) first. Note: `simulate_rename` has no "source is an ancestor of target" check today (`filesystem.rs:2228-2327`); the mutex is where that check belongs, but adding it is a behavior change; record it as a follow-up, do not hide it in this phase.

### Pattern 5: Rank checker design (LOCK-03)

`Rank` enum (values from the CONTEXT decision): `Gate=1, Rename=2, Stripe=3, Cell=4, Short=5, Store=6`. `chunk_store::sync::{Mutex, RwLock}::new(rank, value)` (or `new_unranked`). In `cfg(debug_assertions)` a `thread_local! { static HELD: RefCell<Vec<(Rank, u64)>> }`; `lock()` panics with both names if the new rank is lower than the top, or equal and the secondary key (stripe index) is not strictly greater for ranks 3 and 4 (two cells at once is always a bug); equal ranks 5 and 6 may nest (the store's internal order stays documented in `store.rs:7-15` and `store.rs:198-206` and is not enforced; enforcing sub-ranks there would flag existing legal paths such as `flush_inodes` taking `inode_map` → `inode_live` → `io` → `inode_data`, `store.rs:613-621`). The guard's `Drop` pops; `Condvar::wait` must pop and re-push around the wait. The gate: it stays a std RwLock, so register it in the stack through `SliceFsFilesystem::gate_shared()`/`gate_exclusive()` helper methods returning a small RAII marker (`rank::enter(Rank::Gate)`) and use them in `fuse_callbacks.rs`, `commit_timer.rs`, `gc_start_mode`, and the harness `gated()` (`stress_harness.rs:410-426`). Optional if the diff is judged too large: then the checker covers ranks 2-6 and the gate rule is "taken first, by construction". Tests: `#[cfg(debug_assertions)] #[should_panic]` unit tests for (a) cell then stripe, (b) stripes descending, (c) two cells; and one positive test that the exclusive-gate holder may lock every cell one at a time (`commit_cut_begin`, `gc_start_mode`).

Lock-stats integration: `lock_stats::lock(&crate::sync::Mutex<T>, Site)` keeps its signature shape; the `TryLockError::Poisoned` arm disappears (`lock_stats.rs` `on::lock`). `Site::ALL` and the recorder array are hard-sized to 7 (`lock_stats.rs:38`, `fn recorders() -> &'static [Mutex<SiteHists>; 7]`): size them by `Site::ALL.len()`. New sites: `FileCell`, `Stripe`, `CellMap`, `Handles`, `DirLive`, `InodeLive`, `WalQueue`. `open_files` disappears, so `lock_stats_report.rs` (`for label in ["store","io","wal","wal_writer","wal_pending","open_files","readers"]`) must change its label list.

### Pattern 6: Readers (LOCK-01 / S3)

`read_root` (`filesystem.rs:981-990`) holds the `readers` mutex across `Reader::open` and `Reader::read`. New: `readers: Mutex<HashMap<u64, Arc<Mutex<Reader>>>>` (rank 5 map, rank 5 per-inode); clone the Arc under the map lock, lock the per-inode reader for the read. A `Reader` is small (root `Arc`, up to `NODE_CACHE`=16 nodes, one chunk; `chunk-store/file.rs:226-233`) and `Reader::read` is `&mut self` (`content.rs:316`). Two readers of one hot file would still serialize on the per-inode reader. Recommended (discretion): `try_lock` the per-inode reader and, if busy, open a private `Reader` for this read instead of waiting. The `readers.len() >= 64 → clear()` eviction (`:983-985`) must run under the map lock only. `reclaim_inode` removes the entry (`:602`).

### Pattern 7: Metadata store hot path (LOCK-01)

See the inventory for each site. Target shape in `store.rs`:
- `modify_inode(ino, FnOnce(&mut InodeMeta) -> R) -> Result<R>` under the shard lock; adjusts `logical_bytes` from the size delta inside it (`update_inode` today reads `load_meta` then `store_meta`: two lock holds, `store.rs:1008-1027`). Replace the read-modify-write helpers in `filesystem.rs`: `stamp_content` (`:1527`), `touch_dir` (`:1034`), `adjust_nlinks` (`:1045`), `install_root`'s stamp (`:846`), the unlink/link/rename nlinks and ctime edits (`:2161-2187`, `:2199-2216`, `:2285-2296`, `:2320-2324`), the setattr family (`:1932-1973`, `:2560-2593`).
- Shard `Live<InodeMeta>` and `Live<DirList>` into 16-64 shards by `ino`. Cold loads (`load_inode`, `DirList::load`) happen with no shard lock; insert-if-absent afterwards (what `load_meta` already does at `store.rs:583-588`; `with_dir` does not, `:547-556`).
- Hot-path node reads/writes use `Arc<ChunkStore>` (cloned once from `io`) through `NodeReader` and a small per-call `NodeWriter` (both exist: `store_io.rs` `NodeReader`, `NodeWriter`), and move the `failed`/`unsynced` bookkeeping of `StoreIo` (`store_io.rs`, `StoreIo` struct) into the store as an `AtomicU64` plus `Mutex<Option<io::Error>>`, so `begin_commit_sync` (`store.rs:817`) needs no `io` lock for the hot path. Keep `Arc<Mutex<StoreIo>>` in constructors (ARCHITECTURE.md Part 1 and this research agree).
- `flush_inodes` (`:613-630`) and `flush_dirs` (`:639-653`) hash and put every dirty entry under their locks (up to `DIRTY_FLUSH` = 1024 in non-test builds, `store.rs:76`). New: under the shard lock take the dirty set (move to `flushing`, as `take_dirty` does for commits, `store.rs:118-128`), release, write nodes, then re-lock and install each digest only if the entry is still `flushing` (not re-dirtied) **and still exists**. Without the "still exists" check a concurrent `delete_inode` (`store.rs:1029-1057`) is resurrected by the late install (`finish_inner` already has the `flushing.remove(&ino)` check, `:796-811`).
- `create_directory` (`:1059-1106`) holds `inode_map` across `with_dir` (which can read a directory from disk). Allocate the number, drop `inode_map`, then add to the parent.
- `create_inode_at(ino, &InodeMeta)` and `create_directory_at(parent, name, ino, &InodeMeta)` (or an `Option<InodeId>` argument on the existing methods): reserve `ino` (`next_ino = max(next_ino, ino + 1)`; `AlreadyExists` if present). `MetadataStore` is implemented only by `DictMetadataStore` `[VERIFIED: grep "impl MetadataStore" finds crates/metadata/src/store.rs:982 only]`, so extending the trait in `slicefs-traits/src/metadata.rs` (`create_inode` at the trait line 146, `create_directory` 161) is cheap. `advance_next_ino` (`store.rs:502-507`) stays for tests.
- `with_dir`'s per-call flush (`:561-568`) and `evict_clean` should not run under a held shard lock.

### Pattern 8: `keep_budget` in Phase 19

`keep_budget` (`filesystem.rs:1243-1292`) sums every state under `open_files` on each write (`:1250-1254`), which with cells would nest cell locks on every write even when under budget, and its cross-file branch removes another file's state (`:1267-1274`). It cannot stay as written. Minimal Phase 19 form: global `AtomicUsize` total plus per-cell `dirty: AtomicUsize` (updated under the cell lock by delta); the under-budget check reads the atomic only; when over budget, pick the largest cell by scanning the cell map shards (read each cell's `dirty` atomic, no cell lock), then run `free_dirty` on it through the cell protocol with no other cell lock held. The write that triggers it releases its own cell first. This keeps `dirty_budget_tests.rs` semantics and leaves the flusher thread, the throttle and the time-limited waits to Phase 20. Phase 19 tests must stay below the budget (CONTEXT).

### Anti-Patterns to Avoid
- **Holding the cell lock across chunk/hash/put "because it is simpler".** It makes `serialize_off_target` pass on data but fails LOCK-05 criterion 1; acceptable only as the temporary state at the end of plan 5, and for the 2nd retry attempt and closed-file truncate.
- **Letting a stripe cover content I/O.** Cross-file contention returns through stripe collisions.
- **Taking the gate inside a handler.** The gate is std and writer-preferring (S5: a waiting commit blocks new shared holders up to the longest hold); a second shared acquire on a thread that already holds it can deadlock with a queued commit. No new code may re-take it.
- **Reads outside the request's gate.** The lock-free part of a read (`RangeView::read`, `read_root`, `finish` of a clone) must run inside the shared gate hold (see Pitfall 3).
- **Logging after the locks drop.** That is today's `handlers.rs` shape and the reason replay can fail.

## Don't Hand-Roll

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|-------------|-----|
| Mutex/RwLock/Condvar without poisoning | Own spinlock or atomics | `parking_lot` 0.12 through the shim | Parking, fairness, and `deadlock_detection` are done and shared with fuser |
| Deadlock detection | Own cycle detector | `parking_lot::deadlock::check_deadlock()` in a stress-only watchdog thread + the rank checker | Detection covers parking_lot `Mutex`/`RwLock`; the rank checker covers the gate and ordering at test time |
| Atomic inode read-modify-write | Filesystem-level inode locks | `DictMetadataStore::modify_inode` | One lock hold, no lock table |
| Object-store hashing/chunking | Anything new | Existing `FileWriter`, `splice`, `Stream::clone` + `finish` | Keys must equal the key of a fresh write (splice invariant) |
| Pack-file backpressure | New queue | The existing `RetryingPack` KEEP_LIMIT/KEEP_CAP with the wait moved before the lock | The semantics (`retry.rs:33-37`) are right; only where the wait happens is wrong |
| Lock-timing histograms | New recorder | `chunk_store::lock_stats` (hdrhistogram, existing) | Phase 18 report format and tests depend on it |

**Key insight:** every hard part of this phase is a protocol (when to copy, when to publish, who appends), not a data structure. Reuse `Overlay`, `RangeView`, `Stream` and the existing hooks; add generation checks and tickets.

## Lock Inventory (today, with file:line)

All lines verified by reading the files this session.

### A. Global locks held across disk I/O, hashing or a wait

| # | Site | Lock | Held across | Class |
|---|------|------|-------------|-------|
| 1 | `filesystem.rs:981-990` `read_root` | `readers` (Site::Readers) | `Reader::open` and `Reader::read` (index lookup, pread, zstd, BLAKE3 verify) | LOCK-01; measured: wait p50 8.7 ms at N=8 (S3) |
| 2 | `store.rs:547-556` `with_dir` | `dir_live` (+ `io`) | `DirList::load` (disk read) | LOCK-01 |
| 3 | `store.rs:639-653` `flush_dirs` (called `:563`) | `dir_live` + `io` + `dir_data` | `list.intern` (hash + put) of every dirty dir | LOCK-01 |
| 4 | `store.rs:573-594` `load_meta` | `io` | `load_inode` (disk read) at `:581` | LOCK-01 |
| 5 | `store.rs:613-630` `flush_inodes` (called `:604` from `store_meta`) | `inode_map` + `inode_live` + `io` + `inode_data` | `intern_inode` (hash + put) of every dirty inode, when 1024 are dirty | LOCK-01 |
| 6 | `store.rs:1129-1147` `set_manifest`/`get_manifest` | `io` | `intern_manifest` / `load_manifest` (`get_manifest` runs on every open) | LOCK-01 |
| 7 | `store.rs:1149-1231` xattr methods | `io` | `load_xattrs`/`intern_xattrs` | LOCK-01 |
| 8 | `store.rs:1059-1106` `create_directory` | `inode_map` | `with_dir` (possible disk read) at `:1087` | LOCK-01 |
| 9 | `store.rs:515-525` `log_wal_entry` | `wal` (Site::Wal) | `log_mutation` (record copy + `write()`) | LOCK-01/04; measured hold p50 1.2-2.4 ms (S3) |
| 10 | `wal/per_op.rs:64-77`, `79-83` | `writer` (Site::WalWriter), `behind` | write + flush; `sync()` (fsync) and `Behind::settle` (fsyncs) | LOCK-01/04; max hold about 21 ms (18-02-SUMMARY) |
| 11 | `wal/buffered.rs:79-95`, `99-113` | `pending` + `writer` | `write_out` + `flush` over the cap; fsync in `flush_and_sync` | LOCK-01/04 |
| 12 | `chunk-store/lib.rs:1362-1372` `put` → `append` `:1468-1488` | `inner` (Site::Store) | pack create + header write (`:1472-1473`); `seal` (`:1485` → `:2617` → `join_seal` `:2618`, a thread join = wait for the previous pack's fsync); `flush_if_full` (`:1372`, `:2530`) → `flush_index` (`:2921`, index run write) | LOCK-01 (store lock) |
| 13 | `chunk-store/retry.rs:228-243` `RetryingPack::write_at` (called under `inner` by `append`) | `inner` | `settle(true)` at KEEP_CAP (`:236-237`, a wait for the background fsync), `start()` (thread spawn), `sync_here` (`:150`, inline fsync when the file has no sync handle) | LOCK-01 (wait) |
| 14 | `chunk-store/lib.rs:1398`, `1441` → `raise_trust_floor` `:1450-1458` | `inner` | `write_durable` (fsync) on an index-read error path | LOCK-01 (rare) |
| 15 | `chunk-store/lib.rs:2351-2370` `read` | `inner` | `inner.reader(io, pack)` (`:2949`, opens the pack file if not cached) and a re-lookup when the index generation changed | LOCK-01 (minor) |

### B. Exclusive-gate holds that include I/O (known exception, not in the LOCK-01 list)

| Site | What runs under `commit_gate.write()` |
|------|--------------------------------------|
| `filesystem.rs:670-722` `gc_start_mode` | `gc_begin` (seal + join, `lib.rs:1493-1503`), `state.finish(...)` for each streaming clone (`:695`, hash + put), `commit_cut()` (`:703`: `content.sync()` fsync at `:1555`, `commit_cut_begin`, `commit_cut_finish`) |
| `filesystem.rs:1576-1583` `commit_cut_begin` | `flush_handle` for every open handle (`end_padded`: hash + put; `set_manifest`), and `begin_commit` clones six maps (O(files), `store.rs:701-708`) |
| `filesystem.rs:1602-1621` | `open_files` held across `self.meta.get_inode(ino)` (`:1616`) |

`timed_commit` already releases the gate before `commit_cut_finish` (`commit_timer.rs:44-60`); `gc_start_mode` could use the same begin/finish split. Recommended as a small follow-up inside plan 6 or 7, not required by criteria 1-6. With cells, these loops lock each cell (rank 4) after the exclusive gate (rank 1): ascending, and no holder of a cell can exist because every cell holder is a request that holds the gate shared.

### C. The 15 single-thread dependencies (research doc items, mapped to code)

`[VERIFIED: docs/research/multithreaded-request-handling.md "Migration" table; line numbers read this session]`

| # | Dependency | Where today | Closed by |
|---|-----------|-------------|-----------|
| 1 | `keep_budget` takes another file's state | `filesystem.rs:1243-1292` (take-out `:1267`, re-insert `:1274`) | Pattern 8 (atomics, cell protocol); flusher is Phase 20 |
| 2 | Concurrent write gets `EBADF` | `:1114-1116` (`remove` then `None => EBADF`); re-inserts at `:1138`, `:1158`, `:1222` | Cell stays in map |
| 3 | Stale read (state removed → manifest) | `test_read` `:1457-1509` | Cell lookup by ino |
| 4 | fsync is a no-op | `flush_handle` `:1720` (`None => return Ok(())`) | Cell lookup |
| 5 | Duplicate alias state / "last close wins" | `test_open` `:2456-2473`, `test_release` `:1326-1355` | One cell per inode |
| 6 | `release` skips `finalize_write` | `finalize_write` `:1364-1367` (`None => Ok`) | Last-handle finalize on the cell |
| 7 | `expect("state is in the map")` panic | `test_setattr_size` `:1847` | Cell lookup |
| 8 | Log order differs from apply order | `handlers.rs:107-126` (`Logged`), log calls at lines 175, 190, 205, 292, 308, 328, 351, 371, 392, 411, 436, 452, 468, 487, 512; `oplog.rs:254-278` (`got == *ino`) | Pattern 3 + `create_*_at` |
| 9-10 | Refcount and inode read-modify-write | `install_root` `:833-847`, `touch_dir` `:1034-1042`, `adjust_nlinks` `:1045-1049`, `stamp_content` `:1527-1538`, unlink `:2174-2187`, link `:2212-2216` | `modify_inode`; refcount updates use state read at publish time |
| 11 | Check-then-act namespace operations | `open_handle`/`close_handle`/`drop_last_link` `:576-618`; `simulate_*` `:2079-2383` | `NamespaceLocks` |
| 12 | `getlk`/`setlk` are no-ops | `handlers.rs:520-526` | Not closed (wrong with one thread too); out of scope |
| 13 | Per-write test counters | `write_rebase_bytes`/`write_base_bytes` stores at `:1108-1109`, adds at `:943-944`, `last_rebase_bytes` `:942`, `rebase_count` `:941` | Thread-local or test-only (CONTEXT) |
| 14 | `commit_cut` and GC invariants | `gc_start_mode` `:670-722`, `commit_cut_begin` `:1576-1686` | Keep; iterate cells under the exclusive gate |
| 15 | Unlogged write when the handle is gone | `handlers.rs:327` (`if let Some(ino) = fs.ino_of_fh(fh)`) | Cell gives `ino` inside the locked section; no silent skip |
| (16) | Budget undercount by (threads−1) × per-file cost | `keep_budget` sum `:1250-1256` excludes taken-out states | Atomics (Pattern 8) |

### D. Migration size (shim)

`grep` (patterns `.lock()/.read()/.write()` followed by `.unwrap()`, `.unwrap_or_else`, `.expect`, plus `lock_stats::lock(`), counted this session:

| Crate | `src/` incl. inline `#[cfg(test)]` | `tests/` |
|-------|-----------------------------------|----------|
| slicefs-cli | 102 (filesystem.rs ≈ 60: 22 `lock().unwrap()`-class + 36 `OpenFiles` + 2 `Readers`; fuse_callbacks.rs 33 gate sites) | 22 |
| metadata | 136 (store.rs ≈ 81 non-test + test) | 3 |
| chunk-store | 137 (60 in `mem.rs`, the in-memory backend) | 11 |

So CONTEXT's "about 59" is `filesystem.rs` only. Poison handling to delete: 37 `unwrap_or_else(|e| e.into_inner())` in slicefs-cli, 17 in chunk-store, 0 in metadata. `Condvar` appears in `chunk-store/lib.rs` (`MergeCtl.cv`), `pack_io.rs:92` (`SyncWait`) and `mem.rs`; those 8 sites are manual edits (`wait(&mut guard)` returns `()` in parking_lot, `wait_for` returns `WaitTimeoutResult`), the rest is `sed`-able. `arc_lock` (`Mutex::lock_arc`) is not needed by the stripe design (guards borrow from a local `Arc<FileCell>`); keep the feature only if a helper wants to return an owning guard from `lock_cell(ino)`. Features forward as `deadlock-detect = ["parking_lot/deadlock_detection"]` in `chunk-store`, `metadata`, `slicefs-cli`; cargo feature unification makes fuser's copy deadlock-aware too in that build.

## O_APPEND: what the kernel does and what the daemon must do (answer to the brief)

**Mount mode today.** `init` requests only `FUSE_ATOMIC_O_TRUNC` and `FUSE_BIG_WRITES` (`fuse_callbacks.rs:55`, `:58`); `open` replies `FopenFlags::empty()` (`fuse_callbacks.rs:183`; `create` likewise `:501`); mount options are `FSName`, `DefaultPermissions`, optional `NoAtime`/`AllowOther` and the macOS `backend=` option (`mount.rs:161-176`). So: no writeback cache, no `direct_io`, no `keep_cache`. `[VERIFIED: the files and lines above]`

**Linux.** `[CITED: raw.githubusercontent.com/torvalds/linux/master/fs/read_write.c]` `generic_write_checks_count` contains `if (iocb->ki_flags & IOCB_APPEND)` followed by `iocb->ki_pos = i_size_read(inode);`. `[CITED: .../fs/fuse/file.c]` `fuse_cache_write_iter` takes `inode_lock(inode)` before `generic_write_checks` and releases it at the end, without writeback cache the data goes through `fuse_perform_write`; `fuse_direct_write_iter` forces the exclusive inode lock when `IOCB_APPEND` is set (`fuse_dio_wr_exclusive_lock`). `fuse_write_flags` starts from `iocb->ki_filp->f_flags`, and `fuse_send_write*` store it in `write_in.flags`, so **the daemon receives `O_APPEND` in both OPEN and WRITE requests, but the offset in WRITE is the kernel's cached `i_size` at that moment**, computed under the kernel inode lock. The vendored fuser passes `write_in.flags` to the `write` callback as `flags: OpenFlags` (`vendor/fuser/src/ll/request.rs:~755`, `request.rs:249-261`); SliceFS ignores it (`_flags`, `fuse_callbacks.rs:450`). Both pages were read through a fetch summarizer from the `master` branch, so they are `[CITED]` and not `[VERIFIED]`; Phase 18 S4 ran on kernel 7.2.12 and observed that random writes to one inode keep at most 1 request waiting, consistent with the kernel serializing WRITEs of one inode in write-through mode `[VERIFIED: docs/research/multithreaded-request-handling.md "S4 result"]`.

**What this means.** On Linux libfuse-style mounts the kernel already serializes appends per inode and supplies consecutive offsets, so the daemon would be correct even if it used the kernel's offset. But (a) the daemon must not depend on it (the kernel `i_size` can be stale after an attribute change from another path, and the requirement says the EOF offset comes from the current size under the inode lock), and (b) macOS gives no guarantee: S1 shows FUSE-T 1.2.9 on FSKit overlaps requests (`max_inflight=2`, `reply_order_inversions=5`) `[VERIFIED: runs/s1-macos-fskit.log last line]`, and nothing in the repo records whether FSKit/FUSE-T passes `O_APPEND` or an offset for appends. `[ASSUMED]` that FUSE-T passes open flags (the repo logs `O_APPEND=` in `fuse_callbacks.rs:179` but no run output has it).

**Daemon rules (recommended).**
1. `Handle.append = (open or create flags & O_APPEND != 0)`. In `handle_write` also OR in `write_flags & O_APPEND` from the WRITE request (add `handle_write_flags(fs, fh, offset, data, flags)`; keep `handle_write` as a wrapper with flags 0 so ~100 existing call sites and tests do not change).
2. For an append handle, ignore the offset argument. Under the cell lock (Phase A of the write protocol): `eof` = `state.byte_count` if the state has bytes or is an overlay, else `meta.get_inode(ino).size` for a fresh stream on an existing file (a fresh cell starts with `byte_count = 0`, but the file may have committed content; `to_overlay` takes `size = base_len.max(byte_count).max(min_size)`, `filesystem.rs:873`). Check `eof + len <= MAX_FILE_SIZE` (EFBIG, as `test_write` does at `:1110`). Publish only if `gen` is unchanged; a concurrent extension bumps `gen`, the append retries with the new EOF; the 2nd attempt runs under the lock. This is what makes two appenders never overlap.
3. The log record carries the **effective** offset (`Op::Write { ino, offset, data }`, `oplog.rs:42`), so replay needs no append flag.
4. Return value stays `n`; the harness needs the placement, so add `test_write_at(fh, offset, data) -> (n, effective_offset)` (or a test-only out parameter) for the append test.
5. Cheap macOS probe (optional, macOS CI is allowed): in `examples/s1_probe.rs` or a CI step, open with `O_APPEND`, write twice, and log the `flags`/`offset` the callback receives; record the result as a follow-up note. Until then FSKit behavior is `[ASSUMED]` and the daemon rule above makes it irrelevant for correctness.

## Common Pitfalls

### Pitfall 1: Dropping a rebase result is not free of memory risk
**What goes wrong:** With the coarse generation rule, N writers keep invalidating each other's rebase; the overlay grows past the dirty limit without bound (violates #98).
**Why it happens:** the dirty cost only falls when a publish succeeds.
**How to avoid:** the hard cap (`limit + 2 × REBASE_STEP`) switches the writer to a step under its own cell lock. Test: 4 threads hammer one file with `dirty_limit` 256 KiB; assert `test_dirty_total()` stays below the cap.
**Warning signs:** `rebase_count` flat while writes continue; growing `overlay_cost`.

### Pitfall 2: Taken bytes invisible to readers (or restored over newer bytes)
**What goes wrong:** keeping `take()` semantics makes the extents vanish for readers during Phase B, and the error path (`:959-962`) re-puts the old bytes over later writes.
**How to avoid:** copy, publish with `discard`; never restore on failure (the live overlay never lost them).

### Pitfall 3: GC deletes what a lock-free read still uses
**What goes wrong:** PITFALLS 7 `[VERIFIED: PITFALLS.md:117-131]`: a reader that copied a `RangeView` (base key) and reads the base without a lock while a rebase installs a new root and a GC runs.
**Why it is safe here, and the rule that keeps it safe:** `gc_start_mode` takes the gate exclusively (`filesystem.rs:681`), so every request that started before it has finished; the mark starts from the roots at that instant (commit root, overlay bases, stream clones), and objects put after `gc_begin` are protected by `touched`. A later GC cycle again needs the exclusive gate. Therefore **every lock-free read and every Phase B must run inside the request's shared gate hold** (the callbacks do; `fuse_callbacks.rs` holds `_op` for the whole handler). Do not move reads or Phase B to background threads in Phase 19. Test (new, beside `chunk-store/src/race_tests.rs` style): loop read of a large file + rewrite + `gc_online`; reads must never return `Corrupt`/`NotFound`. The existing `online_gc_during_a_rebase_keeps_every_chunk` and `online_gc_inside_a_rebase_step_keeps_every_chunk` (`gc_online_tests.rs:215-345`) must pass unchanged.

### Pitfall 4: Cell identity at last release
**What goes wrong:** thread A finalizes and removes the cell while thread B (`open`) holds the old `Arc`; B then writes to a retired cell and the bytes are lost at the next open.
**How to avoid:** `retired` flag checked under the cell lock; `open` re-looks-up until it holds a live cell; remove from the map only while holding the cell lock (rank 4 → 5). PITFALLS 6a.

### Pitfall 5: Out-of-order publish of fsync/flush
**What goes wrong:** two `flush_handle`s snapshot at gen g1 and g2; the older one installs its manifest last (content regresses), and refcounts use the stale `old_root`.
**How to avoid:** under the cell lock at publish: skip if a newer snapshot already published (`last_published_gen`), read `last_committed_root` there, and do the refcount changes there. `cas_committed` is set at snapshot time today (`filesystem.rs:1717`); set it at publish only if `gen` still matches.

### Pitfall 6: Index/WAL lock order inversion when splitting the WAL
**What goes wrong:** `BufferedLog` uses `pending` → `writer` (`buffered.rs` comment "Lock order everywhere: pending, then writer"); a new `write_out` that takes `writer` → `pending` deadlocks against old code paths (`rotate`, `cut`, `install_root`).
**How to avoid:** choose `writer` → `queue` for everything, update all five call sites in the same plan, and test with the rank checker (both are rank 6; give them sub-keys in the unit test).

### Pitfall 7: `flush_inodes` outside the lock resurrects deleted inodes
See Pattern 7. Test: thread A flushes (hook parks it after the node write), thread B `delete_inode`s, A installs; the inode must stay deleted.

### Pitfall 8: Parking a thread inside a hook while holding the gate shared
Tests that park a rebase in Phase B hold the gate shared (as the callback does) and a commit thread would wait behind them, and with the std RwLock new readers then also wait. Deterministic tests must not run a commit thread while a thread is parked, or must park with the gate not held (the handlers do not take it; only callbacks and the harness `gated()` do).

### Pitfall 9: Create replay with reversed inode order
Two creates in different directories hold different stripes, so Create(ino 6) can be appended before Create(ino 5). Replay by logged inode number handles it; a replay that still calls `advance_next_ino` + `got == *ino` fails the mount (`oplog.rs:254-278`). Test with hand-built records in reverse order (no threads needed).

### Pitfall 10: Counters and test hooks that assume one writer
`write_rebase_bytes.store(0)` at the start of each write (`filesystem.rs:1108-1109`) and the `rebase_hook` mutex are racy with N writers; make them thread-local (the tests read them right after the same thread's write) or atomics that are only meaningful in single-thread tests. `last_rebase_bytes`, `rebase_count`, `last_relog_bytes` are read by many tests: keep them atomics, do not reset per write.

## Code Examples

All values below match the verbatim quotes in this document (`Site`, `WalEntry`, `Op::Write`, `Rank` from the CONTEXT decision). Illustrative, not final.

### Write step in the cell protocol (overlay path)
```rust
// Source: this research; shapes from filesystem.rs:1106-1224 and overlay.rs
fn write_overlay(&self, cell: &FileCell, off: u64, data: &[u8],
                 log: &mut dyn FnMut(u64 /*effective offset*/) -> Option<Lsn>) -> Result<(u32, Option<Lsn>), i32> {
    // apply to the overlay under the lock: a read sees all of this write or none
    let (lsn, over, snap) = {
        let mut st = cell.state.lock();                  // rank 4
        let off = if st.append { st.eof() } else { off };
        st.overlay_mut()?.write(off, data);               // memory only, bumps gen
        st.touch(now_pair());
        let lsn = log(off);                               // append under the same lock hold
        (lsn, st.cost() > self.dirty_limit(), st.begin_rebase_step(REBASE_STEP)) // copy_step + gen
    };
    // lock released; hash/put outside
    if let Some(snap) = snap { self.rebase_io_and_publish(cell, snap)?; }
    Ok((data.len() as u32, lsn))
}
// handler: after fs method returns (no apply lock held):  fs.meta().log_write_out(lsn);
```

### Replay by logged inode number
```rust
// Source: oplog.rs:239-300 today; replace the advance_next_ino + got == *ino pair
Op::Create { parent, name, mode, umask, uid, gid, flags, ino } => {
    match handlers::handle_create_at(fs, *parent, name, *mode, *umask, *uid, *gid, *flags, Some(*ino)) {
        handlers::CreateResult::Ok(_, fh, _) => { handles.insert(*ino, fh); }
        _ => return Err(bad("create")),
    }
}
```

### Debug rank check in the shim
```rust
// Source: this research (illustrative)
#[cfg(debug_assertions)]
fn push(rank: Rank, key: u64) {
    HELD.with(|h| {
        let mut h = h.borrow_mut();
        if let Some(&(top, tk)) = h.last() {
            let bad = rank < top || (rank == top && matches!(rank, Rank::Stripe | Rank::Cell) && key <= tk);
            assert!(!bad, "lock rank violation: {rank:?}/{key} taken while holding {top:?}/{tk}");
        }
        h.push((rank, key));
    });
}
```

### Deterministic "readers do not wait for writer I/O" test
```rust
// Source: this research; uses the existing test_set_rebase_hook (filesystem.rs:346)
let (parked_tx, parked_rx) = std::sync::mpsc::channel();
let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
fs.test_set_rebase_hook(Some(Arc::new(move || { parked_tx.send(()).ok(); let _ = go_rx.recv_timeout(Duration::from_secs(10)); })));
std::thread::scope(|s| {
    let w = s.spawn(|| fs.test_write(fh, 0, &big));          // goes over the limit, parks inside Phase B
    parked_rx.recv_timeout(Duration::from_secs(10)).expect("rebase reached its I/O phase");
    let got = fs.test_read(ino, 0, 4096).unwrap();            // must return while the rebase is parked
    assert_eq!(got, expected);
    fs.test_write(other_fh, 8192, &patch).unwrap();           // a write must also pass (result will be dropped)
    go_tx.send(()).unwrap();
    w.join().unwrap().unwrap();
});
```

## State of the Art

| Old Approach | Current Approach | When Changed | Impact |
|--------------|------------------|--------------|--------|
| `OpenFileState` removed from `open_files` while in use | `Arc<FileCell>` per inode, state stays in place | This phase | Removes EBADF, stale reads, no-op fsync, alias state |
| Apply then log in a separate critical section | Append (ticket) under the apply locks, write-out after | This phase (owner decision) | Replay equals apply; creates by logged inode |
| `std::sync` locks with poison handling | `parking_lot` 0.12 through a shim | This phase | No poison; deadlock watchdog; rank/timing hooks |
| `begin_commit` under the gate with a clone of six maps | Unchanged (Phase 20+/#19) | — | Note only |

**Deprecated/outdated in the planning docs:** research-doc step 1a (pack fsync off the store lock) and the generation switch landed in #131/#133; the tails left in `ChunkStore::inner` are listed in Inventory A rows 12-15. The `docs/research` "inode locks in increasing inode number" rank row is superseded by stripes (CONTEXT).

## Plan Ordering (answer to "ordering of work")

Each step must keep `cargo test --workspace` green and the harness `short_*` tests passing.

| Plan | Content | Depends on | Notes |
|------|---------|------------|-------|
| 19-01 | `chunk_store::sync` shim, `parking_lot` swap (sed `.lock().unwrap()`, 8 manual Condvar sites), `lock_stats` over the shim, `Rank` + checker core + unit tests, `deadlock-detect` feature, extend `Site` | — | Mechanical, no behavior change. Keeps `Arc<Mutex<StoreIo>>` and the gate on std. Touches everything, so it goes first and alone. |
| 19-02 | Metadata hot path: `modify_inode`, sharded `Live`, cold loads outside shard locks, no `io` lock on hot path, flush-install checks, `create_inode_at`/`create_directory_at`, trait change, `create_directory` allocation order | 19-01 | `store.rs`, `store_io.rs`, `slicefs-traits` only |
| 19-03 | WAL `append`/`write_out`/`Lsn`, fsync outside the writer mutex, `log_append`/`log_write_out` on the store | 19-01 | `wal/*`, `store.rs` log functions; parallel with 19-02 if the `store.rs` regions are disjoint (they are: log functions vs node caches); otherwise sequence |
| 19-04 | Chunk-store `inner` tails: pack create, seal join, `flush_index`, KEEP_CAP wait moved before the lock, trust-floor write; slow-`PackIo` test | 19-01 | `chunk-store/lib.rs`, `retry.rs`; spike first (below) |
| 19-05 | `FileCell` table, handle table with `write`/`append`, per-inode `Arc<Mutex<Reader>>`, all take-out sites converted, `keep_budget` adapted (Pattern 8), counters thread-local, `gc_start_mode`/`commit_cut_begin` iterate cells. Pessimistic: cell lock held across I/O. | 19-02 | After this, `serialize_off_target` one-file mode passes on data (no EBADF/stale) |
| 19-06 | Optimistic protocol (Pattern 1): `Overlay::copy_step`/`discard`/`gen`, stream clone push, `to_overlay`, `flush_handle`, `finalize`, hard cap, hook placement; `O_APPEND`; write/flush/release/truncate/setattr(size) logged inside the cell lock (log closure) | 19-05, 19-03 | Criterion 1 (readers vs writer I/O) and criterion 6 start passing |
| 19-07 | `NamespaceLocks`, rename mutex, `lock_namespace` helper, open/close/orphan atomicity, setattr/xattr stripes, namespace ops log under stripes, replay by logged inode, rank wiring incl. gate helper | 19-02, 19-05, 19-06 | **`serialize_off_target` passes (both modes) here**; criteria 4 and 5 |
| 19-08 | Harness: multi-handle POSIX modes, crash-replay with N threads, deterministic lock-free tests, lock-stats label update, un-ignore `serialize_off_target` (shortened), research doc note, #21 body, ROADMAP/REQUIREMENTS wording on rank order | 19-07 | Validation plan |

**Which plan makes `serialize_off_target` pass:** 19-07 (data-correct from 19-05/19-06; namespace races in many-files mode, notably the open-vs-unlink orphan race and `shared`-directory metadata read-modify-write, need `modify_inode` from 19-02 and stripes). Use a temporary split test or env switch for intermediate plans; remove the `#[ignore]` in 19-07/19-08.

**Spike before 19-04 (reversible, decide and report):** one afternoon: wrap `FileIo` in a `SlowIo` (sleep 100 ms in `PackFile::sync`, `create`, `write_durable`, index write) via `ChunkStore::open_with(io, opts)` (`lib.rs:1159`), run 2 threads of `put`+`read`, and record which of the rows 12-15 hold the lock for the delay. Decide the minimal fix per row (move the KEEP_CAP wait before the lock by exposing the current background-sync wait handle under a short lock; take `sealing` out under the lock and join outside it; write the index run with the memtable swapped out). Ruling for the pack `pwrite`: **out** (documented exception). Reason: `PackFile::write_at` takes `&mut self` and the record is appended at `a.len` under the lock; writing outside the lock needs offset reservation and creates holes in the pack on a crash between reservation and write, which the pack scan (`covers`, `chunk-store/src/lib.rs:2686`) assumes cannot happen. S3 measured `store` hold p50 0.6-1.2 µs. The deterministic test asserts the slow operations (fsync, create, index write, KEEP_CAP wait) do not block another thread's `put`/`read`.

## Risks

1. **Deadlock between gate, cell, stripes.** Rank order `gate → rename → stripe → cell → short maps → store`; no code path re-takes the gate; the exclusive-gate holders (`commit_cut_begin`, `gc_start_mode`) lock cells one at a time and never nest two. A cell holder always holds the gate shared, so the exclusive holder never waits on a cell. A pending exclusive acquire blocks new shared acquires (S5), so a request must never block on a thread that needs a *new* shared gate hold.
2. **`gc_start_mode`/`commit_cut_begin` iterate cells under the exclusive gate.** Safe by the rule above. They also call into `flush_handle` (hash + put) under the exclusive gate (Inventory B): not a regression, but the harness `commit_hold` row should be read with that in mind; the 250 ms commit thread of the harness exercises this on every run.
3. **`keep_budget` stays until Phase 20.** Tests must stay below the dirty budget (CONTEXT; harness asserts `dirty_total < DIRTY_BUDGET`, `stress_harness.rs:1175`). The adapted form must not nest cells.
4. **Generation starvation.** See Pitfall 1; hard cap required.
5. **WAL order inversions in the split.** See Pitfall 6.
6. **The log closure design touches every handler.** 15 log sites (`handlers.rs`) change shape; `replay` runs the same handlers with `replay_guard` logging off (`filesystem.rs:358-368`), so the closure must be a no-op there (`log_record` already checks `replaying`, `:358-362`).
7. **Test-time budget.** Un-ignoring `serialize_off_target` adds two 5 s timed runs (`stress_harness.rs:1166-1170`) to the default suite; shorten to 2 s per mode (about 4 s) and keep `watchdog`; the quick harness binary is 3.03 s today (`cargo test -p slicefs-cli --test stress_harness`: 3 passed, 6 ignored, measured this session).

## Assumptions Log

| # | Claim | Section | Risk if Wrong |
|---|-------|---------|---------------|
| A1 | FUSE-T/FSKit passes `O_APPEND` in OPEN/WRITE flags, and may compute append offsets itself | O_APPEND | None for correctness (daemon computes EOF itself), but the per-handle flag may be false on macOS: appends would then be plain writes at the offset FSKit sends |
| A2 | `parking_lot` 0.13 needs a newer toolchain than 0.12 and would add a second copy beside fuser's 0.12 | Standard Stack | Low: the decision (0.12) is locked by the user anyway |
| A3 | Keeping `Arc<Mutex<StoreIo>>` and the gate as std types is acceptable to the owner (CONTEXT says "replace std locks" in the hot crates) | Standard Stack | Medium: if the owner wants them changed, plan 19-01 grows by about 115 constructor sites and the gate API (~10 test sites) |
| A4 | A page-cache `pwrite` of one record under `ChunkStore::inner` does not count as "disk I/O" for LOCK-01 | Pattern/Plan 19-04 | Medium: if it does, the pack write needs offset reservation and crash-hole handling in the pack scan (one-way-ish change); ask the owner |
| A5 | 256 stripes, 16-64 cell/Live shards, hard cap `limit + 2 × REBASE_STEP`, `FINAL_STEP` 4-8 MiB are good values | Patterns | Low (tunable, Claude's discretion) |
| A6 | Logging `Release`/`Flush` only when they had an effect keeps replay faithful with several handles | Pattern 3 | Low: add the two-handle replay test to confirm |
| A7 | A coarse "any change → drop" generation rule does not starve under the harness load (4 threads, owned stripes) | Pattern 1 | Low-medium: the hard cap bounds memory; throughput may drop under heavy same-file writes until Phase 20 |
| A8 | The kernel read of `fuse_cache_write_iter`/`generic_write_checks_count` on `master` matches the 7.2.12 kernel of the S4 run | O_APPEND | Low: `IOCB_APPEND → ki_pos = i_size` is long-standing; the daemon rule does not depend on it |

## Open Questions

1. **Does "store lock" in LOCK-01 include the page-cache `pwrite` under `ChunkStore::inner`?**
   - What we know: S3 `store` hold p50 is 0.6-1.2 µs; the long holds are seal join, index flush, pack create, KEEP_CAP wait. Writing outside the lock needs `PackFile::write_at(&self)` and hole-safe crash handling.
   - Recommendation: rule "out" (A4); record it in the plan; owner can overrule before 19-04 is executed.
2. **Rank wording.** LOCK-03 and ROADMAP criterion 4 say "two inodes in increasing inode order"; the decision and this design use ascending stripe order (a different total order).
   - Recommendation: add a doc task in 19-08 that edits the requirement/criterion text to "ascending stripe order"; the verifier will otherwise read the old wording literally. The owner approved the stripe decision, but not the wording change.
3. **Should the gate be modeled in the rank checker (helper methods in 33 callback sites) or documented only?** Recommendation: add `gate_shared()`/`gate_exclusive()` helpers; if the diff is too noisy, document and test ranks 2-6 only.
4. **Does FSKit/FUSE-T send `O_APPEND`?** Optional macOS CI probe (see O_APPEND, rule 5).
5. **`inode_map` vs `inode_data` redundancy** (ARCHITECTURE.md open question): not needed for Phase 19; both are cloned in `begin_commit`. Leave.
6. **Add the ancestor check to cross-directory rename?** It is a behavior change; recommend a follow-up issue, not this phase.

## Environment Availability

| Dependency | Required By | Available | Version | Fallback |
|------------|------------|-----------|---------|----------|
| Rust toolchain | all | ✓ | `rustc 1.95.0-nightly (9e79395f9 2026-02-10)`, `cargo 1.95.0-nightly` (`export PATH="$HOME/.cargo/bin:$PATH"`) | — |
| `parking_lot` 0.12.5 source | shim | ✓ (in `~/.cargo/registry/src`, already in `Cargo.lock`) | 0.12.5 | — |
| `gh` CLI | #21 body update (`gh issue edit 21 --body-file`) | ✓ (`gh issue view 21` worked) | — | Edit in the web UI |
| `gsd-tools.cjs` | package legitimacy | ✓ | — | — |
| Docker/OrbStack Linux | optional pjdfstest/mount checks | not probed (not required: no mounts in this phase) | — | CI |
| macOS CI | optional O_APPEND probe | CI only (no local mounts) | — | skip, A1 stays assumed |

**Missing dependencies with no fallback:** none.

## Validation Architecture

### Test Framework
| Property | Value |
|----------|-------|
| Framework | `cargo test` (Rust 2024 edition crates); integration tests in `crates/*/tests/`; optional `lock-stats` feature (own test binary because the recorder is process-global); `hdrhistogram` for rows |
| Config file | none; features in the crate `Cargo.toml` files (`lock-stats`, new `deadlock-detect`) |
| Quick run command | `export PATH="$HOME/.cargo/bin:$PATH"; cargo test -p slicefs-cli --test stress_harness` (3.03 s after build; 3 passed, 6 ignored; measured this session) |
| Full suite command | `cargo test --workspace && cargo check --workspace --all-targets && cargo check --workspace --all-targets --features slicefs-cli/lock-stats && cargo check -p slicefs-cli --features deadlock-detect` |

Default `cargo test --workspace` compiles neither `lock-stats` nor `deadlock-detect`; criteria 1 and 3 have report evidence only with the feature on, so the feature runs below are part of the phase gate, and the deterministic tests (default features) are the pass/fail evidence.

### Phase Requirements → Test Map
| Req ID | Behavior | Test Type | Automated Command | File Exists? |
|--------|----------|-----------|-------------------|-------------|
| LOCK-01 | Reader and other-file writer complete while a rebase is parked in Phase B | integration (deterministic) | `cargo test -p slicefs-cli --test lock_free_io` | ❌ Wave 0 |
| LOCK-01 | WAL `append` returns while `write_out`/`flush_and_sync` is parked | unit (metadata) | `cargo test -p metadata --lib wal::` | ❌ Wave 0 |
| LOCK-01 | Cold directory load on one dir does not block another dir/inode | unit (metadata, slow `PackIo`) | `cargo test -p metadata --lib store::` | ❌ Wave 0 |
| LOCK-01 | Slow fsync/create/index write/KEEP_CAP wait do not block another `put`/`read` | unit (chunk-store, slow `PackIo`) | `cargo test -p chunk-store --lib race_tests` | ❌ Wave 0 |
| LOCK-01 | `flush_inodes` install does not resurrect a deleted inode | unit (metadata) | `cargo test -p metadata --lib store::` | ❌ Wave 0 |
| LOCK-01 | Hold-time rows for readers/wal/dir_live/inode_live/cell in a real run (report) | feature run | `cargo test -p slicefs-cli --features lock-stats --test stress_harness short_ -- --nocapture` | ✅ (labels to update) |
| LOCK-01 | All lock labels record | feature run | `cargo test -p slicefs-cli --features lock-stats --test lock_stats_report` | ✅ (hard-coded labels change) |
| LOCK-03 | Rank violations panic in debug (cell→stripe, descending stripes, two cells) | unit | `cargo test -p chunk-store --lib sync::` | ❌ Wave 0 |
| LOCK-03 | Exclusive-gate holder may lock every cell; harness runs with the checker on | integration | `cargo test -p slicefs-cli --test stress_harness serialize_off_target` | ✅ (un-ignore) |
| LOCK-03 | No deadlock with `deadlock_detection` | feature run | `cargo test -p slicefs-cli --features deadlock-detect --test stress_harness serialize_off_target` | ❌ Wave 0 (watchdog thread) |
| LOCK-04 | Replay equals apply after an N-thread mixed run (crash, no commit) | integration | `cargo test -p slicefs-cli --test crash_replay_threads` | ❌ Wave 0 |
| LOCK-04 | Replay of Create records in reverse inode order | unit/integration | `cargo test -p slicefs-cli --test write_log_replay_tests` (new case) | ✅ file, ❌ case |
| LOCK-04 | `create_inode_at` / `create_directory_at` (exists-error, `next_ino` raise) | unit (metadata) | `cargo test -p metadata --lib store::` | ❌ Wave 0 |
| LOCK-04 | Two write handles, release of the first, replay equal | integration | `cargo test -p slicefs-cli --test write_log_replay_tests` (new case) | ❌ case |
| LOCK-04 | #21 body matches the rule | doc check | `gh issue view 21 --json body -q .body \| grep -c "same locks"` ≥ 1 and no "Append the record to the log (the slow part, no inode lock)" | manual/automated |
| LOCK-05 | Writers on the same range with distinct patterns; readers see exactly one pattern (all-or-none), also across rebases (`dirty_limit` 256 KiB) | integration | `cargo test -p slicefs-cli --test multi_handle_posix all_or_none` | ❌ Wave 0 |
| LOCK-05 | A write completed through one handle is visible via another (RO, RW, append) | integration | `... multi_handle_posix visibility` | ❌ Wave 0 |
| LOCK-05 | K append handles × N appends: size = K·N·rec, every record once, none torn; bogus offsets ignored; log carries effective offsets | integration | `... multi_handle_posix appends` | ❌ Wave 0 |
| LOCK-05 | Parallel I/O on different files: a thread parked in file X's I/O phase does not delay file Y | integration | `cargo test -p slicefs-cli --test lock_free_io many_files` | ❌ Wave 0 |
| LOCK-05 / SC | `serialize_off_target` passes, not ignored, both modes | integration | `cargo test -p slicefs-cli --test stress_harness serialize_off_target` | ✅ (un-ignore, 2 s per mode) |
| LOCK-05 | Read + rewrite + online GC never returns `Corrupt` | integration | `cargo test -p slicefs-cli --test gc_online_tests` (new case) | ✅ file, ❌ case |

### Sampling Rate
- **Per task commit:** the crate-local quick test of the touched area (`cargo test -p chunk-store --lib sync::`, `cargo test -p metadata --lib`, or `cargo test -p slicefs-cli --test stress_harness`), plus `cargo check -p slicefs-cli --features lock-stats` after any lock-site change.
- **Per plan merge:** `cargo test --workspace`, then `cargo test -p slicefs-cli --features lock-stats --test lock_stats_report` and `--test stress_harness short_ -- --nocapture` (read the report rows).
- **Phase gate:** full suite above green, plus `--features deadlock-detect` run of `serialize_off_target`, plus the macOS CI job if the branch triggers it (no local mounts).

### Wave 0 Gaps
- [ ] `crates/slicefs-cli/tests/common/mod.rs` — lift `dump`, `reload_and_replay` from `write_log_replay_tests.rs` (they are file-private today)
- [ ] `crates/slicefs-cli/tests/lock_free_io.rs` — deterministic park-in-I/O tests (needs a hook for the streaming push, e.g. `test_set_push_hook`, beside `test_set_rebase_hook`)
- [ ] `crates/slicefs-cli/tests/multi_handle_posix.rs` — POSIX 2.9.7 cases, with `test_write_at` returning the effective offset
- [ ] `crates/slicefs-cli/tests/crash_replay_threads.rs` — N-thread mixed run, crash, replay, compare with `dump` (include `ino=`, mtime/ctime in the dump as `write_log_replay_tests.rs` does)
- [ ] slow-`PackIo` wrapper in `chunk-store` tests and metadata tests (`ChunkStore::open_with`, `lib.rs:1159`)
- [ ] `sync.rs` rank unit tests; `Overlay` generation property test
- [ ] Update `lock_stats_report.rs` label list; `Site` array sizes
- [ ] `[features] deadlock-detect` and a watchdog thread in the stress binary (calls `parking_lot::deadlock::check_deadlock()` every second; prints threads/backtraces and aborts)
- [ ] Framework install: none (cargo only)

Test sizes: keep each new test under about 3 s in debug (4 threads, 100-200 ops, 64 KiB ranges); never allocate whole-file buffers (the harness derives blocks from `(seed, file, block, version)`; reuse `block_bytes`).

## Security Domain

`security_enforcement` is absent in `.planning/config.json` (treated as enabled). This phase is internal concurrency in a local daemon: no authentication, sessions or network input.

### Applicable ASVS Categories

| ASVS Category | Applies | Standard Control |
|---------------|---------|-----------------|
| V2 Authentication | no | — (FUSE `DefaultPermissions`, kernel checks) |
| V3 Session Management | no | — |
| V4 Access Control | no (unchanged) | kernel `default_permissions`; no new access paths |
| V5 Input Validation | yes | Bounds on kernel-supplied values: `offset + len` overflow and `MAX_FILE_SIZE` (EFBIG) for append EOF (`checked_add`, as `filesystem.rs:1110`); names `check_name`; no allocation by offset or size (#98) |
| V6 Cryptography | no | BLAKE3 unchanged; do not touch hashing |

### Known Threat Patterns

| Pattern | STRIDE | Standard Mitigation |
|---------|--------|---------------------|
| Lock starvation / livelock of one file by a hostile writer (resource exhaustion) | Denial of service | Hard cap on overlay RAM with fallback to a step under the file's own lock; waits with time limits arrive in Phase 20 |
| Unbounded memory from dropped rebase results or the WAL queue | Denial of service | Copy bounded by `REBASE_STEP`; WAL queue bounded by in-flight requests × max write (write-out before reply) |
| TOCTOU between lookup and lock (rename/unlink races leading to wrong-inode operations) | Tampering | Revalidate after locking (Pattern 4 rule 2) |
| Log record order inversion leading to replay failure (mount refuses to start) | Denial of service | Append under apply locks; replay by logged inode |
| Deadlock from lock-order inversion | Denial of service | Rank checker (debug), `deadlock_detection` watchdog (stress), single documented order |

## Sources

### Primary (HIGH confidence)
- Repository code read this session: `crates/slicefs-cli/src/{filesystem,handlers,oplog,overlay,content,commit_timer,fuse_callbacks,mount}.rs`; `crates/metadata/src/{store,store_io}.rs`, `crates/metadata/src/wal/{mod,per_op,buffered,periodic,flush_on_fsync}.rs`, `crates/metadata/src/segment/writer.rs`; `crates/chunk-store/src/{lib,retry,lock_stats,file}.rs`; `crates/slicefs-traits/src/metadata.rs`; `crates/slicefs-cli/tests/{stress_harness,lock_stats_report,write_log_replay_tests,gc_online_tests}.rs`; `vendor/fuser/src/{request,ll/request,session}.rs`
- Planning docs: `19-CONTEXT.md`, `REQUIREMENTS.md`, `ROADMAP.md` (Phase 19), `STATE.md`, `research/{ARCHITECTURE,STACK,PITFALLS,SUMMARY}.md`, `docs/research/multithreaded-request-handling.md` (design, S3, S4, S5 results), `18-02-SUMMARY.md`, `18-VALIDATION.md`
- `parking_lot` 0.12.5 source in `~/.cargo/registry/src/` (Cargo.toml features, `lib.rs`, `raw_mutex.rs`, `raw_rwlock.rs`, `condvar.rs`)
- Measured this session: `cargo test -p slicefs-cli --test stress_harness` (3 passed, 6 ignored, 3.03 s); `serialize_off_target -- --ignored` fails with `errno 9` and "bytes match no version", `max_inflight=4`
- `gsd-tools package-legitimacy check --ecosystem crates parking_lot lock_api` → both OK

### Secondary (MEDIUM confidence)
- https://docs.rs/parking_lot/0.12.5/parking_lot/deadlock/index.html — `deadlock_detection` feature, `check_deadlock()` usage (the page does not list which lock types are covered; coverage taken from the source above)
- `gh issue view 21` — current #21 body (to be updated)

### Tertiary (LOW confidence, flagged)
- https://raw.githubusercontent.com/torvalds/linux/master/fs/fuse/file.c and `fs/read_write.c` and `Documentation/filesystems/fuse/fuse.rst` — read through a page summarizer from `master`, not the 7.2.12 tree of the S4 run; `fuse.rst` says nothing about `O_APPEND`
- FUSE-T/FSKit `O_APPEND` behavior: no source (A1)

## Metadata

**Confidence breakdown:**
- Standard stack: HIGH — one crate, already in the lock file, source read
- Architecture/protocols: MEDIUM — derived from code and the locked decisions; the generation protocol and WAL split are new designs, validated only by reasoning and by the existing hook tests that they must keep passing
- Lock inventory: HIGH — each row read at the cited lines
- O_APPEND kernel behavior: MEDIUM (Linux, cited) / LOW (macOS)
- Pitfalls: MEDIUM-HIGH — three of ten come directly from PITFALLS.md and the code

**Research date:** 2026-11-21
**Valid until:** 2026-12-21 (code changes quickly on this branch; re-read `filesystem.rs` line numbers if other branches merge first)
