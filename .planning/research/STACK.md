# Stack Research

**Domain:** SliceFS v2.2 "Concurrency & Store Evolution": multithreaded request handling (#127, #21, #22), `slicefs rehash` (#62), configurable FastCDC profiles (#67), prolly-tree evaluation (#19)
**Researched:** 2026-11-20
**Confidence:** MEDIUM-HIGH. Crate versions and dates come from the crates.io API and from files in this repo (HIGH). Behaviour claims taken from docs.rs or changelog pages are marked "(doc page)". The source-confidence seam rates web fetches LOW, so check those before they carry a design decision.

This file is a delta. It records only what v2.2 adds, changes or refuses. The existing stack (FastCDC v2020 + BLAKE3, packs, zstd, sorted-run index, write log, online GC, vendored fuser) is not researched again. `docs/research/multithreaded-request-handling.md` is the base for all threading choices here.

## Summary of the answer

| Area | Decision |
|------|----------|
| Locks | Add `parking_lot` **0.12.x** (not 0.13). Own sharded inode table, own commit gate, own request queue. |
| Lock-free reads | Add `arc-swap` 1.9.x, at first use only (chunk-store read view or committed root). |
| Queues | No `crossbeam`. A byte-bounded queue over `parking_lot` `Mutex` + `Condvar`. |
| Parallel CPU | No `rayon`. `std::thread::scope` over packs for rehash. |
| Concurrent maps | No `dashmap`. 64 hand-written shards, as the chunk-store memtable already does. |
| Persistent maps | Nothing now. `imbl` only if #21 option B is triggered. |
| FastCDC (#67) | No crate change. Keep `fastcdc` 3.2.x. Make `min/avg/max` a validated, persisted `ChunkProfile`. |
| Prolly (#19) | No crate. Prototype on the existing content-cut manifest tree. |
| Rehash (#62) | No crate. |
| fuser | Stay on vendored 0.18.0. No `n_threads`. Do not rebase onto 0.19.0 inside v2.2. |
| Benchmarks | No `criterion`. Keep the bin + `getrusage` + fio pattern. Add `hdrhistogram` to one new stress bench only. |
| Concurrency tests | `shuttle` (dev-only) for the gate/budget/flusher protocol. `parking_lot` `deadlock_detection` in stress builds. No `loom`. |

## Recommended Stack

### Core Technologies (new in v2.2)

| Technology | Version | Purpose | Why Recommended |
|------------|---------|---------|-----------------|
| `parking_lot` | `0.12` (lock has 0.12.5) with feature `arc_lock` | Per-inode `Mutex`, sharded inode-table `RwLock`, WAL append `Mutex`, budget/`durable_lsn` `Condvar`, commit-gate internals | See "Why parking_lot 0.12" below. It is already in `Cargo.lock` through fuser, so the tree keeps one copy and no new code is compiled. |
| `arc-swap` | `1.9` (1.9.5, 2026-11-09) | Wait-free reads of an immutable snapshot that a rare writer replaces | Zero dependencies, very widely used (about 420M downloads, 124M recent). Use only where a reader lock shows up in a spike (see "Where arc-swap goes"). |
| `fastcdc` | keep `3.2` (lock 3.2.1) | Content-defined chunking, now with run-time `min/avg/max` | 3.2.1 already takes the three sizes in `FastCDC::new`. Its v2020 limits cover every profile in #67 (see "#67"). No upgrade is needed. |

### Why parking_lot 0.12, not 0.13

- fuser's `Cargo.toml` asks for `parking_lot = "0.12"`. If SliceFS depends on `0.13`, the tree compiles two copies of `parking_lot`, `lock_api` and `parking_lot_core`. `deadlock_detection` would then watch only one copy at a time. Use `"0.12"` and keep one copy.
- 0.13.0 (2026-10-21, one month old) needs Rust 1.95 and edition 2024, removes `const_*` constructors and `hardware-lock-elision`, moves recursive read locks to a new `RecursiveRwLock`, and drops automatic eventual fairness (changelog page). The local toolchain is a February 1.95 nightly, so 0.13 would pin the toolchain to the newest line for no gain.
- Revisit 0.13 when the vendored fuser moves to it.

Why use `parking_lot` at all, when std locks are futex-based and small:

1. **No poisoning.** With N workers, one panic while a std lock is held poisons it, and every other worker then hits the poison path. The repo already has about 59 `.lock().unwrap()` or `unwrap_or_else(|e| e.into_inner())` sites in `filesystem.rs` alone. `parking_lot` removes that noise and gives the semantics that the repo already chooses by hand. Wrap each worker body in `catch_unwind` and reply `EIO`, so a panic fails one request and not the mount.
2. **`deadlock_detection` feature.** A watchdog thread calls `parking_lot::deadlock::check_deadlock()` in stress builds. The largest risk in the research doc is a deadlock between the gate, the budget and the flusher. Detection works only for `parking_lot` locks, so the hot locks should all be `parking_lot`. Forward it as a SliceFS feature (`deadlock-detect = ["parking_lot/deadlock_detection"]`), off by default.
3. **`arc_lock` feature.** `Mutex::lock_arc()` gives an owned guard that holds the `Arc`. This is what two-inode locking (rename, link, in increasing inode order) needs: clone the `Arc`s out of the table, drop the shard guard, then lock. No borrowed guard crosses a function boundary.
4. `Condvar::wait_while` and `wait_for`: needed for the "writer pays by sleeping" budget and for `durable_lsn`.

**Do not expect `parking_lot` to settle the commit gate.** Its `RwLock` is task-fair: a waiting writer blocks new readers (doc page, 0.12 `RwLock`). `std::sync::RwLock` is writer-preferring too on Linux (not verified for the macOS futex implementation). That is exactly the "writer sleeps on budget while holding the gate shared, commit queues for exclusive, flusher cannot take shared" deadlock in the research doc. Spike S5 measures it. Plan for the gate to be a small SliceFS type (`Mutex<{readers, writer_waiting, generation}>` + `Condvar`) with an explicit admission rule, so the policy is ours and the shuttle model checks our code and not a library's fairness. Keep `std::sync::RwLock` until S5 decides. Note `parking_lot` guards are `!Send` by default. The research doc has the worker, not the reader, take the gate, so `send_guard` is not needed.

### Where arc-swap goes

Add it at first use, not before, and only for read-mostly immutable snapshots:

| Candidate | Use | Trigger |
|-----------|-----|---------|
| Chunk-store read view: `ArcSwap<ReadView { generation, packs }>` | `ChunkStore::read` stops taking the store mutex just to check the generation and get the pack handle. Writers publish a new view on pack roll, seal, GC and merge. | S3 shows read-side wait on the store lock after step 1 (pack fsync out of the lock). This is the strongest candidate. |
| Committed root | Replace the root read under the gate with a snapshot. | Only if a profile shows gate-shared cost. The gate already protects the root. |
| Per-inode `ArcSwap<ExtentMap>` | #21 option B. | Only with a persistent map (below) and only if S4 shows readers wait on the inode `RwLock`. |

### Not added, with the reason

| Candidate | Version seen | Decision | Why |
|-----------|--------------|----------|-----|
| `dashmap` | 6.2.1 (2026-05-17); 7.0.0-rc2 since 2025-03-05 | **No** | (1) Its shard locks are internal. Steps 0 and 4 need lock-hold timers and a debug rank-order checker, and they cannot see inside. (2) Many methods "may deadlock if called when holding any sort of reference into the map" (`insert`, `remove`, `entry`, `get_mut`, `retain`, ...; doc page). Our design takes an inode lock after the table lookup, which invites this. (3) 7.0 has sat at rc for 20 months. Write `ShardedTable<V>`: 64 shards of `parking_lot::RwLock<HashMap<u64, Arc<V>>>`, `get_or_insert` returns an `Arc` clone and releases the shard at once. About 60 lines. The chunk-store memtable is already sharded by hand. |
| `crossbeam` / `crossbeam-channel` 0.5.17 | 2026-09-05 | **No, for now** | The dispatch queue must be bounded **in bytes** (a queued WRITE owns a payload copy). A channel gives a count bound only, so a side semaphore would be needed anyway. One `Mutex<VecDeque<Req>> + bytes + 2 Condvars` gives both bounds in one structure that the rank checker and lock timers can see. `std::sync::mpmc` is still nightly-only (tracking issue #126840), and CI builds on stable. Revisit `crossbeam-channel` only if a spike shows that the queue mutex is hot (FUSE request rates are far below that). Never add the umbrella `crossbeam` crate. |
| `rayon` 1.12.0 | 2026-04-14 | **No** | #21 mentions it for CPU work, but request workers block on I/O (pread, fsync waits), and a second pool beside the worker pool oversubscribes the cores and hides memory growth in `par_iter().collect()`. Chunks are 4 to 512 KiB, so `blake3`'s `rayon` feature (for large single inputs) gives nothing. Rehash is parallel by pack: `std::thread::scope` with an `AtomicUsize` cursor over the pack list, and `ChunkStore::put` is already thread-safe (hash and compress run outside the store lock). |
| `tokio` / async | declared in workspace, unused | **No** | The research doc already settles this (CPU-bound work, `tokio::fs` uses a thread pool, no io_uring on macOS). Do not wire the unused `tokio`, `redb` or `anyhow` workspace entries. |
| `im` 15.1.0 | last release 2022-04-29 | **No** | No release for 4 years. Use its maintained fork if a persistent map is ever needed. |
| `imbl` 7.0.2 | 2026-09-09, Rust 1.85 | **Contingent** | Only for #21 option B (per-inode `ArcSwap<ExtentMap>`). Provides `OrdMap` with `range()` and structural sharing, `Send + Sync` when the items are. Do not add before S4 shows contention. Option A (`BTreeMap` under a per-inode `RwLock`) comes first, and the extent map stays behind a small interface so the change is local. |
| `rpds` 1.2.1 | 2026-05-15, Rust 1.85 | **Alternative to imbl** | `RedBlackTreeMap` with `_sync` variants. Choose it only if `imbl` fails a range-query benchmark. |
| `scc` 3.9.1, `papaya` 0.2.5, `left-right` 0.11.8 | active | **No** | Lock-free maps and double-buffered maps solve problems that sharded `RwLock` tables do not have at FUSE rates, and they hide locks from the rank checker. |
| `loom` 0.7.3 | 2026-11-08 | **No** | Exhaustive search. It targets the atomics memory model, and the README says it can miss executions and treats `SeqCst` as `AcqRel` (doc page). Its state space does not scale to a gate + budget + flusher + N workers protocol. Use shuttle. |
| `indicatif` and similar | n/a | **No** | `rehash` reports progress with `tracing` lines and the existing `--json` output (`serde_json` is already a dependency). |

## Supporting Libraries

| Library | Version | Purpose | When to Use |
|---------|---------|---------|-------------|
| `shuttle` | `0.9` (0.9.10, 2026-11-05), **dev-only**, behind `cfg(shuttle)` | Randomized and PCT schedule testing of the concurrency protocol | One phase: the gate + budget + flusher + `durable_lsn` protocol (deadlock guard from the research doc, "test in which every worker is throttled while a commit waits"). `shuttle::sync` has `Mutex`, `RwLock`, `Condvar`, `Barrier`, `mpsc`, atomics with the **std API shape**. It has no `parking_lot` types (doc page). So the protocol code must use a thin `crate::sync` shim (`Mutex`, `Condvar` with a `parking_lot`-shaped API) that maps to `parking_lot` normally and to shuttle under `cfg(shuttle)`. Model our own gate type, not a library `RwLock`. Not exhaustive, so it finds bugs and does not prove their absence. |
| `hdrhistogram` | `7.6` (7.6.0, 2026-07-18), `default-features = false`, **bench-only** | Client-side latency percentiles with coordinated-omission correction | In the new `benchmarks/stress-bench` only: S1 (delayed READ reply while another process stats files) and S2, S3, S5, S6, S7 p99 and "slowest write". Per-thread recorders merge at the end. Not in the daemon. |
| `libc` | existing | `getrusage`, `VmHWM` / RSS reads in benches | Already the repo's measurement method (`chunk-bench`, `index-bench`). |
| `blake3` | existing | Boundary hash of prolly entries; the rehash hash | No new hash crate for boundaries (see #19). |
| `crc32c` | existing 0.6 | Prolly node checksum | Same framing as the other metadata nodes. |
| `serde_json` | existing | `--json` progress of `rehash` | Already in the CLI. |

Atomic counters and a 32-bucket log2 array (`[AtomicU64; 32]`) are enough for in-daemon lock-hold and wait timers (step 0). They need no crate and add no allocation on the hot path.

### Development Tools

| Tool | Purpose | Notes |
|------|---------|-------|
| `fio` (existing job files) | Parallel I/O scaling for S3 and #22 acceptance ("scales with the number of workers") | `numjobs=N` on distinct files. Do not copy this loop into Rust. |
| `scripts/docker-linux-test/run.sh bigwrite` | S2 and S7 (Docker, 1 and 4 GiB) | Existing. Add the new bench as another mode. |
| `chunk-bench` (existing, accepts `fastcdc:<min>:<avg>:<max>` in KiB) | #67 axis (A): dedup bytes and CPU per profile | It already does most of the dedup and CPU part of #67. It does not measure pack and index overhead, RSS, cold open or rewrite bytes after edits. |
| `index-bench` (existing) | #67 object count, index RSS, cold-open time per profile | Reuse its harness. |
| New `benchmarks/stress-bench` (`publish = false`) | S1 and S3 to S7 client harness: N threads of mixed write, read, fsync, open/close, rename against a mount | Step 0 of the migration. Uses `hdrhistogram`. |
| New `benchmarks/prolly-bench` (`publish = false`) | #19 commit-cost benchmark | Same layout as `index-bench`; depends on `chunk-store`. |

## Per-feature integration

### #21 and #22: threads (Phases: steps 0 to 7 of the research doc)

- Replace `std::sync::{Mutex, RwLock, Condvar}` with `parking_lot` in the hot crates: `slicefs-cli` (`filesystem.rs`, `commit_timer.rs`, `gc_pacing.rs`), `metadata` (`store.rs` WAL and `store_io`), `chunk-store` (`retry.rs`, `locality.rs`, `lib.rs` store mutex). Do it as one mechanical commit with no behaviour change, early in the milestone, so later diffs show only logic. Keep the commit gate on std until S5.
- Rank-order checker: a thread-local stack of held ranks, behind `debug_assertions`, wrapped in the `crate::sync` shim (rank 1 gate, 2 rename mutex, 3 inode locks by increasing inode number, 4 short maps, 5 chunk-store and WAL internals). No crate.
- Request queue: `RequestQueue { Mutex<VecDeque<Req>>, bytes, Condvar not_empty, Condvar not_full }`. The reader blocks when the byte or count bound is reached. Drain it before `destroy()`.
- fuser: stay on the vendored 0.18.0. `Filesystem` is already `&self`, `Send + Sync + 'static`, and `Reply` is `Send + 'static`, so the pool needs no fuser change. Each callback copies `Request` fields and the WRITE payload, moves the `Reply` into the queue, and returns. **Do not use `Config::n_threads` or `clone_fd`**: the vendored session refuses them outside Linux, and the receive path of the FUSE-T socket has no lock. They stay a later Linux-only option. Replies from N workers on FUSE-T rely on `STREAM_SEND_LOCK` in `channel.rs` writing whole frames, which S1 must exercise.
- fuser 0.19.0 was released today (2026-11-20). Its changelog (doc page) lists macOS `rename()` flag handling (filesystems must reply `EINVAL` for unsupported flags), a fix for "garbled names in `rename()`" on macOS (it may overlap local patch 3 in `vendor/fuser/PATCHES.md`; not checked), and `BackgroundSession` drop and unmount behaviour changes. The reader-plus-pool design needs nothing from it, and a rebase means re-testing all three patches on FUSE-T. Put it in a separate issue after #22, not inside v2.2.

### #62: `slicefs rehash --to <algorithm>`

No new crate. It reuses code that is already in the tree:

| Need | Existing piece |
|------|----------------|
| Choose and verify algorithms | `chunk_store::hash` (`ObjectHasher`, `hasher_by_name`, `hasher_by_tag`; tag byte `0x1e` is BLAKE3-256) |
| Old ID to new ID map larger than RAM | `chunk-store/src/extsort.rs` (external sort, spills to disk) |
| Write objects under the new ID | `ChunkStore::put` (thread-safe; hash and compression outside the store lock) |
| Parallelism | `std::thread::scope` workers that take packs from an `AtomicUsize` cursor |
| Atomic switch | One `RootUpdate` record in the write log, as a commit does |
| Free old objects | Existing reachability mark (`mark.rs`) and online GC; no new GC code, only a rule that new-ID objects are live while the job runs |
| Tests with two algorithms | `test-hasher` feature (`TestKeyedBlake3`) |

The choice of a real second algorithm is out of scope for #62. Whichever it is, it needs one `ObjectHasher` impl and at most one hash crate.

### #67: configurable FastCDC profiles

- Keep `fastcdc` at `3.2` (lock 3.2.1). The v2020 limits in the 3.2.1 source are `MINIMUM` 64 B to 1 MiB, `AVERAGE` 256 B to 4 MiB, `MAXIMUM` 1 KiB to 16 MiB. Every profile in #67 (4/8/32 up to 32/128/512 KiB) is inside them.
- `fastcdc` 3.x asserts on bad sizes, and 4.x and later turn the checks into `debug_assert`. SliceFS must validate in a `ChunkProfile::new(min, avg, max)` constructor: `min <= avg <= max`, all inside the limits, all powers of two. Powers of two avoid a known trap: 4.0.0 changed the mask choice for non-power-of-two averages and 4.0.1 reverted it (changelog page), so cut points for odd averages differ between crate releases.
- Persist the profile in the format marker next to the `hash` line, for example `chunk fastcdc-v2020 4096 8192 32768`, with the algorithm name, the three sizes and the gear seed (0). A marker without the line means 4/8/32. The profile is chosen when the store is created (`seed`), and a mount cannot change it, because chunk boundaries decide dedup across everything already stored. Do not use `with_level_and_seed`: a seed breaks dedup between stores, and the project goal is a global store.
- Add a golden cut-point test (fixed pseudo-random input, fixed profile, exact boundary list). It must pass before and after any future `fastcdc` upgrade. `fastcdc` 5.0.0 (2026-08-22) is optional and adds only `rechunk`. It requires even sizes and keeps cut points for even sizes (changelog page). Do not upgrade in v2.2 unless the golden test is in place first.
- The constants are used in 23 places in `chunk-store/src/file.rs` (zero-chunk shortcut, `2 * CHUNK_MAX` write buffer, read-ahead step, rebase window) and in `lib.rs`, `content.rs`, `filesystem.rs`, `seed.rs`. Turn them into fields of one `ChunkProfile` value in `Options` and in the file writer. A 512 KiB maximum means a 1 MiB write buffer per open writer, which touches the shared dirty budget and the #98 memory bound.
- Keep the manifest cut parameters (1 in 512 entries, 64 to 4,096 entries per node, `is_cut` on the last 4 digest bytes) independent of the chunk profile. This is the design invariant that #67 states for #19.
- Benchmarks: no new tool for the profile sweep. Extend `chunk-bench` and `index-bench` as listed above, and add pack and index overhead, RSS and rewrite bytes after edits at the start, middle and end.

### #19: prolly-tree evaluation

**Write the prototype; do not take a crate.** The file manifest is already a content-cut tree: leaf nodes cut on the digest of an entry (1 in 512, 64 to 4,096 entries), and index nodes carry `(ObjectId, byte length)`, as the #19 design asks. A one-entry change at the start of a 200,000-chunk file gave 2 new nodes (`docs/design/chunk-store.md`). The prototype for the directory map (key, value) extends this builder with sorted keys, a boundary hash of `blake3(key || value)` and, if needed, a Dolt-style rule where the cut probability depends on the current node size (pure arithmetic). Nodes are stored as chunk-store objects with a hand-written binary format and `crc32c`, like the other metadata nodes. Estimate 1,000 to 2,000 lines, as #19 says. Keep it in `benchmarks/prolly-bench` first.

Crates found and why none fits:

| Crate | Version (date) | What it is | Why not |
|-------|----------------|------------|---------|
| `prolly-map` | 0.7.2 (2026-08-22), about 10k downloads | Content-addressed versioned ordered map; sync and async; diff, merge, proofs; pluggable `Store` trait | Node IDs are SHA-256 CIDs (doc page). SliceFS IDs are 33-byte tagged IDs, and GC mark and `rehash` (#62) must follow and rewrite node links. A second ID scheme breaks both. It stores nodes as CBOR via `serde_cbor`, and pulls `rayon`, `serde_json`, `xxhash-rust`. The page calls it pre-1.0 and says node formats may be hard-cut over. Single owner. |
| `prollytree` | 0.4.1 (2026-08-27), about 3.7k downloads | Probabilistic tree with Git-backed versioning, branching, SQL and vector search | Heavy: `gix`, `sha2`, `bincode`, optional RocksDB, Git object storage. It is a different product (a versioned KV store), not a node format that can live in our packs. |
| `prolly` | 0.1.1 (2022-04-08) | Early implementation | Abandoned. |
| `merkle-search-tree` | 0.8.0 (2024-08-21), about 190k downloads | In-memory Merkle search tree for CRDT anti-entropy | Levels come from a key hash (skip-list style, not size-balanced). No node store and no byte-length index. It is an in-memory structure with page hashes. |

Optional: use `prolly-map` only as an outside reference for correctness of a diff, not as a dependency. The benchmark baseline is the current full re-serialization, and the metrics (commit time, bytes written per commit, on-disk size after several small commits, on the Boost trees with about 78,000 files) come from existing tooling plus a bytes-written counter on the chunk store.

### Benchmark tooling

- Keep the repo pattern: a standalone bin per question, CPU time from `getrusage`, RSS from `VmHWM`, results pasted into the design doc, a README with the command line. These are single-shot or few-run macro measurements (bytes, commits, RSS), so `criterion` statistics do not fit.
- `criterion = 0.5` is declared in `[workspace.dependencies]` but no crate uses it and it is not in `Cargo.lock`. Do not bump it to 0.8.2. Remove the line when convenient.
- Lock-hold and wait timers inside the daemon: `Instant::now()` around `lock()`, with `AtomicU64` sum, max and the log2 bucket array, behind a `lock-stats` cargo feature (off by default). They are for S3 and S5 and for the stress harness.

## Installation

```toml
# Cargo.toml [workspace.dependencies]  (add)
parking_lot  = { version = "0.12", features = ["arc_lock"] }   # one copy with fuser
arc-swap     = "1.9"                                           # add at first use only
hdrhistogram = { version = "7.6", default-features = false }   # benchmarks/stress-bench only
shuttle      = "0.9"                                           # dev-only, cfg(shuttle)

# crates/slicefs-cli/Cargo.toml and crates/metadata/Cargo.toml and crates/chunk-store/Cargo.toml
# parking_lot.workspace = true
#
# crates/slicefs-cli/Cargo.toml
# [features]
# deadlock-detect = ["parking_lot/deadlock_detection"]
# lock-stats = []
```

```bash
# Concurrency model-check run (once the protocol lives behind crate::sync)
RUSTFLAGS="--cfg shuttle" cargo test -p slicefs-cli --lib gate_protocol

# Stress build with deadlock watchdog
cargo build -p slicefs-cli --features deadlock-detect,lock-stats
```

No new crate is needed for #62, #67 and #19.

## Alternatives Considered

| Recommended | Alternative | When to Use the Alternative |
|-------------|-------------|-----------------------------|
| Own `ShardedTable` over `parking_lot::RwLock` | `dashmap` 6.2.1 | If a profile shows shard contention and the rank checker is already built so that it can wrap `dashmap` calls. Do not hold a `Ref` while taking an inode lock. |
| Own byte-bounded `RequestQueue` | `crossbeam-channel` 0.5.17 | If a spike shows the queue mutex hot, or if a pipeline of three or more stages appears (for example the group-commit flusher). |
| Scoped threads over packs for rehash | `rayon` 1.12 | If rehash becomes CPU-bound inside one very large pack and the pack-level split is too coarse. Then use a private `rayon::ThreadPool` of fixed size, never the global pool inside the daemon. |
| `parking_lot` 0.12 | `std::sync` locks | If the team wants zero dependency change. The cost is poisoning and no deadlock watchdog. |
| `shuttle` 0.9 | Stress test only | If the `crate::sync` shim is judged too intrusive. Then rely on the step-0 stress harness plus `deadlock_detection`, and accept that rare interleavings are not explored. |
| Hand-written prolly prototype | `prolly-map` 0.7.2 | If the project accepts a second ID scheme for metadata nodes. It does not today (tagged IDs, #62). |
| `fastcdc` 3.2.1 | `fastcdc` 5.0.0 | When `rechunk` (reuse of masks and gear tables) shows up in a profile as a cost. Gate on the golden cut-point test. |
| `imbl` 7.0.2 | `rpds` 1.2.1 | If `imbl` is slower on range queries over extents. |

## What NOT to Use

| Avoid | Why | Use Instead |
|-------|-----|-------------|
| `parking_lot` 0.13 now | Second copy beside fuser's 0.12, needs Rust 1.95, changes recursive read and fairness behaviour | `"0.12"` |
| `parking_lot::RwLock` or `std::sync::RwLock` as the answer to the commit gate | Writer-preferring: a queued commit blocks new shared holders. That is the deadlock path in the research doc | An own gate type with an explicit admission rule, decided by S5 |
| `dashmap` for the inode table | Hidden shard locks, documented deadlock rules, 7.0 stuck at rc | `ShardedTable` |
| Holding a table shard guard while taking an inode lock | Breaks the lock order (inode-table shard first, then inode) when two inodes are involved | Clone the `Arc` out, drop the shard guard, then lock in increasing inode number |
| `fuser` `Config::n_threads` / `clone_fd` | Refused outside Linux by the vendored session; the FUSE-T socket receive path is unlocked | One reader thread plus the SliceFS worker pool |
| `rayon` global pool in the daemon | Competes with request workers and hides unbounded memory | Scoped threads and bounded queues |
| `with_level_and_seed` in fastcdc | Different seed gives different cuts and no dedup between stores | Seed 0 recorded in the marker |
| Non-power-of-two FastCDC averages | Cut points changed between fastcdc 4.0.0 and 4.0.1, and 5.0 requires even sizes | Powers of two only |
| A prolly-tree crate with its own hash and store | Second ID scheme, GC and rehash must follow links | Prototype on chunk-store objects |
| `criterion` for #19, #67, S1-S7 | These are macro measurements of bytes, RSS and tail latency | Existing bin + `getrusage` + fio pattern, plus `hdrhistogram` |
| `loom` | State explosion for this protocol | `shuttle` |

## Stack Patterns by Variant

**If S1 shows FUSE-T keeps one request outstanding (no overlap on macOS):**
- Ship the pool with N = 1 on macOS and N > 1 on Linux. No stack change; the code path stays the same.
- Because the pool then adds only a hand-off cost on macOS.

**If S3 shows the store lock is the bottleneck after step 1:**
- Add `arc-swap` for the chunk-store read view first.
- Then consider one pack appender per worker. This is a format and GC change, not a crate change.

**If S4 shows readers wait on the per-inode lock:**
- Add `imbl` for `ArcSwap<ExtentMap>` (option B), behind the existing extent-map interface.
- Otherwise stay with option A.

**If #19 shows a clear win on the directory map:**
- The next step is a store-format change with a version bump. The repo allows it (no deployments), and #62 (`rehash`) must then also rewrite prolly node links.

## Version Compatibility

| Package A | Compatible With | Notes |
|-----------|-----------------|-------|
| `parking_lot` 0.12.5 | `fuser` 0.18.0 (vendored, asks `0.12`) | One copy in the lock file. Rust 1.71 or newer. |
| `parking_lot` 0.13.0 | needs Rust 1.95, edition 2024 | Not compatible with fuser's `0.12` requirement without a second copy. |
| `arc-swap` 1.9.x | any Rust the workspace uses | No dependencies. |
| `shuttle` 0.9 | std-shaped `Mutex`, `RwLock`, `Condvar`, `mpsc` | Does not wrap `parking_lot` types; use the `crate::sync` shim. |
| `fastcdc` 3.2.1 | `chunk-store` (edition 2021) | Takes `u32` sizes. 4.x moves them to `usize`; 5.0 requires even sizes. Golden test first. |
| `imbl` 7.0.2, `rpds` 1.2.1 | Rust 1.85 | Local and Docker toolchains are newer. |
| Workspace toolchain | local 1.95 nightly; Docker `rust:1-bookworm` (stable) | `chunk-store` and benches are edition 2021, other crates 2024. No edition change is needed. |

## Sources

- `crates/*/Cargo.toml`, `Cargo.lock`, `vendor/fuser/Cargo.toml`, `vendor/fuser/PATCHES.md`, `vendor/fuser/src/session.rs`, `crates/chunk-store/src/lib.rs`, `docs/design/chunk-store.md`, `docs/research/multithreaded-request-handling.md`, GitHub issues #19, #21, #22, #62, #67, #127: read directly (HIGH).
- crates.io API (versions, dates, MSRV, download counts) for `parking_lot`, `arc-swap`, `dashmap`, `crossbeam*`, `rayon`, `im`, `imbl`, `rpds`, `papaya`, `scc`, `left-right`, `loom`, `shuttle`, `criterion`, `hdrhistogram`, `fastcdc`, `fuser`, `prolly`, `prolly-map`, `prollytree`, `merkle-search-tree` (HIGH for numbers and dates).
- fastcdc 3.2.1 source in the local cargo registry: v2020 size limits (HIGH).
- fastcdc `CHANGELOG.md` (3.2.x, 4.0.0, 4.0.1, 5.0.0): web fetch, LOW per the confidence seam; verify with the golden test before any upgrade.
- parking_lot `CHANGELOG.md` (0.13.0 changes): web fetch, LOW; the `RwLock` task-fair policy is from the 0.12 docs and should be confirmed in S5.
- fuser `CHANGELOG.md` (0.17.0, 0.18.0, 0.19.0): web fetch, LOW.
- docs.rs `dashmap` 6.2.1 `DashMap` (deadlock notes), `prolly-map` 0.7.2, `prollytree` 0.4.1, `shuttle::sync`, `loom` README: web fetch, LOW (page summaries). Maturity statements about `prolly-map` come from its own docs page.
- Rust unstable book and tracking issue #126840 (`std::sync::mpmc` still nightly): web search, LOW.

---
*Stack research for: SliceFS v2.2 Concurrency & Store Evolution*
*Researched: 2026-11-20*
