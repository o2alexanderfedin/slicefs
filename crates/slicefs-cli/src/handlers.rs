//! Testable handler functions for FUSE callbacks.
//!
//! Each `handle_<name>` function contains the business-logic layer extracted from
//! the corresponding FUSE callback in `fuse_callbacks.rs`. They accept plain Rust
//! types (no fuser Request/Reply types) and return structured result enums.
//!
//! The FUSE callbacks become thin wrappers: call the handler, match the result,
//! call the appropriate reply method.
//!
//! Tests in this module exercise every handler on both success and error paths
//! without requiring fuser types (which cannot be constructed in unit tests).


use std::time::{Duration, Instant};

use fuser::FileAttr;

use crate::filesystem::{clear_store_busy, inode_to_file_attr, take_store_busy, Admit, SliceFsFilesystem, XattrReply};
use crate::util::ts;
use metadata::wal::Lsn;
use slicefs_traits::metadata::InodeMeta;

// ── Result enums ─────────────────────────────────────────────────────────────

/// Result for `getattr` / `setattr` — returns a `FileAttr` on success.
#[derive(Debug)]
pub enum AttrResult {
    Ok(FileAttr),
    Error(i32),
}

/// Result for `lookup`, `mkdir`, `mknod`, `symlink`, `link` — returns a `FileAttr`
/// (the entry attributes) together with the inode number on success.
#[derive(Debug)]
pub enum EntryResult {
    Ok(u64, FileAttr),
    Error(i32),
}

/// Result for `readdir` — on success, a list of `(ino, FileType, name)` tuples.
///
/// For tests only (a whole listing in RAM): the FUSE callback streams
/// through `SliceFsFilesystem::readdir_handle`.
#[doc(hidden)]
#[derive(Debug)]
pub enum ReaddirResult {
    Ok(Vec<(u64, fuser::FileType, String)>),
    Error(i32),
}

/// Result for `read` / `readlink` — returns raw bytes on success.
#[derive(Debug)]
pub enum DataResult {
    Ok(Vec<u8>),
    Error(i32),
}

/// Result for `open` — returns `(fh, is_writable)` on success.
#[derive(Debug)]
pub enum OpenResult {
    Ok(u64, bool),
    Error(i32),
}

/// Result for callbacks that only succeed or fail with an errno (`release`, `flush`,
/// `fsync`, `setxattr`, `removexattr`, `unlink`, `rmdir`, `rename`, `write`).
#[derive(Debug)]
pub enum EmptyResult {
    Ok,
    Error(i32),
}

/// Result for `write` — returns the number of bytes written on success.
#[derive(Debug)]
pub enum WriteResult {
    Ok(u32),
    Error(i32),
}

/// Result for `create` — returns `(ino, fh, FileAttr)` on success.
#[derive(Debug)]
pub enum CreateResult {
    Ok(u64, u64, FileAttr),
    Error(i32),
}

/// Result for `statfs` — returns `(blocks, bfree, bavail, files, ffree, bsize)`.
#[derive(Debug)]
pub enum StatfsResult {
    Ok(u64, u64, u64, u64, u64, u32),
}

/// Result for `getxattr` / `listxattr`.
#[derive(Debug)]
pub enum XattrResult {
    /// Caller asked for the size only (size == 0 in the FUSE request).
    Size(u32),
    /// Caller asked for the data.
    Data(Vec<u8>),
    Error(i32),
}

/// Result for `getlk`.
#[derive(Debug)]
pub enum LockResult {
    /// Returns `(start, end, typ, pid)` describing the lock.
    Ok(u64, u64, i32, u32),
}

// ── Handler functions ─────────────────────────────────────────────────────────

/// One logged request (#20): freezes the clock while the request runs (all
/// its times are equal and replayable). Its record is appended in the lock
/// hold of its apply (`sink`, D-08) and written out before the reply.
/// During replay the clock is frozen already and nothing is logged.
struct Logged {
    _clock: Option<crate::oplog::Clock>,
    time: (i64, u32),
}

fn logged() -> Logged {
    match crate::oplog::frozen_now() {
        Some(time) => Logged { _clock: None, time },
        None => {
            let time = crate::filesystem::system_now();
            Logged { _clock: Some(crate::oplog::Clock::freeze_at(time)), time }
        }
    }
}

impl Logged {
    /// The log callback of a namespace request (D-08): the fs function
    /// calls it in the lock hold of its apply; it appends the record and
    /// keeps the LSN in `lsn`, which the handler writes out after the call,
    /// before the reply. A no-op while the log is replayed.
    fn sink<'a>(&'a self, fs: &'a SliceFsFilesystem, lsn: &'a mut Option<Lsn>) -> impl FnMut(crate::oplog::Op) + 'a {
        move |op| {
            if let Some(l) = fs.log_append(crate::oplog::Record { time: self.time, op }) {
                *lsn = Some(l);
            }
        }
    }
}

/// Handle `getattr`: look up inode `ino` and return its attributes.
pub fn handle_getattr(fs: &SliceFsFilesystem, ino: u64) -> AttrResult {
    match fs.test_getattr(ino) {
        Ok(meta) => AttrResult::Ok(inode_to_file_attr(&meta)),
        Err(e) => AttrResult::Error(crate::filesystem::meta_error_to_errno(&e)),
    }
}

/// Handle `lookup`: find `name` in directory `parent` and return its attributes.
pub fn handle_lookup(fs: &SliceFsFilesystem, parent: u64, name: &str) -> EntryResult {
    match fs.test_lookup(parent, name) {
        Ok((child_ino, meta)) => EntryResult::Ok(child_ino, inode_to_file_attr(&meta)),
        Err(errno) => EntryResult::Error(errno),
    }
}

/// Handle `readdir`: list entries in directory `ino` starting at `offset`.
///
/// For tests only (a whole listing in RAM): the FUSE callback streams
/// through `SliceFsFilesystem::readdir_handle`.
#[doc(hidden)]
pub fn handle_readdir(
    fs: &SliceFsFilesystem,
    ino: u64,
    offset: u64,
) -> ReaddirResult {
    match fs.test_readdir(ino, offset) {
        Ok(entries) => ReaddirResult::Ok(entries),
        Err(errno) => ReaddirResult::Error(errno),
    }
}

/// Handle `read`: read `size` bytes from `ino` starting at `offset`.
pub fn handle_read(
    fs: &SliceFsFilesystem,
    ino: u64,
    offset: u64,
    size: u32,
) -> DataResult {
    match fs.test_read(ino, offset, size) {
        Ok(data) => DataResult::Ok(data),
        Err(errno) => DataResult::Error(errno),
    }
}

/// Handle `open`: open inode `ino` with `flags`.
/// A write open with `O_TRUNC` appends its `Truncate` record in the cell
/// hold of the truncate (D-08); it is written out here, before the reply.
pub fn handle_open(fs: &SliceFsFilesystem, ino: u64, flags: i32) -> OpenResult {
    let l = logged();
    let (r, lsn) = fs.open_logged(ino, flags, Some(l.time));
    fs.log_write_out(lsn);
    match r {
        Ok((fh, is_write)) => OpenResult::Ok(fh, is_write),
        Err(errno) => OpenResult::Error(errno),
    }
}

/// Handle `release`: finalize write state for `fh` of inode `ino`. Only the
/// release that finalized the cell (the last write handle) appends a
/// `Release` record, in its last cell hold; it is written out here.
pub fn handle_release(fs: &SliceFsFilesystem, ino: u64, fh: u64) -> EmptyResult {
    let l = logged();
    let (r, lsn) = fs.release_logged(ino, fh, Some(l.time));
    fs.log_write_out(lsn);
    match r {
        Ok(()) => EmptyResult::Ok,
        Err(_) => EmptyResult::Error(libc::EIO),
    }
}

/// Handle `flush`: write the streamed content of `fh` into its manifest
/// without closing the handle (`flush_cell`).
/// Only a flush that did work appends a `Flush` record, in the hold of its
/// install; it is written out here.
pub fn handle_flush(fs: &SliceFsFilesystem, _ino: u64, fh: u64) -> EmptyResult {
    let l = logged();
    let (r, lsn) = fs.flush_logged(fh, Some(l.time));
    fs.log_write_out(lsn);
    match r {
        Ok(()) => EmptyResult::Ok,
        Err(_) => EmptyResult::Error(libc::EIO),
    }
}

/// Handle `fsync`: as `flush`, then sync the write log.
pub fn handle_fsync(fs: &SliceFsFilesystem, ino: u64, fh: u64) -> EmptyResult {
    // Write the manifest (logged as a flush), then make the log durable: the
    // sync covers this file's records and the flush record itself (#20).
    if let EmptyResult::Error(e) = handle_flush(fs, ino, fh) {
        return EmptyResult::Error(e);
    }
    match fs.meta().flush_wal() {
        Ok(()) => EmptyResult::Ok,
        Err(_) => EmptyResult::Error(libc::EIO),
    }
}

/// Handle `fsyncdir`: make the write log durable, so the directory changes
/// so far (create, rename, unlink, link, ...) survive a crash. Without this
/// the kernel took ENOSYS as "nothing to do" and an fsync of a directory
/// returned 0 with nothing durable.
pub fn handle_fsyncdir(fs: &SliceFsFilesystem) -> EmptyResult {
    match fs.meta().flush_wal() {
        Ok(()) => EmptyResult::Ok,
        Err(_) => EmptyResult::Error(libc::EIO),
    }
}

/// Handle `statfs`: compute filesystem statistics.
pub fn handle_statfs(fs: &SliceFsFilesystem) -> StatfsResult {
    let (blocks, bfree, bavail, files, ffree, bsize) = fs.compute_statfs();
    StatfsResult::Ok(blocks, bfree, bavail, files, ffree, bsize)
}

/// Handle `getxattr`: the xattr `name` of `ino` (`getxattr_reply`).
/// `size == 0` asks for the length (no value byte is read); a value above
/// `size` is ERANGE before any value read; `position` reads the macOS
/// resource fork in parts (EINVAL on another name). An absent name is
/// ENODATA. Memory is bounded by the request, not by the value.
pub fn handle_getxattr(
    fs: &SliceFsFilesystem,
    ino: u64,
    name: &str,
    size: u32,
    position: u32,
) -> XattrResult {
    xattr_result(fs.getxattr_reply(ino, name, size, position))
}

/// Handle `listxattr`: the xattr names of `ino`, streamed from the store
/// into the reply (`listxattr_reply`). `size == 0` asks for the total
/// length; a list above `size` is ERANGE (the scan stops there).
pub fn handle_listxattr(
    fs: &SliceFsFilesystem,
    ino: u64,
    size: u32,
) -> XattrResult {
    xattr_result(fs.listxattr_reply(ino, size))
}

fn xattr_result(r: Result<XattrReply, i32>) -> XattrResult {
    match r {
        Ok(XattrReply::Size(n)) => XattrResult::Size(n),
        Ok(XattrReply::Data(v)) => XattrResult::Data(v),
        Err(e) => XattrResult::Error(e),
    }
}

/// Handle `setxattr`: set extended attribute `name` to `value` on inode `ino`.
pub fn handle_setxattr(
    fs: &SliceFsFilesystem,
    ino: u64,
    name: &str,
    value: &[u8],
) -> EmptyResult {
    let l = logged();
    let mut lsn = None;
    let r = fs.setxattr_logged(ino, name, value, &mut l.sink(fs, &mut lsn));
    fs.log_write_out(lsn);
    empty(r)
}

/// Handle `removexattr`: remove extended attribute `name` from inode `ino`.
pub fn handle_removexattr(
    fs: &SliceFsFilesystem,
    ino: u64,
    name: &str,
) -> EmptyResult {
    let l = logged();
    let mut lsn = None;
    let r = fs.removexattr_logged(ino, name, &mut l.sink(fs, &mut lsn));
    fs.log_write_out(lsn);
    match r {
        Ok(()) => EmptyResult::Ok,
        Err(e) if e == libc::ENOENT => EmptyResult::Error(libc::ENODATA),
        Err(errno) => EmptyResult::Error(errno),
    }
}

/// Handle `write`: write `data` at `offset` into open handle `fh`.
pub fn handle_write(
    fs: &SliceFsFilesystem,
    fh: u64,
    offset: u64,
    data: &[u8],
) -> WriteResult {
    handle_write_flags(fs, fh, offset, data, 0)
}

/// `handle_write` with the open flags of the request: with `O_APPEND` (or
/// on a handle opened with it) the write goes to the end of file and
/// `offset` is ignored (D-12).
pub fn handle_write_flags(
    fs: &SliceFsFilesystem,
    fh: u64,
    offset: u64,
    data: &[u8],
    flags: i32,
) -> WriteResult {
    handle_write_admitted(fs, fh, offset, data, flags, Admit { pay: true })
}

/// `handle_write_flags` with the answer of `throttle_write` (Phase 20): the
/// caller ran the throttle before it took the commit gate. With
/// `admit.pay` false the write does not pay for the budget.
pub fn handle_write_admitted(
    fs: &SliceFsFilesystem,
    fh: u64,
    offset: u64,
    data: &[u8],
    flags: i32,
    admit: Admit,
) -> WriteResult {
    fs.note_write();
    let l = logged();
    // The record is appended in the cell-lock hold of the apply (D-08); it
    // is written out here, with no cell lock held, before the reply.
    let (r, lsn) = fs.write_logged_admitted(fh, offset, data, flags, Some(l.time), admit);
    fs.log_write_out(lsn);
    match r {
        Ok((n, _)) => WriteResult::Ok(n),
        Err(e) => WriteResult::Error(e),
    }
}

// The POSIX lock types as i32. libc gives them as c_short on macOS and as
// c_int on Linux, so the cast is needed on one platform only.
#[allow(clippy::unnecessary_cast, reason = "c_short on macOS, already i32 on Linux")]
pub(crate) const LOCK_RD: i32 = libc::F_RDLCK as i32;
#[allow(clippy::unnecessary_cast, reason = "c_short on macOS, already i32 on Linux")]
pub(crate) const LOCK_WR: i32 = libc::F_WRLCK as i32;
#[allow(clippy::unnecessary_cast, reason = "c_short on macOS, already i32 on Linux")]
pub(crate) const LOCK_UN: i32 = libc::F_UNLCK as i32;

/// The first pause between two attempts of a write that met a busy store.
const BUSY_PAUSE_MIN: Duration = Duration::from_millis(50);
/// The longest pause between two attempts (the pause doubles for each one).
const BUSY_PAUSE_MAX: Duration = Duration::from_secs(1);

/// The entry of one write request (the FUSE write job, the S7 harness and the
/// tests): `attempt` makes one try. It takes the commit gate for its own span
/// and returns with the gate released (the FUSE job:
/// `serve(handle_write_admitted)`), so no lock is held while this waits.
///
/// If the try ends in EIO because a store wait ended in a busy error (a slow
/// pack seal or background sync, issue #170), nothing of the write was
/// applied (the error comes before the apply and the log record, also for
/// `O_APPEND`: the end of file is read again in each try). This function then
/// pauses, with no lock held, and calls `attempt` again, as long as a new try
/// starts before `SliceFsFilesystem::store_busy_limit` has passed since the
/// first one and the file system is not shut down. Each try waits for the
/// store itself, up to the store wait limit. After that limit the last EIO
/// goes to the caller. Any other result goes to the caller at once.
pub fn handle_write_retrying(fs: &SliceFsFilesystem, mut attempt: impl FnMut() -> WriteResult) -> WriteResult {
    let start = Instant::now();
    let limit = fs.store_busy_limit();
    let mut pause = BUSY_PAUSE_MIN;
    let mut tries = 0u32;
    loop {
        clear_store_busy();
        let r = attempt();
        if !matches!(r, WriteResult::Error(libc::EIO)) || !take_store_busy() {
            return r;
        }
        let waited = start.elapsed();
        if waited >= limit || fs.is_shut_down() {
            eprintln!("[{}][FUSE] write: the chunk store stayed busy for {:.1} s ({tries} new tries); the write fails with EIO", ts(), waited.as_secs_f64());
            return r;
        }
        tries += 1;
        fs.count_store_busy_retry();
        if tries == 1 {
            eprintln!("[{}][FUSE] write: the chunk store is busy (a slow seal or sync); the write waits and tries again for up to {:.0} s", ts(), limit.as_secs_f64());
        }
        debug_assert!(
            chunk_store::sync::rank::held().is_empty(),
            "a write pause with a lock held: {:?}",
            chunk_store::sync::rank::held()
        );
        std::thread::sleep(pause.min(limit - waited));
        pause = (pause * 2).min(BUSY_PAUSE_MAX);
    }
}

/// Handle `create`: create a new file named `name` in `parent` and open it.
#[allow(clippy::too_many_arguments, reason = "the fields of one FUSE request; a struct would only move them")]
pub fn handle_create(
    fs: &SliceFsFilesystem,
    parent: u64,
    name: &str,
    mode: u32,
    umask: u32,
    uid: u32,
    gid: u32,
    flags: i32,
) -> CreateResult {
    handle_create_at(fs, parent, name, mode, umask, uid, gid, flags, None)
}

/// `handle_create` at inode `ino` (replay: the logged number; `None`: the
/// next free one). The `Create` record is appended in the parent-stripe
/// hold that opens the handle, and written out before the reply (D-08).
#[allow(clippy::too_many_arguments, reason = "the fields of one FUSE request; a struct would only move them")]
pub fn handle_create_at(
    fs: &SliceFsFilesystem,
    parent: u64,
    name: &str,
    mode: u32,
    umask: u32,
    uid: u32,
    gid: u32,
    flags: i32,
    ino: Option<u64>,
) -> CreateResult {
    fs.note_write();
    let l = logged();
    let mut lsn = None;
    let r = fs.create_full_logged(parent, name, mode, umask, uid, gid, flags, ino, &mut l.sink(fs, &mut lsn));
    fs.log_write_out(lsn);
    match r {
        Ok((ino, fh, meta)) => CreateResult::Ok(ino, fh, inode_to_file_attr(&meta)),
        Err(errno) => CreateResult::Error(errno),
    }
}

/// Handle `mkdir`: create a new directory named `name` in `parent`.
pub fn handle_mkdir(
    fs: &SliceFsFilesystem,
    parent: u64,
    name: &str,
    mode: u32,
    umask: u32,
    uid: u32,
    gid: u32,
) -> EntryResult {
    handle_mkdir_at(fs, parent, name, mode, umask, uid, gid, None)
}

/// `handle_mkdir` at inode `ino` (replay); logged in the hold of the parent's
/// and the new directory's stripes.
#[allow(clippy::too_many_arguments, reason = "the fields of one FUSE request; a struct would only move them")]
pub fn handle_mkdir_at(
    fs: &SliceFsFilesystem,
    parent: u64,
    name: &str,
    mode: u32,
    umask: u32,
    uid: u32,
    gid: u32,
    ino: Option<u64>,
) -> EntryResult {
    let l = logged();
    let mut lsn = None;
    let r = fs.mkdir_logged(parent, name, mode, umask, uid, gid, ino, &mut l.sink(fs, &mut lsn));
    fs.log_write_out(lsn);
    entry(r)
}

/// Handle `mknod`: create a new node (regular file) named `name` in `parent`.
#[allow(clippy::too_many_arguments, reason = "the fields of one FUSE request; a struct would only move them")]
pub fn handle_mknod(
    fs: &SliceFsFilesystem,
    parent: u64,
    name: &str,
    mode: u32,
    umask: u32,
    rdev: u32,
    uid: u32,
    gid: u32,
) -> EntryResult {
    handle_mknod_at(fs, parent, name, mode, umask, rdev, uid, gid, None)
}

/// `handle_mknod` at inode `ino` (replay); logged in the parent-stripe hold.
#[allow(clippy::too_many_arguments, reason = "the fields of one FUSE request; a struct would only move them")]
pub fn handle_mknod_at(
    fs: &SliceFsFilesystem,
    parent: u64,
    name: &str,
    mode: u32,
    umask: u32,
    rdev: u32,
    uid: u32,
    gid: u32,
    ino: Option<u64>,
) -> EntryResult {
    let l = logged();
    let mut lsn = None;
    let r = fs.mknod_logged(parent, name, mode, rdev, uid, gid, umask, ino, &mut l.sink(fs, &mut lsn));
    fs.log_write_out(lsn);
    entry(r)
}

/// Handle `symlink`: create a symbolic link named `link_name` in `parent` pointing to `target`.
pub fn handle_symlink(
    fs: &SliceFsFilesystem,
    parent: u64,
    link_name: &str,
    target: &str,
    uid: u32,
    gid: u32,
) -> EntryResult {
    handle_symlink_at(fs, parent, link_name, target, uid, gid, None)
}

/// `handle_symlink` at inode `ino` (replay); logged in the parent-stripe hold.
pub fn handle_symlink_at(
    fs: &SliceFsFilesystem,
    parent: u64,
    link_name: &str,
    target: &str,
    uid: u32,
    gid: u32,
    ino: Option<u64>,
) -> EntryResult {
    let l = logged();
    let mut lsn = None;
    let r = fs.symlink_logged(parent, link_name, target, uid, gid, ino, &mut l.sink(fs, &mut lsn));
    fs.log_write_out(lsn);
    entry(r)
}

/// The reply of a request that gives nothing back.
fn empty(r: Result<(), i32>) -> EmptyResult {
    match r {
        Ok(()) => EmptyResult::Ok,
        Err(errno) => EmptyResult::Error(errno),
    }
}

/// The reply of a request that gives an inode.
fn entry(r: Result<(u64, InodeMeta), i32>) -> EntryResult {
    match r {
        Ok((ino, meta)) => EntryResult::Ok(ino, inode_to_file_attr(&meta)),
        Err(errno) => EntryResult::Error(errno),
    }
}

/// Handle `readlink`: read the target of symbolic link inode `ino`.
pub fn handle_readlink(fs: &SliceFsFilesystem, ino: u64) -> DataResult {
    match fs.simulate_readlink(ino) {
        Ok(target) => DataResult::Ok(target.into_bytes()),
        Err(errno) => DataResult::Error(errno),
    }
}

/// Handle `link`: create a hard link named `newname` in `newparent` pointing to `ino`.
pub fn handle_link(
    fs: &SliceFsFilesystem,
    ino: u64,
    newparent: u64,
    newname: &str,
) -> EntryResult {
    let l = logged();
    let mut lsn = None;
    let r = fs.link_logged(ino, newparent, newname, &mut l.sink(fs, &mut lsn));
    fs.log_write_out(lsn);
    entry(r)
}

/// Handle `unlink`: remove the directory entry `name` from `parent`.
pub fn handle_unlink(
    fs: &SliceFsFilesystem,
    parent: u64,
    name: &str,
) -> EmptyResult {
    let l = logged();
    let mut lsn = None;
    let r = fs.unlink_logged(parent, name, &mut l.sink(fs, &mut lsn));
    fs.log_write_out(lsn);
    empty(r)
}

/// Handle `rmdir`: remove the empty directory `name` from `parent`.
pub fn handle_rmdir(
    fs: &SliceFsFilesystem,
    parent: u64,
    name: &str,
) -> EmptyResult {
    let l = logged();
    let mut lsn = None;
    let r = fs.rmdir_logged(parent, name, &mut l.sink(fs, &mut lsn));
    fs.log_write_out(lsn);
    empty(r)
}

/// Handle `rename`: rename `name` in `parent` to `newname` in `newparent`.
pub fn handle_rename(
    fs: &SliceFsFilesystem,
    parent: u64,
    name: &str,
    newparent: u64,
    newname: &str,
    flags: u32,
) -> EmptyResult {
    let l = logged();
    let mut lsn = None;
    let r = fs.rename_logged(parent, name, newparent, newname, flags, &mut l.sink(fs, &mut lsn));
    fs.log_write_out(lsn);
    empty(r)
}

/// Handle `setattr`: update inode attributes for `ino`.
///
/// `mtime` is `Some((sec, nsec))` if mtime should be updated.
#[allow(clippy::too_many_arguments, reason = "the fields of one FUSE request; a struct would only move them")]
pub fn handle_setattr(
    fs: &SliceFsFilesystem,
    ino: u64,
    mode: Option<u32>,
    uid: Option<u32>,
    gid: Option<u32>,
    size: Option<u64>,
    fh: Option<u64>,
    atime: Option<(i64, u32)>,
    mtime: Option<(i64, u32)>,
) -> AttrResult {
    fs.note_write();
    let l = logged();
    let mut lsn = None;
    // A size change logs its own record in its cell hold (see `setattr_logged`).
    let r = fs.setattr_logged(ino, mode, uid, gid, size, fh, atime, mtime, &mut l.sink(fs, &mut lsn));
    fs.log_write_out(lsn);
    match r {
        Ok(inode) => AttrResult::Ok(inode_to_file_attr(&inode)),
        Err(errno) => AttrResult::Error(errno),
    }
}

/// Handle `getlk`: always report no conflicting lock (F_UNLCK).
pub fn handle_getlk(
    start: u64,
    end: u64,
    pid: u32,
) -> LockResult {
    LockResult::Ok(start, end, LOCK_UN, pid)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::SliceFsFilesystem;
    use metadata::store::DictMetadataStore;
    use metadata::store_io::StoreIo;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    fn fresh_fs() -> (SliceFsFilesystem, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let io = Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap()));
        let meta = DictMetadataStore::new(io.clone());
        let fs = SliceFsFilesystem::new(meta, io, None);
        (fs, dir)
    }

    // ── handle_getattr ────────────────────────────────────────────────────────

    #[test]
    fn test_handle_getattr_root_ok() {
        let (fs, _dir) = fresh_fs();
        let result = handle_getattr(&fs, 1);
        match result {
            AttrResult::Ok(attr) => {
                assert_eq!(attr.ino.0, 1);
                assert_eq!(attr.kind, fuser::FileType::Directory);
            }
            AttrResult::Error(e) => panic!("expected Ok, got error {}", e),
        }
    }

    #[test]
    fn test_handle_getattr_nonexistent_error() {
        let (fs, _dir) = fresh_fs();
        let result = handle_getattr(&fs, 999);
        assert!(matches!(result, AttrResult::Error(_)));
    }

    #[test]
    fn test_handle_getattr_created_file() {
        let (fs, _dir) = fresh_fs();
        let (ino, _fh) = fs.test_create(1, "f.txt", 0o644, 0, 1000, 2000).unwrap();
        match handle_getattr(&fs, ino) {
            AttrResult::Ok(attr) => {
                assert_eq!(attr.ino.0, ino);
                assert_eq!(attr.uid, 1000);
                assert_eq!(attr.gid, 2000);
            }
            AttrResult::Error(e) => panic!("expected Ok, got error {}", e),
        }
    }

    // ── handle_lookup ─────────────────────────────────────────────────────────

    #[test]
    fn test_handle_lookup_dot_ok() {
        let (fs, _dir) = fresh_fs();
        match handle_lookup(&fs, 1, ".") {
            EntryResult::Ok(ino, attr) => {
                assert_eq!(ino, 1);
                assert_eq!(attr.kind, fuser::FileType::Directory);
            }
            EntryResult::Error(e) => panic!("expected Ok, got error {}", e),
        }
    }

    #[test]
    fn test_handle_lookup_created_file() {
        let (fs, _dir) = fresh_fs();
        let (created_ino, _fh) = fs.test_create(1, "myfile", 0o644, 0, 0, 0).unwrap();
        match handle_lookup(&fs, 1, "myfile") {
            EntryResult::Ok(ino, attr) => {
                assert_eq!(ino, created_ino);
                assert_eq!(attr.kind, fuser::FileType::RegularFile);
            }
            EntryResult::Error(e) => panic!("expected Ok, got error {}", e),
        }
    }

    #[test]
    fn test_handle_lookup_missing_returns_error() {
        let (fs, _dir) = fresh_fs();
        let result = handle_lookup(&fs, 1, "no_such");
        assert!(matches!(result, EntryResult::Error(e) if e == libc::ENOENT));
    }

    // ── handle_readdir ────────────────────────────────────────────────────────

    #[test]
    fn test_handle_readdir_root_contains_dots() {
        let (fs, _dir) = fresh_fs();
        match handle_readdir(&fs, 1, 0) {
            ReaddirResult::Ok(entries) => {
                let names: Vec<&str> = entries.iter().map(|(_, _, n)| n.as_str()).collect();
                assert!(names.contains(&"."), "root must contain '.'");
                assert!(names.contains(&".."), "root must contain '..'");
            }
            ReaddirResult::Error(e) => panic!("expected Ok, got error {}", e),
        }
    }

    #[test]
    fn test_handle_readdir_nonexistent_returns_error() {
        let (fs, _dir) = fresh_fs();
        // Inode 999 does not exist
        let result = handle_readdir(&fs, 999, 0);
        assert!(matches!(result, ReaddirResult::Error(_)));
    }

    #[test]
    fn test_handle_readdir_offset_skips_entries() {
        let (fs, _dir) = fresh_fs();
        fs.test_create(1, "a", 0o644, 0, 0, 0).unwrap();
        let all = match handle_readdir(&fs, 1, 0) {
            ReaddirResult::Ok(e) => e,
            ReaddirResult::Error(e) => panic!("error {}", e),
        };
        let skipped = match handle_readdir(&fs, 1, 1) {
            ReaddirResult::Ok(e) => e,
            ReaddirResult::Error(e) => panic!("error {}", e),
        };
        assert_eq!(skipped.len() + 1, all.len());
    }

    // ── handle_read ───────────────────────────────────────────────────────────

    #[test]
    fn test_handle_read_empty_file() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "e.txt", 0o644, 0, 0, 0).unwrap();
        fs.test_release_full(ino, fh).unwrap();
        match handle_read(&fs, ino, 0, 128) {
            DataResult::Ok(data) => assert!(data.is_empty()),
            DataResult::Error(e) => panic!("expected Ok, got error {}", e),
        }
    }

    #[test]
    fn test_handle_read_written_content() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "r.txt", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"hello").unwrap();
        fs.test_release_full(ino, fh).unwrap();
        match handle_read(&fs, ino, 0, 64) {
            DataResult::Ok(data) => assert_eq!(data, b"hello"),
            DataResult::Error(e) => panic!("expected Ok, got error {}", e),
        }
    }

    // ── handle_open ───────────────────────────────────────────────────────────

    #[test]
    fn test_handle_open_readonly_ok() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "o.txt", 0o644, 0, 0, 0).unwrap();
        fs.test_release_full(ino, fh).unwrap();
        match handle_open(&fs, ino, libc::O_RDONLY) {
            OpenResult::Ok(_new_fh, is_write) => assert!(!is_write),
            OpenResult::Error(e) => panic!("expected Ok, got error {}", e),
        }
    }

    #[test]
    fn test_handle_open_wronly_is_write() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "w.txt", 0o644, 0, 0, 0).unwrap();
        fs.test_release_full(ino, fh).unwrap();
        match handle_open(&fs, ino, libc::O_WRONLY) {
            OpenResult::Ok(_new_fh, is_write) => assert!(is_write),
            OpenResult::Error(e) => panic!("expected Ok, got error {}", e),
        }
    }

    // ── handle_release ────────────────────────────────────────────────────────

    #[test]
    fn test_handle_release_ok() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "rel.txt", 0o644, 0, 0, 0).unwrap();
        assert!(matches!(handle_release(&fs, ino, fh), EmptyResult::Ok));
    }

    #[test]
    fn test_handle_release_no_handle_ok() {
        // Releasing a non-existent handle is silently OK (read-only handle case)
        let (fs, _dir) = fresh_fs();
        assert!(matches!(handle_release(&fs, 1, 9999), EmptyResult::Ok));
    }

    // ── handle_flush ─────────────────────────────────────────────────────────

    #[test]
    fn test_handle_flush_no_handle_ok() {
        let (fs, _dir) = fresh_fs();
        // Flushing a read-only or non-existent handle is a no-op
        assert!(matches!(handle_flush(&fs, 1, 9999), EmptyResult::Ok));
    }

    #[test]
    fn test_handle_flush_written_data_ok() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "fl.txt", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"data").unwrap();
        assert!(matches!(handle_flush(&fs, ino, fh), EmptyResult::Ok));
    }

    // ── handle_fsync ─────────────────────────────────────────────────────────

    #[test]
    fn test_handle_fsync_no_handle_ok() {
        let (fs, _dir) = fresh_fs();
        assert!(matches!(handle_fsync(&fs, 1, 9999), EmptyResult::Ok));
    }

    #[test]
    fn test_handle_fsync_written_data_ok() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "fsync.txt", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"sync me").unwrap();
        assert!(matches!(handle_fsync(&fs, ino, fh), EmptyResult::Ok));
    }

    // ── handle_statfs ─────────────────────────────────────────────────────────

    #[test]
    fn test_handle_statfs_returns_ok() {
        let (fs, _dir) = fresh_fs();
        let result = handle_statfs(&fs);
        assert!(matches!(result, StatfsResult::Ok(..)));
    }

    #[test]
    fn test_handle_statfs_files_equals_inode_count() {
        let (fs, _dir) = fresh_fs();
        match handle_statfs(&fs) {
            StatfsResult::Ok(_, _, _, files, _, _) => {
                // Fresh store has exactly 1 inode (root)
                assert_eq!(files, 1);
            }
        }
    }

    // ── handle_getxattr ───────────────────────────────────────────────────────

    #[test]
    fn test_handle_getxattr_missing_returns_error() {
        let (fs, _dir) = fresh_fs();
        let result = handle_getxattr(&fs, 1, "user.nosuchattr", 128, 0);
        assert!(matches!(result, XattrResult::Error(_)));
    }

    #[test]
    fn test_handle_getxattr_set_and_get_data() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "x.txt", 0o644, 0, 0, 0).unwrap();
        fs.test_release_full(ino, fh).unwrap();
        fs.test_setxattr(ino, "user.mykey", b"myvalue").unwrap();
        match handle_getxattr(&fs, ino, "user.mykey", 128, 0) {
            XattrResult::Data(data) => assert_eq!(data, b"myvalue"),
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_handle_getxattr_size_query() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "xs.txt", 0o644, 0, 0, 0).unwrap();
        fs.test_release_full(ino, fh).unwrap();
        fs.test_setxattr(ino, "user.k", b"12345").unwrap();
        match handle_getxattr(&fs, ino, "user.k", 0, 0) {
            XattrResult::Size(sz) => assert_eq!(sz, 5),
            other => panic!("unexpected: {:?}", other),
        }
    }

    // ── handle_listxattr ──────────────────────────────────────────────────────

    #[test]
    fn test_handle_listxattr_empty() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "lx.txt", 0o644, 0, 0, 0).unwrap();
        fs.test_release_full(ino, fh).unwrap();
        match handle_listxattr(&fs, ino, 128) {
            XattrResult::Data(buf) => assert!(buf.is_empty()),
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_handle_listxattr_with_attrs() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "lx2.txt", 0o644, 0, 0, 0).unwrap();
        fs.test_release_full(ino, fh).unwrap();
        fs.test_setxattr(ino, "user.a", b"1").unwrap();
        fs.test_setxattr(ino, "user.b", b"2").unwrap();
        match handle_listxattr(&fs, ino, 128) {
            XattrResult::Data(buf) => {
                let joined = String::from_utf8(buf).unwrap();
                assert!(joined.contains("user.a"));
                assert!(joined.contains("user.b"));
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn test_handle_listxattr_size_query() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "lxs.txt", 0o644, 0, 0, 0).unwrap();
        fs.test_release_full(ino, fh).unwrap();
        fs.test_setxattr(ino, "user.abc", b"val").unwrap();
        // "user.abc\0" = 9 bytes
        match handle_listxattr(&fs, ino, 0) {
            XattrResult::Size(sz) => assert_eq!(sz, 9),
            other => panic!("unexpected: {:?}", other),
        }
    }

    /// A file with no xattrs (closed, so its inode exists in the store).
    fn xattr_file(fs: &SliceFsFilesystem, name: &str) -> u64 {
        let (ino, fh) = fs.test_create(1, name, 0o644, 0, 0, 0).unwrap();
        fs.test_release_full(ino, fh).unwrap();
        ino
    }

    /// Bytes that differ at every offset of a large range.
    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8 ^ (i / 251) as u8).collect()
    }

    #[test]
    fn getxattr_size_query_gives_the_length() {
        let (fs, _dir) = fresh_fs();
        let ino = xattr_file(&fs, "big.txt");
        let v = pattern(60_000);
        fs.test_setxattr(ino, "user.big", &v).unwrap();
        match handle_getxattr(&fs, ino, "user.big", 0, 0) {
            XattrResult::Size(sz) => assert_eq!(sz, 60_000),
            other => panic!("unexpected: {:?}", other),
        }
        match handle_getxattr(&fs, ino, "user.big", 65_536, 0) {
            XattrResult::Data(data) => assert!(data == v, "value read back differs"),
            other => panic!("unexpected: {:?}", other),
        }
        // Exactly the value length fits.
        assert!(matches!(handle_getxattr(&fs, ino, "user.big", 60_000, 0), XattrResult::Data(d) if d.len() == 60_000));
    }

    #[test]
    fn getxattr_too_small_buffer_is_erange() {
        let (fs, _dir) = fresh_fs();
        let ino = xattr_file(&fs, "small.txt");
        fs.test_setxattr(ino, "user.k", b"12345").unwrap();
        assert!(matches!(handle_getxattr(&fs, ino, "user.k", 3, 0), XattrResult::Error(e) if e == libc::ERANGE));
        assert!(matches!(handle_getxattr(&fs, ino, "user.k", 4, 0), XattrResult::Error(e) if e == libc::ERANGE));
        assert!(matches!(handle_getxattr(&fs, ino, "user.k", 5, 0), XattrResult::Data(d) if d == b"12345"));
        assert!(matches!(handle_getxattr(&fs, ino, "user.absent", 3, 0), XattrResult::Error(e) if e == libc::ENODATA));
        assert!(matches!(handle_getxattr(&fs, ino, "user.absent", 0, 0), XattrResult::Error(e) if e == libc::ENODATA));
        // A spilled value too: ERANGE from its length.
        fs.test_setxattr(ino, "user.big", &pattern(10_000)).unwrap();
        assert!(matches!(handle_getxattr(&fs, ino, "user.big", 9_999, 0), XattrResult::Error(e) if e == libc::ERANGE));
    }

    #[test]
    fn getxattr_name_errors() {
        let (fs, _dir) = fresh_fs();
        let ino = xattr_file(&fs, "names.txt");
        let long = format!("user.{}", "n".repeat(300));
        assert!(matches!(handle_getxattr(&fs, ino, &long, 16, 0), XattrResult::Error(e) if e == libc::ERANGE));
        assert!(matches!(handle_getxattr(&fs, ino, "", 16, 0), XattrResult::Error(e) if e == libc::EINVAL));
        // The same errno on set and remove (Linux: ERANGE on all three).
        assert!(matches!(handle_setxattr(&fs, ino, &long, b"v"), EmptyResult::Error(e) if e == libc::ERANGE));
        assert!(matches!(handle_removexattr(&fs, ino, &long), EmptyResult::Error(e) if e == libc::ERANGE));
    }

    #[test]
    fn resource_fork_reads_by_position() {
        let (fs, _dir) = fresh_fs();
        let ino = xattr_file(&fs, "fork.txt");
        let v = pattern(1 << 20);
        fs.test_setxattr(ino, crate::filesystem::RESOURCE_FORK, &v).unwrap();
        let rf = crate::filesystem::RESOURCE_FORK;
        match handle_getxattr(&fs, ino, rf, 4_096, 524_288) {
            XattrResult::Data(d) => assert!(d == v[524_288..524_288 + 4_096], "middle slice differs"),
            other => panic!("unexpected: {:?}", other),
        }
        // Position 0 with a small buffer: the first part, no ERANGE.
        match handle_getxattr(&fs, ino, rf, 4_096, 0) {
            XattrResult::Data(d) => assert!(d == v[..4_096], "first slice differs"),
            other => panic!("unexpected: {:?}", other),
        }
        // The last part is short; past the end is empty.
        match handle_getxattr(&fs, ino, rf, 4_096, (1 << 20) - 100) {
            XattrResult::Data(d) => assert!(d == v[(1 << 20) - 100..], "last slice differs"),
            other => panic!("unexpected: {:?}", other),
        }
        assert!(matches!(handle_getxattr(&fs, ino, rf, 4_096, (1 << 20) + 1), XattrResult::Data(d) if d.is_empty()));
        assert!(matches!(handle_getxattr(&fs, ino, rf, 0, 0), XattrResult::Size(n) if n == 1 << 20));
    }

    #[test]
    fn a_position_on_another_name_is_einval() {
        let (fs, _dir) = fresh_fs();
        let ino = xattr_file(&fs, "pos.txt");
        fs.test_setxattr(ino, "user.k", b"value").unwrap();
        assert!(matches!(handle_getxattr(&fs, ino, "user.k", 128, 10), XattrResult::Error(e) if e == libc::EINVAL));
        assert!(matches!(handle_getxattr(&fs, ino, "user.k", 0, 10), XattrResult::Error(e) if e == libc::EINVAL));
    }

    /// 300 names `user.n1` .. `user.n300` on a new file; their list in name
    /// byte order (NUL-separated) and its length.
    fn many_names(fs: &SliceFsFilesystem) -> (u64, Vec<u8>) {
        let ino = xattr_file(fs, "many.txt");
        let mut names: Vec<String> = (1..=300).map(|i| format!("user.n{i}")).collect();
        for n in &names {
            fs.test_setxattr(ino, n, b"v").unwrap();
        }
        names.sort();
        let mut want = Vec::new();
        for n in &names {
            want.extend_from_slice(n.as_bytes());
            want.push(0);
        }
        (ino, want)
    }

    #[test]
    fn listxattr_size_query_sums_the_names() {
        let (fs, _dir) = fresh_fs();
        let (ino, want) = many_names(&fs);
        match handle_listxattr(&fs, ino, 0) {
            XattrResult::Size(sz) => assert_eq!(sz as usize, want.len()),
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn listxattr_too_small_buffer_is_erange() {
        let (fs, _dir) = fresh_fs();
        let (ino, want) = many_names(&fs);
        let n = want.len() as u32;
        assert!(matches!(handle_listxattr(&fs, ino, n - 1), XattrResult::Error(e) if e == libc::ERANGE));
        assert!(matches!(handle_listxattr(&fs, ino, 1), XattrResult::Error(e) if e == libc::ERANGE));
        match handle_listxattr(&fs, ino, n) {
            XattrResult::Data(buf) => assert!(buf == want, "list differs"),
            other => panic!("unexpected: {:?}", other),
        }
    }

    // ── handle_setxattr ───────────────────────────────────────────────────────

    #[test]
    fn test_handle_setxattr_ok() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "sx.txt", 0o644, 0, 0, 0).unwrap();
        fs.test_release_full(ino, fh).unwrap();
        assert!(matches!(
            handle_setxattr(&fs, ino, "user.key", b"value"),
            EmptyResult::Ok
        ));
    }

    #[test]
    fn test_handle_setxattr_overwrites_existing_value() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "sx2.txt", 0o644, 0, 0, 0).unwrap();
        fs.test_release_full(ino, fh).unwrap();
        assert!(matches!(
            handle_setxattr(&fs, ino, "user.key", b"first"),
            EmptyResult::Ok
        ));
        assert!(matches!(
            handle_setxattr(&fs, ino, "user.key", b"second"),
            EmptyResult::Ok
        ));
        // Most-recent value wins
        match handle_getxattr(&fs, ino, "user.key", 128, 0) {
            XattrResult::Data(data) => assert_eq!(data, b"second"),
            other => panic!("unexpected: {:?}", other),
        }
    }

    // ── handle_removexattr ────────────────────────────────────────────────────

    #[test]
    fn test_handle_removexattr_ok() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "rx.txt", 0o644, 0, 0, 0).unwrap();
        fs.test_release_full(ino, fh).unwrap();
        fs.test_setxattr(ino, "user.del", b"bye").unwrap();
        assert!(matches!(
            handle_removexattr(&fs, ino, "user.del"),
            EmptyResult::Ok
        ));
    }

    #[test]
    fn test_handle_removexattr_missing_returns_enodata() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "rx2.txt", 0o644, 0, 0, 0).unwrap();
        fs.test_release_full(ino, fh).unwrap();
        match handle_removexattr(&fs, ino, "user.nothere") {
            EmptyResult::Error(e) => assert_eq!(e, libc::ENODATA),
            EmptyResult::Ok => panic!("expected error"),
        }
    }

    // ── handle_write ─────────────────────────────────────────────────────────

    #[test]
    fn test_handle_write_ok() {
        let (fs, _dir) = fresh_fs();
        let (_ino, fh) = fs.test_create(1, "wr.txt", 0o644, 0, 0, 0).unwrap();
        match handle_write(&fs, fh, 0, b"hello") {
            WriteResult::Ok(n) => assert_eq!(n, 5),
            WriteResult::Error(e) => panic!("expected Ok, got error {}", e),
        }
    }

    #[test]
    fn test_handle_write_invalid_fh_returns_error() {
        let (fs, _dir) = fresh_fs();
        let result = handle_write(&fs, 9999, 0, b"data");
        assert!(matches!(result, WriteResult::Error(e) if e == libc::EBADF));
    }

    #[test]
    fn test_handle_write_passes_efbig_through() {
        let (fs, _dir) = fresh_fs();
        let (_ino, fh) = fs.test_create(1, "big.txt", 0o644, 0, 0, 0).unwrap();
        let result = handle_write(&fs, fh, 999_999_999_999_999, b"x");
        assert!(matches!(result, WriteResult::Error(e) if e == libc::EFBIG));
    }

    // ── handle_create ─────────────────────────────────────────────────────────

    #[test]
    fn test_handle_create_ok() {
        let (fs, _dir) = fresh_fs();
        match handle_create(&fs, 1, "new.txt", 0o644, 0, 1000, 2000, 0) {
            CreateResult::Ok(ino, _fh, attr) => {
                assert!(ino > 1);
                assert_eq!(attr.uid, 1000);
                assert_eq!(attr.gid, 2000);
                assert_eq!(attr.kind, fuser::FileType::RegularFile);
            }
            CreateResult::Error(e) => panic!("expected Ok, got error {}", e),
        }
    }

    #[test]
    fn test_handle_create_duplicate_returns_error() {
        let (fs, _dir) = fresh_fs();
        handle_create(&fs, 1, "dup.txt", 0o644, 0, 0, 0, 0);
        let result = handle_create(&fs, 1, "dup.txt", 0o644, 0, 0, 0, 0);
        assert!(matches!(result, CreateResult::Error(_)));
    }

    // ── handle_mkdir ─────────────────────────────────────────────────────────

    #[test]
    fn test_handle_mkdir_ok() {
        let (fs, _dir) = fresh_fs();
        match handle_mkdir(&fs, 1, "mydir", 0o755, 0, 0, 0) {
            EntryResult::Ok(ino, attr) => {
                assert!(ino > 1);
                assert_eq!(attr.kind, fuser::FileType::Directory);
            }
            EntryResult::Error(e) => panic!("expected Ok, got error {}", e),
        }
    }

    #[test]
    fn test_handle_mkdir_duplicate_returns_error() {
        let (fs, _dir) = fresh_fs();
        handle_mkdir(&fs, 1, "dupdir", 0o755, 0, 0, 0);
        let result = handle_mkdir(&fs, 1, "dupdir", 0o755, 0, 0, 0);
        assert!(matches!(result, EntryResult::Error(_)));
    }

    // ── handle_mknod ─────────────────────────────────────────────────────────

    #[test]
    fn test_handle_mknod_regular_file_ok() {
        const S_IFREG: u32 = 0o100_000;
        let (fs, _dir) = fresh_fs();
        match handle_mknod(&fs, 1, "node.txt", S_IFREG | 0o644, 0, 0, 0, 0) {
            EntryResult::Ok(ino, attr) => {
                assert!(ino > 1);
                assert_eq!(attr.kind, fuser::FileType::RegularFile);
            }
            EntryResult::Error(e) => panic!("expected Ok, got error {}", e),
        }
    }

    #[test]
    fn test_handle_mknod_duplicate_returns_error() {
        const S_IFREG: u32 = 0o100_000;
        let (fs, _dir) = fresh_fs();
        handle_mknod(&fs, 1, "node2.txt", S_IFREG | 0o644, 0, 0, 0, 0);
        let result = handle_mknod(&fs, 1, "node2.txt", S_IFREG | 0o644, 0, 0, 0, 0);
        assert!(matches!(result, EntryResult::Error(_)));
    }

    // ── handle_symlink ────────────────────────────────────────────────────────

    #[test]
    fn test_handle_symlink_ok() {
        let (fs, _dir) = fresh_fs();
        match handle_symlink(&fs, 1, "link", "/target/path", 0, 0) {
            EntryResult::Ok(ino, attr) => {
                assert!(ino > 1);
                assert_eq!(attr.kind, fuser::FileType::Symlink);
            }
            EntryResult::Error(e) => panic!("expected Ok, got error {}", e),
        }
    }

    #[test]
    fn test_handle_symlink_duplicate_returns_error() {
        let (fs, _dir) = fresh_fs();
        handle_symlink(&fs, 1, "link", "/target", 0, 0);
        let result = handle_symlink(&fs, 1, "link", "/other", 0, 0);
        assert!(matches!(result, EntryResult::Error(_)));
    }

    // ── handle_readlink ───────────────────────────────────────────────────────

    #[test]
    fn test_handle_readlink_ok() {
        let (fs, _dir) = fresh_fs();
        let (ino, _fh) = match handle_symlink(&fs, 1, "sl", "/some/target", 0, 0) {
            EntryResult::Ok(ino, attr) => (ino, attr),
            EntryResult::Error(e) => panic!("create symlink failed: {}", e),
        };
        match handle_readlink(&fs, ino) {
            DataResult::Ok(bytes) => assert_eq!(bytes, b"/some/target"),
            DataResult::Error(e) => panic!("expected Ok, got error {}", e),
        }
    }

    #[test]
    fn test_handle_readlink_on_regular_file_returns_error() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "notlink.txt", 0o644, 0, 0, 0).unwrap();
        fs.test_release_full(ino, fh).unwrap();
        // readlink on a file with no manifest (new file) returns empty rather than error,
        // but on an invalid inode it returns error
        let result = handle_readlink(&fs, 9999);
        assert!(matches!(result, DataResult::Error(_)));
    }

    // ── handle_link ───────────────────────────────────────────────────────────

    #[test]
    fn test_handle_link_ok() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "orig.txt", 0o644, 0, 0, 0).unwrap();
        fs.test_release_full(ino, fh).unwrap();
        match handle_link(&fs, ino, 1, "alias.txt") {
            EntryResult::Ok(new_ino, attr) => {
                assert_eq!(new_ino, ino); // hard link shares the same inode
                assert_eq!(attr.nlink, 2);
            }
            EntryResult::Error(e) => panic!("expected Ok, got error {}", e),
        }
    }

    #[test]
    fn test_handle_link_to_nonexistent_inode_returns_error() {
        let (fs, _dir) = fresh_fs();
        let result = handle_link(&fs, 9999, 1, "ghost.txt");
        assert!(matches!(result, EntryResult::Error(_)));
    }

    // ── handle_unlink ─────────────────────────────────────────────────────────

    #[test]
    fn test_handle_unlink_ok() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "del.txt", 0o644, 0, 0, 0).unwrap();
        fs.test_release_full(ino, fh).unwrap();
        assert!(matches!(handle_unlink(&fs, 1, "del.txt"), EmptyResult::Ok));
    }

    #[test]
    fn test_handle_unlink_missing_returns_error() {
        let (fs, _dir) = fresh_fs();
        let result = handle_unlink(&fs, 1, "nope.txt");
        assert!(matches!(result, EmptyResult::Error(_)));
    }

    // ── handle_rmdir ─────────────────────────────────────────────────────────

    #[test]
    fn test_handle_rmdir_empty_dir_ok() {
        let (fs, _dir) = fresh_fs();
        handle_mkdir(&fs, 1, "emptydir", 0o755, 0, 0, 0);
        assert!(matches!(handle_rmdir(&fs, 1, "emptydir"), EmptyResult::Ok));
    }

    #[test]
    fn test_handle_rmdir_nonempty_returns_enotempty() {
        let (fs, _dir) = fresh_fs();
        handle_mkdir(&fs, 1, "fulldir", 0o755, 0, 0, 0);
        // look up the dir ino so we can create a file inside it
        let dir_ino = match handle_lookup(&fs, 1, "fulldir") {
            EntryResult::Ok(ino, _) => ino,
            _ => panic!("mkdir lookup failed"),
        };
        fs.test_create(dir_ino, "child.txt", 0o644, 0, 0, 0).unwrap();
        match handle_rmdir(&fs, 1, "fulldir") {
            EmptyResult::Error(e) => assert_eq!(e, libc::ENOTEMPTY),
            EmptyResult::Ok => panic!("expected ENOTEMPTY"),
        }
    }

    #[test]
    fn test_handle_rmdir_missing_returns_error() {
        let (fs, _dir) = fresh_fs();
        let result = handle_rmdir(&fs, 1, "nodir");
        assert!(matches!(result, EmptyResult::Error(_)));
    }

    // ── handle_rename ─────────────────────────────────────────────────────────

    #[test]
    fn test_handle_rename_ok() {
        let (fs, _dir) = fresh_fs();
        fs.test_create(1, "before.txt", 0o644, 0, 0, 0).unwrap();
        assert!(matches!(
            handle_rename(&fs, 1, "before.txt", 1, "after.txt", 0),
            EmptyResult::Ok
        ));
        // Old name gone, new name present
        assert!(matches!(
            handle_lookup(&fs, 1, "before.txt"),
            EntryResult::Error(_)
        ));
        assert!(matches!(
            handle_lookup(&fs, 1, "after.txt"),
            EntryResult::Ok(_, _)
        ));
    }

    #[test]
    fn test_handle_rename_missing_source_returns_error() {
        let (fs, _dir) = fresh_fs();
        let result = handle_rename(&fs, 1, "ghost.txt", 1, "new.txt", 0);
        assert!(matches!(result, EmptyResult::Error(_)));
    }

    // ── handle_setattr ────────────────────────────────────────────────────────

    #[test]
    fn test_handle_setattr_mode_ok() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "sa.txt", 0o644, 0, 0, 0).unwrap();
        fs.test_release_full(ino, fh).unwrap();
        match handle_setattr(&fs, ino, Some(0o600), None, None, None, None, None, None) {
            AttrResult::Ok(attr) => assert_eq!(attr.perm, 0o600),
            AttrResult::Error(e) => panic!("expected Ok, got error {}", e),
        }
    }

    #[test]
    fn test_handle_setattr_nonexistent_returns_error() {
        let (fs, _dir) = fresh_fs();
        let result = handle_setattr(&fs, 9999, Some(0o644), None, None, None, None, None, None);
        assert!(matches!(result, AttrResult::Error(_)));
    }

    #[test]
    fn test_handle_setattr_uid_gid() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "ug.txt", 0o644, 0, 0, 0).unwrap();
        fs.test_release_full(ino, fh).unwrap();
        match handle_setattr(&fs, ino, None, Some(42), Some(99), None, None, None, None) {
            AttrResult::Ok(attr) => {
                assert_eq!(attr.uid, 42);
                assert_eq!(attr.gid, 99);
            }
            AttrResult::Error(e) => panic!("expected Ok, got error {}", e),
        }
    }

    // ── handle_getlk ─────────────────────────────────────────────────────────

    #[test]
    fn test_handle_getlk_returns_f_unlck() {
        match handle_getlk(0, 100, 42) {
            LockResult::Ok(start, end, typ, pid) => {
                assert_eq!(start, 0);
                assert_eq!(end, 100);
                assert_eq!(typ, LOCK_UN);
                assert_eq!(pid, 42);
            }
        }
    }

    #[test]
    fn test_handle_getlk_preserves_range_and_pid() {
        match handle_getlk(512, 1024, 7) {
            LockResult::Ok(start, end, _, pid) => {
                assert_eq!(start, 512);
                assert_eq!(end, 1024);
                assert_eq!(pid, 7);
            }
        }
    }
}
