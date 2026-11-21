# Feature Research

**Domain:** Deduplicating POSIX FUSE filesystem, milestone v2.2 "Concurrency & Store Evolution" (multithreaded request handling, store rehash, configurable CDC profiles, prolly-tree evaluation)
**Researched:** 2026-11-20
**Confidence:** HIGH for what exists in the repo and for the issue acceptance criteria. MEDIUM for how comparable systems behave (vendor docs, cross-checked against the repo's own research note). LOW where marked (forum posts, inference).

This file replaces the v2.0 version. It covers only the new v2.2 work. Existing features (streaming writes, write log, on-disk chunk index, online GC, snapshots) are inputs, not scope.

---

## What Already Exists (and Changes the Picture)

Reading the repo before the issues showed that some issue text is older than the code. These facts shape every section below.

| Fact (source) | Effect on v2.2 |
|---------------|----------------|
| File manifests are already a content-cut tree. A leaf holds up to 4,096 `(ObjectId, length)` entries, a cut falls after an entry whose ID matches a 1-in-512 pattern, an index node holds `(node ID, byte length)`, and a read does a binary search on the lengths (`docs/design/chunk-store.md`, "File manifest"; `crates/chunk-store/src/file.rs`). | The file-manifest half of #19 is largely built. It is a prolly tree in all but name. #19 is left with the metadata maps. |
| The metadata maps (inode, directory, manifest, xattr, refcount) are written whole at each commit, but through the chunk store. One changed file in 5,000 cost 12 KB on the next commit, against 1.36 MB for the first (`docs/design/chunk-store.md`, "Objects"; `tests/node_crash_tests.rs`). | Byte savings from a prolly map are already partly captured by chunk dedup. The benchmark must measure CPU time, serialization time and peak memory, not only bytes written. |
| Object IDs are tagged (1-byte tag + 32-byte digest). The store records its write hash in the `FORMAT` marker (`hash <name>`). `--hash` on `mount` and `seed` changes it. Each object is verified with the hasher of its own tag (`crates/chunk-store/src/hash.rs`, `crates/slicefs-cli/src/content.rs`). | The seams for #62 exist. Mixed-hash stores already read, scrub and GC. |
| A release build registers only `blake3-256`. The second hasher, `TestKeyedBlake3` (tag `0xf0`), exists only under `cfg(test)` or the `test-hasher` feature (`crates/chunk-store/src/hash.rs`, `crates/*/Cargo.toml`). | `rehash` cannot be run by a user, or tested end to end in a release build, until a second real hasher is registered. #62 puts that choice out of scope. This is a roadmap decision (see Open Questions). |
| The manifest node cut `is_cut` reads the last 4 digest bytes of each entry ID (`docs/design/chunk-store.md`, "Object IDs and the hash"). | A rehash changes every ID, so it moves every cut. Manifest trees must be rebuilt from the new `(ID, length)` list. Copying the old node shape with new IDs gives a non-canonical tree. |
| Directory-entry keys are always BLAKE3-256 of the name, not a content address (same section). | Rehash must leave directory-entry keys alone. |
| `CHUNK_MIN/AVG/MAX` are `pub const` (4/8/32 KiB) in `crates/chunk-store/src/lib.rs`. `CHUNK_MAX` is also used in `file.rs` for the exact-zero shortcut (a 32 KiB zero run), for the `2 * CHUNK_MAX` carry buffer, and in splice. | #67 touches more than `chunk_ranges()`. Every use of `CHUNK_MAX` must read the store's profile. |
| Splice guarantees that the output key equals the key of a full `FileWriter` write of the same bytes, "whatever the write history" (`docs/design/chunk-store.md`, "Splice (#98)"). | A chunk profile must be fixed for the life of a store. Mixed profiles in one store break this guarantee (see #67). |
| The FUSE session runs one thread. The vendored fuser refuses `n_threads != 1` and `clone_fd` outside Linux (`docs/research/multithreaded-request-handling.md`). Write, settle, setattr and `keep_budget` take `OpenFileState` out of `open_files` while they work. | #21 is not only "add locks". It must remove the take-out pattern first. |

---

## Feature Landscape

### Table Stakes (Users Expect These)

Each group lists what a user or a reviewer expects once the feature is named, based on the issue acceptance criteria and on how comparable systems behave.

#### A. Multithreaded request handling (#127, #21, #22)

Comparable systems: libfuse `fuse_session_loop_mt` (each worker reads, handles and replies; the server does its own per-inode locking), Linux VFS and XFS (per-inode lock with a fixed rank), jbd2/btrfs/ZFS (writers join a transaction, commit waits for the drain), Linux writeback and ZFS dirty-data throttling (the writer pays by sleeping).

| Feature | Why Expected | Complexity | Notes |
|---------|--------------|------------|-------|
| Stress harness: N client threads doing mixed write, read, fsync, open/close and rename on shared files, with per-op latency histograms and lock-hold timers | No multithreaded FUSE test exists. Without it, none of the races listed in the research note would be caught. #21 requires parallel reader/writer tests. | MEDIUM | Research-note step 0. Everything else depends on it. Keep CI runs short (test-time budget). |
| Spike S2 first: pack fsync moved off the store lock | Needs no threads. Removes the measured 0.35-1.7 s tail. Decides whether #108 closes without threads. | MEDIUM | Touches `ChunkStore::put` / `RetryingPack::write_at` (`crates/chunk-store/src/lib.rs`, `retry.rs`). The retry layer's kept copies need a hard cap. |
| Per-inode state stays in the map behind one shared lock (`Arc<Mutex<OpenFileState>>`), and alias handles share it | This replaces the take-out pattern that causes EBADF, stale reads, no-op fsync, duplicate alias state and skipped finalize under concurrency. | HIGH | `crates/slicefs-cli/src/filesystem.rs` (about 15 sites), `handlers.rs`. Per-file dirty cost mirrored in an atomic. |
| Lock rank and a debug-build order checker: commit gate (shared), rename mutex, inode locks by increasing inode number, short map locks, store/WAL internals | Same rule as the VFS and XFS. Needed to meet "no deadlocks" in #21. | MEDIUM | Cross-directory rename also needs the "source is not an ancestor" check. |
| Read while write on one file: the writer publishes its extent before it replies, the reader copies the extent list under a short read lock and then does its I/O without the lock | Core acceptance criterion of #21. Each write is all-or-nothing to a concurrent read. | HIGH | Extent overlay in `crates/slicefs-cli/src/overlay.rs`. |
| No global lock in the read or write path; no lock held across disk I/O, hashing or a wait | Core acceptance criterion of #21. `read_root` holds the `readers` mutex across the whole read today, and `flush_wal` holds the `wal` mutex across `flush_and_sync`. | MEDIUM | Narrow `readers` to an `Arc<Reader>` clone. Move WAL sync outside the mutex. |
| Log record appended while the inode lock is held, and replay with the logged inode number | Otherwise two creates can be logged out of allocation order, and replay refuses the mount (`got == *ino`). | MEDIUM | `handlers.rs`, `oplog.rs`. Format change is allowed (no deployments). A crash-replay test with N threads must run on every change to `handlers.rs`. |
| Lost-update fixes: read-modify-write of inode and metadata under the inode lock; namespace check-then-act under directory locks | Correctness under any concurrency. | MEDIUM | `install_root`, inode helpers, unlink vs open. |
| Dirty budget under N writers: one global atomic total, a rebase flusher thread under the shared gate, soft and hard thresholds, writer-pays sleep, no cross-file inline rebase | Bounded memory is a standing project rule. #127 S7 sets the thresholds. | HIGH | Removes `keep_budget` on another file's state. Fsync and release paths are never throttled. The throttle waits before the gate and inode lock are taken. |
| Reader thread + bounded worker pool, N configurable, default 1 until measured | #22: "N workers configurable by a mount option". One code path on Linux and macOS. Drain workers before `destroy()`. | HIGH | Queue bounded in bytes (each queued WRITE costs one payload copy). |
| Parallel I/O on different files scales with workers (measured); all tests pass with more than one worker on Linux and macOS FSKit | #22 acceptance criteria. | MEDIUM | S3 gives the numbers. |
| A test with every worker throttled while a commit waits | Guards the gate / budget / flusher deadlock the research note names as the largest risk. | MEDIUM | Spike S5 decides the gate lock type. |

#### B. Conditional features (a spike decides whether they exist)

These are not table stakes. Each is built only if its spike says so.

| Feature | Decided by | Build if | Complexity |
|---------|-----------|----------|------------|
| macOS worker pool above N=1 | S1 (FUSE-T/FSKit overlap and reply order, run by hand) | More than one request is in flight and out-of-order replies succeed. Otherwise leave macOS at N=1. | MEDIUM. Also needs a lock on `receive_framed` in the vendored fuser if more than one thread ever reads the stream socket. |
| Per-worker pack files | S3 (store-lock hold and wait at N = 1, 2, 4, 8) | The single appender is the bottleneck at the chosen N. FastCDC + BLAKE3 run at 530-910 MB/s, so a few workers could saturate one appender. | HIGH |
| Commit-gate generation switch (the old generation is written out with the gate released) | S5 (longest wait of a new shared holder while a commit waits) | Wait is long at 78k files (an exclusive hold is about 1.3 s today). | HIGH |
| WAL group commit (one flusher thread, `durable_lsn` + condvar) | S6 (fsyncs per second and p99, per-op vs flusher, tar-like load) | Throughput gain is large, especially on macOS. | MEDIUM |
| `FUSE_WRITEBACK_CACHE` trial | S4 (concurrent requests per inode) | Per-inode lock is contended in write-through mode. | LOW to try, HIGH to make correct |
| Persistent per-inode extent map behind `ArcSwap` (#21 option B) | A profile that shows readers waiting | Reader waits show up in the lock-hold timers. The extent map must sit behind a small interface so this change stays local. | MEDIUM |

#### C. `slicefs rehash --to <algorithm>` (#62)

Comparable systems: git SHA-1 to SHA-256 (new object format, a local bidirectional ID map, conversion cost is a full re-hash and re-encode, old Git versions fail on the new format); restic (chunker parameters are fixed at repository creation, `copy` to a new repository, `migrate` for repository version, `prune --repack-uncompressed` to rewrite old data); borg 2 (`borg transfer` copies archives into a new repository, re-chunks only when `--chunker-params` is given, `--dry-run`, an interrupted transfer is safe and resumes, nothing is deleted from the source). No comparable system rewrites object IDs in place. They copy to a new repository, or keep a translation map.

| Feature | Why Expected | Complexity | Notes |
|---------|--------------|------------|-------|
| Bottom-up rewrite with a durable old-ID to new-ID map | Every reference must change: manifest entries, manifest nodes, metadata nodes, roots, snapshots. A parent can only be written after its children are mapped. | HIGH | The map is large: 100M objects at 66 bytes per pair is 6.6 GB. It must be on disk and bounded in memory. Reuse the sorted-run machinery of the chunk index (`crates/chunk-store/src/index*`). |
| Rebuild manifest trees from the new `(ID, length)` list, not by copying node shape | `is_cut` reads the digest bytes, so cuts move when IDs change. A copied shape would differ from what `FileWriter` makes, and splice's "same key as a full write" guarantee would fail. | HIGH | Oracle test: the rehashed key of a file equals the key of a fresh write of the same bytes under the new hash. |
| Leave directory-entry keys alone | They are BLAKE3 of the name, not content addresses. | LOW | |
| Crash safety: a crash at any point leaves a store that mounts with the old root or the new root | Stated requirement. Same shape as git's and borg's flow: write new objects, switch last. | HIGH | Write new objects (they are unreachable, so garbage if the run dies). One atomic switch: new roots/snapshots plus the `hash` marker line (the marker already uses temp-file + rename + dir sync, `write_marker`). Free old objects only after the switch. |
| Refuse while the store is mounted (use the mount lock) and refuse with a non-empty write log | Simplest safe mode. Replay must finish first, because log records carry object keys. | LOW | `crates/metadata/src/mount_lock.rs`. |
| Old objects freed by GC after the switch | Stated requirement. | LOW | Existing offline `slicefs gc` / online GC. |
| Progress, bytes rewritten, bytes freed | Stated requirement. borg and restic both report progress. | LOW | Follow the `--json` style of existing CLI reports. |
| `--dry-run` | borg's recommended flow is dry-run, transfer, dry-run again. | LOW | Count objects and bytes; no writes. |
| Resume after an interrupted run, with no re-work of finished objects | borg transfer resumes; a rehash of a large store is long. | MEDIUM | Needs the durable map. Idempotent writes make it safe, since objects are content-addressed. |
| Tests: a mixed-algorithm store ends with one algorithm, files and snapshots unchanged, scrub passes, crash injection at each step | Stated requirement. | MEDIUM | Needs a second hasher; the test hasher exists under `test-hasher`. |

#### D. Configurable FastCDC profiles (#67)

Comparable systems: restic (parameters fixed at repository creation; matching them is needed for dedup when copying), kopia (splitter chosen at repository creation, forum posts only, LOW), borg (`--chunker-params` per archive, `borg recreate --chunker-params` re-chunks old archives at a space cost of at least the full deduplicated size, `borg transfer --chunker-params`), casync/desync (`--chunk-size min:avg:max`).

| Feature | Why Expected | Complexity | Notes |
|---------|--------------|------------|-------|
| min/avg/max set per store at creation, recorded in the `FORMAT` marker, default 4/8/32 KiB when the line is absent | #67 and the "no silent change of parameters for existing content" rule. Every comparable tool fixes the profile when the repository is made. | MEDIUM | Marker handling in `crates/slicefs-cli/src/content.rs`. Pass the profile through `chunk_store::Options` (as `hasher` and `zstd_level` already are). |
| Refuse a profile change on a non-empty store | Splice and the oracle tests assume that the same bytes always give the same chunks. Mixed profiles in one store give mixed chunk sizes within a file and break dedup against old data. | LOW | If a later milestone wants a change, it is a rechunk tool (borg `recreate` / restic `copy` shape), not a mount flag. |
| Validation of min < avg < max and of the FastCDC crate's bounds | The crate rejects out-of-range values. A bad value must fail at creation, not at the first write. | LOW | Verify the exact bounds against the pinned `fastcdc` version. |
| Every use of `CHUNK_*` reads the store profile: `chunk_ranges`, the zero-chunk shortcut, the `2 * CHUNK_MAX` carry buffer, splice, the reader's buffer sizing | A profile larger than 32 KiB makes a hard-coded 32 KiB zero run or buffer wrong. | MEDIUM | The `2 * max` carry buffer per open writer grows with the profile (512 KiB max gives 1 MiB per writer). Check it against the dirty budget. |
| Reproducible benchmark scripts for 4/8/32, 8/16/64, 8/32/128, 16/64/256, 32/128/512 KiB on real datasets and successive tree versions | #67 acceptance criteria. | MEDIUM | Measure: physical bytes after dedup + zstd including pack and index overhead; object count, index RSS, cold-open; ingest MB/s and CPU; sequential and random read latency; rewrite bytes and objects after insert/delete/modify at start, middle and end. |
| A recommendation based on the numbers, with 4/8/32 as the default unless the user decides otherwise | PROJECT.md decision. | LOW | The user decides a default change (one-way for new stores). |
| File chunk size independent of manifest node policy | #67 design invariant. Already true: the manifest cut depends on entry IDs (1 in 512, 64-4,096 entries), not on chunk size. | LOW | State it in the benchmark write-up, and test that one profile does not change the manifest policy. |

#### E. Prolly-tree evaluation (#19)

Comparable systems: Noms (prolly trees for blobs and maps), Dolt (key-only chunking, target size about 4 KB, boundary probability from a distribution function and the current chunk size so node sizes cluster, history independence, structural sharing, root-hash diff that skips equal subtrees; a single-row update rewrites one leaf and the path to the root, roughly node size times depth; inserts and deletes can move a boundary with small probability).

| Feature | Why Expected | Complexity | Notes |
|---------|--------------|------------|-------|
| Prototype of one map (the directory map) as a prolly tree on the existing chunk store, with the same `ObjectId` and `ObjectHasher` | #19 deliverable. Nodes must be ordinary chunk-store objects so GC, scrub and pack storage need no change. | MEDIUM | Prototype, off the commit path, behind a feature or an example binary. |
| Benchmark against current full serialization on the Boost trees (about 78,000 files per version) | #19 acceptance criteria. | MEDIUM | Report commit time, bytes written per commit, and on-disk size after several small-change commits. Add commit CPU and peak memory, because bytes are already low (12 KB for one change in 5,000). |
| Node-size policy that uses the current node size when it decides a boundary | Dolt does this to avoid wide node-size spread. Plain hash cuts give a geometric distribution. | MEDIUM | Part of the prototype. |
| Written decision (adopt, adopt later, or drop) with the numbers | The issue is an evaluation; it feeds #15 and #18. | LOW | |

### Differentiators (Competitive Advantage)

| Feature | Value Proposition | Complexity | Notes |
|---------|-------------------|------------|-------|
| Online rehash cooperating with online GC | The store stays mounted during a long conversion. | HIGH | Needs a GC-safe way to keep the new, still-unreachable objects (they sit outside every root until the switch, so a mark and sweep would free them). The offline run is the table-stakes version. Defer. |
| Rehash with verify-after-copy (read each new object back, check its ID, check the content hash of the old and new copies) | Catches a bad rewrite before the switch. borg's recommended second `--dry-run` serves the same purpose. | MEDIUM | Cheap to add because scrub code already verifies by tag. |
| Rehash `--dry-run` that reports the map size and the expected freed bytes | Lets a user plan disk space (the old and the new objects coexist until GC). | LOW | |
| Lock-hold histograms kept as a permanent debug-build feature | Turns S3/S5/S7 into repeatable tests, not one-off spikes. | LOW | |
| Persistent (structurally shared) extent map per inode | Readers never take a lock. | MEDIUM | Only if profiles show waiting (conditional list above). |
| Prolly-tree diff between two snapshot roots | Gives a cheap "what changed" for sync and incremental backup, which fits the decentralized goal. | HIGH | Evaluate, do not build in v2.2. |
| Per-store chunk profile choice per workload (for example a media store with large chunks) | Fewer objects for large immutable files. | LOW once D exists | Per store, not per file. |

### Anti-Features (Commonly Requested, Often Problematic)

| Feature | Why Requested | Why Problematic | Alternative |
|---------|---------------|-----------------|-------------|
| Range locks for writes inside one file | Looks like the way to parallelize writes to one file. | The kernel has no mainline range lock that performs; a 2019 XFS benchmark showed it slower than a shared rwsem. The overlay already absorbs reordered writes. | A per-inode lock, and S4 to see if it is contended. |
| Async runtime (tokio) for request handling | "Modern" concurrency. | The work is CPU-bound (FastCDC, BLAKE3, zstd) and the callbacks are synchronous. `tokio::fs` runs blocking calls on a thread pool anyway. | OS threads, bounded channels, `parking_lot`. |
| Waiting for a commit, a budget or a durable LSN while holding the gate or an inode lock | Simple to write. | Deadlock: the flusher that frees budget cannot take the gate. ZFS returns ERESTART and the caller drops all locks first. | Admit before taking locks; fsync releases locks before it waits. |
| A writer that does rebase work for another file | Cheap way to cap memory. | The 2011 Linux writeback mistake. It is also the most thread-unsafe code today. | The rebase flusher thread plus writer-pays sleep. |
| Several reader threads on the FUSE-T stream socket | Linux `clone_fd` does this. | `receive_framed` has no lock. Two readers can split one frame. | One reader, N workers. |
| FUSE passthrough or io_uring | Throughput. | They do not change which requests overlap. A chunk store has no single backing file for passthrough. fuser has no io_uring transport. | Skip. |
| In-place rehash that rewrites objects where they sit | Saves disk. | A crash leaves a mix of IDs with no safe root. Content-addressed packs are append-only by design. | Write new objects, switch the root, GC the old (the borg and git shape). |
| Rehash that copies old manifest-node shapes with new IDs | Looks faster. | The cut depends on digest bytes, so the result is not canonical. | Rebuild the manifest from the `(ID, length)` list. |
| Choosing the second hash algorithm in this milestone | Makes `rehash` testable in release. | #62 puts the choice out of scope. It is a decision for the user, not a research default. | Test hasher for tests. Raise the decision at roadmap time. |
| Changing the FastCDC profile of a store that has data | "I want bigger chunks now." | Old and new content stop deduplicating, splice's oracle guarantee fails, and old files keep the old chunks. restic cannot do it; borg needs a full `recreate`. | Choose at creation. A rechunk tool is a separate, later issue. |
| Per-file or size-class chunk profiles | Large media vs small edits. | Parked by the user (see "Chunk size deferred" decision). Adds profile metadata per file and breaks cross-file dedup. | Benchmark the five store-wide profiles first. |
| Changing the default from 4/8/32 KiB inside the benchmark issue | A faster path to "better". | The user decides the default after the numbers. The v2.1 index (2.2-3.3 B of RAM per object) already removed most of the memory motivation. | Report, recommend, wait. |
| Migrating the store to prolly-tree metadata in v2.2 | The natural next step after a good benchmark. | #19 asks only for a prototype and a benchmark. A format migration also needs #62-style tooling. | Decide first; schedule a migration as its own milestone if the numbers justify it. |
| Using the `prolly-map` crate for the metadata maps | Saves 1,000-2,000 lines. | It is pre-1.0 (0.7.x, first released July 2026) with breaking changes in minor versions, and it fixes node IDs to SHA-256. It cannot produce SliceFS `ObjectId`s with the `ObjectHasher` abstraction. | A small in-tree prototype that reuses the manifest `is_cut` idea and the chunk store. Use the crate only as a design reference. |
| Making `getlk`/`setlk` correct inside #21 | It appears in the single-thread dependency list. | Wrong even with one thread. Needs separate work. | Open a separate issue. |

---

## Feature Dependencies

```
Stress harness (step 0)
    └──required by──> every locking change below

S2: pack fsync off store lock ──decides──> #108 closes without threads
S1: FUSE-T overlap and reply order ──decides──> macOS pool above N=1

Per-inode state stays in map (shared lock)
    ├──requires──> stress harness
    ├──enables──> log append under inode lock + replay by logged inode
    ├──enables──> lock rank + order checker
    └──enables──> read while write (#21)
                      └──requires──> narrowed `readers` lock, WAL sync outside mutex

Rebase flusher + writer-pays budget (S7)
    └──requires──> per-inode state in map, global atomic dirty total

Worker pool (#22)
    ├──requires──> all of the above (steps 0, 2-5)
    ├──requires──> drain-before-destroy
    └──enhanced by──> WAL group commit (S6), gate generation switch (S5)

Chunk profile in marker (#67)
    ├──requires──> every `CHUNK_*` use reads the profile
    └──independent of──> prolly work (manifest cut is by entry ID)

Rehash (#62)
    ├──requires──> tagged IDs (exists), second registered hasher (decision)
    ├──requires──> durable old->new ID map, bottom-up rewrite, manifest rebuild
    ├──requires──> mount lock + clean write log
    └──followed by──> GC of old objects
Rehash ──conflicts with──> concurrent mount (offline version)

Prolly prototype (#19)
    ├──requires──> chunk-store objects + ObjectHasher (exists)
    └──benchmark shares──> Boost-tree dataset and scripts with #67
```

### Dependency Notes

- **#22 requires #21:** The worker pool is safe only after the take-out pattern is gone and no global lock serializes callbacks. Research-note steps 1-5 make the daemon correct under any concurrency even at N=1, so they are worth doing whatever S1 says.
- **Log order requires the inode lock:** replay refuses the mount if two creates were logged out of allocation order.
- **#62 and #67 touch the same marker:** both add a line to the `FORMAT` marker (`hash`, and a chunker line). Plan one marker-parsing change, not two, and keep the temp-file + rename + dir-sync write.
- **#62 and #19 interact:** a prolly-tree store would hold node IDs inside nodes, so a later rehash must rebuild trees as it does for manifests. Decide #19 before an adoption, not before the prototype.
- **#67 does not need #19:** The issue says not to block on the tree migration. File manifests already cut by entry ID, so the "tree fixed, chunk size varied" benchmark axis is true by construction.
- **#62 conflicts with a mounted store (offline version):** The mount lock gives a clear refusal.
- **Benchmark harness is shared:** #19 and #67 both use the Boost trees and successive versions. Build one dataset runner.

---

## Conflicts Between Sources (Flag, Do Not Resolve Here)

| Topic | Issue text says | Research note / code says | Needs |
|-------|-----------------|---------------------------|-------|
| Log append and locks (#21) | "Never hold a lock during disk I/O"; the log append takes a short mutex only to get the LSN and position, and data is copied outside the lock. | Append each record while the inode lock is held, right after the apply, so that replay order equals apply order. | Decide that the append under the inode lock is a memory copy plus an LSN, with the disk write and the sync on the flusher. Confirm it with the harness. |
| Mechanism of #22 | "Configure `fuser` with more than one worker thread (a mount option)." | The vendored fuser refuses `n_threads != 1` and `clone_fd` outside Linux. The note recommends one fuser reader thread and a SliceFS-owned worker pool, the same on both platforms. Linux `n_threads` stays a later option. | Treat #22 as the worker pool, not a fuser option. Update the issue text at roadmap time. |
| #19 file manifests | Lists offset-indexed file manifests as new work. | Already built (content-cut tree with byte lengths). | Reduce #19 to the metadata maps. |
| #62 and release builds | Needs a second algorithm for tests only. | Only `blake3-256` is registered in release builds. | Decision on a real second hasher (see Open Questions). |

---

## MVP Definition

### Launch With (v2.2 core)

- [ ] Spikes S2 then S1, then S3-S7, with the stress harness — their results decide the conditional rows
- [ ] #21: shared per-inode state, lock rank + checker, log append under the lock, narrowed global locks, read while write, rebase flusher and dirty budget
- [ ] #22: reader + worker pool, N configurable, default 1; default raised on Linux first
- [ ] #62: offline `slicefs rehash` with durable map, manifest rebuild, atomic switch, progress, `--dry-run`, tests with the test hasher
- [ ] #67: store-wide profile in the marker, all `CHUNK_*` uses converted, benchmark scripts and report, default kept
- [ ] #19: one-map prototype and benchmark, written decision

### Add After Validation (v2.2.x)

- [ ] macOS pool above N=1 — after S1 shows overlap and out-of-order replies work
- [ ] WAL group commit — after S6 shows a large gain
- [ ] Gate generation switch — after S5 shows long waits
- [ ] Rehash resume and verify-after-copy — if the first long run needs them
- [ ] Per-worker packs — only if S3 shows the single appender is the limit

### Future Consideration (v2.3+)

- [ ] Online rehash with online GC
- [ ] Prolly-tree metadata migration and snapshot diff for sync
- [ ] Rechunk tool for an existing store (a new profile applied to old data)
- [ ] Real second hash algorithm in release builds (if the user decides it)
- [ ] Idle-merge control (#141), several roots and refs (#86-#92), index layout (#110), GC tuning (#121)

---

## Feature Prioritization Matrix

| Feature | User Value | Implementation Cost | Priority |
|---------|------------|---------------------|----------|
| Stress harness | HIGH | MEDIUM | P1 |
| S2 pack fsync off the store lock | HIGH | MEDIUM | P1 |
| Per-inode shared state, lock rank, log order | HIGH | HIGH | P1 |
| Read while write | HIGH | HIGH | P1 |
| Rebase flusher + dirty budget | HIGH | HIGH | P1 |
| Worker pool (Linux) | HIGH | HIGH | P1 |
| Offline `rehash` | MEDIUM | HIGH | P1 (the user chose #62 for this milestone) |
| Store-wide chunk profile + benchmark | MEDIUM | MEDIUM | P1 |
| Prolly prototype + benchmark | MEDIUM | MEDIUM | P2 |
| macOS pool above N=1 | MEDIUM | MEDIUM | P2 (S1) |
| WAL group commit | MEDIUM | MEDIUM | P2 (S6) |
| Gate generation switch | MEDIUM | HIGH | P2 (S5) |
| Rehash resume + verify | MEDIUM | MEDIUM | P2 |
| Online rehash | LOW | HIGH | P3 |
| Per-worker packs, writeback cache, persistent extent map | LOW | HIGH | P3 (spike-driven) |

---

## Comparable Systems

| Capability | Comparable systems | Our approach |
|------------|--------------------|--------------|
| Parallel request handling | libfuse `fuse_session_loop_mt`: each worker reads, handles and replies; the server does its own locking. Kernel file systems: per-inode lock with a fixed rank. | One reader, a SliceFS worker pool, per-inode locks by inode number. Same model on both platforms because FUSE-T gives no safe multi-reader stream. |
| Commit vs writers | jbd2, btrfs, ZFS: writers join the open transaction, the commit waits for the drain, a new transaction opens at once. | Keep the shared/exclusive gate for now. Generation switch only if S5 shows stalls. |
| Dirty-data control | Linux writeback and ZFS: soft threshold wakes a flusher, then a smooth sleep for the writer, a hard wait at the cap. | Same shape, with a SliceFS rebase flusher. |
| Hash change | git: new object format plus a local ID map and full re-hash; old clients fail. restic and borg: copy into a new repository; borg `transfer` is resumable, with `--dry-run`. | Rewrite into the same store, then switch the root. Durable ID map. Offline first. |
| Chunk parameters | restic, kopia: fixed at creation. borg: per archive, `recreate`/`transfer` to change at a space cost. casync/desync: `min:avg:max` option. | Fixed per store at creation, recorded in the marker. |
| Prolly trees | Noms, Dolt: history-independent, structural sharing, diff by root hash. Node size aims at about 4 KB, boundary probability depends on the current node size. | Prototype for one map. The file manifest already uses the idea. |

---

## Open Questions for the Roadmap

1. Which second hash algorithm gets registered in release builds, if any? Without one, `rehash` has no user-visible target. This is the user's decision (one-way for new IDs on disk).
2. Does rehash stay offline-only in v2.2? The research says yes. Online needs a GC protocol for unreachable new objects.
3. #21 vs the research note on the log append lock (see Conflicts).
4. #22 mechanism: worker pool vs fuser threads (see Conflicts).
5. Exact `fastcdc` bounds for min, avg and max in the pinned version. Check before the validation rules are fixed.
6. Where the chunk profile is set: `seed` only, or also `mount` on an empty store. Recommend creation time only.

## Sources

Repository (HIGH, primary):
- `.planning/PROJECT.md`; GitHub issues #19, #21, #22, #62, #67, #127
- `docs/research/multithreaded-request-handling.md` (itself cites kernel docs: FUSE, locking, directory-locking, journalling, writeback)
- `docs/design/chunk-store.md` (file manifest, object IDs and hash, splice)
- `crates/chunk-store/src/hash.rs`, `crates/chunk-store/src/file.rs`, `crates/chunk-store/src/lib.rs` (`CHUNK_*`, `Options`), `crates/slicefs-cli/src/content.rs` (marker, `--hash`)

External (tiers from the confidence seam: fetched docs LOW, cross-checked search MEDIUM; used for expected behavior only, not for decisions):
- Git hash function transition design, git-scm.com docs, `hash-function-transition` (fetched; MEDIUM after cross-check with prior knowledge)
- restic docs, "Working with repositories" (chunker parameters fixed at creation, `copy`, `migrate`, `prune --repack-uncompressed`) (fetched; MEDIUM)
- borg docs, `borg transfer` (latest) and `borg recreate` (fetched; MEDIUM); a web search summary of the Borg 2 man pages agrees
- Dolt, "How to Chunk Your Database into a Merkle Tree" (2022 blog) and the Dolt prolly-tree architecture page (fetched; MEDIUM). Note: the "Weibull" claim in a 2025 Dolt post was found only through a search summary and is LOW. The pages fetched say the target distribution is chosen by the implementer and "pseudo-Gaussian" in the figure.
- docs.rs `prolly-map` 0.7.2 (fetched; maturity and SHA-256 node IDs; MEDIUM)
- kopia splitter behavior: community forum posts only (LOW). The 16:64:256 `--chunk-size` default is from desync (a casync-compatible tool), not casync (LOW).
