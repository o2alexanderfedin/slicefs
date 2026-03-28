# Pitfalls Research

**Domain:** Deduplicating FUSE Filesystem (Rust, CAS-based, cross-platform)
**Researched:** 2026-03-27
**Confidence:** HIGH (multiple authoritative sources: USENIX FAST papers, ZFS post-mortems, FUSE benchmark studies, official crate documentation)

---

## Critical Pitfalls

### Pitfall 1: Reference Count Corruption Under Concurrent Deletion

**What goes wrong:**
When a file is deleted while a background GC pass is scanning live blocks, a block can be decremented to zero and freed before the deletion completes atomically — or conversely, a newly-written block that duplicates an existing block gets freed prematurely if the GC marks the block unreachable before the new reference is committed. This is the #1 cause of silent data loss in deduplicating storage.

**Why it happens:**
Developers treat reference counting as a simple increment/decrement, but in a CAS system the invariant "refcount == 0 means deletable" must hold across a write that establishes a reference AND a commit that makes that reference durable. Any window between "compute hash" and "commit reference" is a TOCTOU race. USENIX FAST 2013 documented this as a fundamental challenge requiring epoch-based or generation-number protocols to solve correctly.

**How to avoid:**
- Use a write-ahead log (WAL) or journal that records the *intention* to add a reference before the reference is counted as live.
- Implement a two-phase GC: mark phase (find all live hashes from committed metadata) and sweep phase (free unreferenced blocks), with the constraint that any block referenced in an in-progress transaction is never swept.
- Never decrement a refcount and free a block in the same atomic operation without holding a lock that prevents concurrent writers from adding references to the same block.
- Consider epoch-based deletion: a block can only be freed when its refcount has been zero for at least two full GC cycles.
- In Rust: model reference state transitions as an explicit state machine, not a bare `AtomicU64`.

**Warning signs:**
- Test suite passing but integration tests with concurrent readers + deletes producing checksum errors.
- Spurious "block not found" errors during read that disappear on retry.
- Any code path where `refcount.fetch_sub(1)` is not in the same transaction as the metadata deletion.

**Phase to address:** Core CAS + metadata layer (foundational phase); enforce in the first implementation of the GC.

---

### Pitfall 2: GC Race — New Write Lost During Mark Phase

**What goes wrong:**
A GC mark phase scans all live inodes/blocks and produces a "live set." Between the start of the scan and the sweep, a new file is written whose blocks are not in the live set snapshot. The sweep phase then deletes blocks that are actually referenced, causing data loss without error.

**Why it happens:**
Mark-and-sweep GC on a live filesystem cannot take a consistent snapshot without either pausing all writes or using a protocol that safely handles concurrent mutations. Most implementations get this right for the common case but miss the edge case where a write begins *after* the mark starts but *before* the sweep completes.

**How to avoid:**
- Maintain a "pending references" list: any block referenced by an in-progress write is added to a protected set before the write commits. GC never sweeps blocks in this set.
- Alternatively, use reference-counted blocks with strict "no free if refcount transitions through zero while any write transaction is open" semantics.
- For a local single-node filesystem, a readers-writer lock on the GC pass (write lock for the sweep phase, read lock held by all active write transactions) is practical and correct.
- Test with a "GC chaos" mode that injects GC cycles between every write step.

**Warning signs:**
- GC completes without error but subsequent reads of recently written files fail.
- GC trigger timing matters: bugs that only appear when GC runs frequently.
- Any GC implementation with a "collect all hashes first, then delete" structure without transaction coordination.

**Phase to address:** GC implementation phase; do not ship GC without a formal proof or exhaustive concurrency test.

---

### Pitfall 3: Crash Inconsistency — Orphaned Blocks and Dangling References

**What goes wrong:**
Two distinct failures:
1. A write commits new blocks to the block store but crashes before committing the metadata reference. Result: blocks exist but no inode references them — orphaned blocks consuming space forever.
2. A metadata update commits the reference but the blocks are not fully written. Result: a dangling reference pointing to nonexistent or partial block data — data corruption on read.

**Why it happens:**
CAS-based systems require two durable operations to be atomic together: (a) write the block, and (b) record the reference in metadata. Filesystems frequently crash between these two steps. Research on crash-consistency bugs found 10 new bugs in mature Linux filesystems (CrashMonkey, 2021), and deduplication adds new failure windows beyond what standard journaling handles.

**How to avoid:**
- Write blocks to the CAS store first, flush to durable storage, then commit the metadata reference. This ensures orphans (recoverable via GC) rather than dangling references (unrecoverable without additional mechanisms).
- Journal or WAL the metadata reference with "write intent" semantics: the log entry is the ground truth; block existence is confirmed on recovery.
- On startup, run a fast orphan scan (check all blocks with refcount == 0 that are older than a threshold) and either GC them or verify they are referenced.
- Use `fdatasync` / `fsync` at the correct points; missing a sync is the single most common crash-consistency bug.
- Use a crash-testing harness (e.g., a trait that injects failures at arbitrary storage operations) in the test suite from the beginning.

**Warning signs:**
- No explicit ordering between block write and metadata commit in the code.
- `fsync` is called at the end of a batch operation rather than at phase boundaries.
- Recovery logic that assumes "if metadata exists, all referenced blocks exist."

**Phase to address:** Storage layer + metadata layer; write the recovery path before shipping the write path.

---

### Pitfall 4: FUSE Context-Switch Overhead Destroying Small-Write Performance

**What goes wrong:**
Small I/O operations (4K writes) perform at ~20% of native filesystem speed on FUSE. Each FUSE write triggers a kernel/userspace context switch, and FUSE3 adds an extra `getattr` call per write to check for external changes. For a deduplicating filesystem, each write also triggers a hash computation, block lookup, and potential metadata update — compounding the overhead. The resulting latency is 3–4× worse than native for write-heavy workloads.

**Why it happens:**
FUSE's architecture places all VFS requests in a shared pending queue, causing lock contention under concurrent I/O. The userspace daemon round-trip doubles the effective path length of every syscall. The `getxattr` security capability call (on Linux) cannot be cached at the kernel level and fires on every write. USENIX FAST 2017 quantified FUSE worst-case overhead at 83% degradation.

**How to avoid:**
- Enable `writeback_cache` in fuser: batches small writes into 128KB requests, dramatically improving sequential write throughput. Trade-off: data in cache is lost if the daemon crashes — acceptable for a local filesystem with crash consistency guarantees at the storage layer.
- Disable `FUSE_CAP_AUTO_INVAL_DATA` for local-only filesystems where external modification is not possible.
- Hash computation should be done on write-path data *before* the context switch back to the kernel, not on the kernel-received data, to overlap work with scheduling latency.
- Use a buffer pool to avoid per-operation allocation in the write hot path.
- Benchmark the write path early with `fio` and establish latency/throughput baselines. Do not wait until the end.

**Warning signs:**
- Sequential 4K write throughput below 100MB/s on NVMe.
- CPU profiling shows the FUSE daemon thread spending >20% of time in lock/unlock or queue operations.
- Any write path that does a blocking metadata lookup per block without batching.

**Phase to address:** FUSE integration and write path phase; baseline benchmarks must be part of phase acceptance criteria.

---

### Pitfall 5: Dedup Table Memory Explosion (The ZFS DDT Lesson)

**What goes wrong:**
The deduplication index (hash → block location mapping) grows to consume all available RAM, then spills to disk, causing a read-modify-write cycle on every block write. At this point deduplication makes writes *slower* than not deduplicating. ZFS's DDT (Dedup Table) is the canonical example: each entry is ~320 bytes of kernel slab memory; a 16TB pool with 4K blocks has 4 billion potential entries (~1.2TB of RAM required).

**Why it happens:**
Developers size the index for their test data volume, not for the worst-case live production dataset. Unique blocks — blocks that are never duplicated — still require index entries, consuming memory with zero deduplication benefit. ZFS found that in general-purpose workloads, most blocks are unique, making the index size proportional to total storage rather than duplicate storage.

**How to avoid:**
- Implement a probabilistic pre-filter (Bloom filter) before the main index lookup. This eliminates most lookups for unique blocks at the cost of a small false-positive rate.
- Separate "unique" entries (refcount == 1, never seen a duplicate) from "shared" entries (refcount > 1). Aggressively evict or age out unique entries using an LRU with a configurable memory budget.
- The dedup index must have a configurable memory cap with a graceful degradation path: when the index exceeds the cap, fall back to "no dedup for new blocks" rather than crashing or thrashing disk.
- Design the index as a pluggable trait from the start. The in-memory hash map is fine for development; the production implementation needs disk-backed B-tree with memory-mapped access.
- Target: 5GB of RAM per TB of *actual duplicated* data, not per TB of total storage.

**Warning signs:**
- Index memory grows linearly with total bytes written, not with duplicate bytes found.
- Memory usage grows without bound during a write benchmark.
- Dedup ratio is below 1.05× (trivial) but memory consumption is high.

**Phase to address:** Core dedup engine design (first phase); the memory budget must be a first-class design constraint, not an optimization.

---

### Pitfall 6: Block Size Selection Locking In the Wrong Tradeoffs

**What goes wrong:**
A fixed block size that is too large (e.g., 1MB) produces low deduplication ratios because partial block changes force re-storing entire blocks. A fixed block size that is too small (e.g., 512B) produces excellent dedup ratios but a pathologically large index, severe fragmentation on reads, and per-block metadata overhead that exceeds the storage savings. The block size cannot easily be changed after data is written without a full migration.

**Why it happens:**
Block size feels like a configuration detail but determines the fundamental tradeoff surface of the entire system. Developers pick a "round" number (4K, 64K) without benchmarking against the actual workload data types. Variable-length chunking (CDC — content-defined chunking) exists precisely to avoid this trap, but adds complexity.

**How to avoid:**
- Since the owner has existing CDC technology, lean on it from the start. Variable-length chunking with target chunk size configurable at mount time is the correct approach.
- If fixed block size must be used initially, make it a mount-time parameter (not compile-time), defaulting to 64K which offers a reasonable dedup ratio vs. index size balance for general workloads.
- Measure dedup ratio vs. index size vs. read fragmentation across at least three workload types (source code trees, media files, binary/VM images) before finalizing defaults.
- Document that changing block size requires full data migration — this must be in the user-visible design from day one.

**Warning signs:**
- Block size is a compile-time constant (`const BLOCK_SIZE: usize = 65536`).
- No benchmark suite testing dedup ratio across representative workload types.
- README promises "configurable chunk size" but it requires recompilation.

**Phase to address:** Core chunking integration phase (when integrating the owner's CDC technology).

---

### Pitfall 7: The "Dedup Everything" Trap

**What goes wrong:**
Applying deduplication to all writes unconditionally degrades performance for workloads where deduplication is ineffective (encrypted data, compressed media, random write patterns). The hash computation, index lookup, and metadata update costs are paid regardless of whether any deduplication occurs. For encrypted-at-rest content, deduplication is completely ineffective because identical plaintext produces different ciphertext.

**Why it happens:**
Deduplication is built into the write path as a mandatory step. The assumption is that "dedup can only help." In reality, for workloads with no duplicates, dedup adds 5–15% write latency overhead with zero benefit. ZFS's own maintainer documented this: his laptop's DDT had 11.7 million entries with trivial actual savings.

**How to avoid:**
- Make deduplication a per-file or per-directory policy, not a filesystem-global mandate.
- Implement a "dedup skip" heuristic: if a block's hash lookup misses the index consistently for a given inode, disable dedup for that inode's subsequent writes for a configurable window.
- If encryption is a future feature, document clearly that encryption and deduplication are mutually exclusive at the block level.
- Add a `nodup` mount option or xattr that bypasses the dedup pipeline for specific files/directories.

**Warning signs:**
- Deduplication cannot be disabled per file or per directory.
- The write path has no fast-path for non-deduplication mode.
- No measurement of the "dedup overhead for non-duplicate data" in the benchmark suite.

**Phase to address:** FUSE write path phase; ensure the dedup pipeline has a bypass from the start.

---

### Pitfall 8: FUSE-T (macOS) NFS Semantic Gaps

**What goes wrong:**
FUSE-T on macOS implements FUSE semantics via an NFSv4 translation layer. This introduces several POSIX gaps that are not present on Linux:
- `mmap` writes are not flushed to the daemon until `munmap` or file close — applications that rely on `msync` + read visibility fail silently.
- `flock`/`lockf`/`fcntl` byte-range locks bypass FUSE calls entirely and go through the NFS client, breaking any lock-based synchronization in the filesystem daemon.
- `atime` and `mtime` cannot be set independently; NFS always updates both.
- READDIR must return all results in one pass (no pagination); large directories cause memory spikes.
- Attribute caching performed by the NFS client ignores the TTL values returned by the filesystem implementation.
- NFS server has no authentication, potentially exposing it to DoS via a process that refuses to respond to NFS RPCs.

**Why it happens:**
FUSE-T is a pragmatic workaround for Apple's refusal to expose a kernel filesystem API. The NFS translation is clever but lossy — POSIX semantics that FUSE assumes are not all preserved through NFS.

**How to avoid:**
- Build an explicit POSIX compatibility test suite that runs on macOS and asserts behavior for: `mmap` write visibility, `flock` semantics, `atime`/`mtime` independence, and directory listing completeness.
- Document the known gaps clearly. Do not silently fail POSIX tests on macOS — document them as known platform limitations.
- Implement large directory listing with explicit memory limits to prevent OOM on READDIR.
- Track the FSKit API (macOS 15+) as a potential future replacement for FUSE-T; design the FUSE interface layer so that switching backends requires only implementing a new adapter.
- Do not rely on `flock` for internal daemon synchronization; use explicit Rust synchronization primitives instead.

**Warning signs:**
- No macOS-specific POSIX test suite.
- Code that calls `flock` and expects filesystem-mediated lock behavior on macOS.
- `mmap` write tests that pass on Linux but are never run on macOS.

**Phase to address:** macOS FUSE-T integration phase; macOS POSIX test suite is a phase exit criterion.

---

### Pitfall 9: WinFSP POSIX Semantic Gaps

**What goes wrong:**
WinFSP translates between Windows ACL permissions and POSIX permission bits. Critical gaps:
- POSIX `unlink` on an open file (delete-while-open) does not work the same way on Windows; the file cannot be deleted until all handles are closed.
- Close-open consistency (guaranteed on Linux FUSE: data written before `close()` is visible after `open()`) is not guaranteed on WinFSP due to `IRP_CLEANUP` differences.
- `rename` is not atomic on Windows when the destination exists; two-step move with delete is required.
- Hard links via `link(2)` have restrictions on Windows (cannot cross volumes, limited in NTFS).
- Alternate data streams (Windows) have no POSIX equivalent and vice versa.

**Why it happens:**
Windows filesystem semantics diverge from POSIX at a fundamental API level. WinFSP makes best-effort POSIX compatibility but cannot bridge all gaps without OS-level changes.

**How to avoid:**
- Establish a Windows POSIX compatibility test matrix early, identifying which POSIX operations are supported, partially supported, or unsupported.
- For `unlink`-while-open: implement a "deferred delete" mechanism that marks the file as deleted but keeps it accessible until refcount drops to zero.
- For close-open consistency: explicitly flush and sync on `flush` operations, not just on `fsync`.
- Document Windows as a "best-effort POSIX" platform from day one. Do not claim full POSIX compliance on Windows.

**Warning signs:**
- Windows tests not in CI from day one.
- `unlink` implementation on Windows does not handle the open-file case.
- Any code that assumes `rename` is atomic without explicit atomicity guarantees.

**Phase to address:** Windows WinFSP integration phase; separate from the Linux and macOS phases.

---

### Pitfall 10: Read Fragmentation Accumulation Over Time

**What goes wrong:**
As deduplication operates, logically sequential files become physically fragmented across the block store. A file written sequentially may reference blocks scattered across hundreds of non-contiguous storage locations. Read performance degrades progressively as the filesystem ages: what started as sequential reads become hundreds of random reads. In backup system research, deduplication-induced fragmentation has been shown to increase restore time by 42% on average, sometimes 2× or more.

**Why it happens:**
Deduplication breaks the locality of reference that sequential storage provides. A block written at time T may be physically adjacent to blocks written at time T-500 (because those blocks are duplicates of earlier content). The CAS store has no concept of "store this near that" unless explicitly implemented.

**How to avoid:**
- Implement a "container" or "segment" storage model: group blocks that are likely to be read together into physical storage segments. When deduplicating, prefer placing new references near the container where the majority of the file's blocks already reside.
- Track per-file read access patterns and implement a background repack/defragmentation operation that co-locates frequently co-accessed blocks.
- Design the block storage layer with a "hint" interface: the metadata layer can hint at preferred physical locality when storing new blocks.
- Monitor and report fragmentation ratio as a first-class metric (number of storage seeks per file read).

**Warning signs:**
- Read throughput for a filesystem that has been in use for several months is significantly lower than freshly populated filesystem.
- No "fragmentation ratio" or "block locality" metric in the monitoring interface.
- Block store places blocks in purely content-address order with no locality optimization.

**Phase to address:** Storage layer design; locality hints should be in the initial block store interface even if not implemented until a later phase.

---

### Pitfall 11: Hash Collision Handling Absent or Incorrect

**What goes wrong:**
If two distinct blocks produce the same hash (collision), the second block's content is silently discarded and reads return the first block's content. For SHA-256 this is probabilistically negligible in any realistic dataset (~1/2^128 per 4 billion blocks), but:
1. SHA-1 has known practical collisions; if pluggable hashes include SHA-1 for testing, it can be triggered.
2. Bugs in the hash implementation (wrong initialization, truncated output) can cause practical collisions.
3. If hash selection is pluggable and a weak hash (MD5, xxHash) is used, collision probability becomes non-trivial for large datasets.
4. The system must have a defined behavior for collision — not ignore it.

**Why it happens:**
Developers assume "the hash is correct and unique." No collision-detection path is implemented. When a collision occurs due to a bug or weak hash, it is silent data corruption with no error returned to the user.

**How to avoid:**
- Implement a "verify on dedup" mode: when a hash collision is detected (incoming block hash matches an existing block hash), compare the full block content before accepting dedup. If content differs, it is a collision — reject the write with an error or store both blocks under a collision-resolution scheme.
- Make this verification configurable: enabled by default, disable-able for performance-critical deployments with cryptographic hashes only.
- For pluggable hash support: enforce a minimum security level for production use (SHA-256 or BLAKE3 only). Label weaker hashes as "testing only."
- Add a `hash_verify` mode to the CLI that walks all blocks and verifies stored content against their hash.

**Warning signs:**
- No code path that compares block content when a hash match is found.
- Tests use a weak hash (e.g., CRC32) for speed without marking it as collision-unsafe.
- No documentation on minimum hash strength for production use.

**Phase to address:** Core CAS engine (first phase); collision handling must be part of the block store interface contract.

---

### Pitfall 12: Metadata Store Becoming a Bottleneck

**What goes wrong:**
The metadata store (inode table, block index, reference counts) is accessed on every FUSE operation. If it uses a serialized, single-writer database (SQLite in WAL mode, sled beta), it becomes the bottleneck under concurrent access. Worse: if the metadata store is not crash-consistent independently of the block store, partial transactions leave the filesystem in an unrecoverable state.

**Why it happens:**
Developers use a familiar embedded database (SQLite, sled) for the metadata store because it handles transactions and crash consistency "automatically." However, sled is not yet 1.0 (on-disk format changes require manual migration), SQLite's WAL mode serializes writes, and neither is designed for the access patterns of a filesystem (many small, high-frequency, concurrent reads and writes).

**How to avoid:**
- Use a B-tree embedded database with ACID transactions, crash consistency, and a stable on-disk format. Current best options for Rust:
  - `redb` (pure Rust, stable, MVCC, production-ready)
  - `rocksdb` (battle-tested, LSM-tree, excellent for write-heavy workloads, C FFI)
  - `fjall` (pure Rust LSM, newer but growing)
- Define the metadata store as a trait from day one so the implementation can be swapped.
- Separate the dedup index (hash → block location) from the filesystem metadata (inode → block list, refcounts) — they have different access patterns and can benefit from different storage engines.
- Test metadata store crash recovery explicitly: kill the process mid-write, verify consistency on restart.

**Warning signs:**
- Metadata store implementation is not behind a trait.
- Using `sled` without acknowledging it is pre-1.0 and format-unstable.
- Metadata operations are not measured separately in benchmarks.
- Single-threaded metadata access under concurrent FUSE requests.

**Phase to address:** Metadata layer design (foundational phase); trait boundary must be established before any implementation.

---

### Pitfall 13: Inline Dedup Blocking the Write Hot Path

**What goes wrong:**
In-band (inline) deduplication means every write must: chunk the data, compute a hash per chunk, look up the hash in the index, update metadata if new, update refcounts if duplicate — all synchronously on the write path. This directly adds latency to every write, regardless of whether deduplication succeeds. Under write-heavy workloads with low duplicate ratios, this overhead is pure cost.

**Why it happens:**
Inline dedup is architecturally simpler (single write path, no staging area), so it is implemented first. The dedup computation is not separated from the I/O path. Blocking hash computation on the FUSE thread pool starves the Tokio async runtime if not handled via `spawn_blocking`.

**How to avoid:**
- Hash computation and index lookup must run in a `tokio::task::spawn_blocking` context, not in the FUSE callback thread, to avoid starving the async executor.
- Implement an async dedup pipeline: write data to a staging buffer immediately (fast path), then deduplicate asynchronously. The file appears written; deduplication happens in the background.
- The post-process dedup model: write blocks directly, trigger dedup as a background task. Trades dedup latency for write latency — acceptable for a daily-driver filesystem.
- Never hold a `std::sync::Mutex` across an `.await` point in the dedup pipeline; this is a tokio deadlock waiting to happen.

**Warning signs:**
- Hash computation is done synchronously in the FUSE `write()` callback.
- Blocking index lookup in `async fn` without `spawn_blocking`.
- Write latency is proportional to block count per operation rather than data size.

**Phase to address:** Write path architecture (first implementation phase); the sync vs. async dedup decision must be made before implementing the write path.

---

## Technical Debt Patterns

| Shortcut | Immediate Benefit | Long-term Cost | When Acceptable |
|----------|-------------------|----------------|-----------------|
| Global `Mutex<HashMap>` for dedup index | Simple to implement, correct | Single-threaded throughput ceiling; blocks all writes during lookup | Never for production; MVP only if behind a trait |
| Fixed block size (not CDC) | Simpler chunking logic | Lower dedup ratio; cannot change without data migration | Acceptable in Phase 1 if block size is a runtime parameter and CDC integration is planned |
| Synchronous inline dedup | Single write path, easy to reason about | Write latency bloat under non-duplicate workloads | Acceptable in Phase 1 with clear plan to make async |
| SQLite for metadata | Familiar, ACID, immediate | WAL serialization under concurrent access; not optimized for filesystem access patterns | Never; choose redb or rocksdb from the start |
| No Bloom filter for index | Simpler code | Full index lookup for every block, even unique ones; memory pressure | Never; Bloom filter is a 50-line addition that prevents the ZFS DDT problem |
| sled as metadata store | Pure Rust, "champagne of beta databases" | Pre-1.0, on-disk format changes require migration, garbage collection overhead | Never in production; use redb instead |
| Skip crash-consistency tests | Faster initial development | Undetectable data corruption on crash; unfixable without rearchitecture | Never; crash tests must be in CI from day one |

---

## Integration Gotchas

| Integration | Common Mistake | Correct Approach |
|-------------|----------------|------------------|
| fuser crate (Rust FUSE) | Using the synchronous `Filesystem` trait blocking on I/O | Offload all blocking operations to `spawn_blocking`; keep FUSE callbacks non-blocking |
| fuser `writeback_cache` | Disabled by default; developers accept the 4× write overhead | Enable it explicitly via `MountOption`; document the crash-consistency tradeoff |
| FUSE-T (macOS) | Assuming attribute TTL values sent from daemon are respected | NFS client ignores daemon TTL; implement conservative cache invalidation |
| WinFSP | Implementing `unlink` the Linux way (mark deleted, keep data until last handle closes) | Handle `IRP_CLEANUP` explicitly; implement deferred-delete semantics for Windows |
| redb / rocksdb | Opening metadata store in the FUSE callback thread | Open at mount time, keep handle alive for filesystem lifetime; never reopen per-request |
| tokio + FUSE | Running the fuser session loop inside `tokio::main` | Run fuser on a dedicated OS thread; bridge to tokio via channels |

---

## Performance Traps

| Trap | Symptoms | Prevention | When It Breaks |
|------|----------|------------|----------------|
| Dedup index not Bloom-filtered | Index lookup latency grows linearly with dataset; OOM on large datasets | Add Bloom filter as pre-lookup gate; configurable memory budget | At ~1M unique blocks |
| Per-block `fsync` after write | Write throughput collapses to storage device IOPS ceiling | Batch writes; sync at transaction boundaries, not per-block | Immediately on any real workload |
| Unbounded GC pause | GC pause time grows with dataset size; filesystem freezes | Incremental GC with time budgets; yield to write operations | At ~10GB stored data |
| No writeback cache | 4K sequential writes at 20% native speed | Enable `writeback_cache` in fuser mount options | Always — default FUSE behavior |
| Storing all refcounts in a single B-tree | Refcount update is a global write bottleneck | Shard refcount storage by hash prefix | At ~10K concurrent write operations |
| Large READDIR on FUSE-T macOS | OOM when listing directory with >100K files | Page READDIR results; enforce a maximum batch size | Any directory with >50K entries |

---

## Security Mistakes

| Mistake | Risk | Prevention |
|---------|------|------------|
| Using MD5 or SHA-1 as production dedup hash | Hash collision attack: attacker crafts blocks that hash-collide, triggering cross-user data exposure | Enforce SHA-256 or BLAKE3 minimum; reject weak hashes at configuration parse time |
| FUSE-T NFS server on non-loopback interface | Local network can DoS the filesystem or access data without authentication | Bind FUSE-T NFS server to loopback only; verify in FUSE-T integration code |
| Exposing block content via hash-based API without authorization | If two users share a filesystem, a user who knows a block's hash can infer whether another user's file contains that block (hash oracle attack) | Do not expose block hashes externally; all access through POSIX file interface only |
| No integrity verification on block read | Silent data corruption from storage-layer bit rot is undetectable | Verify block hash on every read in "verify" mode; implement a background scrub command |

---

## "Looks Done But Isn't" Checklist

- [ ] **Write path:** Appears to write correctly in single-threaded tests — verify concurrent writes with two threads writing the same block simultaneously without producing refcount > 2 or corruption.
- [ ] **Delete path:** Files delete successfully in tests — verify that GC actually reclaims storage after delete and that refcounts reach zero correctly.
- [ ] **Crash consistency:** Filesystem mounts and reads after clean unmount — verify it mounts and reads correctly after `kill -9` during a write, and after `kill -9` during GC.
- [ ] **Dedup ratio:** Reports dedup savings — verify the savings are real by reading back deduplicated files and comparing content with original.
- [ ] **macOS compatibility:** FUSE-T mounts successfully — verify `mmap` write visibility, `flock` behavior, and large directory listing do not silently fail.
- [ ] **Windows compatibility:** WinFSP mounts successfully — verify delete-while-open, `rename` atomicity, and close-open consistency.
- [ ] **Memory bounds:** Runs for 10 minutes in tests — verify memory usage is bounded after writing 100GB of unique data (index must not grow unboundedly).
- [ ] **Recovery:** Starts up after crash — verify the orphan block scanner runs and produces no false positives on a clean filesystem.

---

## Recovery Strategies

| Pitfall | Recovery Cost | Recovery Steps |
|---------|---------------|----------------|
| Reference count corruption (data loss) | HIGH | Requires offline fsck: walk all inodes, recompute correct refcounts, identify discrepancies, manually adjudicate blocks with refcount 0 but referenced by inodes |
| GC deleted live block | HIGH | No recovery without a separate backup; offline forensic walk of block store looking for blocks that match expected hashes from metadata |
| Crash consistency failure (orphaned blocks) | LOW | Run `slicefs fsck --orphan-gc` on next mount; orphaned blocks are safe to delete |
| Crash consistency failure (dangling reference) | HIGH | Requires offline repair: identify inodes with dangling references, mark those inodes as corrupted, attempt content recovery from any surviving blocks |
| Index OOM crash | LOW | Resize index memory budget in config; restart; index will be rebuilt from block store on next GC cycle if designed correctly |
| Fragmentation-induced read slowdown | MEDIUM | Run `slicefs defrag` command; expect it to take proportionally to dataset size |
| Block size mismatch (data format change) | HIGH | Full data migration required: mount old filesystem, copy all files to new filesystem with new block size |

---

## Pitfall-to-Phase Mapping

| Pitfall | Prevention Phase | Verification |
|---------|------------------|--------------|
| Reference count corruption | Core CAS + metadata (Phase 1) | Concurrent deletion + write stress test; refcount invariant checker |
| GC race — new write lost | GC implementation (Phase 2) | Chaos GC test: trigger GC between every write step; verify no data loss |
| Crash inconsistency | Write path + recovery (Phase 1) | `kill -9` during write; verify clean mount and correct data on restart |
| FUSE context-switch overhead | FUSE integration (Phase 2) | `fio` benchmark: 4K sequential writes must exceed 200MB/s on NVMe |
| Dedup index memory explosion | Core dedup engine (Phase 1) | Write 100GB unique data; verify index memory stays within configured budget |
| Block size locking in tradeoffs | CDC integration (owner's technology) | Dedup ratio benchmark across source code, media, VM disk image workloads |
| "Dedup everything" trap | Write path (Phase 2) | Per-file `nodup` xattr works; benchmark write latency with and without dedup enabled |
| FUSE-T macOS semantic gaps | macOS integration (Phase 3) | POSIX test suite: mmap, flock, atime/mtime, large directory — all passing or explicitly documented as limitations |
| WinFSP POSIX semantic gaps | Windows integration (Phase 4) | Windows POSIX test suite: delete-while-open, rename atomicity, close-open consistency |
| Read fragmentation accumulation | Storage layer design (Phase 1, optimization Phase 3) | Read throughput after 6-month simulated use (random deletes + writes) must not degrade more than 20% |
| Hash collision absent handling | Core CAS engine (Phase 1) | Inject artificial collision (mock hash function); verify collision is detected and rejected |
| Metadata store bottleneck | Metadata layer (Phase 1) | Concurrent read + write benchmark: metadata operations must not serialize |
| Inline dedup blocking write path | Write path architecture (Phase 1) | Write callback latency must not block FUSE thread pool; verify via tokio thread starvation test |

---

## Sources

- USENIX FAST 2017: "To FUSE or Not to FUSE: Performance of User-Space File Systems" — https://www.usenix.org/system/files/conference/fast17/fast17-vangoor.pdf
- USENIX FAST 2017: "The Logic of Physical Garbage Collection in Deduplicating Storage" — https://www.usenix.org/system/files/conference/fast17/fast17-douglis.pdf
- USENIX FAST 2013: "Concurrent Deletion in a Distributed Content-Addressable Storage System" — https://www.usenix.org/system/files/conference/fast13/fast13-final91.pdf
- "OpenZFS deduplication is good now and you shouldn't use it" (Rob Norris, 2024) — https://despairlabs.com/blog/posts/2024-10-27-openzfs-dedup-is-good-dont-use-it/
- Medium: "Linux Fuse File System Performance Learning" — https://medium.com/@xiaolongjiang/linux-fuse-file-system-performance-learning-efb23a1fb83f
- RFUSE: "Modernizing Userspace Filesystem Framework" (USENIX FAST 2024) — https://www.usenix.org/system/files/fast24-cho.pdf
- FUSE-T GitHub: Known Issues — https://github.com/macos-fuse-t/fuse-t
- FUSE-T HN discussion (data corruption reports) — https://news.ycombinator.com/item?id=40217493
- WinFSP: "Native API vs FUSE" — https://winfsp.dev/doc/Native-API-vs-FUSE/
- JuiceFS POSIX Compatibility — https://juicefs.com/docs/community/posix_compatibility/
- Borg Backup: hash collision discussion — https://github.com/borgbackup/borg/issues/170
- TrueNAS ZFS Deduplication reference — https://www.truenas.com/docs/references/zfsdeduplication/
- "Files are hard" (Dan Luu, crash consistency) — https://danluu.com/file-consistency/
- CrashMonkey: "Systematically Testing File-System Crash Consistency" (Microsoft Research) — https://www.microsoft.com/en-us/research/wp-content/uploads/2021/10/tos-crashmonkey.pdf
- fuser crate GitHub + CHANGELOG — https://github.com/cberner/fuser
- Tokio: "Async: What is blocking?" (Alice Ryhl) — https://ryhl.io/blog/async-what-is-blocking/
- USENIX FAST 2015: "Design Tradeoffs for Data Deduplication Performance" — https://www.usenix.org/system/files/conference/fast15/fast15-paper-fu.pdf

---
*Pitfalls research for: Deduplicating FUSE Filesystem (DedupFS)*
*Researched: 2026-03-27*
