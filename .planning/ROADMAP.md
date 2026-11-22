# Roadmap: SliceFS

## Milestones

- ✅ **v1.0 Foundation** - Phases 1-7 (shipped 2026-03-29)
- ✅ **v2.0 Streaming Writes & Hardening** - Phases 7.1-12 (shipped 2026-11-17, tag `milestone-v2.0`)
- ✅ **v2.1 Store Index & Measurements** - Phases 13-17 (shipped 2026-11-20, release `v2.2.0`)
- 🚧 **v2.2 Concurrency & Store Evolution** - Phases 18-24 (in progress)

## Phases

<details>
<summary>✅ v1.0 Foundation (Phases 1-7) - SHIPPED 2026-03-29</summary>

### Phase 1: CAS Foundation
**Goal**: The immutable block store, pluggable hash interface, pluggable chunking interface, and dedup index exist and can be exercised in unit tests — every subsequent component has a foundation to build on
**Depends on**: Nothing (first phase)
**Requirements**: CAS-01, CAS-02, CAS-03, CAS-05, CAS-07
**Success Criteria** (what must be TRUE):
  1. A block can be written to the local disk store keyed by its hash and retrieved by that hash
  2. An alternative hash function can be swapped in by implementing the ContentHasher trait without changing any other code
  3. An alternative chunking strategy can be swapped in by implementing the Chunker trait without changing any other code
  4. A duplicate block write is detected via the bloom filter + on-disk index before any disk write occurs
  5. A retrieved block fails verification if its stored bytes have been corrupted (integrity check on read)
**Plans:** 3/3 plans complete

Plans:
- [x] 01-01-PLAN.md — Cargo workspace setup and CAS trait definitions in slicefs-traits
- [x] 01-02-PLAN.md — Blake3Hasher, FixedChunker, and MemBlockStore stub implementations
- [x] 01-03-PLAN.md — LocalDiskStore and MemDedupIndex with bloom filter

### Phase 2: Metadata Engine
**Goal**: The inode table, directory tree, file manifests, and xattr store exist as an ACID-backed metadata layer completely separated from the block store — FUSE can be wired on top of it
**Depends on**: Phase 1
**Requirements**: META-03, POSIX-06, POSIX-07, POSIX-08, POSIX-10
**Success Criteria** (what must be TRUE):
  1. An inode can be created, read, updated, and deleted via the MetadataStore interface
  2. A directory entry can be created, listed, and removed; . and .. are always present
  3. A file manifest linking inode to ordered list of block hashes can be created and retrieved
  4. Extended attributes can be stored and retrieved on an inode
  5. Inode numbers are stable — the same inode number is assigned to the same file across process restarts
**Plans:** 3/3 plans complete

Plans:
- [x] 02-01-PLAN.md — data-id submodule, slicefs-traits redesign, InodeMeta serialization
- [x] 02-02-PLAN.md — DictMetadataStore with inode CRUD, directory ops, and manifest storage
- [x] 02-03-PLAN.md — Xattr storage and persistence round-trip with inode stability proof

### Phase 3: Read-Only FUSE
**Goal**: The filesystem can be mounted and browsed read-only by the OS; a human can ls, cat, and stat files through the mount point using pre-populated content — the kernel interface is validated before write complexity is introduced
**Depends on**: Phase 2
**Requirements**: POSIX-13, POSIX-15, PLAT-02, CLI-01, CLI-02, CLI-06, META-02
**Success Criteria** (what must be TRUE):
  1. Running the mount command produces a mountable filesystem at the specified path on Linux
  2. Files and directories pre-seeded into the backing store appear correctly under ls, stat, and cat through the mount point
  3. The filesystem unmounts cleanly via the unmount command with no kernel errors; SIGTERM triggers a graceful flush
  4. All POSIX operations return correct errno values for read-only violations (e.g., EROFS on write attempt)
  5. Mount options (noatime, cache size) are accepted and applied
**Plans:** 3/3 plans complete

Plans:
- [x] 03-01-PLAN.md — CLI crate scaffold, StoreIo, clap subcommands, and SliceFsFilesystem FUSE adapter
- [x] 03-02-PLAN.md — Seed command: import directory tree into CAS store via State CDC
- [x] 03-03-PLAN.md — Mount and unmount commands with FUSE lifecycle and end-to-end verification

### Phase 4: Full POSIX Write Path
**Goal**: Files can be created, written, modified, renamed, deleted, and linked through the mount point with inline deduplication active; real tools (editors, package managers, build systems) work correctly; pjdfstest passes >95% on Linux
**Depends on**: Phase 3
**Requirements**: POSIX-01, POSIX-02, POSIX-03, POSIX-04, POSIX-05, POSIX-09, POSIX-12, POSIX-14, CAS-04, CAS-06
**Success Criteria** (what must be TRUE):
  1. A file written through the mount point is stored as deduplicated blocks; writing the same file twice results in one physical copy
  2. Atomic rename works correctly — editors (vim, emacs) can save files without corruption
  3. Hard links share inode reference counts; unlinking one hard link does not remove the file until all links are removed
  4. statfs reports both logical and physical byte counts, showing the dedup ratio
  5. pjdfstest passes >95% of applicable tests on Linux
**Plans:** 4/4 plans complete

Plans:
- [x] 04-01-PLAN.md — Refcount infrastructure, write state types, RW mount, destroy persistence
- [x] 04-02-PLAN.md — Core file write path: create, write, release with CAS flush, setattr/truncate
- [x] 04-03-PLAN.md — Directory ops, rename, symlinks, hard links, unlink with nlinks lifecycle
- [x] 04-04-PLAN.md — Dedup-aware statfs, POSIX locking stubs, custom POSIX compliance test suite

### Phase 5: Crash Safety and GC
**Goal**: The filesystem survives crashes and power loss without data loss or block leaks; garbage collection reclaims orphaned blocks safely without racing against active writes or snapshots
**Depends on**: Phase 4
**Requirements**: META-01, GC-01, GC-02, GC-03, POSIX-11
**Success Criteria** (what must be TRUE):
  1. A kill -9 during an active write followed by remount produces a consistent filesystem — no dangling block references and no data corruption
  2. fsync and fdatasync guarantee durability — data written before the call is on disk before the call returns, verified after crash
  3. Orphaned blocks from an interrupted write are reclaimed by GC and do not grow the store unboundedly
  4. A block referenced by any snapshot is never deleted by GC, even when its refcount reaches zero in the live tree
  5. WAL replay on dirty mount restores the last committed state without manual intervention
**Plans:** 4/4 plans complete

Plans:
- [x] 05-01-PLAN.md — Segment file I/O layer and WAL strategy trait with per-op and no-op implementations
- [x] 05-02-PLAN.md — DictMetadataStore segment integration, dirty mount detection, fsync callback, --wal-strategy CLI
- [x] 05-03-PLAN.md — Mark-and-sweep GC engine with segment compaction and background thread
- [x] 05-04-PLAN.md — Offline GC CLI command, mount with background GC, crash recovery integration tests

### Phase 6: Compression and Snapshots
**Goal**: Stored blocks are compressed to reduce physical footprint; point-in-time snapshots can be created and the filesystem state can be switched between historical versions — both capabilities are natural expressions of the CAS architecture already in place
**Depends on**: Phase 5
**Requirements**: COMP-01, COMP-02, SNAP-01, SNAP-02, SNAP-03
**Success Criteria** (what must be TRUE):
  1. Blocks written to the store are compressed before storage; the physical size on disk is smaller than the logical size for compressible data
  2. An alternative compressor (e.g., LZ4 vs. Zstd) can be swapped in via a pluggable compressor trait without changing the store interface
  3. A snapshot command creates a read-only point-in-time view; files in the snapshot are readable and match their state at snapshot time
  4. Switching to a historical version makes the live filesystem reflect that version's file contents
  5. Two snapshots sharing blocks do not double-count physical storage; shared blocks appear once in physical usage
**Plans:** 4/4 plans complete

Plans:
- [x] 06-01-PLAN.md — Compressor trait in slicefs-traits + Zstd/LZ4/None implementations in slicefs-compression crate
- [x] 06-02-PLAN.md — Compression wired into FUSE write/read path + CLI flags (--compressor, --compressor-level)
- [x] 06-03-PLAN.md — SnapshotRecord segment entry + DictMetadataStore snapshot methods (create/list/find/roots)
- [x] 06-04-PLAN.md — Snapshot CLI commands (create/list/switch) + snapshot mount flags + snapshot-aware GC

### Phase 7: Cross-Platform and Production Hardening
**Goal**: The filesystem runs on macOS via FUSE-T with full POSIX compliance; the CLI is complete with stats, scrub, and structured JSON output; benchmark baselines confirm daily-driver performance; Windows support is deferred to v2
**Depends on**: Phase 6
**Requirements**: PLAT-01, PLAT-03, PLAT-04, CLI-03, CLI-04, CLI-05
**Success Criteria** (what must be TRUE):
  1. The filesystem mounts and passes the macOS-specific POSIX test suite (mmap writes, flock, large directory) via FUSE-T
  2. The filesystem mounts and passes basic POSIX conformance on Windows via WinFSP
  3. The stats command outputs dedup ratio, logical bytes, physical bytes, and block count in both human-readable and JSON formats
  4. The scrub command walks all stored blocks, re-verifies their hashes, and reports any corrupted blocks without modifying the store
  5. Sequential write throughput on NVMe meets or exceeds 200 MB/s as measured by fio with 4K writes; dedup index memory stays below the configured cap under 100 GB unique data load
**Plans:** 3/3 plans complete

Plans:
- [x] 07-01-PLAN.md — FUSE-T write path fix + macOS build ergonomics (direct_io, .cargo/config.toml, build.rs)
- [x] 07-02-PLAN.md — Global --json flag + stats and scrub CLI commands with human and JSON output
- [x] 07-03-PLAN.md — GitHub Actions CI (Linux + macOS), benchmark infrastructure, Windows deferral documentation

</details>

<details>
<summary>✅ v2.0 Streaming Writes & Hardening (Phases 7.1-12) - SHIPPED 2026-11-17</summary>

- [x] Phase 7.1: FileStorage Migration (INSERTED) (3/3 plans)
- [x] Phase 8: Correctness Fixes (1/1 plan) - completed 2026-03-30
- [x] Phase 9: Compression Removal (2/2 plans)
- [x] Phase 10: Streaming Writes Core (3/3 plans) - completed 2026-03-30
- [x] Phase 11: Non-Sequential Write Handling (2/2 plans) - completed 2026-03-30
- [x] Phase 12: FUSE-T Backend Selection (2/2 plans) - completed 2026-04-01

Full details: `.planning/milestones/v2.0-ROADMAP.md`. Audit: `.planning/milestones/v2.0-MILESTONE-AUDIT.md`.

</details>

<details>
<summary>✅ v2.1 Store Index & Measurements (Phases 13-17) - SHIPPED 2026-11-20</summary>

- [x] Phase 13: Write log close-out (#20) (3/3 plans)
- [x] Phase 14: Bounded-memory random writes (#98, #93) (9/9 plans)
- [x] Phase 15: Pack-file measurements (#8) (3/3 plans)
- [x] Phase 16: Chunk index benchmark (#66) (3/3 plans)
- [x] Phase 17: On-disk chunk index (#85) (9/9 plans)

Full details: `.planning/milestones/v2.1-ROADMAP.md`. Audit: `.planning/milestones/v2.1-MILESTONE-AUDIT.md`.

</details>

### 🚧 v2.2 Concurrency & Store Evolution (Phases 18-24)

**Milestone goal:** Handle FUSE requests on more than one thread with fine-grained locks, add `slicefs rehash` with SHA-256 as a second production hash, and measure two store-format options (larger FastCDC profiles, prolly trees) before any default changes.

**Tracks:** Phases 18-21 are one serial track (the concurrency work). Phases 22 and 23 are a second serial track (chunk profiles, then rehash), and Phase 24 follows Phase 22. Phases 22-24 touch other files than Phases 18-21, so they **can run in parallel** with the concurrency track on separate branches. Phase 22 comes before Phase 23 because both edit `file.rs` and the store FORMAT marker.

**Owner decisions** (fixed for this milestone):
- SHA-256 is a production second hash (HASH-01). BLAKE3-256 stays the default.
- Rehash is offline only.
- A worker panic aborts the daemon; the write log replays at the next mount.
- Each log record is appended under the same locks as its apply. The older "append first" table in #21 is superseded and the issue body is updated (LOCK-04).
- The chunk profile is fixed at `seed`. The default stays 4/8/32 KiB.
- Prolly work is a prototype and a written decision. There is no migration.
- The default worker count is 1 on macOS until spike S1 is recorded (SPIKE-03).

**Boundaries between phases:**
- SPIKE-01 covers S1-S7 and closes in Phase 18. S1, S3 and S4 need concurrent dispatch, so plan-phase may use a throwaway dispatcher behind one global handler mutex for them. POOL-01 stays in Phase 21. S7 gets a first baseline from the harness; Phase 20 sets the final thresholds.
- LOCK-02 closes in Phase 20. Phase 19 stops write, settle and setattr from taking state out of `open_files`. `keep_budget` is the last cross-file take-out and goes away with the flusher. Phase 19 concurrency tests stay below the dirty budget.

**Research flags** (run `/gsd-plan-phase <N> --research-phase`): Phase 18 (S5 gate fairness on both platforms), Phase 20 (budget, flusher and gate deadlock protocol), Phase 21 (FUSE-T and FSKit overlap, reply under load), Phase 23 (old-to-new map strategy, GC interaction, snapshot record supersession), Phase 24 (boundary rule details). Skip for Phases 19 and 22: standard patterns.

- [x] **Phase 18: Spikes, baseline and stress harness** (#127) - re-measure S1-S7 on the current code, build the N-thread harness, run S1 on macOS (completed 2026-11-21)
- [ ] **Phase 19: Fine-grained locks** (#21) - no global lock on the hot path, lock rank with a debug checker, log record under the same locks as its apply
- [ ] **Phase 20: Dirty budget and rebase flusher** (#21) - no cross-file take-out, a background flusher, writers throttled before they take locks, no deadlock with a commit
- [ ] **Phase 21: Worker pool** (#22) - N worker threads by mount option, drain before `destroy()`, abort on panic, 4 workers pass pjdfstest
- [ ] **Phase 22: Chunk profiles** (#67) - FastCDC min/avg/max fixed per store at `seed`, benchmark of five profiles, default unchanged (parallel track)
- [ ] **Phase 23: Hashes and rehash** (#62) - SHA-256 write hash and an offline, crash-safe `slicefs rehash` (parallel track)
- [ ] **Phase 24: Prolly evaluation** (#19) - one-map prototype, benchmark on Boost trees, written decision (parallel track)

### Phase 18: Spikes, baseline and stress harness

**Goal**: The migration steps for multithreaded requests are decided by measurements on the current code, and an in-process stress harness exists that the lock phases must pass (#127)
**Depends on**: Nothing (first phase)
**Requirements**: SPIKE-01, SPIKE-02, SPIKE-03
**Success Criteria** (what must be TRUE):
  1. The threading research doc has a result for each of S1-S7, and each result names the migration step it decides; S2 and S5 are measured again on the current code (after #108), and old baselines that no longer hold are marked superseded
  2. A harness runs N threads of mixed operations (read, write, fsync, create, rename, unlink) on one file and on many files, checks the data against a model, and reports latency histograms and lock hold and wait times
  3. The harness has a short run for the test suite and a longer run that is started by hand
  4. S1 is run on a macOS FSKit mount (by hand or in macOS CI), with the maximum number of requests in flight and the reply order recorded, and the doc states whether the macOS default can be more than one worker
**Plans**: 7/7 plans complete

Plans:
**Wave 1**
- [x] 18-01-PLAN.md — Harness tracer (one file, serialize on, model, latency) and the store-lock recorder behind `lock-stats` (optional hdrhistogram)
- [x] 18-04-PLAN.md — S1 probe, Linux control run, macOS FSKit CI workflow and run

**Wave 2** *(blocked on Wave 1 completion)*
- [x] 18-02-PLAN.md — Remaining lock labels (io, wal, WAL strategy mutexes, open_files, readers) and the `lock_stats_report` test
- [x] 18-03-PLAN.md — Full harness: all six op kinds, many-files mode, namespace model, planted-bug test, watchdog, ignored long and serialize-off runs

**Wave 3** *(blocked on Wave 2 completion)*
- [x] 18-05-PLAN.md — Linux mount spikes: S6, S4 scripts and runs; S2 measured again (3 runs per size)

**Wave 4** *(blocked on Wave 3 completion)*
- [x] 18-06-PLAN.md — Harness spikes S3, S5 (macOS and Linux), S6 in process (macOS), S7 baseline (Linux)

**Wave 5** *(blocked on Wave 4 completion)*
- [x] 18-07-PLAN.md — Spike results S1-S7 in the threading research doc, decided steps, SUPERSEDED marks

### Phase 19: Fine-grained locks

**Goal**: Requests can run in parallel on one file and on many files with correct data; no global lock is held during disk I/O, a hash calculation or a wait; and the write log replays to the same state as the apply (#21)
**Depends on**: Phase 18
**Requirements**: LOCK-01, LOCK-03, LOCK-04, LOCK-05
**Success Criteria** (what must be TRUE):
  1. A test with parallel readers and writers on one file returns correct data, and the lock wait times show that readers do not wait for the I/O of writers
  2. A test with parallel I/O on different files shows no contention between them
  3. The store lock, `readers`, the WAL append mutex and `dir_live`/`inode_live` are not held during disk I/O, a hash calculation or a wait; the hold times in the harness report show it
  4. Locks have one fixed rank, with two inodes locked in ascending stripe order (inode number hashed to a stripe); a test that breaks the rank fails in a debug build
  5. After a crash, replay of the log from the mixed-operation harness gives the same state as the apply, including creates by logged inode number, and the #21 body matches this rule
  6. With several handles on one inode in different modes (read-only, read-write, `O_APPEND`), the harness shows the POSIX.1 2.9.7 rules: a completed write through one handle is visible through every other handle, a read sees all or none of a concurrent write, and concurrent appends never overlap and lose no bytes. The research for this phase checks how the kernel FUSE client computes the append offset in the SliceFS mount mode
**Plans**: 9/11 plans executed

One plan per wave; executors run them in order.

Plans:
**Wave 1**
- [x] 19-01-PLAN.md — Lock shim `chunk_store::sync` over parking_lot 0.12, rank checker, `deadlock-detect`, all hot-path locks migrated

**Wave 2** *(blocked on Wave 1 completion)*
- [x] 19-02-PLAN.md — Chunk-store lock tails (pack create, seal join, run write, KEEP_CAP wait, trust floor, pack open) off the store lock

**Wave 3** *(blocked on Wave 2 completion)*
- [x] 19-03-PLAN.md — Metadata hot path: `modify_inode`, sharded caches, cold loads and flushes off the locks, `create_inode_at`

**Wave 4** *(blocked on Wave 3 completion)*
- [x] 19-04-PLAN.md — WAL append (LSN, memory only) and write-out split; fsync outside the WAL mutexes

**Wave 5** *(blocked on Wave 4 completion)*
- [x] 19-05-PLAN.md — `FileCell` per inode, handle table with modes, per-inode readers, `keep_budget` on atomics

**Wave 6** *(blocked on Wave 5 completion)*
- [x] 19-06-PLAN.md — Overlay rebase through copy, I/O without lock, publish if the generation is unchanged

**Wave 7** *(blocked on Wave 6 completion)*
- [x] 19-07-PLAN.md — Streaming, read, fsync, finalize and truncate through the same protocol; no cross-file wait

**Wave 8** *(blocked on Wave 7 completion)*
- [x] 19-08-PLAN.md — Write-path records logged under the cell lock; `O_APPEND` handles; POSIX multi-handle tests

**Wave 9** *(blocked on Wave 8 completion)*
- [x] 19-09-PLAN.md — Namespace stripes and rename mutex, gate at rank 1, `serialize_off_target` in the suite

**Wave 10** *(blocked on Wave 9 completion)*
- [ ] 19-10-PLAN.md — Namespace records logged under their stripes, replay by logged inode, crash replay of the harness

**Wave 11** *(blocked on Wave 10 completion)*
- [ ] 19-11-PLAN.md — Harness multi-handle mode, lock wait rows, #21 body, Phase 19 note in the research doc

### Phase 20: Dirty budget and rebase flusher

**Goal**: With several writers, daemon memory stays under the set limit, a writer over the limit waits before it takes any lock, and a commit never deadlocks with throttled writers (#21)
**Depends on**: Phase 19
**Requirements**: LOCK-02, LOCK-06
**Success Criteria** (what must be TRUE):
  1. No request takes another file's state out of `open_files`; the dirty-budget path uses per-file cost counters and the flusher, and a check in the test suite finds no take-out site, including `keep_budget`
  2. A background flusher lowers the shared dirty budget, a writer over the limit sleeps before it takes any lock, and every wait has a time limit
  3. A test with all workers throttled while a commit waits finishes without deadlock
  4. A Docker run with parallel random writers on large files keeps peak RSS (VmHWM) under one fixed cap
**Plans**: TBD

### Phase 21: Worker pool

**Goal**: FUSE requests run on a pool of worker threads, with the number of workers set by a mount option, on Linux and on macOS FSKit (#22)
**Depends on**: Phase 20 (the macOS default also needs SPIKE-03 from Phase 18)
**Requirements**: POOL-01, POOL-02, POOL-03, POOL-04, POOL-05
**Success Criteria** (what must be TRUE):
  1. A mount option sets the number of workers; with 1 worker the handler runs inline as today, and the macOS default stays 1 unless SPIKE-03 shows that requests overlap
  2. Unmount and SIGTERM drain the queue and join the workers before `destroy()`, and replies are sent after all locks are released
  3. A worker panic aborts the daemon; the next mount replays the write log, and no request is served after a half-done update
  4. All tests and pjdfstest pass with 4 workers on Linux (Docker CI), and the tests pass on macOS FSKit CI with the macOS default
  5. Parallel I/O on different files scales with the number of workers on Linux, and the measured result is recorded
**Plans**: TBD

### Phase 22: Chunk profiles

**Goal**: A store has one FastCDC profile, fixed when the store is seeded, and a benchmark shows what larger profiles cost and gain; the default stays 4/8/32 KiB (#67)
**Depends on**: Nothing in this milestone (parallel with the concurrency track)
**Requirements**: PROF-01, PROF-02, PROF-03
**Success Criteria** (what must be TRUE):
  1. `seed` accepts a profile (min/avg/max, powers of two, inside the crate limits) and keeps it in the store FORMAT marker; a store without a setting uses 4/8/32 KiB, and a mount with a different profile is refused
  2. Two stores with different profiles work in one process: a file written under each profile reads back byte for byte, including files with zero runs and a spliced edit, and a golden cut-point test fails if a cut point moves
  3. One reproducible script reports, for 4/8/32, 8/16/64, 8/32/128, 16/64/256 and 32/128/512 KiB: physical bytes after dedup and zstd, object count, index RAM, cold-open time, ingest throughput, read latency and rewrite bytes after small edits; it runs outside the test suite
  4. The report recommends a default, and a store seeded without a profile setting is still 4/8/32 KiB after v2.2
**Plans**: TBD

### Phase 23: Hashes and rehash

**Goal**: A store can use SHA-256 as its write hash, and `slicefs rehash` converts an offline store from one hash algorithm to another without data loss, even after a crash (#62)
**Depends on**: Phase 22 (same `file.rs` and FORMAT marker; parallel with the concurrency track)
**Requirements**: HASH-01, HASH-02, HASH-03, HASH-04, HASH-05, HASH-06
**Success Criteria** (what must be TRUE):
  1. A store can be seeded or mounted with `--hash sha256` (own algorithm tag, hardware-accelerated where the CPU has it), and BLAKE3-256 stays the default
  2. `slicefs rehash --to <algorithm> <store>` converts a BLAKE3-256 store to SHA-256 and back, and a store with mixed algorithms ends with one; a rehashed file gets the same key as a fresh write of its bytes, files and snapshots do not change, and scrub passes
  3. Rehash refuses while the store is mounted (OS file lock) and while the write log has records after the last root
  4. A crash at any point leaves a store that mounts with the old or the new root; GC frees the old objects after the switch, and old objects are verified as they are read
  5. Rehash reports progress, bytes rewritten and bytes freed; `--dry-run` changes nothing; and its memory stays bounded, with no in-RAM map of all objects
**Plans**: TBD

### Phase 24: Prolly evaluation

**Goal**: The owner has measured numbers and a written decision on prolly trees for metadata maps (#19)
**Depends on**: Phase 22 (the combined benchmark uses the profiles; parallel with the concurrency track)
**Requirements**: PROLLY-01, PROLLY-02, PROLLY-03
**Success Criteria** (what must be TRUE):
  1. A prototype for one metadata map (for example the directory map) is a separate crate that is not wired into the root record; its nodes are chunk-store objects, and tests show that its node boundaries depend only on the keys
  2. A benchmark on the Boost trees (about 78,000 files per version) compares the current commit path with the prototype for commit time, CPU, peak memory, bytes written per commit, and on-disk size after several small commits; it runs outside the test suite
  3. A written decision (adopt, later, or drop) is in the design docs and on #19, and it states that file manifests are already content-cut trees
**Plans**: TBD

## Progress

**Execution order:** the concurrency track runs 18 → 19 → 20 → 21, and the store track runs 22 → 23. Phase 24 follows Phase 22. Phases 22-24 may run in parallel with Phases 18-21.

| Milestone | Phases | Plans | Status | Shipped |
|-----------|--------|-------|--------|---------|
| v1.0 Foundation | 1-7 | 24 | Complete | 2026-03-29 |
| v2.0 Streaming Writes & Hardening | 7.1-12 | 13 | Complete | 2026-11-17 |
| v2.1 Store Index & Measurements | 13-17 | 27 | Complete | 2026-11-20 |
| v2.2 Concurrency & Store Evolution | 18-24 | 0 | In progress | - |

| Phase | Plans Complete | Status | Completed |
|-------|----------------|--------|-----------|
| 18. Spikes, baseline and stress harness | 7/7 | Complete    | 2026-11-21 |
| 19. Fine-grained locks | 9/11 | In Progress | - |
| 20. Dirty budget and rebase flusher | 0/0 | Not started | - |
| 21. Worker pool | 0/0 | Not started | - |
| 22. Chunk profiles | 0/0 | Not started | - |
| 23. Hashes and rehash | 0/0 | Not started | - |
| 24. Prolly evaluation | 0/0 | Not started | - |
