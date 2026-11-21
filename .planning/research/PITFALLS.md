# Pitfalls Research

**Domain:** SliceFS v2.2 "Concurrency & Store Evolution": multithreaded FUSE requests (#127, #21, #22), `slicefs rehash` (#62), configurable FastCDC profiles (#67), prolly-tree evaluation (#19), added to an existing single-dispatch-thread Rust dedup filesystem.
**Researched:** 2026-11-20
**Confidence:** HIGH for items read from the current code (file and line references are repo-relative); MEDIUM for kernel, FUSE and prolly design knowledge; LOW where marked. No macOS concurrency result exists in this research: spike S1 needs a FSKit mount, which this session did not run. Nothing here states how FUSE-T or FSKit behave under overlap.

## Scope

`docs/research/multithreaded-request-handling.md` already covers the 15 single-thread assumptions, the take-out-of-map pattern, the per-inode lock table, log order, the dirty budget curve, group commit and the S1-S7 spike list. This file does **not** repeat them. It adds what that document misses: where it is stale, where the issue texts disagree with it, and the failure modes of #62, #67 and #19, which it does not cover at all.

Phase names below are topics, not numbers. v2.2 phases are not in `.planning/ROADMAP.md` yet (v2.1 ended at Phase 17). Topics used: **Spikes and baseline** (#127), **Lock foundation** (#21), **Budget and flusher**, **Worker pool** (#22), **Rehash** (#62), **CDC profiles** (#67), **Prolly evaluation** (#19), **Benchmark rules** (cross-cutting).

---

## Critical Pitfalls

### Pitfall 1: Designing against a stale baseline (research doc and issue texts are out of date)

**What goes wrong:**
Spikes and designs are built on facts that #108 already changed. The research doc is pinned to commit `6fd1e11`. Since then, the commit gate is held only for the generation cut (`crates/slicefs-cli/src/commit_timer.rs` header), the pack sync runs in the background (commits `bb9fa98`, `48b1921`), and `flush_wal` takes a handle, not the `wal` mutex (`crates/metadata/src/store.rs:453-463`). So S2 ("remove the inline pack fsync") and S5 ("1.3 s exclusive commit") test a problem that is mostly gone, and the 1124 / 1772 ms baseline is no longer the number to beat. Issue #21 is stale in another way: it describes a global `Mutex<StoreIo>` held during hashing and I/O, but `filesystem.rs` now has only 3 `io.lock()` sites. Meanwhile `keep_budget` (`filesystem.rs:1242-1273`) still removes another file's state from `open_files`, so that part of the doc is current.
The documents also **contradict each other**. #21 says "append to the log first, then take the inode lock to publish the extent" and uses `RwLock` + `ArcSwap` + `DashMap`; the research doc says "apply, then append under the inode lock" with a `Mutex`. Both cannot be built.

**Why it happens:**
The issue text was written in the v1 era and the research doc before #108 merged. Spikes are copied from the issue table without re-reading the code.

**How to avoid:**
First plan of the first phase: re-run the current bigwrite and commit-cut measurements (hold time of the cut, not of the whole commit; `flushing` set behaviour in `store.rs` `Live<T>`), record new baselines, then edit S2 and S5 to match. Write one short design note that picks log order (apply-then-append under the inode lock, per the research doc, because replay checks the inode number) and lock type, and mark #21's table as superseded. Update the issue bodies.

**Warning signs:**
A spike result "shows no improvement" because the thing it removes is already removed. A plan that mentions `ArcSwap` or `DashMap` for the inode table without a decision record.

**Phase to address:** Spikes and baseline.
**Confidence:** HIGH (code and git history read).

---

### Pitfall 2: Gate and inode-lock inversion through a background thread

**What goes wrong:**
The rank table puts the commit gate first, then inode locks. A new thread breaks it by taking an inode lock and then the gate. Example: the rebase flusher, the release path, or `destroy` holds `OpenFileState` and then calls `gate.read()`. Meanwhile the commit cut waits for `gate.write()` and a worker holds the gate shared while waiting for that same inode lock. Four parties, no progress. The cut also needs the state of every open file, and it can only take those locks safely if every holder of an inode lock also holds the gate shared. Any thread that holds an inode lock without the gate makes the cut block on a mutex it assumed was free.

**Why it happens:**
Today only the FUSE thread and the cut run, so the order was never tested. `std::sync::RwLock` fairness is implementation-defined and differs by platform; whether a waiting writer blocks new readers is the thing S5 must measure on Linux **and** macOS (LOW confidence on what each does; do not assume a Linux result carries to macOS).

**How to avoid:**
Gate first, always, for every thread type (flusher, timer, GC start, release, destroy). Add the debug-build rank checker in the first lock phase and make it fail tests, with a thread-local stack of held ranks. Add a test where all workers are parked on a throttled budget while a cut waits and a flusher needs the gate. Never wait on budget, durability or a commit while holding gate or inode lock (ZFS ERESTART rule from the research doc), so the admission step runs before either lock is taken.

**Warning signs:**
A hang with the commit timer log line "waiting for gate" and no progress; `fsync` p99 of seconds only under multi-writer tests; the checker panics in a handler that "never" takes two locks.

**Phase to address:** Lock foundation (checker), Budget and flusher (flusher ordering), Worker pool (re-run on N > 1).

---

### Pitfall 3: Replying to FUSE while holding a lock (and the static send lock)

**What goes wrong:**
A handler calls `reply.data(...)` inside its inode-lock scope. On FUSE-T the reply goes through `STREAM_SEND_LOCK`, a process-wide static `Mutex` held across the whole blocking write loop (`vendor/fuser/src/channel.rs:142-159`). If the socket peer is slow to read, every worker's reply queues on that mutex, and each of them may be holding an inode lock or a gate share. The system then looks like a global lock again. On Linux the same shape appears with large READ replies and a slow reader. Also, a `Reply` that is dropped without a call sends EIO (`vendor/fuser/src/reply.rs:150-159`): a panicking worker, an early `return`, or a queue discarded at unmount turns into spurious EIO, not a hang.

**How to avoid:**
Handlers return a value (`Result<Response, Errno>`); the dispatcher sends the reply after all guards are dropped. Make this structural, not a convention: lock guards must not be in scope at the reply call. For shutdown, drain the queue by replying (EIO or ENODEV) to each queued request, then join workers, then run `destroy`. Do not add a second reader thread on the stream socket: `receive_framed` takes no lock, so two readers can split one frame (the vendored fuser refuses `n_threads != 1` outside Linux; keep that check and add a test that it still refuses).

**Warning signs:**
`stat` latency rises when another process reads a large file over FUSE-T; "Reply not sent for operation" warnings in the log.

**Phase to address:** Worker pool.
**Confidence:** HIGH for the code facts; LOW for how FUSE-T behaves when the peer stops reading (not measured).

---

### Pitfall 4: Head-of-line blocking and pool starvation in the reader plus worker design

**What goes wrong:**
One reader thread feeds a queue bounded in bytes. When 4 big writers fill it, the reader blocks, and a `getattr`, `FORGET` or `INTERRUPT` from another process waits behind them. This rebuilds the single-thread latency the milestone is meant to remove. Worse, with N workers all asleep in the dirty-budget admission wait, the `RELEASE` and `fsync` requests that would free budget sit in the same queue and no worker is free to run them. With the default N=1 this deadlocks by itself.

**How to avoid:**
Count only WRITE payload bytes against the bound; metadata requests are never held back by it. Handle `FORGET` (no reply) and `INTERRUPT` on the reader thread. Do admission **before dequeue**: the dispatcher holds a WRITE until budget allows and hands it to a worker only then, so workers never sleep on budget. Keep a reserved lane (or one reserved worker) for release, fsync, flush and flusher-owned work. Add to S7 a probe that runs `stat` in a loop while 4 writers saturate the cap; its p99 is the acceptance number, not only peak RSS. Inline fast path on the reader for cache-hit `getattr`/`lookup`, because the queue handoff costs more than the work on macOS, where the pool may add cost with no overlap gain.

**Warning signs:**
Metadata latency tracks writer count; a hang at N=1 under a small `dirty_budget`.

**Phase to address:** Budget and flusher (admission), Worker pool (queue lanes).

---

### Pitfall 5: Waits that cannot be cancelled (FUSE_INTERRUPT is a TODO) and unmount hangs

**What goes wrong:**
`FUSE_INTERRUPT` is ignored by the vendored fuser (`vendor/fuser/src/request.rs:117-118`, "TODO: handle FUSE_INTERRUPT"). A writer that is paused by the "writer pays" sleep or by the hard-threshold condvar cannot be cancelled, so `kill -9` of the client process waits for the server. An unbounded wait on a stuck flusher (pack fsync stalls of 3.3 s and 9.4 s were measured per the research doc) means an unbounded hang. Unmount while the reader is blocked on a full queue, or while workers wait on a lock, can hang the unmount. `fuset_watch.rs` only ends the session when the mount disappears.

**How to avoid:**
Every wait has a timeout (condvar `wait_timeout`) and a fallback: after the cap, run a bounded step on the writer's own file, never on another file. Unmount order: stop the reader, reply to the queue, join workers, then `destroy`. Test: unmount and `SIGTERM` while 4 writers are throttled.

**Warning signs:**
A client process in D state after `kill -9`; the daemon does not exit after `SIGTERM` (signals.rs path).

**Phase to address:** Budget and flusher, Worker pool.
**Confidence:** HIGH (code), MEDIUM (kernel behaviour for non-interruptible FUSE waits).

---

### Pitfall 6: Lock-table identity bugs (eviction and lookup-then-lock)

**What goes wrong:**
(a) A sharded `HashMap<ino, Arc<Mutex<_>>>` that drops an entry when "nobody uses it" lets thread A hold an `Arc` to the old mutex while thread B inserts a new one for the same inode: two locks for one inode, silent corruption. (b) Locking in increasing inode order needs the child's inode number, and the child is found by reading the parent directory. For unlink, rmdir, rename and link the code reads the parent, drops it, locks both in order, and **must re-check** that the name still maps to the same inode. Without that, it unlinks the wrong thing after a concurrent rename. (c) `create` in a directory that a concurrent `rmdir` just removed leaves entries in an orphan directory; the create must check "directory still live" under the directory lock. (d) Inode numbers are not reused (monotonic), which is a help: entries need not be evicted while the inode is open or has lookups.

**How to avoid:**
Evict only when `Arc::strong_count == 1` under the shard lock, or never evict while the inode is in the inode cache. Write the lookup/lock/revalidate loop once as a helper for all two-inode operations. Stress test: create/rmdir/rename on the same few names from 8 threads, with `pjdfstest` kept as the single-thread semantic gate.

**Warning signs:**
Rare `ENOENT` for a name that exists; inode `nlink` mismatch found by scrub; orphan set grows.

**Phase to address:** Lock foundation.

---

### Pitfall 7: A read-while-write design that lets GC delete what a reader is still using

**What goes wrong:**
#21's lock-free read path copies an extent list or loads a root snapshot and then does `pread` without a lock. If a rebase step or commit installs a new manifest and an online GC runs its mark with the new root only, the objects of the old manifest are unmarked and can be swept while the reader still holds the old one. The `touched` protection covers only objects put or read **after** `gc_begin`, not objects a reader has not read yet. Today's `readers` cache in `filesystem.rs:968-988` re-opens when `r.key() != root`, which hides this with one thread. Long reads and the `readers` cache (up to 64 entries) widen the window with N workers.

**How to avoid:**
Readers register the root (or overlay segment `Arc`) they use as an in-flight GC root for the length of the read (an epoch or a counted pin set, the same mechanism the mark already uses for `file_roots` of open write handles). Add a race test beside `crates/chunk-store/src/race_tests.rs`: read a large file in a loop, rewrite it and run GC in a loop; the read must never return `Corrupt`/`NotFound`. Keep reads on overlay RAM under the inode lock or hold an `Arc` to the segment, so a rebase cannot move bytes mid-copy.

**Warning signs:**
Sporadic `Corrupt` or EIO on reads only when GC and writes run; zero failures in single-thread runs.

**Phase to address:** Lock foundation (design), Worker pool (stress).
**Confidence:** MEDIUM: the mechanism follows from the mark and sweep code read here, but the exact generation check in `ChunkStore::read` may already retry some cases. Verify before building the pin set.

---

### Pitfall 8: Panic and lock-poison policy under N workers

**What goes wrong:**
`filesystem.rs` has 59 `lock().unwrap()` calls. With one thread a panic ends the session. With N workers a panic inside a critical section poisons `open_files`, `readers` or `orphans`, and every other worker then panics on its next `unwrap`, each dropping its `Reply` (EIO to clients) while state may be half-updated. Continuing with `into_inner()` is worse: it serves requests from inconsistent state.

**How to avoid:**
Decide one policy: a worker panic aborts the process (`panic = "abort"` for the daemon, or a panic hook that calls `abort`). The write log and replay are the designed crash path and are tested, so abort-then-replay is safer than limping on. Add a test that kills the daemon from inside a handler at random points under N threads and checks replay.

**Warning signs:**
A burst of EIO after one panic; "PoisonError" in logs.

**Phase to address:** Worker pool (policy), Lock foundation (new locks use the same policy).
**Confidence:** MEDIUM (recommendation; it is a one-way-ish choice and should be confirmed with the owner).

---

### Pitfall 9: Dirty budget races and memory ceilings under N writers

**What goes wrong:**
Check-then-add on the budget (`total <= budget`, then add) lets N writers each pass the check and overshoot by N times the write size. Per-file atomics make the sum cheap but stale. More important for the "no whole file in RAM" rule: the budget covers overlays only. The real ceiling also includes the dispatch queue (payload copies), the retry layer's kept pack copies, and the per-writer buffer of `2 * CHUNK_MAX` (`chunk-store/src/file.rs:136`). With #67 profiles up to 512 KiB max, that buffer is 1 MiB per open writer; 1000 open files is 1 GiB, outside every budget.

**How to avoid:**
Reserve-then-release with `fetch_add` (admission reserves before the work, the work releases on completion). State one worst-case formula: budget + queue bytes + retry kept bytes + open writers x 2 x max chunk, and assert it in S7 with a hard RSS ceiling (`VmHWM`, as the 4 GiB test does). Count writer buffers into the budget or cap open streaming writers.

**Warning signs:**
`VmHWM` grows with the number of open files or with N; budget "overshoot" in the flusher lag graph.

**Phase to address:** Budget and flusher; CDC profiles (buffer size).

---

### Pitfall 10: Test coverage that cannot see the races ("passes with N workers")

**What goes wrong:**
The kernel serializes much of what you want to test. The vendored fuser sets only `FUSE_ASYNC_READ` and big writes (`vendor/fuser/src/lib.rs:107-113`), not `FUSE_PARALLEL_DIROPS` or writeback cache, and `max_background` is the fuser default of 16. So lookups and readdirs in one directory are serialized by the kernel, writes to one inode arrive one at a time in write-through mode, and `pjdfstest` is serial. A green run with N workers can mean no overlap happened. A scaling number measured only with fio `numjobs=4` on distinct files (`benchmarks/*.fio`) proves nothing about same-directory or same-file overlap, and `direct=1` skips the kernel page cache but not the daemon's own caches.

**How to avoid:**
Two harnesses. (1) An in-process stress harness that calls the handlers from threads directly, without the kernel, with random op mixes on a few shared files and directories, plus a replay-equality check (state after crash-replay equals state before). (2) Mounted runs for scaling, with explicit cases: distinct files, same file, same directory create, and a decision about setting `PARALLEL_DIROPS`. Keep long runs on Linux Docker only and the macOS mount run by hand, per the test-time rule.

**Warning signs:**
A max-in-flight counter that never exceeds 1 in a "multithreaded" test; identical throughput at N=1 and N=4.

**Phase to address:** Spikes and baseline (counter and harness), Worker pool.

---

### Pitfall 11: Turning the macOS pool on without an S1 result

**What goes wrong:**
FUSE-T and FSKit concurrency, and whether replies may complete out of order, are unknown (research doc; no result exists in this research). If FUSE-T keeps one request outstanding, the pool only adds handoff cost. If it overlaps but mishandles out-of-order replies, the symptom is a hung client, not an error.

**How to avoid:**
Default N=1 on macOS until S1 is recorded in the repo with the in-flight counter output. Ship the counter (max requests in flight, reply-order inversions) as a debug stat so S1 can be re-run after any FUSE-T upgrade. Run S1 by hand, never in automated tests.

**Phase to address:** Spikes and baseline (S1), Worker pool (default).
**Confidence:** HIGH that no result exists; LOW on FUSE-T behaviour.

---

### Pitfall 12: `rehash` rebuilds the file manifest 1:1 and gets different keys than a fresh write

**What goes wrong:**
The manifest node boundaries depend on the **child ID digest**: `is_cut` takes the last 4 digest bytes of each entry (`chunk-store/src/file.rs:41-46`). Change the hash and every cut moves. A rehash that rewrites each node in place (map old child ID to new, keep node shape) produces a tree whose root key differs from a fresh write of the same bytes under the new hash, and `splice` (which assumes the tree depends only on the chunk sequence) will then resync wrongly or never. Zero chunks are a second trap: the zero shortcut table is per hasher (`file.rs:235-245`), so zero-chunk IDs must be remapped to the new hasher's.

**How to avoid:**
Rebuild each file's manifest from its chunk sequence through the writer path: read the chunk list, map each chunk to its new ID, then push the entries through the same node builder a fresh write uses. Test: for many sizes (0, 1, one chunk, 4096+ entries to force a second level, sparse files of zeros), `rehash(key) == fresh_write(bytes)` under the new hash. A preexisting latent case: after `--hash` today, `splice` copies old entries and adds new ones, so mixed trees already differ from fresh keys. Add a test that documents it now; rehash is its cure.

**Warning signs:**
Rehashed file keys differ from a freshly copied file; no dedup between a rehashed and a new copy of the same file.

**Phase to address:** Rehash.
**Confidence:** HIGH (code).

---

### Pitfall 13: `rehash` rewrites things that are not object IDs, and misses references that are

**What goes wrong:**
Directory name keys are always BLAKE3-256 and must stay so (`crates/metadata/src/directory.rs:253`; `hash_change_tests.rs` header). A rehash that finds "every 33-byte key" and rewrites it breaks lookups. In the other direction, references live outside the chunk store: snapshot roots are `SnapshotRecord` entries in WAL segments (`crates/metadata/src/segment/mod.rs:16`, kept by `compaction.rs`), the root record has the six map keys and a `REFCOUNTS` map keyed by `ObjectKey`, the `FORMAT` marker names the write hash, and write-log records after the last root carry bytes plus inode numbers. If old snapshot records stay in the segments, the GC keeps every old object live forever and the store doubles. Two old IDs of equal content collapse to one new ID, so `refcounts` entries (a sorted `BTreeMap`, order changes with the hash) must be merged by summing, not overwritten.

**How to avoid:**
Walk by typed structure using the same traversal that GC uses (`referenced` / `content_keys`), never by byte pattern. End-of-run check: enumerate all reachable IDs and assert every tag is the target, with an explicit allow-list of name keys. Emit superseding snapshot records for the same version and confirm (and test) that load takes the latest record for a version; segment compaction must keep only the new one after the switch. Require a clean store (no write-log tail) or replay it first.

**Warning signs:**
`ls` fails for some names after rehash; store size does not drop after GC; scrub passes but `stats` shows two tags.

**Phase to address:** Rehash.
**Confidence:** HIGH (code); the "latest record wins" behaviour of snapshot load is not verified here.

---

### Pitfall 14: `rehash` and GC race (new objects swept, old objects kept alive)

**What goes wrong:**
GC sweeps objects of packs up to its boundary that are not in `live` **and** were not put or read since `gc_begin` (`chunk-store/src/lib.rs:1491-1545`, `gc_finish`). New-hash objects that rehash wrote before `gc_begin` and that no root reaches yet are unmarked, so a concurrent GC deletes them; the next root then points to missing objects. In the other direction, rehash reads every old object, so during a GC they all enter `touched` and survive that cycle (the auto-GC schedule already runs a follow-up GC to free false positives of the live filter, `online_gc.rs`). The "bytes freed" the job reports after the first GC will be wrong.

**How to avoid:**
Treat in-progress rehash output as GC roots (the `file_roots` mechanism that already protects open write handles), or hold GC off for the job by taking the GC mutex. Free-space accounting: report freed bytes after the second GC. Check free space before starting (the store needs about 2x the object bytes until the old objects are swept; `gc_pacing` has a low-space mode to reuse).

**Warning signs:**
`NotFound` for a new-hash ID after a crash-free run with GC on; old objects remain after "rehash done".

**Phase to address:** Rehash.
**Confidence:** HIGH (sweep rule read in code), MEDIUM (exact `touched` semantics for reads of objects already in the index).

---

### Pitfall 15: `rehash` launders corruption

**What goes wrong:**
If the job copies the stored bytes or hashes what it reads without verifying the old ID, a bit-rotted chunk gets a valid new ID. Scrub then passes, and the data is silently wrong forever.

**How to avoid:**
Rehash reads through the verifying read (hash the bytes with the old tag's hasher and compare, as `ChunkStore::read` does). A mismatch aborts the job, names the object and the owning path, and leaves the old root. Never skip a bad object. Test with a flipped byte in a pack.

**Phase to address:** Rehash.

---

### Pitfall 16: `rehash` crash windows, resume and "done"

**What goes wrong:**
There are several separate durable steps with no single atomic action: new objects, new file manifests, new metadata nodes, the `RootUpdate`, each superseding `SnapshotRecord`, and the `FORMAT` marker (a separate file written by atomic rename, `content.rs:93-103`). A crash between any two leaves a legal mixed store, because IDs are self-describing; the danger is a design that assumes "marker == new hash means done". Also a mount with `--hash` can change the write hash mid-job, and a build that does not know new marker lines ignores them (`write_hasher` skips unknown lines).

**How to avoid:**
Make every step idempotent and every prefix legal. Persist a small job journal (target hash, phase) by atomic rename. Define done as "a scan finds no reachable ID with another tag" plus a final pass under exclusive access. Order: write all new objects, then all new roots, then one switch record, then marker. Test with a failpoint after each fsync (kill the process, mount, scrub, resume, compare file contents and snapshot contents by content hash, not by ID). The existing `node_crash_tests.rs` pattern in the metadata crate is the model.

**Phase to address:** Rehash.

---

### Pitfall 17: "Refuse while mounted" is not sound with `mount.lock`

**What goes wrong:**
`acquire_mount_lock` checks `exists()` and then writes (`crates/metadata/src/mount_lock.rs:41-50`): a race, no PID, and the file stays after a crash. An offline rehash that trusts it either refuses a dead mount forever or, in the race, runs next to a live mount. Two writers on one store (pack directory and index MANIFEST) corrupt it.

**How to avoid:**
Take an OS file lock (`flock`) on a store lock file for the lifetime of a mount and of rehash; the OS releases it on crash, which also fixes stale-lock detection. Online rehash must run inside the daemon through the same request-file protocol as `slicefs gc` (`online_gc.rs`), never as a second process opening the store read-write.

**Phase to address:** Rehash.

---

### Pitfall 18: Rehash memory and write volume break the v2.1 wins

**What goes wrong:**
An in-RAM old-to-new ID map costs about 66 bytes per object plus hash-map overhead: 6.6 GB or more at 100M objects, against 2.2-3.3 B per object for the index. The job doubles the object count until GC, doubling index entries and triggering the background merge (#141 shows idle merges can rewrite GBs).

**How to avoid:**
No global map. Per-file recompute (read chunk, hash with the new hasher, `put`, which dedups against what exists) with a small bounded cache for hot shared nodes; or a sorted-run side file built with `chunk-store/src/extsort.rs`. State the trade-off: with high dedup ratios an LRU re-reads shared chunks many times, which a sorted sidecar avoids. Pace the job like GC (`gc_pacing`), and report progress, bytes rewritten and bytes freed.

**Phase to address:** Rehash.

---

### Pitfall 19: CDC parameters are not persisted, and the implementation hides parameters outside min/avg/max

**What goes wrong:**
Changing min/avg/max by a mount flag silently changes chunking of new data: dedup against old data collapses, and `splice` never resyncs with old cuts, so it re-chunks to the end of the file (a performance cliff, and keys that differ from a full write). Several inputs to determinism are not recorded today: the normalization level (`FastCDC::new` defaults to level 1 inside `chunk_ranges`, `chunk-store/src/lib.rs:108`), the seed, and the exact `fastcdc` crate version (a crate upgrade can move boundaries). The marker parser skips unknown lines, so an older build mounts a store with CDC lines and chunks with defaults.

**How to avoid:**
Record an algorithm identifier plus all parameters (`fastcdc-v2020 min avg max norm=1 seed=0` and the crate version) in the store marker, fixed at first write, rejected on change at mount (a `rechunk` is a separate, explicit job). Bump the `FORMAT` number so old builds refuse the store. Add a golden-vector test: fixed bytes, exact expected cut offsets, so a crate upgrade or refactor fails loudly. Add a second test that a default store produces byte-identical keys to v2.1 fixtures.

**Warning signs:**
Dedup ratio drops after a flag change; `splice` timing jumps on large files.

**Phase to address:** CDC profiles.
**Confidence:** HIGH (code and crate source read).

---

### Pitfall 20: Hard-coded constants and a process-wide static make profiles unsafe

**What goes wrong:**
`CHUNK_MIN/AVG/MAX` are used as constants in at least `chunk_ranges()`, the `2 * CHUNK_MAX` write buffer and the read-ahead step (`file.rs:111-136, 688-716`), the zero shortcut (`file.rs:555-575`), and `content.rs:384`; `FileWriter` derives `Default`, so there is nowhere to carry parameters. Worst: `zero_chunk()` is a `static OnceLock` built from `CHUNK_MAX` for all hashers (`file.rs:235-245`). With a per-store max, the first store opened in the process wins; another profile gets a zero chunk of the wrong length and the zero shortcut returns wrong bytes. That is data corruption, not a perf issue. The `fastcdc` crate also asserts on bounds (min 64 B to 1 MiB, avg 256 B to 4 MiB, max 1 KiB to 16 MiB) and panics in the daemon on a bad config, and `logarithm2` rounds a non-power-of-two average.

**How to avoid:**
A `ChunkProfile` value owned by the `ChunkStore` and passed to writer, splice, reader and overlay; remove the static (key the zero table by profile, or compute it per store). Validate at config parse: powers of two, `min < avg < max`, inside crate bounds, with a clear error. Test two stores with different profiles in one process, and a sparse-file round trip per profile.

**Phase to address:** CDC profiles.
**Confidence:** HIGH.

---

### Pitfall 21: Prolly evaluation uses the wrong baseline and the wrong target

**What goes wrong:**
Issue #19 says `commit()` serializes each map in full. In the current code, per-inode, per-manifest and per-xattr objects are written only when dirty (`Live` caches with dirty sets, `store.rs`). What is still O(files) is: the six digest maps (`inode_map`, `inode_data`, `dir_data`, `manifest_data`, `xattr_data`, `refcounts`: `u64 -> key`, 41 bytes per entry, about 3.2 MB per map at 78k entries, estimated, not measured) and **each directory's entry list, stored as one node and re-serialized when any entry changes** (`directory.rs` header). A benchmark against "full re-serialization of everything" overstates the gain. File manifests are already content-cut trees with byte lengths (`SFM1`: 64 minimum entries, 1 in 512 content cut, 4096 maximum). The "file manifests" half of #19 is largely done.

**How to avoid:**
Baseline = current code. First measure per-commit bytes by map and by directory size (the 78k-file Boost tree, plus one directory of 100k entries, which is the likely real hotspot), and the entries-per-node spread of existing manifests. Prototype one structure only: a map `u64 -> key`, or a large directory. Sweep the number of changed entries per commit (1, 100, 1k, 10k, clustered like `cp -r` versus scattered like timestamp updates). Compare bytes written, commit time, on-disk size after several commits and GC, and cold point-lookup latency.

**Phase to address:** Prolly evaluation.
**Confidence:** HIGH on the code facts, MEDIUM on the size estimate.

---

### Pitfall 22: Prolly boundaries chosen from values or child IDs

**What goes wrong:**
If a node boundary is decided from the entry value (an `ObjectKey` that changes on every update), each update can create or remove a boundary and cascade into its neighbours: write amplification grows and diffs between versions stop being local. If the boundary is decided from the child ID digest (as `is_cut` does for manifests), then `rehash` reshapes every tree, as in Pitfall 12. If boundary probability is a pure function of the hash, node sizes have a wide geometric spread (Dolt uses a size-dependent probability to narrow it, per issue #19; not verified here, LOW). Note the existing manifest rule has the same spread: with a 64-entry minimum and a 1/512 cut, nodes run from 64 to 4096 entries (about 2.7 KiB to 168 KiB at 41 bytes per entry); measure this before copying it.

**How to avoid:**
Decide boundaries from the **key** only, with a fixed hash that does not depend on the store hash (as name keys always use BLAKE3-256), so rehash keeps the shape. Use a size-aware boundary rule and measure the node-size histogram first. Keep the tree policy independent of the FastCDC profile (the #67 design invariant): a test that changing the chunk profile does not change map-tree shapes, and the reverse. Batch changes per commit: sort the changed keys and rebuild each path once.

**Phase to address:** Prolly evaluation (rule), Rehash (shape requirement).

---

### Pitfall 23: A new node kind that GC, scrub and rehash do not traverse

**What goes wrong:**
If prolly nodes (or any new node kind) are written but the GC mark does not enumerate their children, the next GC sweeps live data. Three traversals exist or will exist (GC mark, scrub, rehash), and each can drift. With structural sharing across snapshots, a mark without a visited set re-walks shared subtrees once per snapshot.

**How to avoid:**
One `children(node)` function used by all three, and a test that writes with every node type, runs GC, and reads everything back. Add a visited-subtree skip to the mark. Do not wire a prototype into the root record in v2.2 (evaluation only; the format change is a later decision).

**Phase to address:** Prolly evaluation (rule: prototype stays outside the root), Rehash (designs the shared traversal first).

---

### Pitfall 24: Benchmarks that cannot support the decision

**What goes wrong:**
(a) **Warm versus cold**: a Docker VM (OrbStack) shares one kernel page cache across runs; `drop_caches` needs privilege there and `purge` needs root on macOS; fio `direct=1` bypasses the kernel cache but not the daemon's own caches (the `readers` map, memtable, mapped index runs, Bloom filters). Cold open of the on-disk index depends on the page cache by design. (b) **Fsync cost is not comparable across OS**: on macOS plain `fsync` does not force media flush; Docker on a virtual disk differs again; S6 and any pack-sync numbers cannot be mixed across platforms. (c) **Dataset**: most Boost files are under 32 KiB, so they are a single chunk under every profile and a Boost run barely moves object count; fio data is either random (no dedup at all, every profile equals ratio 1.0) or, if a buffer repeats, extremely dedupable. Neither tests a profile. Use trees with edits (insert, delete, modify at beginning, middle and end, as #67 lists) and large files such as VM images or archives. (d) **Confounds**: larger objects compress better with per-object zstd, so dedup loss and compression gain offset; report physical bytes **after GC and after index merge settles** (`wait_index_settled`). (e) **Random-read amplification**: a 4 KiB read from a 128 KiB average object decompresses the whole object; measure cold random-read latency per profile, not only throughput. (f) Averages hide stalls; the repo's own metric (slowest single write, p99) should be the headline, with several repetitions and reported spread.

**How to avoid:**
A shared benchmark rule sheet checked into `benchmarks/`: fresh process plus cache drop for "cold", repetition count, medians and spread, platform recorded, dataset recipe (seeded edits on a base tree), size measured after GC, and a statement of which cache each run touches. Reuse `benchmarks/chunk-bench` and `index-bench`. Verify what fio's default buffer fill does to dedup before trusting any fio number (LOW: not tested here).

**Phase to address:** Benchmark rules first, then CDC profiles, Prolly evaluation, Spikes.

---

## Moderate Pitfalls

### Pitfall 25: Log timestamp and order inside locks

**What goes wrong:** The request clock is thread-local (`oplog.rs`). If the timestamp is read before the inode lock, a later write can get an earlier mtime or ctime. **Prevention:** Read the clock inside the inode lock; assert ctime never decreases per inode in the stress harness. **Phase:** Lock foundation.

### Pitfall 26: Failed-log state is global

**What goes wrong:** `wal_fail_seq != wal_fail_cleared` makes every `fsync` fail until the next commit (`store.rs:453-458`). With N workers, many requests fail at once, and a retry storm hits the commit path. **Prevention:** One test with injected append failure under N threads; make the failure state visible in stats. **Phase:** Worker pool.

### Pitfall 27: Marker changes shared by #62 and #67

**What goes wrong:** Both add per-store parameters to the `FORMAT` marker. Two ad-hoc edits give two parsers that skip each other's lines. **Prevention:** One structured parameters block with a parser that rejects unknown keys, and one format-number bump. **Phase:** Rehash and CDC profiles; decide it in whichever starts first.

### Pitfall 28: Larger chunks and rebase read cost

**What goes wrong:** The rebase step reads ahead by `CHUNK_AVG`, doubling (`file.rs:688-691`). With a 128 KiB average, each overlay step reads and re-chunks more bytes per small edit, and the 73-112 ms p99 rebase step time recorded by #122 may move. **Prevention:** Re-run the random-write bigwrite tests per profile as part of the #67 benchmark. **Phase:** CDC profiles.

### Pitfall 29: `readers` cache lock held during the whole read

**What goes wrong:** `read_root` holds the `readers` mutex across `Reader::read` (`filesystem.rs:981-988`), so N workers serialize on reads even after inode locks are fine. **Prevention:** Clone an `Arc<Reader>` under the lock and read outside it (research doc step 1). **Phase:** Lock foundation.

---

## Technical Debt Patterns

| Shortcut | Immediate Benefit | Long-term Cost | When Acceptable |
|----------|-------------------|----------------|-----------------|
| Turn on N workers before the lock rank checker exists | Faster demo | Deadlocks found in production | Never |
| Keep take-out-of-map and add a mutex around `open_files` | Small patch | EBADF and stale reads remain under N | Never |
| Global old-to-new ID `HashMap` in rehash | Simple code | Memory use grows with objects | Only for stores under about 1M objects, behind a hard limit and a clear error |
| Read profile from a CLI flag, not the marker | Easy A/B runs | Silent dedup collapse | Only in a benchmark binary that never opens an existing store |
| Prototype prolly tree wired into the root record | End-to-end demo | Format change with no decision | Never in v2.2 |
| Pool default N > 1 on macOS | Looks faster | May add latency with no overlap | Only after S1 is recorded |

## Integration Gotchas

| Integration | Common Mistake | Correct Approach |
|-------------|----------------|------------------|
| FUSE-T stream socket | Second reader thread or reply under a lock | One reader; replies after guards are dropped; keep the send lock short |
| fuser `Reply` | Dropping it on shutdown or panic | Reply to every queued request before exit; abort on worker panic |
| Online GC | New unreferenced objects before `gc_begin` | Register job output as GC roots |
| Segment compaction | Old snapshot records survive a rehash | Superseding record plus compaction test |
| `fastcdc` crate | Treating the numbers as the whole algorithm | Record algorithm, normalization, seed and crate version; golden vectors |

## Performance Traps

| Trap | Symptoms | Prevention | When It Breaks |
|------|----------|------------|----------------|
| Handoff cost on tiny ops | `getattr` latency up at N > 1 | Inline fast path on the reader | Always on macOS if no overlap |
| Single pack appender | MB/s flat as N grows | S3 store-lock histograms; per-worker packs only after proof | About 3-4 workers at 530-910 MB/s hashing |
| Kernel dir mutex | Same-directory creates do not scale | Decide on `PARALLEL_DIROPS`; benchmark it separately | Any N |
| Flat map re-serialization | Commit time linear in files | Prolly prototype or defer | Hundreds of thousands of files |
| Rehash index growth | RAM and merge load double | Pace the job; free old objects with GC | Stores near 100M objects |
| Large profile writer buffers | RSS grows with open files | Count in the memory formula | 512 KiB max with 1000 open files |

## Security Mistakes

| Mistake | Risk | Prevention |
|---------|------|------------|
| Rehash skips verification of old objects | Corruption gets a valid new ID | Verifying read; abort on mismatch |
| Accepting a profile or hash from an untrusted store marker without validation | Panic (crate asserts) or huge allocation | Validate bounds at mount before any allocation |
| Debug stats expose paths | Information leak in shared logs | Log inode numbers, not names, in lock and queue traces |

## UX Pitfalls

| Pitfall | User Impact | Better Approach |
|---------|-------------|-----------------|
| `rehash` prints no progress or an exit status that hides resume | User kills it, then fears a broken store | Progress, "safe to stop" message, `--resume` is automatic |
| Profile flag on an existing store | Confusing silent no-op or silent change | Hard error that names the stored profile |
| Hang on unmount with pending throttled writers | User force-kills the daemon | Bounded waits and an ordered shutdown |

## "Looks Done But Isn't" Checklist

- [ ] **Worker pool:** Often missing a max-in-flight counter; verify N=4 shows more than 1 request in flight under the stress harness.
- [ ] **Per-inode locks:** Often missing the revalidate step after ordered locking; verify create/rmdir/rename stress.
- [ ] **Log order:** Often missing a replay-equality test under N threads; verify crash then replay equals pre-crash state.
- [ ] **Budget:** Often missing the formula for queue, retry copies and writer buffers; verify `VmHWM` ceiling in S7.
- [ ] **Read while write:** Often missing the GC pin; verify read, rewrite and GC loop without errors.
- [ ] **Rehash:** Often missing the key-equality test with a fresh write; verify across sizes and multi-level manifests.
- [ ] **Rehash:** Often missing snapshots; verify each snapshot's file contents match before and after.
- [ ] **Rehash:** Often missing a crash after each durable step; verify with failpoints.
- [ ] **CDC profile:** Often missing persistence; verify a mount with a different flag is refused.
- [ ] **CDC profile:** Often missing the zero-shortcut per store; verify two profiles in one process.
- [ ] **Prolly:** Often missing GC traversal; verify GC then read all data.
- [ ] **Benchmarks:** Often missing size after GC; verify the report states cold or warm for each number.

## Recovery Strategies

| Pitfall | Recovery Cost | Recovery Steps |
|---------|---------------|----------------|
| Rehash interrupted | LOW | Re-run `rehash`; steps are idempotent; GC frees leftovers |
| Rehash laundered corruption (found later) | HIGH | Restore from a copy made before; the verifying read is the only prevention |
| New objects swept by GC during rehash | MEDIUM | Re-run rehash from the old root (old objects are still referenced by the old root until the switch) |
| Wrong CDC profile applied | MEDIUM | Re-chunk job is a new feature; until then, restore the marker value and accept lower dedup for the new data |
| Deadlock in the pool | LOW | Set N=1 (default); crash replay path restores state |
| Lock-poison cascade | LOW | Abort policy; replay on next mount |

## Pitfall-to-Phase Mapping

| Pitfall | Prevention Phase | Verification |
|---------|------------------|--------------|
| 1 stale baseline | Spikes and baseline | New numbers recorded; issue texts updated |
| 2 gate/inode inversion | Lock foundation, Budget and flusher | Rank checker plus parked-worker test |
| 3 reply under lock | Worker pool | Structural test; unmount under load |
| 4 head-of-line, starvation | Budget and flusher, Worker pool | `stat` p99 under 4 writers |
| 5 uncancellable waits | Budget and flusher, Worker pool | Timeout test; `SIGTERM` under throttle |
| 6 lock identity | Lock foundation | Stress on shared names |
| 7 GC under readers | Lock foundation, Worker pool | Read/rewrite/GC loop |
| 8 panic policy | Worker pool | Abort and replay test |
| 9 budget and memory | Budget and flusher | RSS ceiling in S7 |
| 10 blind tests | Spikes and baseline | In-flight counter in CI |
| 11 macOS without S1 | Spikes and baseline, Worker pool | S1 result file; default N=1 |
| 12 manifest shape | Rehash | Key equality with fresh write |
| 13 wrong references | Rehash | Reachable-tag scan with allow-list |
| 14 GC race | Rehash | GC running during rehash test |
| 15 laundering | Rehash | Flipped-byte test |
| 16 crash windows | Rehash | Failpoint after each fsync |
| 17 mount lock | Rehash | `flock` race test |
| 18 rehash memory | Rehash | RSS bound at synthetic large store |
| 19 CDC persistence | CDC profiles | Golden vectors; refusal test |
| 20 constants and static | CDC profiles | Two profiles in one process |
| 21-23 prolly | Prolly evaluation | Baseline report; GC-after-prototype test |
| 24 benchmark validity | Benchmark rules, then each | Rule sheet attached to each report |

## Sources

- `docs/research/multithreaded-request-handling.md` (not repeated here); issues #19, #21, #22, #62, #67, #127 (`gh issue view`).
- Code read in this repo: `crates/chunk-store/src/{file.rs,hash.rs,lib.rs}`, `crates/metadata/src/{store.rs,directory.rs,mount_lock.rs,gc/mod.rs,segment/}`, `crates/slicefs-cli/src/{filesystem.rs,content.rs,commit_timer.rs,online_gc.rs}`, `vendor/fuser/{PATCHES.md,src/channel.rs,src/reply.rs,src/request.rs,src/lib.rs}`, `benchmarks/*.fio`; git log for #108 (HIGH).
- `fastcdc` 3.2.1 source in the local cargo registry: bounds asserts and `logarithm2` (HIGH).
- Kernel and FS design points (per-inode lock rank, ZFS and jbd2 wait rules) come from the research doc's cited sources (MEDIUM). Dolt boundary-probability detail comes only from the text of issue #19 (LOW). `std::sync::RwLock` fairness, fio buffer defaults, and FUSE-T behaviour were not verified (LOW).
