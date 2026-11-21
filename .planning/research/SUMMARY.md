# Project Research Summary

**Project:** SliceFS, milestone v2.2 "Concurrency & Store Evolution" (issues #127, #21, #22, #62, #67, #19)
**Domain:** Deduplicating POSIX FUSE filesystem in Rust (FastCDC + BLAKE3 chunk store, write log, online GC)
**Researched:** 2026-11-20
**Confidence:** MEDIUM-HIGH (code facts HIGH; macOS FUSE-T/FSKit concurrency LOW, no result exists)

This file replaces the v2.0 summary. Detail is in STACK.md, FEATURES.md, ARCHITECTURE.md and PITFALLS.md in this folder.

## Executive Summary

v2.2 is not a greenfield build. It changes a working single-dispatch-thread daemon in four independent ways: (1) make request handling correct and fast under N threads (#127 spikes, #21 fine-grained locks, #22 worker pool); (2) add an offline `slicefs rehash` that moves a store to another write hash (#62); (3) make the FastCDC min/avg/max a per-store profile (#67); (4) evaluate prolly trees for the metadata maps (#19). Experts build (1) the way the kernel and libfuse do: one lock rank (gate, rename, namespace, per-file, short maps, store internals), state that never leaves its map, log append inside the same locks as the apply, writers that pay by sleeping before they take locks, and a stress harness first. Comparable systems (git, restic, borg) do (2) and (3) by copy-and-switch with parameters fixed at creation. (4) is an evaluation, and its biggest finding is that half of it is already built (file manifests are content-cut trees).

Recommended approach: a serial lock track and two parallel tracks. Track A: stress harness and re-baselined spikes, then a dispatcher with N workers behind one global handler mutex (so S1 and S4 are measurable early), then narrow global locks, `FileCell` table, log-under-lock plus replay-by-logged-inode, namespace locks with a debug rank checker, rebase flusher with dirty budget, then remove the global mutex. Default N stays 1 everywhere and rises on Linux first. Track B: `ChunkProfile` plumbing and one marker change, then rehash (rebuild manifests bottom-up from chunk sequences, never node-for-node). Track C: a prolly prototype crate and benchmark, not wired into the root record. The stack delta is small: `parking_lot` 0.12 (one copy with fuser), `arc-swap` only at first use, `shuttle` and `hdrhistogram` dev/bench only. No `rayon`, `dashmap`, `crossbeam`, `tokio`, `criterion` or prolly crate.

Key risks: (a) designing against a stale baseline, since #108 already landed the pack-fsync move and the gate generation switch and #21's text no longer matches the code; (b) deadlocks from lock inversion between the gate, the budget wait and a new flusher thread; (c) rehash correctness (cut points move with the hash, GC can sweep unreachable new objects, corruption can be laundered); (d) a process-wide `zero_chunk` static that makes any non-default chunk profile a data-corruption bug; (e) the pool enabled on macOS with no S1 result. Each has a concrete prevention in the phases below.

## Decisions Needed From the Owner

Each has a research recommendation; none is settled by research alone.

1. **Second production hash for rehash (or test-only target).** A release build registers only `blake3-256`; `TestKeyedBlake3` (tag `0xf0`) exists only under `cfg(test)` or `test-hasher`. #62 puts the choice out of scope. Without a real second algorithm, `rehash` ships fully tested but has no user-visible target. This is a one-way door for IDs on disk. Recommendation: ship v2.2 rehash with the test hasher as the only target and raise the production-hash choice as its own decision. If the owner picks one now, it needs one `ObjectHasher` impl and at most one crate.
2. **Worker-panic policy.** `filesystem.rs` has about 59 `lock().unwrap()` sites. Under N workers one panic poisons shared locks and cascades EIO over half-updated state. Recommendation: a worker panic aborts the process (panic hook calling abort, or `panic = "abort"` for the daemon), and write-log replay is the recovery path. The alternative in STACK.md (`parking_lot`, `catch_unwind`, EIO for one request) keeps the mount up but serves requests after a panic inside a critical section. This is hard to reverse once clients depend on it, so it needs owner confirmation.
3. **Conflict on log append under the inode lock (#21 vs the research doc).** #21 says never hold a lock during disk I/O, and to append to the log first and then take the inode lock to publish the extent. The research doc and ARCHITECTURE.md say to apply, then append while the inode lock is held, so replay order equals apply order (replay checks `got == *ino` for creates). Both cannot be built. Recommendation: the research-doc rule, where the append under the lock is only a memory copy plus an LSN, with the disk write and sync on the flusher or group commit. Mark #21's table superseded, update the issue body, and confirm with the stress harness.
4. **Chunk profile fixed at store creation.** Recommendation: `seed --chunk-size MIN:AVG:MAX`, persisted in the `FORMAT` marker (absent line means 4/8/32 KiB), and rejected on any change at mount. Changing it on a populated store breaks dedup and the splice "key equals full write" guarantee. That is a separate rechunk job, out of scope. The default stays 4/8/32 KiB; the owner decides any default change after the benchmark numbers.
5. **Rehash offline-only in v2.2.** Refuse while mounted, and refuse with a write-log tail after the last root. Online rehash needs a GC protocol for unreachable new objects and depends on the per-inode state work. Deferred to v2.3+.
6. **Prolly work is a prototype plus a written decision, no migration.** `crates/prolly` plus `benchmarks/prolly-bench`, not wired into `root_rec`, no format bump. Adopt, adopt later or drop is decided from the numbers. Any migration is its own milestone and needs #62-style tooling.

Smaller items to raise: #22's wording ("configure fuser with more than one worker thread") should become "SliceFS worker pool, N by mount option", because the vendored fuser refuses `n_threads` outside Linux. A rebase onto fuser 0.19.0 (released 2026-11-20) belongs in a separate issue after #22.

## Key Findings

### Recommended Stack (delta only)

The existing stack is unchanged. v2.2 adds almost nothing, and most obvious crates are refused with reasons (STACK.md).

**Core technologies:**
- `parking_lot` 0.12 with `arc_lock`: no poisoning, `deadlock_detection` for stress builds, owned guards for two-inode locking. Use 0.12, not 0.13, so the tree keeps one copy shared with fuser and the toolchain stays put.
- `arc-swap` 1.9: only at first use, for read-mostly immutable snapshots. The chunk-store read view is the strongest candidate, after S3 shows read-side wait.
- `fastcdc` 3.2.1 kept: run-time min/avg/max is already supported. Validate in a `ChunkProfile::new` constructor (powers of two, `min <= avg <= max`, inside crate limits), use seed 0, and add a golden cut-point test before any upgrade.
- `shuttle` 0.9 (dev-only, `cfg(shuttle)`) behind a thin `crate::sync` shim, for the gate + budget + flusher + durable_lsn protocol. `hdrhistogram` 7.6 in the new stress bench only.
- Own code instead of crates: `ShardedTable` (no `dashmap`), a byte-bounded `RequestQueue` over Mutex + Condvar (no `crossbeam`), `std::thread::scope` over packs for rehash (no `rayon`), and a hand-written prolly prototype on chunk-store objects. `prolly-map` is refused because its SHA-256 node IDs conflict with tagged IDs, GC and rehash.
- Do not use fuser `n_threads` or `clone_fd`; stay on vendored 0.18.0.

The commit gate should be a small SliceFS type with an explicit admission rule, not a library `RwLock`, because writer-preferring fairness is exactly the deadlock path. S5 decides the final form.

### Expected Features

**Must have (table stakes):**
- Stress harness (N threads, mixed ops, latency histograms, lock-hold timers) and re-baselined spikes. Everything else depends on it.
- #21: `FileCell` state that never leaves the map; lock rank plus debug checker; log under lock with replay by logged inode number; narrowed `readers`, WAL and store locks; read-while-write; rebase flusher with a global dirty budget (writer pays by sleeping, never waits while holding gate or inode lock).
- #22: one reader plus a bounded worker pool, N by mount option, default 1, inline at N=1, drain before `destroy()`.
- #62: offline rehash with a durable or bounded old-to-new mapping, manifest trees rebuilt from the `(ID, length)` list, directory-entry keys untouched, a crash-safe single switch, progress, `--dry-run`, and tests with the test hasher.
- #67: store-wide profile in the marker, every `CHUNK_*` use reading it, benchmark scripts and a report for 4/8/32, 8/16/64, 8/32/128, 16/64/256 and 32/128/512 KiB, default kept.
- #19: one-map prototype, benchmark against current code, written decision.

**Should have (conditional on spikes):**
- macOS pool above N=1 (S1), WAL group commit (S6), per-worker packs (S3), `FUSE_WRITEBACK_CACHE` (S4), persistent extent map via `imbl` (S4). Rehash resume and verify-after-copy if the first long run needs them.

**Defer (v2.3+):**
- Online rehash with online GC, prolly metadata migration and snapshot diff, rechunk tool, real production second hash (if not decided now), `getlk`/`setlk` correctness (separate issue), fuser 0.19 rebase.

### Architecture Approach

A thin FUSE shell hands copied args and the `Reply` to a `Dispatcher`. The worker (not the reader) takes the gate shared and runs the handler. The handler locks namespace stripes in ascending order, then the `FileCell`, applies, appends the log record, unlocks, and replies after all guards are dropped. Lock rank: gate, rename mutex, `NamespaceLocks` stripes, `FileCell`, short maps, metadata/WAL/chunk-store internals. Budget, durability and commit waits happen before any lock is taken. The exclusive-gate holders (commit cut, GC start) may lock every cell because every other holder has the gate shared.

Rehash lives in `metadata`, because node-kind wire formats are private there. It shares one `node_graph.rs` enumeration with the GC mark and rewrites each file from its chunk sequence through the normal writer path. The `ChunkProfile` is a value on `Options`. Metadata nodes stay on the default profile, and manifest cut policy stays independent of it. Prolly is its own crate outside `root_rec`.

**Major components:**
1. `Dispatcher` (`dispatch.rs`) — byte-bounded queue, N workers, inline at N=1, drain-before-destroy.
2. `FileCell` table + `NamespaceLocks` (`locks.rs`) + rank checker — per-open-file state and namespace consistency kept as two separate locks. This deviates from #21's single per-inode table; it is reversible and the harness decides.
3. Rebase flusher (`flusher.rs`) and WAL group commit (leader/follower on the existing `file_for_sync`).
4. `ChunkProfile` plumbing in `chunk-store` plus one structured `FORMAT` marker writer shared with rehash.
5. Rehash engine (`node_graph.rs`, `metadata/rehash.rs`, `file.rs::rewrite_file`, CLI), and `crates/prolly` + `benchmarks/prolly-bench`.

### Critical Pitfalls

1. **Stale baseline.** The research doc and #21 predate #108. Pack fsync is already off the store lock and the gate generation switch has landed. Re-measure first, then edit S2 and S5, and mark #21 superseded where it conflicts.
2. **Lock inversion with a new thread (flusher, release, destroy).** Take the gate first for every thread type, use a debug rank checker that fails tests, and never wait on budget, durability or commit while holding gate or inode lock. Add a test with all workers parked while a commit waits.
3. **`zero_chunk()` is a process-wide static built from `CHUNK_MAX`.** With a per-store profile the first store opened wins and the zero shortcut returns wrong bytes. That is corruption, not a perf issue. Make it per-profile and test two profiles in one process.
4. **Rehash correctness.** `is_cut` reads digest bytes, so node-for-node mapping gives non-canonical trees. Rebuild from the chunk sequence and test `rehash(key) == fresh_write(bytes)` across sizes. Read through the verifying read so corruption is not laundered. Treat in-progress output as GC roots or exclude GC. Sum `refcounts` when keys collapse. Use a real OS file lock, not the exists-then-write `mount.lock`.
5. **Head-of-line blocking and uncancellable waits.** Count only WRITE payload bytes against the queue bound, admit before dequeue, and keep a reserved lane for release and fsync. Give every wait a timeout. Shut down in order: stop the reader, reply to the queue, join workers, then destroy. `FUSE_INTERRUPT` is unhandled in the vendored fuser.
6. **Pool on macOS without S1.** Default N=1 on macOS until S1 is recorded by hand with an in-flight counter.

Also: GC can sweep objects a lock-free reader still uses, so a reader must pin its root. The dirty-budget formula must include queue bytes, retry copies and `2 * max` writer buffers (1 MiB per open writer at 512 KiB max). #19's baseline is today's code, not "full re-serialization of everything".

## Implications for Roadmap

Suggested structure is 7 phases. Phase numbers continue after v2.1 (which ended at Phase 17); the roadmapper assigns them.

### Phase A: Spikes, baseline and stress harness
**Rationale:** The concurrency track depends on it, and the research baseline is stale. Its red tests define "done" for later phases. Benchmark rules belong here because #67 and #19 reuse them.
**Delivers:**
- Re-measured S2/S5 baselines (cut hold time, `begin_commit` clone cost at 100k and 1M files).
- `concurrency_tests.rs` N-thread in-process harness with a max-in-flight counter and replay-equality check.
- `benchmarks/stress-bench` with `hdrhistogram`.
- S3 and S6 (Linux) microbenches.
- `--trace-fuse`, off by default.
- One benchmark rule sheet.
- A short design note recording owner decisions 2 and 3, and updated issue bodies.
- The `parking_lot` swap as one mechanical commit.

**Avoids:** Pitfalls 1, 10, 11 (counter), 24.

### Phase B: Lock foundation (#21 core)
**Rationale:** A2 to A4 rewrite the same few hundred lines of `filesystem.rs` and `handlers.rs`, so they stay serial. Correctness under concurrency pays off even at N=1.
**Delivers:**
- `readers` Arc-per-inode, and WAL sync outside the append mutex.
- Sharded `Live` + `modify_inode`.
- `FileCell` table with shared alias state.
- Log under the lock, `create_inode_at`, and replay without `got == ino`.
- `NamespaceLocks`, the rename mutex and the debug rank checker.
- A revalidate-after-lock helper and a reader GC pin.

**Uses:** `parking_lot` 0.12, own `ShardedTable`, the `crate::sync` shim, shuttle.
**Avoids:** Pitfalls 2, 6, 7, 25, 29, and applies the Pitfall 8 policy to new locks.

### Phase C: Dirty budget and rebase flusher
**Rationale:** It needs the per-inode atomics from Phase B. It carries the highest deadlock risk, so it gets its own phase and test.
**Delivers:**
- `flusher.rs` with soft and hard thresholds.
- Writer-pays sleep with timeouts.
- Removal of cross-file `keep_budget`.
- Admission before gate and cell.
- One worst-case memory formula, asserted in S7 with an RSS ceiling.

**Avoids:** Pitfalls 4, 5, 9. Includes the "every worker throttled while a commit waits" test. S5 decides the gate lock type.

### Phase D: Worker pool (#22)
**Rationale:** Safe only after B and C. The dispatcher skeleton with one global handler mutex can be pulled forward so S1 and S4 run early. This phase removes the mutex and ships the pool.
**Delivers:**
- `dispatch.rs` and `--threads N` (default 1, inline).
- Drain-before-destroy, the panic policy, and reply-after-guards structure.
- Queue lanes.
- Linux CI at N=4 with pjdfstest.
- S1/S4 results recorded by hand on macOS.
- Optional WAL group commit if S6 supports it.

**Avoids:** Pitfalls 3, 4, 5, 8, 11, 26.

### Phase E: Chunk profiles (#67)
**Rationale:** Independent of Track A files. It must land before rehash, since both edit `file.rs` and the `FORMAT` marker.
**Delivers:**
- `ChunkProfile` on `Options`, a per-store zero-chunk table, and all `CHUNK_*` uses converted.
- One structured marker writer and parser (decision 4).
- A golden cut-point test, `seed --chunk-size`, and refusal on mismatch.
- Benchmark scripts and a report, with no default change.

**Avoids:** Pitfalls 19, 20, 27, 28.

### Phase F: Offline rehash (#62)
**Rationale:** It needs the marker work from E, benefits from the Phase B `store.rs` refactor, and shares a traversal with GC.
**Delivers:**
- `node_graph.rs` (children + rewrite per node kind, shared with the GC mark) and `rewrite_file`.
- A bounded memo (LRU, with an extsort sidecar only if the benchmark requires it).
- Refcount summing and snapshot records.
- The root segment switch as the single commit point.
- An OS file lock, a clean-log precondition, `--dry-run`, progress, GC exclusion and verifying reads.
- Crash failpoint tests and end-to-end tests with `TestKeyedBlake3`.

**Avoids:** Pitfalls 12-18.

### Phase G: Prolly evaluation (#19)
**Rationale:** It is evaluation only. It needs a baseline and the profile plumbing for the combined benchmark, so it goes last. The baseline measurement can start earlier in parallel.
**Delivers:**
- The `crates/prolly` prototype: one map, key-based boundaries with a size-aware rule, and nodes as single chunk-store objects.
- `benchmarks/prolly-bench`.
- A baseline of today's per-commit CPU, memory, bytes and gate-hold clone time, and a combined run with the #67 profiles.
- A written adopt/later/drop decision. The file-manifest half is reported as already built.

**Avoids:** Pitfalls 21-23.

### Phase Ordering Rationale

- Track A (Phases A to D) is serial. Tracks B (E, F) and C (G) touch different files and can run beside it on separate branches.
- E comes before F because both edit `file.rs` and the marker code, so the marker is changed once.
- The dispatcher skeleton with a global handler mutex can be pulled forward so S1/S4 run long before the lock work finishes. If S1 shows no overlap on macOS, stop at N=1 there; the lock work still pays off in tail latency.
- Every capability ships dark with a safe default (N=1, 4/8/32 KiB, rehash as a separate command, prolly outside the root).

### Research Flags

Phases likely needing `/gsd-plan-phase --research-phase <N>`:
- **Phase A:** gate fairness (S5) on both platforms and macOS S1 by hand. Re-baselining depends on measurements not yet taken.
- **Phase C:** the budget/flusher/gate deadlock protocol is the largest risk, and the shuttle model needs design.
- **Phase D:** FUSE-T/FSKit overlap and out-of-order replies are unknown (LOW), as is reply-under-lock behaviour with a slow peer.
- **Phase F:** the old-to-new map strategy (LRU vs sorted sidecar) depends on dedup ratio. GC interaction and snapshot record supersession ("latest record wins") are unverified.
- **Phase G:** Dolt-style boundary rule details are LOW confidence, and node-size spread must be measured.

Phases with standard patterns (skip research-phase):
- **Phase B:** established lock-rank and take-out-removal pattern, already mapped to code by the research doc.
- **Phase E:** mechanical plumbing plus benchmarks with the existing `chunk-bench` and `index-bench`.

## Confidence Assessment

| Area | Confidence | Notes |
|------|------------|-------|
| Stack | MEDIUM-HIGH | Versions and dates are from crates.io and local source (HIGH). Behaviour claims from doc pages are LOW (parking_lot 0.13 changes, RwLock policy); verify in S5 and with the golden cut test. |
| Features | HIGH | Issue acceptance criteria and repo facts are primary. Comparable-system behaviour comes from vendor docs (MEDIUM); kopia details are LOW. |
| Architecture | HIGH for current code, MEDIUM for new design, LOW for macOS overlap | Read directly from the repo. Stripes vs a per-inode table is a reversible choice for the harness to confirm. |
| Pitfalls | MEDIUM-HIGH | Most items were read from code. FUSE-T behaviour, `std` RwLock fairness on macOS, fio buffer dedup and Dolt boundary details are unverified. |

**Overall confidence:** MEDIUM-HIGH

### Gaps to Address

- **macOS FUSE-T/FSKit concurrency (S1):** no result exists. Run it by hand after the dispatcher skeleton and keep N=1 as the default.
- **Gate lock fairness (S5):** `parking_lot` and `std` writer preference are unverified on macOS. Decide the gate type from measurement.
- **Marker format-number bump:** ARCHITECTURE.md says a new optional `FORMAT` line needs no bump. PITFALLS.md says to bump it, because old parsers skip unknown lines and would chunk with defaults. Recommend the bump; decide in Phase E.
- **Rehash memo strategy:** ARCHITECTURE.md recommends a bounded LRU with recompute. FEATURES.md and PITFALLS.md lean toward a durable sorted map (resume, high-dedup stores). Decide after a benchmark in Phase F. Both agree there should be no unbounded in-RAM map.
- **Gate generation switch:** ARCHITECTURE.md shows it already landed (#133), while FEATURES.md and STACK.md still list it as conditional. Treat it as landed. S5 now decides only the lock type and the `begin_commit` clone cost.
- **`inode_map` vs `inode_data` redundancy:** they look redundant. Verify before sharding.
- **Reader GC pin:** verify whether the `ChunkStore::read` generation checks already cover the window before building a pin set.
- **Snapshot record supersession:** confirm that load takes the latest record for a version before rehash relies on it.
- **fastcdc bounds:** confirmed in the 3.2.1 source (min 64 B to 1 MiB, avg 256 B to 4 MiB, max 1 KiB to 16 MiB). Re-verify on any upgrade.
- **Prolly baseline sizes:** the 3.2 MB per map at 78k entries is estimated, not measured.

## Sources

### Primary (HIGH confidence)
- Repository code and docs: `docs/research/multithreaded-request-handling.md`, `docs/design/chunk-store.md`, `docs/design/write-log.md`, the `slicefs-cli`, `metadata` and `chunk-store` crates, `vendor/fuser` sources and PATCHES.md, `benchmarks/`, and git history for #108 (PRs #131, #133).
- GitHub issues #19, #21, #22, #62, #67, #127.
- crates.io API (versions, dates, MSRV) and the local fastcdc 3.2.1 source.

### Secondary (MEDIUM confidence)
- restic, borg and git hash-transition documentation (parameters fixed at creation, copy-and-switch, resumable transfer).
- Dolt prolly-tree articles; docs.rs pages for `prolly-map` and `prollytree`.
- Kernel and filesystem design points as cited by the research doc (lock rank, ZFS and jbd2 wait rules).

### Tertiary (LOW confidence, needs validation)
- fastcdc, parking_lot and fuser changelog pages (web fetch), `std::sync::RwLock` fairness on macOS, and `std::sync::mpmc` status.
- FUSE-T and FSKit overlap and reply-order behaviour (no source; S1).
- kopia splitter behaviour (forum posts), Dolt boundary-probability detail (issue text only), and fio default buffer fill and its dedup effect.

---
*Research completed: 2026-11-20*
*Ready for roadmap: yes, after the six owner decisions above are recorded (decisions 1 and 2 are the only ones that change phase scope)*
