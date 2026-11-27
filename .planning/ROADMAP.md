# Roadmap: SliceFS

## Milestones

- ✅ **v1.0 Foundation** - Phases 1-7 (shipped 2026-03-29)
- ✅ **v2.0 Streaming Writes & Hardening** - Phases 7.1-12 (shipped 2026-11-17, tag `milestone-v2.0`)
- ✅ **v2.1 Store Index & Measurements** - Phases 13-17 (shipped 2026-11-20, release `v2.2.0`)
- ✅ **v2.2 Concurrency & Store Evolution** - Phases 18-24 (shipped 2026-11-26, release `v2.3.0`)
- 🚧 **v2.3 Prolly Metadata & Robustness** - Phases 25-30 (in progress; release `v2.4.0`)

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

<details>
<summary>✅ v2.2 Concurrency & Store Evolution (Phases 18-24) - SHIPPED 2026-11-26</summary>

- [x] Phase 18: Spikes, baseline and stress harness (#127) (7/7 plans) — completed 2026-11-21
- [x] Phase 19: Fine-grained locks (#21) (16/16 plans) — completed 2026-11-22
- [x] Phase 20: Dirty budget and rebase flusher (#21) (8/8 plans) — completed 2026-11-23
- [x] Phase 21: Worker pool (#22) (9/9 plans) — completed 2026-11-24
- [x] Phase 21.1: Store header and format policy (#154) (4/4 plans) — completed 2026-11-24
- [x] Phase 21.2: Thread-safety verification and CI gates (11/11 plans) — completed 2026-11-25
- [x] Phase 22: Chunk profiles (#67) (4/4 plans) — completed 2026-11-26
- [x] Phase 23: Hashes and rehash (#62) (5/5 plans) — completed 2026-11-26
- [x] Phase 24: Prolly evaluation (#19) (5/5 plans) — completed 2026-11-26

Release `v2.3.0` (`v2.2.0` was already used). Full details: `.planning/milestones/v2.2-ROADMAP.md`. Audit: `.planning/milestones/v2.2-MILESTONE-AUDIT.md`.

</details>

### 🚧 v2.3 Prolly Metadata & Robustness (Phases 25-30)

**Milestone goal:** Move the metadata maps to prolly trees so a commit stays fast with millions of inodes, and make the store safer to use: a migration path between store formats, random crash tests, a scrub that names the damaged files, and a user page for the limits. It ships as release `v2.4.0`.

**Order of work** (owner-approved): prolly maps (#215), then migration (#155), then crash and soak tests (#157), then the scrub report (#159), then the limits page (#158). The phases are one serial track. Each phase depends on the one before it.

**Boundaries between phases:**
- Phase 25 fixes the format: layout, boundary rule and store format 8. Phase 26 puts the maps into the commit, read and GC paths and measures them.
- Phase 27 comes after Phase 26. Its test copies a flat-map store made by release `v2.3.0` into a prolly store.
- Phase 26 owns the commit-cut crash test (PMAP-08). Phase 28 owns the `kill -9` run on a live mount (CRSH-01). Phase 28 runs on a prolly store.
- Phase 30 is last because the page cites the crash tests and the other results.

**Owner decisions** (fixed for this milestone):
- No backward compatibility for stores. The new build refuses a store made by `v2.3.0`. The copy path (#155) is the upgrade.
- The default FastCDC profile stays 4/8/32 KiB.
- Crash tests run by hand and nightly on `ci/**` branches, not on each PR.
- The format freeze (#156) stays an owner decision after this milestone.

**Research flags** (run `/gsd-plan-phase <N> --research-phase`): Phase 25 (Q-2: one map per metadata map, or one combined namespace map; this needs an owner choice), Phase 26 (GC mark over prolly nodes, the cut points of a commit), Phase 28 (random kill and soak harness in Docker). Skip for Phases 27, 29 and 30: standard patterns.

- [x] **Phase 25: Prolly format and layout** (#215) - map layout decided, `inode_map`/`inode_data` duplication removed, fixed-point boundary rule with a version byte, store format 8 (completed 2026-11-27)
- [ ] **Phase 26: Prolly maps in commit, read and GC** (#215) - every metadata map is a prolly map, commit work in proportion to the change, GC mark with bounded memory, commit-cut replay, benchmark on the product code
- [ ] **Phase 27: Store migration** (#155) - `scripts/migrate-store.sh`, a Docker test from a `v2.3.0` store to a prolly store, `docs/user/upgrade.md`
- [ ] **Phase 28: Crash and soak tests** (#157) - random `kill -9` and container-stop rounds on a live mount, a 2-hour soak with a memory check
- [ ] **Phase 29: Scrub damage report** (#159) - scrub names the files and snapshots that a bad object damages, `--json`, exit codes 0/1/2, bounded memory
- [ ] **Phase 30: Limits page** (#158) - `docs/user/limits.md` with status, crash guarantees, limits, platforms and known issues; README link

### Phase 25: Prolly format and layout

**Goal**: The format of the prolly metadata maps is decided and written down, the duplicate inode map is gone, and the new format is store format 8 (#215)
**Depends on**: Phase 24 (the v2.2 evaluation and the `crates/prolly` prototype)
**Requirements**: PMAP-01, PMAP-02, PMAP-04, PMAP-05
**Success Criteria** (what must be TRUE):
  1. The map layout is decided and recorded in `docs/design/store-format.md`: one prolly map per metadata map, or one combined namespace map (Q-2 of #19), with the reason and the effect on the root record
  2. The root record holds only one of `inode_map` and `inode_data`; a store written by the new build mounts, and the existing metadata and mount tests pass with it
  3. The boundary rule uses a fixed-point table (no float test) and has a version byte; the rule, its constants and the prolly node format are in `docs/design/store-format.md`; a test with fixed input gives the same cut points on Linux and macOS
  4. A store written by the new build is store format 8; release `v2.3.0` refuses to mount it, and the new build refuses a format 7 store made by `v2.3.0` with a clear message that points to the upgrade doc
**Plans:** 8/8 plans complete

Plans:
**Wave 1**
- [x] 25-01-PLAN.md — Integer boundary rule with a generated fixed-point table, rule version byte in the node header, Config::new, salt and forced-cut counts for the bench (wave 1)
- [x] 25-02-PLAN.md — One inode map: drop the duplicate, 197-byte root record, inode number allocator, lock site, bench copy; Docker mount gate (wave 1)
- [x] 25-03-PLAN.md — Store format 8: format 7 refused with the upgrade-doc message, docs/user/upgrade.md, real v2.3.0 check in both directions, format policy D-13 (wave 1)

**Wave 2** *(blocked on Wave 1 completion)*
- [x] 25-04-PLAN.md — Boundary salt: header line with its ro_compat flag, `seed --boundary-salt`, refusals and M18, doc (wave 2)
- [x] 25-05-PLAN.md — Bench layout engine: five-map batches of real commits, layouts a/b/c, fixed output schema (wave 2)

**Wave 3** *(blocked on Wave 2 completion)*
- [x] 25-06-PLAN.md — Bench synthetic 1M/10M source, matrix runner, Docker case, pre-registered decision script (wave 3)

**Wave 4** *(blocked on Wave 3 completion)*
- [x] 25-07-PLAN.md — Full benchmark run in Docker and the layout and constants decision by the pre-registered rules; owner report (wave 4)

**Wave 5** *(blocked on Wave 4 completion)*
- [x] 25-08-PLAN.md — Pin the constants and goldens (macOS and Linux), write the metadata map section of store-format.md, phase gate (wave 5)

### Phase 26: Prolly maps in commit, read and GC

**Goal**: Every metadata map is a prolly map in the live store, a commit with one change does work in proportion to the change, and the gain is measured on the product code (#215)
**Depends on**: Phase 25
**Requirements**: PMAP-03, PMAP-06, PMAP-07, PMAP-08
**Success Criteria** (what must be TRUE):
  1. After seed, mount, write, commit and remount, the files, directories, hard links, xattrs and snapshots are the same as before; the mount, snapshot and pjdfstest (4 workers) suites pass on a prolly store
  2. A commit with one change does work in proportion to the change: a counter test at two store sizes shows that the nodes read and written grow at most with the tree height
  3. GC mark reaches every prolly node of the live root and of each snapshot with bounded memory (no whole map in RAM; a test caps peak memory on a large map); online GC at mount still runs, frees nodes that no root reaches, and keeps the rest
  4. A commit cut at any point replays to a consistent tree: a crash test cuts the commit at each write and sync step and checks the tree after replay
  5. The Phase 24 benchmark on the product code shows: a one-change commit at 1,000,000 inodes or more does its pre-sync work in at most 1/5 of the flat time and its total wall time, sync included, is lower than flat (flat time measured with release `v2.3.0`; owner accepts a sync-bound total such as 0.7x); Boost numbers stay within the rubric in `docs/research/prolly-trees.md`; the insert/delete-heavy regression is measured and recorded with a mitigation or a written acceptance
**Plans:** 13 plans

Plans:
**Wave 1**
- [ ] 26-01-PLAN.md — PMAP-07 rule pre-registered before any run (checks A pre-sync 1/5, B total lower than flat, C Boost not drop), commit bench for both trees, early flat baseline of v2.3.0 (wave 1)

**Wave 2** *(blocked on Wave 1 completion)*
- [ ] 26-02-PLAN.md — prolly crate: bounded NodeCache and Ctx, range with seek, count, height, node visitor (wave 2)
- [ ] 26-03-PLAN.md — prolly-bench: retire the flat-store readers, IN-05 (--l 8192), bench.sh runs whole and scale (wave 2)

**Wave 3** *(blocked on Wave 2 completion)*
- [ ] 26-04-PLAN.md — Tracer: manifest_data as a prolly tree through overlay, commit, load, GC mark, rehash, counters and bench; IN-01 (wave 3)

**Wave 4** *(blocked on Wave 3 completion)*
- [ ] 26-05-PLAN.md — Commit protocol on the manifest tree: fault points, requeue of every overlay, races, online GC order, crash windows (wave 4)
- [ ] 26-06-PLAN.md — Boundary salt to every tree writer and rehash; --meta-cache-size mount option (wave 4)

**Wave 5** *(blocked on Wave 4 completion)*
- [ ] 26-07-PLAN.md — refcounts and xattr_data as trees (saturating RMW; inline or spilled xattr sets) (wave 5)

**Wave 6** *(blocked on Wave 5 completion)*
- [ ] 26-08-PLAN.md — inode_data as a tree with inline inode records; the overlay replaces the inode cache (wave 6)

**Wave 7** *(blocked on Wave 6 completion)*
- [ ] 26-09-PLAN.md — dir_data by name with . and .. entries, range readdir, dir_is_empty in the fs layer (wave 7)

**Wave 8** *(blocked on Wave 7 completion)*
- [ ] 26-10-PLAN.md — Format switch: flat code removed, IN-02 refusal (M19), store-format doc, Docker all + pjdfstest 4 workers + verify (wave 8)

**Wave 9** *(blocked on Wave 8 completion)*
- [ ] 26-11-PLAN.md — Scale tests: bulk loader, five-tree counter test at two sizes, GC peak memory, snapshots and online GC, snapshot cost record (wave 9)
- [ ] 26-12-PLAN.md — Crash matrix over all five trees with a cross-tree validator and random cut schedules (wave 9)

**Wave 10** *(blocked on Wave 9 completion)*
- [ ] 26-13-PLAN.md — PMAP-07 decision runs and verdict by the rule, insert/delete record, phase gate (wave 10)

### Phase 27: Store migration

**Goal**: A user can move an existing store to the new format with one documented procedure, and the procedure is tested between two real formats (#155)
**Depends on**: Phase 26 (the test copies a flat-map store into a prolly store)
**Requirements**: MIGR-01, MIGR-02, MIGR-03
**Success Criteria** (what must be TRUE):
  1. `scripts/migrate-store.sh` mounts the old store read-only with the old binary, mounts the new store with the new binary, copies with `rsync -aHAX`, and compares `slicefs stats` and a file checksum list; it exits with an error on any difference
  2. A Docker test copies a store made by the `v2.3.0` release (flat maps) into a store made by the branch binary (prolly maps), and the compare step passes; a planted difference makes the script fail (by hand with the log recorded, if CI cannot get the old binary)
  3. `docs/user/upgrade.md` says how to see the store format, which releases read which format, the copy steps, the disk space needed, how to check the result, and what to do on failure
**Plans**: TBD

### Phase 28: Crash and soak tests

**Goal**: Random crashes and long runs of a live mount on a prolly store show that synced data survives and that memory stays under its cap (#157)
**Depends on**: Phase 27
**Requirements**: CRSH-01, CRSH-02, CRSH-03, CRSH-04
**Success Criteria** (what must be TRUE):
  1. A Docker Linux test runs a mixed load (create, write in place, append, rename, unlink, fsync, snapshot) from several processes on a live mount of a prolly store, kills the daemon with `kill -9` at a random time, remounts, and checks: `slicefs scrub` exits 0, every fsynced file has its synced content, and no file has content that was never written
  2. A second mode stops the container instead of `kill -9`, with the same checks
  3. The test repeats N rounds with a recorded seed; a 50-round run passes; each run writes a short log (seed, rounds, result); it runs by hand and nightly on `ci/**` branches, not on each PR
  4. A soak mode runs the same load for hours with periodic unmount and mount and checks VmHWM against the memory cap; a 2-hour run passes
  5. `docs/design/write-log.md` has a "Crash tests" section that says what the test checks and how to run it
**Plans**: TBD

### Phase 29: Scrub damage report

**Goal**: When scrub finds a bad object, the user sees which files and snapshots it damages, in a form that scripts can read, and scrub memory stays bounded (#159)
**Depends on**: Phase 28
**Requirements**: SCRB-01, SCRB-02, SCRB-03, SCRB-04
**Success Criteria** (what must be TRUE):
  1. When scrub finds a bad object, it prints the paths that reference it, in the current root and in each snapshot
  2. Scrub memory is bounded: manifests are streamed, and no whole file and no full key set is held in RAM (#210); a measurement on a store with many objects shows peak memory that does not grow with the object count
  3. `slicefs scrub --json` gives the same report; exit codes are 0 (clean), 1 (corruption found) and 2 (scrub could not run); a test plants a bad chunk that two files share, and scrub lists both files and exits 1
  4. `docs/user/scrub.md` says when to run scrub, its run time, the output, the exit codes, and what to do after corruption; `slicefs scrub --help` matches it
**Plans**: TBD

### Phase 30: Limits page

**Goal**: A user can read on one page what state the product is in, what a crash can and cannot lose, and what the limits are, and each fact on the page is checked (#158)
**Depends on**: Phase 29 (the page cites the crash tests and the other results)
**Requirements**: DOCS-01, DOCS-02
**Success Criteria** (what must be TRUE):
  1. `docs/user/limits.md` gives the status (preview; the format can change until #156), the crash guarantees (from the write log and the crash tests, with links), the limits (1 TiB EFBIG cap, FastCDC 4/8/32 KiB default, memory options, the 64 MiB reorder window, one mount per store), the platforms (Linux libfuse; macOS FUSE-T with FSKit only, default 1 worker; no Windows), and the known issues with links
  2. Each fact on the page is checked against the code or a test, and the check names the source of each fact
  3. The README has a short "Status and limits" section with a link to the page
**Plans**: TBD

## Progress

**Execution order:** Phases run in numeric order: 25 → 26 → 27 → 28 → 29 → 30. They are one serial track.

| Milestone | Phases | Plans | Status | Shipped |
|-----------|--------|-------|--------|---------|
| v1.0 Foundation | 1-7 | 24 | Complete | 2026-03-29 |
| v2.0 Streaming Writes & Hardening | 7.1-12 | 13 | Complete | 2026-11-17 |
| v2.1 Store Index & Measurements | 13-17 | 27 | Complete | 2026-11-20 |
| v2.2 Concurrency & Store Evolution | 18-24 | 69 | Complete | 2026-11-26 |
| v2.3 Prolly Metadata & Robustness | 25-30 | 0 | In progress | - |

| Phase | Plans Complete | Status | Completed |
|-------|----------------|--------|-----------|
| 25. Prolly format and layout | 8/8 | Complete    | 2026-11-27 |
| 26. Prolly maps in commit, read and GC | 0/TBD | Not started | - |
| 27. Store migration | 0/TBD | Not started | - |
| 28. Crash and soak tests | 0/TBD | Not started | - |
| 29. Scrub damage report | 0/TBD | Not started | - |
| 30. Limits page | 0/TBD | Not started | - |
