# Architecture Research

**Domain:** Deduplicating FUSE file system (Rust) - milestone v2.2 "Concurrency & Store Evolution"
**Researched:** 2026-11-20
**Confidence:** HIGH for what the code does today (read directly); MEDIUM for the new design choices; LOW for macOS FUSE-T/FSKit overlap (no documentation exists, spike S1 decides)

This file replaces the v2.0 version. It covers only what v2.2 adds: #127 spikes, #21 fine-grained locks, #22 worker threads, #62 `slicefs rehash`, #67 FastCDC profiles, #19 prolly-tree evaluation. It maps `docs/research/multithreaded-request-handling.md` (written against commit 6fd1e11) onto the code as it is now. It does not repeat that research. All paths are relative to the repository root. Line numbers are left out on purpose; they move. Functions are named instead.

## Where the threading research is already out of date

The research doc lists 8 migration steps. Two things landed after it was written. Give them a status before planning:

| Research step | Status now | Evidence in code |
|---|---|---|
| 1a. Pack fsync and seal off the store lock | **Landed** (PR #131, #108) | `crates/chunk-store/src/retry.rs`, `crates/chunk-store/src/lib.rs` (`ChunkStore::put`, `sync`) |
| Gate generation switch (the "later" part of "Commit gate") | **Landed** (PR #133, #108) | `crates/slicefs-cli/src/commit_timer.rs::timed_commit` calls `commit_cut_begin` under the gate and `commit_cut_finish` after it; `crates/metadata/src/store.rs::begin_commit` / `finish_commit` |
| 1b. `readers` lock held across `Reader::read` | **Open** | `crates/slicefs-cli/src/filesystem.rs::read_root` |
| 1c. WAL fsync under the `writer` mutex | **Open** | `crates/metadata/src/wal/per_op.rs::flush_and_sync` locks `writer` and syncs; `log_mutation` takes the same mutex. `buffered.rs` (FlushOnFsync, Periodic) is the same shape. |
| 1d. `Mutex<StoreIo>` held across node reads and puts | **Open** | `crates/metadata/src/store.rs`: `with_dir`, `load_meta`, `flush_inodes`, `flush_dirs`, `set_manifest`, `get_manifest`, the xattr methods. Lock-free `NodeReader` / `NodeWriter` already exist in `crates/metadata/src/store_io.rs`. |
| 1e. Global `dir_live` / `inode_live` caches | **Open** | `store.rs`: one `Mutex<Live<_>>` each; `with_dir` even loads a cold `DirList` from disk while holding `dir_live`. |
| 0. Stress harness | **Open** | No test runs two request threads. `scripts/docker-linux-test/mt-linux.sh` is a mount smoke test ("mt" is "mount"), not a threading test. `crates/chunk-store/src/race_tests.rs` covers the chunk store only. |
| 2-7 | **Open** | `open_files: Mutex<HashMap<fh, OpenFileState>>`, take-out pattern, `keep_budget` taking another file's state, `handle_write` logging after the apply, no flusher, no pool. |

Consequences for the spikes: S2 (inline pack fsync) is now a re-measure of code that already shipped, not a new experiment. S5 (gate fairness) now decides only the lock type, because the exclusive hold is already just the cut. One thing the research did not note: `begin_commit` clones six maps (`inode_map`, `inode_data`, `dir_data`, `manifest_data`, `xattr_data`, `refcounts`) while the gate is held exclusively. That is O(files) and is part of S5's measurement (`commit_timer` already logs `hold_ms`). It also links to #19 (a persistent map makes this clone O(1)).

## Standard Architecture

### System overview: today and after v2.2

```
 FUSE kernel / FUSE-T socket
        |
        v
 fuser reader thread  (vendor/fuser; one on both platforms, n_threads stays 1)
        |   callback in crates/slicefs-cli/src/fuse_callbacks.rs (MountedFs)
        |   TODAY: callback takes commit gate (shared), runs handler, replies, all inline
        |   v2.2 : callback copies args, moves Reply into Dispatcher            [NEW dispatch.rs]
        v
 +-------------------------------+      +--------------------------------------+
 | Dispatcher (bounded queue,    |      | background threads                   |
 |  bytes-bounded for WRITE)     |      |  commit timer   (exists)             |
 |  N workers, N=1 => inline     |      |  online GC      (exists)             |
 +-------------------------------+      |  rebase flusher [NEW flusher.rs]     |
        | worker: gate.read()           |  WAL leader/follower sync [A7]       |
        v                               +--------------------------------------+
 handlers.rs  (handle_*: apply + log record, now INSIDE the locks)
        |
        v
 filesystem.rs  SliceFsFilesystem
   gate(RwLock) > rename mutex > NamespaceLocks(stripes) > FileCell mutex > short maps
        |                                   |
        v                                   v
 crates/metadata  DictMetadataStore      crates/chunk-store  ChunkStore
   sharded Live caches, NodeReader/Writer    FileWriter / splice / FileReader
   WAL (segments/), root records             packs, on-disk index, GC
        |                                   |
        +--------------- <store>/ -----------+
          FORMAT (hash line, + chunking line in v2.2)   segments/   packs/
```

### Component responsibilities (existing and new)

| Component | Status | Responsibility | File |
|---|---|---|---|
| `MountedFs` callbacks | modify | Thin FUSE shell. Today every callback repeats `commit_gate().read()` and an ENTRY/EXIT `eprintln!`. | `crates/slicefs-cli/src/fuse_callbacks.rs` |
| `Dispatcher` | **new** | Reader-to-worker handoff, byte-bounded queue, N workers, drain before `destroy()`. | `crates/slicefs-cli/src/dispatch.rs` |
| `FileCell` table | **new** (replaces `open_files` keyed by fh) | One `Arc<FileCell>` per open inode; `Mutex<OpenFileState>` plus an `AtomicUsize` dirty cost. Alias handles share the Arc. | `filesystem.rs` (or `file_cells.rs`) |
| `NamespaceLocks` | **new** | Striped lock array for multi-step namespace operations; ascending stripe order; debug-build rank checker. | `crates/slicefs-cli/src/locks.rs` |
| Rebase flusher | **new** | Thread that runs `rebase_step` on the dirtiest file when dirty bytes pass a soft threshold. | `crates/slicefs-cli/src/flusher.rs` |
| `DictMetadataStore` | modify | Drop `Mutex<StoreIo>` from the hot path, shard `Live`, add atomic `modify_inode`. | `crates/metadata/src/store.rs` |
| WAL | modify | Sync outside the append mutex; leader/follower group commit with `durable_lsn`. | `crates/metadata/src/wal/per_op.rs`, `buffered.rs`, `segment/writer.rs` |
| `oplog` replay | modify | Create by logged inode number. | `crates/slicefs-cli/src/oplog.rs`, `MetadataStore` trait in `crates/slicefs-traits/src/metadata.rs` |
| `ChunkParams` | **new** | FastCDC min/avg/max as a value, not constants. | `crates/chunk-store/src/lib.rs`, `file.rs` |
| Rehash engine | **new** | Rewrite every object and reference to the store's new write hash. | `crates/chunk-store/src/file.rs` (entry copy), `crates/metadata/src/rehash.rs`, `crates/slicefs-cli/src/rehash.rs` |
| Prolly map prototype | **new, not wired in** | A key-ordered, content-cut Merkle map over `NodeRead` / `NodeWrite`. | `crates/prolly/` (new crate) + `benchmarks/prolly-bench/` |

### Recommended project structure (additions only)

```
crates/
  slicefs-cli/src/
    dispatch.rs        # Dispatcher, worker loop, byte-bounded queue         (#22)
    locks.rs           # NamespaceLocks, rank checker (debug builds)          (#21)
    flusher.rs         # rebase flusher + soft/hard dirty thresholds          (#21)
    rehash.rs          # `slicefs rehash` command, preflight, progress        (#62)
  slicefs-cli/tests/
    concurrency_tests.rs   # N-thread in-process harness over handlers        (#127 step 0)
    rehash_tests.rs        # mixed-hash store -> one hash, snapshots, scrub   (#62)
  metadata/src/
    rehash.rs          # per-node-kind key rewriting for the root's graph     (#62)
  chunk-store/src/
    lib.rs, file.rs    # ChunkParams plumbing (#67); chunk-entry copy (#62)
  prolly/              # prototype crate, depends only on NodeRead/NodeWrite   (#19)
benchmarks/
  prolly-bench/        # commit cost vs full re-serialization                  (#19)
  chunk-bench/         # exists; already takes fastcdc:MIN:AVG:MAX              (#67)
```

Rationale: every new concurrency piece lives next to `filesystem.rs` in `slicefs-cli` because that is where the take-out pattern and the gate are. The rehash walker lives in `metadata` because it must know the wire format of each node kind, which is private to `store.rs`, `directory.rs`, `manifest.rs` and `inode_map.rs`. The prolly prototype is its own crate so it cannot leak into `root_rec`.

## Part 1: Multithreaded request handling (#127, #21, #22)

### What each of the 15 single-thread dependencies becomes in code

| Today (function in `filesystem.rs` unless noted) | Change |
|---|---|
| `test_write`, `settle_pending`, `finalize_write`, `flush_handle`, `test_setattr_size`: `open_files.lock().remove(&fh)` then re-insert | Look up the `Arc<FileCell>`, lock `cell.state`, work in place. The state never leaves the map, so a second thread waits instead of seeing EBADF, a stale manifest read or a no-op fsync. |
| `keep_budget`, `free_dirty`: take another file's state out of `open_files` | Delete the cross-file path. Replace with: each `FileCell` mirrors its dirty cost in `AtomicUsize`; a global `AtomicUsize` total; the writer only steps its own file inline as a fallback; the flusher steps the largest file. |
| `write_alias: Mutex<HashMap<fh, fh>>`, `owner_fh`, the heir logic in `test_release` | Each handle maps to the same `Arc<FileCell>`. `owner_fh` and the heir swap disappear. Release drops the Arc; the last drop runs `finalize_write`. |
| `test_read`: scans all `open_files` for `s.ino == ino` under the global lock, clones a `Stream` | O(1) lookup by inode in a sharded `ino -> Arc<FileCell>` map; hold `cell.state` only to copy the overlay view or clone the stream, then read with no lock. This is the "read while write" half of #21. |
| `write_handle_of`, `ino_of_fh`, `test_dirty_total` | Same lookups through the cell map. |
| `gc_start_mode`, `commit_cut_begin`: iterate `open_files` under the exclusive gate | Keep. The exclusive-gate holder may lock every `FileCell` in any order, because every worker holds the gate shared whenever it holds a cell. Rule to write down and test: no lock of rank 3 or lower is ever held outside a shared gate hold, except by the exclusive holder. |
| `handle_write` (`handlers.rs`): `test_write`, then `ino_of_fh`, then `log` | Log inside the cell lock (see step A3). `ino` comes from the cell. This also removes the silent "no log record" path when the fh is gone. |
| `install_root`, `stamp_content`, `touch_dir`, `adjust_nlinks`: read inode, modify, `update_inode` | Add `DictMetadataStore::modify_inode(ino, FnOnce(&mut InodeMeta))` which does the read-modify-write under the shard lock of `inode_live`. Single-inode lost updates are then closed in the metadata layer, with no filesystem-level lock. |
| `simulate_unlink/rmdir/link/rename/mkdir/symlink`, `test_create`, `drop_last_link`: check-then-act on `handles` / directory state | Take `NamespaceLocks` stripes for the parent directory (and child, and second parent for rename) in ascending stripe order. Put the "is there an open handle" check in `drop_last_link` and the `handles.insert` in `open_handle` under the same stripe of the inode. |
| `close_handle`: two separate `handles.lock()` calls | Do the remove and the "still open" check under one lock. (Not a bug today, but cheap to fix while the maps are touched.) |
| `read_root`: `readers` mutex held across `Reader::read` | `readers: Mutex<HashMap<ino, Arc<Mutex<Reader>>>>`, clone the Arc under the map lock, lock the per-inode reader only for the read. `Reader::read` takes `&mut self` (it keeps node and chunk caches), so a plain `Arc<Reader>` does not work. Longer term: per-worker readers. |
| `getlk` / `setlk` no-ops (item 12) | Not part of v2.2. Wrong with one thread too. |
| Per-write test counters (`write_rebase_bytes`, `write_base_bytes`, `last_rebase_bytes`): `store(0)` at the start of each write | Racy under N workers. Make them `thread_local!` or test-only behind a single-thread assertion. They are only read by tests. |
| `replaying: AtomicBool`, thread-local `oplog::Clock` | Keep as they are. Replay runs before the session starts, and `Clock::freeze_at` is per thread, which is correct as long as a request runs wholly on one worker (it does). |

### The lock rank to enforce (adapted from the research table)

| Rank | Lock | Rule |
|---|---|---|
| 1 | Commit gate (`commit_gate: Arc<RwLock<()>>`), shared | Taken by the worker, not the reader (a `std` read guard cannot cross threads). Never held while waiting for dirty budget or for durability. |
| 2 | Rename mutex | Cross-directory rename only; taken before any stripe. |
| 3 | `NamespaceLocks` stripe, ascending stripe index | Multi-inode operations lock the set of stripes sorted. Stripe order replaces "increasing inode number" and also handles two inodes that hash to one stripe. |
| 4 | `FileCell.state` | Taken after stripes. A write needs no stripe: metadata read-modify-write is atomic in the store (`modify_inode`). |
| 5 | Short maps (`handles`, `orphans`, cell map shards, `readers` map) | Never held across I/O. |
| 6 | `DictMetadataStore` internals, WAL, `ChunkStore` | Existing documented order (`inode_map`, `io`, `inode_data`, ...); `io` leaves the hot path. |

Deviation from #21 and the research doc, with the reason: #21 proposes a per-inode `RwLock` table and the research merges the `OpenFileState` mutex with the inode lock. This plan keeps two things apart. The `FileCell` mutex guards data of an *open* file. A stripe guards *namespace* consistency for inodes that may not be open at all (unlink of a closed file). Merging them would force a cell to exist for every inode touched by `create`/`unlink`. Striping avoids an unbounded lock table. This is a reversible choice and a candidate for the stress harness to confirm.

Add a debug-build rank checker (`locks.rs`, thread-local stack, `cfg(debug_assertions)`); every test run then checks the order for free.

### Data flow with the pool

```
reader thread: callback(args, Reply)
   copy args (WRITE: one payload copy; queue weight = len)  -> Dispatcher.submit(closure)   [blocks if queue full]
worker k:   gate.read() ----> handler body ----> reply
              |                  |
              |                  +-- inside: stripes (asc) -> FileCell lock -> apply -> log_record -> unlock
              +-- released before any wait for budget / durability
fsync:      handler flushes its file under the cell lock, drops all locks, then waits on durable_lsn
commit:     timer thread: gate.write() -> commit_cut_begin (cut) -> drop gate -> commit_cut_finish
flusher:    gate.read() -> cell lock of largest dirty file -> rebase_step -> unlock
destroy():  Dispatcher.drain() (join workers) -> test_destroy()  (finalizes handles, closes store)
```

`N = 1` must run the closure inline on the reader thread, so the default behavior and every existing test are unchanged. The pool is opt-in by `--threads N` on `slicefs mount` (flag in `cli.rs`, parameter into `mount.rs::run_mount`, stored in `MountedFs`). Do not set fuser `n_threads` on either platform for now; the vendored fuser refuses it outside Linux, and one reader plus the pool is one code path for both.

### Gate placement: one place, not 25

Each callback in `fuse_callbacks.rs` repeats the same two lines to take the gate. Move this into `Dispatcher` (the worker takes the guard, runs the closure, drops it). Do the same for the ENTRY/EXIT `eprintln!` lines: today every request writes two lines to stderr, and the stderr lock is itself a global serialization point once there are workers. Put them behind a `--trace-fuse` flag (default off) or `tracing` at debug level before measuring S3. Measure first with the lines on and off; that costs one afternoon and removes a confounder from every spike.

### Replay with out-of-order logs (step A3)

Today `handlers.rs` applies first and logs after (`Logged::log`), and `oplog::replay_stream` checks `got == *ino` for creates. With two workers creating at once, two Create records can reach the log in the reverse order of inode allocation, and the next mount fails. Two changes, both required together:

1. Log inside the same locks as the apply (the stripes of the directory, plus the cell for file data). `handle_write` and the handlers for create/mkdir/mknod/symlink/link/unlink/rmdir/rename/setattr/setxattr/removexattr move their `l.log(...)` call into `SliceFsFilesystem` methods or pass a log callback so the record is appended before the lock drops. Ops that touch the same inode or directory are then logged in apply order; ops on different inodes commute.
2. Replay creates *with the logged inode number*: add `create_inode_at(ino, meta)` (or an `Option<InodeId>` argument) to `DictMetadataStore` and the `MetadataStore` trait, raise `next_ino` to at least `ino + 1`, and drop the `got == *ino` equality check in `replay_stream`. `advance_next_ino` already exists for the old skip-a-number approach; it is not enough for reversed pairs. The project has no deployments, so the log format may change (see memory note on no store backward compatibility).

A crash-replay test with N threads must run on every change to `handlers.rs`.

### Metadata layer: what "no global lock on the hot path" means here

In `crates/metadata/src/store.rs`:

- Add `cs: Arc<ChunkStore>` to `DictMetadataStore` (cloned once from `io` at construction). Hot-path node reads and writes use `NodeReader(cs.clone())` and a small per-call writer. Keep `Arc<Mutex<StoreIo>>` in the public constructors, so `DictMetadataStore::new`, `SliceFsFilesystem::new` and `load_from_root` keep their signatures. This avoids touching the large number of tests that build a store from an `io`. `StoreIo`'s only unique job is `failed` / `unsynced` bookkeeping for `begin_commit_sync`; move that to an `AtomicU64` plus `Mutex<Option<io::Error>>` inside the store, or keep calling `io.lock()` only at commit time.
- Shard `Live<T>` (`inode_live`, `dir_live`) into 16-64 shards keyed by `ino`. Load a cold `DirList` or inode *outside* the shard lock, then insert if absent (as `load_meta` already does for inodes; `with_dir` does not).
- `inode_map` and `inode_data` both hold `ino -> inode digest` and `flush_inodes` writes the same value to both. This looks redundant. Verify before sharding; removing one halves the clone cost in `begin_commit` and one root-record field. (Not verified beyond reading `flush_inodes` and `finish_inner`.)
- `modify_inode(ino, f)` as above. `update_inode` currently calls `load_meta` then `store_meta`, which is two lock acquisitions, so it is not atomic.
- `set_manifest` / `get_manifest` take `io.lock()` around `intern_manifest` / `load_manifest`. `get_manifest` runs on every open. This is the most visible hot-path use of the global lock.

### WAL: group commit using code that already exists

`WalStrategy` already has the pieces: segment frames carry an LSN (`docs/design/write-log.md`), and `segment/writer.rs::SegmentWriter::file_for_sync` returns a cloned file handle so a caller can `sync_all` without holding the writer. `install_root` already uses it ("The disk sync runs with no lock held"). Group commit is therefore:

1. `log_mutation` gets the LSN under the writer mutex (exists) and returns it.
2. `flush_and_sync(lsn)`: if `durable_lsn >= lsn` return; else become the leader if none is syncing: under the mutex take `file_for_sync()` and the current LSN, drop the mutex, `sync_all`, store `durable_lsn`, notify a `Condvar`. Followers wait on the condvar. No separate flusher thread is needed for the first version.
3. Apply to `PerOpWal`, `FlushOnFsyncWal` and `PeriodicWal` (`buffered.rs` holds the shared logic).

This is independent of the pool, but only pays off with two or more concurrent fsyncs. S6 measures the gain, mostly for macOS where fsync is `F_FULLFSYNC`.

### Spikes: which can run without the lock work

| Spike | Needs | Can run | Notes |
|---|---|---|---|
| S2 inline pack fsync | Nothing (code landed) | Now, Docker Linux `scripts/docker-linux-test/run.sh bigwrite 1024 4096` | Compare with 1124 / 1772 ms in the research. Likely closes #108's remaining question. |
| S3 store-lock contention | A `ChunkStore`-level microbench, N threads each writing distinct data through `FileWriter` | Now, in-process, no mount | Same style as `race_tests.rs`. Decides per-worker packs. |
| S5 gate fairness | Step 0 harness | Early, in-process | Real `timed_commit` against N threads holding the gate shared. Also record `begin_commit` clone time at 100k and 1M files. |
| S6 group commit (Linux part) | WAL-only microbench (N threads: log + `flush_and_sync`) | Early, in-process | The macOS part needs a by-hand mount. |
| S1 FUSE-T overlap and reply order | Dispatcher skeleton "6a" (below) | By hand by the user on a Mac, after 6a | Not run by the research agent or in CI; it needs a real FSKit mount. |
| S4 per-inode overlap on Linux | 6a plus a kernel mount | Docker Linux (OrbStack), by hand | Counts concurrent requests per inode, under current mount options. |
| S7 budget under N writers | Rebase flusher (A5) | After A5 | Peak RSS and flusher lag. |

Key sequencing idea: build the dispatcher early as **6a: N workers, but every handler body runs under one global `Mutex`**. That is safe with today's single-thread assumptions (execution is serialized exactly as now, only the thread differs). It makes S1 and S4 measurable long before the lock work is done, and it tells the team early whether macOS overlaps at all. Step **6b** later removes that mutex, once A2-A5 have landed. If S1 says FUSE-T keeps one request outstanding, stop at N=1 on macOS and do not spend more on a macOS pool; steps A1-A5 still pay off through lower tail latency.

### Build order, track A (serial; each step ships alone)

| Step | Change | Depends on | Spike/test gate |
|---|---|---|---|
| A0 | `concurrency_tests.rs`: N threads over `handlers::*` on `Arc<SliceFsFilesystem>` (take the gate shared in the test, as callbacks do); mixed write, read, fsync, open/close, rename, unlink on shared files; per-op latency histograms; lock-hold timers on store, gate and WAL. Seeded and short (test-time budget). | nothing | Red tests: EBADF on concurrent write, stale read, lost update. They define "done" for A2-A4. Runs S3, S5, S6 (Linux) here. |
| A6a | Dispatcher with N workers and one global handler mutex; `--threads` flag; drain before `destroy` | A0 (for the harness), `fuse_callbacks.rs` | Enables S1, S4. |
| A1 | Remaining narrowing: `readers` Arc-per-inode, fsync outside the WAL `writer` mutex, `NodeReader`/`NodeWriter` in `store.rs`, sharded `Live`, `modify_inode`, trace lines off by default | A0 | S2 re-measure; S3 re-run. All still single dispatch thread, behavior unchanged. |
| A2 | `FileCell` table, shared alias state, atomic dirty cost, O(1) lookup by inode; `OpenFileState` never leaves the map | A0 red tests, A1 (`modify_inode`) | A0 tests for EBADF, stale read, no-op fsync go green. |
| A3 | Log under the lock; `create_inode_at`; replay without `got == ino` | A2 (cell lock) | N-thread crash-replay test. |
| A4 | `NamespaceLocks`, rename mutex, rank checker, atomic orphan/open-handle check | A2 | Rename/unlink/link stress; pjdfstest in Docker with `--threads 4`. |
| A5 | `flusher.rs`: soft/hard thresholds, writer-pays sleep, delete cross-file `keep_budget`; admission wait before taking gate and cell | A2 (atomics in cells), A4 | S7; "every worker throttled while a commit waits" deadlock test. |
| A6b | Remove the global handler mutex; default N stays 1 | A2-A5 | Linux CI with N=4 first; macOS only after S1. |
| A7 | WAL group commit (leader/follower, `durable_lsn`) | A1 (fsync outside mutex); worth doing after A6b | S6 on macOS by hand. |

Gate lock type (std vs a fair lock) is decided by S5, any time after A0.

### What the existing GC and commit invariants require (do not break)

- A rebase never straddles `gc_begin`: callbacks and the flusher hold the gate shared for the whole `rebase_step`; `gc_start_mode` takes it exclusively. The flusher must therefore take the gate itself. A rebase also must not run while holding the gate and waiting on budget (deadlock rule).
- `commit_cut_begin` relies on no request being in flight. With workers this still holds because workers hold the gate shared for their whole request. The dispatcher queue holds no gate and no lock, so a commit never waits for queued requests.
- Online GC adds the base of each overlay and a finished clone of each stream as roots (`gc_start_mode`). With `FileCell`s, that loop locks each cell while the gate is exclusive. Safe by the rule above.

## Part 2: `slicefs rehash --to <algorithm>` (#62)

### Decision: offline first

The issue allows "refuse while mounted" or "online job". Recommend **offline only in v2.2**: take `mount.lock` (the way `snapshot switch` and `gc` check it: `acquire_mount_lock` in `crates/metadata/src/mount_lock.rs`, refuse if present). An online job would have to run beside writers that put new objects under the write hash, interact with online GC marks, and race the open overlays. That depends on A2 (per-inode state) and is a separate piece of work. This is a reversible choice; report it.

Honest note for planning: a release build knows one hasher (`Blake3_256` in `crates/chunk-store/src/hash.rs`; `TestKeyedBlake3` exists only with the `test-hasher` feature, which `metadata` and `slicefs-cli` already enable in their dev-dependencies). So the rehash feature is fully testable but has **no production target until a second algorithm is added**. That choice is out of scope for #62. All end-to-end tests use `TestKeyedBlake3` (tag 0xf0).

### Why the rewrite cannot be a node-for-node ID map

`file.rs::is_cut` reads the last 4 bytes of the entry digest to decide manifest node boundaries. Under a new hash, the same chunk sequence gets different node boundaries, so the rewritten tree is a different shape. The correct rewrite is bottom-up from the chunk sequence:

```
old file root --> iterate level-0 entries (chunk id, len) in order, one node per level in memory
   for each: bytes = store.read(old id)          // verifies with the OLD tag's hasher
             new id = store.put(bytes)           // uses the store's write hasher (the new one)
             writer.push_entry(0, Entry{new id, len})
new file root = writer.finish()   // equals the key of a fresh full write of the same bytes
```

`FileWriter::push_entry` already exists as a private method (used by `splice`). Chunk boundaries are copied, not recomputed, so there is no FastCDC cost and it is independent of the #67 profile. Add to `crates/chunk-store/src/file.rs` a public `rewrite_file(store, old_root, memo) -> Result<ObjectId>` plus a streaming `for_each_chunk(store, key, f)` (next to `for_each_file_object`).

### What must be rewritten (complete list of places an `ObjectKey` appears)

| Place | How |
|---|---|
| File chunk objects and file manifest nodes | `rewrite_file` as above. |
| Metadata node *values* (inode, directory list, xattr set) | Contain no keys. Copy the bytes: `get_value` (old hash verifies) then `put_value` (new hash). Their node keys change. |
| Manifest list nodes (`manifest.rs`) | Bytes are 33-byte file root keys. Decode, map each through `rewrite_file`, re-encode, `put_value`. |
| The six maps of the root record | `inode_map` (`inode_map.rs`), `inode_data`, `dir_data`, `manifest_data`, `xattr_data` (`intern_u64_digest_map`), `refcounts` (`intern_key_u64_map`). Map each value (or key, for `refcounts`). **`refcounts` is keyed by content root: in a mixed-hash store two old keys can map to one new key, so counts must be summed**, not overwritten. |
| Root record (`root_rec`, 230 bytes) | Rebuild with the six new map keys; `next_ino`, `root_dir_ino`, `logical_bytes`, `inode_count` unchanged. |
| Snapshots | Every `SnapshotEntry.root` is a root record; rewrite each (shared subtrees hit the memo). `version`, `name`, `created_at` unchanged. |
| WAL root segment | New segment with seed `[RootUpdate(new root), Snapshot(...)*]`, written through `WalStrategy::rotate` (PerOp), which writes the root segment, syncs, and deletes older segments. |
| `FORMAT` marker `hash` line | `content::select_hash(store, Some(new))` before the rewrite starts (so `put` uses the new hasher), written by the same durable temp-and-rename code (`write_marker`). |
| **Not rewritten:** directory entry keys | `directory.rs::name_key` is always BLAKE3-256 of the name, by design. Do not touch. |

The enumeration of node kinds and their key references already exists once, in `store.rs::referenced` (GC mark). Rehash should not hand-copy it. Recommend a small shared module in `crates/metadata` (`node_graph.rs`): per node kind, a `children(bytes)` for the GC and a `rewrite(bytes, f: &mut dyn FnMut(ObjectKey) -> ObjectKey) -> Vec<u8>` for rehash. `referenced()` then calls `children`. When a later milestone adds prolly nodes, there is one place to extend, not two. Keep this refactor small: only wrap the existing `load_*` / `intern_*` functions (`load_u64_digest_map`, `load_manifest`, `load_inode_map`, `parse_key`, `root_rec` become `pub(crate)`).

### Crash safety and ordering

```
1. acquire mount.lock; refuse if present or dirty; refuse if the write log has records after the last root
   (metadata::segment::scan_store_with_log -> log.is_empty() must hold; else say "mount and unmount once to replay")
2. select_hash(new)               marker durable; mixed store is a supported state, mounts fine
3. rewrite objects, bottom-up     purely additive: puts are idempotent, old root untouched
4. cs.sync()                      every new object durable
5. install new root segment       the ONE commit point: before it, old root; after it, new root
6. gc --full                      frees the old objects (offline `gc.rs::gc_report`, or `--no-gc` to leave it)
```

A crash in 2-4 leaves the old root and a store that mounts (new writes use the new hash, old objects still read with their own tag). A crash after 5 leaves the new root. Re-running the command is safe: puts dedup, so finished work is cheap (it still re-reads and re-hashes). A log with records after the last root must be refused, because ops in the older segments would be dropped when the new root segment sorts after them.

### Keeping the memo bounded

The old-to-new ID map is the RAM risk (66 bytes per entry; 100M objects is 6.6 GB). Correctness does not depend on the memo, because every new ID is a pure function of the rewritten bytes. So: a bounded LRU `old ObjectId -> new ObjectId` (default about 256k entries, tens of MB) at two levels (file roots, metadata nodes). A miss recomputes, and `put` dedup keeps repeated work to read plus hash with no new write. The cost to watch is repeated work across snapshots; walk roots oldest to newest so shared subtrees stay hot. Only if the benchmark shows this repeat cost matters, add a disk-backed sorted map reusing the sorter in `crates/chunk-store/src/extsort.rs`. The metadata maps themselves are decoded in full (as `load_from_root` already does at mount), so RAM for a rewrite is about one mount's worth plus the LRU.

### Reporting and verification

Progress goes to stderr every N objects and to the final report: objects walked, objects rewritten, bytes read, bytes written, free space check up front (needs roughly a second copy until GC). Verify at the end (cheap): walk the new root with `referenced_roots` + `content_keys` + `file_objects` and assert every tag equals the target; then the test suite compares file contents and snapshot contents with the originals and runs `scrub`. Tests use `TestKeyedBlake3`: seed under blake3, `rehash --to test-keyed-blake3`, assert one tag, read back, take a snapshot before the rehash and read it after, run `gc --full`, assert the old tag is gone.

## Part 3: Configurable FastCDC profiles (#67)

### Where the constants reach (beyond `chunk_ranges`)

All in `crates/chunk-store/src/file.rs` unless noted:

- `FileWriter::write` buffers until `2 * CHUNK_MAX` and `cut` calls `chunk_ranges(&self.buf)`.
- `zero_chunk()`: a **process-wide** `static ZERO: OnceLock` of the zero chunk of exactly `CHUNK_MAX` bytes under each hasher. Under a profile with another max this is wrong. Make it a function of the profile (cache keyed by max, or a field computed at store open).
- `known_zero`, and the zero shortcut in `splice` (`start + max <= new_len`).
- `splice` read-ahead: starts at `CHUNK_AVG` and doubles; the read buffer holds `2 * CHUNK_MAX`.
- `crates/slicefs-cli/src/content.rs`: a test uses `chunk_store::CHUNK_MAX`.
- Metadata nodes also go through `FileWriter` (`store_io::put_value`), so they use the same profile unless told otherwise.

### Design

1. `pub struct ChunkParams { min: u32, avg: u32, max: u32 }` with `ChunkParams::DEFAULT` (4/8/32 KiB) and `ChunkParams::new` that validates against what `fastcdc::v2020::FastCDC::new` asserts, so a bad `--chunk-size` is an error, not a panic. Keep `CHUNK_MIN/AVG/MAX` as aliases of `DEFAULT` for tests and benches.
2. `Options.chunking: ChunkParams` (next to `hasher`), `ChunkStore::chunking()`. `chunk_ranges(params, data)`. `FileWriter` gets a `params` field (`FileWriter::with_params`; `Default` is `DEFAULT`, so the many existing tests are unchanged). `FileReader` needs nothing: it reads entries.
3. **Metadata nodes stay on `DEFAULT`**, always: `put_value` uses `FileWriter::default()`. File content uses the store profile (`content::Stream`, `splice`). This keeps data chunking independent of metadata and, later, of prolly node policy, as #67 and #19 require.
4. **Fixed at store creation.** Persist as a third `FORMAT` line (`chunking fastcdc 4096 8192 32768`; absent means default), written by `seed` (`--chunk-size MIN:AVG:MAX`) and read in `content::open` / `open_read_only` into `Options`. `mount` and other commands never change it; a mismatch is an error. Reason: `splice` and the key rule ("the key of a splice equals the key of a full write of the same bytes") hold only when the base was cut with the same profile. A base cut with another profile would still read correctly, but a rebase would restart at an old cut that is not a cut of the new chunking and re-chunk far past the change, and the canonical key would be lost. Changing a store's profile is a re-chunk job, out of scope. Bump the `FORMAT` line count carefully: the format number (6) only changes if an old reader would misread a new store; a new optional line does not need a bump.
5. Memory: the stream buffer is `2 * max` per open writer; with a 512 KiB max that is 1 MiB per open write handle on top of the dirty budget. Count it in the memory-bound tests (`memory_bound_tests.rs`).

### Benchmark (no code needed for the first part)

`benchmarks/chunk-bench` already accepts `fastcdc:MIN:AVG:MAX` (KiB). Step one is pure measurement: dedup ratio, chunk count and CPU of 4/8/32, 8/16/64, 8/32/128, 16/64/256, 32/128/512 on the Boost trees and on successive versions of one tree. Step two (needs the plumbing) is end-to-end on a real store: `seed --chunk-size`, then physical bytes, object count, index RAM and cold-open time from `slicefs stats` and `benchmarks/index-bench`, ingest MB/s, and rewrite bytes after insert/delete/modify at start, middle and end. The default stays 4/8/32 KiB unless the user decides otherwise.

## Part 4: Prolly-tree evaluation (#19)

### What is already there (the file-manifest half is largely built)

`crates/chunk-store/src/file.rs` already implements a content-defined Merkle tree over file chunks: entries are `(ObjectId, byte length)`, nodes end after an entry whose digest matches a 1-in-512 pattern (`NODE_CUT_ONE_IN`), with `NODE_MIN` 64 and `LEAF_ENTRIES` 4096, level k entries cover level k-1 nodes, and `FileReader` finds a byte offset by walking cumulative lengths. `splice` reuses whole old nodes. So for file manifests the open questions in #19 reduce to the node-size spread of a fixed cut probability versus Dolt's size-dependent probability, and write amplification per rebase. That is a short measurement plus a note, not a new implementation. (Evidence: `file.rs` module comment and `is_cut`, `FileWriter::push`, `docs/design/chunk-store.md` "Splice".)

### What the metadata baseline really costs

The metadata maps are not stored as one large object. `store_io::put_value` runs the serialized bytes through `FileWriter`, so a one-entry change in a 78k-entry map stores only the changed chunks (`docs/design/chunk-store.md` records 12 KB after one change in 5,000 files). What a commit still pays is O(n) **CPU and memory**: `begin_commit` clones the six maps under the exclusive gate; `finish_inner` re-serializes each map (`intern_u64_digest_map`, `intern_key_u64_map`, `intern_inode_map`) and FastCDC-chunks and hashes all of it; `load_from_root` decodes every map into RAM at mount. So the benchmark must report time and CPU, not only bytes, and must also count the gate hold from the clone. A prolly map attacks all three: only changed paths are re-serialized, a persistent tree can be snapshotted in O(1), and the map need not fit in RAM.

### Prototype plan

- New crate `crates/prolly`: `ProllyMap` over the existing `NodeRead` / `NodeWrite` traits with `u64 -> ObjectKey` entries (matches `dir_data`, `inode_data`, `manifest_data`). Operations: build from sorted entries, get, batched insert/delete producing a new root, iterate. Node boundary by hash of the entry key with a size-dependent probability (Dolt style), min and max entries; independent of the FastCDC profile (design invariant of #67/#19).
- Store each prolly node as **one object** (a new `put_object` on `NodeWrite`, a single `ChunkStore::put`) instead of through `FileWriter`. A 4-16 KiB node does not need chunking, and this keeps node policy independent of `ChunkParams`. Nodes then carry child `ObjectKey`s in a way the GC and rehash walkers can enumerate (`node_graph.rs` above).
- **Do not touch `root_rec`.** The prototype lives beside the real format, so rehash (B3) handles one format. Adoption would need: new root layout and a `FORMAT` bump, a prolly node kind in `node_graph.rs` (GC mark and rehash), an on-disk or lazily loaded map in `DictMetadataStore`, and changes to `begin_commit`. List these in the report; do not build them in v2.2.
- Benchmark in `benchmarks/prolly-bench` (same shape as `index-bench`): for the directory map, N = 78k, 1M, 10M entries, a commit after K = 1, 100, 10k changes. Compare today's path (serialize + `put_value`) with the prolly path: commit wall time and CPU, bytes written, on-disk size after several commits, and the map-clone time that `begin_commit` pays under the gate. Also record RAM.
- Crate maturity check first (the issue asks for it): look at existing Rust prolly or Merkle-search-tree crates before writing one; the estimate is 1,000-2,000 lines.
- Last step: the combined run with the #67 profiles (axis C of #67), after B2 and C1 exist. Axes A and B can run independently before that.

## Build order across all tracks

```
Track A (serial)   A0 -> A6a -> A1 -> A2 -> A3 -> A4 -> A5 -> A6b -> A7
                     \---- S3,S5,S6(Linux) ----/  S1,S4 after A6a  S7 after A5
Track B (parallel) B1 #67 benchmark with chunk-bench (no code)
                   -> B2 ChunkParams plumbing + FORMAT chunking line
                   -> B3 rehash (node_graph.rs, metadata/rehash.rs, file.rs rewrite, CLI)
Track C (parallel) C1 prolly crate + baseline measurement of today's commit  -> C2 combined #67 x #19 benchmark (needs B2, C1)
```

Why this order:

- Track A is serial because A2-A5 all rewrite the same few hundred lines of `filesystem.rs` and `handlers.rs`. Parallel work there creates merge pain, not speed. A0 comes first because its red tests define the exit of A2-A4, and A6a comes second because it unlocks the two spikes (S1, S4) that decide whether macOS is worth any effort.
- Tracks B and C do not touch Track A's files. They can be developed beside it by a different phase or on a different branch.
- **Conflicts to sequence inside tracks B and C:** B2 and B3 both edit `file.rs` and the `FORMAT` marker code in `content.rs` (`marker`, `select_hash`, `write_marker`). Do B2 first: it is a mechanical plumbing change, and B3 can then test with a non-default profile. B3 also needs `store.rs` internals made `pub(crate)`; do it after A1 lands its `store.rs` refactor, or isolate it in `node_graph.rs` and keep the diff small. C1 only reads `store.rs` for the baseline.
- The combined #67 x #19 benchmark goes last: it needs B2 (profile in the store) and C1 (prolly prototype).

### Test and CI plan

| Check | Where | Notes |
|---|---|---|
| N-thread in-process harness | `crates/slicefs-cli/tests/concurrency_tests.rs` | No mount; fast; seeded; part of normal `cargo test`. |
| Rank checker | debug builds, every test | Free coverage. |
| pjdfstest with `--threads 4` | `scripts/docker-linux-test/pjdfstest.sh` in CI Docker | Linux libfuse only. |
| FSKit mount with N workers | by hand and CI main only (per the test-time-budget rule) | Never run by the research or in the default test run. |
| Rehash end to end | `slicefs-cli/tests/rehash_tests.rs` with `TestKeyedBlake3` | Includes snapshot, crash-at-each-step (`CommitFault`-style hooks), `gc --full`, `scrub`. |
| Profile tests | `chunk-store` unit tests with a non-default `ChunkParams` | Oracle test: splice key equals full-write key under each profile; zero shortcut under each max. |

## Patterns to Follow

### Pattern 1: State stays in the map; the lock is the ownership

**What:** every open file has one `Arc<FileCell>`. Code that needs the state locks it; nobody removes it.
**When:** any code that today does `open_files.remove(&fh)` then re-inserts.
**Trade-offs:** a long step (a rebase) holds the cell, so readers of that file wait. The rebase step is bounded (1 MiB steps, `REBASE_STEP`), and a reader that only needs an overlay view copies it and releases.

```rust
let cell = self.cell_of(fh)?;             // Arc<FileCell>, map lock released at once
let mut st = cell.state.lock().unwrap();  // the inode's file lock
// apply the write, then append the log record, still holding `st`
```

### Pattern 2: Drop every lock before waiting

**What:** a request never waits for dirty budget, a commit, or durability while holding the gate, a stripe or a cell.
**When:** the budget admission step, fsync, anything that sleeps.
**Trade-offs:** a request re-takes its locks after the wait; the code is a retry loop. This is the only guard against the gate/budget/flusher deadlock (the largest risk in the research).

### Pattern 3: Add a seam, switch it off by default

**What:** `--threads` defaults to 1 and runs inline; the profile line defaults to 4/8/32; `rehash` is a separate command. Each new capability ships dark and is enabled after its spike result.
**When:** every step in this milestone.

## Anti-Patterns to Avoid

### Anti-Pattern 1: Mapping rehash node to node

**What people do:** keep a table of old ID to new ID and rewrite each manifest node in place.
**Why wrong:** node cuts depend on the digest (`is_cut`), so the new tree has different boundaries; the result is not the key of a fresh write and dedup against new writes fails.
**Instead:** copy chunk entries and rebuild the tree with `push_entry`.

### Anti-Pattern 2: Changing the chunk profile of a populated store

**What people do:** make `--chunk-size` a mount option.
**Why wrong:** splice and the canonical-key rule assume the base was cut with the same profile; the global `zero_chunk` cache is wrong; dedup between old and new data stops.
**Instead:** fix it at creation, record it in `FORMAT`, treat a change as a separate re-chunk job.

### Anti-Pattern 3: Taking the gate on the reader thread

**What people do:** keep `commit_gate().read()` in the callback and hand the guard to a worker.
**Why wrong:** a `std::sync::RwLockReadGuard` cannot move across threads, and a held guard while a request waits in the queue blocks the commit cut.
**Instead:** the worker takes the gate; queued requests hold nothing.

### Anti-Pattern 4: Logging after releasing the lock

**What people do:** apply, unlock, then append the record (today's handlers).
**Why wrong:** with two workers the log order differs from the apply order; replay of creates fails.
**Instead:** append under the locks and replay creates by inode number.

### Anti-Pattern 5: Letting prolly nodes into the root record in v2.2

**What people do:** wire the prototype into `root_rec` to "see it work".
**Why wrong:** a second metadata format doubles the rehash and GC walker work and blocks the format decision (#15).
**Instead:** keep it a separate crate with its own benchmark.

## Scaling Considerations (daemon)

| Concern | 1 worker (today) | 4-8 workers | When it breaks |
|---|---|---|---|
| Hot-path locks | Uncontended | `ChunkStore` inner lock (short), WAL append mutex, shard locks | Per-worker packs only if S3 shows the appender saturates (FastCDC + BLAKE3 at 530-910 MB/s per core can outrun one appender). |
| fsync cost | One at a time | Group commit shares one sync | macOS `F_FULLFSYNC` dominates; S6 decides priority. |
| Dirty memory | `DIRTY_BUDGET` 64 MiB, `DIRTY_LIMIT` 32 MiB per file | Same, via global atomic and flusher | Plus the dispatcher queue (bytes) and the retry layer's kept copies; both must be capped (#98). |
| Metadata maps | Cloned and re-serialized per commit | Same; the clone is under the gate | Around millions of files: `begin_commit` clone time and mount RAM. #19 is the answer if measured. |
| Rehash | n/a | n/a | RAM follows the largest metadata map and the LRU; disk needs about 2x until GC. |

## Integration Points

### External services

| Service | Integration | Notes |
|---|---|---|
| fuser (vendored, `vendor/fuser`) | `Filesystem` trait methods take `&self`; `Reply` is `Send + 'static` | Keep one reader. On the FUSE-T socket, `send` holds a process-wide `STREAM_SEND_LOCK` for the whole write loop, so a large READ reply blocks every other reply. Include it in S1. `receive_framed` has no lock, which is another reason to keep one reader. |
| FUSE-T / FSKit (macOS) | Closed server | Overlap and out-of-order replies are unknown (LOW). S1 by hand on a Mac. |
| Linux libfuse | Kernel decides per-inode ordering | The comment "FUSE serializes per-fh writes" holds only with one thread; do not rely on it. |

### Internal boundaries

| Boundary | Communication | Notes |
|---|---|---|
| `fuse_callbacks.rs` to `Dispatcher` | Boxed closure plus moved `Reply` | Bytes-bounded queue; inline when N = 1. |
| `handlers.rs` to `filesystem.rs` | Direct calls; log append moves inside | Replay still calls the same handlers with logging off (`replay_guard`). |
| `slicefs-cli` to `metadata` | `MetadataStore` trait plus `DictMetadataStore` methods | New: `modify_inode`, `create_inode_at`. `Arc<Mutex<StoreIo>>` stays in constructors. |
| `metadata` to `chunk-store` | `NodeRead` / `NodeWrite`, `put_value` / `get_value` | `put_value` pinned to the default profile; new `put_object` for prolly nodes. |
| `rehash` to everything | `node_graph.rs` shared with GC `referenced()` | One enumeration of node kinds and their key references. |
| `content.rs` to `chunk-store::Options` | `FORMAT` marker lines (`hash`, `chunking`) | One writer function for the marker; rehash and `seed` both use it. |

## Open questions to settle in phase research

- Does `std::sync::RwLock` block new readers while a writer waits on this platform and version? (S5; decides std vs a fair lock.)
- Is `inode_map` redundant with `inode_data`? (Read of `flush_inodes` and `finish_inner` says yes; verify before sharding.)
- How large is the `begin_commit` clone hold at 1M and 10M files? (`commit_timer` logs `hold_ms`; measure.)
- Do FUSE-T and FSKit deliver more than one request at once, and accept out-of-order replies? (S1, by hand.)
- Is a second production hash planned? Without it, rehash ships with test-only coverage of its real purpose.
- Does a Rust prolly or Merkle-search-tree crate exist that is mature enough to use instead of writing one? (First task of C1.)

## Sources

- `docs/research/multithreaded-request-handling.md` (design, spikes S1-S7, steps 0-7; written against 6fd1e11)
- `docs/design/write-log.md`, `docs/design/chunk-store.md` (splice, object IDs and hash, index, GC)
- GitHub issues #19, #21, #22, #62, #67, #127 (`gh issue view`)
- Code read for this file: `crates/slicefs-cli/src/{filesystem,handlers,fuse_callbacks,mount,commit_timer,oplog,content,gc,snapshot,online_gc}.rs`; `crates/metadata/src/{store,store_io,directory,manifest,inode_map,snapshot,gc/mod}.rs`, `crates/metadata/src/wal/{mod,per_op}.rs`, `crates/metadata/src/segment/`; `crates/chunk-store/src/{lib,file,hash,mark}.rs`; `vendor/fuser/PATCHES.md`, `vendor/fuser/src/{session,channel}.rs`; `benchmarks/chunk-bench`
- Git history on `main` for #108 (PRs #131, #133)

---
*Architecture research for: SliceFS v2.2 Concurrency & Store Evolution*
*Researched: 2026-11-20*
