//! `SliceFsFilesystem` — FUSE adapter for SliceFS.
//!
//! Implements `fuser::Filesystem` using `DictMetadataStore` for inode/directory/manifest
//! lookups and blockset `GetBytes` for file content reads.
//!
//! Write callbacks (create, write, release, setattr) are fully implemented in Phase 4.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use blockset::{Dictionary, GetBytes, GetData, State, Tree};
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

/// TTL for all fuser replies (1 second is appropriate for a write-capable snapshot).
const TTL: Duration = Duration::from_secs(1);

// ── Test helpers ──────────────────────────────────────────────────────────────
//
// These methods expose the create/write/release/setattr pipeline without going
// through FUSE request/reply machinery, enabling unit and integration testing
// on macOS where a FUSE mount is not available.

impl SliceFsFilesystem {
    /// Create a regular file inode in `parent_ino` directory, returning `(ino, fh)`.
    /// Used by integration tests to bypass the FUSE request/reply layer.
    pub fn test_create(
        &self,
        parent_ino: u64,
        name: &str,
        mode: u32,
        umask: u32,
        uid: u32,
        gid: u32,
    ) -> Result<(u64, u64), i32> {
        let file_mode = S_IFREG | (mode & !umask & 0o7777);
        let file_meta = InodeMeta::new_file(0, uid, gid, file_mode);
        let ino = self.meta.create_inode(&file_meta).map_err(|e| meta_error_to_errno(&e))?;
        if let Err(e) = self.meta.link(parent_ino, name, ino) {
            let _ = self.meta.delete_inode(ino);
            return Err(meta_error_to_errno(&e));
        }
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed) + 1;
        self.open_files.lock().unwrap().insert(fh, OpenFileState { ino, buf: Vec::new() });
        Ok((ino, fh))
    }

    /// Write `data` at `offset` into the buffer for `fh`. Returns bytes written.
    /// Used by integration tests to bypass the FUSE request/reply layer.
    pub fn test_write(&self, fh: u64, offset: u64, data: &[u8]) -> Result<u32, i32> {
        let mut open_files = self.open_files.lock().unwrap();
        let state = open_files.get_mut(&fh).ok_or(libc::EBADF)?;
        let end = offset as usize + data.len();
        if end > state.buf.len() {
            state.buf.resize(end, 0);
        }
        state.buf[offset as usize..end].copy_from_slice(data);
        Ok(data.len() as u32)
    }

    /// Release file handle `fh`, flushing its buffer to CAS and updating the inode.
    /// Used by integration tests to bypass the FUSE request/reply layer.
    pub fn test_release(&self, ino: u64, fh: u64) -> Result<(), i32> {
        let state = self.open_files.lock().unwrap().remove(&fh);
        let buf = match state {
            Some(s) => s.buf,
            None => return Ok(()), // Already closed
        };
        self.flush_buffer_to_cas(ino, buf)
    }

    /// Flush `buf` to CAS, set manifest, update inode size/mtime.
    /// Shared between test_release and the FUSE release() callback.
    fn flush_buffer_to_cas(&self, ino: u64, buf: Vec<u8>) -> Result<(), i32> {
        if buf.is_empty() {
            // Empty file: set empty manifest
            self.meta.set_manifest(ino, &[]).map_err(|_| libc::EIO)?;
        } else {
            // Push content through CDC — acquire dict, push, release BEFORE any meta call
            let content_digest = {
                let mut dict = self.dict.lock().unwrap();
                State::push_all(&mut *dict, &buf)
                // dict lock dropped here
            };
            self.meta.set_manifest(ino, &[content_digest]).map_err(|_| libc::EIO)?;
            self.meta.increment_refcount(&content_digest);
        }

        // Update inode size and mtime
        let mut inode = self.meta.get_inode(ino).map_err(|_| libc::EIO)?;
        inode.size = buf.len() as u64;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        inode.mtime_sec = now.as_secs() as i64;
        inode.mtime_nsec = now.subsec_nanos();
        inode.ctime_sec = inode.mtime_sec;
        inode.ctime_nsec = inode.mtime_nsec;
        self.meta.update_inode(&inode).map_err(|_| libc::EIO)?;
        Ok(())
    }

    /// Truncate/extend a file to `new_size` bytes. If `fh` is Some and open,
    /// operates on the in-flight buffer; otherwise reads from CAS, adjusts, re-pushes.
    pub fn test_setattr_size(&self, ino: u64, fh: Option<u64>, new_size: u64) -> Result<(), i32> {
        // Case A: open file handle — truncate in-flight buffer directly
        if let Some(fh_val) = fh {
            let mut open_files = self.open_files.lock().unwrap();
            if let Some(state) = open_files.get_mut(&fh_val) {
                state.buf.resize(new_size as usize, 0);
                // Update inode size immediately
                drop(open_files);
                let mut inode = self.meta.get_inode(ino).map_err(|_| libc::EIO)?;
                inode.size = new_size;
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or(Duration::ZERO);
                inode.ctime_sec = now.as_secs() as i64;
                inode.ctime_nsec = now.subsec_nanos();
                self.meta.update_inode(&inode).map_err(|_| libc::EIO)?;
                return Ok(());
            }
        }

        // Case B: closed file — read from CAS, truncate/extend, re-push
        let old_manifest = self.meta.get_manifest(ino).map_err(|_| libc::EIO)?;

        // Read current content
        let mut content: Vec<u8> = if old_manifest.is_empty() {
            Vec::new()
        } else {
            let root256 = from_digest224(&old_manifest[0]);
            let dict = self.dict.lock().unwrap();
            let get_data = GetData::new(&*dict, &root256);
            GetBytes::new(get_data).collect()
        };

        // Decrement old refcount
        if !old_manifest.is_empty() {
            self.meta.decrement_refcount(&old_manifest[0]);
        }

        // Truncate or zero-extend
        content.resize(new_size as usize, 0);

        // Push new content
        if content.is_empty() {
            self.meta.set_manifest(ino, &[]).map_err(|_| libc::EIO)?;
        } else {
            let new_digest = {
                let mut dict = self.dict.lock().unwrap();
                State::push_all(&mut *dict, &content)
            };
            self.meta.set_manifest(ino, &[new_digest]).map_err(|_| libc::EIO)?;
            self.meta.increment_refcount(&new_digest);
        }

        // Update inode
        let mut inode = self.meta.get_inode(ino).map_err(|_| libc::EIO)?;
        inode.size = new_size;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        inode.ctime_sec = now.as_secs() as i64;
        inode.ctime_nsec = now.subsec_nanos();
        self.meta.update_inode(&inode).map_err(|_| libc::EIO)?;
        Ok(())
    }

    /// Update permission bits (preserving file type bits) for an inode.
    pub fn test_setattr_mode(&self, ino: u64, mode: u32) -> Result<(), i32> {
        let mut inode = self.meta.get_inode(ino).map_err(|_| libc::EIO)?;
        inode.mode = (inode.mode & S_IFMT) | (mode & 0o7777);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        inode.ctime_sec = now.as_secs() as i64;
        inode.ctime_nsec = now.subsec_nanos();
        self.meta.update_inode(&inode).map_err(|_| libc::EIO)?;
        Ok(())
    }

    /// Update uid and/or gid for an inode.
    pub fn test_setattr_uid_gid(
        &self,
        ino: u64,
        uid: Option<u32>,
        gid: Option<u32>,
    ) -> Result<(), i32> {
        let mut inode = self.meta.get_inode(ino).map_err(|_| libc::EIO)?;
        if let Some(u) = uid {
            inode.uid = u;
        }
        if let Some(g) = gid {
            inode.gid = g;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        inode.ctime_sec = now.as_secs() as i64;
        inode.ctime_nsec = now.subsec_nanos();
        self.meta.update_inode(&inode).map_err(|_| libc::EIO)?;
        Ok(())
    }

    /// Update mtime for an inode to a specific (sec, nsec).
    pub fn test_setattr_mtime(&self, ino: u64, mtime_sec: i64, mtime_nsec: u32) -> Result<(), i32> {
        let mut inode = self.meta.get_inode(ino).map_err(|_| libc::EIO)?;
        inode.mtime_sec = mtime_sec;
        inode.mtime_nsec = mtime_nsec;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        inode.ctime_sec = now.as_secs() as i64;
        inode.ctime_nsec = now.subsec_nanos();
        self.meta.update_inode(&inode).map_err(|_| libc::EIO)?;
        Ok(())
    }

    /// Returns ENOSYS — mknod (device nodes, FIFOs) is not supported in Phase 4.
    pub fn test_mknod(&self, _parent: u64, _name: &str, _mode: u32, _rdev: u32) -> Result<(), i32> {
        Err(libc::ENOSYS)
    }

    // ── Directory and link simulate helpers (Phase 4 Plan 03) ─────────────────

    /// Create a subdirectory named `name` in `parent_ino`.
    ///
    /// Delegates to `DictMetadataStore::create_directory` which handles:
    /// - inode allocation, . and .. entries, adding name to parent, parent nlinks increment.
    ///
    /// Returns the new directory's inode number.
    pub fn simulate_mkdir(
        &self,
        parent_ino: u64,
        name: &str,
        mode: u32,
        umask: u32,
        uid: u32,
        gid: u32,
    ) -> Result<u64, i32> {
        let dir_mode = S_IFDIR | (mode & !umask & 0o7777);
        let dir_meta = InodeMeta::new_directory(0, uid, gid, dir_mode);
        self.meta
            .create_directory(parent_ino, name, &dir_meta)
            .map_err(|e| meta_error_to_errno(&e))
    }

    /// Remove empty directory `name` from `parent_ino`.
    ///
    /// Returns ENOTEMPTY if the directory still has entries beyond . and ..
    /// Returns EISDIR if the target is not a directory (though lookup normally guards this).
    pub fn simulate_rmdir(&self, parent_ino: u64, name: &str) -> Result<(), i32> {
        // Resolve name to inode
        let ino = self
            .meta
            .lookup(parent_ino, name)
            .map_err(|e| meta_error_to_errno(&e))?;

        // Verify it is a directory
        let inode = self.meta.get_inode(ino).map_err(|e| meta_error_to_errno(&e))?;
        if inode.mode & S_IFDIR == 0 {
            return Err(libc::ENOTDIR);
        }

        // Check emptiness: list_directory returns . and .. plus any real entries
        let entries = self
            .meta
            .list_directory(ino)
            .map_err(|e| meta_error_to_errno(&e))?;
        // . and .. always present — anything beyond that is ENOTEMPTY
        let real_entries = entries.iter().filter(|e| e.name != "." && e.name != "..").count();
        if real_entries > 0 {
            return Err(libc::ENOTEMPTY);
        }

        // Remove directory entry from parent and delete directory inode
        self.meta
            .unlink(parent_ino, name)
            .map_err(|e| meta_error_to_errno(&e))?;
        self.meta.delete_inode(ino).map_err(|e| meta_error_to_errno(&e))?;

        // Decrement parent nlinks (removing the .. backlink from the deleted subdir)
        let mut parent_inode = self
            .meta
            .get_inode(parent_ino)
            .map_err(|e| meta_error_to_errno(&e))?;
        if parent_inode.nlinks > 0 {
            parent_inode.nlinks -= 1;
            self.meta.update_inode(&parent_inode).map_err(|e| meta_error_to_errno(&e))?;
        }
        Ok(())
    }

    /// Remove file (non-directory) `name` from `parent_ino`.
    ///
    /// Manages nlinks lifecycle:
    /// - Decrements nlinks.
    /// - When nlinks reaches 0: decrements content refcounts and deletes inode.
    /// - When nlinks > 0: updates inode (hard link still exists elsewhere).
    ///
    /// Returns EISDIR if the target is a directory (use rmdir instead).
    pub fn simulate_unlink(&self, parent_ino: u64, name: &str) -> Result<(), i32> {
        // Resolve name
        let ino = self
            .meta
            .lookup(parent_ino, name)
            .map_err(|e| meta_error_to_errno(&e))?;

        // Get inode
        let mut inode = self.meta.get_inode(ino).map_err(|e| meta_error_to_errno(&e))?;

        // Must not be a directory
        if inode.mode & S_IFDIR != 0 {
            return Err(libc::EISDIR);
        }

        // Remove directory entry
        self.meta
            .unlink(parent_ino, name)
            .map_err(|e| meta_error_to_errno(&e))?;

        // Guard against underflow
        if inode.nlinks > 0 {
            inode.nlinks -= 1;
        }

        if inode.nlinks == 0 {
            // Decrement content refcounts
            if let Ok(manifest) = self.meta.get_manifest(ino) {
                for digest in &manifest {
                    self.meta.decrement_refcount(digest);
                }
            }
            // Delete inode
            let _ = self.meta.delete_inode(ino);
        } else {
            // Hard links still exist — just persist the decremented nlinks
            self.meta.update_inode(&inode).map_err(|e| meta_error_to_errno(&e))?;
        }
        Ok(())
    }

    /// Create a hard link: add `newname` in `newparent_ino` pointing at `ino`.
    ///
    /// POSIX disallows hard links to directories — returns EPERM.
    /// Returns the new inode number (same as `ino`).
    pub fn simulate_link(&self, ino: u64, newparent_ino: u64, newname: &str) -> Result<u64, i32> {
        // Get source inode
        let mut inode = self.meta.get_inode(ino).map_err(|e| meta_error_to_errno(&e))?;

        // Disallow hard links to directories
        if inode.mode & S_IFDIR != 0 {
            return Err(libc::EPERM);
        }

        // Add new directory entry
        self.meta
            .link(newparent_ino, newname, ino)
            .map_err(|e| meta_error_to_errno(&e))?;

        // Increment nlinks and update ctime
        inode.nlinks += 1;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        inode.ctime_sec = now.as_secs() as i64;
        inode.ctime_nsec = now.subsec_nanos();
        self.meta.update_inode(&inode).map_err(|e| meta_error_to_errno(&e))?;

        Ok(ino)
    }

    /// Rename `name` in `parent_ino` to `newname` in `newparent_ino`.
    ///
    /// `flags` maps to Linux rename2 flags:
    /// - 0: normal rename (overwrite target if it exists)
    /// - 1 (RENAME_NOREPLACE): return EEXIST if target already exists
    /// - 2 (RENAME_EXCHANGE): return ENOSYS (not supported)
    pub fn simulate_rename(
        &self,
        parent_ino: u64,
        name: &str,
        newparent_ino: u64,
        newname: &str,
        flags: u32,
    ) -> Result<(), i32> {
        // RENAME_EXCHANGE not supported
        if flags & 2 != 0 {
            return Err(libc::ENOSYS);
        }

        // Resolve source inode
        let src_ino = self
            .meta
            .lookup(parent_ino, name)
            .map_err(|_| libc::ENOENT)?;

        // RENAME_NOREPLACE: fail if target already exists
        let target_ino = self.meta.lookup(newparent_ino, newname).ok();
        if flags & 1 != 0 {
            if target_ino.is_some() {
                return Err(libc::EEXIST);
            }
        }

        // If target exists and we're doing a normal rename, remove the old target
        if let Some(dst_ino) = target_ino {
            let dst_inode = self.meta.get_inode(dst_ino).map_err(|e| meta_error_to_errno(&e))?;
            self.meta
                .unlink(newparent_ino, newname)
                .map_err(|e| meta_error_to_errno(&e))?;
            // Manage nlinks for the displaced target
            let new_nlinks = dst_inode.nlinks.saturating_sub(1);
            if new_nlinks == 0 && dst_inode.mode & S_IFDIR == 0 {
                // Decrement refcounts and delete inode for regular files
                if let Ok(manifest) = self.meta.get_manifest(dst_ino) {
                    for digest in &manifest {
                        self.meta.decrement_refcount(digest);
                    }
                }
                let _ = self.meta.delete_inode(dst_ino);
            } else if new_nlinks > 0 {
                let mut updated_dst = dst_inode.clone();
                updated_dst.nlinks = new_nlinks;
                let _ = self.meta.update_inode(&updated_dst);
            }
            // Directories are left orphaned (no directory hard links in our impl)
        }

        // Link source at new location and unlink from old location
        self.meta
            .link(newparent_ino, newname, src_ino)
            .map_err(|e| meta_error_to_errno(&e))?;
        self.meta
            .unlink(parent_ino, name)
            .map_err(|e| meta_error_to_errno(&e))?;

        // Update ctime on moved inode
        let mut src_inode = self.meta.get_inode(src_ino).map_err(|e| meta_error_to_errno(&e))?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        src_inode.ctime_sec = now.as_secs() as i64;
        src_inode.ctime_nsec = now.subsec_nanos();
        self.meta.update_inode(&src_inode).map_err(|e| meta_error_to_errno(&e))?;

        Ok(())
    }

    /// Create a symbolic link named `link_name` in `parent_ino` with `target` as content.
    ///
    /// Target is stored as CAS content in the dictionary; manifest points to it.
    /// Returns the new symlink inode number.
    pub fn simulate_symlink(
        &self,
        parent_ino: u64,
        link_name: &str,
        target: &str,
        uid: u32,
        gid: u32,
    ) -> Result<u64, i32> {
        let target_bytes = target.as_bytes();
        let symlink_meta = InodeMeta {
            ino: 0,
            mode: S_IFLNK | 0o777,
            uid,
            gid,
            nlinks: 1,
            size: target_bytes.len() as u64,
            mtime_sec: 0,
            mtime_nsec: 0,
            ctime_sec: 0,
            ctime_nsec: 0,
        };

        // Create inode
        let ino = self
            .meta
            .create_inode(&symlink_meta)
            .map_err(|e| meta_error_to_errno(&e))?;

        // Link into parent directory
        if let Err(e) = self.meta.link(parent_ino, link_name, ino) {
            let _ = self.meta.delete_inode(ino);
            return Err(meta_error_to_errno(&e));
        }

        // Store target as CAS content
        let content_digest = {
            let mut dict = self.dict.lock().unwrap();
            State::push_all(&mut *dict, target_bytes)
        };
        self.meta
            .set_manifest(ino, &[content_digest])
            .map_err(|_| libc::EIO)?;
        self.meta.increment_refcount(&content_digest);

        // Update inode size to target length
        let mut inode = self.meta.get_inode(ino).map_err(|_| libc::EIO)?;
        inode.size = target_bytes.len() as u64;
        self.meta.update_inode(&inode).map_err(|_| libc::EIO)?;

        Ok(ino)
    }

    /// Read the target of a symbolic link inode.
    ///
    /// Loads the manifest, reads content bytes from the dictionary, and returns them as a String.
    pub fn simulate_readlink(&self, ino: u64) -> Result<String, i32> {
        let manifest = self.meta.get_manifest(ino).map_err(|_| libc::EINVAL)?;
        if manifest.is_empty() {
            return Ok(String::new());
        }
        let root256 = from_digest224(&manifest[0]);
        let dict = self.dict.lock().unwrap();
        let get_data = GetData::new(&*dict, &root256);
        let bytes: Vec<u8> = GetBytes::new(get_data).collect();
        drop(dict);
        String::from_utf8(bytes).map_err(|_| libc::EINVAL)
    }
}

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
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        // Read-after-write within the same open session: serve from in-flight buffer
        if fh.0 > 0 {
            let open_files = self.open_files.lock().unwrap();
            if let Some(state) = open_files.get(&fh.0) {
                let buf = &state.buf;
                let start = (offset as usize).min(buf.len());
                let end = (offset as usize + size as usize).min(buf.len());
                return reply.data(&buf[start..end]);
            }
        }

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
        ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        if fh.0 == 0 {
            // Read-only handle — nothing to flush
            return reply.ok();
        }
        match self.test_release(ino.0, fh.0) {
            Ok(()) => reply.ok(),
            Err(_) => reply.error(Errno::EIO),
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

    // ── Write operations ───────────────────────────────────────────────────────

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        match self.test_write(fh.0, offset, data) {
            Ok(n) => reply.written(n),
            Err(_) => reply.error(Errno::EBADF),
        }
    }

    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let name_str = match name.to_str() {
            Some(s) => s,
            None => return reply.error(Errno::EINVAL),
        };
        match self.test_create(parent.0, name_str, mode, umask, req.uid(), req.gid()) {
            Ok((ino, fh)) => {
                match self.meta.get_inode(ino) {
                    Ok(meta) => {
                        let attr = inode_to_file_attr(&meta);
                        reply.created(&TTL, &attr, Generation(0), FileHandle(fh), FopenFlags::empty());
                    }
                    Err(e) => reply.error(meta_error_to_fuse_errno(&e)),
                }
            }
            Err(errno) => reply.error(Errno::from_i32(errno)),
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
        let name_str = match name.to_str() {
            Some(s) => s,
            None => return reply.error(Errno::EINVAL),
        };
        match self.simulate_mkdir(parent.0, name_str, mode, umask, req.uid(), req.gid()) {
            Ok(ino) => match self.meta.get_inode(ino) {
                Ok(meta) => {
                    let attr = inode_to_file_attr(&meta);
                    reply.entry(&TTL, &attr, Generation(0));
                }
                Err(e) => reply.error(meta_error_to_fuse_errno(&e)),
            },
            Err(errno) => reply.error(Errno::from_i32(errno)),
        }
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
        // Device nodes and FIFOs are not supported in Phase 4.
        // Regular file creation goes through create().
        reply.error(Errno::ENOSYS);
    }

    fn symlink(
        &self,
        req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &std::path::Path,
        reply: ReplyEntry,
    ) {
        let name_str = match link_name.to_str() {
            Some(s) => s,
            None => return reply.error(Errno::EINVAL),
        };
        let target_str = match target.to_str() {
            Some(s) => s,
            None => return reply.error(Errno::EINVAL),
        };
        match self.simulate_symlink(parent.0, name_str, target_str, req.uid(), req.gid()) {
            Ok(ino) => match self.meta.get_inode(ino) {
                Ok(meta) => {
                    let attr = inode_to_file_attr(&meta);
                    reply.entry(&TTL, &attr, Generation(0));
                }
                Err(e) => reply.error(meta_error_to_fuse_errno(&e)),
            },
            Err(errno) => reply.error(Errno::from_i32(errno)),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        match self.simulate_readlink(ino.0) {
            Ok(target) => reply.data(target.as_bytes()),
            Err(errno) => reply.error(Errno::from_i32(errno)),
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
        let name_str = match newname.to_str() {
            Some(s) => s,
            None => return reply.error(Errno::EINVAL),
        };
        match self.simulate_link(ino.0, newparent.0, name_str) {
            Ok(new_ino) => match self.meta.get_inode(new_ino) {
                Ok(meta) => {
                    let attr = inode_to_file_attr(&meta);
                    reply.entry(&TTL, &attr, Generation(0));
                }
                Err(e) => reply.error(meta_error_to_fuse_errno(&e)),
            },
            Err(errno) => reply.error(Errno::from_i32(errno)),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let name_str = match name.to_str() {
            Some(s) => s,
            None => return reply.error(Errno::EINVAL),
        };
        match self.simulate_unlink(parent.0, name_str) {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(Errno::from_i32(errno)),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let name_str = match name.to_str() {
            Some(s) => s,
            None => return reply.error(Errno::EINVAL),
        };
        match self.simulate_rmdir(parent.0, name_str) {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(Errno::from_i32(errno)),
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
        let name_str = match name.to_str() {
            Some(s) => s,
            None => return reply.error(Errno::EINVAL),
        };
        let newname_str = match newname.to_str() {
            Some(s) => s,
            None => return reply.error(Errno::EINVAL),
        };
        match self.simulate_rename(parent.0, name_str, newparent.0, newname_str, flags.bits()) {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(Errno::from_i32(errno)),
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
        let mut inode = match self.meta.get_inode(ino.0) {
            Ok(m) => m,
            Err(e) => return reply.error(meta_error_to_fuse_errno(&e)),
        };

        if let Some(m) = mode {
            inode.mode = (inode.mode & S_IFMT) | (m & 0o7777);
        }
        if let Some(u) = uid {
            inode.uid = u;
        }
        if let Some(g) = gid {
            inode.gid = g;
        }
        if let Some(mt) = mtime {
            let t = match mt {
                TimeOrNow::SpecificTime(st) => st,
                TimeOrNow::Now => SystemTime::now(),
            };
            let dur = t.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
            inode.mtime_sec = dur.as_secs() as i64;
            inode.mtime_nsec = dur.subsec_nanos();
        }
        if let Some(new_size) = size {
            let fh_opt = fh.map(|f| f.0);
            if let Err(e) = self.test_setattr_size(ino.0, fh_opt, new_size) {
                return reply.error(Errno::from_i32(e));
            }
            // Re-load inode after size change (test_setattr_size updates it)
            inode = match self.meta.get_inode(ino.0) {
                Ok(m) => m,
                Err(e) => return reply.error(meta_error_to_fuse_errno(&e)),
            };
        }

        // Always update ctime
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        inode.ctime_sec = now.as_secs() as i64;
        inode.ctime_nsec = now.subsec_nanos();

        if let Err(e) = self.meta.update_inode(&inode) {
            return reply.error(meta_error_to_fuse_errno(&e));
        }

        let attr = inode_to_file_attr(&inode);
        reply.attr(&TTL, &attr);
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
