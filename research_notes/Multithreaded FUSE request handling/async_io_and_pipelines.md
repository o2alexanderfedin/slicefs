# Async IO and work-offload options for a Rust FUSE server; write pipelines of dedup/backup engines

Research date: 2026-11-19. 18 search/fetch calls. Many items come from search-result summaries rather than full primary pages; these are marked "(secondary/snippet)".

## 1. io_uring in Rust (tokio-uring, glommio, monoio, compio, io-uring crate), thread-per-core vs work-stealing, fsync/linked SQEs, registered buffers, macOS fallback

### Takeaway
In 2026 the only io_uring runtime with a strong, recent, production-grade endorsement is **compio**. Apache Iggy picked it in February 2026 over monoio and glommio. glommio has had no release since March 2024, and tokio-uring looks dormant. Tokio itself now has io_uring file ops, but only behind `tokio_unstable`. On macOS there is no io_uring, so the portable path is blocking file syscalls on a thread pool, which is how tokio::fs already works.

### Cited Findings
- **Iggy's runtime choice (Feb 2026).** The Apache Iggy team built its proof of concept on **monoio**. They then rejected it because "its feature coverage [was] lagging behind io_uring's fast-moving API" and its maintenance was slowing. They found **glommio** "unmaintained and opinionated"; it is thread-per-core with 3 io_uring rings per thread and a proportional-share scheduler. They chose **compio** for its broad io_uring coverage, active maintenance ("patches merged within hours") and a driver decoupled from the executor (it also supports IOCP on Windows). tokio-uring was not evaluated. — [Apache Iggy blog](https://iggy.apache.org/blogs/2026/02/27/thread-per-core-io_uring/)
- **Iggy's numbers.** These compare Iggy v0.5.0 (tokio) with v0.7.0 (compio, thread-per-core), so the gains mix the runtime change with other release changes. The comparison is not isolated. — [Iggy blog](https://iggy.apache.org/blogs/2026/02/27/thread-per-core-io_uring/)
  - fsync-per-batch mode, 16 partitions: 843 → 992 MB/s, P99 19.72 → 13.04 ms.
  - fsync-per-batch mode, 32 partitions: 931 → 1,102 MB/s, P99 37.09 → 23.74 ms.
  - Without fsync, 8 producers: P9999 34.00 → 6.51 ms at the same 1,000 MB/s.
  - Without fsync, 32 producers: P99 4.52 → 1.82 ms.
  - The gap over tokio is small at low load and grows with load.
- **Iggy's lessons:**
  - Getting io_uring's benefit "requires heavy syscall batching". Swapping in an io_uring runtime alone is not enough.
  - io_uring completions are unordered, so ordered writes need `IOSQE_IO_LINK`.
  - compio boxes each submission (a heap allocation per request), which the team accepted with mimalloc.
  - Holding a `RefCell` borrow across `.await` caused "already borrowed" panics; clippy's `await_holding_refcell_ref` lint catches this.
  - Pure shared-nothing was abandoned for a "shared something" model: `left-right` with a single writer on shard0.
  - Hot shards need manual handling because there is no work-stealing. — [Iggy blog](https://iggy.apache.org/blogs/2026/02/27/thread-per-core-io_uring/)
- **Work-stealing vs thread-per-core.** Work-stealing (tokio) balances load automatically, but tasks migrate between threads, which invalidates caches and makes execution less predictable. Thread-per-core means "work steering": better cache locality and less lock contention, but load balancing and cross-shard state become the application's job. — [Iggy blog](https://iggy.apache.org/blogs/2026/02/27/thread-per-core-io_uring/)
- **glommio** latest release: 0.9.0, 2024-03-25. — [lib.rs glommio](https://lib.rs/crates/glommio) (secondary/snippet)
- **tokio-uring.** A Rust forum reply says "there hasn't been a release of tokio-uring in three years". The thread date was not visible, so this is undated. — [users.rust-lang.org](https://users.rust-lang.org/t/tokio-uring-experiences/134314) (secondary/snippet)
- **Tokio's own io_uring support.** "Tokio will use ordinary blocking file operations behind the scenes … using the spawn_blocking threadpool". It "uses io_uring for file operations on Linux when compiled with tokio_unstable"; io_uring may also make operations cancellable that spawn_blocking cannot cancel. — [tokio::fs docs](https://docs.rs/tokio/latest/tokio/fs/index.html)
- **Write/fsync ordering.**
  - A write followed by an fsync in the same submission queue is NOT ordered: "an application which places a write I/O followed by an fsync in the submission queue cannot expect the fsync to apply to the write". — [io_uring_enter(2)](https://man7.org/linux/man-pages/man2/io_uring_enter.2.html)
  - The fix is to set `IOSQE_IO_LINK` on the write so the fsync waits for it. If a request in a soft-linked chain fails, the rest of the chain is cancelled. `IOSQE_IO_HARDLINK` makes the next request run anyway. — [io_uring_linked_requests(7)](https://www.man7.org/linux/man-pages/man7/io_uring_linked_requests.7.html)
  - A kernel list thread names the 2 options: wait for the completion, or link the requests. `IOSQE_IO_DRAIN` is "rather inefficient". — [lore.kernel.org io-uring](https://lore.kernel.org/io-uring/op.0e3nafvf0ncgu9@localhost/T/)
- **Registered buffers.** Iggy mentions them only as something POSIX-style `File` abstractions cannot expose. They give no measurement. — [Iggy blog](https://iggy.apache.org/blogs/2026/02/27/thread-per-core-io_uring/)

### Inferences
- If SliceFS adds io_uring, it should use a decoupled driver (compio-style) or tokio with `tokio_unstable`, and only on Linux. glommio and tokio-uring look like poor long-term bets in 2026. monoio is usable, but per Iggy its io_uring coverage lags.
- io_uring cannot be the only IO path, because FSKit/macOS has no io_uring and the user wants FSKit only on macOS. A thread pool running blocking `pwrite`/`fsync` is the common denominator. kqueue does not give async regular-file IO.
- The data that clearly favors io_uring is under high concurrency (Iggy, 16 to 32 partitions). It does not show that io_uring cuts single-stream fsync latency.

### Gaps
- No primary 2026 status found for monoio, tokio-uring or the raw `io-uring` crate (release dates, GitHub activity).
- No measurement found of registered buffers or fixed files in Rust.
- No source on kqueue as a file-IO fallback in Rust runtimes.
- The tokio io_uring feature's stability timeline was not found.

## 2. Does async IO help a FUSE server dominated by fsync and CPU work? Offload vs group commit

### Takeaway
No source measures a FUSE server directly. The evidence points one way: when fsync dominates, **batching fsyncs (group commit)** gives 5 to 100x more than making IO async. CPU work such as chunking and hashing gains nothing from async IO; it needs worker threads.

### Cited Findings
- **muopdb under tokio** (all numbers from a partly garbled table, author's M1 MacBook Pro, Criterion, 1000 concurrent WAL writes):
  - The 3 designs and their times — [muopdb blog](https://hunglongtrangithub.github.io/blogs/posts/muopdb-wal-write-group):

    | Design | Time |
    |---|---|
    | Per-write `fdatasync` | ~4.3 s |
    | Background sync thread on a 100 ms timer | ~335 ms |
    | Leader/follower write groups (group 940, 10 ms timeout) | ~37 ms |

  - On Linux a colleague measured 166 ms vs 32 ms, about 5x.
  - The author's estimate of about 6 writes/ms for sync-on-write does not match the 3 to 20 ms per sync quoted in the same post; treat it as suspect.
  - Design: the coordinator sits behind an async mutex. Followers push their entry plus a oneshot sender, **drop the lock**, then await. The leader writes every entry and calls one `sync_data()`. If the group does not fill, a timeout promotes the first follower to leader.
  - Pitfalls the author names: holding the mutex while awaiting stalls every writer; `sync_data()` inside a task still blocks a tokio worker; a 100 ms timeout added about 100 ms to the last partial group. — [muopdb](https://hunglongtrangithub.github.io/blogs/posts/muopdb-wal-write-group)
- **Rejected alternatives in muopdb:**
  - `spawn_blocking` per sync makes too many blocking threads, and slow (NFS) syncs can starve the pool.
  - `async-fs` still uses a thread pool.
  - tokio-uring needs a separate runtime and a new kernel. — [muopdb](https://hunglongtrangithub.github.io/blogs/posts/muopdb-wal-write-group)
- **PostgreSQL:**
  - `commit_delay` is "how many microseconds a group commit leader process will sleep after acquiring a lock within XLogFlush, while group commit followers queue up behind the leader". The default is 0.
  - Group commit still happens at delay 0: each group holds the sessions that reach their flush point while the previous flush runs. At high client counts this "gangway effect" makes tuning matter less.
  - Suggested setting: half the average single 8 kB flush time as measured by `pg_test_fsync`.
  - Too high a delay cuts throughput. — [PostgreSQL WAL configuration](https://www.postgresql.org/docs/current/wal-configuration.html)
- **RocksDB:**
  - A leader/follower write group: the leader writes the WAL records of all queued writers at once.
  - With `allow_concurrent_memtable_write`, followers insert into the memtable in parallel.
  - With `enable_pipelined_write`, WAL and memtable stages form a 2-stage pipeline. — [TiKV write-batch post](https://medium.com/@siddontang/how-we-optimize-rocksdb-in-tikv-write-batch-optimization-28751a4bdd8b); [besthub explainer](https://www.besthub.dev/articles/understanding-rocksdb-write-process-and-group-commit-mechanism-d364a27c9109) (secondary)
- **Tokio file IO** runs on the spawn_blocking pool. The docs advise to "batch your operations into as few spawn_blocking calls as possible" and to use `std::fs` inside `spawn_blocking` by hand. — [tokio::fs](https://docs.rs/tokio/latest/tokio/fs/index.html)
- **macOS fsync cost.**
  - `fsync()` only pushes data to the drive; `F_FULLFSYNC` also flushes the drive cache, which Linux `fsync` does. — [mjtsai](https://mjtsai.com/blog/2022/02/17/apple-ssd-benchmarks-and-f_fullsync)
  - Apple SSDs honor `F_FULLFSYNC` and pay a clear flush cost; this is 2022 data. — [mjtsai](https://mjtsai.com/blog/2022/02/17/apple-ssd-benchmarks-and-f_fullsync)
  - `F_FULLFSYNC` is "usually much slower". — [eclecticlight](https://eclecticlight.co/2022/02/18/how-can-you-trust-a-disk-to-write-data/)
  - SQLite developers found it slowed every drive they tested. — [SQLite forum](https://sqlite.org/forum/info/b94afa45dda82aae8cbf49f9d511a00b332870fc926cba18954acd889bbfb7cd)
  - An anecdote of about 4 ms per synced write on an M3 Max, blamed on Rust std calling `F_FULLFSYNC`, is unverified. — [coderapp](https://coderapp.vercel.app/answer/78628333)

### Inferences
- **For SliceFS, the main win is group commit of the write log and pack fsyncs.** One leader thread does one `fdatasync`/`F_FULLFSYNC` for every write that arrived during the previous flush; this is PostgreSQL's zero-delay "gangway" form, which needs no timer. On macOS, where `F_FULLFSYNC` costs milliseconds, this matters even more.
- **Async IO does not speed up the fsync itself.** It only frees the request thread. The same is true of a dedicated sync thread with a condvar or channel, which is portable and simpler.
- **FastCDC and BLAKE3 are CPU-bound.** They belong on a rayon or std-thread pool, never on async executor threads.
- A **multithreaded FUSE loop** (several threads reading `/dev/fuse`) combined with a **group-commit sync thread** probably captures most of the gain without an async runtime. This is not measured for FUSE.

### Gaps
- No measured FUSE-specific comparison of async vs threaded fsync handling was found.
- No SQLite WAL group-commit source was fetched; SQLite has no built-in multi-writer group commit, but that is not verified here.
- No raw `F_FULLFSYNC` latency figures on Apple silicon were found.

## 3. Write pipelines in dedup/backup engines (restic, borg, kopia, casync/desync, ZFS dedup, bupstash, rustic)

### Takeaway
The common pattern is staged worker pools joined by **bounded channels**: read/chunk → hash plus index lookup (dedup) → compress/encrypt → pack append → upload/fsync. Pack writing is usually serialized per open pack. Borg is the outlier: it has stayed single-threaded and is only now gaining a threaded pipeline.

### Cited Findings
- **restic** (`internal/archiver`):
  - The FileSaver constructor takes separate file-worker and blob-worker counts. File-saver workers reuse a chunker instance (Rabin). — [pkg.go.dev restic archiver](https://beta.pkg.go.dev/github.com/restic/restic@v0.12.1/internal/archiver); [DeepWiki restic dedup](https://deepwiki.com/restic/restic/5.3-data-deduplication) (secondary)
  - The BlobSaver worker pool checks the index and known blobs before saving. A newer `SaveBlobAsync` takes a callback that reports whether the blob was already known, plus its size. — [restic blob_saver mirror](https://git.0l.de/Mirrors/restic/raw/commit/15cc4d74b2bc9ea21beb0540c1ffe8c1e94c1bb4/internal/archiver/blob_saver.go) (secondary/snippet; versions vary)
- **kopia:**
  - Pipeline: split (default dynamic BUZHASH, e.g. `DYNAMIC-4M-BUZHASH`) → hash, check for an existing match → compress → encrypt → pack into blobs of 20 MB or more (`Max pack length: 21 MB`). — [kopia discourse chunk size](https://kopia.discourse.group/t/kopia-chunk-size-configuration/759); [kopia discourse spanning](https://kopia.discourse.group/t/do-hashed-chunks-span-multiple-files/1444) (secondary/snippet)
  - File-read parallelism defaults to the number of logical cores; snapshot-level parallelism defaults to 1. — [kopia discourse parallelism](https://kopia.discourse.group/t/parallelism-default-number-of-threads/2890) (snippet)
  - An older maintainer comment says uploads of a single large file were not parallelized. — [kopia discourse](https://kopia.discourse.group/t/low-read-performance-per-distinct-hashing-process/90) (older)
  - Kopia keeps the raw data when compression would make a chunk larger. — [kopia compression docs](https://kopia.io/docs/advanced/compression)
- **borg:**
  - Borg 1.x is single-threaded; maintainers deferred multithreading past borg2. — [borg discussion #6958](https://www.github.com/borgbackup/borg/discussions/6958)
  - A recent PR adds groundwork for a "threaded create pipeline (#37)": no cipher or chunker object is shared across threads, and single-thread throughput is unchanged. — [borg PR #9959](https://github.com/borgbackup/borg/pull/9959) (snippet)
  - Another PR lets large chunks use BLAKE3's internal thread pool, while small chunks stay single-threaded. — [borg PR #10014](https://github.com/borgbackup/borg/pull/10014) (snippet; PR-to-claim mapping is from the search summary)
- **desync (casync in Go):** it chunks in parallel by splitting a file into N equal parts and chunking each part independently. Its optimizations need the minimum chunk size to be larger than the 48-byte rolling-hash window. A user reported single-threaded borg compressing faster than parallel desync. — [desync fork](https://github.com/kmeaw/desync); [desync issue #243](https://github.com/folbricht/desync/issues/243) (snippet)
- **rustic_core:** depends on crossbeam-channel, pariter and rayon; the packer sends blobs over **bounded crossbeam channels**. — [rustic_core packer.rs](https://docs.rs/crate/rustic_core/0.12.0/source/src/blob/packer.rs); [rustic_core Cargo.toml](https://docs.rs/crate/rustic_core/0.12.0/source/Cargo.toml.orig) (snippet)
- **backpak (Rust):** chunks go to a file packer and trees to a tree packer; both feed an indexer, then an uploader, each a named scoped thread. — [backpak backup.rs](https://docs.rs/crate/backpak/0.3.0/source/src/backup.rs) (snippet)
- **Other Rust tools:** ffsync and hexz use bounded crossbeam channels between chunkers and hash workers to cap chunks in flight, with no async runtime in the data path. — [ffsync](https://docs.rs/crate/ffsync/0.8.0); [hexz parallel_pack.rs](https://docs.rs/hexz-ops/0.8.2/src/hexz_ops/parallel_pack.rs.html) (snippet)
- **rdedup:** an older post on its multi-stage thread pipeline. — [dpc.pw: Rust's fearless concurrency in rdedup](https://dpc.pw/rusts-fearless-concurrency-in-rdedup) (older; not fetched)

### Inferences
- **The Rust dedup tools surveyed** (rustic, backpak, ffsync, hexz) all use **OS threads plus bounded crossbeam channels plus rayon**, not async, for chunk, hash and pack. This matches SliceFS's CPU-heavy profile.
- **Parallelism within one file** has two forms. desync's split-into-N-parts approach changes chunk boundaries at the split points, which costs some dedup. For SliceFS (one FUSE write stream per file), a better fit is:
  - chunk sequentially, which is cheap with FastCDC;
  - hash and compress chunks in parallel;
  - append to the pack in order through a single pack-writer thread.
- **Borg's PR #10014 suggests a cheap option:** BLAKE3's own multithreading only helps for large inputs, so it gives little at 4 to 32 KiB chunks.

### Gaps
- No primary-source worker counts, queue depths or throughput numbers for restic or kopia.
- ZFS dedup (including OpenZFS 2.3 "fast dedup"), bupstash and casync (C) were not researched within the call budget.

## 4. Rust async FUSE crates (fuse3, polyfuse, fuse-backend-rs, fuser async/multithread) and their production users

### Takeaway
**fuse-backend-rs** (cloud-hypervisor org, v0.14.0) is the production-proven choice for Linux and virtio-fs; Nydus is its named user. Its async-io feature pulls in tokio-uring, which is Linux-only. **fuse3** is a general tokio or async-io FUSE crate. For fuser, async support exists as an experimental `AsyncFilesystem` upstream and as the fork **async-fuser 0.17**, whose multithreaded async sessions are Linux-only. None of these helps on macOS FSKit.

### Cited Findings
- **fuse-backend-rs:**
  - A "rust library for Fuse (filesystem in userspace) servers and virtio-fs devices", latest 0.14.0.
  - Features: `fusedev` (default), `virtiofs`, `vhost-user-fs`, and an optional `async-io` that pulls in async-trait, io-uring and tokio-uring.
  - The README names Nydus (Dragonfly Image Service) as its example user, with fusedev and vhost-user-fs servers. — [docs.rs fuse-backend-rs](https://docs.rs/crate/fuse-backend-rs/latest); [lib.rs features](https://lib.rs/crates/fuse-backend-rs/features)
- **virtiofsd:** the README names no Rust crates. Its `--thread-pool-size` ("Maximum thread pool size. A value of '0' disables the pool") **defaults to 0**. The claim that virtiofsd uses fuse-backend-rs is **unconfirmed** by these sources. — [virtiofsd README](https://gitlab.com/virtio-fs/virtiofsd/-/raw/main/README.md)
- **fuse3:** an "improved rewrite of the FUSE user-space library", running on a tokio or async-io runtime, with unprivileged mount via fusermount3. **rfuse3** is a separate async fork from the rk8s team. — [docs.rs fuse3](https://docs.rs/fuse3); [rfuse3](https://docs.rs/crate/rfuse3/0.0.4/source/Cargo.toml) (snippet)
- **fuser upstream:** release notes mention an experimental async API (`AsyncFilesystem`) and `BackgroundSession::join` renamed to `umount_and_join`, which returns `io::Result`. The version could not be confirmed. — [fuser releases mirror](https://gh.ali360.cloudns.ch/cberner/fuser/releases) (snippet)
- **async-fuser 0.17.0** (a fork, not upstream):
  - Adds a tokio async interface and claims to be a drop-in replacement for fuser ≥ 0.17.0.
  - Multithreaded async sessions are Linux-only, with one event-loop thread per channel. — [async-fuser docs](https://docs.rs/crate/async-fuser/0.17.0); [session_async.rs](https://docs.rs/async-fuser/0.17.0/src/async_fuser/session_async.rs.html)
- **fuser-ng 0.7:** moved to fuser 0.17 and switched to "fuser's native threading instead of an internal thread pool". — [fuser-ng docs](https://docs.rs/fuser-ng/0.8.2/fuser_ng/) (snippet)
- **ZeroFS:**
  - Rust; serves **NFS, 9P and NBD** (not FUSE) from one process over a shared filesystem layer, with metadata in an LSM tree on S3.
  - The README recommends 9P over NFS for strong durability, because fsync on 9P gives a real flush signal.
  - No source found on the server's async runtime. — [ZeroFS README](https://raw.githubusercontent.com/Barre/ZeroFS/main/README.md)

### Inferences
- SliceFS vendors a patched fuser and uses FSKit only on macOS. The lowest-risk path is to keep fuser and use its native multithreading on Linux, then dispatch heavy work to its own pools. A move to fuse3 or fuse-backend-rs would be Linux-only and would not help the FSKit backend.
- The multithreaded mode of async-fuser and fuser is Linux-only, and virtiofsd's pool defaults to off. Together these hint that more FUSE threads are not automatically a win.

### Gaps
- polyfuse: not researched (status and users unknown).
- No confirmed fuser upstream version or changelog for multithreaded sessions or `AsyncFilesystem`.
- No production lessons write-up from Nydus or virtiofsd on threading was found.

## 5. Bounded memory under async: bounded channels, semaphores as byte budgets, pitfalls

### Takeaway
Bound memory with **bounded channels between stages** (the pattern in every Rust dedup tool above) plus a **byte-budget semaphore**. With tokio, use `acquire_many(n_bytes)`. Tokio's semaphore is FIFO-fair, so a large request at the head of the queue blocks smaller ones behind it. Never hold locks or RefCell borrows across `.await`, and never fsync on an executor thread.

### Cited Findings
- **tokio Semaphore:**
  - It is fair: "permits are given out in the order they were requested".
  - A large `acquire_many` at the head of the queue blocks a later `acquire`, even when enough permits exist for it.
  - `MAX_PERMITS` = `usize::MAX >> 3`; exceeding it typically panics.
  - The docs show no memory-budget example; the closest caps open files. — [tokio Semaphore docs](https://docs.rs/tokio/latest/tokio/sync/struct.Semaphore.html)
- **Bounded crossbeam channels** give backpressure between chunkers and hashers and cap chunks in flight (ffsync, hexz, rustic_core packer). — [ffsync](https://docs.rs/crate/ffsync/0.8.0); [hexz](https://docs.rs/hexz-ops/0.8.2/src/hexz_ops/parallel_pack.rs.html); [rustic_core packer](https://docs.rs/crate/rustic_core/0.12.0/source/src/blob/packer.rs) (snippet)
- **Pitfalls:**
  - Holding the coordinator's async mutex while awaiting the group result stalls all writers; the fix is to drop the lock before awaiting a oneshot. — [muopdb](https://hunglongtrangithub.github.io/blogs/posts/muopdb-wal-write-group)
  - A `RefCell` borrow held across `.await` panics at runtime; clippy's `await_holding_refcell_ref` catches it. — [Iggy blog](https://iggy.apache.org/blogs/2026/02/27/thread-per-core-io_uring/)
  - Blocking `sync_data()` inside a task blocks a worker thread. — [muopdb](https://hunglongtrangithub.github.io/blogs/posts/muopdb-wal-write-group)
  - Slow syncs can starve the spawn_blocking pool. — [muopdb](https://hunglongtrangithub.github.io/blogs/posts/muopdb-wal-write-group)
  - Leaders in group-commit designs take borrowed data from other tasks, so muopdb switched to `Arc<[T]>`. — [muopdb](https://hunglongtrangithub.github.io/blogs/posts/muopdb-wal-write-group)

### Inferences
- **A per-file byte-budget semaphore** fits the project rule "no whole file in RAM; per-file memory bounded" (#98).
  - Acquire chunk-size permits before buffering a FUSE write; release them after the chunk is appended to a pack.
  - A global semaphore on top caps total daemon memory.
  - FIFO fairness means a request larger than the budget deadlocks. Cap each request at the budget size, since max FastCDC chunk (32 KiB) times queue depth is known.
- **In a threads-only design**, `crossbeam::bounded` plus a counting semaphore (std `Mutex` + `Condvar`) does the same job without tokio.

### Gaps
- No source on priority inversion was found. One example: GC or compaction holding the pack-writer lock while foreground writes wait.
- No source on byte-budget semaphore patterns in production Rust FUSE servers was found.
