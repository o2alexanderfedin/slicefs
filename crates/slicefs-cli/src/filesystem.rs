//! `SliceFsFilesystem` — FUSE adapter for SliceFS.
//!
//! Implements `fuser::Filesystem` using `DictMetadataStore` for inode/directory/manifest
//! lookups and blockset `GetBytes` for file content reads.
//!
//! Write callbacks (create, write, release, setattr) are fully implemented in Phase 4.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::util::ts;

use blockset::{Digest224, FileStorageAdd, State, StorageAdd, Tree, file_storage_get};
use fuser::{Errno, FileAttr, FileType, INodeNo};
use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use slicefs_traits::metadata::{InodeMeta, MetaError, MetadataStore};
use tracing::debug;

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
    pub(crate) store_path: Option<PathBuf>,
    pub(crate) auto_snapshot: bool,
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
    #[allow(dead_code)] // public lib API; consumed by tests + future code paths
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
    let blocks = meta.size.div_ceil(512);

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
#[allow(dead_code)] // public lib API; consumed by tests
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
pub(crate) const TTL: Duration = Duration::from_secs(1);

/// Recursively sum file sizes under `dir`.
///
/// Used by `statfs` to compute physical bytes from the `vt0/` CAS directory.
#[allow(dead_code)] // referenced by future statfs path; retained for symmetry
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
        let ino = self
            .meta
            .create_inode(&file_meta)
            .map_err(|e| meta_error_to_errno(&e))?;
        if let Err(e) = self.meta.link(parent_ino, name, ino) {
            let _ = self.meta.delete_inode(ino);
            return Err(meta_error_to_errno(&e));
        }
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed) + 1;
        self.open_files.lock().unwrap().insert(
            fh,
            OpenFileState {
                ino,
                write_mode: WriteMode::Streaming {
                    state: State::default(),
                    next_expected_offset: 0,
                },
                byte_count: 0,
                cas_committed: false,
                last_committed_root: None,
            },
        );
        Ok((ino, fh))
    }

    /// Write `data` at `offset` into the file handle `fh`. Returns bytes written.
    ///
    /// Dual dispatch on WriteMode:
    /// - Streaming: sequential writes via push_bytes (O(log N) memory).
    ///   If offset != next_expected_offset, triggers one-way fallback to Buffered.
    /// - Buffered: standard pwrite semantics into Vec<u8> (O(N) memory).
    ///
    /// Lock ordering: open_files is released before io to prevent contention.
    /// Sequential writes: temporarily remove OpenFileState from the map, push bytes
    /// under io lock only, then re-insert. This prevents holding open_files during
    /// potentially slow CAS I/O (which would block concurrent reads on other fh's).
    /// Fallback transition: release open_files before io for materialization,
    /// re-acquire open_files to swap WriteMode. FUSE serializes per-fh writes.
    pub fn test_write(&self, fh: u64, offset: u64, data: &[u8]) -> Result<u32, i32> {
        // Take the file state out of the map so we can release open_files before io.
        let mut file_state = match self.open_files.lock().unwrap().remove(&fh) {
            Some(s) => s,
            None => return Err(libc::EBADF),
        };
        // open_files lock is released here.

        let result = match &mut file_state.write_mode {
            WriteMode::Streaming {
                state,
                next_expected_offset,
            } => {
                if offset != *next_expected_offset {
                    // Fallback from Streaming to Buffered mode
                    let snapshot = state.clone();
                    let byte_count = file_state.byte_count;
                    let ino = file_state.ino;
                    let old_root = file_state.last_committed_root.take();

                    debug!(
                        fh = fh,
                        ino = ino,
                        offset = offset,
                        expected = byte_count,
                        "fallback from Streaming to Buffered mode"
                    );

                    // Materialize current streaming content (or load from committed)
                    let mut buf = if byte_count > 0 {
                        let digest: Digest224 = {
                            let mut io = self.io.lock().unwrap();
                            let mut fsa = FileStorageAdd::new(&mut *io);
                            let d256 = snapshot.end(&mut fsa);
                            fsa.end(&d256)
                        };
                        let mut io = self.io.lock().unwrap();
                        file_storage_get(&mut *io, &digest).unwrap_or_default()
                    } else {
                        // No streaming content yet -- load from committed manifest
                        // (prevents existing file content loss per Research Pitfall 1)
                        match self.meta.get_manifest(ino) {
                            Ok(m) if !m.is_empty() => {
                                let mut io = self.io.lock().unwrap();
                                file_storage_get(&mut *io, &m[0]).unwrap_or_default()
                            }
                            _ => Vec::new(),
                        }
                    };

                    // Apply pwrite semantics
                    let end = offset as usize + data.len();
                    if end > buf.len() {
                        buf.resize(end, 0);
                    }
                    buf[offset as usize..end].copy_from_slice(data);

                    // Swap to Buffered mode
                    let buf_len = buf.len() as u64;
                    file_state.write_mode = WriteMode::Buffered { buf };
                    file_state.byte_count = buf_len;
                    file_state.cas_committed = false;

                    // Decrement old committed root
                    if let Some(old) = old_root {
                        self.meta.decrement_refcount(&old);
                    }

                    Ok(data.len() as u32)
                } else {
                    // Sequential write -- push bytes under io lock only (open_files not held)
                    let io_err = {
                        let mut io = self.io.lock().unwrap();
                        let mut fsa = FileStorageAdd::new(&mut *io);
                        state.push_bytes(&mut fsa, data);
                        // Take error before fsa drops (drop may flush more nodes).
                        // After drop, any additional errors are lost but the first is captured.
                        let err = fsa.take_io_error();
                        drop(fsa);
                        err
                    };
                    if let Some(e) = io_err {
                        eprintln!(
                            "[{}][FUSE] test_write: CAS I/O error during push_bytes: {}",
                            ts(),
                            e
                        );
                        // Re-insert state before returning error
                        self.open_files.lock().unwrap().insert(fh, file_state);
                        return Err(libc::EIO);
                    }
                    *next_expected_offset += data.len() as u64;
                    file_state.byte_count += data.len() as u64;
                    file_state.cas_committed = false;
                    Ok(data.len() as u32)
                }
            }
            WriteMode::Buffered { buf } => {
                // Standard pwrite into buffer -- no io lock needed
                let end = offset as usize + data.len();
                if end > buf.len() {
                    buf.resize(end, 0);
                }
                buf[offset as usize..end].copy_from_slice(data);
                file_state.byte_count = buf.len() as u64;
                file_state.cas_committed = false;
                Ok(data.len() as u32)
            }
        };

        // Re-insert the file state back into the map.
        self.open_files.lock().unwrap().insert(fh, file_state);
        result
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
        let (write_mode, byte_count, cas_committed, last_committed_root) =
            match self.open_files.lock().unwrap().remove(&fh) {
                Some(s) => (
                    s.write_mode,
                    s.byte_count,
                    s.cas_committed,
                    s.last_committed_root,
                ),
                None => return Ok(()), // Already closed
            };

        // No bytes written through this handle.
        // Only set empty manifest if this is a genuinely new file (no existing manifest).
        // FUSE-T on macOS may open existing files with O_RDWR even for read-only access
        // (issue #15). Setting an empty manifest here would erase the file's content.
        if byte_count == 0 {
            if !cas_committed {
                // Check if the file already has content — if so, don't overwrite it.
                let has_existing = self
                    .meta
                    .get_manifest(ino)
                    .map(|m| !m.is_empty())
                    .unwrap_or(false);
                if !has_existing {
                    self.meta.set_manifest(ino, &[]).map_err(|_| libc::EIO)?;
                }
            }
            return Ok(());
        }

        // Finalize to CAS based on write mode
        let new_digest: Digest224 = match write_mode {
            WriteMode::Streaming { state, .. } => {
                let mut io = self.io.lock().unwrap();
                let mut fsa = FileStorageAdd::new(&mut *io);
                let d256 = state.end(&mut fsa);
                fsa.end(&d256)
            }
            WriteMode::Buffered { buf } => {
                let mut io = self.io.lock().unwrap();
                let mut fsa = FileStorageAdd::new(&mut *io);
                let mut fresh = State::default();
                fresh.push_bytes(&mut fsa, &buf);
                let d256 = fresh.end(&mut fsa);
                fsa.end(&d256)
            }
        };

        // If the digest matches last committed root (fsync already committed this exact state),
        // skip redundant manifest write and refcount increment
        if last_committed_root == Some(new_digest) && cas_committed {
            return Ok(());
        }

        // Decrement old committed root if different
        if let Some(old) = last_committed_root
            && old != new_digest
        {
            self.meta.decrement_refcount(&old);
        }

        // Set manifest and increment refcount
        self.meta
            .set_manifest(ino, &[new_digest])
            .map_err(|_| libc::EIO)?;
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
        // Check for open write handles on this inode (cross-handle read support).
        // In Buffered mode, clone buf directly (no CAS roundtrip needed).
        // In Streaming mode, clone State for materialization.
        enum WriterSnapshot {
            Streaming(State),
            Buffered(Vec<u8>),
        }

        let writer_snapshot: Option<WriterSnapshot> = {
            let open_files = self.open_files.lock().unwrap();
            open_files
                .values()
                .find(|s| s.ino == ino && s.byte_count > 0)
                .map(|s| match &s.write_mode {
                    WriteMode::Streaming { state, .. } => WriterSnapshot::Streaming(state.clone()),
                    WriteMode::Buffered { buf } => WriterSnapshot::Buffered(buf.clone()),
                })
        };
        // open_files lock released

        let raw_bytes: Vec<u8> = match writer_snapshot {
            Some(WriterSnapshot::Buffered(buf)) => buf,
            Some(WriterSnapshot::Streaming(snapshot)) => {
                // Materialize uncommitted content via clone+end+file_storage_get
                let digest: Digest224 = {
                    let mut io = self.io.lock().unwrap();
                    let mut fsa = FileStorageAdd::new(&mut *io);
                    let d256 = snapshot.end(&mut fsa);
                    fsa.end(&d256)
                };
                let mut io = self.io.lock().unwrap();
                file_storage_get(&mut *io, &digest).ok_or(libc::EIO)?
            }
            None => {
                // Fall back to committed manifest
                let manifest = self.meta.get_manifest(ino).map_err(|_| libc::EIO)?;
                if manifest.is_empty() {
                    return Ok(vec![]);
                }
                let mut io = self.io.lock().unwrap();
                file_storage_get(&mut *io, &manifest[0]).ok_or(libc::EIO)?
            }
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
        // 1. Clone state/buf snapshot under open_files lock
        enum FsyncSnapshot {
            Streaming(State),
            Buffered(Vec<u8>),
        }

        let (snapshot, byte_count, old_root) = {
            let mut open_files = self.open_files.lock().unwrap();
            match open_files.get_mut(&fh) {
                Some(s) => {
                    if s.byte_count == 0 {
                        // Empty file -- nothing to flush
                        return Ok(());
                    }
                    s.cas_committed = true;
                    let snap = match &s.write_mode {
                        WriteMode::Streaming { state, .. } => {
                            FsyncSnapshot::Streaming(state.clone())
                        }
                        WriteMode::Buffered { buf } => FsyncSnapshot::Buffered(buf.clone()),
                    };
                    (snap, s.byte_count, s.last_committed_root)
                }
                None => return Ok(()), // No write handle -- fsync is a no-op
            }
        };
        // open_files lock released here

        // 2. Materialize under io lock
        let new_digest: Digest224 = match snapshot {
            FsyncSnapshot::Streaming(state_snapshot) => {
                let mut io = self.io.lock().unwrap();
                let mut fsa = FileStorageAdd::new(&mut *io);
                let d256 = state_snapshot.end(&mut fsa);
                fsa.end(&d256)
            }
            FsyncSnapshot::Buffered(buf_clone) => {
                let mut io = self.io.lock().unwrap();
                let mut fsa = FileStorageAdd::new(&mut *io);
                let mut fresh = State::default();
                fresh.push_bytes(&mut fsa, &buf_clone);
                let d256 = fresh.end(&mut fsa);
                fsa.end(&d256)
            }
        };

        // 3. Decrement old committed root if any
        if let Some(old) = old_root
            && old != new_digest
        {
            self.meta.decrement_refcount(&old);
        }

        // 4. Set manifest and increment refcount (skip increment if same as old -- already counted)
        self.meta
            .set_manifest(ino, &[new_digest])
            .map_err(|_| libc::EIO)?;
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
    /// operates on the in-flight write state; otherwise reads from CAS, adjusts, re-pushes.
    ///
    /// Dual dispatch on WriteMode:
    /// - Streaming: materialize via clone+end, resize, repush into fresh State.
    /// - Buffered: simple buf.resize() (no CAS operations needed for non-zero truncate).
    pub fn test_setattr_size(&self, ino: u64, fh: Option<u64>, new_size: u64) -> Result<(), i32> {
        // Case A: open file handle — truncate in-flight write state
        if let Some(fh_val) = fh {
            let mut open_files = self.open_files.lock().unwrap();
            if let Some(s) = open_files.get_mut(&fh_val) {
                match &mut s.write_mode {
                    WriteMode::Buffered { buf } => {
                        if new_size == 0 {
                            let old_root = s.last_committed_root.take();
                            *buf = Vec::new();
                            s.byte_count = 0;
                            s.cas_committed = false;
                            drop(open_files);

                            if let Some(old) = old_root {
                                self.meta.decrement_refcount(&old);
                            }
                        } else {
                            buf.resize(new_size as usize, 0);
                            s.byte_count = buf.len() as u64;
                            s.cas_committed = false;
                            drop(open_files);
                        }
                    }
                    WriteMode::Streaming { state, .. } => {
                        if new_size == 0 {
                            // Fast path: reset to empty Streaming State
                            let old_root = s.last_committed_root.take();
                            s.write_mode = WriteMode::Streaming {
                                state: State::default(),
                                next_expected_offset: 0,
                            };
                            s.byte_count = 0;
                            s.cas_committed = false;
                            drop(open_files);

                            if let Some(old) = old_root {
                                self.meta.decrement_refcount(&old);
                            }
                        } else {
                            // Materialize current content from in-progress State
                            let snapshot = state.clone();
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
                                s.write_mode = WriteMode::Streaming {
                                    state: new_state_and_count.0,
                                    next_expected_offset: new_state_and_count.1,
                                };
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
                file_storage_get(&mut *io, &root_digest).ok_or(libc::EIO)?
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
            self.meta
                .set_manifest(ino, &[new_digest])
                .map_err(|_| libc::EIO)?;
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
    #[allow(dead_code)] // public lib API; consumed by tests
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
    #[allow(dead_code)] // public lib API; consumed by tests
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
    #[allow(dead_code)] // public lib API; consumed by tests
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

    /// Create a regular file inode via mknod (without opening it).
    ///
    /// Unlike `test_create()`, this does NOT allocate a file handle.
    /// The caller must issue a separate `open()` to get a writable handle.
    /// Returns the new inode number on success.
    ///
    /// Non-regular file types (device nodes, FIFOs) return ENOSYS.
    pub fn test_mknod(
        &self,
        parent: u64,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
        umask: u32,
    ) -> Result<u64, i32> {
        let file_type = mode & S_IFMT;
        if file_type != S_IFREG && file_type != 0 {
            return Err(libc::ENOSYS);
        }
        let file_mode = S_IFREG | (mode & !umask & 0o7777);
        let file_meta = InodeMeta::new_file(0, uid, gid, file_mode);
        let ino = self
            .meta
            .create_inode(&file_meta)
            .map_err(|e| meta_error_to_errno(&e))?;
        if let Err(e) = self.meta.link(parent, name, ino) {
            let _ = self.meta.delete_inode(ino);
            return Err(meta_error_to_errno(&e));
        }
        Ok(ino)
    }

    /// Return `(blocks, bfree, bavail, files, ffree, bsize)` — the same values
    /// that `statfs()` passes to the kernel. Used by integration tests.
    #[allow(dead_code)] // public lib API; consumed by tests
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
                let bsize = if sv.f_frsize > 0 {
                    sv.f_frsize as u32
                } else {
                    4096
                };
                let blocks = sv.f_blocks as u64;
                let bfree = sv.f_bfree as u64;
                let bavail = sv.f_bavail as u64;
                // Some filesystems (exFAT on macOS) return 0 for block counts
                // via statvfs even though space is available. Fall through to
                // statfs(2) which uses a different struct and often works.
                if blocks > 0 {
                    return (blocks, bfree, bavail, files, ffree, bsize);
                }
            }
            // statvfs returned zeros or failed — try statfs(2) as fallback.
            // On macOS, statfs uses a different struct that handles exFAT correctly.
            let mut sf: libc::statfs = unsafe { std::mem::zeroed() };
            let ret2 = unsafe { libc::statfs(path_cstr.as_ptr(), &mut sf) };
            if ret2 == 0 && sf.f_blocks > 0 {
                let bsize = if sf.f_bsize > 0 {
                    sf.f_bsize as u32
                } else {
                    4096
                };
                let blocks = sf.f_blocks as u64;
                let bfree = sf.f_bfree as u64;
                let bavail = sf.f_bavail as u64;
                return (blocks, bfree, bavail, files, ffree, bsize);
            }
            // Both failed — fall through to graceful fallback
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
        let inode = self
            .meta
            .get_inode(ino)
            .map_err(|e| meta_error_to_errno(&e))?;
        if inode.mode & S_IFDIR == 0 {
            return Err(libc::ENOTDIR);
        }

        // Check emptiness: list_directory returns . and .. plus any real entries
        let entries = self
            .meta
            .list_directory(ino)
            .map_err(|e| meta_error_to_errno(&e))?;
        // . and .. always present — anything beyond that is ENOTEMPTY
        let real_entries = entries
            .iter()
            .filter(|e| e.name != "." && e.name != "..")
            .count();
        if real_entries > 0 {
            return Err(libc::ENOTEMPTY);
        }

        // Remove directory entry from parent and delete directory inode
        self.meta
            .unlink(parent_ino, name)
            .map_err(|e| meta_error_to_errno(&e))?;
        self.meta
            .delete_inode(ino)
            .map_err(|e| meta_error_to_errno(&e))?;

        // Decrement parent nlinks (removing the .. backlink from the deleted subdir)
        let mut parent_inode = self
            .meta
            .get_inode(parent_ino)
            .map_err(|e| meta_error_to_errno(&e))?;
        if parent_inode.nlinks > 0 {
            parent_inode.nlinks -= 1;
            self.meta
                .update_inode(&parent_inode)
                .map_err(|e| meta_error_to_errno(&e))?;
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
        let mut inode = self
            .meta
            .get_inode(ino)
            .map_err(|e| meta_error_to_errno(&e))?;

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
            self.meta
                .update_inode(&inode)
                .map_err(|e| meta_error_to_errno(&e))?;
        }
        Ok(())
    }

    /// Create a hard link: add `newname` in `newparent_ino` pointing at `ino`.
    ///
    /// POSIX disallows hard links to directories — returns EPERM.
    /// Returns the new inode number (same as `ino`).
    pub fn simulate_link(&self, ino: u64, newparent_ino: u64, newname: &str) -> Result<u64, i32> {
        // Get source inode
        let mut inode = self
            .meta
            .get_inode(ino)
            .map_err(|e| meta_error_to_errno(&e))?;

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
        self.meta
            .update_inode(&inode)
            .map_err(|e| meta_error_to_errno(&e))?;

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
        if flags & 1 != 0 && target_ino.is_some() {
            return Err(libc::EEXIST);
        }

        // If target exists and we're doing a normal rename, remove the old target
        if let Some(dst_ino) = target_ino {
            let dst_inode = self
                .meta
                .get_inode(dst_ino)
                .map_err(|e| meta_error_to_errno(&e))?;
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
        let mut src_inode = self
            .meta
            .get_inode(src_ino)
            .map_err(|e| meta_error_to_errno(&e))?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        src_inode.ctime_sec = now.as_secs() as i64;
        src_inode.ctime_nsec = now.subsec_nanos();
        self.meta
            .update_inode(&src_inode)
            .map_err(|e| meta_error_to_errno(&e))?;

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
            file_storage_get(&mut *io, &root_digest).ok_or(libc::EINVAL)?
        };
        String::from_utf8(raw_bytes).map_err(|_| libc::EINVAL)
    }

    // ── Testable wrappers for FUSE callbacks ─────────────────────────────────

    /// Get inode attributes. Returns `InodeMeta` on success.
    pub fn test_getattr(&self, ino: u64) -> Result<InodeMeta, MetaError> {
        self.meta.get_inode(ino)
    }

    /// Lookup a name in a directory. Returns `(child_ino, InodeMeta)` on success.
    pub fn test_lookup(&self, parent: u64, name: &str) -> Result<(u64, InodeMeta), i32> {
        let child_ino = self
            .meta
            .lookup(parent, name)
            .map_err(|e| meta_error_to_errno(&e))?;
        let meta = self
            .meta
            .get_inode(child_ino)
            .map_err(|e| meta_error_to_errno(&e))?;
        Ok((child_ino, meta))
    }

    /// List directory entries starting at `offset`.
    /// Returns `Vec<(ino, kind, name)>` tuples.
    pub fn test_readdir(&self, ino: u64, offset: u64) -> Result<Vec<(u64, FileType, String)>, i32> {
        let entries = self
            .meta
            .list_directory(ino)
            .map_err(|e| meta_error_to_errno(&e))?;
        let mut result = Vec::new();
        for entry in entries.iter().skip(offset as usize) {
            let kind = match self.meta.get_inode(entry.ino) {
                Ok(m) => match m.mode & S_IFMT {
                    S_IFDIR => FileType::Directory,
                    S_IFLNK => FileType::Symlink,
                    _ => FileType::RegularFile,
                },
                Err(_) => FileType::RegularFile,
            };
            result.push((entry.ino, kind, entry.name.clone()));
        }
        Ok(result)
    }

    /// Open a file. Returns `(fh, is_write)` on success.
    /// If `O_TRUNC` is set and the file is opened for writing, truncates to size 0.
    pub fn test_open(&self, ino: u64, flags: i32) -> Result<(u64, bool), i32> {
        let acc_mode = flags & libc::O_ACCMODE;
        let is_write = acc_mode == libc::O_WRONLY || acc_mode == libc::O_RDWR;

        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed) + 1;

        if is_write {
            self.open_files.lock().unwrap().insert(
                fh,
                OpenFileState {
                    ino,
                    write_mode: WriteMode::Streaming {
                        state: State::default(),
                        next_expected_offset: 0,
                    },
                    byte_count: 0,
                    cas_committed: false,
                    last_committed_root: None,
                },
            );

            // FUSE_ATOMIC_O_TRUNC
            if flags & libc::O_TRUNC != 0
                && let Err(e) = self.test_setattr_size(ino, Some(fh), 0)
            {
                self.open_files.lock().unwrap().remove(&fh);
                return Err(e);
            }
        }

        Ok((fh, is_write))
    }

    /// Get an extended attribute value. Returns the raw bytes.
    pub fn test_getxattr(&self, ino: u64, name: &str) -> Result<Vec<u8>, i32> {
        self.meta.get_xattr(ino, name).map_err(|_| libc::ENODATA)
    }

    /// List extended attribute names for an inode.
    /// Returns a null-separated byte buffer of attribute names.
    pub fn test_listxattr(&self, ino: u64) -> Result<Vec<u8>, i32> {
        let names = self
            .meta
            .list_xattrs(ino)
            .map_err(|e| meta_error_to_errno(&e))?;
        let mut buf = Vec::new();
        for name in &names {
            buf.extend_from_slice(name.as_bytes());
            buf.push(0);
        }
        Ok(buf)
    }

    /// Set an extended attribute.
    pub fn test_setxattr(&self, ino: u64, name: &str, value: &[u8]) -> Result<(), i32> {
        self.meta
            .set_xattr(ino, name, value)
            .map_err(|e| meta_error_to_errno(&e))
    }

    /// Remove an extended attribute.
    pub fn test_removexattr(&self, ino: u64, name: &str) -> Result<(), i32> {
        self.meta
            .remove_xattr(ino, name)
            .map_err(|e| meta_error_to_errno(&e))
    }

    /// Combined setattr: apply mode, uid, gid, mtime, and size changes.
    /// Returns the updated `InodeMeta`.
    #[allow(clippy::too_many_arguments)] // mirrors the FUSE setattr signature
    pub fn test_setattr(
        &self,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        fh: Option<u64>,
        mtime: Option<(i64, u32)>,
    ) -> Result<InodeMeta, i32> {
        let mut inode = self
            .meta
            .get_inode(ino)
            .map_err(|e| meta_error_to_errno(&e))?;

        if let Some(m) = mode {
            inode.mode = (inode.mode & S_IFMT) | (m & 0o7777);
        }
        if let Some(u) = uid {
            inode.uid = u;
        }
        if let Some(g) = gid {
            inode.gid = g;
        }
        if let Some((sec, nsec)) = mtime {
            inode.mtime_sec = sec;
            inode.mtime_nsec = nsec;
        }
        if let Some(new_size) = size {
            self.test_setattr_size(ino, fh, new_size)?;
            // Re-load inode after size change
            inode = self
                .meta
                .get_inode(ino)
                .map_err(|e| meta_error_to_errno(&e))?;
        }

        // Always update ctime
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);
        inode.ctime_sec = now.as_secs() as i64;
        inode.ctime_nsec = now.subsec_nanos();

        self.meta
            .update_inode(&inode)
            .map_err(|e| meta_error_to_errno(&e))?;
        Ok(inode)
    }

    // ── Higher-level testable wrappers ───────────────────────────────────────
    //
    // These combine the core test_*/simulate_* method with the get_inode
    // call that the FUSE callback needs, moving logic out of untestable
    // Filesystem trait methods into testable pub methods.

    /// Create a file and return `(ino, fh, InodeMeta)`.
    /// Handles `O_TRUNC` flag atomically.
    #[allow(clippy::too_many_arguments)] // mirrors the FUSE create signature
    pub fn test_create_full(
        &self,
        parent: u64,
        name: &str,
        mode: u32,
        umask: u32,
        uid: u32,
        gid: u32,
        flags: i32,
    ) -> Result<(u64, u64, InodeMeta), i32> {
        let (ino, fh) = self.test_create(parent, name, mode, umask, uid, gid)?;
        if flags & libc::O_TRUNC != 0
            && let Err(e) = self.test_setattr_size(ino, Some(fh), 0)
        {
            self.open_files.lock().unwrap().remove(&fh);
            return Err(e);
        }
        let meta = self
            .meta
            .get_inode(ino)
            .map_err(|e| meta_error_to_errno(&e))?;
        Ok((ino, fh, meta))
    }

    /// Create a directory and return `(ino, InodeMeta)`.
    pub fn test_mkdir_full(
        &self,
        parent: u64,
        name: &str,
        mode: u32,
        umask: u32,
        uid: u32,
        gid: u32,
    ) -> Result<(u64, InodeMeta), i32> {
        let ino = self.simulate_mkdir(parent, name, mode, umask, uid, gid)?;
        let meta = self
            .meta
            .get_inode(ino)
            .map_err(|e| meta_error_to_errno(&e))?;
        Ok((ino, meta))
    }

    /// Create a node (mknod) and return `(ino, InodeMeta)`.
    pub fn test_mknod_full(
        &self,
        parent: u64,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
        umask: u32,
    ) -> Result<(u64, InodeMeta), i32> {
        let ino = self.test_mknod(parent, name, mode, uid, gid, umask)?;
        let meta = self
            .meta
            .get_inode(ino)
            .map_err(|e| meta_error_to_errno(&e))?;
        Ok((ino, meta))
    }

    /// Create a symlink and return `(ino, InodeMeta)`.
    pub fn test_symlink_full(
        &self,
        parent: u64,
        link_name: &str,
        target: &str,
        uid: u32,
        gid: u32,
    ) -> Result<(u64, InodeMeta), i32> {
        let ino = self.simulate_symlink(parent, link_name, target, uid, gid)?;
        let meta = self
            .meta
            .get_inode(ino)
            .map_err(|e| meta_error_to_errno(&e))?;
        Ok((ino, meta))
    }

    /// Create a hard link and return `(ino, InodeMeta)`.
    pub fn test_link_full(
        &self,
        ino: u64,
        newparent: u64,
        newname: &str,
    ) -> Result<(u64, InodeMeta), i32> {
        let new_ino = self.simulate_link(ino, newparent, newname)?;
        let meta = self
            .meta
            .get_inode(new_ino)
            .map_err(|e| meta_error_to_errno(&e))?;
        Ok((new_ino, meta))
    }

    /// Flush write buffer without closing the handle.
    /// Wrapper around `flush_buffer_for_fsync` exposed for testing.
    pub fn test_flush(&self, ino: u64, fh: u64) -> Result<(), i32> {
        self.flush_buffer_for_fsync(ino, fh)
    }

    /// Release with read-only handle check (matches FUSE release behavior).
    pub fn test_release_full(&self, ino: u64, fh: u64) -> Result<(), i32> {
        if !self.open_files.lock().unwrap().contains_key(&fh) {
            return Ok(()); // read-only handle, no write state
        }
        self.test_release(ino, fh)
    }

    /// Destroy: commit, optionally snapshot, and shut down WAL.
    /// Returns true if commit succeeded.
    pub fn test_destroy(&self) -> bool {
        if let Ok(_root) = self.meta.commit() {
            if self.auto_snapshot {
                let _ = self.meta.create_snapshot(Some("auto-unmount".to_string()));
            }
            let _ = self.meta.shutdown_wal();
            true
        } else {
            false
        }
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
        assert_eq!(
            meta_error_to_errno(&MetaError::AlreadyExists(0)),
            libc::EEXIST
        );
        assert_eq!(
            meta_error_to_errno(&MetaError::NotADirectory(0)),
            libc::ENOTDIR
        );
        assert_eq!(
            meta_error_to_errno(&MetaError::IsADirectory(0)),
            libc::EISDIR
        );
        assert_eq!(
            meta_error_to_errno(&MetaError::Corrupted("x".into())),
            libc::EIO
        );
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
        let io = Arc::new(std::sync::Mutex::new(metadata::store_io::StoreIo::new(
            dir.path(),
        )));
        let meta = metadata::store::DictMetadataStore::new(io.clone());
        let fs = super::SliceFsFilesystem::new(meta, io, Some(dir.path().to_path_buf()));
        let (_blocks, _bfree, _bavail, files, _ffree, _bsize) = fs.test_statfs_values();
        assert_ne!(files, 1_000_000, "files must not be hardcoded 1_000_000");
        assert_eq!(files, 1, "fresh store files must equal inode_count (1)");
    }

    // ── dir_size internal unit tests ──────────────────────────────────────────

    #[test]
    fn test_dir_size_empty_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let size = dir_size(dir.path());
        assert_eq!(size, 0, "empty directory should have size 0");
    }

    #[test]
    fn test_dir_size_with_files() {
        let dir = tempfile::TempDir::new().unwrap();
        // Create two files with known sizes
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        std::fs::write(dir.path().join("b.txt"), b"world!!!").unwrap();
        let size = dir_size(dir.path());
        assert!(size >= 5 + 8, "dir_size must sum file sizes (>=13)");
    }

    #[test]
    fn test_dir_size_recursive() {
        let dir = tempfile::TempDir::new().unwrap();
        let subdir = dir.path().join("subdir");
        std::fs::create_dir(&subdir).unwrap();
        std::fs::write(subdir.join("nested.txt"), b"nested content").unwrap();
        std::fs::write(dir.path().join("top.txt"), b"top").unwrap();

        let size = dir_size(dir.path());
        assert!(size >= 14 + 3, "dir_size must recurse into subdirectories");
    }

    #[test]
    fn test_dir_size_nonexistent_dir() {
        let path = std::path::Path::new("/nonexistent/path/that/does/not/exist");
        let size = dir_size(path);
        assert_eq!(size, 0, "nonexistent dir should return 0");
    }

    // ── fresh_fs_with_store: helper that sets store_path ─────────────────────

    fn fresh_fs_with_store() -> (SliceFsFilesystem, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let io = Arc::new(Mutex::new(StoreIo::new(dir.path())));
        let meta = DictMetadataStore::new(io.clone());
        let fs = SliceFsFilesystem::new(meta, io, Some(dir.path().to_path_buf()));
        (fs, dir)
    }

    // ── test_getattr ─────────────────────────────────────────────────────────

    #[test]
    fn test_getattr_root_returns_directory() {
        let (fs, _dir) = fresh_fs();
        let meta = fs.test_getattr(1).unwrap();
        assert_eq!(meta.ino, 1);
        assert_ne!(meta.mode & S_IFDIR, 0);
    }

    #[test]
    fn test_getattr_nonexistent_returns_not_found() {
        let (fs, _dir) = fresh_fs();
        let result = fs.test_getattr(999);
        assert!(result.is_err());
    }

    #[test]
    fn test_getattr_created_file() {
        let (fs, _dir) = fresh_fs();
        let (ino, _fh) = fs
            .test_create(1, "hello.txt", 0o644, 0, 1000, 1000)
            .unwrap();
        let meta = fs.test_getattr(ino).unwrap();
        assert_eq!(meta.uid, 1000);
        assert_eq!(meta.gid, 1000);
        assert_ne!(meta.mode & S_IFREG, 0);
    }

    // ── test_lookup ──────────────────────────────────────────────────────────

    #[test]
    fn test_lookup_dot_returns_self() {
        let (fs, _dir) = fresh_fs();
        let (ino, meta) = fs.test_lookup(1, ".").unwrap();
        assert_eq!(ino, 1);
        assert_ne!(meta.mode & S_IFDIR, 0);
    }

    #[test]
    fn test_lookup_created_file() {
        let (fs, _dir) = fresh_fs();
        let (created_ino, _fh) = fs.test_create(1, "myfile", 0o644, 0, 0, 0).unwrap();
        let (found_ino, meta) = fs.test_lookup(1, "myfile").unwrap();
        assert_eq!(found_ino, created_ino);
        assert_ne!(meta.mode & S_IFREG, 0);
    }

    #[test]
    fn test_lookup_nonexistent_returns_enoent() {
        let (fs, _dir) = fresh_fs();
        let err = fs.test_lookup(1, "no_such_file").unwrap_err();
        assert_eq!(err, libc::ENOENT);
    }

    // ── test_readdir ─────────────────────────────────────────────────────────

    #[test]
    fn test_readdir_root_has_dot_dotdot() {
        let (fs, _dir) = fresh_fs();
        let entries = fs.test_readdir(1, 0).unwrap();
        let names: Vec<&str> = entries.iter().map(|(_, _, n)| n.as_str()).collect();
        assert!(names.contains(&"."));
        assert!(names.contains(&".."));
    }

    #[test]
    fn test_readdir_after_create() {
        let (fs, _dir) = fresh_fs();
        fs.test_create(1, "alpha", 0o644, 0, 0, 0).unwrap();
        fs.test_create(1, "beta", 0o644, 0, 0, 0).unwrap();
        let entries = fs.test_readdir(1, 0).unwrap();
        let names: Vec<&str> = entries.iter().map(|(_, _, n)| n.as_str()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));
    }

    #[test]
    fn test_readdir_with_offset_skips_entries() {
        let (fs, _dir) = fresh_fs();
        fs.test_create(1, "a", 0o644, 0, 0, 0).unwrap();
        let all = fs.test_readdir(1, 0).unwrap();
        let skipped = fs.test_readdir(1, 1).unwrap();
        assert_eq!(skipped.len(), all.len() - 1);
    }

    #[test]
    fn test_readdir_nonexistent_dir() {
        let (fs, _dir) = fresh_fs();
        let result = fs.test_readdir(999, 0);
        assert!(result.is_err(), "readdir on non-existent ino should fail");
    }

    #[test]
    fn test_readdir_shows_correct_file_types() {
        let (fs, _dir) = fresh_fs();
        fs.test_create(1, "file.txt", 0o644, 0, 0, 0).unwrap();
        fs.simulate_mkdir(1, "subdir", 0o755, 0, 0, 0).unwrap();
        fs.simulate_symlink(1, "link", "/tmp", 0, 0).unwrap();
        let entries = fs.test_readdir(1, 0).unwrap();
        let file_entry = entries.iter().find(|(_, _, n)| n == "file.txt").unwrap();
        assert_eq!(file_entry.1, FileType::RegularFile);
        let dir_entry = entries.iter().find(|(_, _, n)| n == "subdir").unwrap();
        assert_eq!(dir_entry.1, FileType::Directory);
        let link_entry = entries.iter().find(|(_, _, n)| n == "link").unwrap();
        assert_eq!(link_entry.1, FileType::Symlink);
    }

    // ── test_open ────────────────────────────────────────────────────────────

    #[test]
    fn test_open_rdonly() {
        let (fs, _dir) = fresh_fs();
        let (ino, _fh) = fs.test_create(1, "f.txt", 0o644, 0, 0, 0).unwrap();
        let _ = fs.test_release(ino, _fh);
        let (fh, is_write) = fs.test_open(ino, libc::O_RDONLY).unwrap();
        assert!(!is_write);
        assert!(fh > 0);
    }

    #[test]
    fn test_open_wronly() {
        let (fs, _dir) = fresh_fs();
        let (ino, _fh) = fs.test_create(1, "f.txt", 0o644, 0, 0, 0).unwrap();
        let _ = fs.test_release(ino, _fh);
        let (fh, is_write) = fs.test_open(ino, libc::O_WRONLY).unwrap();
        assert!(is_write);
        assert!(fh > 0);
    }

    #[test]
    fn test_open_rdwr() {
        let (fs, _dir) = fresh_fs();
        let (ino, _fh) = fs.test_create(1, "f.txt", 0o644, 0, 0, 0).unwrap();
        let _ = fs.test_release(ino, _fh);
        let (_fh2, is_write) = fs.test_open(ino, libc::O_RDWR).unwrap();
        assert!(is_write);
    }

    #[test]
    fn test_open_with_trunc() {
        let (fs, _dir) = fresh_fs();
        // Create file and write content
        let (ino, fh) = fs.test_create(1, "f.txt", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"hello world").unwrap();
        fs.test_release(ino, fh).unwrap();
        // Open with O_TRUNC
        let (fh2, _) = fs.test_open(ino, libc::O_WRONLY | libc::O_TRUNC).unwrap();
        // Size should be 0 after truncation
        let meta = fs.test_getattr(ino).unwrap();
        assert_eq!(meta.size, 0);
        fs.test_release(ino, fh2).unwrap();
    }

    // ── test_getxattr / test_setxattr / test_listxattr / test_removexattr ────

    #[test]
    fn test_xattr_set_get_list_remove() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        let _ = fs.test_release(ino, fh);

        // Set
        fs.test_setxattr(ino, "user.key", b"value").unwrap();

        // Get
        let val = fs.test_getxattr(ino, "user.key").unwrap();
        assert_eq!(val, b"value");

        // List
        let list = fs.test_listxattr(ino).unwrap();
        assert!(!list.is_empty());
        // Should contain "user.key\0"
        let expected = b"user.key\0";
        assert!(list.windows(expected.len()).any(|w| w == expected));

        // Remove
        fs.test_removexattr(ino, "user.key").unwrap();

        // Get after remove should fail
        let err = fs.test_getxattr(ino, "user.key").unwrap_err();
        assert_eq!(err, libc::ENODATA);
    }

    #[test]
    fn test_getxattr_nonexistent_attr() {
        let (fs, _dir) = fresh_fs();
        let err = fs.test_getxattr(1, "user.nonexistent").unwrap_err();
        assert_eq!(err, libc::ENODATA);
    }

    #[test]
    fn test_listxattr_empty() {
        let (fs, _dir) = fresh_fs();
        let buf = fs.test_listxattr(1).unwrap();
        assert_eq!(buf.len(), 0);
    }

    // ── test_setattr ─────────────────────────────────────────────────────────

    #[test]
    fn test_setattr_mode() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        let _ = fs.test_release(ino, fh);
        let meta = fs
            .test_setattr(ino, Some(0o755), None, None, None, None, None)
            .unwrap();
        assert_eq!(meta.mode & 0o7777, 0o755);
    }

    #[test]
    fn test_setattr_uid_gid() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        let _ = fs.test_release(ino, fh);
        let meta = fs
            .test_setattr(ino, None, Some(500), Some(600), None, None, None)
            .unwrap();
        assert_eq!(meta.uid, 500);
        assert_eq!(meta.gid, 600);
    }

    #[test]
    fn test_setattr_mtime() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        let _ = fs.test_release(ino, fh);
        let meta = fs
            .test_setattr(ino, None, None, None, None, None, Some((12345, 678)))
            .unwrap();
        assert_eq!(meta.mtime_sec, 12345);
        assert_eq!(meta.mtime_nsec, 678);
    }

    #[test]
    fn test_setattr_size_truncate_closed_file() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"hello world").unwrap();
        fs.test_release(ino, fh).unwrap();
        // Truncate to 5 bytes
        let meta = fs
            .test_setattr(ino, None, None, None, Some(5), None, None)
            .unwrap();
        assert_eq!(meta.size, 5);
        // Read back
        let data = fs.test_read(ino, 0, 100).unwrap();
        assert_eq!(&data, b"hello");
    }

    #[test]
    fn test_setattr_size_extend_closed_file() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"hi").unwrap();
        fs.test_release(ino, fh).unwrap();
        // Extend to 10 bytes
        let meta = fs
            .test_setattr(ino, None, None, None, Some(10), None, None)
            .unwrap();
        assert_eq!(meta.size, 10);
        let data = fs.test_read(ino, 0, 100).unwrap();
        assert_eq!(data.len(), 10);
        assert_eq!(&data[0..2], b"hi");
        // Extended region should be zero-filled
        assert!(data[2..].iter().all(|&b| b == 0));
    }

    #[test]
    fn test_setattr_size_truncate_to_zero_closed_file() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"data").unwrap();
        fs.test_release(ino, fh).unwrap();
        let meta = fs
            .test_setattr(ino, None, None, None, Some(0), None, None)
            .unwrap();
        assert_eq!(meta.size, 0);
    }

    #[test]
    fn test_setattr_size_with_open_fh_streaming() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"hello world").unwrap();
        // Truncate while handle is open (streaming mode)
        fs.test_setattr_size(ino, Some(fh), 5).unwrap();
        let meta = fs.test_getattr(ino).unwrap();
        assert_eq!(meta.size, 5);
        fs.test_release(ino, fh).unwrap();
        let data = fs.test_read(ino, 0, 100).unwrap();
        assert_eq!(&data, b"hello");
    }

    #[test]
    fn test_setattr_size_zero_with_open_fh_streaming() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"data").unwrap();
        fs.test_setattr_size(ino, Some(fh), 0).unwrap();
        let meta = fs.test_getattr(ino).unwrap();
        assert_eq!(meta.size, 0);
        fs.test_release(ino, fh).unwrap();
    }

    #[test]
    fn test_setattr_mode_preserves_type_bits() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        let _ = fs.test_release(ino, fh);
        fs.test_setattr_mode(ino, 0o777).unwrap();
        let meta = fs.test_getattr(ino).unwrap();
        // Type bits should still be S_IFREG
        assert_ne!(meta.mode & S_IFREG, 0);
        assert_eq!(meta.mode & 0o7777, 0o777);
    }

    #[test]
    fn test_setattr_uid_gid_separate() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        let _ = fs.test_release(ino, fh);
        // Set only uid
        fs.test_setattr_uid_gid(ino, Some(42), None).unwrap();
        let meta = fs.test_getattr(ino).unwrap();
        assert_eq!(meta.uid, 42);
        assert_eq!(meta.gid, 0);
        // Set only gid
        fs.test_setattr_uid_gid(ino, None, Some(99)).unwrap();
        let meta = fs.test_getattr(ino).unwrap();
        assert_eq!(meta.uid, 42);
        assert_eq!(meta.gid, 99);
    }

    #[test]
    fn test_setattr_mtime_specific() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        let _ = fs.test_release(ino, fh);
        fs.test_setattr_mtime(ino, 999, 123).unwrap();
        let meta = fs.test_getattr(ino).unwrap();
        assert_eq!(meta.mtime_sec, 999);
        assert_eq!(meta.mtime_nsec, 123);
    }

    // ── test_create / test_mknod ─────────────────────────────────────────────

    #[test]
    fn test_create_allocates_unique_inos() {
        let (fs, _dir) = fresh_fs();
        let (ino1, fh1) = fs.test_create(1, "a", 0o644, 0, 0, 0).unwrap();
        let (ino2, fh2) = fs.test_create(1, "b", 0o644, 0, 0, 0).unwrap();
        assert_ne!(ino1, ino2);
        assert_ne!(fh1, fh2);
    }

    #[test]
    fn test_create_duplicate_name_fails() {
        let (fs, _dir) = fresh_fs();
        fs.test_create(1, "dup", 0o644, 0, 0, 0).unwrap();
        let err = fs.test_create(1, "dup", 0o644, 0, 0, 0).unwrap_err();
        assert_eq!(err, libc::EEXIST);
    }

    #[test]
    fn test_create_applies_umask() {
        let (fs, _dir) = fresh_fs();
        let (ino, _fh) = fs.test_create(1, "f", 0o666, 0o022, 0, 0).unwrap();
        let meta = fs.test_getattr(ino).unwrap();
        assert_eq!(meta.mode & 0o7777, 0o644);
    }

    #[test]
    fn test_mknod_creates_file_without_fh() {
        let (fs, _dir) = fresh_fs();
        let ino = fs
            .test_mknod(1, "node", S_IFREG_TEST | 0o644, 0, 0, 0)
            .unwrap();
        let meta = fs.test_getattr(ino).unwrap();
        assert_ne!(meta.mode & S_IFREG, 0);
    }

    #[test]
    fn test_mknod_non_regular_file_returns_enosys() {
        let (fs, _dir) = fresh_fs();
        // S_IFCHR = 0o020_000
        let err = fs
            .test_mknod(1, "chardev", 0o020_000 | 0o644, 0, 0, 0)
            .unwrap_err();
        assert_eq!(err, libc::ENOSYS);
    }

    // ── test_write / test_read pipeline ──────────────────────────────────────

    #[test]
    fn test_write_read_roundtrip() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        let n = fs.test_write(fh, 0, b"hello").unwrap();
        assert_eq!(n, 5);
        fs.test_release(ino, fh).unwrap();
        let data = fs.test_read(ino, 0, 100).unwrap();
        assert_eq!(&data, b"hello");
    }

    #[test]
    fn test_write_sequential_multiple() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"aaa").unwrap();
        fs.test_write(fh, 3, b"bbb").unwrap();
        fs.test_write(fh, 6, b"ccc").unwrap();
        fs.test_release(ino, fh).unwrap();
        let data = fs.test_read(ino, 0, 100).unwrap();
        assert_eq!(&data, b"aaabbbccc");
    }

    #[test]
    fn test_write_nonsequential_triggers_buffered_fallback() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"hello").unwrap();
        // Non-sequential write at offset 10 (gap)
        fs.test_write(fh, 10, b"world").unwrap();
        fs.test_release(ino, fh).unwrap();
        let data = fs.test_read(ino, 0, 100).unwrap();
        assert_eq!(data.len(), 15);
        assert_eq!(&data[0..5], b"hello");
        assert_eq!(&data[10..15], b"world");
    }

    #[test]
    fn test_write_bad_fh_returns_ebadf() {
        let (fs, _dir) = fresh_fs();
        let err = fs.test_write(99999, 0, b"test").unwrap_err();
        assert_eq!(err, libc::EBADF);
    }

    #[test]
    fn test_read_empty_file() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "empty", 0o644, 0, 0, 0).unwrap();
        fs.test_release(ino, fh).unwrap();
        let data = fs.test_read(ino, 0, 100).unwrap();
        assert_eq!(data.len(), 0);
    }

    #[test]
    fn test_read_with_offset_and_size() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"abcdefghij").unwrap();
        fs.test_release(ino, fh).unwrap();
        let data = fs.test_read(ino, 3, 4).unwrap();
        assert_eq!(&data, b"defg");
    }

    #[test]
    fn test_read_beyond_eof_returns_empty() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"short").unwrap();
        fs.test_release(ino, fh).unwrap();
        let data = fs.test_read(ino, 100, 50).unwrap();
        assert_eq!(data.len(), 0);
    }

    // ── test_release ─────────────────────────────────────────────────────────

    #[test]
    fn test_release_nonexistent_fh_is_ok() {
        let (fs, _dir) = fresh_fs();
        // Releasing a non-existent fh should succeed (already closed)
        fs.test_release(1, 99999).unwrap();
    }

    #[test]
    fn test_release_empty_file_no_existing_content() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        // Release without writing anything
        fs.test_release(ino, fh).unwrap();
        // Should have empty manifest
        let data = fs.test_read(ino, 0, 100).unwrap();
        assert_eq!(data.len(), 0);
    }

    // ── test_fsync ───────────────────────────────────────────────────────────

    #[test]
    fn test_fsync_commits_data() {
        let (fs, _dir) = fresh_fs_with_store();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"synced data").unwrap();
        fs.test_fsync(ino, fh).unwrap();
        // Data should be readable even though handle is still open
        let data = fs.test_read(ino, 0, 100).unwrap();
        assert_eq!(&data, b"synced data");
        fs.test_release(ino, fh).unwrap();
    }

    #[test]
    fn test_fsync_noop_for_readonly_handle() {
        let (fs, _dir) = fresh_fs();
        // fsync on a non-existent fh should be a no-op
        fs.test_fsync(1, 99999).unwrap();
    }

    #[test]
    fn test_fsync_then_write_then_release() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"part1").unwrap();
        fs.test_fsync(ino, fh).unwrap();
        fs.test_write(fh, 5, b"part2").unwrap();
        fs.test_release(ino, fh).unwrap();
        let data = fs.test_read(ino, 0, 100).unwrap();
        assert_eq!(&data, b"part1part2");
    }

    // ── simulate_mkdir / simulate_rmdir ──────────────────────────────────────

    #[test]
    fn test_mkdir_creates_directory() {
        let (fs, _dir) = fresh_fs();
        let ino = fs.simulate_mkdir(1, "subdir", 0o755, 0, 0, 0).unwrap();
        let meta = fs.test_getattr(ino).unwrap();
        assert_ne!(meta.mode & S_IFDIR, 0);
    }

    #[test]
    fn test_mkdir_duplicate_fails() {
        let (fs, _dir) = fresh_fs();
        fs.simulate_mkdir(1, "subdir", 0o755, 0, 0, 0).unwrap();
        let err = fs.simulate_mkdir(1, "subdir", 0o755, 0, 0, 0).unwrap_err();
        assert_eq!(err, libc::EEXIST);
    }

    #[test]
    fn test_rmdir_empty_directory() {
        let (fs, _dir) = fresh_fs();
        fs.simulate_mkdir(1, "subdir", 0o755, 0, 0, 0).unwrap();
        fs.simulate_rmdir(1, "subdir").unwrap();
        let err = fs.test_lookup(1, "subdir").unwrap_err();
        assert_eq!(err, libc::ENOENT);
    }

    #[test]
    fn test_rmdir_nonempty_fails() {
        let (fs, _dir) = fresh_fs();
        let dir_ino = fs.simulate_mkdir(1, "subdir", 0o755, 0, 0, 0).unwrap();
        fs.test_create(dir_ino, "file", 0o644, 0, 0, 0).unwrap();
        let err = fs.simulate_rmdir(1, "subdir").unwrap_err();
        assert_eq!(err, libc::ENOTEMPTY);
    }

    #[test]
    fn test_rmdir_non_directory_fails() {
        let (fs, _dir) = fresh_fs();
        fs.test_create(1, "file", 0o644, 0, 0, 0).unwrap();
        let err = fs.simulate_rmdir(1, "file").unwrap_err();
        assert_eq!(err, libc::ENOTDIR);
    }

    // ── simulate_unlink ──────────────────────────────────────────────────────

    #[test]
    fn test_unlink_file() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"data").unwrap();
        fs.test_release(ino, fh).unwrap();
        fs.simulate_unlink(1, "f").unwrap();
        let err = fs.test_lookup(1, "f").unwrap_err();
        assert_eq!(err, libc::ENOENT);
    }

    #[test]
    fn test_unlink_directory_fails() {
        let (fs, _dir) = fresh_fs();
        fs.simulate_mkdir(1, "d", 0o755, 0, 0, 0).unwrap();
        let err = fs.simulate_unlink(1, "d").unwrap_err();
        assert_eq!(err, libc::EISDIR);
    }

    // ── simulate_link ────────────────────────────────────────────────────────

    #[test]
    fn test_link_creates_hard_link() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "original", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"content").unwrap();
        fs.test_release(ino, fh).unwrap();
        let linked_ino = fs.simulate_link(ino, 1, "hardlink").unwrap();
        assert_eq!(linked_ino, ino);
        // Both names should resolve to same inode
        let (found_ino, _) = fs.test_lookup(1, "hardlink").unwrap();
        assert_eq!(found_ino, ino);
        // nlinks should be 2
        let meta = fs.test_getattr(ino).unwrap();
        assert_eq!(meta.nlinks, 2);
    }

    #[test]
    fn test_link_directory_fails() {
        let (fs, _dir) = fresh_fs();
        let dir_ino = fs.simulate_mkdir(1, "d", 0o755, 0, 0, 0).unwrap();
        let err = fs.simulate_link(dir_ino, 1, "link_to_dir").unwrap_err();
        assert_eq!(err, libc::EPERM);
    }

    #[test]
    fn test_unlink_with_hard_links_decrements_nlinks() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "a", 0o644, 0, 0, 0).unwrap();
        fs.test_release(ino, fh).unwrap();
        fs.simulate_link(ino, 1, "b").unwrap();
        // Unlink one name
        fs.simulate_unlink(1, "a").unwrap();
        // File should still exist via "b"
        let (found_ino, meta) = fs.test_lookup(1, "b").unwrap();
        assert_eq!(found_ino, ino);
        assert_eq!(meta.nlinks, 1);
    }

    // ── simulate_rename ──────────────────────────────────────────────────────

    #[test]
    fn test_rename_basic() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "old", 0o644, 0, 0, 0).unwrap();
        fs.test_release(ino, fh).unwrap();
        fs.simulate_rename(1, "old", 1, "new", 0).unwrap();
        assert_eq!(fs.test_lookup(1, "old").unwrap_err(), libc::ENOENT);
        let (found_ino, _) = fs.test_lookup(1, "new").unwrap();
        assert_eq!(found_ino, ino);
    }

    #[test]
    fn test_rename_overwrite_target() {
        let (fs, _dir) = fresh_fs();
        let (ino1, fh1) = fs.test_create(1, "src", 0o644, 0, 0, 0).unwrap();
        fs.test_release(ino1, fh1).unwrap();
        let (_ino2, fh2) = fs.test_create(1, "dst", 0o644, 0, 0, 0).unwrap();
        fs.test_release(_ino2, fh2).unwrap();
        fs.simulate_rename(1, "src", 1, "dst", 0).unwrap();
        let (found_ino, _) = fs.test_lookup(1, "dst").unwrap();
        assert_eq!(found_ino, ino1);
    }

    #[test]
    fn test_rename_noreplace_fails_if_exists() {
        let (fs, _dir) = fresh_fs();
        fs.test_create(1, "a", 0o644, 0, 0, 0).unwrap();
        fs.test_create(1, "b", 0o644, 0, 0, 0).unwrap();
        let err = fs.simulate_rename(1, "a", 1, "b", 1).unwrap_err(); // RENAME_NOREPLACE=1
        assert_eq!(err, libc::EEXIST);
    }

    #[test]
    fn test_rename_exchange_returns_enosys() {
        let (fs, _dir) = fresh_fs();
        fs.test_create(1, "a", 0o644, 0, 0, 0).unwrap();
        fs.test_create(1, "b", 0o644, 0, 0, 0).unwrap();
        let err = fs.simulate_rename(1, "a", 1, "b", 2).unwrap_err(); // RENAME_EXCHANGE=2
        assert_eq!(err, libc::ENOSYS);
    }

    // ── simulate_symlink / simulate_readlink ─────────────────────────────────

    #[test]
    fn test_symlink_readlink_roundtrip() {
        let (fs, _dir) = fresh_fs();
        let ino = fs.simulate_symlink(1, "link", "/tmp/target", 0, 0).unwrap();
        let target = fs.simulate_readlink(ino).unwrap();
        assert_eq!(target, "/tmp/target");
    }

    #[test]
    fn test_symlink_is_link_type() {
        let (fs, _dir) = fresh_fs();
        let ino = fs.simulate_symlink(1, "link", "/foo", 0, 0).unwrap();
        let meta = fs.test_getattr(ino).unwrap();
        assert_eq!(meta.mode & S_IFMT, S_IFLNK);
    }

    // ── compute_statfs ───────────────────────────────────────────────────────

    #[test]
    fn test_statfs_no_store_path() {
        let (fs, _dir) = fresh_fs(); // store_path = None
        let (blocks, bfree, bavail, files, ffree, bsize) = fs.compute_statfs();
        assert_eq!(blocks, 0);
        assert_eq!(bfree, 0);
        assert_eq!(bavail, 0);
        assert_eq!(files, 1); // root inode
        assert!(ffree > 0);
        assert_eq!(bsize, 4096);
    }

    #[test]
    fn test_statfs_with_store_path() {
        let (fs, _dir) = fresh_fs_with_store();
        let (blocks, _bfree, _bavail, files, _ffree, bsize) = fs.compute_statfs();
        // With a real store path, statvfs should return non-zero values
        assert!(blocks > 0, "blocks should be > 0 on a real filesystem");
        assert!(bsize > 0, "bsize should be > 0");
        assert_eq!(files, 1);
    }

    // ── inode_to_file_attr edge cases ────────────────────────────────────────

    #[test]
    fn test_inode_to_file_attr_symlink() {
        let meta = InodeMeta {
            ino: 10,
            mode: S_IFLNK | 0o777,
            uid: 0,
            gid: 0,
            nlinks: 1,
            size: 10,
            mtime_sec: 0,
            mtime_nsec: 0,
            ctime_sec: 0,
            ctime_nsec: 0,
        };
        let attr = inode_to_file_attr(&meta);
        assert_eq!(attr.kind, FileType::Symlink);
        assert_eq!(attr.perm, 0o777);
    }

    #[test]
    fn test_inode_to_file_attr_negative_mtime() {
        let meta = InodeMeta {
            ino: 1,
            mode: S_IFREG | 0o644,
            uid: 0,
            gid: 0,
            nlinks: 1,
            size: 0,
            mtime_sec: -100,
            mtime_nsec: 0,
            ctime_sec: -50,
            ctime_nsec: 0,
        };
        let attr = inode_to_file_attr(&meta);
        // Should not panic for negative timestamps
        assert!(attr.mtime < UNIX_EPOCH);
        assert!(attr.ctime < UNIX_EPOCH);
    }

    #[test]
    fn test_inode_to_file_attr_blocks_calculation() {
        let meta = InodeMeta {
            ino: 1,
            mode: S_IFREG | 0o644,
            uid: 0,
            gid: 0,
            nlinks: 1,
            size: 1024,
            mtime_sec: 0,
            mtime_nsec: 0,
            ctime_sec: 0,
            ctime_nsec: 0,
        };
        let attr = inode_to_file_attr(&meta);
        // blocks = (1024 + 511) / 512 = 2
        assert_eq!(attr.blocks, 2);
    }

    // ── meta_error_to_fuse_errno ─────────────────────────────────────────────

    #[test]
    fn test_meta_error_to_fuse_errno_all_variants() {
        // Errno doesn't implement PartialEq, so we just verify the function
        // doesn't panic for all variants and returns a value.
        let _ = meta_error_to_fuse_errno(&MetaError::NotFound(0));
        let _ = meta_error_to_fuse_errno(&MetaError::AlreadyExists(0));
        let _ = meta_error_to_fuse_errno(&MetaError::NotADirectory(0));
        let _ = meta_error_to_fuse_errno(&MetaError::IsADirectory(0));
        let _ = meta_error_to_fuse_errno(&MetaError::NotEmpty(0));
        let _ = meta_error_to_fuse_errno(&MetaError::InvalidName("x".into()));
        let _ = meta_error_to_fuse_errno(&MetaError::Corrupted("x".into()));
        let _ = meta_error_to_fuse_errno(&MetaError::Io(std::io::Error::other("x")));
    }

    // ── meta_error_to_errno full coverage ────────────────────────────────────

    #[test]
    fn test_meta_error_to_errno_all_variants() {
        assert_eq!(
            meta_error_to_errno(&MetaError::NotEmpty(0)),
            libc::ENOTEMPTY
        );
        assert_eq!(
            meta_error_to_errno(&MetaError::InvalidName("x".into())),
            libc::EINVAL
        );
        assert_eq!(
            meta_error_to_errno(&MetaError::Io(std::io::Error::other("x"))),
            libc::EIO
        );
    }

    // ── Buffered mode write tests ────────────────────────────────────────────

    #[test]
    fn test_write_buffered_mode_pwrite() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        // Write at offset 0 first (streaming)
        fs.test_write(fh, 0, b"AAAA").unwrap();
        // Write at offset 0 again (non-sequential, triggers buffered)
        fs.test_write(fh, 0, b"BB").unwrap();
        fs.test_release(ino, fh).unwrap();
        let data = fs.test_read(ino, 0, 100).unwrap();
        assert_eq!(&data, b"BBAA");
    }

    #[test]
    fn test_write_buffered_mode_extend() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"abc").unwrap();
        // Non-sequential triggers buffered
        fs.test_write(fh, 0, b"X").unwrap();
        // Now in buffered mode, write beyond current size
        fs.test_write(fh, 10, b"YZ").unwrap();
        fs.test_release(ino, fh).unwrap();
        let data = fs.test_read(ino, 0, 100).unwrap();
        assert_eq!(data.len(), 12);
        assert_eq!(data[0], b'X');
        assert_eq!(&data[10..12], b"YZ");
    }

    // ── test_setattr_size with buffered mode ────────────────────────────────

    #[test]
    fn test_setattr_size_buffered_truncate() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"hello").unwrap();
        // Non-sequential write triggers buffered mode
        fs.test_write(fh, 0, b"H").unwrap();
        // Now truncate while in buffered mode
        fs.test_setattr_size(ino, Some(fh), 3).unwrap();
        fs.test_release(ino, fh).unwrap();
        let data = fs.test_read(ino, 0, 100).unwrap();
        assert_eq!(&data, b"Hel");
    }

    #[test]
    fn test_setattr_size_buffered_truncate_to_zero() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"hello").unwrap();
        fs.test_write(fh, 0, b"H").unwrap(); // trigger buffered
        fs.test_setattr_size(ino, Some(fh), 0).unwrap();
        let meta = fs.test_getattr(ino).unwrap();
        assert_eq!(meta.size, 0);
        fs.test_release(ino, fh).unwrap();
    }

    #[test]
    fn test_setattr_size_buffered_extend() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"ab").unwrap();
        fs.test_write(fh, 0, b"A").unwrap(); // trigger buffered
        fs.test_setattr_size(ino, Some(fh), 10).unwrap();
        fs.test_release(ino, fh).unwrap();
        let data = fs.test_read(ino, 0, 100).unwrap();
        assert_eq!(data.len(), 10);
        assert_eq!(data[0], b'A');
    }

    // ── cross-handle read tests ──────────────────────────────────────────────

    #[test]
    fn test_read_during_write_streaming() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"uncommitted data").unwrap();
        // Read from another "handle" (using test_read which checks open_files)
        let data = fs.test_read(ino, 0, 100).unwrap();
        assert_eq!(&data, b"uncommitted data");
        fs.test_release(ino, fh).unwrap();
    }

    #[test]
    fn test_read_during_write_buffered() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"hello").unwrap();
        fs.test_write(fh, 0, b"H").unwrap(); // trigger buffered
        let data = fs.test_read(ino, 0, 100).unwrap();
        assert_eq!(&data, b"Hello");
        fs.test_release(ino, fh).unwrap();
    }

    // ── fsync with buffered mode ─────────────────────────────────────────────

    #[test]
    fn test_fsync_buffered_mode() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"hello").unwrap();
        fs.test_write(fh, 0, b"H").unwrap(); // trigger buffered
        fs.test_fsync(ino, fh).unwrap();
        let data = fs.test_read(ino, 0, 100).unwrap();
        assert_eq!(&data, b"Hello");
        fs.test_release(ino, fh).unwrap();
    }

    #[test]
    fn test_fsync_empty_file_is_noop() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        // fsync on empty file should be fine
        fs.test_fsync(ino, fh).unwrap();
        fs.test_release(ino, fh).unwrap();
    }

    // ── release after fsync (skip redundant commit) ──────────────────────────

    #[test]
    fn test_release_after_fsync_skips_redundant_commit() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"data").unwrap();
        fs.test_fsync(ino, fh).unwrap();
        // Release should detect that fsync already committed and skip
        fs.test_release(ino, fh).unwrap();
        let data = fs.test_read(ino, 0, 100).unwrap();
        assert_eq!(&data, b"data");
    }

    // ── set_auto_snapshot / meta / io accessors ──────────────────────────────

    #[test]
    fn test_auto_snapshot_setter() {
        let (mut fs, _dir) = fresh_fs();
        fs.set_auto_snapshot(true);
        assert!(fs.auto_snapshot);
        fs.set_auto_snapshot(false);
        assert!(!fs.auto_snapshot);
    }

    #[test]
    fn test_meta_accessor() {
        let (fs, _dir) = fresh_fs();
        let meta = fs.meta();
        // Should be able to get root inode
        assert!(meta.get_inode(1).is_ok());
    }

    #[test]
    fn test_io_accessor() {
        let (fs, _dir) = fresh_fs();
        let io = fs.io();
        // Should be able to lock
        let _guard = io.lock().unwrap();
    }

    // ── rename across directories ────────────────────────────────────────────

    #[test]
    fn test_rename_across_directories() {
        let (fs, _dir) = fresh_fs();
        let dir_a = fs.simulate_mkdir(1, "a", 0o755, 0, 0, 0).unwrap();
        let dir_b = fs.simulate_mkdir(1, "b", 0o755, 0, 0, 0).unwrap();
        let (ino, fh) = fs.test_create(dir_a, "file", 0o644, 0, 0, 0).unwrap();
        fs.test_release(ino, fh).unwrap();
        fs.simulate_rename(dir_a, "file", dir_b, "moved", 0)
            .unwrap();
        assert_eq!(fs.test_lookup(dir_a, "file").unwrap_err(), libc::ENOENT);
        let (found, _) = fs.test_lookup(dir_b, "moved").unwrap();
        assert_eq!(found, ino);
    }

    // ── test_setattr combined ────────────────────────────────────────────────

    #[test]
    fn test_setattr_combined_mode_uid_gid_mtime() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        fs.test_release(ino, fh).unwrap();
        let meta = fs
            .test_setattr(
                ino,
                Some(0o755),
                Some(100),
                Some(200),
                None,
                None,
                Some((1234567890, 42)),
            )
            .unwrap();
        assert_eq!(meta.mode & 0o7777, 0o755);
        assert_eq!(meta.uid, 100);
        assert_eq!(meta.gid, 200);
        assert_eq!(meta.mtime_sec, 1234567890);
        assert_eq!(meta.mtime_nsec, 42);
    }

    #[test]
    fn test_setattr_with_size_change() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"hello world data").unwrap();
        fs.test_release(ino, fh).unwrap();
        // Note: when size is set, test_setattr re-loads the inode after the
        // size change, so mode/uid/gid changes applied before the reload are
        // overwritten. Test size separately.
        let meta = fs
            .test_setattr(ino, None, None, None, Some(5), None, None)
            .unwrap();
        assert_eq!(meta.size, 5);
        // Now apply mode/uid/gid separately
        let meta2 = fs
            .test_setattr(ino, Some(0o755), Some(100), Some(200), None, None, None)
            .unwrap();
        assert_eq!(meta2.mode & 0o7777, 0o755);
        assert_eq!(meta2.uid, 100);
        assert_eq!(meta2.gid, 200);
        assert_eq!(meta2.size, 5);
    }

    #[test]
    fn test_setattr_nonexistent_inode() {
        let (fs, _dir) = fresh_fs();
        let err = fs
            .test_setattr(999, Some(0o755), None, None, None, None, None)
            .unwrap_err();
        assert_ne!(err, 0);
    }

    // ── readlink edge cases ──────────────────────────────────────────────────

    #[test]
    fn test_readlink_empty_manifest() {
        let (fs, _dir) = fresh_fs();
        // Create a symlink-like inode manually with empty manifest
        let sym_meta = InodeMeta {
            ino: 0,
            mode: S_IFLNK | 0o777,
            uid: 0,
            gid: 0,
            nlinks: 1,
            size: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            ctime_sec: 0,
            ctime_nsec: 0,
        };
        let ino = fs.meta.create_inode(&sym_meta).unwrap();
        fs.meta.link(1, "emptylink", ino).unwrap();
        fs.meta.set_manifest(ino, &[]).unwrap();
        let target = fs.simulate_readlink(ino).unwrap();
        assert_eq!(target, "");
    }

    // ── setattr_size on file with no manifest yet ────────────────────────────

    #[test]
    fn test_setattr_size_no_manifest() {
        let (fs, _dir) = fresh_fs();
        // Create a file via mknod (no manifest, no open handle)
        let ino = fs
            .test_mknod(1, "newfile", S_IFREG | 0o644, 0, 0, 0)
            .unwrap();
        // Extend to 10 bytes (no manifest yet -- should handle NotFound gracefully)
        fs.test_setattr_size(ino, None, 10).unwrap();
        let meta = fs.test_getattr(ino).unwrap();
        assert_eq!(meta.size, 10);
    }

    // ── rename with target that has hardlinks ────────────────────────────────

    #[test]
    fn test_rename_overwrite_target_with_hardlinks() {
        let (fs, _dir) = fresh_fs();
        let (src_ino, fh1) = fs.test_create(1, "src", 0o644, 0, 0, 0).unwrap();
        fs.test_release(src_ino, fh1).unwrap();
        let (dst_ino, fh2) = fs.test_create(1, "dst", 0o644, 0, 0, 0).unwrap();
        fs.test_release(dst_ino, fh2).unwrap();
        fs.simulate_link(dst_ino, 1, "dst_link").unwrap();
        // Rename src -> dst (overwrites dst, but dst_link still exists)
        fs.simulate_rename(1, "src", 1, "dst", 0).unwrap();
        // dst_link should still exist and point to the original dst inode
        let (found, meta) = fs.test_lookup(1, "dst_link").unwrap();
        assert_eq!(found, dst_ino);
        assert_eq!(meta.nlinks, 1);
    }

    // ── Higher-level testable wrapper tests ──────────────────────────────────

    #[test]
    fn test_create_full_basic() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh, meta) = fs
            .test_create_full(1, "file.txt", 0o644, 0, 0, 0, 0)
            .unwrap();
        assert!(ino > 1);
        assert!(fh > 0);
        assert_ne!(meta.mode & S_IFREG, 0);
        fs.test_release(ino, fh).unwrap();
    }

    #[test]
    fn test_create_full_with_o_trunc() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh, meta) = fs
            .test_create_full(1, "f", 0o644, 0, 0, 0, libc::O_TRUNC)
            .unwrap();
        assert_eq!(meta.size, 0);
        fs.test_release(ino, fh).unwrap();
    }

    #[test]
    fn test_create_full_duplicate_fails() {
        let (fs, _dir) = fresh_fs();
        let (_ino, _fh, _) = fs.test_create_full(1, "f", 0o644, 0, 0, 0, 0).unwrap();
        let err = fs.test_create_full(1, "f", 0o644, 0, 0, 0, 0).unwrap_err();
        assert_eq!(err, libc::EEXIST);
    }

    #[test]
    fn test_mkdir_full_basic() {
        let (fs, _dir) = fresh_fs();
        let (ino, meta) = fs.test_mkdir_full(1, "dir", 0o755, 0, 0, 0).unwrap();
        assert!(ino > 1);
        assert_ne!(meta.mode & S_IFDIR, 0);
    }

    #[test]
    fn test_mknod_full_basic() {
        let (fs, _dir) = fresh_fs();
        let (ino, meta) = fs
            .test_mknod_full(1, "node", S_IFREG_TEST | 0o644, 0, 0, 0)
            .unwrap();
        assert!(ino > 1);
        assert_ne!(meta.mode & S_IFREG, 0);
    }

    #[test]
    fn test_symlink_full_basic() {
        let (fs, _dir) = fresh_fs();
        let (ino, meta) = fs.test_symlink_full(1, "link", "/target", 0, 0).unwrap();
        assert!(ino > 1);
        assert_eq!(meta.mode & S_IFMT, S_IFLNK);
        assert_eq!(meta.size, "/target".len() as u64);
    }

    #[test]
    fn test_link_full_basic() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "orig", 0o644, 0, 0, 0).unwrap();
        fs.test_release(ino, fh).unwrap();
        let (linked_ino, meta) = fs.test_link_full(ino, 1, "hardlink").unwrap();
        assert_eq!(linked_ino, ino);
        assert_eq!(meta.nlinks, 2);
    }

    #[test]
    fn test_flush_basic() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"flushed").unwrap();
        fs.test_flush(ino, fh).unwrap();
        let data = fs.test_read(ino, 0, 100).unwrap();
        assert_eq!(&data, b"flushed");
        fs.test_release(ino, fh).unwrap();
    }

    #[test]
    fn test_flush_noop_for_unknown_fh() {
        let (fs, _dir) = fresh_fs();
        fs.test_flush(1, 99999).unwrap();
    }

    #[test]
    fn test_release_full_readonly_handle() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        fs.test_release(ino, fh).unwrap();
        // Open read-only
        let (fh2, is_write) = fs.test_open(ino, libc::O_RDONLY).unwrap();
        assert!(!is_write);
        // test_release_full should detect no write state and return Ok
        fs.test_release_full(ino, fh2).unwrap();
    }

    #[test]
    fn test_release_full_write_handle() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"data").unwrap();
        fs.test_release_full(ino, fh).unwrap();
        let data = fs.test_read(ino, 0, 100).unwrap();
        assert_eq!(&data, b"data");
    }

    #[test]
    fn test_destroy_basic() {
        let (fs, _dir) = fresh_fs_with_store();
        let ok = fs.test_destroy();
        assert!(ok, "destroy should succeed on fresh fs");
    }

    #[test]
    fn test_destroy_with_auto_snapshot() {
        let (mut fs, _dir) = fresh_fs_with_store();
        fs.set_auto_snapshot(true);
        let ok = fs.test_destroy();
        assert!(ok, "destroy with auto-snapshot should succeed");
    }

    // ── statfs with store path that has NUL byte ─────────────────────────────

    #[test]
    fn test_statfs_invalid_path_cstring() {
        let dir = tempfile::tempdir().unwrap();
        let io = Arc::new(Mutex::new(StoreIo::new(dir.path())));
        let meta = DictMetadataStore::new(io.clone());
        // Create a path with a NUL byte which can't convert to CString
        let bad_path = PathBuf::from("/tmp/\0bad");
        let fs = SliceFsFilesystem::new(meta, io, Some(bad_path));
        let (blocks, bfree, bavail, _files, _ffree, bsize) = fs.compute_statfs();
        assert_eq!(blocks, 0);
        assert_eq!(bfree, 0);
        assert_eq!(bavail, 0);
        assert_eq!(bsize, 4096);
    }
}
