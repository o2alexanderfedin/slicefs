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

use blockset::{State, Tree, FileStorageAdd, file_storage_get, StorageAdd, Digest224};
use tracing::debug;
use fuser::{
    AccessFlags, BsdFileFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, InitFlags, KernelConfig, LockOwner, OpenFlags, RenameFlags, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow, WriteFlags,
};
use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use slicefs_traits::metadata::{InodeMeta, MetaError, MetadataStore};

// POSIX inode type bits
const S_IFMT: u32 = 0o170_000;
const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFLNK: u32 = 0o120_000;

/// Write mode for an open file handle.
///
/// Streaming mode is the default for sequential writes (O(log N) memory).
/// Buffered mode is a one-way fallback for non-sequential writes (O(N) memory).
/// Once a handle transitions to Buffered, it never reverts to Streaming.
enum WriteMode {
    Streaming {
        state: State,
        next_expected_offset: u64,
    },
    Buffered {
        buf: Vec<u8>,
    },
}

/// Per-handle state for a writable file descriptor.
///
/// Created on `open()` when `O_WRONLY` or `O_RDWR` flags are present.
/// Removed on `release()`. Writes are streamed incrementally through the
/// blockset `State` Merkle tree accumulator — O(log N) memory regardless
/// of file size — unless a non-sequential write triggers fallback to
/// Buffered mode (O(N) memory).
///
/// Lifecycle:
/// - Created with `WriteMode::Streaming` and `byte_count = 0`.
/// - Sequential writes: `state.push_bytes()` called on each write, `byte_count` incremented.
/// - Non-sequential write: one-way transition to `WriteMode::Buffered`.
/// - `cas_committed` set to `true` after fsync/flush commits to CAS.
/// - `cas_committed` set to `false` when new writes arrive (dirty again).
/// - `last_committed_root` tracks the most recently committed Digest224
///   for refcount decrement-on-overwrite.
struct OpenFileState {
    ino: u64,
    /// Write mode — Streaming (sequential, O(log N)) or Buffered (non-sequential, O(N)).
    write_mode: WriteMode,
    /// Total bytes written. Tracks inode size.
    byte_count: u64,
    /// True if the manifest was committed to CAS by a prior flush/fsync and the
    /// state has not been dirtied by subsequent writes.
    cas_committed: bool,
    /// The most recently committed Digest224 root. Used to decrement refcount
    /// when a new root is committed (fsync or release), preventing refcount leaks.
    last_committed_root: Option<Digest224>,
}

/// FUSE filesystem adapter backed by a [`DictMetadataStore`].
///
/// `meta` provides all inode/directory/manifest metadata.
/// `io` is the shared `StoreIo` used for file content reads/writes via
/// `file_storage_get` / `FileStorageAdd`. It is the same `Arc<Mutex<StoreIo>>`
/// held by `meta`, accessed via `meta.io().clone()`.
///
/// `open_files` tracks per-handle write buffers; `next_fh` allocates unique handles.
/// `store_path` is the on-disk store root used by `destroy()` to persist state.
///
/// Raw bytes flow directly into the Merkle tree (v3 store format — no compression header).
/// Digest224 is computed on raw bytes, enabling content-addressed deduplication.
pub struct SliceFsFilesystem {
    pub(crate) meta: Arc<DictMetadataStore>,
    pub(crate) io: Arc<Mutex<StoreIo>>,
    open_files: Mutex<HashMap<u64, OpenFileState>>,
    next_fh: AtomicU64,
    store_path: Option<PathBuf>,
    auto_snapshot: bool,
}

impl SliceFsFilesystem {
    /// Construct a new filesystem.
    ///
    /// `meta` provides metadata. `io` is the shared `StoreIo` for content reads/writes
    /// (obtain via `meta.io().clone()` after constructing `DictMetadataStore`).
    /// `store_path` is the on-disk store root; when `Some`, `destroy()` persists
    /// state after the FUSE session ends.
    ///
    /// Raw bytes flow directly into the Merkle tree (v3 store format — no compression).
    pub fn new(
        meta: DictMetadataStore,
        io: Arc<Mutex<StoreIo>>,
        store_path: Option<PathBuf>,
    ) -> Self {
        Self {
            meta: Arc::new(meta),
            io,
            open_files: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(0),
            store_path,
            auto_snapshot: false,
        }
    }

    /// Enable auto-snapshot on clean unmount.
    pub fn set_auto_snapshot(&mut self, enabled: bool) {
        self.auto_snapshot = enabled;
    }

    /// Access the metadata store (used by the mount command after unmount).
    pub fn meta(&self) -> &Arc<DictMetadataStore> {
        &self.meta
    }

    /// Access the shared Io backend (used for content reads/writes).
    pub fn io(&self) -> &Arc<Mutex<StoreIo>> {
        &self.io
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

/// Recursively sum file sizes under `dir`.
///
/// Used by `statfs` to compute physical bytes from the `vt0/` CAS directory.
fn dir_size(dir: &std::path::Path) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                if meta.is_file() {
                    total += meta.len();
                } else if meta.is_dir() {
                    total += dir_size(&entry.path());
                }
            }
        }
    }
    total
}

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
        self.open_files.lock().unwrap().insert(fh, OpenFileState {
            ino,
            write_mode: WriteMode::Streaming { state: State::default(), next_expected_offset: 0 },
            byte_count: 0,
            cas_committed: false,
            last_committed_root: None,
        });
        Ok((ino, fh))
    }

    /// Write `data` at `offset` into the streaming state for `fh`. Returns bytes written.
    ///
    /// Phase 10: sequential writes only. Data is always appended via `push_bytes`.
    /// The `offset` parameter is accepted but not validated for sequential ordering;
    /// Phase 11 (STRM-02) will add offset validation and fallback for non-sequential writes.
    ///
    /// Lock ordering: open_files -> io (canonical order).
    pub fn test_write(&self, fh: u64, offset: u64, data: &[u8]) -> Result<u32, i32> {
        // Phase 10: sequential writes only. Non-sequential offset detection is Phase 11.
        // For now, accept any offset — push_bytes always appends sequentially.
        let mut open_files = self.open_files.lock().unwrap();
        let state = open_files.get_mut(&fh).ok_or(libc::EBADF)?;

        // Lock ordering: open_files -> io is canonical.
        // We need both locks simultaneously because push_bytes needs mutable State
        // (held via open_files) and mutable io (for FileStorageAdd).
        let mut io = self.io.lock().unwrap();
        let mut fsa = FileStorageAdd::new(&mut *io);
        state.state.push_bytes(&mut fsa, data);
        drop(fsa);
        drop(io);

        state.byte_count += data.len() as u64;
        state.cas_committed = false; // New writes invalidate prior CAS commit
        Ok(data.len() as u32)
    }

    /// Release file handle `fh`, finalizing streaming state to CAS and updating the inode.
    /// Used by integration tests to bypass the FUSE request/reply layer.
    ///
    /// Consumes the State via `state.end()` (this is release — handle is closing).
    /// If the final digest matches `last_committed_root` (fsync already committed this
    /// exact state), skips redundant manifest write and refcount increment.
    ///
    /// Refcount lifecycle: decrements old committed root if different from new digest,
    /// increments new digest. Empty files (byte_count==0) set empty manifest without CAS push.
    pub fn test_release(&self, ino: u64, fh: u64) -> Result<(), i32> {
        let (state, byte_count, cas_committed, last_committed_root) = match self.open_files.lock().unwrap().remove(&fh) {
            Some(s) => (s.state, s.byte_count, s.cas_committed, s.last_committed_root),
            None => return Ok(()), // Already closed
        };

        // Empty file: set empty manifest (unless already committed via fsync)
        if byte_count == 0 {
            if !cas_committed {
                self.meta.set_manifest(ino, &[]).map_err(|_| libc::EIO)?;
            }
            return Ok(());
        }

        // Finalize the State (consumes it -- this is release, handle is closing)
        let new_digest: Digest224 = {
            let mut io = self.io.lock().unwrap();
            let mut fsa = FileStorageAdd::new(&mut *io);
            let d256 = state.end(&mut fsa);
            fsa.end(&d256)
        };

        // If the digest matches last committed root (fsync already committed this exact state),
        // skip redundant manifest write and refcount increment
        if last_committed_root == Some(new_digest) && cas_committed {
            return Ok(());
        }

        // Decrement old committed root if different
        if let Some(old) = last_committed_root {
            if old != new_digest {
                self.meta.decrement_refcount(&old);
            }
        }

        // Set manifest and increment refcount
        self.meta.set_manifest(ino, &[new_digest]).map_err(|_| libc::EIO)?;
        if last_committed_root != Some(new_digest) {
            self.meta.increment_refcount(&new_digest);
        }

        // Update inode size and mtime
        let mut inode = self.meta.get_inode(ino).map_err(|_| libc::EIO)?;
        inode.size = byte_count;
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

    /// Flush the write buffer for `fh` to CAS, then reset the buffer to empty.
    ///
    /// Unlike `test_release`, the file handle remains open after this call.
    /// Used by `test_fsync` and the FUSE `fsync()` callback.
    pub fn test_fsync(&self, ino: u64, fh: u64) -> Result<(), i32> {
        self.flush_buffer_for_fsync(ino, fh)?;
        // Flush WAL to disk — ensures all pending mutations are durable
        self.meta.flush_wal().map_err(|_| libc::EIO)?;
        Ok(())
    }

    /// Read `size` bytes from inode `ino` starting at `offset`.
    ///
    /// Supports read-during-write (STRM-03): checks for open write handles on
    /// the inode and materializes uncommitted content via clone+end+file_storage_get.
    /// Cross-handle reads work — a read-only handle sees uncommitted writes from
    /// a separate write handle on the same inode.
    ///
    /// Falls back to committed manifest when no open write handle exists.
    ///
    /// Lock ordering: open_files (clone State) -> io (materialize/read).
    ///
    /// Used by integration tests to bypass the FUSE request/reply layer.
    pub fn test_read(&self, ino: u64, offset: u64, size: u32) -> Result<Vec<u8>, i32> {
        // Check for open write handles on this inode (cross-handle read support)
        let writer_state: Option<(State, u64)> = {
            let open_files = self.open_files.lock().unwrap();
            open_files.values()
                .find(|s| s.ino == ino && s.byte_count > 0)
                .map(|s| (s.state.clone(), s.byte_count))
        };
        // open_files lock released

        let raw_bytes: Vec<u8> = if let Some((snapshot, _byte_count)) = writer_state {
            // Materialize uncommitted content via clone+end+file_storage_get
            let digest: Digest224 = {
                let mut io = self.io.lock().unwrap();
                let mut fsa = FileStorageAdd::new(&mut *io);
                let d256 = snapshot.end(&mut fsa);
                fsa.end(&d256)
            };
            let mut io = self.io.lock().unwrap();
            file_storage_get(&mut *io, &digest).ok_or(libc::EIO)?
        } else {
            // Fall back to committed manifest
            let manifest = self.meta.get_manifest(ino).map_err(|_| libc::EIO)?;
            if manifest.is_empty() { return Ok(vec![]); }
            let mut io = self.io.lock().unwrap();
            file_storage_get(&mut *io, &manifest[0]).ok_or(libc::EIO)?
        };

        let start = (offset as usize).min(raw_bytes.len());
        let end = (start + size as usize).min(raw_bytes.len());
        Ok(raw_bytes[start..end].to_vec())
    }

    /// Flush the streaming state for `fh` to CAS without closing the handle.
    ///
    /// Uses clone+end pattern: clones the in-progress State, calls end() on the
    /// clone to produce a Digest224 snapshot, sets manifest, keeps original State
    /// alive for further writes. After fsync, subsequent writes continue appending
    /// to the same State accumulator.
    ///
    /// Refcount lifecycle: decrements old committed root (if any and different),
    /// increments new root. Tracks `last_committed_root` for next decrement.
    ///
    /// If `fh` is not in `open_files` (read-only handle or invalid), returns Ok
    /// without error — fsync is a no-op for read-only handles.
    fn flush_buffer_for_fsync(&self, ino: u64, fh: u64) -> Result<(), i32> {
        // 1. Clone state snapshot under open_files lock
        let (state_snapshot, byte_count, old_root) = {
            let mut open_files = self.open_files.lock().unwrap();
            match open_files.get_mut(&fh) {
                Some(s) => {
                    if s.byte_count == 0 {
                        // Empty file -- nothing to flush
                        return Ok(());
                    }
                    s.cas_committed = true;
                    (s.state.clone(), s.byte_count, s.last_committed_root)
                }
                None => return Ok(()), // No write handle -- fsync is a no-op
            }
        };
        // open_files lock released here

        // 2. Materialize clone under io lock
        let new_digest: Digest224 = {
            let mut io = self.io.lock().unwrap();
            let mut fsa = FileStorageAdd::new(&mut *io);
            let d256 = state_snapshot.end(&mut fsa);
            fsa.end(&d256)
        };

        // 3. Decrement old committed root if any
        if let Some(old) = old_root {
            if old != new_digest {
                self.meta.decrement_refcount(&old);
            }
        }

        // 4. Set manifest and increment refcount (skip increment if same as old -- already counted)
        self.meta.set_manifest(ino, &[new_digest]).map_err(|_| libc::EIO)?;
        if old_root != Some(new_digest) {
            self.meta.increment_refcount(&new_digest);
        }

        // 5. Update last_committed_root
        {
            let mut open_files = self.open_files.lock().unwrap();
            if let Some(s) = open_files.get_mut(&fh) {
                s.last_committed_root = Some(new_digest);
            }
        }

        // 6. Update inode size and mtime
        let mut inode = self.meta.get_inode(ino).map_err(|_| libc::EIO)?;
        inode.size = byte_count;
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
    /// operates on the in-flight streaming State; otherwise reads from CAS, adjusts, re-pushes.
    pub fn test_setattr_size(&self, ino: u64, fh: Option<u64>, new_size: u64) -> Result<(), i32> {
        // Case A: open file handle — truncate in-flight streaming State
        if let Some(fh_val) = fh {
            let mut open_files = self.open_files.lock().unwrap();
            if let Some(s) = open_files.get_mut(&fh_val) {
                if new_size == 0 {
                    // Fast path: reset to empty State
                    let old_root = s.last_committed_root.take();
                    s.state = State::default();
                    s.byte_count = 0;
                    s.cas_committed = false;
                    drop(open_files); // Release before io/meta operations

                    // Decrement old committed root if any
                    if let Some(old) = old_root {
                        self.meta.decrement_refcount(&old);
                    }
                } else {
                    // Materialize current content from in-progress State
                    let snapshot = s.state.clone();
                    let current_byte_count = s.byte_count;
                    let old_root = s.last_committed_root.take();
                    drop(open_files); // Release before io lock

                    // Materialize to bytes
                    let mut content: Vec<u8> = if current_byte_count == 0 {
                        Vec::new()
                    } else {
                        let digest = {
                            let mut io = self.io.lock().unwrap();
                            let mut fsa = FileStorageAdd::new(&mut *io);
                            let d256 = snapshot.end(&mut fsa);
                            fsa.end(&d256)
                        };
                        let mut io = self.io.lock().unwrap();
                        file_storage_get(&mut *io, &digest).unwrap_or_default()
                    };

                    // Truncate or zero-extend
                    content.resize(new_size as usize, 0);

                    // Push into fresh State
                    let new_state_and_count = {
                        let mut io = self.io.lock().unwrap();
                        let mut fsa = FileStorageAdd::new(&mut *io);
                        let mut fresh = State::default();
                        fresh.push_bytes(&mut fsa, &content);
                        (fresh, content.len() as u64)
                    };

                    // Update OpenFileState
                    let mut open_files = self.open_files.lock().unwrap();
                    if let Some(s) = open_files.get_mut(&fh_val) {
                        s.state = new_state_and_count.0;
                        s.byte_count = new_state_and_count.1;
                        s.cas_committed = false;
                        s.last_committed_root = None;
                    }
                    drop(open_files);

                    // Decrement old committed root if any
                    if let Some(old) = old_root {
                        self.meta.decrement_refcount(&old);
                    }
                }

                // Update inode size immediately
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

        // Case B: closed file (or open file without FATTR_FH) — read from CAS, truncate/extend,
        // re-push. NotFound from get_manifest means the inode exists but has no content yet
        // (brand-new file created by create() before any write/release). Treat as empty content.
        let old_manifest = match self.meta.get_manifest(ino) {
            Ok(m) => m,
            Err(MetaError::NotFound(_)) => vec![],
            Err(_) => return Err(libc::EIO),
        };

        // Read current content (raw bytes — v3 format, no compression)
        let mut content: Vec<u8> = if old_manifest.is_empty() {
            Vec::new()
        } else {
            let root_digest = old_manifest[0];
            let raw_bytes: Vec<u8> = {
                let mut io = self.io.lock().unwrap();
                file_storage_get(&mut *io, &root_digest)
                    .ok_or(libc::EIO)?
            };
            raw_bytes
        };

        // Decrement old refcount
        if !old_manifest.is_empty() {
            self.meta.decrement_refcount(&old_manifest[0]);
        }

        // Truncate or zero-extend
        content.resize(new_size as usize, 0);

        // Push new content (raw bytes directly — v3 format, no compression)
        if content.is_empty() {
            self.meta.set_manifest(ino, &[]).map_err(|_| libc::EIO)?;
        } else {
            let new_digest = {
                let mut io = self.io.lock().unwrap();
                let mut fsa = FileStorageAdd::new(&mut *io);
                let digest = State::push_all(&mut fsa, &content);
                drop(fsa);
                digest
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

    /// Return `(blocks, bfree, bavail, files, ffree, bsize)` — the same values
    /// that `statfs()` passes to the kernel. Used by integration tests.
    pub fn test_statfs_values(&self) -> (u64, u64, u64, u64, u64, u32) {
        self.compute_statfs()
    }

    /// Compute statfs values — extracted so both `statfs()` and unit tests can call it.
    ///
    /// Three-tier space reporting:
    /// - blocks/bfree/bavail: host disk capacity via `libc::statvfs` on the store path
    /// - files: actual live inode count from `inode_count` AtomicU64
    /// - ffree: approximate free inode slots (u64::MAX - inode_count)
    ///
    /// Returns `(blocks, bfree, bavail, files, ffree, bsize)`.
    pub fn compute_statfs(&self) -> (u64, u64, u64, u64, u64, u32) {
        let files = self.meta.inode_count();
        let ffree = u64::MAX.saturating_sub(files);

        if let Some(ref sp) = self.store_path {
            // Convert path to CString for libc::statvfs
            use std::ffi::CString;
            let path_cstr = match sp.to_str().and_then(|s| CString::new(s).ok()) {
                Some(c) => c,
                None => return (0, 0, 0, files, ffree, 4096),
            };
            let mut sv: libc::statvfs = unsafe { std::mem::zeroed() };
            let ret = unsafe { libc::statvfs(path_cstr.as_ptr(), &mut sv) };
            if ret == 0 {
                let bsize = sv.f_frsize as u32;
                let blocks = sv.f_blocks as u64;
                let bfree = sv.f_bfree as u64;
                let bavail = sv.f_bavail as u64;
                return (blocks, bfree, bavail, files, ffree, bsize);
            }
            // statvfs failed — fall through to graceful fallback
        }

        // Graceful fallback when store_path is None or statvfs fails.
        (0, 0, 0, files, ffree, 4096)
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

        // Store target as CAS content (raw bytes — v3 format, no compression)
        let content_digest = {
            let mut io = self.io.lock().unwrap();
            let mut fsa = FileStorageAdd::new(&mut *io);
            let digest = State::push_all(&mut fsa, target_bytes);
            drop(fsa);
            digest
        };
        self.meta
            .set_manifest(ino, &[content_digest])
            .map_err(|_| libc::EIO)?;
        self.meta.increment_refcount(&content_digest);

        // Update inode size to raw target length (uncompressed)
        let mut inode = self.meta.get_inode(ino).map_err(|_| libc::EIO)?;
        inode.size = target_bytes.len() as u64;
        self.meta.update_inode(&inode).map_err(|_| libc::EIO)?;

        Ok(ino)
    }

    /// Read the target of a symbolic link inode.
    ///
    /// Loads the manifest, reads raw bytes from the CAS, and returns as String.
    pub fn simulate_readlink(&self, ino: u64) -> Result<String, i32> {
        let manifest = self.meta.get_manifest(ino).map_err(|_| libc::EINVAL)?;
        if manifest.is_empty() {
            return Ok(String::new());
        }
        let root_digest = manifest[0];
        let raw_bytes: Vec<u8> = {
            let mut io = self.io.lock().unwrap();
            file_storage_get(&mut *io, &root_digest)
                .ok_or(libc::EINVAL)?
        };
        String::from_utf8(raw_bytes).map_err(|_| libc::EINVAL)
    }
}

impl Filesystem for SliceFsFilesystem {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        // Advertise FUSE_ATOMIC_O_TRUNC so that FUSE-T passes O_TRUNC directly in
        // the create()/open() flags rather than sending a separate setattr(size=0)
        // after the create. Without this, FUSE-T sends setattr(size=0) for every
        // O_CREAT|O_TRUNC open, which fails with EIO on a brand-new inode (no manifest
        // entry yet) and causes FUSE-T's NFS layer to stall/hang indefinitely.
        let _ = config.add_capabilities(InitFlags::FUSE_ATOMIC_O_TRUNC);
        Ok(())
    }

    fn destroy(&mut self) {
        // Commit the final root — this logs all remaining dict entries and a RootUpdate
        // to the WAL segment, making the state recoverable after restart.
        if let Ok(_root) = self.meta.commit() {
            // Auto-snapshot on clean unmount if enabled via --auto-snapshot.
            if self.auto_snapshot {
                let _ = self.meta.create_snapshot(Some("auto-unmount".to_string()));
            }
            // Flush and close the WAL — writes EofMarker and calls sync_all.
            // The mount.lock file is removed by the MountLock RAII guard in run_mount.
            let _ = self.meta.shutdown_wal();
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
                Err(e) => {
                    reply.error(meta_error_to_fuse_errno(&e));
                }
            },
            Err(e) => {
                reply.error(meta_error_to_fuse_errno(&e));
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
        match self.test_read(ino.0, offset, size) {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        use fuser::OpenAccMode;
        // On macOS with FUSE-T, FOPEN_PURGE_UBC instructs the macOS Unified Buffer Cache
        // to discard any cached data for this file handle on open. Without this, the NFS
        // client may serve stale reads from UBC even after new writes have been committed.
        #[cfg(target_os = "macos")]
        let base_flags = FopenFlags::FOPEN_PURGE_UBC;
        #[cfg(not(target_os = "macos"))]
        let base_flags = FopenFlags::empty();

        let mode = flags.acc_mode();
        if mode == OpenAccMode::O_WRONLY || mode == OpenAccMode::O_RDWR {
            let fh = self.next_fh.fetch_add(1, Ordering::Relaxed) + 1;
            self.open_files.lock().unwrap().insert(
                fh,
                OpenFileState {
                    ino: ino.0,
                    write_mode: WriteMode::Streaming { state: State::default(), next_expected_offset: 0 },
                    byte_count: 0,
                    cas_committed: false,
                    last_committed_root: None,
                },
            );

            // FUSE_ATOMIC_O_TRUNC: when we advertise this capability in init(), the kernel
            // passes O_TRUNC directly here. Handle it by truncating the inode to size 0.
            // For an existing file opened with O_TRUNC this empties the content immediately
            // so subsequent reads on the open handle see an empty file.
            if flags.0 & libc::O_TRUNC != 0 {
                if let Err(e) = self.test_setattr_size(ino.0, Some(fh), 0) {
                    reply.error(Errno::from_i32(e));
                    // Clean up the fh we just inserted
                    self.open_files.lock().unwrap().remove(&fh);
                    return;
                }
            }

            reply.opened(FileHandle(fh), base_flags);
        } else {
            reply.opened(FileHandle(0), base_flags);
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
        flush: bool,
        reply: ReplyEmpty,
    ) {
        if fh.0 == 0 {
            // Read-only handle — nothing to flush
            return reply.ok();
        }
        match self.test_release(ino.0, fh.0) {
            Ok(()) => {
                reply.ok();
            }
            Err(_) => {
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
        // flush_buffer_for_fsync flushes the write buffer to CAS and resets it to empty,
        // leaving the file handle open for further writes (correct flush semantics).
        match self.flush_buffer_for_fsync(ino.0, fh.0) {
            Ok(()) => {
                reply.ok();
            }
            Err(_) => {
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
        match self.test_fsync(ino.0, fh.0) {
            Ok(()) => reply.ok(),
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let (blocks, bfree, bavail, files, ffree, bsize) = self.compute_statfs();
        reply.statfs(blocks, bfree, bavail, files, ffree, bsize, 255, 0);
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
        let name_str = name.to_str().unwrap_or("");
        match self.meta.set_xattr(ino.0, name_str, value) {
            Ok(()) => {
                reply.ok();
            }
            Err(e) => {
                reply.error(meta_error_to_fuse_errno(&e));
            }
        }
    }

    fn removexattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let name_str = name.to_str().unwrap_or("");
        match self.meta.remove_xattr(ino.0, name_str) {
            Ok(()) => {
                reply.ok();
            }
            Err(MetaError::NotFound(_)) => {
                // Attribute did not exist — POSIX says ENOATTR (same value as ENODATA on Linux).
                // fuser exports this as Errno::NO_XATTR on macOS.
                reply.error(Errno::NO_XATTR);
            }
            Err(e) => {
                reply.error(meta_error_to_fuse_errno(&e));
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
        match self.test_write(fh.0, offset, data) {
            Ok(n) => {
                reply.written(n);
            }
            Err(_) => {
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
        // On macOS with FUSE-T, FOPEN_PURGE_UBC ensures the NFS UBC discards any
        // previously cached data for this inode when the file is created/opened.
        #[cfg(target_os = "macos")]
        let fopen_flags = FopenFlags::FOPEN_PURGE_UBC;
        #[cfg(not(target_os = "macos"))]
        let fopen_flags = FopenFlags::empty();

        let name_str = match name.to_str() {
            Some(s) => s,
            None => return reply.error(Errno::EINVAL),
        };
        match self.test_create(parent.0, name_str, mode, umask, req.uid(), req.gid()) {
            Ok((ino, fh)) => {
                // With FUSE_ATOMIC_O_TRUNC advertised, the kernel passes O_TRUNC in
                // create() flags. For a new file this is a no-op (nothing to truncate),
                // but we handle it explicitly for correctness and to avoid a separate
                // setattr(size=0) from FUSE-T.
                if flags & libc::O_TRUNC != 0 {
                    if let Err(e) = self.test_setattr_size(ino, Some(fh), 0) {
                        reply.error(Errno::from_i32(e));
                        self.open_files.lock().unwrap().remove(&fh);
                        return;
                    }
                }
                match self.meta.get_inode(ino) {
                    Ok(meta) => {
                        let attr = inode_to_file_attr(&meta);
                        reply.created(&TTL, &attr, Generation(0), FileHandle(fh), fopen_flags);
                    }
                    Err(e) => {
                        reply.error(meta_error_to_fuse_errno(&e));
                    }
                }
            }
            Err(errno) => {
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
    use blockset::file_storage_get;
    use metadata::store::DictMetadataStore;
    use metadata::store_io::StoreIo;
    use slicefs_traits::metadata::{InodeMeta, MetadataStore};
    use tempfile::TempDir;

    const S_IFDIR_TEST: u32 = 0o040_000;
    const S_IFREG_TEST: u32 = 0o100_000;

    fn fresh_fs() -> (SliceFsFilesystem, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let io = Arc::new(Mutex::new(StoreIo::new(dir.path())));
        let meta = DictMetadataStore::new(io.clone());
        let fs = SliceFsFilesystem::new(meta, io, None);
        (fs, dir)
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
        let (fs, _dir) = fresh_fs();
        let meta = fs.meta.get_inode(1).unwrap();
        let attr = inode_to_file_attr(&meta);
        assert_eq!(attr.ino, INodeNo(1));
        assert_eq!(attr.kind, FileType::Directory);
    }

    #[test]
    fn test_lookup_nonexistent_returns_notfound() {
        let (fs, _dir) = fresh_fs();
        let result = fs.meta.lookup(1, "nonexistent_file");
        assert!(result.is_err());
        let errno = meta_error_to_errno(&result.unwrap_err());
        assert_eq!(errno, libc::ENOENT);
    }

    #[test]
    fn test_readdir_root_contains_dot_entries() {
        let (fs, _dir) = fresh_fs();
        let entries = fs.meta.list_directory(1).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"."), "root must contain '.'");
        assert!(names.contains(&".."), "root must contain '..'");
    }

    #[test]
    fn test_read_returns_correct_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let io = Arc::new(Mutex::new(StoreIo::new(dir.path())));
        let content = b"hello, SliceFS content!";
        // Push content into FileStorage
        let root_digest224 = {
            let mut io_guard = io.lock().unwrap();
            let mut fsa = FileStorageAdd::new(&mut *io_guard);
            let digest = State::push_all(&mut fsa, content);
            drop(fsa);
            digest
        };

        let meta_store = DictMetadataStore::new(io.clone());
        let file_meta = InodeMeta::new_file(0, 0, 0, S_IFREG_TEST | 0o644);
        let ino = meta_store.create_inode(&file_meta).unwrap();
        meta_store.link(1, "testfile", ino).unwrap();
        meta_store.set_manifest(ino, &[root_digest224]).unwrap();

        let fs = SliceFsFilesystem::new(meta_store, io.clone(), None);

        let manifest = fs.meta.get_manifest(ino).unwrap();
        assert!(!manifest.is_empty());

        let bytes = {
            let mut io_guard = io.lock().unwrap();
            file_storage_get(&mut *io_guard, &manifest[0]).unwrap()
        };

        assert_eq!(bytes, content);
    }

    #[test]
    fn test_read_with_offset() {
        let dir = tempfile::tempdir().unwrap();
        let io = Arc::new(Mutex::new(StoreIo::new(dir.path())));
        let content = b"hello, SliceFS offset test!";
        let root_digest224 = {
            let mut io_guard = io.lock().unwrap();
            let mut fsa = FileStorageAdd::new(&mut *io_guard);
            let digest = State::push_all(&mut fsa, content);
            drop(fsa);
            digest
        };

        let meta_store = DictMetadataStore::new(io.clone());
        let file_meta = InodeMeta::new_file(0, 0, 0, S_IFREG_TEST | 0o644);
        let ino = meta_store.create_inode(&file_meta).unwrap();
        meta_store.set_manifest(ino, &[root_digest224]).unwrap();

        let fs = SliceFsFilesystem::new(meta_store, io.clone(), None);

        let manifest = fs.meta.get_manifest(ino).unwrap();
        let bytes = {
            let mut io_guard = io.lock().unwrap();
            file_storage_get(&mut *io_guard, &manifest[0]).unwrap()
        };
        let offset = 7usize;
        let bytes_from_offset = &bytes[offset..];

        assert_eq!(bytes_from_offset, &content[offset..]);
    }

    #[test]
    fn test_statfs_files_not_hardcoded() {
        // statfs files field should reflect actual inode count, not hardcoded 1_000_000.
        // Fresh store has 1 inode (root), so inode_count() == 1 != 1_000_000.
        let dir = tempfile::TempDir::new().unwrap();
        let io = Arc::new(std::sync::Mutex::new(
            metadata::store_io::StoreIo::new(dir.path()),
        ));
        let meta = metadata::store::DictMetadataStore::new(io.clone());
        let fs = super::SliceFsFilesystem::new(
            meta,
            io,
            Some(dir.path().to_path_buf()),
        );
        let (_blocks, _bfree, _bavail, files, _ffree, _bsize) = fs.test_statfs_values();
        assert_ne!(files, 1_000_000, "files must not be hardcoded 1_000_000");
        assert_eq!(files, 1, "fresh store files must equal inode_count (1)");
    }
}
