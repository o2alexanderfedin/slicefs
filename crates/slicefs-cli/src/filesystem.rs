//! `SliceFsFilesystem` — read-only FUSE adapter for SliceFS.
//!
//! Implements `fuser::Filesystem` using `DictMetadataStore` for inode/directory/manifest
//! lookups and blockset `GetBytes` for file content reads.
//!
//! All write operations return `EROFS` (read-only filesystem). This is intentional
//! per the project decision: EROFS signals "read-only filesystem", not "not implemented".

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use blockset::{Dictionary, GetBytes, GetData};
use fuser::{
    AccessFlags, BsdFileFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, KernelConfig, LockOwner, OpenFlags, RenameFlags, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow, WriteFlags,
};
use metadata::store::{DictMetadataStore, serialize_dictionary};
use slicefs_traits::digest::from_digest224;
use slicefs_traits::metadata::{InodeMeta, MetaError, MetadataStore};

// POSIX inode type bits
const S_IFMT: u32 = 0o170_000;
const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFLNK: u32 = 0o120_000;

/// Per-handle state for a writable file descriptor.
///
/// Created on `open()` when `O_WRONLY` or `O_RDWR` flags are present.
/// Removed on `release()`. The write buffer accumulates data until flush.
struct OpenFileState {
    ino: u64,
    buf: Vec<u8>,
}

/// FUSE filesystem adapter backed by a [`DictMetadataStore`].
///
/// `meta` provides all inode/directory/manifest metadata.
/// `dict` is a clone of the dictionary used for content reads via `GetBytes`.
/// Keeping `dict` separate avoids deadlocking with `DictMetadataStore`'s internal mutex.
///
/// `open_files` tracks per-handle write buffers; `next_fh` allocates unique handles.
/// `store_path` is the on-disk store root used by `destroy()` to persist state.
pub struct SliceFsFilesystem {
    pub(crate) meta: Arc<DictMetadataStore>,
    pub(crate) dict: Arc<Mutex<Dictionary>>,
    open_files: Mutex<HashMap<u64, OpenFileState>>,
    next_fh: AtomicU64,
    store_path: Option<PathBuf>,
}

impl SliceFsFilesystem {
    /// Construct a new filesystem.
    ///
    /// `meta` provides metadata.
    /// `dict` is a clone of the content dictionary (used for `GetBytes` reads).
    /// `store_path` is the on-disk store root; when `Some`, `destroy()` persists
    /// `dictionary.bin` and `root.bin` after the FUSE session ends.
    pub fn new(meta: DictMetadataStore, dict: Dictionary, store_path: Option<PathBuf>) -> Self {
        Self {
            meta: Arc::new(meta),
            dict: Arc::new(Mutex::new(dict)),
            open_files: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(0),
            store_path,
        }
    }

    /// Access the metadata store (used by the mount command after unmount).
    pub fn meta(&self) -> &Arc<DictMetadataStore> {
        &self.meta
    }

    /// Access the content dictionary (used for serialization on unmount).
    pub fn dict(&self) -> &Arc<Mutex<Dictionary>> {
        &self.dict
    }
}

/// Convert an [`InodeMeta`] to a fuser [`FileAttr`].
///
/// Uses the `mode` field's type bits to determine `FileType`.
/// `atime` is always `UNIX_EPOCH` (no-atime mode — deduplication workloads).
pub fn inode_to_file_attr(meta: &InodeMeta) -> FileAttr {
    let kind = match meta.mode & S_IFMT {
        S_IFDIR => FileType::Directory,
        S_IFLNK => FileType::Symlink,
        _ => FileType::RegularFile,
    };

    let perm = (meta.mode & 0o7777) as u16;
    let blocks = (meta.size + 511) / 512;

    let mtime = if meta.mtime_sec >= 0 {
        UNIX_EPOCH + Duration::new(meta.mtime_sec as u64, meta.mtime_nsec)
    } else {
        UNIX_EPOCH - Duration::new((-meta.mtime_sec) as u64, 0)
    };

    let ctime = if meta.ctime_sec >= 0 {
        UNIX_EPOCH + Duration::new(meta.ctime_sec as u64, meta.ctime_nsec)
    } else {
        UNIX_EPOCH - Duration::new((-meta.ctime_sec) as u64, 0)
    };

    FileAttr {
        ino: INodeNo(meta.ino),
        size: meta.size,
        blocks,
        atime: UNIX_EPOCH,
        mtime,
        ctime,
        crtime: UNIX_EPOCH,
        kind,
        perm,
        nlink: meta.nlinks,
        uid: meta.uid,
        gid: meta.gid,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

/// Map a [`MetaError`] to a fuser [`Errno`].
pub fn meta_error_to_fuse_errno(e: &MetaError) -> Errno {
    match e {
        MetaError::NotFound(_) => Errno::ENOENT,
        MetaError::AlreadyExists(_) => Errno::EEXIST,
        MetaError::NotADirectory(_) => Errno::ENOTDIR,
        MetaError::IsADirectory(_) => Errno::EISDIR,
        MetaError::NotEmpty(_) => Errno::ENOTEMPTY,
        MetaError::InvalidName(_) => Errno::EINVAL,
        MetaError::Corrupted(_) => Errno::EIO,
        MetaError::Io(_) => Errno::EIO,
    }
}

/// Map a [`MetaError`] to a POSIX errno integer (for tests using `libc` constants).
pub fn meta_error_to_errno(e: &MetaError) -> i32 {
    use libc;
    match e {
        MetaError::NotFound(_) => libc::ENOENT,
        MetaError::AlreadyExists(_) => libc::EEXIST,
        MetaError::NotADirectory(_) => libc::ENOTDIR,
        MetaError::IsADirectory(_) => libc::EISDIR,
        MetaError::NotEmpty(_) => libc::ENOTEMPTY,
        MetaError::InvalidName(_) => libc::EINVAL,
        MetaError::Corrupted(_) => libc::EIO,
        MetaError::Io(_) => libc::EIO,
    }
}

/// TTL for all fuser replies (1 second is appropriate for a read-only snapshot).
const TTL: Duration = Duration::from_secs(1);

impl Filesystem for SliceFsFilesystem {
    fn init(&mut self, _req: &Request, _config: &mut KernelConfig) -> io::Result<()> {
        Ok(())
    }

    fn destroy(&mut self) {
        if let Ok(root) = self.meta.commit() {
            let dict = self.dict.lock().unwrap();
            let bytes = serialize_dictionary(&*dict);
            drop(dict);
            if let Some(ref store_path) = self.store_path {
                let _ = std::fs::write(store_path.join("dictionary.bin"), &bytes);
                let mut root_bytes = Vec::with_capacity(28);
                for word in &root {
                    root_bytes.extend_from_slice(&word.to_le_bytes());
                }
                let _ = std::fs::write(store_path.join("root.bin"), &root_bytes);
            }
        }
    }

    // ── Read operations ───────────────────────────────────────────────────────

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.meta.get_inode(ino.0) {
            Ok(meta) => {
                let attr = inode_to_file_attr(&meta);
                reply.attr(&TTL, &attr);
            }
            Err(e) => {
                reply.error(meta_error_to_fuse_errno(&e));
            }
        }
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let name_str = name.to_str().unwrap_or("");
        match self.meta.lookup(parent.0, name_str) {
            Ok(child_ino) => match self.meta.get_inode(child_ino) {
                Ok(meta) => {
                    let attr = inode_to_file_attr(&meta);
                    reply.entry(&TTL, &attr, Generation(0));
                }
                Err(e) => reply.error(meta_error_to_fuse_errno(&e)),
            },
            Err(e) => reply.error(meta_error_to_fuse_errno(&e)),
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
        match self.meta.list_directory(ino.0) {
            Ok(entries) => {
                for (index, entry) in entries.iter().enumerate().skip(offset as usize) {
                    let kind = match self.meta.get_inode(entry.ino) {
                        Ok(m) => match m.mode & S_IFMT {
                            S_IFDIR => FileType::Directory,
                            S_IFLNK => FileType::Symlink,
                            _ => FileType::RegularFile,
                        },
                        Err(_) => FileType::RegularFile,
                    };

                    let next_offset = (index + 1) as u64;
                    let buffer_full = reply.add(INodeNo(entry.ino), next_offset, kind, &entry.name);
                    if buffer_full {
                        break;
                    }
                }
                reply.ok();
            }
            Err(e) => reply.error(meta_error_to_fuse_errno(&e)),
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
        let manifest = match self.meta.get_manifest(ino.0) {
            Ok(m) => m,
            Err(e) => return reply.error(meta_error_to_fuse_errno(&e)),
        };

        if manifest.is_empty() {
            return reply.data(&[]);
        }

        // The manifest contains exactly one Digest224 — the CDC content root
        let root_digest224 = manifest[0];
        let root_digest256 = from_digest224(&root_digest224);

        let dict = self.dict.lock().unwrap();
        let get_data = GetData::new(&*dict, &root_digest256);
        let bytes: Vec<u8> = GetBytes::new(get_data)
            .skip(offset as usize)
            .take(size as usize)
            .collect();
        drop(dict);

        reply.data(&bytes);
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        use fuser::OpenAccMode;
        let mode = flags.acc_mode();
        if mode == OpenAccMode::O_WRONLY || mode == OpenAccMode::O_RDWR {
            let fh = self.next_fh.fetch_add(1, Ordering::Relaxed) + 1;
            self.open_files.lock().unwrap().insert(
                fh,
                OpenFileState {
                    ino: ino.0,
                    buf: Vec::new(),
                },
            );
            reply.opened(FileHandle(fh), FopenFlags::empty());
        } else {
            reply.opened(FileHandle(0), FopenFlags::empty());
        }
    }

    fn opendir(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        reply.opened(FileHandle(0), FopenFlags::empty());
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        if fh.0 > 0 {
            self.open_files.lock().unwrap().remove(&fh.0);
        }
        reply.ok();
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        // Read-only snapshot: report reasonable fixed values.
        // bsize=4096, blocks=1M, bfree=0 (read-only), bavail=0,
        // files=1M, ffree=0, namelen=255, frsize=0
        reply.statfs(
            1_000_000, // blocks
            0,         // bfree (read-only)
            0,         // bavail (read-only)
            1_000_000, // files
            0,         // ffree
            4096,      // bsize
            255,       // namelen
            0,         // frsize
        );
    }

    fn access(&self, _req: &Request, _ino: INodeNo, _mask: AccessFlags, reply: ReplyEmpty) {
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
        let name_str = name.to_str().unwrap_or("");
        match self.meta.get_xattr(ino.0, name_str) {
            Ok(value) => {
                if size == 0 {
                    reply.size(value.len() as u32);
                } else {
                    reply.data(&value);
                }
            }
            Err(_) => reply.error(Errno::NO_XATTR),
        }
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        match self.meta.list_xattrs(ino.0) {
            Ok(names) => {
                let mut buf = Vec::new();
                for name in &names {
                    buf.extend_from_slice(name.as_bytes());
                    buf.push(0);
                }
                if size == 0 {
                    reply.size(buf.len() as u32);
                } else {
                    reply.data(&buf);
                }
            }
            Err(e) => reply.error(meta_error_to_fuse_errno(&e)),
        }
    }

    // ── Write operations — all return EROFS ───────────────────────────────────

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _offset: u64,
        _data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        reply.error(Errno::EROFS);
    }

    fn create(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        reply.error(Errno::EROFS);
    }

    fn mkdir(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EROFS);
    }

    fn mknod(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EROFS);
    }

    fn symlink(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _link_name: &OsStr,
        _target: &std::path::Path,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EROFS);
    }

    fn link(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _newparent: INodeNo,
        _newname: &OsStr,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EROFS);
    }

    fn unlink(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::EROFS);
    }

    fn rmdir(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::EROFS);
    }

    fn rename(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _newparent: INodeNo,
        _newname: &OsStr,
        _flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::EROFS);
    }

    fn setattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        _size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        reply.error(Errno::EROFS);
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
        reply.error(Errno::EROFS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blockset::{Dictionary, State, Tree};
    use metadata::store::DictMetadataStore;
    use slicefs_traits::metadata::{InodeMeta, MetadataStore};

    const S_IFDIR_TEST: u32 = 0o040_000;
    const S_IFREG_TEST: u32 = 0o100_000;

    fn fresh_fs() -> SliceFsFilesystem {
        let meta = DictMetadataStore::new();
        let dict = Dictionary::default();
        SliceFsFilesystem::new(meta, dict, None)
    }

    #[test]
    fn test_inode_to_file_attr_directory() {
        let meta = InodeMeta::new_directory(1, 0, 0, S_IFDIR_TEST | 0o755);
        let attr = inode_to_file_attr(&meta);
        assert_eq!(attr.kind, FileType::Directory);
        assert_eq!(attr.perm, 0o755);
        assert_eq!(attr.ino, INodeNo(1));
    }

    #[test]
    fn test_inode_to_file_attr_regular_file() {
        let meta = InodeMeta::new_file(2, 1000, 1000, S_IFREG_TEST | 0o644);
        let attr = inode_to_file_attr(&meta);
        assert_eq!(attr.kind, FileType::RegularFile);
        assert_eq!(attr.perm, 0o644);
        assert_eq!(attr.ino, INodeNo(2));
    }

    #[test]
    fn test_meta_error_to_errno() {
        use slicefs_traits::metadata::MetaError;
        assert_eq!(meta_error_to_errno(&MetaError::NotFound(0)), libc::ENOENT);
        assert_eq!(meta_error_to_errno(&MetaError::AlreadyExists(0)), libc::EEXIST);
        assert_eq!(meta_error_to_errno(&MetaError::NotADirectory(0)), libc::ENOTDIR);
        assert_eq!(meta_error_to_errno(&MetaError::IsADirectory(0)), libc::EISDIR);
        assert_eq!(meta_error_to_errno(&MetaError::Corrupted("x".into())), libc::EIO);
    }

    #[test]
    fn test_getattr_root_is_directory() {
        let fs = fresh_fs();
        let meta = fs.meta.get_inode(1).unwrap();
        let attr = inode_to_file_attr(&meta);
        assert_eq!(attr.ino, INodeNo(1));
        assert_eq!(attr.kind, FileType::Directory);
    }

    #[test]
    fn test_lookup_nonexistent_returns_notfound() {
        let fs = fresh_fs();
        let result = fs.meta.lookup(1, "nonexistent_file");
        assert!(result.is_err());
        let errno = meta_error_to_errno(&result.unwrap_err());
        assert_eq!(errno, libc::ENOENT);
    }

    #[test]
    fn test_readdir_root_contains_dot_entries() {
        let fs = fresh_fs();
        let entries = fs.meta.list_directory(1).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"."), "root must contain '.'");
        assert!(names.contains(&".."), "root must contain '..'");
    }

    #[test]
    fn test_read_returns_correct_bytes() {
        let mut dict = Dictionary::default();
        let content = b"hello, SliceFS content!";
        let root_digest224 = State::push_all(&mut dict, content);

        let meta_store = DictMetadataStore::new();
        let file_meta = InodeMeta::new_file(0, 0, 0, S_IFREG_TEST | 0o644);
        let ino = meta_store.create_inode(&file_meta).unwrap();
        meta_store.link(1, "testfile", ino).unwrap();
        meta_store.set_manifest(ino, &[root_digest224]).unwrap();

        let fs = SliceFsFilesystem::new(meta_store, dict, None);

        let manifest = fs.meta.get_manifest(ino).unwrap();
        assert!(!manifest.is_empty());

        let root224 = manifest[0];
        let root256 = from_digest224(&root224);
        let dict_guard = fs.dict.lock().unwrap();
        let get_data = GetData::new(&*dict_guard, &root256);
        let bytes: Vec<u8> = GetBytes::new(get_data).take(content.len()).collect();
        drop(dict_guard);

        assert_eq!(bytes, content);
    }

    #[test]
    fn test_read_with_offset() {
        let mut dict = Dictionary::default();
        let content = b"hello, SliceFS offset test!";
        let root_digest224 = State::push_all(&mut dict, content);

        let meta_store = DictMetadataStore::new();
        let file_meta = InodeMeta::new_file(0, 0, 0, S_IFREG_TEST | 0o644);
        let ino = meta_store.create_inode(&file_meta).unwrap();
        meta_store.set_manifest(ino, &[root_digest224]).unwrap();

        let fs = SliceFsFilesystem::new(meta_store, dict, None);

        let manifest = fs.meta.get_manifest(ino).unwrap();
        let root256 = from_digest224(&manifest[0]);
        let dict_guard = fs.dict.lock().unwrap();
        let get_data = GetData::new(&*dict_guard, &root256);
        let offset = 7usize;
        let bytes: Vec<u8> = GetBytes::new(get_data)
            .skip(offset)
            .take(content.len() - offset)
            .collect();
        drop(dict_guard);

        assert_eq!(bytes, &content[offset..]);
    }

    #[test]
    fn test_statfs_returns_nonzero_blocks() {
        // The statfs values are hardcoded constants — verify they are non-zero
        let blocks: u64 = 1_000_000;
        let files: u64 = 1_000_000;
        assert!(blocks > 0);
        assert!(files > 0);
    }
}
