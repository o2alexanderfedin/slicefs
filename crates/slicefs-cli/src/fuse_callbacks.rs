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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    AccessFlags, BsdFileFlags, Errno, FileHandle, Filesystem, FopenFlags,
    Generation, INodeNo, InitFlags, KernelConfig, LockOwner, OpenFlags, RenameFlags, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyLock, ReplyOpen,
    ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow, WriteFlags,
};

use crate::filesystem::{SliceFsFilesystem, TTL};
use crate::handlers::{
    handle_create, handle_flush, handle_fsync, handle_getattr, handle_getlk,
    handle_getxattr, handle_link, handle_listxattr, handle_lookup, handle_mkdir,
    handle_mknod, handle_open, handle_read, handle_readdir, handle_readlink,
    handle_release, handle_removexattr, handle_rename, handle_rmdir, handle_setattr,
    handle_setxattr, handle_statfs, handle_symlink, handle_unlink, handle_write,
    AttrResult, CreateResult, DataResult, EmptyResult, EntryResult, LockResult,
    OpenResult, ReaddirResult, StatfsResult, WriteResult, XattrResult,
};

impl Filesystem for SliceFsFilesystem {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        eprintln!("[FUSE] init: ENTRY — requesting capabilities: FUSE_ATOMIC_O_TRUNC, FUSE_BIG_WRITES");
        // Advertise FUSE_ATOMIC_O_TRUNC so that FUSE-T passes O_TRUNC directly in
        // the create()/open() flags rather than sending a separate setattr(size=0)
        // after the create. Without this, FUSE-T sends setattr(size=0) for every
        // O_CREAT|O_TRUNC open, which fails with EIO on a brand-new inode (no manifest
        // entry yet) and causes FUSE-T's NFS layer to stall/hang indefinitely.
        let r1 = config.add_capabilities(InitFlags::FUSE_ATOMIC_O_TRUNC);
        // Request FUSE_BIG_WRITES so write requests can exceed a single page (4KB).
        // On macOS, fuser's defaults omit this flag, limiting write throughput.
        let r2 = config.add_capabilities(InitFlags::FUSE_BIG_WRITES);
        eprintln!("[FUSE] init: EXIT ok — FUSE_ATOMIC_O_TRUNC={:?}, FUSE_BIG_WRITES={:?}, store_path={:?}, auto_snapshot={}", r1.is_ok(), r2.is_ok(), self.store_path, self.auto_snapshot);
        Ok(())
    }

    fn destroy(&mut self) {
        eprintln!("[FUSE] destroy: ENTRY — auto_snapshot={}", self.auto_snapshot);
        let ok = self.test_destroy();
        eprintln!("[FUSE] destroy: EXIT — commit_ok={}", ok);
    }

    // ── Read operations ───────────────────────────────────────────────────────

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        eprintln!("[FUSE] getattr: ENTRY ino={}, fh={:?}", ino.0, _fh.map(|f| f.0));
        match handle_getattr(self, ino.0) {
            AttrResult::Ok(attr) => {
                eprintln!("[FUSE] getattr: EXIT ino={} -> ok size={}, mode={:#o}, uid={}, gid={}, nlink={}, kind={:?}", ino.0, attr.size, attr.perm, attr.uid, attr.gid, attr.nlink, attr.kind);
                reply.attr(&TTL, &attr);
            }
            AttrResult::Error(e) => {
                eprintln!("[FUSE] getattr: EXIT ino={} -> error {}", ino.0, e);
                reply.error(Errno::from_i32(e));
            }
        }
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        eprintln!("[FUSE] lookup: ENTRY parent={}, name={:?}", parent.0, name);
        let name_str = name.to_str().unwrap_or("");
        match handle_lookup(self, parent.0, name_str) {
            EntryResult::Ok(child_ino, attr) => {
                eprintln!("[FUSE] lookup: EXIT parent={}, name={:?} -> found ino={}, size={}, mode={:#o}, kind={:?}", parent.0, name, child_ino, attr.size, attr.perm, attr.kind);
                reply.entry(&TTL, &attr, Generation(0));
            }
            EntryResult::Error(errno) => {
                eprintln!("[FUSE] lookup: EXIT parent={}, name={:?} -> error {}", parent.0, name, errno);
                reply.error(Errno::from_i32(errno));
            }
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        eprintln!("[FUSE] readdir: ENTRY ino={}, fh={}, offset={}", ino.0, _fh.0, offset);
        match handle_readdir(self, ino.0, offset) {
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
                eprintln!("[FUSE] readdir: EXIT ino={} -> ok, total_entries={}, returned={}", ino.0, total, returned);
                reply.ok();
            }
            ReaddirResult::Error(errno) => {
                eprintln!("[FUSE] readdir: EXIT ino={} -> error {}", ino.0, errno);
                reply.error(Errno::from_i32(errno));
            }
        }
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
        eprintln!("[FUSE] read: ENTRY ino={}, fh={}, offset={}, size={}, flags={:#x}", ino.0, _fh.0, offset, size, _flags.0);
        match handle_read(self, ino.0, offset, size) {
            DataResult::Ok(data) => {
                eprintln!("[FUSE] read: EXIT ino={}, fh={} -> ok, {} bytes returned", ino.0, _fh.0, data.len());
                reply.data(&data);
            }
            DataResult::Error(e) => {
                eprintln!("[FUSE] read: EXIT ino={}, fh={} -> error {}", ino.0, _fh.0, e);
                reply.error(Errno::from_i32(e));
            }
        }
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
        let has_trunc = flags.0 & libc::O_TRUNC != 0;
        let has_append = flags.0 & libc::O_APPEND != 0;
        eprintln!("[FUSE] open: ENTRY ino={}, flags={:#x}, acc_mode={}, O_TRUNC={}, O_APPEND={}", ino.0, flags.0, acc_str, has_trunc, has_append);
        match handle_open(self, ino.0, flags.0) {
            OpenResult::Ok(fh, is_write) => {
                eprintln!("[FUSE] open: EXIT ino={} -> ok, fh={}, writable={}", ino.0, fh, is_write);
                reply.opened(FileHandle(fh), FopenFlags::empty());
            }
            OpenResult::Error(e) => {
                eprintln!("[FUSE] open: EXIT ino={} -> error {}", ino.0, e);
                reply.error(Errno::from_i32(e));
            }
        }
    }

    fn opendir(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        eprintln!("[FUSE] opendir: ENTRY ino={}, flags={:#x}", _ino.0, _flags.0);
        eprintln!("[FUSE] opendir: EXIT ino={} -> ok, fh=0", _ino.0);
        reply.opened(FileHandle(0), FopenFlags::empty());
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
        eprintln!("[FUSE] release: ENTRY ino={}, fh={}, flush={}, flags={:#x}, lock_owner={:?}", ino.0, fh.0, flush, _flags.0, _lock_owner.map(|l| l.0));
        match handle_release(self, ino.0, fh.0) {
            EmptyResult::Ok => {
                eprintln!("[FUSE] release: EXIT ino={}, fh={} -> ok", ino.0, fh.0);
                reply.ok();
            }
            EmptyResult::Error(e) => {
                eprintln!("[FUSE] release: EXIT ino={}, fh={} -> error {}", ino.0, fh.0, e);
                reply.error(Errno::EIO);
            }
        }
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        eprintln!("[FUSE] releasedir: ENTRY ino={}, fh={}, flags={:#x}", _ino.0, _fh.0, _flags.0);
        eprintln!("[FUSE] releasedir: EXIT ino={} -> ok", _ino.0);
        reply.ok();
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
        eprintln!("[FUSE] flush: ENTRY ino={}, fh={}, lock_owner={}", ino.0, fh.0, _lock_owner.0);
        match handle_flush(self, ino.0, fh.0) {
            EmptyResult::Ok => {
                eprintln!("[FUSE] flush: EXIT ino={}, fh={} -> ok", ino.0, fh.0);
                reply.ok();
            }
            EmptyResult::Error(e) => {
                eprintln!("[FUSE] flush: EXIT ino={}, fh={} -> error {}", ino.0, fh.0, e);
                reply.error(Errno::EIO);
            }
        }
    }

    /// Flush any buffered writes for `fh` to CAS and sync the WAL to disk.
    ///
    /// `datasync` is ignored — SliceFS treats fsync and fdatasync identically.
    /// If `fh` has a write buffer, it is flushed through the CAS pipeline and
    /// the buffer is reset to empty (file handle stays open for subsequent writes).
    /// The WAL is then synced to disk to ensure durability.
    fn fsync(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        eprintln!("[FUSE] fsync: ENTRY ino={}, fh={}, datasync={}", ino.0, fh.0, _datasync);
        match handle_fsync(self, ino.0, fh.0) {
            EmptyResult::Ok => {
                eprintln!("[FUSE] fsync: EXIT ino={}, fh={} -> ok", ino.0, fh.0);
                reply.ok();
            }
            EmptyResult::Error(e) => {
                eprintln!("[FUSE] fsync: EXIT ino={}, fh={} -> error {}", ino.0, fh.0, e);
                reply.error(Errno::EIO);
            }
        }
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        eprintln!("[FUSE] statfs: ENTRY ino={}", _ino.0);
        match handle_statfs(self) {
            StatfsResult::Ok(blocks, bfree, bavail, files, ffree, bsize) => {
                eprintln!("[FUSE] statfs: EXIT ino={} -> ok, blocks={}, bfree={}, bavail={}, files={}, ffree={}, bsize={}", _ino.0, blocks, bfree, bavail, files, ffree, bsize);
                reply.statfs(blocks, bfree, bavail, files, ffree, bsize, 255, bsize);
            }
        }
    }

    fn access(&self, _req: &Request, _ino: INodeNo, _mask: AccessFlags, reply: ReplyEmpty) {
        eprintln!("[FUSE] access: ENTRY ino={}, mask={:#x}", _ino.0, _mask.bits());
        eprintln!("[FUSE] access: EXIT ino={} -> ok", _ino.0);
        // Read-only filesystem — all reads are allowed
        reply.ok();
    }

    fn getxattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        name: &OsStr,
        size: u32,
        reply: ReplyXattr,
    ) {
        eprintln!("[FUSE] getxattr: ENTRY ino={}, name={:?}, size={}", ino.0, name, size);
        let name_str = name.to_str().unwrap_or("");
        match handle_getxattr(self, ino.0, name_str, size) {
            XattrResult::Size(sz) => {
                eprintln!("[FUSE] getxattr: EXIT ino={}, name={:?} -> size_query, value_len={}", ino.0, name, sz);
                reply.size(sz);
            }
            XattrResult::Data(value) => {
                eprintln!("[FUSE] getxattr: EXIT ino={}, name={:?} -> ok, value_len={}", ino.0, name, value.len());
                reply.data(&value);
            }
            XattrResult::Error(_) => {
                eprintln!("[FUSE] getxattr: EXIT ino={}, name={:?} -> NO_XATTR", ino.0, name);
                reply.error(Errno::NO_XATTR);
            }
        }
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        eprintln!("[FUSE] listxattr: ENTRY ino={}, size={}", ino.0, size);
        match handle_listxattr(self, ino.0, size) {
            XattrResult::Size(sz) => {
                eprintln!("[FUSE] listxattr: EXIT ino={} -> size_query, total_len={}", ino.0, sz);
                reply.size(sz);
            }
            XattrResult::Data(buf) => {
                eprintln!("[FUSE] listxattr: EXIT ino={} -> ok, total_len={}", ino.0, buf.len());
                reply.data(&buf);
            }
            XattrResult::Error(errno) => {
                eprintln!("[FUSE] listxattr: EXIT ino={} -> error {}", ino.0, errno);
                reply.error(Errno::from_i32(errno));
            }
        }
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
        eprintln!("[FUSE] setxattr: ENTRY ino={}, name={:?}, value_len={}, flags={:#x}, position={}", ino.0, name, value.len(), _flags, _position);
        let name_str = name.to_str().unwrap_or("");
        match handle_setxattr(self, ino.0, name_str, value) {
            EmptyResult::Ok => {
                eprintln!("[FUSE] setxattr: EXIT ino={}, name={:?} -> ok", ino.0, name);
                reply.ok();
            }
            EmptyResult::Error(errno) => {
                eprintln!("[FUSE] setxattr: EXIT ino={}, name={:?} -> error {}", ino.0, name, errno);
                reply.error(Errno::from_i32(errno));
            }
        }
    }

    fn removexattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        eprintln!("[FUSE] removexattr: ENTRY ino={}, name={:?}", ino.0, name);
        let name_str = name.to_str().unwrap_or("");
        match handle_removexattr(self, ino.0, name_str) {
            EmptyResult::Ok => {
                eprintln!("[FUSE] removexattr: EXIT ino={}, name={:?} -> ok", ino.0, name);
                reply.ok();
            }
            EmptyResult::Error(e) if e == libc::ENODATA => {
                eprintln!("[FUSE] removexattr: EXIT ino={}, name={:?} -> NO_XATTR (not found)", ino.0, name);
                reply.error(Errno::NO_XATTR);
            }
            EmptyResult::Error(errno) => {
                eprintln!("[FUSE] removexattr: EXIT ino={}, name={:?} -> error {}", ino.0, name, errno);
                reply.error(Errno::from_i32(errno));
            }
        }
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
        eprintln!("[FUSE] write: ENTRY ino={}, fh={}, offset={}, data_len={}, write_flags={:#x}, flags={:#x}, lock_owner={:?}", ino.0, fh.0, offset, data.len(), _write_flags.bits(), _flags.0, _lock_owner.map(|l| l.0));
        match handle_write(self, fh.0, offset, data) {
            WriteResult::Ok(n) => {
                eprintln!("[FUSE] write: EXIT ino={}, fh={} -> ok, written={} bytes", ino.0, fh.0, n);
                reply.written(n);
            }
            WriteResult::Error(e) => {
                eprintln!("[FUSE] write: EXIT ino={}, fh={} -> error {}", ino.0, fh.0, e);
                reply.error(Errno::EBADF);
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
        eprintln!("[FUSE] create: ENTRY parent={}, name={:?}, mode={:#o}, umask={:#o}, flags={:#x}, uid={}, gid={}", parent.0, name, mode, umask, flags, req.uid(), req.gid());
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                eprintln!("[FUSE] create: EXIT parent={}, name={:?} -> EINVAL (invalid name)", parent.0, name);
                return reply.error(Errno::EINVAL);
            }
        };
        match handle_create(self, parent.0, name_str, mode, umask, req.uid(), req.gid(), flags) {
            CreateResult::Ok(ino, fh, attr) => {
                eprintln!("[FUSE] create: EXIT parent={}, name={:?} -> ok, ino={}, fh={}, size={}, mode={:#o}", parent.0, name, ino, fh, attr.size, attr.perm);
                reply.created(&TTL, &attr, Generation(0), FileHandle(fh), FopenFlags::empty());
            }
            CreateResult::Error(errno) => {
                eprintln!("[FUSE] create: EXIT parent={}, name={:?} -> error {}", parent.0, name, errno);
                reply.error(Errno::from_i32(errno));
            }
        }
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
        eprintln!("[FUSE] mkdir: ENTRY parent={}, name={:?}, mode={:#o}, umask={:#o}, uid={}, gid={}", parent.0, name, mode, umask, req.uid(), req.gid());
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                eprintln!("[FUSE] mkdir: EXIT parent={}, name={:?} -> EINVAL", parent.0, name);
                return reply.error(Errno::EINVAL);
            }
        };
        match handle_mkdir(self, parent.0, name_str, mode, umask, req.uid(), req.gid()) {
            EntryResult::Ok(ino, attr) => {
                eprintln!("[FUSE] mkdir: EXIT parent={}, name={:?} -> ok, ino={}, mode={:#o}", parent.0, name, ino, attr.perm);
                reply.entry(&TTL, &attr, Generation(0));
            }
            EntryResult::Error(errno) => {
                eprintln!("[FUSE] mkdir: EXIT parent={}, name={:?} -> error {}", parent.0, name, errno);
                reply.error(Errno::from_i32(errno));
            }
        }
    }

    fn mknod(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        eprintln!("[FUSE] mknod: ENTRY parent={}, name={:?}, mode={:#o}, umask={:#o}, rdev={}, uid={}, gid={}", parent.0, name, mode, umask, _rdev, req.uid(), req.gid());
        let name_str = match name.to_str() {
            Some(s) => s,
            None => return reply.error(Errno::EINVAL),
        };
        match handle_mknod(self, parent.0, name_str, mode, umask, req.uid(), req.gid()) {
            EntryResult::Ok(ino, attr) => {
                eprintln!("[FUSE] mknod: EXIT parent={}, name={:?} -> ok, ino={}, mode={:#o}", parent.0, name, ino, attr.perm);
                reply.entry(&TTL, &attr, Generation(0));
            }
            EntryResult::Error(errno) => {
                eprintln!("[FUSE] mknod: EXIT parent={}, name={:?} -> error {}", parent.0, name, errno);
                reply.error(Errno::from_i32(errno));
            }
        }
    }

    fn symlink(
        &self,
        req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &std::path::Path,
        reply: ReplyEntry,
    ) {
        eprintln!("[FUSE] symlink: ENTRY parent={}, link_name={:?}, target={:?}, uid={}, gid={}", parent.0, link_name, target, req.uid(), req.gid());
        let name_str = match link_name.to_str() {
            Some(s) => s,
            None => {
                eprintln!("[FUSE] symlink: EXIT parent={} -> EINVAL (invalid link_name)", parent.0);
                return reply.error(Errno::EINVAL);
            }
        };
        let target_str = match target.to_str() {
            Some(s) => s,
            None => {
                eprintln!("[FUSE] symlink: EXIT parent={} -> EINVAL (invalid target)", parent.0);
                return reply.error(Errno::EINVAL);
            }
        };
        match handle_symlink(self, parent.0, name_str, target_str, req.uid(), req.gid()) {
            EntryResult::Ok(ino, attr) => {
                eprintln!("[FUSE] symlink: EXIT parent={}, link_name={:?} -> ok, ino={}, size={}", parent.0, link_name, ino, attr.size);
                reply.entry(&TTL, &attr, Generation(0));
            }
            EntryResult::Error(errno) => {
                eprintln!("[FUSE] symlink: EXIT parent={}, link_name={:?} -> error {}", parent.0, link_name, errno);
                reply.error(Errno::from_i32(errno));
            }
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        eprintln!("[FUSE] readlink: ENTRY ino={}", ino.0);
        match handle_readlink(self, ino.0) {
            DataResult::Ok(bytes) => {
                eprintln!("[FUSE] readlink: EXIT ino={} -> ok, len={}", ino.0, bytes.len());
                reply.data(&bytes);
            }
            DataResult::Error(errno) => {
                eprintln!("[FUSE] readlink: EXIT ino={} -> error {}", ino.0, errno);
                reply.error(Errno::from_i32(errno));
            }
        }
    }

    fn link(
        &self,
        _req: &Request,
        ino: INodeNo,
        newparent: INodeNo,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        eprintln!("[FUSE] link: ENTRY ino={}, newparent={}, newname={:?}", ino.0, newparent.0, newname);
        let name_str = match newname.to_str() {
            Some(s) => s,
            None => {
                eprintln!("[FUSE] link: EXIT ino={} -> EINVAL (invalid name)", ino.0);
                return reply.error(Errno::EINVAL);
            }
        };
        match handle_link(self, ino.0, newparent.0, name_str) {
            EntryResult::Ok(new_ino, attr) => {
                eprintln!("[FUSE] link: EXIT ino={}, newparent={}, newname={:?} -> ok, nlink={}", ino.0, newparent.0, newname, attr.nlink);
                reply.entry(&TTL, &attr, Generation(0));
            }
            EntryResult::Error(errno) => {
                eprintln!("[FUSE] link: EXIT ino={} -> error {}", ino.0, errno);
                reply.error(Errno::from_i32(errno));
            }
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        eprintln!("[FUSE] unlink: ENTRY parent={}, name={:?}", parent.0, name);
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                eprintln!("[FUSE] unlink: EXIT parent={}, name={:?} -> EINVAL", parent.0, name);
                return reply.error(Errno::EINVAL);
            }
        };
        match handle_unlink(self, parent.0, name_str) {
            EmptyResult::Ok => {
                eprintln!("[FUSE] unlink: EXIT parent={}, name={:?} -> ok", parent.0, name);
                reply.ok();
            }
            EmptyResult::Error(errno) => {
                eprintln!("[FUSE] unlink: EXIT parent={}, name={:?} -> error {}", parent.0, name, errno);
                reply.error(Errno::from_i32(errno));
            }
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        eprintln!("[FUSE] rmdir: ENTRY parent={}, name={:?}", parent.0, name);
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                eprintln!("[FUSE] rmdir: EXIT parent={}, name={:?} -> EINVAL", parent.0, name);
                return reply.error(Errno::EINVAL);
            }
        };
        match handle_rmdir(self, parent.0, name_str) {
            EmptyResult::Ok => {
                eprintln!("[FUSE] rmdir: EXIT parent={}, name={:?} -> ok", parent.0, name);
                reply.ok();
            }
            EmptyResult::Error(errno) => {
                eprintln!("[FUSE] rmdir: EXIT parent={}, name={:?} -> error {}", parent.0, name, errno);
                reply.error(Errno::from_i32(errno));
            }
        }
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
        eprintln!("[FUSE] rename: ENTRY parent={}, name={:?}, newparent={}, newname={:?}, flags={:#x}", parent.0, name, newparent.0, newname, flags.bits());
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                eprintln!("[FUSE] rename: EXIT -> EINVAL (invalid name)");
                return reply.error(Errno::EINVAL);
            }
        };
        let newname_str = match newname.to_str() {
            Some(s) => s,
            None => {
                eprintln!("[FUSE] rename: EXIT -> EINVAL (invalid newname)");
                return reply.error(Errno::EINVAL);
            }
        };
        match handle_rename(self, parent.0, name_str, newparent.0, newname_str, flags.bits()) {
            EmptyResult::Ok => {
                eprintln!("[FUSE] rename: EXIT parent={}, name={:?} -> newparent={}, newname={:?} -> ok", parent.0, name, newparent.0, newname);
                reply.ok();
            }
            EmptyResult::Error(errno) => {
                eprintln!("[FUSE] rename: EXIT parent={}, name={:?} -> error {}", parent.0, name, errno);
                reply.error(Errno::from_i32(errno));
            }
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        eprintln!("[FUSE] setattr: ENTRY ino={}, mode={:?}, uid={:?}, gid={:?}, size={:?}, mtime={:?}, fh={:?}", ino.0, mode.map(|m| format!("{:#o}", m)), uid, gid, size, mtime.as_ref().map(|t| match t { TimeOrNow::Now => "Now".to_string(), TimeOrNow::SpecificTime(st) => format!("{:?}", st) }), fh.map(|f| f.0));
        // Convert TimeOrNow to (sec, nsec) for the testable method
        let mtime_pair = mtime.map(|mt| {
            let t = match mt {
                TimeOrNow::SpecificTime(st) => st,
                TimeOrNow::Now => SystemTime::now(),
            };
            let dur = t.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
            (dur.as_secs() as i64, dur.subsec_nanos())
        });
        let fh_val = fh.map(|f| f.0);
        match handle_setattr(self, ino.0, mode, uid, gid, size, fh_val, mtime_pair) {
            AttrResult::Ok(attr) => {
                eprintln!("[FUSE] setattr: EXIT ino={} -> ok, size={}, mode={:#o}, uid={}, gid={}", ino.0, attr.size, attr.perm, attr.uid, attr.gid);
                reply.attr(&TTL, &attr);
            }
            AttrResult::Error(errno) => {
                eprintln!("[FUSE] setattr: EXIT ino={} -> error {}", ino.0, errno);
                reply.error(Errno::from_i32(errno));
            }
        }
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
        let type_str = match _typ { x if x == libc::F_RDLCK as i32 => "F_RDLCK", x if x == libc::F_WRLCK as i32 => "F_WRLCK", x if x == libc::F_UNLCK as i32 => "F_UNLCK", _ => "UNKNOWN" };
        eprintln!("[FUSE] getlk: ENTRY ino={}, fh={}, lock_owner={}, start={}, end={}, type={}({}), pid={}", ino.0, _fh.0, _lock_owner.0, start, end, type_str, _typ, pid);
        match handle_getlk(start, end, pid) {
            LockResult::Ok(s, e, typ, p) => {
                eprintln!("[FUSE] getlk: EXIT ino={}, fh={} -> F_UNLCK (no conflict)", ino.0, _fh.0);
                reply.locked(s, e, typ, p);
            }
        }
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
        let type_str = match _typ { x if x == libc::F_RDLCK as i32 => "F_RDLCK", x if x == libc::F_WRLCK as i32 => "F_WRLCK", x if x == libc::F_UNLCK as i32 => "F_UNLCK", _ => "UNKNOWN" };
        eprintln!("[FUSE] setlk: ENTRY ino={}, fh={}, lock_owner={}, start={}, end={}, type={}({}), pid={}, sleep={}", ino.0, _fh.0, _lock_owner.0, _start, _end, type_str, _typ, _pid, _sleep);
        eprintln!("[FUSE] setlk: EXIT ino={}, fh={} -> ok (no-op, single-threaded)", ino.0, _fh.0);
        reply.ok();
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
        eprintln!("[FUSE] fallocate: ENTRY ino={}, fh={}, offset={}, length={}, mode={:#x}", _ino.0, _fh.0, _offset, _length, _mode);
        // Return ENOSYS (not supported) instead of EROFS (read-only filesystem).
        // EROFS would incorrectly signal to the NFS client that the filesystem is
        // read-only, potentially blocking all subsequent write operations.
        eprintln!("[FUSE] fallocate: EXIT ino={} -> ENOSYS (not supported)", _ino.0);
        reply.error(Errno::ENOSYS);
    }
}
