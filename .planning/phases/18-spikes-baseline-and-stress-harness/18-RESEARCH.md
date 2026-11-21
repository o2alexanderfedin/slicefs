# Phase 18: Spikes, baseline and stress harness - Research

**Researched:** 2026-11-20
**Domain:** Rust concurrency measurement: an in-process stress harness over the `SliceFsFilesystem` core, a `lock-stats` timing feature, spike scripts (Linux Docker mount), and a FUSE-T/FSKit overlap probe (macOS CI)
**Confidence:** HIGH for code facts (read this session); MEDIUM for macOS/FSKit behaviour (nothing measured yet, that is the point of S1)

<user_constraints>
## User Constraints (from CONTEXT.md)

### Locked Decisions

**Harness dependencies**
- Latency and lock histograms use `hdrhistogram` 7.6 as a dev-dependency only (user choice, as in `.planning/research/STACK.md`) — **Reversibility:** costly — a new crate in `Cargo.lock`; removal means a rewrite of the harness report code.

**Decided autonomously** (these are in CONTEXT.md under "Decided autonomously"; treat as locked)
- [Harness] Q: "Where does the harness run?" → Selected: "In process, against the `Filesystem` core API (no FUSE mount), as an integration test in `crates/slicefs-cli/tests/`" — test code only; it can move without effect on the product.
- [Harness] Q: "How does the harness pass on today's code, which is safe only with one thread?" → Selected: "A `serialize` mode: one harness mutex around each operation, so N threads run but only one operation at a time. The suite runs with serialize on. A run with serialize off is the target for Phases 19-21 and is `#[ignore]` until then" — a harness flag; Phases 19-21 turn it off.
- [Harness] Q: "Short run and long run?" → Selected: "Short run in `cargo test` (a few seconds, small files, below the dirty budget); long run is `#[ignore]` with env vars for threads, duration and file size, started by hand" — test settings only; the "keep tests short" rule applies.
- [Harness] Q: "How are the data checked?" → Selected: "A model of each file (expected bytes per range) and of the namespace (names to inodes), compared after each step and at the end; a seed is printed so a failure can be run again" — test code only.
- [Lock stats] Q: "How are lock hold and wait times measured?" → Selected: "Thin timing wrappers on the store lock (`io`), the commit gate, `open_files`, `readers` and the WAL append mutex, compiled only with a cargo feature `lock-stats` (off by default, on for the harness and spikes)" — feature-gated; no cost in normal builds and simple to remove.
- [Spikes] Q: "How are S3, S5 and S7 run before a worker pool exists?" → Selected: "With the harness threads, which call the core directly (S3 store lock with N = 1, 2, 4, 8; S5 gate fairness with a 1 s commit timer and long shared holds; S7 4 random writers on 1 GiB files with a 256 MiB cap)" — measurement code only.
- [Spikes] Q: "How are S2, S4 and S6 run?" → Selected: "On the Linux Docker mount (`scripts/docker-linux-test/`): S2 with `bigwrite.sh` at 1 GiB and 4 GiB; S4 by the kernel queue depth per inode (`/sys/fs/fuse/connections/*/waiting`) during random writes plus readahead; S6 with many small fsynced files, per-op log against periodic" — scripts only.
- [Spikes] Q: "How is S1 run, given that local macOS mounts are not allowed?" → Selected: "A small standalone fuser test server (one reader and a 4-worker pool, one READ reply delayed by 200 ms while another process stats files), run in a separate macOS CI workflow that starts by hand (`workflow_dispatch`) or on a `ci/**` branch, like `macos-mounts.yml`" — a test binary and a CI workflow; both can be removed.
- [Docs] Q: "Where do the results go?" → Selected: "A results section per spike in `docs/research/multithreaded-request-handling.md`, with the numbers, the decided step, and superseded baselines marked" — documentation only.

### Claude's Discretion
- Exact operation mix, thread counts and durations of the short run, inside the "keep tests short" rule.

### Deferred Ideas (OUT OF SCOPE)
None — discussion stayed within phase scope. (Phase boundary: no change to the request path; fine-grained locks are Phase 19, budget flusher Phase 20, worker pool Phase 21.)
</user_constraints>

<phase_requirements>
## Phase Requirements

| ID | Description | Research Support |
|----|-------------|------------------|
| SPIKE-01 | Spike results S1-S7 are recorded in the threading research doc, each names the migration step it decides; S2 and S5 are measured again on the current code (after #108) | "Spike-by-spike plan" table, "State of the Art" (stale-baseline table), Validation grep checks. The doc numbers steps **0-7**, not 1-8 as CONTEXT says. |
| SPIKE-02 | In-process stress harness: N threads of mixed operations (read, write, fsync, create, rename, unlink) on one file and on many files; checked against a model; latency histograms and lock hold/wait times | "Harness design", "lock-stats design", entry points `slicefs_cli::handlers::handle_*`, `hdrhistogram` 7.6 verified to compile with `default-features = false` |
| SPIKE-03 | S1 (FUSE-T overlap and reply order) run on a macOS FSKit mount and recorded; result decides whether the macOS default can be more than one worker | "S1 probe", compiled skeleton, workflow pattern from `macos-mounts.yml`, decision table |
</phase_requirements>

## Project Constraints (from CLAUDE.md and memory)

No `./CLAUDE.md` exists in the repo. The user's global CLAUDE.md says "Avoid doing things with little to no value." Memory entries that bind this phase (treat as locked):
- **Test time budget:** short waits; the Boost mount run only by hand / main / `ci/**`; merge on CI green. The short harness run must take a few seconds.
- **No whole file in RAM:** per-file daemon memory is bounded, never O(file size) (#98). The harness model must not hold file bytes either (use seed-based expected content, see below).
- **No local macOS mounts:** macOS CI mounts are fine. In-process tests may run locally on macOS (they are not mounts).
- **Chunk size deferred:** FastCDC stays 4/8/32 KiB. Do not touch.
- **Delegates reversible choices:** pick and report; ask only for one-way doors. Nothing in this phase is a one-way door except the `hdrhistogram` dependency, which is already decided.
- **Cargo needs** `export PATH="$HOME/.cargo/bin:$PATH"`. Wait loops in scripts must exit when the process ends and have a time limit.

## Summary

The phase is measurement code and docs only. Five facts in the current tree change what the CONTEXT assumes, and the plan must absorb them:

1. **#108 is closed, so two baselines in the research doc are superseded.** The pack sync runs in the background (#131), and the commit gate is held only for the generation cut (#133: `commit_timer.rs` header, `timed_commit`). Post-#108 numbers already exist in the repo history: slowest write 440 / 686 ms at 1 / 4 GiB on `90a5443`, and 117 / 1542 ms on `3f3ff34` (the 4 GiB tail was a writer at `KEEP_CAP` waiting for a 2 s disk sync), plus a gate hold of about 28 ms of a 470 ms commit. S2 and S5 are therefore re-measured to *confirm and tighten*, not to discover. Single bigwrite runs vary a lot (commit `736a490`: 1 GiB base runs 127 and 1427 ms), so each S2 point needs at least 3 interleaved runs reported as min / median / max.
2. **The "store lock" is two locks.** CONTEXT says "store lock (`io`)". `io` (`Arc<Mutex<StoreIo>>`) guards metadata nodes: 16 non-test lock sites in `metadata/src/store.rs`, only constructor-time sites in `filesystem.rs`. File content goes through `self.content` (`Arc<ChunkStore>`) whose `inner: Arc<Mutex<Inner>>` is the lock that `put` and `read` take and that the research doc means for S3. Time **both**, under separate labels (`io`, `store`).
3. **The WAL append lock is `DictMetadataStore.wal`, not only the segment writer.** `log_wal_entry` holds the `wal` guard across `log_mutation` (`store.rs:514-523`), so it is still a global append lock; `flush_wal` was fixed to use a cloned handle (`wal_handle`, `store.rs:453-463, 654-657`), but `per_op.rs:81` and `buffered.rs:98-111` still sync while holding their own `writer` mutex. PITFALLS.md "WAL sync is off the lock" is only half true.
4. **The commit gate is taken only in `fuse_callbacks.rs`.** The `test_*` and `handlers::handle_*` entry points do not take it. The harness must take `fs.commit_gate().read()` per operation itself, so it can time the gate (wait and hold) with no product change; `timed_commit` already returns `CommitTimes { gate_wait, hold, total }`.
5. **S3, S5 and S7 cannot run with free threads on today's code** except in a safe subset. `write`, `settle`, `finalize_write` take the file's `OpenFileState` out of `open_files` for the whole operation (EBADF for a concurrent op on the same handle), and `keep_budget` takes another file's state out. So: S3 uses distinct files, no namespace ops, total dirty below the budget (safe, to be proven by the model check); S5 uses synthetic shared holders plus a real commit; S7 can only be a serialized baseline (peak RSS and slowest write) until Phase 20.

**Primary recommendation:** Build one integration test file `crates/slicefs-cli/tests/stress_harness.rs` that drives `slicefs_cli::handlers::handle_*` (the exact calls that FUSE callbacks make, including WAL logging) from N threads with a `serialize` flag; add a `lock-stats` feature whose recorder lives in `chunk-store` (lowest crate) and forwards through `metadata` and `slicefs-cli`, instrumented at lock *sites* (no type changes); add `hdrhistogram` 7.6 (`default-features = false`) as a `slicefs-cli` dev-dependency; run S1 with a compiled-and-checked fuser probe in a `workflow_dispatch` macOS job; record results in the research doc as `### S<n> result` sections with a `**Decides:**` line.

## Architectural Responsibility Map

| Capability | Primary Tier | Secondary Tier | Rationale |
|------------|-------------|----------------|-----------|
| Stress harness (threads, model, histograms) | Test code (`slicefs-cli/tests`) | — | Calls the core in process, no mount; can move without product effect (locked) |
| Lock hold/wait timing | `chunk-store` (recorder, lowest crate) | `metadata`, `slicefs-cli` (call sites) | Locks live in three crates; `chunk-store` does not depend on `slicefs-traits`, so the recorder must live in `chunk-store` |
| Commit-gate timing | Harness itself | `commit_timer::timed_commit` (existing) | Gate is taken by callers, not by the core; harness plays the callback role |
| S1 overlap probe | Standalone fuser example (`slicefs-cli/examples`) | macOS CI workflow + shell client | Needs a real FUSE-T/FSKit mount; no SliceFS state needed |
| S2/S4/S6 (mounted Linux runs) | Docker scripts (`scripts/docker-linux-test/`) | — | Same container pattern as `bigwrite.sh`; by hand, not CI |
| Results and decisions | `docs/research/multithreaded-request-handling.md` | — | Locked location |

## Standard Stack

### Core
| Library | Version | Purpose | Why Standard |
|---------|---------|---------|--------------|
| `hdrhistogram` | 7.6.0 (published 2026-07-18) [VERIFIED: crates.io API, `cargo info hdrhistogram`] | Latency histograms and percentiles in the harness | Locked by CONTEXT; dev-dependency only. `rust-version = 1.88`, `edition = "2024"` [VERIFIED: `cargo info hdrhistogram`] |
| `libc` | already in workspace (`"0.2"`) | `getrusage` for peak RSS (S7); `ru_maxrss` is KiB on Linux, bytes on macOS | Already a `slicefs-cli` dependency [VERIFIED: crates/slicefs-cli/Cargo.toml] |
| `tempfile` | already a dev-dep | Store dirs | Existing test pattern |

Add to `crates/slicefs-cli/Cargo.toml` `[dev-dependencies]`:
```toml
hdrhistogram = { version = "7.6", default-features = false }
```
Checked: a scratch crate (edition 2024) with exactly this line built and ran `Histogram::<u64>::new_with_bounds(1, 60_000_000_000, 3)`, `record`, `value_at_quantile` on the local toolchain (rustc 1.95.0-nightly, 2026-02-10). `default-features = false` drops `flate2`, `nom`, `base64`, `crossbeam-channel`; the lockfile then gains only `hdrhistogram` and `byteorder` (`num-traits` and `autocfg` are already locked) [VERIFIED: scratch `Cargo.lock`].

**MSRV note:** the workspace declares **no** `rust-version` in any crate [VERIFIED: `grep rust-version crates/*/Cargo.toml` found none]. `hdrhistogram` 7.6 needs 1.88; local rustc is 1.95 nightly. The Docker image (`rust:1-bookworm`, latest stable) and the macOS runner toolchains are `[ASSUMED]` to be >= 1.88; the Docker run of `cargo test --workspace` in CI will show it immediately.

**STACK.md says** `hdrhistogram` belongs in a new `benchmarks/stress-bench`. **CONTEXT is locked and wins:** it goes in `slicefs-cli` dev-dependencies, harness in `tests/`. Product code must not depend on it, so the `lock-stats` recorder cannot use it (see below).

### Alternatives Considered
| Instead of | Could Use | Tradeoff |
|------------|-----------|----------|
| Instrument at lock sites | Swap `Mutex` for a timed newtype | Rejected: `Arc<Mutex<StoreIo>>` and `Arc<RwLock<()>>` are public and built by about 25 test files; `chunk-store/src/lib.rs:2747` and `metadata/src/store.rs:153` name `MutexGuard` in types |
| `lock-stats` feature | `parking_lot` deadlock/lock-stats tools, `tracing` spans | Adds dependencies for a measurement; STACK.md already decided `parking_lot` is for Phase 19+, not here |

## Package Legitimacy Audit

| Package | Registry | Age | Downloads | Source Repo | Verdict | Disposition |
|---------|----------|-----|-----------|-------------|---------|-------------|
| hdrhistogram | crates.io | since 2015-07-07 (11 yrs) | ~2.0M/wk | github.com/HdrHistogram/HdrHistogram_rust | OK | Approved (also named in locked CONTEXT) |
| byteorder (transitive of hdrhistogram) | crates.io | long-lived | very high | github.com/BurntSushi/byteorder | not run (transitive, not recommended directly) | Comes in through `Cargo.lock` only |

**Packages removed due to [SLOP] verdict:** none
**Packages flagged as suspicious [SUS]:** none

`gsd-tools query package-legitimacy check --ecosystem crates hdrhistogram` returned `"verdict": "OK"`, `weeklyDownloads: 2004534`, `postinstall: null`, `deprecated: false` [VERIFIED: gsd-tools seam]. Crates have no postinstall scripts; `build.rs` of the transitive deps is the standard `autocfg`/`byteorder` kind (not inspected beyond the lockfile list).

## Architecture Patterns

### System Architecture Diagram

```
                          harness test (crates/slicefs-cli/tests/stress_harness.rs)
 seed + env (threads,         |
 secs, file MiB, serialize)   v
        |            +------------------+      per-thread: op picker (seeded RNG)
        +----------> | thread pool N    |  --> picks op from mix + owned ranges/names
                     +--------+---------+
                              |  for each op:
                              |   [serialize? take harness mutex]
                              |   t_wait0 -> gate.read() -> t_hold0         <- harness times the gate itself
                              v
              slicefs_cli::handlers::handle_{read,write,fsync,create,rename,unlink,open,release}
                              |   (same calls the FUSE callbacks make; they log to the WAL)
                              v
              SliceFsFilesystem  --open_files / readers / handles-->  ChunkStore.inner  --> packs
                       |   \--> meta (DictMetadataStore) --io lock--> StoreIo --> ChunkStore
                       \--> meta.log_op --wal lock--> WalStrategy (per_op writer | buffered pending+writer)
                              |
        lock-stats (feature):  chunk_store::lock_stats records wait/hold per Site into atomic log2 buckets
                              |
 after op: update model (per-block versions, name->ino), record latency in thread-local hdrhistogram
 commit thread: commit_timer::timed_commit(&fs, &gate) every 1 s -> CommitTimes {gate_wait, hold, total}
                              |
 end: join, merge histograms, model-vs-fs final compare, print seed + report (latency p50/p99/max per op,
      lock-stats table, gate wait/hold). Failure message prints the seed.

 S1 (separate): macOS CI job -> fuse-t + enable-fskit -> build example s1_probe -> mount (backend=fskit)
      -> shell client: dd of "slow" (READ delayed 200 ms by pool worker) + python3 stat loop of new names
      -> probe prints "S1 RESULT arrived=.. max_inflight=.. reply_order_inversions=.."
```

### Recommended Project Structure
```
crates/chunk-store/src/lock_stats.rs     # recorder (feature "lock-stats"); no-op inline fns otherwise
crates/chunk-store/Cargo.toml            # lock-stats = []
crates/metadata/Cargo.toml               # lock-stats = ["chunk-store/lock-stats"]
crates/slicefs-cli/Cargo.toml            # lock-stats = ["chunk-store/lock-stats", "metadata/lock-stats"]; dev-dep hdrhistogram
crates/slicefs-cli/tests/stress_harness.rs        # SPIKE-02 harness (+ S3/S5/S6/S7 entry tests, #[ignore])
crates/slicefs-cli/examples/s1_probe.rs           # S1 server
scripts/docker-linux-test/s4-waiting.sh, s6-smallfsync.sh   # S4, S6 (beside bigwrite.sh)
scripts/macos-mount-test/s1-overlap.sh            # S1 client + limits
.github/workflows/s1-overlap.yml                  # macOS job
docs/research/multithreaded-request-handling.md   # "## Spike results (Phase 18)" with ### S1..S7 result sections
```

### Public API the harness can call without a mount

[VERIFIED: crates/slicefs-cli/src/lib.rs:3-25 `pub mod handlers;`, `pub mod filesystem;`, `pub mod commit_timer;`, `pub mod oplog;`]. `SliceFsFilesystem` (the type is named `SliceFsFilesystem`, not `Filesystem`) is `Send + Sync + 'static`: a scratch example with `fn need<T: Send + Sync + 'static>() {}; need::<slicefs_cli::filesystem::SliceFsFilesystem>();` passed `cargo check` this session. (The trait `fuser::Filesystem: Send + Sync + 'static` is at `vendor/fuser/src/lib.rs:400`, and `MountedFs(Arc<SliceFsFilesystem>)` implements it.)

Use **`slicefs_cli::handlers`**, not the `test_*` methods. The handlers are what `fuse_callbacks.rs` calls; they freeze the thread-local request clock and append the WAL record (`handlers.rs` `Logged::log`). The `test_*` methods log nothing, so the WAL locks would never be hit. [VERIFIED: handlers.rs `handle_write`, `handle_create`, `handle_unlink`, `handle_rename`, `handle_fsync`]. Signatures, verbatim:

```rust
pub fn handle_read(fs: &SliceFsFilesystem, ino: u64, offset: u64, size: u32) -> DataResult
pub fn handle_open(fs: &SliceFsFilesystem, ino: u64, flags: i32) -> OpenResult          // OpenResult::Ok(fh, is_write)
pub fn handle_release(fs: &SliceFsFilesystem, ino: u64, fh: u64) -> EmptyResult
pub fn handle_fsync(fs: &SliceFsFilesystem, ino: u64, fh: u64) -> EmptyResult           // flush record + meta().flush_wal()
pub fn handle_write(fs: &SliceFsFilesystem, fh: u64, offset: u64, data: &[u8]) -> WriteResult
pub fn handle_create(fs: &SliceFsFilesystem, parent: u64, name: &str, mode: u32, umask: u32, uid: u32, gid: u32, flags: i32) -> CreateResult   // CreateResult::Ok(ino, fh, FileAttr)
pub fn handle_unlink(fs: &SliceFsFilesystem, parent: u64, name: &str) -> EmptyResult
pub fn handle_rename(fs: &SliceFsFilesystem, parent: u64, name: &str, newparent: u64, newname: &str, flags: u32) -> EmptyResult
pub fn handle_lookup(fs: &SliceFsFilesystem, parent: u64, name: &str) -> EntryResult   // EntryResult::Ok(ino, FileAttr)
pub fn handle_readdir(...)  // for the final namespace compare
```

**How existing tests build a filesystem** (copy this; it gives a WAL so handlers log) [VERIFIED: tests/write_log_replay_tests.rs:27-36 and dirty_budget_tests.rs:104-112]:

```rust
fn new_fs(store: &Path, config: WalConfig) -> SliceFsFilesystem {
    std::fs::create_dir_all(store.join("segments")).unwrap();
    let wal = create_wal(config, store, 1).unwrap();
    let io = Arc::new(Mutex::new(StoreIo::open(store).unwrap()));
    let mut meta = DictMetadataStore::new(io.clone());
    meta.set_wal(wal);
    let fs = SliceFsFilesystem::new(meta, io, Some(store.to_path_buf()));
    fs.meta().commit().unwrap();
    fs
}
```
`WalConfig` variants [VERIFIED: wal/mod.rs:62-72]: `PerOp` (daemon default, `parse_wal_config` default), `FlushOnFsync`, `Periodic { interval_secs }`, `NoWal`. `small_file_bench.rs` builds a bare fs with no WAL (`DictMetadataStore::new(io.clone())`, `None`): do not copy that for the harness.

Other useful pub items: `fs.commit_gate() -> Arc<RwLock<()>>`; `commit_timer::timed_commit(&SliceFsFilesystem, &RwLock<()>) -> Result<CommitTimes, MetaError>` (`gate_wait`, `hold`, `total`); `fs.test_set_dirty_budget(bytes)`, `fs.test_set_dirty_limit(bytes)`, `fs.test_dirty_total()`; consts `slicefs_cli::filesystem::{DIRTY_LIMIT (32 << 20), DIRTY_BUDGET (64 << 20), REBASE_STEP (1 << 20)}` [VERIFIED: filesystem.rs:97, 102, 109]; `chunk_store::retry::{KEEP_LIMIT (8 << 20), KEEP_CAP (3 * KEEP_LIMIT)}` [VERIFIED: retry.rs:33, 37].

### Where each lock is taken (for `lock-stats`)

All quoted from files read this session; "sites" are non-test lock calls.

| Label | Lock | Where (file:line) | Held across I/O? |
|-------|------|-------------------|------------------|
| `store` | `ChunkStore.inner: Arc<Mutex<Inner>>` (`chunk-store/src/lib.rs:712`) | `inner.lock()` at lib.rs lines 859, 878, 890 (`step`), 1340, 1360 (`put`), 1396, 1493, 1510 (`gc_begin*`), 1540, 1661, 2357 (`read`), 2409, 2442, 2523, 2543, 2551 (`sync`), 2565, 2597, 2659 — 19 sites | `put`: yes by design (pack append); background pack sync (#131) is outside it |
| `io` | `Arc<Mutex<StoreIo>>` (metadata node I/O) | `metadata/src/store.rs`: 285, 551, 579, 618, 643, 742, 816, 1131, 1144, 1155, 1165, 1182, 1197, 1214, 1223, 1368 (16); `filesystem.rs`: 261 (ctor), 673 (GC check) | node get/put |
| `wal` | `DictMetadataStore.wal: Mutex<Option<Arc<dyn WalStrategy>>>` (`store.rs:255`) | `store.rs:515` `if let Some(ref w) = *self.wal.lock().unwrap() { ... w.log_mutation(entry) ...` — guard held across the append (global append lock); `wal_handle()` at 655-657 clones the Arc | yes, a kernel `write` per op in PerOp |
| `wal_writer` / `wal_pending` | per-strategy segment writer and buffer | `wal/per_op.rs`: `writer.lock()` at 41, 50, 56, 65, 81 (`flush_and_sync` syncs *under* it), 92; `wal/buffered.rs`: `pending.lock()` 79, 102, 115, 133, 150; `writer.lock()` 86, 106 (`flush_and_sync`: `write_out(..).and_then(\|()\| writer.sync())` under both) , 116, 134, 154 | yes: fsync under the mutex |
| `open_files` | `Mutex<HashMap<u64, OpenFileState>>` (`filesystem.rs:201`) | `open_files.lock()` at lines 335, 372, 687, 814, 996, 1012, 1018, 1080, 1113, 1137, 1157, 1221, 1250, 1266, 1273, 1347, 1363, 1456, 1464, 1577, 1601, 1659, 1699, 1743, 1771, 1776, 1804, 1856, 2409, 2457, 2461, 2478, 2579, 2616, 2688, 2700 (36 non-test; 4190 is test code) | short; never held across I/O; the *state* is taken out (see below) |
| `readers` | `Mutex<HashMap<u64, content::Reader>>` (`filesystem.rs:211`) | lines 601 (remove) and 981 (`read_root`: guard held across `Reader::read(...)`, "readers.get_mut(&ino).expect(...).read(&self.content, offset, size)") | **yes: across the whole read** (step-1 item "narrow the readers lock" is NOT done) |
| gate | `commit_gate: Arc<RwLock<()>>` (`filesystem.rs:221`) | `fuse_callbacks.rs`: one `gate.read()` per callback (36 sites); `commit_timer::timed_commit` `gate.write()`; `filesystem.rs:680` GC. **Not** taken by `handlers::*` or `test_*` | the harness takes it itself |

**Take-out pattern (why serialize mode is needed)** [VERIFIED: filesystem.rs:1112-1118]: `// Take the file state out of the map so we can release open_files before io.` `let mut file_state = match self.open_files.lock().unwrap().remove(&fh) { Some(s) => s, None => return Err(libc::EBADF), };`. `finalize_write` (1361-1366) does the same and returns `Ok(())` on a missing entry ("Already closed"). `keep_budget` (1242-1273) does `let Some(mut other) = self.open_files.lock().unwrap().remove(&fh) else { return Ok(()); };` on the *largest other file* when the total passes the budget. Consequence: two threads on one handle get EBADF or silent no-ops, and above the budget two threads on different files can collide. Keep every concurrent (serialize-off) run below `DIRTY_BUDGET`.

### lock-stats design (smallest change that works)

1. **Crates and features.** Recorder in `chunk-store` (it has no dependency on `slicefs-traits`; `metadata` and `slicefs-cli` both depend on it). `chunk-store`: `lock-stats = []`. `metadata`: `lock-stats = ["chunk-store/lock-stats"]`. `slicefs-cli`: `lock-stats = ["chunk-store/lock-stats", "metadata/lock-stats"]`. `slicefs-cli` already has a dev-dependency on `chunk-store` with `features = ["test-hasher"]`; Cargo unifies features, no conflict. Note `chunk-store` is `edition = "2021"`, the others `2024`.
2. **Instrument sites, do not change types.** Add one function and call it at the sites in the table:
```rust
// chunk_store::lock_stats  (sketch)
pub enum Site { Store, Io, Wal, WalWriter, WalPending, OpenFiles, Readers }
#[cfg(not(feature = "lock-stats"))]
#[inline(always)]
pub fn lock<'a, T>(m: &'a std::sync::Mutex<T>, _s: Site) -> std::sync::MutexGuard<'a, T> { m.lock().unwrap() }
#[cfg(feature = "lock-stats")]
pub fn lock<'a, T>(m: &'a std::sync::Mutex<T>, s: Site) -> TimedGuard<'a, T> {
    // try_lock first: uncontended = zero wait; else time the blocking lock() = wait. Hold = guard lifetime.
}
```
   With the feature on, `TimedGuard` derefs to `T` and records hold in `Drop`. Sites that name `MutexGuard<'_, Inner>` (lib.rs:2747) or pass a guard by type (`store.rs:153` holds `MutexGuard<'a, ()>`, a different mutex) need a small fix only for the `Inner` one; wrap `inner` through a private `fn lock_inner(&self)` so the guard type is named once. Convert sites mechanically (`sed`), then `cargo check` with the feature on and off.
3. **Recorder storage:** product code cannot use `hdrhistogram` (dev-dependency only). Use fixed log2 buckets of nanoseconds in `AtomicU64` arrays per (Site, wait|hold) plus count, sum, max. Expose `snapshot() -> Vec<SiteStats>` and `reset()`. The harness converts a snapshot to percentiles (resolution factor 2 is enough for a lock table) or prints bucket counts. Per-sample callbacks into a sink are an alternative; atomics are simpler and add less overhead.
4. **Gate:** no feature code. The harness wraps `gate.read()`/`gate.write()` itself; `timed_commit` already reports `gate_wait` and `hold`.
5. **Cost:** `Instant::now()` twice per lock when the feature is on; zero when off (the off-path function inlines to `m.lock().unwrap()`, the same behaviour as today's sites, including panic-on-poison).

**CI shape (decide in the plan):** CI runs `cargo test --workspace` with no features (`.github/workflows/ci.yml`; Docker `run.sh test` runs `cargo test -q --workspace`). So the short run must compile and pass **without** `lock-stats` (lock table printed as "lock-stats off"), and a second short test gated `#[cfg(feature = "lock-stats")]` checks that the six site labels appear with non-zero counts. Recommendation: do not add a lock-stats CI step now; run `cargo test -p slicefs-cli --features lock-stats --test stress_harness` by hand and in the verification step.

### Harness design

**Threads and ownership (so a global model is checkable with serialize off).** With serialize on, one mutex makes every op atomic, so a plain sequential model compared after each step is exact. With serialize off (Phase 19-21 target), overlapping writers to the same bytes have no deterministic result, so the *same* harness code must use ownership: split each shared file into 4 KiB blocks; block `b` is owned by thread `b % N` (only that thread writes it). Content of a block is a pure function `block_bytes(seed, file, block, version)` (the `byte_of`/`bytes_of` splitmix pattern in `dirty_budget_tests.rs:82-95`). The model holds only a `u32` version per block (1 GiB file = 262,144 blocks = 1 MiB) and an `AtomicU32` published version, so **no file bytes live in RAM** (memory constraint, and S7's RSS stays honest). Reader check: for an owned block expect exactly the last written version; for another thread's block accept any version in `[published_at_read_start, published_at_read_end]`; block never written reads as zeros (sparse) or base content. Namespace: each thread owns names `t{n}-{k}` in a few shared directories plus its own directory; create/rename/unlink only touch owned names, model = `HashMap<name, ino>` per thread; at the end compare `handle_readdir` of every directory to the union of the models, and `handle_lookup` of each name. Optional (discretion, valuable for Phase 19): after the run, `commit` then reload and replay (`reload_and_replay` pattern in `write_log_replay_tests.rs:46-65`) and compare to the pre-crash model.

**Modes (one codebase):** `one-file` (N threads, one shared file, block ownership) and `many-files` (each thread many small files, plus cross-thread shared dir). `serialize: bool` and `Config { threads, secs, file_bytes, seed, wal }` from env for the long run. Print the seed on start and in every failure message.

**Short run (in `cargo test`, serialize on):** 4 threads, files <= 4 MiB, total dirty well below `DIRTY_BUDGET` (64 MiB; stay under about 16 MiB to leave margin), ~1,500 ops total. Bound the fsync count (for example 1 op in 25): a durable sync costs about 20 ms per `F_FULLFSYNC` on the macOS dev machine [CITED: docs/research/arena-spikes.md, "F_FULLFSYNC, 8 KiB write (ms) p50 20.6"], and the CI macOS job runs `cargo test --workspace`. Test profile is `dev` (no `[profile]` in the workspace `Cargo.toml`), so hashing and zstd are slow: calibrate with one timed debug run and cut the op count until the test takes about 3-5 s on the slowest CI target. Use `WalConfig::PerOp` (daemon default).

**Long run:** `#[ignore]`, env `STRESS_THREADS`, `STRESS_SECS`, `STRESS_FILE_MIB`, `STRESS_SEED`, `STRESS_SERIALIZE=0|1`; run with `--release`. Every thread loop checks a `deadline` and an `AtomicBool stop`; the joiner uses a bounded join (watchdog that aborts the process after `secs + 60`).

**Serialize-off target test:** `#[ignore]` with a reason string naming Phases 19-21. Also add a **planted-bug self-test that is not ignored**: write a byte behind the model's back (`handle_write` at a model-tracked offset without updating the model), run the final compare, and `assert!` it reports a mismatch. A harness that cannot fail defines nothing.

**Histograms (`hdrhistogram`):** per thread, per op kind: `Histogram::<u64>::new_with_bounds(1, 60_000_000_000, 3)` in nanoseconds, merged at the end with `Histogram::add`; print count, p50, p99, p99.9, max. Coordinated omission is not an issue in a closed loop with no target rate; say so in the report header and do not add correction.

### S1 probe (compiled skeleton)

The skeleton below compiled with `cargo check -p slicefs-cli --example s1_probe_tmp` on this machine (macOS, vendored fuser 0.18) and the scratch file was then removed. It proves: `ReplyEntry`, `ReplyAttr`, `ReplyData` are `Send + 'static` (they are moved through an `mpsc` channel into worker threads; the bound is `pub(crate) trait Reply: Send + 'static`, `vendor/fuser/src/reply.rs:104`); `Request::unique()` is public (`request_param.rs:20`); the default `open` replies `FileHandle(0)` (`lib.rs:576-577`) so no `open` override is needed; `fuser::mount(fs, mountpoint, &Config)` blocks until unmount (`lib.rs:1051`). It records arrival order (`seq`), in-flight count, and reply-order inversions (a reply whose `seq` is lower than one already sent). `lookup` maps the name `slow` to inode 2 (READ delayed 200 ms) and everything else to inode 3.

```rust
// crates/slicefs-cli/examples/s1_probe.rs  — compiled and checked (as s1_probe_tmp)
use std::ffi::OsStr;
use std::sync::atomic::{AtomicU64, Ordering::SeqCst};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};
use fuser::{Config, FileAttr, FileHandle, FileType, Filesystem, Generation, INodeNo, LockOwner, MountOption,
            OpenFlags, ReplyAttr, ReplyData, ReplyEntry, Request, SessionACL};

enum Kind { Lookup(ReplyEntry, u64), Attr(ReplyAttr, u64), Read(ReplyData, bool) }
struct Job { seq: u64, unique: u64, kind: Kind }
#[derive(Default)]
struct Stats { arrived: AtomicU64, inflight: AtomicU64, max_inflight: AtomicU64, last_replied: AtomicU64, inversions: AtomicU64 }
struct Probe { tx: Mutex<Sender<Job>>, st: Arc<Stats>, t0: Instant }

fn attr(ino: u64, kind: FileType) -> FileAttr {
    FileAttr { ino: INodeNo(ino), size: 4096, blocks: 8, atime: UNIX_EPOCH, mtime: UNIX_EPOCH, ctime: UNIX_EPOCH,
        crtime: UNIX_EPOCH, kind, perm: 0o755, nlink: 1, uid: 501, gid: 20, rdev: 0, flags: 0, blksize: 4096 }
}
impl Probe {
    fn submit(&self, req: &Request, kind: Kind) {          // runs on the single fuser reader thread
        let seq = self.st.arrived.fetch_add(1, SeqCst) + 1;
        let n = self.st.inflight.fetch_add(1, SeqCst) + 1;
        self.st.max_inflight.fetch_max(n, SeqCst);
        eprintln!("ARR seq={seq} unique={} inflight={n} t_ms={}", req.unique().0, self.t0.elapsed().as_millis());
        self.tx.lock().unwrap().send(Job { seq, unique: req.unique().0, kind }).unwrap();
    }
}
impl Filesystem for Probe {
    fn lookup(&self, req: &Request, _p: INodeNo, name: &OsStr, reply: ReplyEntry) {
        self.submit(req, Kind::Lookup(reply, if name == "slow" { 2 } else { 3 }));
    }
    fn getattr(&self, req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        self.submit(req, Kind::Attr(reply, ino.0));
    }
    fn read(&self, req: &Request, ino: INodeNo, _fh: FileHandle, _o: u64, _s: u32, _f: OpenFlags,
            _l: Option<LockOwner>, reply: ReplyData) {
        self.submit(req, Kind::Read(reply, ino.0 == 2));
    }
    fn destroy(&mut self) {
        let s = &self.st;
        println!("S1 RESULT arrived={} max_inflight={} reply_order_inversions={}",
                 s.arrived.load(SeqCst), s.max_inflight.load(SeqCst), s.inversions.load(SeqCst));
    }
}
fn worker(rx: Arc<Mutex<Receiver<Job>>>, st: Arc<Stats>, t0: Instant) {
    loop {
        let Ok(job) = rx.lock().unwrap().recv() else { return };
        match job.kind {
            Kind::Read(reply, slow) => { if slow { std::thread::sleep(Duration::from_millis(200)); } reply.data(&[0u8; 4096]); }
            Kind::Lookup(reply, ino) => reply.entry(&Duration::ZERO, &attr(ino, FileType::RegularFile), Generation(0)),
            Kind::Attr(reply, ino) => reply.attr(&Duration::ZERO,
                &attr(ino, if ino == 1 { FileType::Directory } else { FileType::RegularFile })),
        }
        if job.seq < st.last_replied.fetch_max(job.seq, SeqCst) { st.inversions.fetch_add(1, SeqCst); }
        st.inflight.fetch_sub(1, SeqCst);
        eprintln!("REP seq={} unique={} t_ms={}", job.seq, job.unique, t0.elapsed().as_millis());
    }
}
fn main() {
    let (tx, rx) = channel(); let rx = Arc::new(Mutex::new(rx));
    let st = Arc::new(Stats::default()); let t0 = Instant::now();
    for _ in 0..4 { let (rx, st) = (rx.clone(), st.clone()); std::thread::spawn(move || worker(rx, st, t0)); }
    let fs = Probe { tx: Mutex::new(tx), st, t0 };
    let mut cfg = Config::default();
    cfg.mount_options = vec![MountOption::FSName("s1probe".into())];
    #[cfg(target_os = "macos")]
    cfg.mount_options.push(MountOption::CUSTOM("backend=fskit".into()));   // as backend.rs:34 "backend=fskit"
    cfg.acl = SessionACL::Owner;
    let r = fuser::mount(fs, std::env::args().nth(1).expect("mountpoint"), &cfg);
    eprintln!("mount returned {r:?}");
}
```
The probe needs only `fuser`, which is already a regular dependency of `slicefs-cli`, so a plain `examples/` file works: `cargo build --release -p slicefs-cli --example s1_probe`. `cargo test --workspace` also builds examples; the file compiles on Linux (the macOS-only mount option is `cfg`-gated), so it adds no CI risk. The same binary on a Linux Docker mount is a cheap control run that proves the probe can show overlap (libfuse on Linux hands requests to a reader, replies from other threads are legal).

**Client and workflow (S1).** Pattern: `.github/workflows/macos-mounts.yml` [VERIFIED, read in full]: `runs-on: macos-26`, steps `brew install pkg-config`, `brew install --cask macos-fuse-t/cask/fuse-t`, `scripts/macos-mount-test/enable-fskit.sh`, `cargo build --release ...`, a diagnostics step on failure, an upload-artifact step. Triggers there are `workflow_dispatch` and `push: branches: [main, "ci/**"]`.
- **Trigger warning:** a push to `ci/**` already starts the 80-110 min `macos-mounts.yml`. For the S1 workflow use `workflow_dispatch` only, or a different branch pattern such as `ci-s1/**` (not matched by `ci/**`; `[ASSUMED]` from GitHub glob semantics). Do not trigger on `main`: S1 needs no release cadence, and PITFALLS Pitfall 11 says "run S1 by hand, never in automated tests".
- `timeout-minutes: 20` on the job, `permissions: contents: read`, and an upload-artifact step for probe stderr (`ARR`/`REP` lines) and the client log.
- Client `scripts/macos-mount-test/s1-overlap.sh <probe-binary>`: reuse the helpers of `scripts/macos-mount-test/large-copy.sh` (`mounted`, `waitmnt` with a 60 s limit that also checks `kill -0 "$MP"`, `waitexit` 120 s, `run <name> <cmd>` with a kill-based limit, `trap cleanup EXIT`); macOS has no `timeout`, which is why that script builds its own. Steps: mount probe in background; start `dd if=$M/slow of=/dev/null bs=4096 count=1` in the background and note the time; 50 ms later run a `python3` loop that `os.stat`s 50 *new* names `f0..f49` (zero-TTL replies and new names force a server LOOKUP each time) and records each latency in ms; wait for `dd` (limit 10 s); unmount with `umount` then `diskutil unmount force` (macOS commands as in `unmount.rs`: soft unmount, then `diskutil unmount force`); wait for the probe to exit (limit 60 s); print the probe's `S1 RESULT ...` line plus `fast_stats_during_slow=<n>` `max_stat_ms=<x>`.
- **Decision rule to write into the doc:** (a) `max_inflight >= 2` **and** `reply_order_inversions >= 1` **and** the stats completed while the 200 ms READ was pending **and** no client error or hang → FUSE-T/FSKit overlap requests and accept out-of-order replies: the macOS default *may* be > 1 (throughput still to be shown in Phase 21). (b) `max_inflight == 1` (and stats wait about 200 ms) → the transport serializes: macOS default stays 1. (c) overlap but errors, hangs or EIO → pool must not reply out of order on macOS: default stays 1 and the finding goes to Phase 21. State the sentence "the macOS default can / cannot be more than one worker" explicitly (Success Criterion 4).

### S2 / S4 / S6: Linux Docker scripts

**How they run** [VERIFIED: scripts/docker-linux-test/run.sh, bigwrite.sh read in full]: `scripts/docker-linux-test/run.sh bigwrite [size-MiB ...]` builds `slicefs-fusetest` (Dockerfile: `rust:1-bookworm` + `libfuse-dev fuse pkg-config attr`), runs a container with `--device /dev/fuse --cap-add SYS_ADMIN`, inside `unshare -m --propagation private`, builds `--release`, waits until the container is 45 s old (OrbStack mount-hold workaround), then calls `/dk/bigwrite.sh /target/release/slicefs "$@"`. Env passed through: `BW_KEEP`, `BW_QUIET_REF`. `bigwrite.sh` mounts with `--wal-strategy per-op --gc-interval 0` (so GC is off), copies a urandom file, runs `rw.py` (512 MiB of seeded random 1 MiB and 4 KiB writes, fsync every 64 MiB, log per write: start time, latency ms, kind, size, offset), then `match.py` prints `rw: N writes`, `rebase steps`, `pack syncs: n, sum, max`, `commit at ...`, and the slowest writes with the daemon probe lines inside them, and finally `bigwrite <S> MiB: VmHWM after cp ... after random write ... ; writes N, max write X ms, max fsync Y ms; overlay switches; ... ` and `RESULT bigwrite: PASS|FAIL`. The store lives in `/tmp/bw` (overlayfs in the container).

- **S2:** `BW_QUIET_REF=1 scripts/docker-linux-test/run.sh bigwrite 1024 4096`, **at least 3 runs per size, interleaved with a baseline run of `3f3ff34`** if a like-for-like comparison is needed, reporting min / median / max of "max write". Variance is large (commit `736a490`: 1 GiB 127 and 1427 ms; 4 GiB 462 and 766 ms on a btrfs volume at `/tmp/bw`). That earlier spike mounted a btrfs Docker volume at `/tmp/bw`; `run.sh` has no such option. Record `stat -f -c %T /tmp/bw` in the output and either add an optional volume env to `run.sh` or accept overlayfs and say so. Compare against the post-#108 numbers (440 / 686 ms on `90a5443`; 117 / 1542 ms on `3f3ff34`), **not** against 1124 / 1772 ms (see State of the Art). The 500 ms bound of #108 is the reference.
- **S4:** new script `s4-waiting.sh` modelled on `bigwrite.sh` (`mnt`, `umnt`, `step` helpers; `timeout -s KILL`; every wait loop `kill -0 $MP` + a counter). Sampler: a short `python3` loop (python3 is already required by `bigwrite.sh`) reading `/sys/fs/fuse/connections/*/waiting` about every 2 ms while (a) the `rw.py` random-write pass runs on one file and (b) a concurrent `dd if=file of=/dev/null bs=1M` readahead runs; stop when the writer process ends; also a hard `deadline`. Record max and a histogram of the value. **Limits to state in the result:** (1) `waiting` is `fuse_chan_num_waiting(fc->chan)` for the whole connection, not per inode [CITED: kernel `fs/fuse/control.c`, `fuse_conn_waiting_read`: `value = fuse_chan_num_waiting(fc->chan);`] — it equals per-inode overlap only when exactly one file is active, so run one active file only; the exact meaning of "waiting" (requests accepted and not yet answered) is `[ASSUMED]` — verify with a sanity run where one op is slowed (a `kill -STOP` of the daemon for 1 s while a client does one `stat` should show `waiting >= 1`). (2) Whether `fusectl` is mounted inside the container is **not observed**; the script's first step must probe `mountpoint -q /sys/fs/fuse/connections || mount -t fusectl none /sys/fs/fuse/connections` (SYS_ADMIN is granted) and fail with a clear message if the directory stays empty. (3) With the daemon's one dispatch thread, requests never overlap *in the daemon*, so only kernel-side queue depth is observable. Current mount options request only `FUSE_ATOMIC_O_TRUNC` and `FUSE_BIG_WRITES` (`fuse_callbacks.rs` `init`), fuser sets `FUSE_ASYNC_READ`, `max_background` 16 (`vendor/fuser/src/lib.rs:107, 229`) [VERIFIED].
- **S6:** new script `s6-smallfsync.sh`: mount with `--wal-strategy per-op`, then `flush-on-fsync`, then `periodic`, each time create and fsync 2,000 small files (python3 `os.open/os.write/os.fsync/os.close`, per-op latency into a list, report fsyncs/s, p50, p99); unmount and verify the daemon exits (bigwrite's `umnt`). Note there is no flusher yet: "PerOp vs flusher" can only be "PerOp vs buffered strategies" now; group-commit benefit is a Phase-21+ question (REQUIREMENTS line 58: "WAL group commit (spike S6 decides)" is deferred). An in-process version also runs on macOS with no mount (`handle_create/write/fsync/release` loop in the harness file, `#[ignore]`, `WalConfig` as a parameter); do that for the macOS numbers, because local macOS mounts are not allowed.

### S3 / S5 / S7: harness spikes

| Spike | Mode (today's code) | What runs | Report |
|-------|--------------------|-----------|--------|
| S3 | serialize **off**, safe subset: N in {1,2,4,8} threads, each its **own** file (own handle), read/write only after setup, dirty total below `DIRTY_BUDGET` | MB/s per N; `lock-stats` hold/wait for `store`, `io`, `readers`, `open_files`, `wal`; model check must pass (this *proves* the subset is safe: `[ASSUMED]` until the first run) | Expect `readers` wait to grow with N (guard held across `Reader::read`); decides default N and per-worker packs |
| S5 | gate lock behaviour, not file state: synthetic holders `gate.read(); sleep(hold 50-500 ms)` on 4 threads + real ops under serialize + a commit thread calling `timed_commit` every 1 s on a store with many files (create ~78k empty files with `handle_create` + `handle_release`, by hand only) | per-thread acquire-wait of new shared holders while a commit waits (max, p99); `CommitTimes.gate_wait`, `hold`, `total` | Lock type and whether the generation switch is already enough |
| S7 | serialize **on** (keep_budget crosses files, unsafe otherwise): 4 writer threads, each on its own 1 GiB file, bigwrite mix (1 MiB + 256 x 4 KiB random), `ru_maxrss` via `libc::getrusage` | peak RSS vs the 256 MiB cap (`CAP_MIB` in bigwrite.sh), slowest write per thread, rebase count (`test_rebase_count`), `test_dirty_total` | Baseline only: flusher lag and pause curve cannot be measured before Phase 20 |

**S5 finding already available from the std source** (cited, answers the doc's "not verified" statement): Rust's `std::sync::RwLock` blocks new readers once a writer is waiting, on both platforms.
- **Linux** (`sys/sync/rwlock/futex.rs`, selected for `target_os = "linux"` in `rwlock/mod.rs`): `fn is_read_lockable(state)` is `state & MASK < MAX_READERS && !has_readers_waiting(state) && !has_writers_waiting(state)`; comment: "We don't allow read-locking if there's readers waiting, even if the lock is unlocked and there's no writers waiting. The only situation when this happens is after unlocking, at which point the unlocking thread might be waking up writers, which have priority over readers." `read()` goes to `read_contended()` when `!is_read_lockable(state)`.
- **macOS** (`target_family = "unix"` falls to `queue.rs`; macOS is not in the futex list in `rwlock/mod.rs`): `fn read_lock(state)` returns `Some(..)` only `if state.addr() & QUEUED == 0 && state.addr() != LOCKED`, i.e. no new reader while any thread is queued; module doc: "writer-preferring, even if some readers may still slip through" and "unfair".
- Both are from the local toolchain `rustc 1.95.0-nightly (9e79395f9 2026-02-10)` `library/std` source [VERIFIED: read from `$(rustc --print sysroot)/lib/rustlib/src/rust/library/std/src/sys/sync/rwlock/`]. A stable toolchain at a different date may differ; the S5 measurement shows the behaviour on the toolchain actually used. Consequence for the design (carried to Phase 20): the rule "release the gate before any wait for budget or durability" is required, not optional.

### Don't Hand-Roll

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|-------------|-----|
| Latency percentiles | Own histogram/percentile code in the harness | `hdrhistogram` 7.6 | Locked by CONTEXT; correct quantiles, merge |
| Content for the model | Store expected bytes per file | Seed-pure `bytes_of(seed, ..)` (copy from `dirty_budget_tests.rs:82-95`) | Memory bound (#98) and RSS honesty in S7 |
| Mount wait/unmount loops with limits | New shell helpers | Copy `waitmnt`/`waitexit`/`run` from `scripts/macos-mount-test/large-copy.sh` and `mnt`/`umnt` from `bigwrite.sh` | Already enforce "exit when the process ends" and time limits |
| Commit under load | Own commit loop | `commit_timer::timed_commit(&fs, &gate)` / `spawn_commit_timer` | Returns gate wait/hold; same code as the daemon |
| Peak RSS in-process | Parsing `/proc` or `ps` per platform | `libc::getrusage(RUSAGE_SELF)` (`ru_maxrss`: KiB Linux, bytes macOS) | Cross-platform; `ps -o rss=` is what `small_file_bench.rs` does, but it is a snapshot, not a peak |
| Overlap detection for S1 | Timing-only inference | Server-side `inflight`/`max_inflight`/`unique` + arrival/reply `seq` | Server arrival is authoritative; client timing alone can't tell client-side serialization from transport serialization |

## Common Pitfalls

### Pitfall 1: Spike on a stale baseline
**What goes wrong:** S2 ("remove the inline pack fsync") and S5 ("1.3 s exclusive commit") test problems #108 already fixed, and a result "no improvement" is read as a failure.
**How to avoid:** Mark old numbers `[SUPERSEDED by ...]` in the doc (table in State of the Art) and compare to post-#108 numbers. First task: re-run S2 and S5 baselines.
**Warning signs:** a result that says "the pack fsync is not under the store lock" (it is not: #131).

### Pitfall 2: A harness that cannot fail
**What goes wrong:** A model check that compares the fs to itself, or a short run that never overlaps, passes forever. PITFALLS.md Pitfall 10: "A max-in-flight counter that never exceeds 1 in a 'multithreaded' test".
**How to avoid:** the planted-bug self-test (not `#[ignore]`); in serialize-off runs count the maximum number of ops in flight (an `AtomicUsize` around each op) and assert it exceeds 1 in the serialize-off test and equals 1 in serialize-on.

### Pitfall 3: Serialize mutex held around the gate hold
**What goes wrong:** If the harness takes the serialize mutex *inside* `gate.read()`, gate hold times include queueing for the harness mutex and S5 looks terrible. **How to avoid:** take the serialize mutex *before* `gate.read()`; measure gate wait and hold separately from op latency.

### Pitfall 4: Going over the dirty budget in a serialize-off run
**What goes wrong:** `keep_budget` takes another file's state out of `open_files` and a concurrent op on that handle gets EBADF. **How to avoid:** keep the total of dirty bytes (threads x files x write volume between releases) well under `DIRTY_BUDGET` for every unserialized run; assert `fs.test_dirty_total() < DIRTY_BUDGET` in the harness; S7 stays serialized.

### Pitfall 5: Unbounded waits
**What goes wrong:** a wedged FUSE-T mount or a deadlocked harness hangs CI for hours. **How to avoid:** every shell wait has a deadline *and* `kill -0 $pid` (the `large-copy.sh` pattern); job `timeout-minutes`; harness watchdog thread that `std::process::abort()`s after the limit; client `dd` limit.

### Pitfall 6: Timing the wrong lock or the wrong thing
**What goes wrong:** Reporting only `io` and concluding "store lock is cold". **How to avoid:** label both `io` and `store` and name the one the doc calls "store lock" (`ChunkStore.inner`).

### Pitfall 7: Time-based flakiness of S1 on the CI VM
**What goes wrong:** a stat that happens to finish after the 200 ms READ is read as "serialized". **How to avoid:** use the server-side `max_inflight` and `reply_order_inversions` as the primary result; repeat the pair 5 times in one run; client latency is secondary.

### Pitfall 8: `ru_maxrss` units
`getrusage` returns KiB on Linux and **bytes** on macOS; normalize in one helper.

### Pitfall 9: `cargo test --workspace` compiles examples on Linux
The S1 example must compile on Linux (no macOS-only symbol outside `cfg(target_os = "macos")`). The compiled skeleton uses only the macOS-gated `MountOption::CUSTOM(...)` push.

## Code Examples

### Thread-local histograms merged at the end (verified to compile as a scratch crate)
```rust
// Source: hdrhistogram 7.6 (default-features = false) — scratch crate build this session
use hdrhistogram::Histogram;
let mut h = Histogram::<u64>::new_with_bounds(1, 60_000_000_000, 3).unwrap(); // ns, 3 sig. digits
h.record(1500).unwrap();
let p99 = h.value_at_quantile(0.99);
// per thread: one Histogram per op kind; at the end: total.add(&thread_hist).unwrap();
```

### One harness operation with gate timing (pattern, matching fuse_callbacks.rs)
```rust
// serialize mutex first, then the gate (Pitfall 3)
let _s = cfg.serialize.then(|| serial.lock().unwrap());
let w = Instant::now();
let gate = fs.commit_gate();
let _g = gate.read().unwrap_or_else(|e| e.into_inner());   // as fuse_callbacks.rs:47-48
gate_wait.record(w.elapsed().as_nanos() as u64).ok();
let t = Instant::now();
let r = slicefs_cli::handlers::handle_write(&fs, fh, offset, &data);
lat[Op::Write].record(t.elapsed().as_nanos() as u64).ok();
// _g dropped here: gate hold = time from acquire to drop (record in a Drop guard)
```

### Commit thread for S5 (existing API)
```rust
// Source: crates/slicefs-cli/src/commit_timer.rs:41-56
let times = slicefs_cli::commit_timer::timed_commit(&fs, &gate)?; // CommitTimes { gate_wait, hold, total }
```

## State of the Art

Baselines in `docs/research/multithreaded-request-handling.md` that no longer hold (mark `[SUPERSEDED]` in the doc; the doc is pinned to commit `6fd1e11` per PITFALLS.md):

| Old statement in the doc | Status now | Evidence |
|--------------------------|-----------|----------|
| Slowest single write 1124 / 1772 ms at 1 / 4 GiB (PR #122) | **Superseded**: 440 / 686 ms on `90a5443`; 117 / 1542 ms on `3f3ff34` (4 GiB tail = writer waiting at `KEEP_CAP` for a 2 s VM disk sync); early-writeback spike dropped | `.planning/milestones/v2.1-MILESTONE-AUDIT.md` ledger row 14 (line 126 and the earlier edit in commit `97bf4a5`); commit `736a490` (`SPIKE-108-early-writeback.md` lives in that commit only, not in the working tree: cite the commit, not a path) |
| "Every write above 400 ms now contains a pack fsync of 0.35-1.7 s", inline under the store lock | **Superseded** for the lock: pack sync is a background sync with a RAM cap (`KEEP_LIMIT` 8 MiB, `KEEP_CAP` 24 MiB); the writer still waits when the cap is reached | `retry.rs:9-17, 33-37` (commits `6935fa1`, `bb9fa98`, `48b1921`) |
| "A commit holds the gate exclusively for the whole commit, about 1.3 s at 78k files" | **Superseded**: the gate is held only for the generation cut ("about 28 ms of a 470 ms commit") | `commit_timer.rs:9-14`, `timed_commit` 41-56 (commit `edd02e0`, #133); audit ledger line 126 |
| `flush_wal` holds the `wal` mutex during `flush_and_sync` | **Superseded** for `flush_wal` (clones a handle) but **still true** for the append (`store.rs:514-523`) and for per-strategy `writer` mutexes (`per_op.rs:81`, `buffered.rs:98-111`) | `store.rs:453-463, 654-657` |
| `read_root` holds the `readers` mutex across `Reader::read` | **Still true** (`filesystem.rs:979-989`); step-1 item "narrow the `readers` lock" is not done | read this session |
| Step 7 "gate generation switch if S5 shows stalls" | Already shipped as #133; S5 now decides the gate *lock type*, not the switch | as above |
| "An older blockset-era macOS measurement: SliceFS's own write+fsync about one third of the copy time" | Not measured for the pack store; stays marked as old | doc, risk 3 |
| Doc "Migration in eight steps" numbers steps **0-7**; CONTEXT says "steps 1-8" | Use the doc's numbering in every `**Decides:**` line | doc table, lines 78-87 |

Step-to-phase mapping for the `Decides` lines: step 0 = this phase; step 1 = largely done (background pack sync #131, gate cut #133, `flush_wal` handle); the `readers` narrowing and WAL append/sync locks remain open; steps 2-4 = Phase 19; step 5 = Phase 20; step 6 = Phase 21; step 7 = deferred (REQUIREMENTS: WAL group commit, spike S6 decides).

**Spike → decided step** (each result section ends with `**Decides:** step N (...)`):
S1 → 6 (macOS default N; POOL-04). S2 → 1 (is #108 closed without threads: yes, per post-#108 data, re-confirm). S3 → 6 (default N) and per-worker packs (REQUIREMENTS line 59, deferred). S4 → 2 (per-inode mutex contention) and the `FUSE_WRITEBACK_CACHE` trial (REQUIREMENTS line 60, deferred). S5 → 7 / gate lock type (feeds step 5 and 6 rule "release the gate before waiting"). S6 → 7 priority. S7 → 5 (thresholds; partly open until Phase 20, say so).

## Assumptions Log

| # | Claim | Section | Risk if Wrong |
|---|-------|---------|---------------|
| A1 | Docker `rust:1-bookworm` and the macOS runner toolchains are >= Rust 1.88 (hdrhistogram 7.6 MSRV) | Standard Stack | `cargo test --workspace` fails in CI at dependency resolution; fix: pin `hdrhistogram` 7.5 or update toolchain |
| A2 | S3's "distinct files, no namespace ops, below budget" subset is safe without serialize on today's code | S3 | Model check fails on first run; then S3 must run serialized (no contention data) — a finding, not a blocker |
| A3 | `/sys/fs/fuse/connections/*/waiting` counts requests accepted and not yet answered for the connection | S4 | S4 numbers mean something else; the planned `kill -STOP` sanity run checks it |
| A4 | `fusectl` can be mounted in the container (SYS_ADMIN granted) | S4 | S4 needs a different container flag (`--privileged`); script probes and fails loudly |
| A5 | A branch pattern `ci-s1/**` does not match the `ci/**` filter of `macos-mounts.yml` | S1 workflow | A push also starts the 80-110 min job; use `workflow_dispatch` only to be safe |
| A6 | `File::sync_all` costs about 20 ms per call on the macOS CI runner (as on the dev machine in `arena-spikes.md`) | Harness short run | Short run is slower than a few seconds on macOS CI; cap fsync count and calibrate |
| A7 | std `RwLock` behaviour on a stable toolchain matches the 2026-02-10 nightly source read here | S5 | S5 measurement shows the real behaviour anyway |
| A8 | The three SliceFS WAL strategies, not just `PerOp`, matter for S6 numbers on macOS | S6 | None for correctness |

## Open Questions

1. **Should `lock-stats` be in CI?**
   - Known: CI runs `cargo test --workspace` without features.
   - Recommendation: no CI step; short run passes with the feature off; a `#[cfg(feature = "lock-stats")]` test runs by hand and in the phase verification.
2. **S2 storage under `/tmp/bw`.** Overlayfs by default; the earlier #108 spike used a btrfs volume (different tails). Recommendation: record the fs type in every result; add an optional volume env to `run.sh` only if the user wants like-for-like numbers.
3. **S7 and S3 before Phase 20/21.** Free-thread S3 is a safe subset only; S7 can only be a serialized baseline. Recommendation: write the results as "baseline; re-run with serialize off after Phase 20" and state which part of the decision remains open. This is honest and does not block the phase.
4. **Replay-equality check in the harness** (crash-replay compare). Valuable for Phase 19 (log order), cheap in serialize mode. Recommendation: include as an optional end-of-run step if time allows; Claude's discretion.
5. **S1 run frequency.** One manual dispatch is enough for the decision; the probe can be re-run after a FUSE-T upgrade (PITFALLS Pitfall 11).

## Environment Availability

| Dependency | Required By | Available | Version | Fallback |
|------------|------------|-----------|---------|----------|
| Rust toolchain | all code | yes | rustc 1.95.0-nightly (2026-02-10); needs `export PATH="$HOME/.cargo/bin:$PATH"` | — |
| Docker (OrbStack context) | S2, S4, S6 | yes | client 29.8.2, context `orbstack` | — |
| python3 | scripts, S1 client | yes (local 3.9.6; container image provides it, as `bigwrite.sh` already needs it) | 3.9.6 local | — |
| fio | not needed | no | — | not required (STACK.md: no fio for these spikes) |
| FUSE-T + FSKit module | S1 | not used locally (no local macOS mounts) | CI only (`macos-26`) | — |
| crates.io access | `hdrhistogram` | yes | 7.6.0 downloaded and built | — |

**Missing dependencies with no fallback:** none for planning. **Not probed (by design):** Docker daemon state beyond `docker info` client output; macOS runner toolchain.

## Validation Architecture

`workflow.nyquist_validation` is `true` in `.planning/config.json`.

### Test Framework
| Property | Value |
|----------|-------|
| Framework | `cargo test` (built-in), integration tests in `crates/slicefs-cli/tests/` |
| Config file | none; new `[features] lock-stats` and dev-dependency `hdrhistogram` in `crates/slicefs-cli/Cargo.toml` |
| Quick run command | `export PATH="$HOME/.cargo/bin:$PATH"; cargo test -p slicefs-cli --test stress_harness` (target: a few seconds) |
| Quick run with timing | `cargo test -p slicefs-cli --features lock-stats --test stress_harness -- --nocapture` |
| Full suite command | `cargo test --workspace` (what CI and `run.sh test` run); plus `cargo build --release -p slicefs-cli --example s1_probe` |
| Long run (by hand) | `STRESS_THREADS=8 STRESS_SECS=300 STRESS_FILE_MIB=1024 cargo test --release -p slicefs-cli --features lock-stats --test stress_harness -- --ignored --nocapture long_run` |

### Phase Requirements → Test Map
| Req ID | Behavior | Test Type | Automated Command | File Exists? |
|--------|----------|-----------|-------------------|-------------|
| SPIKE-02 | Mixed ops on one file and on many files, N threads, serialize on, data equals model | integration | `cargo test -p slicefs-cli --test stress_harness short_` | ❌ Wave 0 |
| SPIKE-02 | Model check detects a planted divergence | integration | `cargo test -p slicefs-cli --test stress_harness model_detects_planted_bug` | ❌ Wave 0 |
| SPIKE-02 | Latency histograms and lock hold/wait are reported; all six site labels present with the feature on | integration | `cargo test -p slicefs-cli --features lock-stats --test stress_harness lock_stats_report` | ❌ Wave 0 |
| SPIKE-02 | Short run and long run exist; long run and serialize-off target are `#[ignore]` | integration | `cargo test -p slicefs-cli --test stress_harness -- --list \| grep -c ignored` | ❌ Wave 0 |
| SPIKE-02 | Product build unchanged with the feature off | build | `cargo check --workspace --all-targets` | ✅ |
| SPIKE-01 | Each of S1-S7 has a result section naming a step | doc check | `for s in 1 2 3 4 5 6 7; do grep -c "^### S$s result" docs/research/multithreaded-request-handling.md; done` (each prints 1) and `grep -c '^\*\*Decides:\*\* step [0-7]' docs/research/multithreaded-request-handling.md` (prints 7) | ❌ Wave 0 (doc edit) |
| SPIKE-01 | Superseded baselines marked | doc check | `grep -c "SUPERSEDED" docs/research/multithreaded-request-handling.md` (>= 3) | ❌ Wave 0 |
| SPIKE-01 | S2 and S5 numbers recorded with run count and fs type | manual-only (needs Docker, minutes) | `scripts/docker-linux-test/run.sh bigwrite 1024 4096` x3; S5 via the `#[ignore]` S5 test | ❌ by hand |
| SPIKE-03 | Probe builds on Linux and macOS | build | `cargo build --release -p slicefs-cli --example s1_probe` | ❌ Wave 0 |
| SPIKE-03 | S1 run on FSKit with in-flight and reply order recorded | manual / CI dispatch | `gh workflow run s1-overlap.yml` then read the `S1 RESULT arrived=.. max_inflight=.. reply_order_inversions=..` line in the log artifact | ❌ by hand |
| SPIKE-03 | Doc states whether the macOS default can be more than one worker | doc check | `grep -n "macOS default" docs/research/multithreaded-request-handling.md` shows a sentence with "can" or "cannot" | ❌ Wave 0 (doc edit) |

Manual-only items are justified: S1 needs a real FSKit mount (CI by dispatch only); S2/S4/S6 need Docker minutes and quiet disks, and the test-time budget forbids them in the suite.

### Sampling Rate
- **Per task commit:** `cargo test -p slicefs-cli --test stress_harness` (and `cargo check -p slicefs-cli --features lock-stats` after touching a lock site).
- **Per wave merge:** `cargo test --workspace` and `cargo check --workspace --all-targets`; with the feature: `cargo test -p slicefs-cli --features lock-stats --test stress_harness`.
- **Phase gate:** full suite green; doc greps above pass; S1 workflow run linked from the doc; then `/gsd-verify-work`.

### Wave 0 Gaps
- [ ] `crates/slicefs-cli/tests/stress_harness.rs` — SPIKE-02 (short run, model, planted bug, ignored long run, serialize-off ignored target, S3/S5/S6/S7 `#[ignore]` entry tests)
- [ ] `crates/chunk-store/src/lock_stats.rs` + `[features]` in three `Cargo.toml` files + site conversion (store 19, io 16, open_files 36, readers 2, wal 1, WAL strategies about 13)
- [ ] `hdrhistogram` dev-dependency (`default-features = false`), `Cargo.lock` update
- [ ] `crates/slicefs-cli/examples/s1_probe.rs`, `scripts/macos-mount-test/s1-overlap.sh`, `.github/workflows/s1-overlap.yml`
- [ ] `scripts/docker-linux-test/s4-waiting.sh`, `s6-smallfsync.sh` (+ `run.sh` modes `s4`, `s6`)
- [ ] Doc: `## Spike results (Phase 18)` with `### S1 result` ... `### S7 result`, each with a `**Decides:** step N` line, superseded markers

## Security Domain

`security_enforcement` is not set in config (absent = enabled). This phase adds test code, scripts, a CI workflow and a feature-gated timing recorder; no new network surface, no data at rest, no user input parsing.

### Applicable ASVS Categories

| ASVS Category | Applies | Standard Control |
|---------------|---------|-----------------|
| V2 Authentication | no | — |
| V3 Session Management | no | — |
| V4 Access Control | no (probe mounts with `SessionACL::Owner`, as `mount.rs` does for the default) | keep `Owner`, never `allow_other` |
| V5 Input Validation | minimal | env vars of the long run parsed with explicit bounds; shell scripts quote paths and use `set -u`-safe patterns |
| V6 Cryptography | no | — |
| V14 Configuration / CI | yes | workflow `permissions: contents: read`, no secrets, no `pull_request_target`; `timeout-minutes`; third-party action pins as in `macos-mounts.yml` (`actions/checkout@v4`, `actions/cache@v4`, `actions/upload-artifact@v4`) |

### Known Threat Patterns

| Pattern | STRIDE | Standard Mitigation |
|---------|--------|---------------------|
| Runaway CI job / hung mount (DoS of runner minutes) | Denial of service | job and step time limits; every script wait exits on process end |
| Supply chain: new crate `hdrhistogram` | Tampering | legitimacy check OK; dev-dependency only (not in release binary); `Cargo.lock` committed |
| Workflow triggered by a branch push by a fork | Elevation of privilege | `workflow_dispatch` only (or same-repo branch pattern); read-only token |

## Sources

### Primary (HIGH confidence — files read this session)
- `docs/research/multithreaded-request-handling.md` (S1-S7, steps 0-7, baselines)
- `crates/slicefs-cli/src/{filesystem.rs,handlers.rs,fuse_callbacks.rs,commit_timer.rs,lib.rs,backend.rs,mount.rs,oplog.rs,unmount.rs}`, `crates/metadata/src/{store.rs,wal/per_op.rs,wal/buffered.rs,wal/mod.rs}`, `crates/chunk-store/src/{lib.rs,retry.rs}`, `vendor/fuser/src/{lib.rs,reply.rs,channel.rs,session.rs,request_param.rs}`, `vendor/fuser/PATCHES.md`
- `crates/slicefs-cli/tests/{dirty_budget_tests.rs,write_log_replay_tests.rs,small_file_bench.rs}`
- `scripts/docker-linux-test/{run.sh,bigwrite.sh,mt-linux.sh,Dockerfile}`, `scripts/macos-mount-test/{large-copy.sh,enable-fskit.sh,README.md}`, `.github/workflows/{macos-mounts.yml,ci.yml}`
- `.planning/{STATE.md,ROADMAP.md,REQUIREMENTS.md,research/STACK.md,research/PITFALLS.md}`, `.planning/milestones/v2.1-MILESTONE-AUDIT.md` (#108 ledger), git commit `736a490` (early-writeback spike results), commits `edd02e0`, `bb9fa98`, `48b1921`, `6935fa1`
- Rust std source (local toolchain 1.95.0-nightly 2026-02-10): `sys/sync/rwlock/{mod.rs,futex.rs,queue.rs}`
- `cargo info hdrhistogram`, crates.io API `hdrhistogram/7.6.0`, `gsd-tools query package-legitimacy check`, scratch-crate build of `hdrhistogram` with `default-features = false`
- Compile checks: `cargo check -p slicefs-cli --example <scratch>` for the S1 skeleton and for `SliceFsFilesystem: Send + Sync + 'static` (scratch files removed; git tree clean)

### Secondary (MEDIUM confidence)
- Linux kernel `fs/fuse/control.c` (`fuse_conn_waiting_read`, files `waiting`, `abort`, `max_background`, `congestion_threshold`) via raw.githubusercontent.com [CITED]; meaning of `fuse_chan_num_waiting` not in that file (A3)
- `docs/research/arena-spikes.md` (macOS `F_FULLFSYNC` p50 20.6 ms, dev machine under load)

### Tertiary (LOW confidence)
- Behaviour of FUSE-T/FSKit under overlap: nothing measured; Apple DTS forum text quoted in the research doc is not official documentation. S1 settles it.

## Metadata

**Confidence breakdown:**
- Standard stack: HIGH — crate verified, builds, lockfile delta known
- Architecture / lock map: HIGH — read from the current tree with line numbers
- S1/macOS behaviour: LOW until the spike runs (that is the deliverable)
- Pitfalls: HIGH for code-derived items, MEDIUM for CI-trigger and fusectl assumptions (A4, A5)

**Research date:** 2026-11-20
**Valid until:** 2026-12-20 (code moves quickly; re-check line numbers before editing lock sites)
