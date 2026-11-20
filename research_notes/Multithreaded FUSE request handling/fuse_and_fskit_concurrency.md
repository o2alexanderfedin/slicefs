# FUSE and FSKit concurrency as seen by a user-space file system server

Scope: what reaches a FUSE server in parallel on Linux (kernel fs/fuse + libfuse) and on macOS (FUSE-T FSKit backend, macFUSE, Apple FSKit), and what ordering the kernel/host side guarantees. Researched 2026-11-19. Kernel source read from torvalds/linux `master` on that date (Documentation labels the tree 7.4.0-rc3). SliceFS context: vendored fuser 0.18.0 (`vendor/fuser`), macOS mounts use FUSE-T with `backend=fskit` only.

## Linux FUSE kernel: what it serializes and what can arrive concurrently for one inode

### Takeaway
The kernel gives no general request-ordering guarantee. A server sees per-inode serialization only where the VFS/fuse code holds a lock while it sends the request: buffered writes (write-through mode), non-parallel direct writes, fsync and truncate hold the exclusive inode lock (`i_rwsem`), and lookups/readdir in one directory are serialized by a per-inode mutex unless the server enables `FUSE_PARALLEL_DIROPS`. Reads, readahead, writeback-cache WRITEs, RELEASE and requests for different inodes can all arrive concurrently, so the server must lock its own per-inode state.

### Cited Findings
- The kernel FUSE doc states only one ordering rule: "INTERRUPT requests take precedence over other requests." Requests sit on `fc->pending`, move to `fc->processing` when the daemon reads them, and replies are matched there by `unique`. Nothing about multithreaded daemons or per-inode serialization is documented. — [Documentation/filesystems/fuse/fuse.rst](https://raw.githubusercontent.com/torvalds/linux/master/Documentation/filesystems/fuse/fuse.rst)
- `FUSE_PARALLEL_DIROPS` (protocol 7.25, bit 18): "allow parallel lookups and readdir". Without it, `fuse_lock_inode()` takes `fuse_inode->mutex` of the directory (`if (!get_fuse_conn(inode)->parallel_dirops) mutex_lock(...)`); `dir.c` calls it around LOOKUP. So without the flag the server never sees two LOOKUP/READDIR for the same directory at once. — [fs/fuse/inode.c](https://github.com/torvalds/linux/blob/master/fs/fuse/inode.c), [include/uapi/linux/fuse.h](https://github.com/torvalds/linux/blob/master/include/uapi/linux/fuse.h), [fs/fuse/dir.c](https://github.com/torvalds/linux/blob/master/fs/fuse/dir.c)
- Buffered write path `fuse_cache_write_iter()` takes `inode_lock(inode)` (exclusive `i_rwsem`). Without writeback cache, buffered writes are write-through, so WRITE requests for one inode from `write(2)` are sent one at a time. — [fs/fuse/file.c](https://github.com/torvalds/linux/blob/master/fs/fuse/file.c)
- Direct I/O writes: `fuse_dio_wr_exclusive_lock()` returns "exclusive" unless the open reply set `FOPEN_PARALLEL_DIRECT_WRITES` ("Allow concurrent direct writes on the same inode", protocol 7.36). Even with the flag, the kernel takes the exclusive lock for `O_APPEND`, when the inode is in page-cache I/O mode, and for writes past EOF ("Parallel dio beyond EOF is not supported, at least for now"); otherwise `inode_lock_shared`. — [fs/fuse/file.c](https://github.com/torvalds/linux/blob/master/fs/fuse/file.c), [fuse.h](https://github.com/torvalds/linux/blob/master/include/uapi/linux/fuse.h)
- Writeback cache (`FUSE_WRITEBACK_CACHE`, bit 16): dirty pages are sent later by writepages as background requests. A per-inode `writectr` counts in-flight WRITEs; `fuse_flush_writepages()` sends "all queued writepage requests" while `writectr >= 0`. So many WRITE requests for one inode can be in flight together, from a kernel thread, not the writer's context. — [fs/fuse/file.c](https://github.com/torvalds/linux/blob/master/fs/fuse/file.c)
- `fuse_set_nowrite()` (must be called with the inode locked, `BUG_ON(!inode_is_locked(inode))`) adds a negative bias to `writectr` and waits until all pending writepages are done. It is used by `fuse_sync_writes()` (fsync, some flush paths) and by setattr when `is_truncate` (and for writeback-cache attribute changes). So with writeback cache, a truncating SETATTR or an FSYNC is not sent while writeback WRITEs for that inode are outstanding, and no new writeback WRITE starts until it finishes. — [fs/fuse/dir.c](https://github.com/torvalds/linux/blob/master/fs/fuse/dir.c), [fs/fuse/file.c](https://github.com/torvalds/linux/blob/master/fs/fuse/file.c)
- FSYNC: `fuse_fsync()` takes `inode_lock`, calls `file_write_and_wait_range()` then `fuse_sync_writes()`, then sends FSYNC. So FSYNC arrives after all earlier WRITEs of that inode were answered. — [fs/fuse/file.c](https://github.com/torvalds/linux/blob/master/fs/fuse/file.c)
- FLUSH (on every `close(2)`): `fuse_flush()` first runs `write_inode_now(inode, 1)` (writes dirty pages back), then sends FLUSH synchronously. It is skipped for `FOPEN_NOFLUSH` unless writeback cache is on. FLUSH does not take the inode lock, so it can run concurrently with WRITE/READ from other fds. — [fs/fuse/file.c](https://github.com/torvalds/linux/blob/master/fs/fuse/file.c)
- RELEASE: `fuse_file_put(ff, sync)` sends RELEASE via `fuse_simple_background()` when `sync` is false; `fuse_release()` passes `sync = fc->auto_submounts`. So RELEASE is normally asynchronous: `close()` returns before the server sees RELEASE, and a following OPEN of the same file can reach the server before the previous RELEASE. — [fs/fuse/file.c](https://github.com/torvalds/linux/blob/master/fs/fuse/file.c)
- Readahead: `fuse_readahead()` sends READs with `fc->async_read` (negotiated by `FUSE_ASYNC_READ`, bit 0), i.e. as background requests, so several READs of one file can be in flight at once. — [fs/fuse/file.c](https://github.com/torvalds/linux/blob/master/fs/fuse/file.c), [fuse.h](https://github.com/torvalds/linux/blob/master/include/uapi/linux/fuse.h)
- `max_background`: when the number of background requests reaches it, further background requests block until others finish. `congestion_threshold`: above it the kernel skips async readahead and suspends non-synchronous writeback. Default threshold is `FUSE_DEFAULT_MAX_BACKGROUND * 3 / 4`; a module parameter `max_user_bgreq` caps what unprivileged servers may set. — [fuse.rst](https://raw.githubusercontent.com/torvalds/linux/master/Documentation/filesystems/fuse/fuse.rst), [fs/fuse/inode.c](https://github.com/torvalds/linux/blob/master/fs/fuse/inode.c)
- Passthrough (`FUSE_PASSTHROUGH`, bit 37) cannot be combined with `FUSE_WRITEBACK_CACHE`: inode.c comment "don't allow the combination of FUSE_PASSTHROUGH and FUSE_WRITEBACK_CACHE, current design doesn't handle…". — [fs/fuse/inode.c](https://github.com/torvalds/linux/blob/master/fs/fuse/inode.c)

### Inferences
- For one inode, a multithreaded server must expect at the same time: READ + READ, READ + WRITE (reads take no inode lock in the cached path, and readahead is async), many writeback WRITEs (with writeback cache), FLUSH + WRITE from another fd, GETATTR + anything, and an OPEN overtaking an older RELEASE. It will not see two write-through WRITEs from `write(2)` on one inode at once, nor FSYNC/truncate overlapping with writeback WRITEs of that inode.
- Different inodes have no kernel serialization at all; RENAME/UNLINK/CREATE in one directory are serialized by the VFS directory `i_rwsem` (standard VFS behaviour, not FUSE-specific; not re-verified here), but LOOKUP/READDIR are not once `FUSE_PARALLEL_DIROPS` is set.
- Because the kernel already serializes write-through WRITEs per inode, a per-open-file (or per-inode) mutex in the server is enough for write state; it will be uncontended in the common case. Metadata maps (inode table, open-file table) need their own short locks.

### Gaps
- `FUSE_DEFAULT_MAX_BACKGROUND` is defined in a header I could not fetch from master (not in `fuse_i.h`); the long-standing value is 12 (recalled, not re-verified for 2026).
- I did not trace the cached read path (`fuse_file_read_iter`) line by line; "reads take no inode lock" is from the generic page-cache read path and not re-verified in the 2026 source.

## libfuse multithreaded loop and how real servers lock per-inode state

### Takeaway
libfuse's `fuse_session_loop_mt()` runs a worker pool (bounded by `max_threads` since 3.12; `max_idle_threads` is deprecated), each worker reads one request and handles it; `clone_fd` gives each worker its own `/dev/fuse` fd. libfuse 3.18.0 (2025-12-18) added fuse-over-io-uring. libfuse itself does no per-inode locking in the low-level API — servers do it.

### Cited Findings
- libfuse 3.12.0 (2022-09-08): "`max_idle_threads` has been deprecated in favor of the new `max_threads`… Using max_threads == 1 and calling fuse_session_loop_mt() will run single threaded similar to fuse_session_loop()." `struct fuse_loop_config` became private, built with `fuse_loop_cfg_create()`/`fuse_loop_cfg_set_*()`; reference user is `example/passthrough_hp.cc`. — [libfuse ChangeLog](https://github.com/libfuse/libfuse/blob/master/ChangeLog.rst)
- libfuse 3.17 (2025-01-01) / 3.17.1 (2025-03-24): passthrough read/write support, `fuse_passthrough_open()`/`fuse_passthrough_close()`, see `passthrough_hp.cc`. — [libfuse ChangeLog](https://github.com/libfuse/libfuse/blob/master/ChangeLog.rst)
- libfuse 3.18.0 (2025-12-18): "fuse-over-io-uring communication", statx, request timeouts, thread naming. — [libfuse ChangeLog](https://github.com/libfuse/libfuse/blob/master/ChangeLog.rst)
- libfuse unreleased (master, 2026): `-o io_uring_bufpool` (one contiguous buffer region per queue); `fuse_session_loop()` with FUSE_USE_VERSION ≥ 319 now calls the filesystem from a worker thread but "Requests are still handled one at a time"; new `fuse_file_info::stream` so "a read and a write on the same fd can then run at the same time". — [libfuse ChangeLog](https://github.com/libfuse/libfuse/blob/master/ChangeLog.rst)

### Inferences
- The libfuse model (N workers, each blocks in `read(/dev/fuse)`, handles, replies) is the same as fuser's `n_threads` model; replies can be sent out of order because the kernel matches by `unique`.

### Gaps
- I did not fetch the source of sshfs, gcsfuse, JuiceFS, rclone mount, s3fs, ntfs-3g or mergerfs within the call budget, so their per-inode locking is not cited here. (From general knowledge, unverified: ntfs-3g runs single-threaded by default; go-fuse based servers such as gcsfuse/rclone handle each request on its own goroutine and use per-inode mutexes. Treat as leads, not facts.)

## Rust crates: fuser, fuse3, polyfuse

### Takeaway
fuser 0.17.0 (2026-02-14) added multiple event loops per session via `Config::n_threads` and `FUSE_DEV_IOC_CLONE` (clone_fd); 0.18.0 (2026-07-22) is current. The `Filesystem` trait is `Send + Sync` with `&self` methods, shared by `Arc` across loops. More than one thread is refused outside Linux, so on macOS concurrency must come from handing `Reply*` objects to worker threads.

### Cited Findings
- fuser CHANGELOG: 0.18.0 – 2026-07-22; 0.17.0 – 2026-02-14 ("Add support for multiple event loops per session, which can be enabled via `Config::n_threads`"; "Add `FUSE_DEV_IOC_CLONE` support"; "Remove `osxfuse` support and improve `macfuse` compatibility"); 0.18.0 renamed `spawn_mount2()` to `spawn_mount()` and made `Session::run()` public; unreleased: dropping a `BackgroundSession` unmounts and waits. — [fuser CHANGELOG](https://github.com/cberner/fuser/blob/master/CHANGELOG.md)
- Vendored fuser 0.18.0 `Session::run()`: "This read-dispatch-loop is non-concurrent to prevent having multiple buffers… but the filesystem methods may run concurrent by spawning threads." It creates `n_threads` channels (cloned `Arc` fd, or `clone_fd()` via ioctl if `config.clone_fd`), one thread `fuser-{i}` per channel, all sharing `Arc<Filesystem>`. Non-Linux with `n_threads != 1` returns "n_threads != 1 is only supported on Linux" (TODO to test macOS/FreeBSD); clone_fd is Linux-only. — `vendor/fuser/src/session.rs` lines 240–300 (local, upstream commit 9c957f74)
- `pub trait Filesystem: Send + Sync + 'static` with `&self` methods; `trait Reply: Send + 'static`, so a reply can be moved to another thread and answered later. — `vendor/fuser/src/lib.rs:400`, `vendor/fuser/src/reply.rs:104` (local)
- SliceFS patch 2 to fuser: on FUSE-T the fd is a `SOCK_STREAM` socket, `receive()` reads the 40-byte header then exactly the rest; `send()` "takes a lock and loops until the whole reply is written". — `vendor/fuser/PATCHES.md` (local)
- SliceFS today relies on one dispatch thread: `keep_budget()` comment says state leaves `open_files` during a rebase step and "Nothing can see the state gone: the mount runs one fuser dispatch thread… A mount with more dispatch threads would need the state kept in the map during the step." — `crates/slicefs-cli/src/filesystem.rs:1215-1231` (local)

### Inferences
- On Linux: set `Config::n_threads` (and `clone_fd = true` to avoid all threads contending on one fd's wait queue). Each thread does read→handle→reply, so a slow handler blocks only its own thread.
- On macOS/FUSE-T: several threads reading the same stream socket would break the framed read (header and body could be split across threads), so keep one reader and move work + `Reply` to a worker pool; the patched `send()` already serializes writes with a lock. Whether FUSE-T accepts out-of-order replies is not documented (see FSKit section gaps) — needs a spike.
- Code that removes per-file state from a shared map while working on it (as `keep_budget` does) must change first; that is a known unsafe point for >1 thread.

### Gaps
- fuse3 (async, tokio/async-std) and polyfuse (async, request-per-task) concurrency details were not fetched within budget; no current version/date cited.

## FUSE passthrough and FUSE over io_uring

### Takeaway
Passthrough (Linux 6.9, merged 2024-03-14) lets READ/WRITE (and mmap) of an opened file go straight to a backing file, removing the server from the data path; it needs a privileged server and excludes writeback cache. FUSE-over-io_uring (Linux 6.14, pull 2025-01-29) replaces `/dev/fuse` read/write with per-CPU io_uring queues; libfuse supports it from 3.18.0; later kernels (7.3, 2026) add buffer pools and zero-copy. Neither adds per-inode ordering guarantees.

### Cited Findings
- 6.9 pull request: "Add passthrough mode for regular file I/O"; reads and writes, including through memory maps, go to a backing file "without incurring the overhead of roundtrips to userspace"; "Initially this is only allowed to privileged servers". Merged 2024-03-14. — [GIT PULL fuse update for 6.9](https://lore.kernel.org/all/171052321821.31681.13880051934148095319.pr-tracker-bot@kernel.org/T/)
- Passthrough excludes writeback cache in current kernel (`inode.c` check). — [fs/fuse/inode.c](https://github.com/torvalds/linux/blob/master/fs/fuse/inode.c)
- 6.14 pull (Szeredi, author Bernd Schubert, 2025-01-29): io_uring transport; goals fewer syscalls and CPU/NUMA affinity; "every CPU core has its own fuse-io-uring queue"; sync requests handled on the same core, large async ones on core+1. An early RFC: 1 thread creates 3944/s via /dev/fuse vs 10121/s via io_uring (2.57x; early-version number). — [GIT PULL fuse update for 6.14](https://lkml.iu.edu/hypermail/linux/kernel/2501.3/05151.html), [LWN v9 series](https://lwn.net/Articles/1004358/)
- Kernel doc: daemon registers ring entries with `FUSE_URING_REQ_REGISTER` SQEs (`IORING_OP_URING_CMD` on the /dev/fuse fd); once each queue has an entry the kernel queues requests to rings; daemon answers with `FUSE_URING_REQ_COMMIT_AND_FETCH` which also re-arms; "Notifications from the daemon and interrupts are not yet supported over io_uring" (still via /dev/fuse); optional buffer pool and CAP_SYS_ADMIN zero-copy. The doc states no ordering guarantees between requests. — [fuse-io-uring.rst](https://docs.kernel.org/filesystems/fuse/fuse-io-uring.html)
- Linux 7.3 (Phoronix, 2026-08-26): io_uring buffer pools and zero-copy for the FUSE io_uring transport. — [Phoronix](https://phoronix.com/news/Linux-7.3-FUSE)

### Inferences
- io_uring mode implies one server thread per CPU queue, i.e. the server must already be fully multithread-safe; it changes nothing about which requests can be concurrent, only throughput/latency. fuser 0.18.0 has no io_uring transport (not in its CHANGELOG).
- Passthrough is of little use to SliceFS (content-addressed chunks, no single backing file per user file).

### Gaps
- Whether the Docker/OrbStack Linux kernel used by SliceFS tests has `CONFIG_FUSE_IO_URING` enabled was not checked.

## macOS: FUSE-T, macFUSE and Apple FSKit threading

### Takeaway
Apple's FSKit headers do not document whether `FSVolume` operations are called concurrently; the API is asynchronous (reply handler or Swift `async`), and the only thread-safety text says FSKit enforces safety for attribute get/set and asks modules to fill one `FSItemAttributes` per item within one "synchronization context". FUSE-T and macFUSE (5.x) both offer `backend=fskit` through libfuse3, but neither documents its threading. So on macOS SliceFS must assume operations can be concurrent and verify empirically.

### Cited Findings
- FSKit `FSItem.h` (macOS 26.5 SDK): "The FSItemAttributes properties have no explicit thread safety provisions, since the operations that either get or set these properties enforce thread safety." `FSItem` "is the FSKit equivelant of a vnode in the kernel." — local SDK `/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk/System/Library/Frameworks/FSKit.framework/Headers/FSItem.h` (Apple docs: [FSItem](https://developer.apple.com/documentation/fskit/fsitem))
- `FSItemAttributes`: "FSKit internally maintains a unique sequence number for each set of attributes… all attributes of a given FSItemAttributes object should be populated within the same synchronization context. This synchronization context should prevent any other thread from populating another FSItemAttributes object of the given FSItem." — same header. This implies FSKit may call attribute-producing operations for one item from more than one thread.
- `FSVolume.h` operations (mount, unmount, synchronize, getAttributes, setAttributes, lookupItem, …) take a `replyHandler`; "For an async Swift implementation, there's no reply handler; simply return…". No threading/serialization statement found by grep for thread/queue/serial/concurrent/order. — local SDK `FSVolume.h`; [FSVolume.Operations](https://developer.apple.com/documentation/fskit/fsvolume/operations) (page body not retrievable by fetch)
- Third-party FSKit sample guards its inode→item cache with a serial `DispatchQueue` ("serializes access"), which suggests operations arrive on multiple threads; it does not say so explicitly. — [GHFS FSKIT.md](https://github.com/indragiek/GHFS/blob/main/FSKIT.md)
- macFUSE 5.0.0 (2025-05-05) introduced the FSKit backend; 5.1.0 (2025-10-30): `backend=fskit` works only with libfuse3; 5.3.0 (2026-06-08) added a new channel API for exchanging FUSE messages that "significantly improves I/O" for FSKit volumes; 5.5.0 pre-release adds FUSE_FALLOCATE for FSKit. — [macFUSE 5.0.0](https://macfuse.github.io/2025/05/05/macfuse-5.0.0.html), [5.1.0](https://macfuse.github.io/2025/10/30/macfuse-5.1.0.html), [5.3.0](https://macfuse.github.io/2026/06/08/macfuse-5.3.0.html), [5.5.0](https://newreleases.io/project/github/macfuse/macfuse/release/macfuse-5.5.0)
- FUSE-T wiki (edited 2026-04-02): `backend` option "can be either NFS, SMB or FSKit. The default is NFS"; notifications unsupported for NFS and FSKit; locks "Work in progress", bypass FUSE. Nothing on threading or on the libfuse↔server socket protocol. — [FUSE-T wiki](https://github.com/macos-fuse-t/fuse-t/wiki)
- Apple forum: FSKit lacks some FUSE operations (e.g. forget notifications). — [Apple Developer Forums](https://developer.apple.com/forums/thread/831338?answerId=891228022)
- FUSE-T hands libfuse a SOCK_STREAM socket (not a device) — `vendor/fuser/src/channel.rs` comment (local), mirroring FUSE-T's `lib/fuse_kern_chan.c`.

### Inferences
- Treat FSKit (and FUSE-T's translation to FUSE messages) as able to issue concurrent requests for the same item; the server-side locking needed on Linux is also needed here.
- Since FUSE-T's socket protocol carries the FUSE `unique` field, out-of-order replies are probably accepted, but this is unverified; a spike (one reader thread, worker pool, deliberately delayed replies) should confirm before relying on it.

### Gaps
- No primary source states FSKit's dispatch model (one serial queue per volume vs. concurrent queue, per-item serialization). Apple's FSKit docs pages render without body text to automated fetch; WWDC sessions were not reviewed.
- FUSE-T's own threading (how many requests it keeps outstanding on the socket, whether it waits for each reply) is undocumented; FUSE-T server is closed source.
- macFUSE's FSKit backend threading not documented in release notes.
