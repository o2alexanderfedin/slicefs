# Fix the fsync tail before adding threads

Other file systems get parallelism from one rule, and SliceFS does not follow it yet. They let many requests run at once, and they serialize only on a per-inode lock plus a short, ordered set of global locks. SliceFS instead gets its safety from a single fuser dispatch thread. At least 15 code sites depend on that thread, most of them because per-file write state is taken out of the `open_files` map while it is in use. [SUPERSEDED: Phase 19 keeps the state in place under per-file locks, and Phase 21 runs the requests on a worker pool (`--workers`); see the Phase 19 and Phase 20 results.] The recommended design has five parts. It keeps fuser and adds no async runtime. One reader thread hands each request and its `Reply` to a SliceFS worker pool, the same way on Linux and on macOS. A per-inode lock table replaces the take-out pattern. Each log record is appended in the same critical section as its apply. A dedicated rebase flusher and a writer-pays throttle replace cross-file `keep_budget`, and group commit moves fsyncs off the request path. Threads alone will not reach the 500 ms bound of #108. The measured tail today is an 8 MiB pack fsync that runs inline under the single chunk-store lock, and every worker would queue behind that lock. [SUPERSEDED: the pack sync runs in the background with a RAM cap since #131, and the writer waits only at the cap; see the S2 result.] So the first step fixes that fsync, the cache lock and the WAL lock, and none of those fixes need threads. On macOS the gain depends on two unknowns: whether FUSE-T and FSKit deliver more than one request at a time, and whether they accept replies out of order. A short spike must settle these before the macOS pool is turned on.

## What can reach a server at the same time

On Linux the kernel gives almost no ordering guarantee. The FUSE documentation names only one rule: "INTERRUPT requests take precedence over other requests" ([fuse.rst](https://raw.githubusercontent.com/torvalds/linux/master/Documentation/filesystems/fuse/fuse.rst)). A server sees per-inode serialization only where the kernel holds a lock while it sends the request. **Buffered write-through writes hold the exclusive `i_rwsem`**, so `write(2)` sends WRITEs for one inode one at a time. FSYNC is sent only after all earlier WRITEs of the inode were answered, and a truncating SETATTR waits for outstanding writeback WRITEs ([fs/fuse/file.c](https://github.com/torvalds/linux/blob/master/fs/fuse/file.c), [fs/fuse/dir.c](https://github.com/torvalds/linux/blob/master/fs/fuse/dir.c)). Several other paths do arrive concurrently for one inode. **Readahead READs are async background requests.** With `FUSE_WRITEBACK_CACHE`, many WRITEs of one inode are in flight together from a kernel thread. FLUSH takes no inode lock. RELEASE is normally sent in the background, so a new OPEN can reach the server before the RELEASE of an earlier handle ([file.c](https://github.com/torvalds/linux/blob/master/fs/fuse/file.c)). LOOKUP and READDIR in one directory are serialized by a per-directory mutex until the server sets `FUSE_PARALLEL_DIROPS` ([fs/fuse/inode.c](https://github.com/torvalds/linux/blob/master/fs/fuse/inode.c)). Requests for different inodes are never serialized. So per-inode WRITE serialization depends on the mount mode. The SliceFS code comment "FUSE serializes per-fh writes" is true only with one dispatch thread ([filesystem.rs:1090-1211](https://github.com/o2alexanderfedin/slicefs/blob/6fd1e11/crates/slicefs-cli/src/filesystem.rs#L1090-L1211)). A correct design must not rely on it.

The Linux servers handle this with a plain worker model. libfuse's `fuse_session_loop_mt()` runs up to `max_threads` workers, and each one reads, handles and replies. libfuse does no per-inode locking for the server ([libfuse ChangeLog](https://github.com/libfuse/libfuse/blob/master/ChangeLog.rst)). fuser 0.17 added the same model through `Config::n_threads` and `FUSE_DEV_IOC_CLONE` ([fuser CHANGELOG](https://github.com/cberner/fuser/blob/master/CHANGELOG.md)). The vendored fuser 0.18 starts one `fuser-{i}` thread per channel. **It refuses `n_threads != 1` and `clone_fd` outside Linux** ([session.rs:246-330](https://github.com/o2alexanderfedin/slicefs/blob/6fd1e11/vendor/fuser/src/session.rs#L246-L330)). SliceFS never sets either option, so every mount runs exactly one thread ([mount.rs:161-185](https://github.com/o2alexanderfedin/slicefs/blob/6fd1e11/crates/slicefs-cli/src/mount.rs#L161-L185)). Two facts make an in-process pool workable. A fuser `Reply` is `Send + 'static`, so it can be moved to another thread and answered later ([reply.rs:104](https://github.com/o2alexanderfedin/slicefs/blob/6fd1e11/vendor/fuser/src/reply.rs#L104)). And on FUSE-T the send path already holds `STREAM_SEND_LOCK`. **The receive path, `receive_framed`, has no lock**, so two reader threads on the FUSE-T stream socket could split one frame between them ([channel.rs:66-160](https://github.com/o2alexanderfedin/slicefs/blob/6fd1e11/vendor/fuser/src/channel.rs#L66-L88)). FUSE over io_uring (Linux 6.14) and passthrough (6.9) change throughput, not which requests may overlap ([fuse-io-uring.rst](https://docs.kernel.org/filesystems/fuse/fuse-io-uring.html), [6.9 pull](https://lore.kernel.org/all/171052321821.31681.13880051934148095319.pr-tracker-bot@kernel.org/T/)). fuser has no io_uring transport. Passthrough needs a single backing file per user file, which a chunk store does not have.

macOS is the real unknown. The FSKit headers say nothing about whether `FSVolume` operations run concurrently. The only threading text says the attribute get and set operations "enforce thread safety", and it asks modules to fill each `FSItemAttributes` within one "synchronization context" ([FSItem](https://developer.apple.com/documentation/fskit/fsitem)). An Apple DTS engineer wrote on the forums that "all operations are received over NSXPC, on a serial queue", and advised modules to dispatch work to other queues ([Apple Developer Forums](https://developer.apple.com/forums/thread/831338?answerId=891228022)). That is not official documentation. Also, SliceFS does not talk to FSKit directly. It talks to FUSE-T, whose server is closed source. The FUSE-T wiki says nothing about threading or the socket protocol ([FUSE-T wiki](https://github.com/macos-fuse-t/fuse-t/wiki)), and macFUSE's FSKit backend does not document its threading either ([macFUSE 5.3.0](https://macfuse.github.io/2026/06/08/macfuse-5.3.0.html)). So SliceFS must lock as if requests can overlap. It must also measure whether they actually do, because if FUSE-T waits for each reply, a macOS worker pool overlaps nothing.

## Three kernel file-system patterns that SliceFS can copy

**The first pattern is a per-inode lock with a fixed rank order.** The VFS takes `i_rwsem` shared for lookup and readdir, and exclusive on the parent and the victim for create, unlink, rename and setattr ([locking.rst](https://docs.kernel.org/filesystems/locking.html)). Deadlock freedom comes from a strict rank. A per-superblock rename mutex is taken only for cross-directory rename, and it comes before any inode lock. Directories are locked ancestor-first. Non-directories are locked "in order by increasing address", and the common-ancestor check "is crucial" to the proof ([directory-locking.rst](https://docs.kernel.org/filesystems/directory-locking.html)). XFS orders two inodes by inode number and fixes the order of its own lock levels "to prevent deadlock" ([xfs_inode.c](https://raw.githubusercontent.com/torvalds/linux/master/fs/xfs/xfs_inode.c)). Range locks for writes inside one file are still not in mainline. A 2019 benchmark showed them slower than a shared rwsem ([Chinner, 2019](https://lore.kernel.org/linux-xfs/20190416122240.GN29573@dread.disaster.area/)), and a 2025 thread still has no range lock that performs well ([LKML, Jan 2025](https://lkml.iu.edu/hypermail/linux/kernel/2501.1/06354.html)). For SliceFS this means range locking is not worth the cost. In write-through mode the kernel already sends WRITEs for one inode one at a time, so a per-inode mutex is then mostly uncontended. With writeback cache or over FUSE-T this is not known, and spike S4 measures it.

**The second pattern is join-and-drain commits that never wait while holding a handle.** jbd2, btrfs and ZFS all use the same model as the SliceFS commit gate. Writers "join" the open transaction with a cheap hold (a jbd2 handle, btrfs `num_writers`, ZFS `txg_hold_open`), and the commit waits for the holders to drain ([journalling.rst](https://docs.kernel.org/filesystems/journalling.html), [btrfs transaction.h](https://raw.githubusercontent.com/torvalds/linux/master/fs/btrfs/transaction.h), [txg.c](https://raw.githubusercontent.com/openzfs/zfs/master/module/zfs/txg.c)). The kernel designs add two rules. First, "there is always a txg in the open state", so new writers join the next generation while the old one quiesces and syncs. Writers block only for the drain, not for the flush ([txg.c](https://raw.githubusercontent.com/openzfs/zfs/master/module/zfs/txg.c)). Second, **a thread must never wait for a commit or for space while it holds a lock that the commit needs**. ZFS returns ERESTART, and the caller must "drop all locks, call dmu_tx_wait(), and try again" ([dmu_tx.c](https://raw.githubusercontent.com/openzfs/zfs/master/module/zfs/dmu_tx.c)). jbd2 says you cannot force a commit "if we don't have an active handle, otherwise, we will deadlock" ([journalling.rst](https://docs.kernel.org/filesystems/journalling.html)). Today a SliceFS commit holds the gate exclusively for the whole commit, about **1.3 s at 78k files** ([commit_timer.rs:9-12](https://github.com/o2alexanderfedin/slicefs/blob/6fd1e11/crates/slicefs-cli/src/commit_timer.rs#L9-L12)). Every worker would stall for that time. [SUPERSEDED: since #133 the gate is held only for the generation cut; see the S5 result.]

**The third pattern is a dirty budget in which the writer pays by sleeping.** Linux starts background writeback at 10% dirty and throttles dirtying tasks at 20%. Since IO-less throttling, the dirtier sleeps in `balance_dirty_pages()` for at most 200 ms per pause, on a smooth curve, and does not write back pages itself ([page-writeback.c](https://raw.githubusercontent.com/torvalds/linux/master/mm/page-writeback.c), [LWN](https://lwn.net/Articles/456904/)). The old design made the dirtier do the writeback, and that gave "terrible I/O patterns" ([LWN](https://lwn.net/Articles/456904/)). Linux also uses per-CPU counters and checks the exact sum only near the limit. ZFS starts a txg sync at 20% of `zfs_dirty_data_max`. Above 60% it delays each transaction by `scale*(dirty-min)/(max-dirty)`, scheduled relative to the last waiter's wakeup, so total throughput stays bounded however many threads write. At 100% it blocks on a condvar that frees when space frees ([zfs(4)](https://openzfs.github.io/openzfs-docs/man/master/4/zfs.4.html), [dmu_tx.c](https://raw.githubusercontent.com/openzfs/zfs/master/module/zfs/dmu_tx.c)). SliceFS's `keep_budget` does the opposite. A writer over the 64 MiB `DIRTY_BUDGET` picks the largest dirty file, which may belong to someone else, and runs that file's rebase step inline ([filesystem.rs:1214-1283](https://github.com/o2alexanderfedin/slicefs/blob/6fd1e11/crates/slicefs-cli/src/filesystem.rs#L1214-L1283)). This is the 2011 Linux mistake, and it is also the most thread-unsafe code in the daemon.

## SliceFS today: one thread, 15 hidden assumptions, one inline fsync

Most of the request work runs on the one `fuser-0` thread: FastCDC, BLAKE3 and zstd for every written byte, rebase steps, fsync, and waits for the timed commit and the GC start ([filesystem.rs:1180-1205](https://github.com/o2alexanderfedin/slicefs/blob/6fd1e11/crates/slicefs-cli/src/filesystem.rs#L1180-L1205), [chunk-store file.rs](https://github.com/o2alexanderfedin/slicefs/blob/6fd1e11/crates/chunk-store/src/file.rs#L160)). PR #122 replaced the whole 32 MiB rebase with bounded steps. That cut the slowest single write from **3361 / 2751 ms to 1124 / 1772 ms** in the 1 GiB / 4 GiB Docker bigwrite runs, and rebase steps now take 73-112 ms at p99 ([PR #122](https://github.com/o2alexanderfedin/slicefs/pull/122)). [SUPERSEDED: the "1124 / 1772 ms" baseline was replaced by the measurement on the current code; see the S2 result.] The same PR names what remains: **"Every write above 400 ms now contains a pack fsync of 0.35-1.7 s."** The pack is synced every 8 MiB (`KEEP_LIMIT`) on the FUSE thread ([PR #122](https://github.com/o2alexanderfedin/slicefs/pull/122)). [SUPERSEDED: the pack sync runs in the background with a RAM cap since #131, and the writer waits only at the cap; see the S2 result.] That sync runs inside `RetryingPack::write_at`, which `ChunkStore::put` calls while it holds the store mutex ([retry.rs:86-94](https://github.com/o2alexanderfedin/slicefs/blob/6fd1e11/crates/chunk-store/src/retry.rs#L86-L94), [chunk-store lib.rs:1092-1160](https://github.com/o2alexanderfedin/slicefs/blob/6fd1e11/crates/chunk-store/src/lib.rs#L1092-L1160)). `ChunkStore::read` must take the same lock to check the generation and get the pack handle ([lib.rs:2000-2045](https://github.com/o2alexanderfedin/slicefs/blob/6fd1e11/crates/chunk-store/src/lib.rs#L2000-L2045)). So with N threads, one inline fsync stalls every writer and every cold reader, not only its own request. More threads move that tail around but do not remove it. [SUPERSEDED: the inline sync under the store mutex no longer exists since #131; see the S2 result.] Two smaller global points add to the problem. `read_root` holds the global `readers` mutex across the whole `Reader::read` (index lookups, pread, zstd, BLAKE3 verify) ([filesystem.rs:968-981](https://github.com/o2alexanderfedin/slicefs/blob/6fd1e11/crates/slicefs-cli/src/filesystem.rs#L968-L981)). And `flush_wal` holds the `wal` mutex during `flush_and_sync`, so one fsync blocks every log append ([store.rs:360-430](https://github.com/o2alexanderfedin/slicefs/blob/6fd1e11/crates/metadata/src/store.rs#L360-L372)). [SUPERSEDED in part: `flush_wal` uses a cloned handle now, but the log append and the sync of the strategy writer still run under their own mutexes; see the S3 result.]

The lower layers are already built for concurrency. In `ChunkStore`, hashing, compression and index lookups run outside the store lock and are checked again by generation. The memtable is sharded, and race tests cover put and read against GC and merge ([race_tests.rs](https://github.com/o2alexanderfedin/slicefs/blob/6fd1e11/crates/chunk-store/src/race_tests.rs)). Online GC already runs mark and sweep beside requests, with roots taken under the exclusive gate ([filesystem.rs:640-704](https://github.com/o2alexanderfedin/slicefs/blob/6fd1e11/crates/slicefs-cli/src/filesystem.rs#L640-L704)). Each `DictMetadataStore` call is thread-safe and has a documented lock order ([store.rs:1-14](https://github.com/o2alexanderfedin/slicefs/blob/6fd1e11/crates/metadata/src/store.rs#L1-L14)). The request clock is thread-local ([oplog.rs:58-88](https://github.com/o2alexanderfedin/slicefs/blob/6fd1e11/crates/slicefs-cli/src/oplog.rs#L58-L88)). The unsafe part is the file-system layer above these. **Write, settle, setattr and `keep_budget` take `OpenFileState` out of `open_files` for the whole operation.** Other paths treat a missing entry as "no such handle". A concurrent write gets `EBADF`. A read returns the committed manifest, which is stale. fsync returns success as a no-op. A second open creates an independent state, and that brings back "last close wins". `release` skips `finalize_write`. `handle_write` silently skips the log record. The dirty budget undercounts by up to (threads − 1) times the per-file cost ([filesystem.rs:1103-1263, 1446-1490, 1657-1690, 2421-2438](https://github.com/o2alexanderfedin/slicefs/blob/6fd1e11/crates/slicefs-cli/src/filesystem.rs#L1103-L1211), [handlers.rs:316-332](https://github.com/o2alexanderfedin/slicefs/blob/6fd1e11/crates/slicefs-cli/src/handlers.rs#L316-L332)). The `keep_budget` comment states the dependency: "A mount with more dispatch threads would need the state kept in the map during the step" ([filesystem.rs:1214-1231](https://github.com/o2alexanderfedin/slicefs/blob/6fd1e11/crates/slicefs-cli/src/filesystem.rs#L1214-L1231)). There are also three more problems:

- **Log order.** Handlers apply first and append afterwards, in a separate critical section. Replay checks that each Create gets back the same inode number (`got == *ino`) and refuses the mount if it does not ([handlers.rs:102-125](https://github.com/o2alexanderfedin/slicefs/blob/6fd1e11/crates/slicefs-cli/src/handlers.rs#L102-L125), [oplog.rs:250-280](https://github.com/o2alexanderfedin/slicefs/blob/6fd1e11/crates/slicefs-cli/src/oplog.rs#L250-L280)). With two threads, two creates can be logged out of allocation order, and the next mount fails.
- **Lost updates.** `install_root` and the inode helpers read, modify and write metadata with no per-inode lock ([filesystem.rs:816-838, 1023-1039](https://github.com/o2alexanderfedin/slicefs/blob/6fd1e11/crates/slicefs-cli/src/filesystem.rs#L816-L838)).
- **Check-then-act namespace operations.** For example, unlink can see "no open handle" while a concurrent open adds one ([filesystem.rs:577-611, 2118-2296](https://github.com/o2alexanderfedin/slicefs/blob/6fd1e11/crates/slicefs-cli/src/filesystem.rs#L577-L611)).

No multithreaded FUSE test exists in the repo, so none of these races would be caught today. [SUPERSEDED: the stress harness (`stress_harness.rs`, Phase 18) and the parallel replay tests (Phase 19) cover them now.]

## The recommended design: one reader, a pool, per-inode locks, flushers

**Dispatch.** Keep the vendored fuser and add no async runtime. Keep one fuser reader thread on both platforms. Each callback copies its arguments, moves them and its `Reply` into a bounded queue, and returns at once. A SliceFS worker pool of N OS threads takes requests from that queue. The worker, not the reader, takes the commit gate shared, because a `std::sync::RwLock` read guard cannot move to another thread. Queued requests hold no gate and no lock. The fuser `write` callback gets `data: &[u8]` borrowed from the reader's buffer ([fuser lib.rs:620-631](https://github.com/o2alexanderfedin/slicefs/blob/6fd1e11/vendor/fuser/src/lib.rs#L620-L631)), so each queued WRITE costs one payload copy. The queue must therefore be bounded in bytes, and the reader blocks when it is full. This is the only model that works on macOS without changing how frames are read. It is the same idea as the DTS advice to FSKit modules, although SliceFS sits behind FUSE-T and not on the FSKit queue itself. Using it on Linux as well gives one code path and one place to drain workers before `destroy()`. `destroy()` finalizes handles and closes the chunk store, so it must not run while workers still hold requests ([filesystem.rs:2663-2690](https://github.com/o2alexanderfedin/slicefs/blob/6fd1e11/crates/slicefs-cli/src/filesystem.rs#L2663-L2690)). Linux `n_threads` with `clone_fd` stays a later option, for use only if the reader thread turns out to be a bottleneck. An async runtime gives no gain here. The heavy work (FastCDC, BLAKE3, zstd) is CPU-bound. `tokio::fs` itself runs blocking calls on a thread pool ([tokio::fs](https://docs.rs/tokio/latest/tokio/fs/index.html)). macOS has no io_uring. The Rust dedup tools surveyed (rustic, ffsync, hexz) all use OS threads and bounded crossbeam channels ([rustic_core packer](https://docs.rs/crate/rustic_core/0.12.0/source/src/blob/packer.rs), [hexz](https://docs.rs/hexz-ops/0.8.2/src/hexz_ops/parallel_pack.rs.html)). The strongest 2026 io_uring data, from Apache Iggy moving to compio, mixes a runtime change with other release changes. Its gains are small at low load and grow with load ([Iggy](https://iggy.apache.org/blogs/2026/02/27/thread-per-core-io_uring/)), and that does not describe a FUSE daemon that is bound by fsync.

**Per-inode state and locks.** Use a sharded inode-lock table, as planned in #21 ([#21](https://github.com/o2alexanderfedin/slicefs/issues/21)). Each open inode gets one `Arc<Mutex<OpenFileState>>`, and every alias handle shares it, so `write_alias` becomes a pointer instead of a key lookup. Per-file state never leaves the map. A thread that needs it locks it, so other threads wait instead of seeing "absent". Each file's dirty cost is mirrored in an atomic, so the budget can be summed without locking any file. The `OpenFileState` mutex is the file's inode lock, not a separate level. The lock rank follows the VFS and #21:

| Rank | Lock | Order inside the rank |
|---|---|---|
| 1 | Commit gate, shared | One per request |
| 2 | Rename mutex | Only for cross-directory rename |
| 3 | Inode locks (directories and files, including `OpenFileState`) | Increasing inode number, as in #21 and XFS |
| 4 | Short map locks (`open_files`, `handles`, `orphans`) | Never held across I/O |
| 5 | Chunk-store and WAL internals | Their existing documented order |

Ordering by inode number avoids the need to find ancestry, which SliceFS cannot get cheaply. Cross-directory rename then needs the rename mutex plus a check that the source is not an ancestor of the target, as the VFS does. Add a debug-build checker for this order. **Do not add range locks.** The overlay already absorbs reordered writes, and the kernel designs show range locks lose to a plain lock.

**Log order.** Append each log record while the inode lock (and, for namespace operations, the directory locks) is still held, directly after the apply. Apply and append also happen inside one shared hold of the commit gate, so a commit's root record can never fall between an apply and its log record. Ops that touch the same inode or directory are then logged in the order they were applied. For creates, replay should create with the logged inode number and raise `next_ino` to at least that number, instead of checking `got == *ino`. Ops on unrelated inodes then commute, and no global log mutex is needed around the apply. This is a format and replay change, which the project allows because there are no deployments to stay compatible with.

**Commit gate.** Keep the RwLock for now. Every request holds it shared only for the time it works on its own data, and it must release the gate before any wait for budget or durability (the ZFS ERESTART rule). The reason is a deadlock. Suppose a writer sleeps on the budget while holding the gate shared, and the commit timer queues for the exclusive gate. If the lock lets that waiting writer block new readers, the rebase flusher cannot take the gate shared to free budget, and nothing moves. Whether `std::sync::RwLock` behaves this way was not verified, so spike S5 must measure it. Later, the btrfs and ZFS shape removes the 1.3 s stall. The exclusive hold would only switch the open generation (WAL segment and dirty set), and the old generation would be written out with the gate released.

**Dirty budget.** Use two thresholds, as Linux and ZFS do. At a soft threshold, wake a dedicated **rebase flusher thread**. It takes the gate shared and the inode lock, runs `rebase_step` on the largest dirty file, and releases both. Because it holds the gate shared exactly as a callback does today, it keeps the GC invariant that a rebase "never straddles `gc_begin`" ([filesystem.rs:640-650](https://github.com/o2alexanderfedin/slicefs/blob/6fd1e11/crates/slicefs-cli/src/filesystem.rs#L640-L650)). This is #108 option 1, done without breaking that invariant. Above the soft threshold a writer sleeps on a ZFS-style curve with each pause capped. At the hard threshold it waits on a condvar that the flusher signals. This wait is an admission step: the worker does it before it takes the gate and the inode lock, so it holds neither while it waits. A writer may still run a bounded step on its own file inline as a fallback, but **never on another file's state**. The flusher and the fsync and release paths are never throttled, as with ZFS sync and the Linux flusher. Otherwise every worker could block on budget while the work that frees budget waits for a worker. With one global atomic total, overlay memory stays near `DIRTY_BUDGET` no matter what N is. Two other buffers must also be capped to meet the bounded-memory rule of #98: the dispatch queue (payload copies, bounded in bytes) and the retry layer's kept copies (see below).

**I/O path.** Three changes take blocking I/O off the request locks:

- The `KEEP_LIMIT` sync moves out of the store lock. A pack-sync thread fsyncs sealed or full-enough ranges while appends go on, and the retry layer keeps its copies until that sync confirms. Those copies grow while a slow fsync runs, and pack fsync stalls of 3.3 s and one of 9.4 s were measured ([slow-single-fuse-write.md](https://github.com/o2alexanderfedin/slicefs/blob/6fd1e11/.planning/debug/slow-single-fuse-write.md)). So the kept bytes need a hard cap, and an appender that reaches it waits for the sync.
- The `readers` cache lock is held only to clone an `Arc<Reader>`.
- The WAL becomes the "Version 2" in its design doc: appends under a short mutex, one flusher thread, and concurrent fsyncs sharing one sync through a `durable_lsn` and a condvar ([write-log.md](https://github.com/o2alexanderfedin/slicefs/blob/6fd1e11/docs/design/write-log.md), [#21](https://github.com/o2alexanderfedin/slicefs/issues/21)).

The evidence for group commit is consistent, though not measured on FUSE. In muopdb's benchmark of 1000 concurrent WAL writes, leader and follower write groups took about 37 ms against about 4.3 s for a sync per write. A Linux rerun reported 166 ms vs 32 ms, but the post does not say which two designs that compares, and its table is partly garbled ([muopdb](https://hunglongtrangithub.github.io/blogs/posts/muopdb-wal-write-group)). PostgreSQL groups commits even at `commit_delay = 0`, because the sessions that arrive during one flush share the next one ([PostgreSQL WAL](https://www.postgresql.org/docs/current/wal-configuration.html)). Keep one pack appender for now. CPU work already runs outside the store lock, and Rust dedup tools such as rustic feed one packer through bounded channels ([rustic_core packer](https://docs.rs/crate/rustic_core/0.12.0/source/src/blob/packer.rs)).

| Single-thread dependency (SliceFS note Q5) | Closed by |
|---|---|
| 1 keep_budget takes another file's state; 16 budget undercount | Rebase flusher, per-file atomic cost, no cross-file inline steps |
| 2-7, 15 take-out pattern (EBADF, stale read, no-op fsync, duplicate alias state, skipped finalize, `expect` panic, unlogged write) | State stays in the map behind a per-inode mutex |
| 8 log order differs from apply order | Append under the inode/dir lock; replay with the logged inode number |
| 9-10 refcount and inode read-modify-write | Done under the inode lock |
| 11 multi-step namespace operations | Directory and inode locks in rank order, plus the rename mutex |
| 12 getlk/setlk no-ops | Not closed: wrong even with one thread; needs separate work |
| 13 per-write test counters | Make them per-request or per-thread |
| 14 commit_cut and GC invariant | Kept: callbacks and the flusher take the gate shared |

## Migration in eight steps, with the risks and the spikes that decide

Each step ships alone and is kept small. Steps 1-4 still run on one dispatch thread, so behaviour stays the same while the invariants change underneath. Step 5 adds the first new thread that touches file state, so it depends on steps 0 and 2-4. Step 6 turns on concurrent requests.

| Step | Change | What it settles | Issues |
|---|---|---|---|
| 0 | Stress harness: N client threads doing mixed write, read, fsync, open/close, rename on shared files; per-op latency histograms; lock-hold timers on the store, gate and WAL | Detects the races above; gives the baseline | #22 |
| 1 | Move the `KEEP_LIMIT` sync off the store lock; narrow the `readers` lock; WAL sync outside the `wal` mutex | Removes the measured 0.35-1.7 s tail; may close #108 with no threads [SUPERSEDED: shipped as #131 and #133; see the S2 result] | #108 |
| 2 | Per-inode lock table; `OpenFileState` stays in the map; shared alias state; per-file atomic dirty cost | Items 1-7, 15, 16 | #21, #105 |
| 3 | Log append under the inode lock; replay with the logged inode number | Item 8 | #20 |
| 4 | Directory locks, rename mutex, rank-order checker; read-modify-write under locks | Items 9-11 | #21 |
| 5 | Rebase flusher thread under the shared gate; soft and hard thresholds; writer-pays sleep; remove cross-file `keep_budget` | #108 option 1; bounded memory under N | #108, #98 |
| 6 | Reader plus worker pool, N configurable, default 1. Raise the default on Linux in CI first, then on macOS after spike S1; drain workers before `destroy` [SHIPPED: Phase 21, `--workers N`, default on Linux half the CPUs (1 to 8), on macOS 1; see the Phase 21 result] | Concurrency on | #22 |
| 7 | WAL group-commit flusher; then gate generation switch if S5 shows stalls [SUPERSEDED: the switch shipped as #133; S5 now decides the gate lock type; see the S5 result] | Durable fsync throughput; 1.3 s commit stall | #20, #21 |

The largest risk is **deadlock between the gate, the budget and the flusher**. The guard is the drop-locks-before-waiting rule, plus a test in which every worker is throttled while a commit waits. The second risk is **replay divergence** if any handler logs outside its lock. A crash-replay test with N threads should run on every change to `handlers.rs`. The third risk is that **macOS gains nothing**. If FUSE-T keeps one request outstanding, the pool only adds handoff cost, and the right result is then to leave N=1 on macOS. An older, blockset-era measurement on the macOS Boost copy put SliceFS's own `write` and `fsync` calls at about one third of the copy time, with FSKit taking the rest ([chunk-store.md](https://github.com/o2alexanderfedin/slicefs/blob/6fd1e11/docs/design/chunk-store.md)). It was not measured again for the pack store, but it suggests that the macOS gain from SliceFS-side threads has a low upper bound. The fourth risk is **the single store lock becoming the new bottleneck** once CPU work runs in parallel. FastCDC 8 KiB plus BLAKE3 runs at 530-910 MB/s ([chunk-store.md](https://github.com/o2alexanderfedin/slicefs/blob/6fd1e11/docs/design/chunk-store.md)), so a few workers could saturate one appender. That would justify per-worker packs, but only after measurement. Two smaller risks remain. Out-of-order replies on FUSE-T are unverified ([channel.rs](https://github.com/o2alexanderfedin/slicefs/blob/6fd1e11/vendor/fuser/src/channel.rs#L141-L160)). And the test-time budget limits how long the stress runs can be in CI.

The spikes below are short scenarios. Each one settles a question that the sources cannot.

| Spike | Scenario | Measure | Decides |
|---|---|---|---|
| S1 FUSE-T concurrency and reply order | macOS FSKit mount, one reader plus a 4-worker pool; delay one READ reply by 200 ms while another process stats files | Maximum requests in flight; whether later replies complete first; any errors | Whether to turn on the macOS pool (step 6) |
| S2 Inline fsync removed | Step 1 alone; the 1 GiB and 4 GiB bigwrite random-write runs in Docker | Slowest single write vs 1124 / 1772 ms [SUPERSEDED: the post-#108 numbers 440 / 686 ms and 117 / 1542 ms are the reference now; see the S2 result] | Whether #108 closes without threads |
| S3 Store-lock contention | N = 1, 2, 4, 8; parallel copy of distinct large files (Linux) | Store-lock hold and wait histograms; MB/s | Default N; whether per-worker packs are needed |
| S4 Per-inode overlap on Linux | Current mount options; random writes plus readahead on one file | Number of concurrent requests per inode | How contended the per-inode mutex is; whether writeback cache is worth trying |
| S5 Gate fairness | Commit timer at 1 s with 78k files while 4 workers do long shared holds | Maximum wait of new shared holders while a commit waits | Lock type; when to do the generation switch |
| S6 Group commit | Many small files, each fsynced (tar-like), on Linux and on macOS | Fsyncs per second and p99, PerOp vs flusher | Step 7 priority, mainly for macOS |
| S7 Budget under N writers | 4 random writers on 1 GiB files, 256 MiB cap | Peak RSS; slowest write; flusher lag | Soft and hard thresholds; pause curve |

## Spike results (Phase 18)

This section gives the results of the seven spikes. Each result names the commit and the platform of its runs. The runs used four set-ups. One set-up is a Linux mount in a Docker container (OrbStack host, kernel 7.2.12, 32 CPUs, 15.68 GiB of memory). The second is a test in one process on Linux, in the same container, with no mount. The third is a test in one process on macOS, with no mount. The fourth is a macOS FSKit mount (FSKit is the Apple framework for file systems in user space) in a continuous integration (CI) job. The steps are the steps 0 to 7 of the migration table above. Each result ends with one **Decides:** line. Percentiles are written p50, p99 and so on. A p99 value is the value that 99 of 100 samples do not exceed. Every number below comes from a saved run log, and it is written as the run printed it.

### S1 result

S1 used a small test server (a "probe") that was built on the vendored fuser. The probe has one reader thread and a pool of 4 worker threads. It answers every READ at once, except one READ of a special file, which it delays by 200 ms. A client starts a `dd` on that file. 50 ms later, the client calls `stat` on 50 new names, one after the other. A new name forces a LOOKUP request each time. The probe counts the largest number of requests in flight (`max_inflight`) and the number of times a later request got its reply before an earlier one (`reply_order_inversions`). One run has 5 rounds of this test.

| Run | Platform | max_inflight | reply_order_inversions | Requests received | `stat` calls done during the slow READ | Slowest `stat`, ms | Slow `dd`, ms |
|---|---|---|---|---|---|---|---|
| Linux control, 1 run, commit 86f9e8c plus the S1 files | Docker mount, kernel 7.2.12 | 3 | 16 | 510 | 50 of 50 in each of 5 rounds | 0 or 1 | 203 to 204 |
| macOS FSKit, 1 run, commit 97086b2d6a45337a0f2469472f2ad492483945ea | macOS 26.7.2 (Darwin 25.6.0), FUSE-T 1.2.9 | 2 | 5 | 548 | 50 of 50 in each of 5 rounds | 2 | 211 to 298 |

The macOS run is the CI run [actions/runs/44882009650](https://github.com/o2alexanderfedin/slicefs/actions/runs/44882009650). The Linux control shows that the probe can see overlap, so the macOS number 2 is not a fault of the probe. Neither run had a client error or a hang. The macOS `dd` took 297 and 298 ms in rounds 0 and 1, and 211 to 248 ms in the later rounds. On Linux it took 203 to 204 ms in every round. The extra time on macOS is not explained.

The macOS default can be more than one worker. FUSE-T 1.2.9 on macOS 26.7.2 gave the probe 2 requests at the same time, it accepted 5 replies out of order, and all 250 `stat` calls finished while the 200 ms READ was still open.

The limits are these. The test is one slow READ against a loop of `stat` calls, so a maximum of 2 in flight is a small sample, and it does not show a gain in throughput. The run was one CI run on one macOS version. Phase 21 must show the throughput gain on macOS before the default goes above 1.

**Decides:** step 6 (the macOS default may be raised above 1 after Phase 21 shows a throughput gain; the pool needs no special case for out-of-order replies on FUSE-T 1.2.9)

### S2 result

S2 ran the bigwrite test on the Linux mount, on code commit f31ef2e (the SliceFS code is the code of commit 98e3522, and only scripts changed after it). It ran 3 runs one after the other for each size, with a release build and nothing else active. Each run writes 65792 random writes. The store was on overlayfs inside the container. The #108 spike used btrfs, so the file system is not the same. The table shows the slowest single write and the slowest fsync of each run (minimum, median and maximum of 3 runs).

| Size | Slowest write, ms (min / median / max) | Slowest fsync, ms (min / median / max) | Peak VmHWM, MiB |
|---|---|---|---|
| 1 GiB | 124 / 224 / 244 | 136 / 308 / 621 | 112 |
| 4 GiB | 314 / 343 / 1353 | 220 / 677 / 1637 | 170 |

The post-#108 references are 440 / 686 ms at 1 / 4 GiB on commit 90a5443, and 117 / 1542 ms on commit 3f3ff34. The new runs are in the same band. The bound of #108 is 500 ms for the slowest write. Five of the six runs are under it. The one run over it is the second 4 GiB run (1353 ms). VmHWM, the peak resident memory of the daemon, stays under the cap of 256 MiB in all six runs (the maximum is 170 MiB).

What is left in the tail is a disk effect, as far as these runs show. The second 4 GiB run had one pack sync of 1756 ms and 16.7 s of total pack sync time over 231 syncs. The other two 4 GiB runs had 4.2 s and 5.1 s of total pack sync time, and a longest sync of 65 ms and 235 ms. This link to the disk is an inference. It was not tested, for example with a repeat on btrfs. The run count is small, so the 1353 ms value shows that the bound can be missed, and it does not give a rate.

**Decides:** step 1 (the pack sync is already off the store lock since #131, and the median slowest write is under 500 ms at both sizes, so #108 does not need threads for the typical case; the bound was missed in 1 of 6 runs, so close #108 only after the 4 GiB tail is repeated on btrfs or a disk stall is shown as the cause)

### S3 result

S3 ran in one process, with no mount, on commit d24da49. It ran 4 values of N (1, 2, 4 and 8 threads). Each thread has its own file and its own handle, and writes 256 MiB in 1 MiB writes with a dirty limit of 4 MiB per file. This is the safe subset: no thread touches the state of another file, and the total dirty size stays under the budget. The data check found 0 mismatches at every N on both platforms. The test ran twice on each platform, 1 run per N each time. The first pass has the lock recorder off, and it gives the throughput. The second pass has the recorder on, and it gives the lock rows. The recorder adds a shared mutex for each lock event, so it changes the timing, and the two passes must not be mixed. The throughput is the bytes of a phase divided by the longest time that any thread spent in the wrapped calls (MB means MiB). Linux ran in the container (rustc 1.100.0, store on overlayfs, 32 CPUs). macOS ran in the test process (rustc 1.95.0-nightly, 32 CPUs). The log of the macOS runs does not name the file system type of the store.

| N | Linux write MB/s | Linux read MB/s | macOS write MB/s | macOS read MB/s |
|---|---|---|---|---|
| 1 | 117.9 | 854.3 | 79.7 | 689.8 |
| 2 | 160.4 | 918.5 | 102.4 | 786.2 |
| 4 | 132.9 | 319.5 | 110.3 | 766.8 |
| 8 | 120.4 | 762.1 | 112.1 | 783.3 |

The Linux read value at N = 4 (319.5) is one run, and it is lower than the three other values. The lock pass at N = 4 had 813.4 MB/s, and the dip was not examined. Write throughput does not scale with N. On Linux it rises 1.36 times at N = 2 and is 2% above N = 1 at N = 8. On macOS it rises about 1.4 times from N = 1 to N = 8, and it is flat from N = 4 (110.3 and 112.1).

The lock rows come from the second pass. All times are in microseconds (us). The `store` lock is the lock of the chunk store, and the `readers` lock is the lock of the reader cache. The `wal` lock is the log append lock. The `gate_wait` p99 is 0.7 us or less in both passes, so the commit gate costs nothing here.

Linux, lock pass:

| N | store wait p99 | store wait max | wal wait p99 | readers wait p50 | readers wait p99 | readers hold p50 |
|---|---|---|---|---|---|---|
| 1 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 887.3 |
| 2 | 145.9 | 3506.2 | 4976.6 | 814.1 | 1417.2 | 1045.0 |
| 4 | 1234.9 | 11485.2 | 2730.0 | 3596.3 | 4579.3 | 1215.5 |
| 8 | 3381.2 | 20611.1 | 3741.7 | 8716.3 | 12492.8 | 1187.8 |

macOS, lock pass:

| N | store wait p99 | store wait max | wal wait p99 | readers wait p50 | readers wait p99 | readers hold p50 |
|---|---|---|---|---|---|---|
| 1 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 1223.7 |
| 2 | 58.7 | 39878.7 | 1233.9 | 989.2 | 1997.8 | 1352.7 |
| 4 | 344.1 | 42958.8 | 1991.7 | 3842.0 | 6135.8 | 1397.8 |
| 8 | 1831.9 | 47480.8 | 2601.0 | 8994.8 | 12746.8 | 1322.0 |

The `readers` lock is the one that grows with N. Its wait p50 goes from 0.0 to 8716.3 us (Linux) and 8994.8 us (macOS), while its hold p50 stays between 887.3 and 1397.8 us. The cause is the guard that is held across the whole `Reader::read`. The `store` lock has a wait p50 of 0.0 at every N. It waits only in the tail: the p99 is 3381.2 us (Linux) and 1831.9 us (macOS) at N = 8. The p50 hold of the `store` lock is 0.6 to 1.2 us. The `wal` mutex is held for 1176.6 us (macOS) to 2437.1 us (Linux) at p50 with N = 1, and the writer lock of the strategy (`wal_writer`) for 841.2 and 1909.8 us. So the `wal` mutex, and not only the `store` lock, makes threads wait. This is the part of the old text about `flush_wal` that holds. The `open_files` lock waits at most 752.6 us in all runs, and the `io` lock waits 0.0 us at p50, p99 and maximum.

The limits are these. S3 is a baseline of the lock layout of today. It used one file per thread, so it does not test per-inode contention. It ran with no mount, so the FUSE cost is not in it. It is one run per N and pass. The reader cache lock must be narrowed first (step 1), and S3 must run again after that change.

**Decides:** step 6 (no evidence for per-worker packs, because the `store` lock waits 0.0 us at p50 and 3381.2 us at p99 at N = 8; the data gives no support for a default N above 2 on today's locks, because write throughput grows little past N = 2 (on Linux it falls back to 120.4 MB/s at N = 8, and on macOS N = 8 is about 9% above N = 2) and the `readers` lock queues the reads; the number is a baseline, and Phase 21 sets the default after the `readers` lock is narrowed)

### S4 result

S4 ran on the Linux mount, on commit 49f5a21, with a release build and kernel 7.2.12. It sampled the `waiting` counter of the FUSE connection about every 2 ms, in three passes of 60 s each. The file is one file of 256 MiB. The pass "write" does random writes. The pass "readahead" reads the file with `dd` after the page cache of the file is dropped (`fadvise`), so that the reads reach the daemon. The pass "both" does both on the same file. A sanity check stopped the daemon and called `stat` once. The counter was 1, so the counter does count a request that waits.

| Pass | Samples | waiting max | p50 | p99 | Histogram (value:count) |
|---|---|---|---|---|---|
| write | 25523 | 1 | 1 | 1 | 0:5012, 1:20511 |
| readahead | 26030 | 2 | 2 | 2 | 0:1783, 1:6961, 2:17286 |
| both | 25753 | 7 | 3 | 5 | 0:456, 1:1407, 2:5617, 3:15382, 4:2597, 5:271, 6:22, 7:1 |

Random writes alone keep at most 1 request waiting. This agrees with the rule that the kernel sends WRITEs of one inode one at a time in write-through mode. Reads alone reach 2. Reads and writes on one file together reach 7, with p50 3. So the per-inode mutex will see overlap mainly between reads and writes, and not between writes.

The limits are these. `waiting` counts the whole connection and not one inode. The test used one active file, so the count is the count of that file in practice. SliceFS has one dispatch thread, so the test shows only the queue in the kernel, and it does not show how many requests a pool would run at the same time. The writeback cache was not tried. The mount used the current options.

**Decides:** step 2 (the per-inode mutex sees overlap of reads and writes of one file, up to 7 waiting requests, and no overlap of writes alone, so a plain mutex is enough and range locks stay out; the writeback-cache trial is not shown to be needed, because the write pass reached only 1, and it stays open until someone measures it)

### S5 result

S5 ran in one process, with no mount, on commit d24da49. The store has 78000 empty files in 100 directories. Four threads take the commit gate shared and hold it for 50, 200 and 500 ms. One thread does real operations. A probe thread takes the gate shared every 10 ms. A commit runs every second for 30 s. The run was done once on each platform. Linux ran in the container (rustc 1.100.0, 32 CPUs, store on overlayfs). macOS ran in the test process (rustc 1.95.0-nightly, 32 CPUs). The log of the macOS run does not name the file system type of the store.

The source of the standard library says that `std::sync::RwLock` is writer-preferring on both platforms. A new reader waits when a writer waits. On Linux the check is `is_read_lockable`, and on macOS the code is the queue lock in `queue.rs`. This source was read on the macOS toolchain (rustc 1.95.0-nightly). The Linux run used a newer rustc (1.100.0), so the measurement below shows the behavior on that version.

| Platform | Commits | Gate wait p50 / max, ms | Hold (the cut) p50 / max, ms | Commit total p50 / max, ms | New-reader wait while a commit is active, max, ms | Same, idle, max, ms | Longest hold, ms |
|---|---|---|---|---|---|---|---|
| Linux | 19 | 450.1 / 484.4 | 9.2 / 146.5 | 524.0 / 713.6 | 597.7 | 0.0 | 505.8 |
| macOS | 19 | 386.7 / 501.0 | 10.8 / 14.1 | 563.6 / 675.3 | 508.3 | 0.0 | 510.0 |

The longest wait of a new shared holder while a commit waits for the gate is 597.7 ms on Linux and 508.3 ms on macOS. These values are the maximum of the window "commit active". The p50 and p99 of that window also include time after the gate is released, so they are not used. The longest wait is about as long as the longest hold (505.8 and 510.0 ms). On Linux it is 91.9 ms longer, and the cause of that difference was not examined. When no commit is active, the longest wait is 0.0 ms. So a waiting commit blocks every new request for up to the length of the longest shared hold. The cut itself is short: its p50 is 9.2 ms on Linux and 10.8 ms on macOS, against the 1.3 s of the old measurement.

The limits are these. The holds of 50 to 500 ms are set by the test, and they are longer than the holds of real requests. So the maximum is a bound that the test sets. It is one 30 s run on each platform, with 19 commits. The two platforms used two versions of the standard library.

**Decides:** step 7 (keep the `std::sync::RwLock` for the gate in steps 5 and 6, because the generation switch already shipped as #133 and the remaining stall is the wait for holders, which is as long as the longest hold; a lock that lets new readers pass would remove that stall but could starve the commit, and this was not measured; the rule "release the gate before any wait for budget or durability" is required for steps 5 and 6, because a new reader waits behind a waiting commit on both platforms)

### S6 result

S6 made 2000 files of 4096 bytes each. For each file it did a write, a timed fsync and a close. It ran the three write-log (WAL) strategies, two passes each, with the passes interleaved, and with GC and the commit timer off. No WAL flusher exists yet, so the test compares PerOp (a sync for each operation) with the two buffered strategies, flush-on-fsync and periodic. The arm with a flusher is not measured. Group commit is deferred in the project requirements.

Linux mount, commit 98e3522, release build, container without `--privileged`, store on overlayfs:

| Strategy | Pass | fsyncs/s | fsync p50 ms | fsync p99 ms | fsync max ms | Wall time, s |
|---|---|---|---|---|---|---|
| per-op | 1 | 71.0 | 4.84 | 143.52 | 437.24 | 29.7 |
| flush-on-fsync | 1 | 237.7 | 3.11 | 27.18 | 121.90 | 9.7 |
| periodic | 1 | 280.1 | 2.87 | 18.41 | 109.55 | 8.3 |
| per-op | 2 | 252.4 | 2.47 | 31.78 | 179.80 | 8.9 |
| flush-on-fsync | 2 | 358.4 | 2.09 | 16.36 | 168.93 | 6.4 |
| periodic | 2 | 320.1 | 2.04 | 15.21 | 402.36 | 7.0 |

In process on macOS (macOS 26.2, rustc 1.95.0-nightly, release build, store in the system temp directory, no FUSE, so only the SliceFS cost is in the numbers). The log does not name the file system type of the store.

| Strategy | Pass | fsyncs/s | fsync p50 ms | fsync p99 ms | fsync max ms | Wall time, s |
|---|---|---|---|---|---|---|
| per-op | 1 | 48.1 | 20.00 | 36.80 | 65.40 | 41.6 |
| flush-on-fsync | 1 | 44.3 | 20.91 | 41.75 | 64.65 | 45.2 |
| periodic | 1 | 45.0 | 20.81 | 41.91 | 115.61 | 44.4 |
| per-op | 2 | 47.1 | 20.25 | 36.96 | 73.33 | 42.5 |
| flush-on-fsync | 2 | 43.1 | 21.02 | 41.71 | 136.84 | 46.4 |
| periodic | 2 | 44.7 | 20.91 | 41.48 | 67.96 | 44.8 |

On Linux the buffered strategies are faster than PerOp: in pass 2, 358.4 and 320.1 fsyncs/s against 252.4, and the p99 is 15.21 to 16.36 ms against 31.78 ms. The PerOp run of pass 1 is much slower (71.0 fsyncs/s) than in pass 2. The cause was not established. The gap between the strategies is therefore not a stable number, because there are only two passes. On macOS the three strategies are within 10% of each other (43.1 to 48.1 fsyncs/s), and the fsync p50 is about 20 ms for all of them. So on macOS a buffered strategy does not help, and the cost of one fsync is the cost of the device sync. A wall time of 41.6 to 46.4 s for 2000 files is the result of that cost. A flusher that lets many fsyncs share one device sync is the only change that could lower it, and this was not measured. Fsyncs can share a sync only if they are in flight at the same time, and that needs the pool of step 6. This is an inference from the design, not a measured result.

**Decides:** step 7 (group commit has the higher priority on macOS, where one fsync costs about 20 ms and the buffered strategies do not help, and a lower priority on Linux, where one fsync costs 2.04 to 4.84 ms at p50; the gain of a flusher is not measured and needs the pool)

### S7 result

S7 ran in one process, with no mount, on commit d24da49, in the container (rustc 1.100.0, store on overlayfs, 32 CPUs). The writes are serialized, because `keep_budget` takes the state of another file, and that is not safe with free threads. There are 4 threads. Each thread has its own 1 GiB file, written in 1 MiB writes. Then each thread writes 128 MiB of the bigwrite mix (1 MiB and 4 KiB random writes, half of the bytes each). Each thread does an fsync every 64 MiB. A commit runs every 30 s. This is one run. The data check, which compares every block with its version, passed.

The reason for the serialized run does not hold after Phase 19: no function takes the state of another file now. The "Phase 20 result" section below has the numbers of the parallel runs.

| Value | Result |
|---|---|
| Peak RSS (resident set size) of the test process | 234 MiB, against the cap of 256 MiB (`within_cap=true`) |
| RSS at the start / after the writes | 42 MiB / 214 MiB |
| Slowest write | 347.1 ms |
| Rebase steps | 449 |
| Largest dirty size | 64 MiB (the dirty budget was reached) |
| Commits | 3 (gate hold max 171.8 ms, commit total max 299.9 ms) |

The margin to the cap is 22 MiB. The peak RSS is the value of the whole test process, so it also contains the model and the check buffers. It is not the same measurement as the VmHWM of the daemon in S2 (112 MiB for one 1 GiB file). The cap is reported and the test does not assert it.

This is a baseline for the serialized code. The flusher lag and the pause curve cannot be measured before Phase 20 adds the rebase flusher, so the soft and hard thresholds are only partly open. The number that it gives to step 5 is the margin. The dirty budget of 64 MiB with 4 writers already gives 234 MiB. The dispatch queue and the kept pack copies must fit in the 22 MiB that remain, or the budget must be lower.

**Decides:** step 5 (the thresholds stay open until Phase 20 measures the flusher lag and the pause curve; the baseline sets the limit that the new buffers must fit in: 22 MiB of margin at 4 writers)

### Notes after the code review

A code review after the runs (`.planning/phases/18-spikes-baseline-and-stress-harness/18-REVIEW.md`) found four points that affect how to read the recorded numbers. The code is now fixed. The spikes were not run again, so the numbers above come from the old code.

- **S1.** The old probe lowered its in-flight count after it sent a reply, so `max_inflight` could show 1 more than the true value. The fixed probe lowers the count before the reply. The S1 decision does not change. A reply-order inversion is possible only when a later request arrives before an earlier request gets its reply, so 5 inversions prove that 2 requests were in flight at the same time. The old client also started the stat calls after a fixed 50 ms sleep, so `fast_stats_during_slow` is not proof of overlap by itself. The fixed client waits until the probe logs the slow READ.
- **S6.** The Linux value is fsyncs per second of fsync time only. The macOS in-process value is files per second of wall time. Do not compare the two numbers directly. The fixed code reports both values, as `fsyncs_per_s_fsync_only` and `files_per_s_wall`.
- **S7.** The old code took the sample of the largest dirty size outside the serialization. So the sample could miss the overlay of one file, and the true value can be higher than 64 MiB. The fixed code takes the sample inside the timed write.
- The other fixes (the watchdog limit, and cleanup after a failed mount in the S4, S6 and bigwrite scripts) do not change a recorded number.

## Phase 19 result

Phase 19 (issue #21) did steps 2, 3 and 4 of the migration, and the lock parts of step 1. The core is still called from one dispatch thread in the daemon, but the stress harness now runs its target with free threads (`serialize_off_target`, in the default suite).

What is done:
- **Per-file cells instead of take-out (step 2).** Each open inode has one `FileCell` in a sharded `ino -> cell` map, and all handles of the inode share it. Writes, flush and setattr change the state in place. Nothing takes a file's state out of a map now. The dirty cost is a per-cell atomic plus a global total, and a writer over the budget steps only its own file.
- **Copy, I/O, publish.** A rebase step or a stream push copies its range under the cell lock, does the chunk, hash and store work with no cell lock, and publishes only if the file generation did not change.
  - Each cell has one step token, so one step is in flight per file. Another writer waits for the token with the cell released; a reader never waits for it.
  - A dropped step retries with no lock: a stream step at most 16 times in a row, then EIO; a rebase step of a write leaves the work to the next write or the release.
  - Hard cap (`limit + limit/8 + 2 * REBASE_STEP`): a write that would go above it waits for the step in flight or runs its own unlocked step before it applies. Each such step frees at most one final step (`FINAL_STEP`, 8 MiB, plus at most 64 KiB of a split extent), and the writer steps again until it is under the cap. So no hold copies more than one step.
  - The last release (finalize) and a size change of an open file run only unlocked steps under the token. A finalize stops the forced drops of a test hook after 8 in a row, so it ends.
  - A reader holds the cell lock only to copy the overlay view, or the stream state and the parts of pending writes in its range. It composes the pending writes on top in memory, with no settle.
- **Lock tails off the global locks (step 1).** Pack create, seal join, run write, the `KEEP_CAP` wait, the trust floor write and pack open left the store lock. Cold loads and flushes left the `dir_live`/`inode_live` shard locks (single-inode changes use `modify_inode`). The WAL append is a memory copy plus an LSN under the queue lock. The write-out runs after the apply locks are released.
  - The store and the WAL write records with no lock held, one write in flight at the end of the active pack or segment. The offset is given out under the lock, and the length and the index move only when the write is published.
  - The phase-2 fsync of `ChunkStore::sync`, the fsync of the index flush and the directory syncs run with no store lock. Dirty flags are cleared after the fsync, by mark.
  - A log cut creates its next segment file before it takes the WAL writer lock.
- **Stripes and the rank checker (step 4).** A fixed array of namespace stripes and a rename mutex for cross-directory rename. Locks have one rank: commit gate, rename mutex, stripes (ascending), file cell, short maps, store internals. A debug build checks the rank on each lock, and the harness worker threads also check that every cell or stripe is taken inside a shared gate hold.
- **The log under the apply locks (step 3).** Every record (writes, setattr, xattr, namespace) is appended in the lock hold of its apply and written out after the locks are released, before the reply. Replay creates each inode at its logged number.

Decisions that differ from the design text above:
- **Stripes, not inode-number order.** Two inodes are locked in ascending stripe order (the inode number is hashed to a stripe). This gives the same freedom from deadlock and also covers two inodes in one stripe.
- **The commit gate stays a std `RwLock` at rank 1.** The checker sees it through the gate helper methods. `StoreIo` also stays a std mutex.
- **No record write under the store or WAL lock.** The first plans kept a page-cache `pwrite` under the store lock, because it does no sync. On macOS such a `pwrite` waits about 20 ms behind any F_FULLFSYNC on the volume, so the record writes left the locks too.
- **The file cell is one mutex, not a read-write lock.** Readers hold it only to copy, so a read-write lock gives nothing yet.
- **`O_APPEND`:** the daemon keeps a per-handle append flag, takes the end of file under the cell lock and logs that offset, whatever offset the kernel sends.

Tests per ROADMAP Phase 19 criterion:
1. Readers do not wait for writer I/O: `reader_does_not_wait_for_rebase_io` and `reader_does_not_wait_for_streaming_push` (`lock_free_io.rs`), and the wait rows of `readers_do_not_wait_for_writer_io_rows` (`lock_wait_rows.rs`, feature `lock-stats`). With steps forced to drop: `readers_do_not_wait_when_steps_drop_rows`, `release_with_drops_does_not_block_readers_rows` and `forced_drops_keep_content_and_the_cap`.
2. No contention between files: `many_files_no_cross_file_wait` and `other_files_do_not_wait_rows`.
3. No global lock held during I/O, a hash or a wait: the parked tests in `lock_tail_tests.rs` (chunk store, including `put_and_read_pass_a_held_phase_two_sync` and `put_and_read_pass_a_held_record_write`), `the_writer_lock_is_free_during_a_record_write` (WAL), and the metadata tests. The serialize-off harness with `lock-stats` (debug build, 4 threads, 3 runs of 3 modes on each system) gives these hold times in microseconds, the range over all modes and runs:

| Site | macOS before: p99 / max | macOS after: p99 / max | Linux before: p99 / max | Linux after: p99 / max |
|---|---|---|---|---|
| store | 22-158 / 21000-91000 | 6-26 / 61-262 | 29-67 / 3400-144000 | 11-48 / 88-1967 |
| wal_writer | 20600-36400 / 36600-43400 | 17-21 / 38-1776 | 264-1491 / 9400-49000 | 22-26 / 101-1019 |
| wal_queue | 2-12 / 8-32 | 2-10 / 14-35 | 2-3 / 30-117 | 2-3 / 44-216 |
| readers | 2-11 / 4-77 | 2-15 / 5-61 | 3-23 / 16-380 | 3-25 / 30-242 |
| dir_live | 7-14 / 11-40 | 7-21 / 10-51 | 8-26 / 11-190 | 9-36 / 10-874 |
| inode_live | 7-8 / 15-30 | 8-14 / 12-99 | 11-15 / 14-812 | 12-17 / 15-164 |

   "Before" is the 19-11 run on macOS (the store max is over 5 runs) and the 3 spike 19g runs on Linux. "After" is 3 runs on each system at the end of Phase 19, at a host load average of 15-17. The bar was p99 at most 500 us on both systems, and max at most 5 ms on macOS and 2 ms on Linux. No row was above it, so no outlier needed a cause. Linux ran in OrbStack Docker; its virtual disk does not flush a drive cache on fsync. The cell and stripe locks are not global, but they had longer holds (see "Still open").
4. Lock rank: the rank tests in `chunk_store::sync` and `strict_gate_catches_a_request_without_the_gate`.
5. Replay equals apply: `crash_replay_matches_apply` (harness), `reverse_order_creates_replay`, `parallel_namespace_ops_replay` and `parallel_overlapping_writes_replay_to_the_same_content`.
6. POSIX.1 2.9.7 with several handles: `multi_handle_posix.rs` (including `appends_never_overlap`) and the harness multi-handle mode (read-only, read-write and `O_APPEND` handles on one inode, with serialize off).

Still open after Phase 19:
- Phase 20: the background rebase flusher, writers throttled before they take a lock, and the cross-file dirty budget (today a writer over the budget steps only its own file, so nothing lowers the dirty bytes of other files; LOCK-02, LOCK-06).
- Phase 21: the worker pool (N threads by mount option).
- **I/O that still runs under a lock.** Off the read and write paths: the GC copy loop and its syncs, `gc_begin`, `close`, the offline log `rotate`, destroy, and the commit cut (a locked cell flush under the exclusive gate, when no request runs). On a request path: the size change of a closed file does its splice under its own transient cell (reads and getattr do not lock that cell, but an open or a second size change of the inode waits for it). An unlink of the last link takes only the manifest key from memory and deletes the inode under the namespace stripes; it reads the manifest and drops the refcounts after the stripes are released, inside the shared gate (review fix IN-01). In the chunk store, a `put` looks up the index with the store lock held, and after an index change that lookup can read a run page from disk; a pack whose writer or sync handle cannot be cloned keeps the locked append or sync.
- **Long stripe and cell holds.**
  - `stripe`: on macOS, the many-files mode had a hold of about 20 ms in each of the 3 runs (21.5, 20.2 and 20.2 ms); on Linux the max was 1.1-2.6 ms. A probe named `unlink_logged`: the first store read of the manifest in `reclaim_inode` takes about 20 ms, with the parent and child stripes held. No global lock is held then (store hold max 122 us, `io` hold max 0.5 ms in that mode). Why the read takes 20 ms is not measured. The review fix IN-01 moved that read after the stripes are released; at the fix commit the macOS stripe hold max was 298.5 us.
  - `file_cell`: on macOS the one-file mode had holds of 3.6-7.9 ms in every run; on Linux one hold of 13 ms. The probe caught only holds of 3.2-4.8 ms, released in `write_in_cell`; their span is not measured. The parked tests show that no writer I/O is in a cell hold.
- Known windows, older than Phase 19: a size change of a closed file has no stripe, so it can race an unlink or a setattr of other fields of the same inode; there is no ancestor check for a cross-directory rename. (mkdir now takes the new number first and locks the stripes of the parent and of the new directory, so its window is closed.)

## Phase 20 result

Phase 20 (issue #21) did step 5 of the migration: the dirty budget is safe with N writers (LOCK-02, LOCK-06).

The design in short:
- **Throttle.** A write calls `throttle_write` before it takes the commit gate, a stripe or a cell. Below the budget it goes on. Over the budget it waits with no lock held, in 50 ms slices, until the flusher frees memory, for at most 5 s (200 ms since the change after Phase 20). Then it writes and pays with steps of its own file (`pay_budget`). A write never fails here.
- **Flusher.** One thread per mount (`slicefs-flusher`). It takes the commit gate shared for one step of one file (`FLUSH_STEP`, 4 MiB), and never across a wait. It skips a cell that has a step in flight; it does not wait for it. It never takes a cell out of the cell map.
- **Triggers.** A file with no write for the idle time (5 s). Dirty data that reached the maximum age (30 s). The buffer is full: the total is at half the budget or more, or one quiet file is at its own limit; then the largest quiet file goes first.
- **The hot rule.** A file with a write in the last 300 ms is hot. Below the budget the flusher never steps a hot file, because every write to it would drop the step. Its own writer keeps it under its limit. At the budget a hot file can be stepped, because its writers wait in the throttle.
- **Stop.** At unmount the flusher stops and is joined before the destroy work takes the gate. The stop, and a flusher panic, wake every throttled writer at once.

Mount options: `--dirty-budget` (default 64M), `--dirty-limit` (default 32M), `--dirty-idle` (default 5 s) and `--dirty-expire` (default 30 s). See the README.

Tests per ROADMAP Phase 20 criterion:
1. No take-out: `only_four_functions_remove_a_cell`, `the_budget_step_is_own_file_only` and `the_flusher_takes_no_cell_out` (`no_take_out.rs`). Only four functions remove a cell from the cell map, and the writer-side step (`pay_budget`) steps only its own file. The rank rule: the flusher thread runs with the gate check of the rank checker (a cell only inside a shared gate hold), and a thread never holds two cells (`two_cells_panic_even_in_key_order`).
2. Flusher and throttle (`flusher_tests.rs`): `a_writer_over_the_budget_waits_for_the_flusher`, `the_flusher_lowers_the_total`, `a_throttled_writer_writes_after_the_limit`, `a_write_past_half_the_budget_kicks_the_flusher`, `stop_wakes_throttled_writers`, `a_flusher_panic_wakes_throttled_writers`, `a_throttle_with_a_lock_held_panics`, `an_idle_file_is_flushed`, `an_expired_file_is_flushed`, `a_hot_file_is_left_to_its_own_writer`, `at_the_budget_a_hot_file_is_stepped` and `dirty_total_stays_bounded_with_parallel_writers`. The last one shows that with 4 parallel writers the largest sampled total (4.07 MiB) stays under the budget plus 4 writes plus one rebase step (5.13 MiB).
3. No deadlock: `all_writers_throttled_while_a_commit_waits_flusher_paused` and `all_writers_throttled_while_a_commit_waits_flusher_in_a_step`. Also `shutdown_with_a_parked_flusher_step_and_a_queued_commit_ends`.
4. Peak RSS under one fixed cap: the S7 run (in process, no mount) in OrbStack Docker, kernel `Linux 7.2.12-orbstack x86_64`, 32 CPUs, rustc 1.100.0, release. 4 writers, 1 GiB file each, 128 MiB of the bigwrite mix each, budget 64 MiB.

| Value | Result |
|---|---|
| P_old: Phase 19 code, writes serialized (3 runs, VmHWM max) | 260 MiB |
| P0: Phase 19 code, writes parallel (3 runs, VmHWM max) | 267 MiB |
| Cap rule (set before the runs) | P0 max + one flusher step (8) + a reserve (max of 16, 10 % of P0, 2 x spread = 40), rounded up to a multiple of 8 |
| CAP_MIB | 320 MiB |
| P1: Phase 20 code with the flusher, writes parallel (3 runs, VmHWM) | 257, 278, 287 MiB; max 287 MiB, margin 33 MiB |
| Flusher steps (published) per run | 128, 120, 118 (all published) |
| Dropped steps | 0 |
| Throttle waits per run / timeouts | 428, 437, 411 / 0 |
| Slowest write (no throttle wait in it) | 321.1, 2485.1, 1238.4 ms |
| Largest throttle wait | 1015, 525, 282 ms |
| Largest commit gate wait | 692.6, 12.4, 105.8 ms |
| Largest dirty total | 65 MiB |

The delay of one write is its throttle wait plus its write time. These are two separate maxima, so the worst write delay is at most their sum. P1 is 20 MiB above P0, with the same dirty total. A part of the extra memory can be the malloc arenas of the flusher thread (a P0 run with `MALLOC_ARENA_MAX=2` gave 230 MiB); P1 did not measure it.

The S7 numbers above were measured in plan 20-07, before the empty-pass fix. In those runs, 98.6 % to 99.7 % of the flusher passes found no candidate (up to about 900 passes per second): each write at half the budget or more woke the flusher, also when the only dirty file was hot. After the runs, a fix stopped this: after a pass that found no candidate with the total at half the budget or more, a write below the budget does not wake the flusher. The timed pass (every 100 ms) and a write at the full budget still do. A pass below half the budget does not stop the next kick (`a_write_past_half_the_budget_kicks_the_flusher`). The test `a_hot_file_above_half_the_budget_does_not_kick_after_an_empty_pass` shows 1 pass for 200 writes (200 before the fix). The S7 runs were not done again after the fix.

Limits:
- The daemon still has one dispatch thread until Phase 21. In a mount, a throttled write also stops reads and all other requests for up to the wait limit (5 s in Phase 20; 200 ms after it). The S7 run in process is the evidence for parallel writers.
- A stream can hold out-of-order writes up to a fixed 64 MiB window, whatever `--dirty-limit` is.

Still open after Phase 20: see `.planning/phases/20-dirty-budget-and-rebase-flusher/deferred-items.md` (and the Phase 19 list that it points to).

## Phase 21 result

Phase 21 (issue #22) did step 6 of the migration: requests run on a pool of worker threads (POOL-01 to POOL-05).

The design in short:
- **Reader and pool.** One fuser reader thread reads the requests. With N of 2 or more, it gives each request to a SliceFS pool of N threads (`slicefs-worker-<k>`). The reader copies the arguments and the write data into the job. With N = 1 there is no pool: each request runs on the reader thread, as before.
- **Queue.** The queue has max(2N, 4) slots and holds at most 8 MiB of payload. One larger job goes in when the queue is empty. The reader waits for space with no lock held, in 1 s slices.
- **Gate and reply.** A job takes the commit gate only inside `serve`. The worker sends the reply after `serve` returns, so no reply goes out with a lock held. A debug build asserts this at each reply.
- **Unmount.** `destroy` first closes the queue, runs every queued job and joins the workers. Then it calls the Phase 20 `shutdown` (flusher stop, then the gate).
- **Wait limits.** `submit` and the drain wait at most 200 s (`SUBMIT_WAIT`, `DRAIN_WAIT`). Then the daemon aborts, and the write log replays at the next mount. The 200 s cover the longest request: a 60 s busy-store retry, a 120 s step wait, the 10 s store wait and the 200 ms throttle wait.
- **Panic.** A panic hook aborts the daemon when the panic is on a request thread (`fuser-*` or `slicefs-worker-*`). The locks of the half-done request stay held, no reply goes out, and `destroy` does not run, so nothing commits the half-done state. The next mount replays the write log. A panic on another thread (for example the flusher) does not abort.
- **Busy store (#170).** A slow pack sync can keep the chunk store busy for more than its 10 s wait. A write that meets a busy store now pauses with no lock held (the commit gate is released between the tries) and tries again for up to 60 s. Then it gives EIO. A flusher step that meets a busy store is not an error: it counts as `flusher_busy` and backs off. With this fix, `SUBMIT_WAIT` and `DRAIN_WAIT` went from 150 s to 200 s.
- **Memory (the [Step limit] item).** No step limit was built. The growth for each S7 writer was glibc arena retention, not live data. On Linux glibc the daemon calls `mallopt(M_MMAP_THRESHOLD, 128 KiB)` at start. The mount log prints `Allocator: mmap threshold 131072 bytes (fixed)`. The unmount prints a `steps:` line (save steps, freed bytes).
- **Trace lines.** The per-request ENTRY and EXIT lines are off by default. `SLICEFS_FUSE_TRACE=1` turns them on (each value other than `0` does). Stderr has one global lock, so with the lines on, the workers wait for each other.
- **Counters.** The unmount prints a `pool:` line: jobs, jobs for each worker, the largest queue depth, the largest job weight and the longest queue wait.

The option: `--workers N`. The default on Linux is half the CPUs, at least 1 and at most 8. The default on macOS is 1. An explicit value above 8 is allowed, and 0 is refused. The cap of 8 has two reasons. CAP_MIB 320 was measured with 4 writers, and each busy worker adds memory (its own save steps and an allocator arena). The bench below shows no gain from 8 to 16 workers.

Tests per ROADMAP Phase 21 criterion:
1. One reader, the pool, the option, inline with 1: `pool_tests` (`jobs_run_in_parallel`, `jobs_run_on_named_workers`, `submit_blocks_at_the_slot_bound`, `submit_blocks_at_the_byte_bound`, `an_oversize_job_passes_when_the_queue_is_empty`, `stats_count_the_queue_and_the_jobs_per_worker`, `a_submit_with_a_lock_held_panics`). The option: `workers_flag_parses`, `workers_zero_is_refused`, `workers_absent_is_none`, `workers_above_eight_is_accepted`, `workers_for_cpus_halves_and_clamps`, `default_workers_is_one_on_macos` and `default_workers_is_in_range` (Linux). Inline: `run_without_a_pool_runs_on_the_caller`. Docker smoke mounts with 4 and 1 workers.
2. Drain before `destroy`, reply after the locks: `drain_runs_every_queued_job`, `drain_limit_reports_a_stuck_worker`, `destroy_drains_before_shutdown`, `the_gate_is_taken_only_in_serve_and_init`, `every_request_callback_uses_serve` and `serve_panics_when_a_lock_is_held_at_the_reply`. The SIGTERM, SIGINT and SIGHUP live checks pass with 4 workers.
3. Panic aborts, the remount replays: `pool_panic_tests` (`a_worker_panic_aborts_and_the_remount_replays`, `a_reader_thread_panic_aborts_and_the_remount_replays`, `a_full_queue_that_stays_full_aborts`, `a_panic_on_another_thread_does_not_abort`). The live check `panic-replay.sh` passes with 4 and with 1 worker.
4. Tests with 4 workers: in Docker, the workspace tests, the FUSE smoke, the live checks and pjdfstest (8792 tests, PASS) with `--workers 4`. CI for the last code commit: the ci.yml Linux job with `Workers: 4 (set)` checked in each of its three steps, the ci.yml macOS job, and macos-mounts.yml on FSKit with `Workers: 1 (default)`. The busy-store fix: `store_busy_tests` (`a_write_waits_for_a_busy_store_and_succeeds`, `an_append_that_meets_a_busy_store_is_in_the_file_once`, `a_store_that_stays_busy_ends_the_write_with_eio_at_the_limit`, `a_flusher_step_that_meets_a_busy_store_is_not_an_error_and_goes_again`).
5. Scale with the worker count: the bench below.

**S7 with the allocator setting (plan 21-06).** S7 in process, Docker Linux, 32 CPUs, release, 1 GiB file and 128 MiB of the mix for each writer, budget 64 MiB, flusher on. 19 runs. Pm(N) is the largest VmHWM at N writers. "Before" is the code before plan 21-05, with no allocator setting.

| Writers | 1 | 2 | 4 | 8 | 16 |
|---|---|---|---|---|---|
| Runs | 3 | 3 | 7 | 3 | 3 |
| Pm before (MiB) | 97 | 216 | 340 | 441 | 730 |
| Pm after (MiB) | 106 | 173 | 227 | 290 | 426 |
| Spread after (MiB) | 5 | 5 | 28 | 19 | 14 |
| CAP(N) (MiB) | | | 296 | 336 | 480 |

- CAP(N) = Pm(N) + 8 + the largest of 16, Pm(N) / 10 and 2 x Spread(N), rounded up to a multiple of 8. The values are recorded only. CAP_MIB stays 320.
- **R1=pass (hard bar).** All 16 runs at 1, 2, 4 and 8 writers stay at or under 320 MiB (largest 290 MiB at 8 writers), with 0 mismatches and no EIO. So the 320 MiB bar holds at 1, 2, 4 and 8 writers.
- **R2=miss (report to the owner).** 16 writers give 412 to 426 MiB. 16 is above the default cap of 8, so this does not block.
- **R3=miss (report to the owner).** Pm(16) - Pm(4) = 199 MiB (rule: at most 64). Pm(8) - Pm(4) = 63 MiB passes by 1 MiB.
- The harness term (the version model, the write buffer and the check read of S7) is about 3 MiB for each writer. It does not explain the rise. Above 8 writers, most of the rise is live heap (about 10 MiB for each writer) and mapped files that follow the stored bytes, not malloc retention.
- The [Step limit] item is closed: R1=pass, and the bench gives mem_bar=pass.

**Linux bench (plan 21-08).** Docker Linux (OrbStack), 32 CPUs, release, mount form. 16 clients on different files at each worker count. Write: 16 parallel `dd` of 64 MiB with fsync. Read: the same files from a cold cache. Random write: 64 writes of 1 MiB for each file, then fsync. Metadata: each client creates, stats, reads and unlinks 250 files of 4 KiB. Medians of 3 runs with the default trace setting (off). VmHWM is the largest daemon value of the write mount and the read mount in the 3 runs. All 22 runs have 0 content mismatches.

| Workers | Write MB/s | Read MB/s | Random write MB/s | Create files/s | Stat ops/s | Read ops/s | Unlink ops/s | VmHWM max (MiB) |
|---|---|---|---|---|---|---|---|---|
| 1 | 134.9 | 345.8 | 115.2 | 5000 | 57971 | 11527 | 27972 | 124 |
| 2 | 178.8 | 550.8 | 128.3 | 5839 | 61539 | 12158 | 31496 | 140 |
| 4 | 169.5 | 931.8 | 145.6 | 6579 | 60606 | 16260 | 41667 | 152 |
| 8 | 165.6 | 1304.5 | 151.4 | 6601 | 58824 | 16327 | 40816 | 178 |
| 16 | 149.1 | 1190.7 | 131.4 | 5571 | 55556 | 16000 | 42553 | 223 |

- **Verdict.** The bar: at 4 workers the median write or read MB/s is at least 1.5 times the value at 1 worker. `ratio_write_4_1=1.26`, `ratio_read_4_1=2.69`, so `verdict=PASS`. The pass comes from the reads.
- **Writes do not scale.** They rise from 1 to 2 workers and then stay flat or fall. In the lock-stats runs, the `store` lock has the largest wait in the write phase (p99 90.9 us at 4 workers and 249.0 us at 8, max 13.5 ms). All writers append to the store under this one lock (the single pack appender). This is the likely cause, but the bench does not rule out fsync or disk limits.
- **Memory.** The largest VmHWM at 1, 2, 4 and 8 workers is 178 MiB, under the 320 MiB bar: `mem_bar=pass`. At 16 workers it is 223 MiB (`mem16_max_mib=223`). VmHWM grows by about 6.6 MiB for each worker.
- **Metadata.** The create, read and unlink rates rise to 4 workers and then stay flat. At 8 workers the `store`, `inode_map`, `wal_writer` and `readers` locks have a wait p99 of about 100 to 160 us. The stat rate is partly a kernel-cache rate (about 2200 of 4000 stats reach the daemon).
- **Trace lines on** (`SLICEFS_FUSE_TRACE=1`, one run pair after each round): no cost for writes, 16 % (1 worker) and 34 % (4 workers) of the read rate, and 51 % to 77 % of the metadata rates. With the lines on, the create rate is the same at 1 and 4 workers (2430 and 2477 files/s). This is the reason for the default off.
- **Arena run** (16 workers, `MALLOC_ARENA_MAX=2`, one run): 9 to 13 MiB less VmHWM with a similar rate. Recorded only.
- No follow-up issue is necessary for the bar. The write plateau is a report to the owner.

**macOS bench (FSKit CI, 3 CPUs, load about 50).** 3 runs at each worker count, 0 mismatches.

| Workers | Write MB/s (runs) | Write median | Read MB/s (runs) | Read median | Max RSS (MiB) |
|---|---|---|---|---|---|
| 1 | 25.2, 23.4, 60.8 | 25.2 | 330.7, 171.1, 265.6 | 265.6 | 63 |
| 2 | 77.7, 13.3, 37.3 | 37.3 | 116.5, 206.8, 165.7 | 165.7 | 69 |

The runner was loaded and the numbers vary a lot. They do not show a gain with 2 workers on FSKit, so the macOS default stays 1. A rise needs a measured FSKit gain.

**Rule for a later rise of the default cap of 8.** Raise it only when the median at 16 workers is at least 1.1 times the median at 8 in some phase, and the largest VmHWM at 16 workers is under CAP(16) = 480 MiB. Now the ratios at 16 against 8 are 0.84 to 1.04 in all phases, and VmHWM is 223 MiB. The rule is not met, so the cap stays 8.

Limits:
- With 2 or more workers, the Phase 19 windows can now occur: a size change of a closed file can race an unlink or a setattr of other fields of the same inode, and a cross-directory rename has no ancestor check. On Linux, pjdfstest and the live checks passed with 4 workers. The risk on FUSE-T with more than 1 worker is not measured.
- With 2 or more workers on FUSE-T, WRITEs of one file can arrive in parallel (research A6). This was not measured.
- With 2 or more workers, a throttled write no longer stops the other requests. The Phase 20 limit "one dispatch thread" now holds only with 1 worker (the macOS default).
- fsync, release, replay and commit still give EIO for a busy store. Only the write path tries again.

Still open after Phase 21: see `.planning/phases/21-worker-pool/deferred-items.md` (and the Phase 19 and Phase 20 lists that it points to).

## Conclusion

Adding threads does not solve SliceFS's latency problem by itself. Two other changes do most of the work. The first takes the inline 8 MiB pack fsync out of the store lock, so a thread-pool design does not hit that tail on every worker. The second replaces the take-out-of-map ownership trick with a real per-inode lock. That trick is the hidden reason SliceFS needs one thread, and replacing it also makes the log order, the dirty budget and the rebase-off-thread fix (#108 option 1) simple to state. The kernel file systems point to the same design: per-inode locks taken in a fixed rank, commits that never wait while a lock is held, and writers that pay by sleeping instead of doing other files' work.

The open question is macOS. Every source about FSKit and FUSE-T is silent or unofficial on concurrency. So the macOS value of a worker pool is an empirical result that the short spike S1 can settle, and the design is planned so that it is still worth building if the answer is "no overlap". Phase 18 settled the overlap part: FUSE-T 1.2.9 on macOS 26.7.2 handed the probe 2 requests at once and accepted replies out of order (see the S1 result). The throughput gain on macOS is still open. Steps 1-5 make the daemon correct under any concurrency and lower its tail latency even at N=1. That makes them low-risk work no matter what the spikes show.
