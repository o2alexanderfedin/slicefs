//! FUSE `Filesystem` trait implementation for `SliceFsFilesystem`.
//!
//! These are thin wrappers that extract parameters from fuser types,
//! call the corresponding `handle_*` functions from `handlers.rs`,
//! and map results to fuser reply types.
//!
//! All business logic lives in `filesystem.rs` / `handlers.rs`; this module
//! handles only the FUSE protocol layer and is excluded from coverage reporting
//! because fuser's Request/Reply types cannot be constructed in unit tests.

use std::ffi::OsStr;
use std::io;
use std::sync::{Arc, OnceLock};
use std::time::{Instant, SystemTime};

use fuser::{
    AccessFlags, BsdFileFlags, Errno, FileHandle, Filesystem, FopenFlags,
    Generation, INodeNo, InitFlags, KernelConfig, LockOwner, OpenFlags, RenameFlags, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyLock, ReplyOpen,
    ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow, WriteFlags,
};

use crate::filesystem::{SliceFsFilesystem, TTL};
use crate::pool::{Pool, DRAIN_WAIT};
use crate::util::ts;
use crate::handlers::{
    handle_create, handle_flush, handle_fsync, handle_fsyncdir, handle_getattr, handle_getlk,
    handle_getxattr, handle_link, handle_listxattr, handle_lookup, handle_mkdir,
    handle_mknod, handle_open, handle_read, handle_readdir, handle_readlink,
    handle_release, handle_removexattr, handle_rename, handle_rmdir, handle_setattr,
    handle_setxattr, handle_statfs, handle_symlink, handle_unlink, handle_write_admitted,
    AttrResult, CreateResult, DataResult, EmptyResult, EntryResult, LockResult,
    OpenResult, ReaddirResult, StatfsResult, WriteResult, XattrResult,
};

/// False when `SLICEFS_FUSE_TRACE` is `0`, else true. It is read once, at
/// the first request. The bench of plan 21-08 turns the per-request lines
/// off to measure the cost of the global stderr lock; the default does not
/// change the logs.
fn trace_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("SLICEFS_FUSE_TRACE").is_none_or(|v| v != "0"))
}

/// `eprintln!` for one per-request ENTRY or EXIT line. When `trace_on()`
/// is false, it prints nothing and formats nothing.
macro_rules! fuse_trace {
    ($($arg:tt)*) => {
        if trace_on() {
            eprintln!($($arg)*);
        }
    };
}

/// The file system as the FUSE session owns it. fuser takes the file system
/// by value; this shares it, so the GC thread of the mount can reach it too.
/// With a pool, the request jobs run on its workers; with none (one
/// worker), they run on the FUSE reader thread.
pub struct MountedFs {
    fs: Arc<SliceFsFilesystem>,
    pool: Option<Pool>,
}

impl std::ops::Deref for MountedFs {
    type Target = SliceFsFilesystem;
    fn deref(&self) -> &SliceFsFilesystem {
        &self.fs
    }
}

impl MountedFs {
    /// The session value of `fs`; `pool` is `None` for one worker.
    pub fn new(fs: Arc<SliceFsFilesystem>, pool: Option<Pool>) -> Self {
        MountedFs { fs, pool }
    }

    /// Run a request job: on this (reader) thread with no pool, else on a
    /// worker of the pool. `weight` is the payload bytes that the job owns
    /// (0 for most requests).
    fn run(&self, weight: usize, job: impl FnOnce() + Send + 'static) {
        match &self.pool {
            None => job(),
            Some(pool) => pool.submit(weight, Box::new(job)),
        }
    }
}

/// Run one handler inside one shared hold of the commit gate, and return
/// its result with every lock released. The caller sends the reply after
/// this returns, so no reply is sent with the gate held. This and `init`
/// are the only places in this file that take the gate (test
/// `reply_after_locks`). In a debug build, it panics when its result keeps
/// a ranked lock alive at the reply point.
fn serve<T>(fs: &SliceFsFilesystem, f: impl FnOnce() -> T) -> T {
    let r = {
        let _op = fs.gate_shared();
        f()
    };
    debug_assert!(
        chunk_store::sync::rank::held().is_empty(),
        "reply with a lock held: {:?}",
        chunk_store::sync::rank::held()
    );
    r
}

/// The fields of one WRITE request that its job needs, copied from the
/// request. The payload is passed apart, so the inline path can borrow it.
#[derive(Clone, Copy)]
struct WriteReq {
    ino: u64,
    fh: u64,
    offset: u64,
    write_flags: u32,
    flags: i32,
    lock_owner: Option<u64>,
}

/// One WRITE: the budget admission with no lock held (Phase 20), then the
/// handler inside `serve`, then the reply. With no pool, `data` is the
/// borrowed request payload (no copy); with a pool, it is the job's copy.
fn write_job(fs: &SliceFsFilesystem, w: WriteReq, data: &[u8], reply: ReplyWrite) {
    let t_throttle = Instant::now();
    let admit = fs.throttle_write();
    let throttle = t_throttle.elapsed();
    fuse_trace!("[{}][FUSE] write: ENTRY ino={}, fh={}, offset={}, data_len={}, write_flags={:#x}, flags={:#x}, lock_owner={:?}", ts(), w.ino, w.fh, w.offset, data.len(), w.write_flags, w.flags, w.lock_owner);
    let t_serve = Instant::now();
    let (handled, handle) = serve(fs, || {
        let t_handle = Instant::now();
        // The open flags of the request: O_APPEND writes at the end of file.
        let r = handle_write_admitted(fs, w.fh, w.offset, data, w.flags, admit);
        (r, t_handle.elapsed())
    });
    // The serve time less the handler time: the gate wait (and its release).
    let gate_wait = t_serve.elapsed().saturating_sub(handle);
    if (throttle + gate_wait + handle).as_millis() >= 50 {
        eprintln!("[t={}][slow-write] ino={} offset={} len={} throttle_ms={} gate_wait_ms={} handle_ms={}",
            crate::util::epoch(), w.ino, w.offset, data.len(), throttle.as_millis(), gate_wait.as_millis(), handle.as_millis());
    }
    match handled {
        WriteResult::Ok(n) => {
            fuse_trace!("[{}][FUSE] write: EXIT ino={}, fh={} -> ok, written={} bytes", ts(), w.ino, w.fh, n);
            reply.written(n);
        }
        WriteResult::Error(e) => {
            fuse_trace!("[{}][FUSE] write: EXIT ino={}, fh={} -> error {}", ts(), w.ino, w.fh, e);
            reply.error(Errno::from_i32(e));
        }
    }
}

impl Filesystem for MountedFs {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        let _op = self.gate_shared();
        eprintln!("[{}][FUSE] init: ENTRY — requesting capabilities: FUSE_ATOMIC_O_TRUNC, FUSE_BIG_WRITES", ts());
        // Advertise FUSE_ATOMIC_O_TRUNC so that FUSE-T passes O_TRUNC directly in
        // the create()/open() flags rather than sending a separate setattr(size=0)
        // after the create. Without this, FUSE-T sends setattr(size=0) for every
        // O_CREAT|O_TRUNC open, which fails with EIO on a brand-new inode (no manifest
        // entry yet) and causes FUSE-T's NFS layer to stall/hang indefinitely.
        let r1 = config.add_capabilities(InitFlags::FUSE_ATOMIC_O_TRUNC);
        // Request FUSE_BIG_WRITES so write requests can exceed a single page (4KB).
        // On macOS, fuser's defaults omit this flag, limiting write throughput.
        let r2 = config.add_capabilities(InitFlags::FUSE_BIG_WRITES);
        eprintln!("[{}][FUSE] init: EXIT ok — FUSE_ATOMIC_O_TRUNC={:?}, FUSE_BIG_WRITES={:?}, store_path={:?}, auto_snapshot={}", ts(), r1.is_ok(), r2.is_ok(), self.store_path, self.auto_snapshot);
        Ok(())
    }

    fn destroy(&mut self) {
        eprintln!("[{}][FUSE] destroy: ENTRY — auto_snapshot={}", ts(), self.auto_snapshot);
        // The reader has ended, so no new job comes. Run every queued job and
        // join the workers first: the destroy work assumes that no request
        // runs. The flusher still runs during the drain.
        if let Some(pool) = self.pool.take() {
            match pool.drain_and_join(DRAIN_WAIT) {
                Ok(stats) => eprintln!("[{}][FUSE] destroy: {stats}", ts()),
                Err(busy) => {
                    eprintln!(
                        "[{}][FUSE] destroy: {busy} worker(s) still busy after {} s: abort; the write log replays at the next mount",
                        ts(),
                        DRAIN_WAIT.as_secs()
                    );
                    std::process::abort();
                }
            }
        }
        // Stop and join the flusher before the gate, then the destroy work.
        let ok = self.shutdown();
        eprintln!("[{}][FUSE] destroy: EXIT — commit_ok={}", ts(), ok);
    }

    // ── Read operations ───────────────────────────────────────────────────────

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let ino = ino.0;
        let fh = _fh.map(|f| f.0);
        let fs = Arc::clone(&self.fs);
        self.run(0, move || {
            fuse_trace!("[{}][FUSE] getattr: ENTRY ino={}, fh={:?}", ts(), ino, fh);
            match serve(&fs, || handle_getattr(&fs, ino)) {
                AttrResult::Ok(attr) => {
                    fuse_trace!("[{}][FUSE] getattr: EXIT ino={} -> ok size={}, mode={:#o}, uid={}, gid={}, nlink={}, kind={:?}", ts(), ino, attr.size, attr.perm, attr.uid, attr.gid, attr.nlink, attr.kind);
                    reply.attr(&TTL, &attr);
                }
                AttrResult::Error(e) => {
                    fuse_trace!("[{}][FUSE] getattr: EXIT ino={} -> error {}", ts(), ino, e);
                    reply.error(Errno::from_i32(e));
                }
            }
        });
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let parent = parent.0;
        let name = name.to_os_string();
        let fs = Arc::clone(&self.fs);
        self.run(0, move || {
            fuse_trace!("[{}][FUSE] lookup: ENTRY parent={}, name={:?}", ts(), parent, name);
            let name_str = name.to_str().unwrap_or("");
            match serve(&fs, || handle_lookup(&fs, parent, name_str)) {
                EntryResult::Ok(child_ino, attr) => {
                    fuse_trace!("[{}][FUSE] lookup: EXIT parent={}, name={:?} -> found ino={}, size={}, mode={:#o}, kind={:?}", ts(), parent, name, child_ino, attr.size, attr.perm, attr.kind);
                    reply.entry(&TTL, &attr, Generation(0));
                }
                EntryResult::Error(errno) => {
                    fuse_trace!("[{}][FUSE] lookup: EXIT parent={}, name={:?} -> error {}", ts(), parent, name, errno);
                    reply.error(Errno::from_i32(errno));
                }
            }
        });
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let (ino, fh) = (ino.0, _fh.0);
        let fs = Arc::clone(&self.fs);
        self.run(0, move || {
            fuse_trace!("[{}][FUSE] readdir: ENTRY ino={}, fh={}, offset={}", ts(), ino, fh, offset);
            match serve(&fs, || handle_readdir(&fs, ino, offset)) {
                ReaddirResult::Ok(entries) => {
                    let total = entries.len();
                    let mut returned = 0u64;
                    for (index, (entry_ino, kind, name)) in entries.iter().enumerate() {
                        let next_offset = offset + (index as u64) + 1;
                        let buffer_full = reply.add(INodeNo(*entry_ino), next_offset, *kind, name);
                        if buffer_full {
                            break;
                        }
                        returned += 1;
                    }
                    fuse_trace!("[{}][FUSE] readdir: EXIT ino={} -> ok, total_entries={}, returned={}", ts(), ino, total, returned);
                    reply.ok();
                }
                ReaddirResult::Error(errno) => {
                    fuse_trace!("[{}][FUSE] readdir: EXIT ino={} -> error {}", ts(), ino, errno);
                    reply.error(Errno::from_i32(errno));
                }
            }
        });
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let (ino, fh, flags) = (ino.0, _fh.0, _flags.0);
        let fs = Arc::clone(&self.fs);
        self.run(0, move || {
            fuse_trace!("[{}][FUSE] read: ENTRY ino={}, fh={}, offset={}, size={}, flags={:#x}", ts(), ino, fh, offset, size, flags);
            match serve(&fs, || handle_read(&fs, ino, offset, size)) {
                DataResult::Ok(data) => {
                    fuse_trace!("[{}][FUSE] read: EXIT ino={}, fh={} -> ok, {} bytes returned", ts(), ino, fh, data.len());
                    reply.data(&data);
                }
                DataResult::Error(e) => {
                    fuse_trace!("[{}][FUSE] read: EXIT ino={}, fh={} -> error {}", ts(), ino, fh, e);
                    reply.error(Errno::from_i32(e));
                }
            }
        });
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        use fuser::OpenAccMode;
        let acc = flags.acc_mode();
        #[allow(unreachable_patterns)]
        let acc_str = match acc {
            OpenAccMode::O_RDONLY => "O_RDONLY",
            OpenAccMode::O_WRONLY => "O_WRONLY",
            OpenAccMode::O_RDWR => "O_RDWR",
            _ => "UNKNOWN",
        };
        let (ino, flags) = (ino.0, flags.0);
        let fs = Arc::clone(&self.fs);
        self.run(0, move || {
            let has_trunc = flags & libc::O_TRUNC != 0;
            let has_append = flags & libc::O_APPEND != 0;
            fuse_trace!("[{}][FUSE] open: ENTRY ino={}, flags={:#x}, acc_mode={}, O_TRUNC={}, O_APPEND={}", ts(), ino, flags, acc_str, has_trunc, has_append);
            match serve(&fs, || handle_open(&fs, ino, flags)) {
                OpenResult::Ok(fh, is_write) => {
                    fuse_trace!("[{}][FUSE] open: EXIT ino={} -> ok, fh={}, writable={}", ts(), ino, fh, is_write);
                    reply.opened(FileHandle(fh), FopenFlags::empty());
                }
                OpenResult::Error(e) => {
                    fuse_trace!("[{}][FUSE] open: EXIT ino={} -> error {}", ts(), ino, e);
                    reply.error(Errno::from_i32(e));
                }
            }
        });
    }

    fn opendir(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let (ino, flags) = (_ino.0, _flags.0);
        let fs = Arc::clone(&self.fs);
        self.run(0, move || {
            fuse_trace!("[{}][FUSE] opendir: ENTRY ino={}, flags={:#x}", ts(), ino, flags);
            let fh = serve(&fs, || fs.alloc_dir_handle());
            fuse_trace!("[{}][FUSE] opendir: EXIT ino={} -> ok, fh={}", ts(), ino, fh);
            reply.opened(FileHandle(fh), FopenFlags::empty());
        });
    }

    fn release(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        flush: bool,
        reply: ReplyEmpty,
    ) {
        let (ino, fh, flags, owner) = (ino.0, fh.0, _flags.0, _lock_owner.map(|l| l.0));
        let fs = Arc::clone(&self.fs);
        self.run(0, move || {
            fuse_trace!("[{}][FUSE] release: ENTRY ino={}, fh={}, flush={}, flags={:#x}, lock_owner={:?}", ts(), ino, fh, flush, flags, owner);
            match serve(&fs, || handle_release(&fs, ino, fh)) {
                EmptyResult::Ok => {
                    fuse_trace!("[{}][FUSE] release: EXIT ino={}, fh={} -> ok", ts(), ino, fh);
                    reply.ok();
                }
                EmptyResult::Error(e) => {
                    fuse_trace!("[{}][FUSE] release: EXIT ino={}, fh={} -> error {}", ts(), ino, fh, e);
                    reply.error(Errno::EIO);
                }
            }
        });
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        let (ino, fh, flags) = (_ino.0, _fh.0, _flags.0);
        let fs = Arc::clone(&self.fs);
        self.run(0, move || {
            fuse_trace!("[{}][FUSE] releasedir: ENTRY ino={}, fh={}, flags={:#x}", ts(), ino, fh, flags);
            serve(&fs, || ());
            fuse_trace!("[{}][FUSE] releasedir: EXIT ino={} -> ok", ts(), ino);
            reply.ok();
        });
    }

    /// Flush any buffered writes for `fh` to CAS in response to a `close(2)` syscall.
    ///
    /// Under FUSE-T on macOS, the NFS layer translates the NFS4 CLOSE operation into a
    /// FUSE flush call. Returning ENOSYS (the fuser default) causes the macOS NFS client
    /// to stall indefinitely, making every write hang. This implementation flushes the
    /// write buffer through the CAS pipeline and replies ok(), unblocking the NFS CLOSE.
    ///
    /// Unlike `fsync`, the WAL is NOT synced here — durability is provided by a subsequent
    /// `release` or explicit `fsync`. `flush` may be called multiple times for the same
    /// file handle (once per dup'd fd that is closed), so the buffer is preserved
    /// (reset to empty) rather than the handle being removed.
    fn flush(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        let (ino, fh, owner) = (ino.0, fh.0, _lock_owner.0);
        let fs = Arc::clone(&self.fs);
        self.run(0, move || {
            fuse_trace!("[{}][FUSE] flush: ENTRY ino={}, fh={}, lock_owner={}", ts(), ino, fh, owner);
            match serve(&fs, || handle_flush(&fs, ino, fh)) {
                EmptyResult::Ok => {
                    fuse_trace!("[{}][FUSE] flush: EXIT ino={}, fh={} -> ok", ts(), ino, fh);
                    reply.ok();
                }
                EmptyResult::Error(e) => {
                    fuse_trace!("[{}][FUSE] flush: EXIT ino={}, fh={} -> error {}", ts(), ino, fh, e);
                    reply.error(Errno::EIO);
                }
            }
        });
    }

    /// Flush any buffered writes for `fh` to CAS and sync the WAL to disk.
    ///
    /// `datasync` is ignored — SliceFS treats fsync and fdatasync identically.
    /// If `fh` has a write buffer, it is flushed through the CAS pipeline and
    /// the buffer is reset to empty (file handle stays open for subsequent writes).
    /// The WAL is then synced to disk to ensure durability.
    /// Make the directory changes so far durable (see `handle_fsyncdir`).
    fn fsyncdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        let ino = ino.0;
        let fs = Arc::clone(&self.fs);
        // The durability wait stays inside the handler, so inside `serve`.
        self.run(0, move || match serve(&fs, || handle_fsyncdir(&fs)) {
            EmptyResult::Ok => reply.ok(),
            EmptyResult::Error(e) => {
                eprintln!("[{}][FUSE] fsyncdir: ino={} -> error {}", ts(), ino, e);
                reply.error(Errno::EIO);
            }
        });
    }

    fn fsync(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        let (ino, fh) = (ino.0, fh.0);
        let fs = Arc::clone(&self.fs);
        // The durability wait stays inside the handler, so inside `serve`.
        self.run(0, move || {
            fuse_trace!("[{}][FUSE] fsync: ENTRY ino={}, fh={}, datasync={}", ts(), ino, fh, _datasync);
            match serve(&fs, || handle_fsync(&fs, ino, fh)) {
                EmptyResult::Ok => {
                    fuse_trace!("[{}][FUSE] fsync: EXIT ino={}, fh={} -> ok", ts(), ino, fh);
                    reply.ok();
                }
                EmptyResult::Error(e) => {
                    fuse_trace!("[{}][FUSE] fsync: EXIT ino={}, fh={} -> error {}", ts(), ino, fh, e);
                    reply.error(Errno::EIO);
                }
            }
        });
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let ino = _ino.0;
        let fs = Arc::clone(&self.fs);
        self.run(0, move || {
            fuse_trace!("[{}][FUSE] statfs: ENTRY ino={}", ts(), ino);
            match serve(&fs, || handle_statfs(&fs)) {
                StatfsResult::Ok(blocks, bfree, bavail, files, ffree, bsize) => {
                    fuse_trace!("[{}][FUSE] statfs: EXIT ino={} -> ok, blocks={}, bfree={}, bavail={}, files={}, ffree={}, bsize={}", ts(), ino, blocks, bfree, bavail, files, ffree, bsize);
                    reply.statfs(blocks, bfree, bavail, files, ffree, bsize, 255, bsize);
                }
            }
        });
    }

    fn access(&self, _req: &Request, _ino: INodeNo, _mask: AccessFlags, reply: ReplyEmpty) {
        let (ino, mask) = (_ino.0, _mask.bits());
        let fs = Arc::clone(&self.fs);
        self.run(0, move || {
            fuse_trace!("[{}][FUSE] access: ENTRY ino={}, mask={:#x}", ts(), ino, mask);
            serve(&fs, || ());
            fuse_trace!("[{}][FUSE] access: EXIT ino={} -> ok", ts(), ino);
            // Read-only filesystem — all reads are allowed
            reply.ok();
        });
    }

    fn getxattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        name: &OsStr,
        size: u32,
        reply: ReplyXattr,
    ) {
        let ino = ino.0;
        let name = name.to_os_string();
        let fs = Arc::clone(&self.fs);
        self.run(0, move || {
            fuse_trace!("[{}][FUSE] getxattr: ENTRY ino={}, name={:?}, size={}", ts(), ino, name, size);
            let name_str = name.to_str().unwrap_or("");
            match serve(&fs, || handle_getxattr(&fs, ino, name_str, size)) {
                XattrResult::Size(sz) => {
                    fuse_trace!("[{}][FUSE] getxattr: EXIT ino={}, name={:?} -> size_query, value_len={}", ts(), ino, name, sz);
                    reply.size(sz);
                }
                XattrResult::Data(value) => {
                    fuse_trace!("[{}][FUSE] getxattr: EXIT ino={}, name={:?} -> ok, value_len={}", ts(), ino, name, value.len());
                    reply.data(&value);
                }
                XattrResult::Error(_) => {
                    fuse_trace!("[{}][FUSE] getxattr: EXIT ino={}, name={:?} -> NO_XATTR", ts(), ino, name);
                    reply.error(Errno::NO_XATTR);
                }
            }
        });
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        let ino = ino.0;
        let fs = Arc::clone(&self.fs);
        self.run(0, move || {
            fuse_trace!("[{}][FUSE] listxattr: ENTRY ino={}, size={}", ts(), ino, size);
            match serve(&fs, || handle_listxattr(&fs, ino, size)) {
                XattrResult::Size(sz) => {
                    fuse_trace!("[{}][FUSE] listxattr: EXIT ino={} -> size_query, total_len={}", ts(), ino, sz);
                    reply.size(sz);
                }
                XattrResult::Data(buf) => {
                    fuse_trace!("[{}][FUSE] listxattr: EXIT ino={} -> ok, total_len={}", ts(), ino, buf.len());
                    reply.data(&buf);
                }
                XattrResult::Error(errno) => {
                    fuse_trace!("[{}][FUSE] listxattr: EXIT ino={} -> error {}", ts(), ino, errno);
                    reply.error(Errno::from_i32(errno));
                }
            }
        });
    }

    fn setxattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        name: &OsStr,
        value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        let ino = ino.0;
        let name = name.to_os_string();
        // A small value: copied on both paths; the job weighs its length.
        let value = value.to_vec();
        let fs = Arc::clone(&self.fs);
        self.run(value.len(), move || {
            fuse_trace!("[{}][FUSE] setxattr: ENTRY ino={}, name={:?}, value_len={}, flags={:#x}, position={}", ts(), ino, name, value.len(), _flags, _position);
            let name_str = name.to_str().unwrap_or("");
            match serve(&fs, || handle_setxattr(&fs, ino, name_str, &value)) {
                EmptyResult::Ok => {
                    fuse_trace!("[{}][FUSE] setxattr: EXIT ino={}, name={:?} -> ok", ts(), ino, name);
                    reply.ok();
                }
                EmptyResult::Error(errno) => {
                    fuse_trace!("[{}][FUSE] setxattr: EXIT ino={}, name={:?} -> error {}", ts(), ino, name, errno);
                    reply.error(Errno::from_i32(errno));
                }
            }
        });
    }

    fn removexattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let ino = ino.0;
        let name = name.to_os_string();
        let fs = Arc::clone(&self.fs);
        self.run(0, move || {
            fuse_trace!("[{}][FUSE] removexattr: ENTRY ino={}, name={:?}", ts(), ino, name);
            let name_str = name.to_str().unwrap_or("");
            match serve(&fs, || handle_removexattr(&fs, ino, name_str)) {
                EmptyResult::Ok => {
                    fuse_trace!("[{}][FUSE] removexattr: EXIT ino={}, name={:?} -> ok", ts(), ino, name);
                    reply.ok();
                }
                EmptyResult::Error(e) if e == libc::ENODATA => {
                    fuse_trace!("[{}][FUSE] removexattr: EXIT ino={}, name={:?} -> NO_XATTR (not found)", ts(), ino, name);
                    reply.error(Errno::NO_XATTR);
                }
                EmptyResult::Error(errno) => {
                    fuse_trace!("[{}][FUSE] removexattr: EXIT ino={}, name={:?} -> error {}", ts(), ino, name, errno);
                    reply.error(Errno::from_i32(errno));
                }
            }
        });
    }

    // ── Write operations ───────────────────────────────────────────────────────

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let w = WriteReq {
            ino: ino.0,
            fh: fh.0,
            offset,
            write_flags: _write_flags.bits(),
            flags: _flags.0,
            lock_owner: _lock_owner.map(|l| l.0),
        };
        match &self.pool {
            // One worker: on this thread, with the borrowed payload (no copy).
            None => write_job(&self.fs, w, data, reply),
            // A pool: the job owns a copy of the payload and weighs its length.
            Some(pool) => {
                let data = data.to_vec();
                let fs = Arc::clone(&self.fs);
                pool.submit(data.len(), Box::new(move || write_job(&fs, w, &data, reply)));
            }
        }
    }

    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let (uid, gid) = (req.uid(), req.gid());
        let parent = parent.0;
        let name = name.to_os_string();
        let fs = Arc::clone(&self.fs);
        self.run(0, move || {
            fuse_trace!("[{}][FUSE] create: ENTRY parent={}, name={:?}, mode={:#o}, umask={:#o}, flags={:#x}, uid={}, gid={}", ts(), parent, name, mode, umask, flags, uid, gid);
            let name_str = match name.to_str() {
                Some(s) => s,
                None => {
                    fuse_trace!("[{}][FUSE] create: EXIT parent={}, name={:?} -> EINVAL (invalid name)", ts(), parent, name);
                    return reply.error(Errno::EINVAL);
                }
            };
            match serve(&fs, || handle_create(&fs, parent, name_str, mode, umask, uid, gid, flags)) {
                CreateResult::Ok(ino, fh, attr) => {
                    fuse_trace!("[{}][FUSE] create: EXIT parent={}, name={:?} -> ok, ino={}, fh={}, size={}, mode={:#o}", ts(), parent, name, ino, fh, attr.size, attr.perm);
                    reply.created(&TTL, &attr, Generation(0), FileHandle(fh), FopenFlags::empty());
                }
                CreateResult::Error(errno) => {
                    fuse_trace!("[{}][FUSE] create: EXIT parent={}, name={:?} -> error {}", ts(), parent, name, errno);
                    reply.error(Errno::from_i32(errno));
                }
            }
        });
    }

    fn mkdir(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        let (uid, gid) = (req.uid(), req.gid());
        let parent = parent.0;
        let name = name.to_os_string();
        let fs = Arc::clone(&self.fs);
        self.run(0, move || {
            fuse_trace!("[{}][FUSE] mkdir: ENTRY parent={}, name={:?}, mode={:#o}, umask={:#o}, uid={}, gid={}", ts(), parent, name, mode, umask, uid, gid);
            let name_str = match name.to_str() {
                Some(s) => s,
                None => {
                    fuse_trace!("[{}][FUSE] mkdir: EXIT parent={}, name={:?} -> EINVAL", ts(), parent, name);
                    return reply.error(Errno::EINVAL);
                }
            };
            match serve(&fs, || handle_mkdir(&fs, parent, name_str, mode, umask, uid, gid)) {
                EntryResult::Ok(ino, attr) => {
                    fuse_trace!("[{}][FUSE] mkdir: EXIT parent={}, name={:?} -> ok, ino={}, mode={:#o}", ts(), parent, name, ino, attr.perm);
                    reply.entry(&TTL, &attr, Generation(0));
                }
                EntryResult::Error(errno) => {
                    fuse_trace!("[{}][FUSE] mkdir: EXIT parent={}, name={:?} -> error {}", ts(), parent, name, errno);
                    reply.error(Errno::from_i32(errno));
                }
            }
        });
    }

    fn mknod(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        rdev: u32,
        reply: ReplyEntry,
    ) {
        let (uid, gid) = (req.uid(), req.gid());
        let parent = parent.0;
        let name = name.to_os_string();
        let fs = Arc::clone(&self.fs);
        self.run(0, move || {
            fuse_trace!("[{}][FUSE] mknod: ENTRY parent={}, name={:?}, mode={:#o}, umask={:#o}, rdev={}, uid={}, gid={}", ts(), parent, name, mode, umask, rdev, uid, gid);
            let name_str = match name.to_str() {
                Some(s) => s,
                None => return reply.error(Errno::EINVAL),
            };
            match serve(&fs, || handle_mknod(&fs, parent, name_str, mode, umask, rdev, uid, gid)) {
                EntryResult::Ok(ino, attr) => {
                    fuse_trace!("[{}][FUSE] mknod: EXIT parent={}, name={:?} -> ok, ino={}, mode={:#o}", ts(), parent, name, ino, attr.perm);
                    reply.entry(&TTL, &attr, Generation(0));
                }
                EntryResult::Error(errno) => {
                    fuse_trace!("[{}][FUSE] mknod: EXIT parent={}, name={:?} -> error {}", ts(), parent, name, errno);
                    reply.error(Errno::from_i32(errno));
                }
            }
        });
    }

    fn symlink(
        &self,
        req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &std::path::Path,
        reply: ReplyEntry,
    ) {
        let (uid, gid) = (req.uid(), req.gid());
        let parent = parent.0;
        let link_name = link_name.to_os_string();
        let target = target.to_path_buf();
        let fs = Arc::clone(&self.fs);
        self.run(0, move || {
            fuse_trace!("[{}][FUSE] symlink: ENTRY parent={}, link_name={:?}, target={:?}, uid={}, gid={}", ts(), parent, link_name, target, uid, gid);
            let name_str = match link_name.to_str() {
                Some(s) => s,
                None => {
                    fuse_trace!("[{}][FUSE] symlink: EXIT parent={} -> EINVAL (invalid link_name)", ts(), parent);
                    return reply.error(Errno::EINVAL);
                }
            };
            let target_str = match target.to_str() {
                Some(s) => s,
                None => {
                    fuse_trace!("[{}][FUSE] symlink: EXIT parent={} -> EINVAL (invalid target)", ts(), parent);
                    return reply.error(Errno::EINVAL);
                }
            };
            match serve(&fs, || handle_symlink(&fs, parent, name_str, target_str, uid, gid)) {
                EntryResult::Ok(ino, attr) => {
                    fuse_trace!("[{}][FUSE] symlink: EXIT parent={}, link_name={:?} -> ok, ino={}, size={}", ts(), parent, link_name, ino, attr.size);
                    reply.entry(&TTL, &attr, Generation(0));
                }
                EntryResult::Error(errno) => {
                    fuse_trace!("[{}][FUSE] symlink: EXIT parent={}, link_name={:?} -> error {}", ts(), parent, link_name, errno);
                    reply.error(Errno::from_i32(errno));
                }
            }
        });
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let ino = ino.0;
        let fs = Arc::clone(&self.fs);
        self.run(0, move || {
            fuse_trace!("[{}][FUSE] readlink: ENTRY ino={}", ts(), ino);
            match serve(&fs, || handle_readlink(&fs, ino)) {
                DataResult::Ok(bytes) => {
                    fuse_trace!("[{}][FUSE] readlink: EXIT ino={} -> ok, len={}", ts(), ino, bytes.len());
                    reply.data(&bytes);
                }
                DataResult::Error(errno) => {
                    fuse_trace!("[{}][FUSE] readlink: EXIT ino={} -> error {}", ts(), ino, errno);
                    reply.error(Errno::from_i32(errno));
                }
            }
        });
    }

    fn link(
        &self,
        _req: &Request,
        ino: INodeNo,
        newparent: INodeNo,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        let (ino, newparent) = (ino.0, newparent.0);
        let newname = newname.to_os_string();
        let fs = Arc::clone(&self.fs);
        self.run(0, move || {
            fuse_trace!("[{}][FUSE] link: ENTRY ino={}, newparent={}, newname={:?}", ts(), ino, newparent, newname);
            let name_str = match newname.to_str() {
                Some(s) => s,
                None => {
                    fuse_trace!("[{}][FUSE] link: EXIT ino={} -> EINVAL (invalid name)", ts(), ino);
                    return reply.error(Errno::EINVAL);
                }
            };
            match serve(&fs, || handle_link(&fs, ino, newparent, name_str)) {
                EntryResult::Ok(new_ino, attr) => {
                    fuse_trace!("[{}][FUSE] link: EXIT ino={}, newparent={}, newname={:?} -> ok, ino={}, nlink={}", ts(), ino, newparent, newname, new_ino, attr.nlink);
                    reply.entry(&TTL, &attr, Generation(0));
                }
                EntryResult::Error(errno) => {
                    fuse_trace!("[{}][FUSE] link: EXIT ino={} -> error {}", ts(), ino, errno);
                    reply.error(Errno::from_i32(errno));
                }
            }
        });
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let parent = parent.0;
        let name = name.to_os_string();
        let fs = Arc::clone(&self.fs);
        self.run(0, move || {
            fuse_trace!("[{}][FUSE] unlink: ENTRY parent={}, name={:?}", ts(), parent, name);
            let name_str = match name.to_str() {
                Some(s) => s,
                None => {
                    fuse_trace!("[{}][FUSE] unlink: EXIT parent={}, name={:?} -> EINVAL", ts(), parent, name);
                    return reply.error(Errno::EINVAL);
                }
            };
            match serve(&fs, || handle_unlink(&fs, parent, name_str)) {
                EmptyResult::Ok => {
                    fuse_trace!("[{}][FUSE] unlink: EXIT parent={}, name={:?} -> ok", ts(), parent, name);
                    reply.ok();
                }
                EmptyResult::Error(errno) => {
                    fuse_trace!("[{}][FUSE] unlink: EXIT parent={}, name={:?} -> error {}", ts(), parent, name, errno);
                    reply.error(Errno::from_i32(errno));
                }
            }
        });
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let parent = parent.0;
        let name = name.to_os_string();
        let fs = Arc::clone(&self.fs);
        self.run(0, move || {
            fuse_trace!("[{}][FUSE] rmdir: ENTRY parent={}, name={:?}", ts(), parent, name);
            let name_str = match name.to_str() {
                Some(s) => s,
                None => {
                    fuse_trace!("[{}][FUSE] rmdir: EXIT parent={}, name={:?} -> EINVAL", ts(), parent, name);
                    return reply.error(Errno::EINVAL);
                }
            };
            match serve(&fs, || handle_rmdir(&fs, parent, name_str)) {
                EmptyResult::Ok => {
                    fuse_trace!("[{}][FUSE] rmdir: EXIT parent={}, name={:?} -> ok", ts(), parent, name);
                    reply.ok();
                }
                EmptyResult::Error(errno) => {
                    fuse_trace!("[{}][FUSE] rmdir: EXIT parent={}, name={:?} -> error {}", ts(), parent, name, errno);
                    reply.error(Errno::from_i32(errno));
                }
            }
        });
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let (parent, newparent, flags) = (parent.0, newparent.0, flags.bits());
        let (name, newname) = (name.to_os_string(), newname.to_os_string());
        let fs = Arc::clone(&self.fs);
        self.run(0, move || {
            fuse_trace!("[{}][FUSE] rename: ENTRY parent={}, name={:?}, newparent={}, newname={:?}, flags={:#x}", ts(), parent, name, newparent, newname, flags);
            let name_str = match name.to_str() {
                Some(s) => s,
                None => {
                    fuse_trace!("[{}][FUSE] rename: EXIT -> EINVAL (invalid name)", ts());
                    return reply.error(Errno::EINVAL);
                }
            };
            let newname_str = match newname.to_str() {
                Some(s) => s,
                None => {
                    fuse_trace!("[{}][FUSE] rename: EXIT -> EINVAL (invalid newname)", ts());
                    return reply.error(Errno::EINVAL);
                }
            };
            match serve(&fs, || handle_rename(&fs, parent, name_str, newparent, newname_str, flags)) {
                EmptyResult::Ok => {
                    fuse_trace!("[{}][FUSE] rename: EXIT parent={}, name={:?} -> newparent={}, newname={:?} -> ok", ts(), parent, name, newparent, newname);
                    reply.ok();
                }
                EmptyResult::Error(errno) => {
                    fuse_trace!("[{}][FUSE] rename: EXIT parent={}, name={:?} -> error {}", ts(), parent, name, errno);
                    reply.error(Errno::from_i32(errno));
                }
            }
        });
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let ino = ino.0;
        let fh_val = fh.map(|f| f.0);
        let fs = Arc::clone(&self.fs);
        self.run(0, move || {
            fuse_trace!("[{}][FUSE] setattr: ENTRY ino={}, mode={:?}, uid={:?}, gid={:?}, size={:?}, mtime={:?}, fh={:?}", ts(), ino, mode.map(|m| format!("{:#o}", m)), uid, gid, size, mtime.as_ref().map(|t| match t { TimeOrNow::Now => "Now".to_string(), TimeOrNow::SpecificTime(st) => format!("{:?}", st) }), fh_val);
            // Convert TimeOrNow to (sec, nsec) for the testable method
            let to_pair = |t: TimeOrNow| crate::filesystem::system_time_to_pair(match t {
                TimeOrNow::SpecificTime(st) => st,
                TimeOrNow::Now => SystemTime::now(),
            });
            let atime_pair = atime.map(to_pair);
            let mtime_pair = mtime.map(to_pair);
            match serve(&fs, || handle_setattr(&fs, ino, mode, uid, gid, size, fh_val, atime_pair, mtime_pair)) {
                AttrResult::Ok(attr) => {
                    fuse_trace!("[{}][FUSE] setattr: EXIT ino={} -> ok, size={}, mode={:#o}, uid={}, gid={}", ts(), ino, attr.size, attr.perm, attr.uid, attr.gid);
                    reply.attr(&TTL, &attr);
                }
                AttrResult::Error(errno) => {
                    fuse_trace!("[{}][FUSE] setattr: EXIT ino={} -> error {}", ts(), ino, errno);
                    reply.error(Errno::from_i32(errno));
                }
            }
        });
    }

    /// Test for a POSIX file lock.
    ///
    /// FUSE-T on macOS translates NFS4 LOCK operations to FUSE getlk/setlk.
    /// The fuser default returns ENOSYS, which on Linux causes kernel fallback
    /// to local locking. However, FUSE-T's NFS server does NOT implement this
    /// fallback — ENOSYS causes the NFS4 LOCK compound to fail, and with hard
    /// mount semantics the NFS client retries forever, hanging all reads/writes.
    ///
    /// Since SliceFS runs single-threaded (fuser on macOS), there is no actual
    /// lock contention. Return "no conflicting lock" (F_UNLCK) unconditionally.
    fn getlk(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        start: u64,
        end: u64,
        _typ: i32,
        pid: u32,
        reply: ReplyLock,
    ) {
        let (ino, fh, owner, typ) = (ino.0, _fh.0, _lock_owner.0, _typ);
        let fs = Arc::clone(&self.fs);
        self.run(0, move || {
            let type_str = match typ { x if x == libc::F_RDLCK as i32 => "F_RDLCK", x if x == libc::F_WRLCK as i32 => "F_WRLCK", x if x == libc::F_UNLCK as i32 => "F_UNLCK", _ => "UNKNOWN" };
            fuse_trace!("[{}][FUSE] getlk: ENTRY ino={}, fh={}, lock_owner={}, start={}, end={}, type={}({}), pid={}", ts(), ino, fh, owner, start, end, type_str, typ, pid);
            match serve(&fs, || handle_getlk(start, end, pid)) {
                LockResult::Ok(s, e, typ, p) => {
                    fuse_trace!("[{}][FUSE] getlk: EXIT ino={}, fh={} -> F_UNLCK (no conflict)", ts(), ino, fh);
                    reply.locked(s, e, typ, p);
                }
            }
        });
    }

    /// Acquire, modify or release a POSIX file lock.
    ///
    /// FUSE-T on macOS translates NFS4 LOCK/LOCKU operations to FUSE setlk.
    /// Must return success (not ENOSYS) to prevent NFS client from hanging on
    /// hard-mount retry. Since fuser runs single-threaded on macOS, there is no
    /// real lock contention — all lock requests succeed as no-ops.
    fn setlk(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        _start: u64,
        _end: u64,
        _typ: i32,
        _pid: u32,
        _sleep: bool,
        reply: ReplyEmpty,
    ) {
        let (ino, fh, owner, typ) = (ino.0, _fh.0, _lock_owner.0, _typ);
        let fs = Arc::clone(&self.fs);
        self.run(0, move || {
            let type_str = match typ { x if x == libc::F_RDLCK as i32 => "F_RDLCK", x if x == libc::F_WRLCK as i32 => "F_WRLCK", x if x == libc::F_UNLCK as i32 => "F_UNLCK", _ => "UNKNOWN" };
            fuse_trace!("[{}][FUSE] setlk: ENTRY ino={}, fh={}, lock_owner={}, start={}, end={}, type={}({}), pid={}, sleep={}", ts(), ino, fh, owner, _start, _end, type_str, typ, _pid, _sleep);
            serve(&fs, || ());
            fuse_trace!("[{}][FUSE] setlk: EXIT ino={}, fh={} -> ok (no-op, single-threaded)", ts(), ino, fh);
            reply.ok();
        });
    }

    fn fallocate(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _offset: u64,
        _length: u64,
        _mode: i32,
        reply: ReplyEmpty,
    ) {
        let (ino, fh) = (_ino.0, _fh.0);
        let fs = Arc::clone(&self.fs);
        self.run(0, move || {
            fuse_trace!("[{}][FUSE] fallocate: ENTRY ino={}, fh={}, offset={}, length={}, mode={:#x}", ts(), ino, fh, _offset, _length, _mode);
            serve(&fs, || ());
            // Return ENOSYS (not supported) instead of EROFS (read-only filesystem).
            // EROFS would incorrectly signal to the NFS client that the filesystem is
            // read-only, potentially blocking all subsequent write operations.
            fuse_trace!("[{}][FUSE] fallocate: EXIT ino={} -> ENOSYS (not supported)", ts(), ino);
            reply.error(Errno::ENOSYS);
        });
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use metadata::store::DictMetadataStore;
    use metadata::store_io::StoreIo;
    use metadata::wal::{create_wal, WalConfig};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    /// A file system on a new per-op write-log store in `dir`.
    fn make_fs(dir: &TempDir) -> Arc<SliceFsFilesystem> {
        std::fs::create_dir_all(dir.path().join("segments")).unwrap();
        let wal = create_wal(WalConfig::PerOp, dir.path(), 1).unwrap();
        let io = Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap()));
        let mut meta = DictMetadataStore::new(io.clone());
        meta.set_wal(wal);
        Arc::new(SliceFsFilesystem::new(meta, io, Some(dir.path().to_path_buf())))
    }

    #[test]
    fn run_without_a_pool_runs_on_the_caller() {
        let dir = TempDir::new().unwrap();
        let mfs = MountedFs::new(make_fs(&dir), None);
        let seen = Arc::new(Mutex::new(None));
        let s = Arc::clone(&seen);
        mfs.run(0, move || *s.lock().unwrap() = Some(std::thread::current().id()));
        assert_eq!(*seen.lock().unwrap(), Some(std::thread::current().id()), "the job ran on the caller");
    }

    #[test]
    fn destroy_drains_before_shutdown() {
        let dir = TempDir::new().unwrap();
        let fs = make_fs(&dir);
        fs.start_flusher();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !fs.test_flusher_running() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(fs.test_flusher_running(), "the flusher runs");
        let mut mfs = MountedFs::new(Arc::clone(&fs), Some(Pool::new(2).unwrap()));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (s, f) = (Arc::clone(&seen), Arc::clone(&fs));
        mfs.run(0, move || {
            std::thread::sleep(Duration::from_millis(100));
            s.lock().unwrap().push((f.is_shut_down(), f.test_flusher_running()));
        });
        Filesystem::destroy(&mut mfs);
        assert_eq!(
            *seen.lock().unwrap(),
            vec![(false, true)],
            "the job ran once, before the shutdown: (shut down, flusher running)"
        );
        assert!(fs.is_shut_down(), "destroy shut the file system down");
    }

    /// The debug assert of `serve`: a result that keeps a ranked entry alive
    /// past the gate drop panics at the reply point.
    #[cfg(debug_assertions)]
    #[test]
    fn serve_panics_when_a_lock_is_held_at_the_reply() {
        use chunk_store::sync::{rank, Rank};
        let dir = TempDir::new().unwrap();
        let fs = make_fs(&dir);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _kept = serve(&fs, || rank::enter(Rank::Short));
        }));
        let err = r.expect_err("serve returned with a lock held and did not panic");
        let msg = err
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| err.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default();
        assert!(msg.contains("reply with a lock held"), "panic message: {msg:?}");
        assert!(rank::held().is_empty(), "the unwind left a rank entry: {:?}", rank::held());
    }
}
