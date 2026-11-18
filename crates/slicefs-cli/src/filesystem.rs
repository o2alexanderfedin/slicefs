//! `SliceFsFilesystem` — FUSE adapter for SliceFS.
//!
//! Implements `fuser::Filesystem` using `DictMetadataStore` for inode/directory/manifest
//! lookups and the chunk store for file content reads.
//!
//! Write callbacks (create, write, release, setattr) are fully implemented in Phase 4.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::util::ts;

use slicefs_traits::key::ObjectKey;
use crate::content::{self, Content, Stream};
use crate::overlay::Overlay;
use fuser::{Errno, FileAttr, FileType, INodeNo};
use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use slicefs_traits::metadata::{InodeMeta, MetaError, MetadataStore};

// POSIX inode type bits
const S_IFMT: u32 = 0o170_000;
const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFLNK: u32 = 0o120_000;
const S_IFIFO: u32 = 0o010_000;
const S_IFSOCK: u32 = 0o140_000;
const S_IFCHR: u32 = 0o020_000;
const S_IFBLK: u32 = 0o060_000;

/// Largest file size SliceFS accepts (1 TiB). Larger truncate/write requests
/// fail with EFBIG. This is the size limit of a file; no write or truncate
/// allocates memory by file size or offset (an out-of-order write goes to an
/// overlay, see `crate::overlay`).
pub const MAX_FILE_SIZE: u64 = 1 << 40;

/// Most bytes of out-of-order writes a streaming handle holds while it waits
/// for a gap to close. Beyond it the handle switches to an overlay.
pub const REORDER_WINDOW: usize = 64 << 20;

/// The new content of a splice over an overlay: reads go through
/// `Overlay::view` (base bytes below `visible`, zeros, extents on top), so a
/// read holds only the requested range.
struct OverlaySource<'a> {
    cs: &'a chunk_store::ChunkStore,
    ov: &'a Overlay,
    reader: Option<content::Reader>,
}

impl<'a> OverlaySource<'a> {
    fn new(cs: &'a chunk_store::ChunkStore, ov: &'a Overlay) -> std::io::Result<Self> {
        let reader = match &ov.base {
            Some(k) => Some(content::Reader::open(cs, k)?),
            None => None,
        };
        Ok(OverlaySource { cs, ov, reader })
    }
}

impl chunk_store::file::SpliceSource for OverlaySource<'_> {
    fn read(&mut self, offset: u64, len: usize) -> chunk_store::Result<Vec<u8>> {
        let (cs, reader) = (self.cs, &mut self.reader);
        self.ov.view(offset, len as u64).read(|o, n| match reader.as_mut() {
            Some(r) => r.read(cs, o, n),
            None => Ok(Vec::new()),
        }).map_err(chunk_store::Error::Io)
    }
}

/// Largest data size of one `Write` record that a commit logs again (well
/// below the segment payload limit of 64 MiB). Each piece is copied, logged
/// and dropped before the next one is made.
const RELOG_PIECE: u64 = 1 << 20;

/// Dirty limit of one open file: when the RAM of its overlay (extent
/// capacity plus `EXTENT_OVERHEAD` per extent) passes it, the write that
/// passed it stores the overlay as a new root (a rebase) before it returns.
/// An extent grows by at most 1/8 of its length (`overlay::GROW_DIV`), not by
/// doubling, so the capacity stays near the dirty bytes. The real
/// peak of one handle is max(REORDER_WINDOW, DIRTY_LIMIT) + one write + the
/// rebase work space (one piece and the stream state).
/// Measured (#98, Docker, release build): VmHWM 114 MiB (1 GiB) and 161 MiB (4 GiB), cap 256 MiB;
/// slowest write 5870 ms (1 GiB) and 7253 ms (4 GiB). Both are above 2000 ms,
/// so 16 MiB was also run: 7778 ms and 5726 ms (VmHWM 97 and 146 MiB). The
/// slowest write of the run was 7778 ms against 7253 ms, so the value stays
/// 32 MiB.
pub const DIRTY_LIMIT: usize = 32 << 20;

/// A function the tests run inside a rebase (see `test_set_rebase_hook`).
pub type RebaseHook = Arc<dyn Fn() + Send + Sync>;

/// Write mode for an open file handle.
///
/// Streaming mode is the default for sequential writes (O(log N) memory).
/// Overlay mode is a one-way fallback for non-sequential writes: the stored
/// content (a root that the manifest names) plus the extents written since
/// then. Its memory follows the written bytes, not the file size. A handle in
/// Overlay mode goes back to Streaming only through a truncate to 0.
enum WriteMode {
    Streaming {
        state: Stream,
        next_expected_offset: u64,
        /// Writes that arrived ahead of `next_expected_offset` (NFS sends
        /// WRITEs in parallel). Non-overlapping; drained into `state` when
        /// the gap closes. Bounded by `REORDER_WINDOW` bytes.
        pending: BTreeMap<u64, Vec<u8>>,
    },
    Overlay(Overlay),
}

/// Per-handle state for a writable file descriptor.
///
/// Created on `open()` when `O_WRONLY` or `O_RDWR` flags are present.
/// Removed on `release()`. Writes are streamed incrementally into FastCDC
/// chunks and a manifest tree (`content::Stream`) — O(log N) memory regardless
/// of file size — unless a non-sequential write triggers fallback to
/// Overlay mode (memory follows the written bytes, not the file size).
///
/// Lifecycle:
/// - Created with `WriteMode::Streaming` and `byte_count = 0`.
/// - Sequential writes: `state.push_bytes()` called on each write, `byte_count` incremented.
/// - Non-sequential write: one-way transition to `WriteMode::Overlay`.
/// - `cas_committed` set to `true` after fsync/flush commits to CAS.
/// - `cas_committed` set to `false` when new writes arrive (dirty again).
/// - `last_committed_root` tracks the most recently committed ObjectKey
///   for refcount decrement-on-overwrite.
struct OpenFileState {
    ino: u64,
    /// Write mode — Streaming (sequential, O(log N)) or Overlay (non-sequential).
    write_mode: WriteMode,
    /// Total bytes written. Tracks inode size (for an overlay: its `size`).
    byte_count: u64,
    /// True if the manifest was committed to CAS by a prior flush/fsync and the
    /// state has not been dirtied by subsequent writes.
    cas_committed: bool,
    /// The most recently committed ObjectKey root. Used to decrement refcount
    /// when a new root is committed (fsync or release), preventing refcount leaks.
    last_committed_root: Option<ObjectKey>,
    /// Size set by setattr while streaming (macOS cp extends the file before
    /// writing it). Bytes between the stream end and this size are zeros;
    /// they are added at fsync/release instead of materializing the file.
    min_size: u64,
    /// Time of the last change to the content through this state. The flush
    /// stamps it as the file's mtime, so a flush at fsync, release or commit
    /// (or at the end of a replay) does not move mtime to its own time.
    modified: Option<(i64, u32)>,
}

/// FUSE filesystem adapter backed by a [`DictMetadataStore`].
///
/// `meta` provides all inode/directory/manifest metadata.
/// `io` is the shared `StoreIo` used for file content reads/writes via
/// `get_node` / `put_node`. It is the same `Arc<Mutex<StoreIo>>`
/// held by `meta`, accessed via `meta.io().clone()`.
///
/// `open_files` tracks per-handle write buffers; `next_fh` allocates unique handles.
/// `store_path` is the on-disk store root used by `destroy()` to persist state.
///
/// Raw bytes flow directly into the Merkle tree (v3 store format — no compression header).
/// ObjectKey is computed on raw bytes, enabling content-addressed deduplication.
pub struct SliceFsFilesystem {
    pub(crate) meta: Arc<DictMetadataStore>,
    pub(crate) io: Arc<Mutex<StoreIo>>,
    open_files: Mutex<HashMap<u64, OpenFileState>>,
    /// A second (third, ...) write handle of an inode shares the write state
    /// of the first: alias fh -> the fh that holds the state in `open_files`.
    /// POSIX: every handle writes the same file. With a state per handle,
    /// the last close won and the other handle's bytes were lost.
    write_alias: Mutex<HashMap<u64, u64>>,
    /// File content store (`<store>/packs/`).
    pub(crate) content: Content,
    /// A content reader per inode (keeps its node and chunk caches). A reader
    /// whose key no longer matches the file content is replaced on use.
    readers: Mutex<HashMap<u64, content::Reader>>,
    /// Every open handle (read-only too): fh -> ino. Used to keep an unlinked
    /// inode alive until its last handle closes.
    handles: Mutex<HashMap<u64, u64>>,
    /// Inodes whose last link was removed while handles were still open.
    orphans: Mutex<HashSet<u64>>,
    next_fh: AtomicU64,
    pub(crate) store_path: Option<PathBuf>,
    pub(crate) auto_snapshot: bool,
    /// Shared by FUSE callbacks, exclusive for a timed commit (see `commit_gate()`).
    commit_gate: Arc<RwLock<()>>,
    /// True while the write log is replayed: requests are not logged again.
    replaying: std::sync::atomic::AtomicBool,
    /// Dirty limit of one overlay (`DIRTY_LIMIT`; tests set it lower).
    dirty_limit: AtomicUsize,
    /// Rebases done so far (for tests).
    rebase_count: AtomicU64,
    /// Bytes the last rebase fed to the chunker (for tests).
    last_rebase_bytes: AtomicU64,
    /// `Write` payload bytes the last commit logged again (for tests).
    last_relog_bytes: AtomicU64,
    /// Run inside each rebase, after the new root is finished and before it
    /// is installed (for tests: online GC during a rebase).
    rebase_hook: Mutex<Option<RebaseHook>>,
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
        // Metadata nodes and file content share one chunk store.
        let content = Arc::clone(io.lock().unwrap().store());
        let cs = Arc::clone(&content);
        meta.set_pre_root_sync(Arc::new(move || cs.sync().map_err(std::io::Error::other)));
        Self {
            meta: Arc::new(meta),
            io,
            content,
            open_files: Mutex::new(HashMap::new()),
            write_alias: Mutex::new(HashMap::new()),
            readers: Mutex::new(HashMap::new()),
            handles: Mutex::new(HashMap::new()),
            orphans: Mutex::new(HashSet::new()),
            next_fh: AtomicU64::new(0),
            store_path,
            auto_snapshot: false,
            commit_gate: Arc::new(RwLock::new(())),
            replaying: std::sync::atomic::AtomicBool::new(false),
            dirty_limit: AtomicUsize::new(DIRTY_LIMIT),
            rebase_count: AtomicU64::new(0),
            last_rebase_bytes: AtomicU64::new(0),
            last_relog_bytes: AtomicU64::new(0),
            rebase_hook: Mutex::new(None),
        }
    }

    /// Test hook: set the dirty limit of each overlay to `bytes`.
    pub fn test_set_dirty_limit(&self, bytes: usize) {
        self.dirty_limit.store(bytes, Ordering::Relaxed);
    }

    /// Test hook: the number of rebases so far.
    pub fn test_rebase_count(&self) -> u64 {
        self.rebase_count.load(Ordering::Relaxed)
    }

    /// Test hook: the bytes the last splice cut again with FastCDC (a rebase,
    /// or a size change with no open handle): `SpliceStats::rechunked`.
    pub fn test_last_rebase_bytes(&self) -> u64 {
        self.last_rebase_bytes.load(Ordering::Relaxed)
    }

    /// Test hook: the `Write` payload bytes the last commit logged again.
    pub fn test_last_relog_bytes(&self) -> u64 {
        self.last_relog_bytes.load(Ordering::Relaxed)
    }

    /// Test hook: run `hook` inside each rebase, after the new root is
    /// finished and before it is installed (`None`: no hook).
    pub fn test_set_rebase_hook(&self, hook: Option<Arc<dyn Fn() + Send + Sync>>) {
        *self.rebase_hook.lock().unwrap() = hook;
    }

    /// Enable auto-snapshot on clean unmount.
    pub fn set_auto_snapshot(&mut self, enabled: bool) {
        self.auto_snapshot = enabled;
    }

    /// Every FUSE callback holds this gate shared; the commit timer holds it
    /// exclusively, so a timed commit never sees half of an operation.
    /// Append a request to the write log (#20), unless the log is being replayed.
    pub(crate) fn log_record(&self, rec: crate::oplog::Record) {
        if !self.replaying.load(Ordering::Relaxed) {
            self.meta.log_op(rec.encode());
        }
    }

    /// Turn off logging until the returned guard is dropped (replay).
    pub(crate) fn replay_guard(&self) -> ReplayGuard<'_> {
        self.replaying.store(true, Ordering::Relaxed);
        ReplayGuard(&self.replaying)
    }

    /// The inode of an open handle.
    pub(crate) fn ino_of_fh(&self, fh: u64) -> Option<u64> {
        let fh = self.owner_fh(fh);
        self.open_files.lock().unwrap().get(&fh).map(|s| s.ino)
    }

    /// The handle that holds the write state of `fh`: `fh` itself, or the
    /// first write handle of the same inode (see `write_alias`).
    fn owner_fh(&self, fh: u64) -> u64 {
        self.write_alias.lock().unwrap().get(&fh).copied().unwrap_or(fh)
    }

    pub fn commit_gate(&self) -> Arc<RwLock<()>> {
        Arc::clone(&self.commit_gate)
    }

    /// Access the metadata store (used by the mount command after unmount).
    pub fn meta(&self) -> &Arc<DictMetadataStore> {
        &self.meta
    }

    /// Access the shared Io backend (metadata objects).
    pub fn io(&self) -> &Arc<Mutex<StoreIo>> {
        &self.io
    }

    /// The file content store.
    pub fn content(&self) -> &Content {
        &self.content
    }

    /// All bytes of the content `key` (for tests and tools).
    pub fn read_content(&self, key: &ObjectKey) -> Option<Vec<u8>> {
        content::get_all(&self.content, key)
    }
}

/// Push zeros into the stream until it reaches offset `to`.
fn push_zeros(state: &mut Stream, cs: &chunk_store::ChunkStore, next: &mut u64, to: u64) -> std::io::Result<()> {
    if *next < to {
        state.push_zeros(cs, to - *next)?;
        *next = to;
    }
    Ok(())
}

/// Push the pending writes that start before `upto` into the stream in offset
/// order, with zeros in the gaps, then zeros up to `upto`. Memory stays O(1)
/// in the gap size. Only for a fresh stream, where unwritten bytes read as zero.
fn fill_stream(
    state: &mut Stream,
    cs: &chunk_store::ChunkStore,
    next: &mut u64,
    pending: &mut BTreeMap<u64, Vec<u8>>,
    upto: u64,
) -> std::io::Result<()> {
    while let Some((&o, _)) = pending.first_key_value() {
        if o >= upto {
            break;
        }
        push_zeros(state, cs, next, o)?;
        let v = pending.remove(&o).expect("first key exists");
        state.push(cs, &v)?;
        *next += v.len() as u64;
    }
    push_zeros(state, cs, next, upto)
}

/// Convert a `SystemTime` to `(seconds, nanoseconds)` since the epoch.
/// Times before 1970 give negative seconds with a non-negative nanosecond part.
pub fn system_time_to_pair(t: SystemTime) -> (i64, u32) {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
        Err(e) => {
            let d = e.duration();
            if d.subsec_nanos() == 0 {
                (-(d.as_secs() as i64), 0)
            } else {
                (-(d.as_secs() as i64) - 1, 1_000_000_000 - d.subsec_nanos())
            }
        }
    }
}

/// Inverse of [`system_time_to_pair`].
pub fn pair_to_system_time(sec: i64, nsec: u32) -> SystemTime {
    if sec >= 0 {
        UNIX_EPOCH + Duration::new(sec as u64, nsec)
    } else {
        UNIX_EPOCH - Duration::new(sec.unsigned_abs(), 0) + Duration::new(0, nsec)
    }
}

/// Current time as `(seconds, nanoseconds)`.
/// Now, or the frozen time of the current logged request (see `oplog::Clock`).
fn now_pair() -> (i64, u32) {
    crate::oplog::frozen_now().unwrap_or_else(|| system_time_to_pair(SystemTime::now()))
}

/// The real current time (not frozen).
pub(crate) fn system_now() -> (i64, u32) {
    system_time_to_pair(SystemTime::now())
}

/// Logging is back on when this is dropped (see `SliceFsFilesystem::replay_guard`).
pub(crate) struct ReplayGuard<'a>(&'a std::sync::atomic::AtomicBool);

impl Drop for ReplayGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

/// Convert an [`InodeMeta`] to a fuser [`FileAttr`].
///
/// Uses the `mode` field's type bits to determine `FileType`.
/// `atime` is the stored access time (set by utimensat; reads do not update it).
pub fn inode_to_file_attr(meta: &InodeMeta) -> FileAttr {
    let kind = match meta.mode & S_IFMT {
        S_IFDIR => FileType::Directory,
        S_IFLNK => FileType::Symlink,
        S_IFIFO => FileType::NamedPipe,
        S_IFSOCK => FileType::Socket,
        S_IFCHR => FileType::CharDevice,
        S_IFBLK => FileType::BlockDevice,
        _ => FileType::RegularFile,
    };

    let perm = (meta.mode & 0o7777) as u16;
    let blocks = (meta.size + 511) / 512;

    let mtime = pair_to_system_time(meta.mtime_sec, meta.mtime_nsec);
    let ctime = pair_to_system_time(meta.ctime_sec, meta.ctime_nsec);
    let atime = pair_to_system_time(meta.atime_sec, meta.atime_nsec);

    FileAttr {
        ino: INodeNo(meta.ino),
        size: meta.size,
        blocks,
        atime,
        mtime,
        ctime,
        crtime: UNIX_EPOCH,
        kind,
        perm,
        nlink: meta.nlinks,
        uid: meta.uid,
        gid: meta.gid,
        rdev: meta.rdev,
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
        MetaError::NameTooLong(_) => Errno::ENAMETOOLONG,
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
        MetaError::NameTooLong(_) => libc::ENAMETOOLONG,
        MetaError::Corrupted(_) => libc::EIO,
        MetaError::Io(_) => libc::EIO,
    }
}

/// TTL for all fuser replies (1 second is appropriate for a write-capable snapshot).
pub(crate) const TTL: Duration = Duration::from_secs(1);

// ── Test helpers ──────────────────────────────────────────────────────────────
//
// These methods expose the create/write/release/setattr pipeline without going
// through FUSE request/reply machinery, enabling unit and integration testing
// on macOS where a FUSE mount is not available.

impl SliceFsFilesystem {
    /// Allocate a unique, non-zero handle for an opened directory.
    ///
    /// Directory handles carry no state (readdir/releasedir ignore them), but
    /// FUSE-T's SMB backend treats handle 0 as invalid: with `fh=0` from
    /// opendir, `mkdir` on an SMB mount fails with EIO. Share the file-handle
    /// counter so every handle is unique.
    pub(crate) fn alloc_dir_handle(&self) -> u64 {
        self.next_fh.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Allocate a file handle and record it as open on `ino`.
    fn open_handle(&self, ino: u64) -> u64 {
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed) + 1;
        self.handles.lock().unwrap().insert(fh, ino);
        fh
    }

    /// Forget handle `fh`. If it was the last handle of an orphaned inode,
    /// reclaim the inode. A handle that is not open is ignored.
    fn close_handle(&self, fh: u64) {
        let ino = match self.handles.lock().unwrap().remove(&fh) {
            Some(ino) => ino,
            None => return,
        };
        let still_open = self.handles.lock().unwrap().values().any(|&i| i == ino);
        if !still_open && self.orphans.lock().unwrap().remove(&ino) {
            self.reclaim_inode(ino);
        }
    }

    /// Delete a non-directory inode that has no links left and release its content.
    fn reclaim_inode(&self, ino: u64) {
        if let Ok(manifest) = self.meta.get_manifest(ino) {
            for digest in &manifest {
                self.meta.decrement_refcount(digest);
            }
        }
        self.readers.lock().unwrap().remove(&ino);
        let _ = self.meta.delete_inode(ino);
    }

    /// The last link of a non-directory inode was removed: reclaim it now, or
    /// keep it as an orphan (nlink 0) while handles are still open.
    fn drop_last_link(&self, mut inode: InodeMeta) -> Result<(), i32> {
        let open = self.handles.lock().unwrap().values().any(|&i| i == inode.ino);
        if open {
            inode.nlinks = 0;
            self.meta.update_inode(&inode).map_err(|e| meta_error_to_errno(&e))?;
            self.orphans.lock().unwrap().insert(inode.ino);
        } else {
            self.reclaim_inode(inode.ino);
        }
        Ok(())
    }

    /// ftruncate(fd, 0) takes effect at once: empty the committed manifest and
    /// release its root, unless that root is `released` (already released by
    /// the handle). Later writes on the handle then start from an empty file.
    fn clear_manifest(&self, ino: u64, released: Option<ObjectKey>) -> Result<(), i32> {
        if let Ok(m) = self.meta.get_manifest(ino) {
            if let Some(root) = m.first() {
                if Some(*root) != released {
                    self.meta.decrement_refcount(root);
                }
            }
        }
        self.meta.set_manifest(ino, &[]).map_err(|_| libc::EIO)
    }

    /// Finish a clone of a streaming state as a stored root, first appending
    /// zeros from `streamed` up to `min_size` (an extension set by setattr).
    fn end_padded(&self, state: &Stream, streamed: u64, min_size: u64) -> Result<ObjectKey, i32> {
        let mut st = state.clone();
        let cs = &*self.content;
        st.push_zeros(cs, min_size.saturating_sub(streamed))
            .and_then(|_| st.finish(cs))
            .map_err(|e| {
                eprintln!("[{}][FUSE] content store error: {e}", ts());
                libc::EIO
            })
    }

    /// Start an online GC (#25). Turns on object tracking in the store, then
    /// takes the GC roots under the commit gate: a root for each open
    /// streaming handle, the base of each overlay, and a commit that is made
    /// durable before the sweep can delete anything. The gate is held only
    /// for that.
    ///
    /// A streaming handle holds subtrees that are on disk but that no commit
    /// refers to yet; a clone of its state, ended, gives a root that covers
    /// them. Pending (out-of-order) writes and overlay extents are only in
    /// RAM, so they need no root. An overlay base is in the manifest and is
    /// also a file root here. A rebase runs inside a callback that holds
    /// `commit_gate.read()`, so it never straddles `gc_begin`.
    pub fn gc_start(&self) -> Result<(metadata::gc::OnlineGc, u64), String> {
        if !Arc::ptr_eq(&self.io, self.meta.io()) {
            return Err("file system and metadata do not share one StoreIo".into());
        }
        if !Arc::ptr_eq(self.io.lock().unwrap().store(), &self.content) {
            return Err("metadata and file content do not share one chunk store".into());
        }
        let gate = self.commit_gate.write().unwrap_or_else(|e| e.into_inner());
        // From here the chunk store keeps each object put or read (nodes and
        // file content), and the GC changes only packs up to `boundary`.
        let boundary = self.content.gc_begin().map_err(|e| format!("GC start: chunk store error: {e}"))?;
        let mut file_roots = Vec::new();
        let mut states: Vec<Stream> = Vec::new();
        for s in self.open_files.lock().unwrap().values() {
            match &s.write_mode {
                WriteMode::Streaming { state, .. } => states.push(state.clone()),
                WriteMode::Overlay(ov) => file_roots.extend(ov.base),
            }
        }
        for state in states {
            match state.finish(&self.content) {
                Ok(k) => file_roots.push(k),
                Err(e) => {
                    self.content.gc_abort();
                    return Err(format!("GC start: chunk store error: {e}"));
                }
            }
        }
        let committed = self.commit_cut().map_err(|e| e.to_string()).and_then(|root| {
            self.meta.flush_wal().map_err(|e| e.to_string())?;
            Ok(root)
        });
        drop(gate);
        let root = match committed {
            Ok(root) => root,
            Err(e) => {
                self.content.gc_abort();
                return Err(format!("GC start: commit failed: {e}"));
            }
        };
        let mut meta_roots = vec![root];
        for r in self.meta.snapshot_roots() {
            if !meta_roots.contains(&r) {
                meta_roots.push(r);
            }
        }
        Ok((metadata::gc::OnlineGc::new(Arc::clone(&self.content), meta_roots, file_roots), boundary))
    }

    /// Run a full online GC: start, mark, sweep. `Ok(None)` if `stop` was set
    /// before the GC completed (then nothing is deleted).
    pub fn gc_online(
        &self,
        stop: &std::sync::atomic::AtomicBool,
    ) -> Result<Option<chunk_store::GcStats>, String> {
        let (gc, boundary) = self.gc_start()?;
        self.gc_content(&gc, boundary, stop)
    }

    /// The mark and sweep of an online GC: mark the objects of `gc` (metadata
    /// nodes and file content), then delete and compact the packs up to
    /// `boundary` (from `gc_start`). `Ok(None)` if `stop` was set first. On
    /// an error or a stop, nothing is deleted.
    pub fn gc_content(
        &self,
        gc: &metadata::gc::OnlineGc,
        boundary: u64,
        stop: &std::sync::atomic::AtomicBool,
    ) -> Result<Option<chunk_store::GcStats>, String> {
        let cs = &self.content;
        match gc.live(stop) {
            Ok(Some(ids)) => cs.gc_finish(&ids, boundary).map(Some).map_err(|e| format!("GC failed: {e}")),
            Ok(None) => {
                cs.gc_abort();
                Ok(None)
            }
            Err(e) => {
                cs.gc_abort();
                Err(format!("GC mark failed, nothing deleted: {e}"))
            }
        }
    }

    /// The open write handle of `ino` that holds the most data, if any.
    /// (FUSE-T may open a file O_RDWR only to read it; such an idle handle
    /// must not take a size change meant for the real writer.)
    fn write_handle_of(&self, ino: u64) -> Option<u64> {
        self.open_files.lock().unwrap().iter()
            .filter(|(_, s)| s.ino == ino)
            .max_by_key(|(fh, s)| (s.byte_count, **fh))
            .map(|(&fh, _)| fh)
    }

    /// True if the committed file has bytes (so a fresh stream would lose them).
    fn has_content(&self, ino: u64) -> bool {
        self.meta.get_inode(ino).map_or(false, |m| m.size > 0)
            && self.meta.get_manifest(ino).map_or(false, |m| !m.is_empty())
    }

    /// Make `new` the content root of `ino` (`None`: no content) and stamp
    /// the inode with `size` and `modified`. The refcount of the new root goes
    /// up and that of the root the manifest named before goes down. A root
    /// that the manifest still names is never released. The inode size is
    /// stamped too, so that a replayed write takes the overlay path on this
    /// root (not a fresh stream that drops it).
    fn install_root(&self, ino: u64, new: Option<ObjectKey>, size: u64, modified: Option<(i64, u32)>) -> Result<(), i32> {
        let old = self.meta.get_manifest(ino).ok().and_then(|m| m.first().copied());
        match new {
            None => self.clear_manifest(ino, None)?,
            Some(k) if Some(k) != old => {
                self.meta.set_manifest(ino, &[k]).map_err(|_| libc::EIO)?;
                self.meta.increment_refcount(&k);
                if let Some(o) = old {
                    self.meta.decrement_refcount(&o);
                }
            }
            Some(_) => {}
        }
        self.stamp_content(ino, size, modified)
    }

    /// Switch a streaming state to an overlay (the caller took it out of
    /// `open_files`). The base is the stream so far (ended, no zero padding),
    /// or the committed content if nothing was streamed. The base goes into
    /// the manifest at once (online GC keeps it, and replay starts from it).
    /// Pending writes move into the extents; the caller then applies the
    /// write that caused the switch. No file content is read.
    fn to_overlay(&self, s: &mut OpenFileState) -> Result<(), i32> {
        let ino = s.ino;
        let WriteMode::Streaming { state, next_expected_offset, pending } = &mut s.write_mode else {
            return Ok(());
        };
        let cs = &*self.content;
        let store_err = |e: std::io::Error| {
            eprintln!("[{}][FUSE] overlay: content store error: {e}", ts());
            libc::EIO
        };
        let (base, base_len) = if *next_expected_offset > 0 {
            (Some(state.clone().finish(cs).map_err(store_err)?), *next_expected_offset)
        } else if self.has_content(ino) {
            let k = self.meta.get_manifest(ino).map_err(|_| libc::EIO)?[0];
            (Some(k), content::Reader::open(cs, &k).map_err(store_err)?.len())
        } else {
            (None, 0)
        };
        let size = base_len.max(s.byte_count).max(s.min_size);
        self.install_root(ino, base, size, s.modified)?;
        let mut ov = Overlay::new(base, base_len, size);
        for (o, v) in std::mem::take(pending) {
            ov.put(o, v);
        }
        s.byte_count = ov.size;
        s.write_mode = WriteMode::Overlay(ov);
        s.last_committed_root = base;
        s.min_size = 0;
        s.cas_committed = false;
        Ok(())
    }

    /// Store the content of an overlay as a new root and start a clean
    /// overlay on it. The splice cuts again only the chunks near the extents
    /// (and near a cut or an extended tail); every other chunk and whole old
    /// node is reused by ID. Fixed memory, canonical key, CPU in the change
    /// (on long repeating non-zero data the re-chunk can run to the end of
    /// the file). A clean overlay is kept.
    fn rebase_overlay(&self, ino: u64, s: &mut OpenFileState) -> Result<(), i32> {
        let WriteMode::Overlay(ov) = &s.write_mode else {
            return Ok(());
        };
        if ov.is_clean() {
            return Ok(());
        }
        let cs = &*self.content;
        let store_err = |e: std::io::Error| {
            eprintln!("[{}][FUSE] rebase: content store error: {e}", ts());
            libc::EIO
        };
        let size = ov.size;
        let p0 = chunk_store::probe::snap();
        chunk_store::probe::reset_max();
        let t = std::time::Instant::now();
        let n_ext = ov.extent_ranges().len();
        let ext_bytes: u64 = ov.extent_ranges().iter().map(|r| r.end - r.start).sum();
        let cost = ov.cost();
        let (root, rechunked) = if size == 0 {
            (None, 0)
        } else {
            let mut src = OverlaySource::new(cs, ov).map_err(store_err)?;
            let (k, stats) = content::splice(cs, ov.base.as_ref(), ov.visible, &ov.extent_ranges(), size, &mut src)
                .map_err(store_err)?;
            eprintln!("[{}][PROBE] splice {:.1} ms: size={} extents={} ext_mib={:.1} cost_mib={:.1} rechunked_mib={:.1} reused_entries={} reused_nodes={} zero_runs={} |{}",
                ts(), t.elapsed().as_secs_f64() * 1e3, size, n_ext, ext_bytes as f64 / 1048576.0, cost as f64 / 1048576.0,
                stats.rechunked as f64 / 1048576.0, stats.reused_entries, stats.reused_nodes, stats.zero_runs, chunk_store::probe::diff(&p0));
            (Some(k), stats.rechunked)
        };
        let t_install = std::time::Instant::now();
        self.rebase_count.fetch_add(1, Ordering::Relaxed);
        self.last_rebase_bytes.store(rechunked, Ordering::Relaxed);
        let hook = self.rebase_hook.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook();
        }
        self.install_root(ino, root, size, s.modified)?;
        let t_drop = std::time::Instant::now();
        s.write_mode = WriteMode::Overlay(Overlay::new(root, size, size));
        eprintln!("[{}][PROBE] rebase tail: install_root={:.1} ms drop_old_overlay={:.1} ms", ts(),
            (t_drop - t_install).as_secs_f64() * 1e3, t_drop.elapsed().as_secs_f64() * 1e3);
        s.last_committed_root = root;
        s.byte_count = size;
        Ok(())
    }

    /// Read `size` bytes at `offset` of the content `root` of `ino` through
    /// the per-inode reader cache (it keeps its node and chunk caches).
    fn read_root(&self, ino: u64, root: &ObjectKey, offset: u64, size: usize) -> std::io::Result<Vec<u8>> {
        let mut readers = self.readers.lock().unwrap();
        if readers.len() >= 64 && !readers.contains_key(&ino) {
            readers.clear();
        }
        if readers.get(&ino).map_or(true, |r| r.key() != *root) {
            readers.insert(ino, content::Reader::open(&self.content, root)?);
        }
        readers.get_mut(&ino).expect("reader just inserted").read(&self.content, offset, size)
    }

    /// If handle `fh` streams with out-of-order writes still pending, stream
    /// them in order so its content is complete (read, fsync, truncate,
    /// release need the whole picture; gaps become zero bytes).
    fn settle_pending(&self, fh: u64) {
        let fh = self.owner_fh(fh);
        let mut file_state = match self.open_files.lock().unwrap().remove(&fh) {
            Some(s) => s,
            None => return,
        };
        if let WriteMode::Streaming { state, next_expected_offset, pending } = &mut file_state.write_mode {
            if !pending.is_empty() {
                // Pending writes exist only on a fresh stream: the gaps are zeros.
                // Stream them in order; no full-file buffer.
                let upto = pending.iter().next_back().map(|(&o, v)| o + v.len() as u64).unwrap_or(0);
                if let Err(e) = fill_stream(state, &self.content, next_expected_offset, pending, upto) {
                    eprintln!("[{}][FUSE] settle: content store error: {}", ts(), e);
                }
                file_state.byte_count = file_state.byte_count.max(*next_expected_offset);
                file_state.cas_committed = false;
            }
        }
        self.open_files.lock().unwrap().insert(fh, file_state);
    }

    /// Test hook: true if handle `fh` is still in Streaming mode.
    pub fn test_is_streaming(&self, fh: u64) -> bool {
        let fh = self.owner_fh(fh);
        matches!(self.open_files.lock().unwrap().get(&fh).map(|s| &s.write_mode), Some(WriteMode::Streaming { .. }))
    }

    /// Set access, modification and change times of a new inode to now.
    fn stamp_new(meta: &mut InodeMeta) {
        let (sec, nsec) = now_pair();
        meta.atime_sec = sec;
        meta.atime_nsec = nsec;
        meta.mtime_sec = sec;
        meta.mtime_nsec = nsec;
        meta.ctime_sec = sec;
        meta.ctime_nsec = nsec;
    }

    /// A directory's entries changed: set its mtime and ctime to now.
    fn touch_dir(&self, ino: u64) -> Result<(), i32> {
        let mut inode = self.meta.get_inode(ino).map_err(|e| meta_error_to_errno(&e))?;
        let (sec, nsec) = now_pair();
        inode.mtime_sec = sec;
        inode.mtime_nsec = nsec;
        inode.ctime_sec = sec;
        inode.ctime_nsec = nsec;
        self.meta.update_inode(&inode).map_err(|e| meta_error_to_errno(&e))
    }

    /// Add `delta` to the link count of `ino` (directory ".." bookkeeping).
    fn adjust_nlinks(&self, ino: u64, delta: i32) -> Result<(), i32> {
        let mut inode = self.meta.get_inode(ino).map_err(|e| meta_error_to_errno(&e))?;
        inode.nlinks = (inode.nlinks as i64 + delta as i64).max(0) as u32;
        self.meta.update_inode(&inode).map_err(|e| meta_error_to_errno(&e))
    }

    /// Reclaim all orphans (used at unmount: no handle survives it).
    fn reclaim_all_orphans(&self) {
        let orphans: Vec<u64> = self.orphans.lock().unwrap().drain().collect();
        for ino in orphans {
            self.reclaim_inode(ino);
        }
    }


    /// Used by integration tests to bypass the FUSE request/reply layer.
    /// Create a regular file inode in `parent_ino` directory, returning `(ino, fh)`.
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
        let mut file_meta = InodeMeta::new_file(0, uid, gid, file_mode);
        Self::stamp_new(&mut file_meta);
        let ino = self.meta.create_inode(&file_meta).map_err(|e| meta_error_to_errno(&e))?;
        if let Err(e) = self.meta.link(parent_ino, name, ino) {
            let _ = self.meta.delete_inode(ino);
            return Err(meta_error_to_errno(&e));
        }
        self.touch_dir(parent_ino)?;
        let fh = self.open_handle(ino);
        self.open_files.lock().unwrap().insert(fh, OpenFileState {
            ino,
            write_mode: WriteMode::Streaming { state: Stream::default(), next_expected_offset: 0, pending: BTreeMap::new() },
            byte_count: 0,
            cas_committed: false,            modified: None,
            last_committed_root: None,
            min_size: 0,
        });
        Ok((ino, fh))
    }

    /// Write `data` at `offset` into the file handle `fh`. Returns bytes written.
    ///
    /// Dual dispatch on WriteMode:
    /// - Streaming: sequential writes via push_bytes (O(log N) memory).
    ///   If offset != next_expected_offset (and not inside the reorder window),
    ///   triggers a one-way switch to an overlay (`to_overlay`).
    /// - Overlay: pwrite semantics into the extents (memory follows the
    ///   written bytes; nothing is allocated by offset or file size).
    ///
    /// Lock ordering: open_files is released before io to prevent contention.
    /// Every write takes OpenFileState out of the map, works without the lock
    /// (stream pushes, the switch), then re-inserts it. This prevents holding
    /// open_files during potentially slow CAS I/O (which would block concurrent
    /// reads on other fh's). FUSE serializes per-fh writes.
    pub fn test_write(&self, fh: u64, offset: u64, data: &[u8]) -> Result<u32, i32> {
        let fh = self.owner_fh(fh);
        if offset.checked_add(data.len() as u64).map_or(true, |end| end > MAX_FILE_SIZE) {
            return Err(libc::EFBIG);
        }
        // Take the file state out of the map so we can release open_files before io.
        let mut file_state = match self.open_files.lock().unwrap().remove(&fh) {
            Some(s) => s,
            None => return Err(libc::EBADF),
        };
        // open_files lock is released here.

        let result = match &mut file_state.write_mode {
            WriteMode::Streaming { state, next_expected_offset, pending } => {
                let end = offset + data.len() as u64;
                // Streaming from scratch is only right when the file starts
                // empty; otherwise unwritten parts must keep the old bytes.
                let fresh = *next_expected_offset > 0 || !pending.is_empty() || !self.has_content(file_state.ino);
                let overlaps_pending = pending.range(..end).next_back()
                    .map_or(false, |(&o, v)| o + v.len() as u64 > offset);
                let mut pending_bytes: usize = pending.values().map(Vec::len).sum();

                // A jump past the reorder window is a sparse write, not NFS
                // reordering: stream the gap as zeros (O(1) memory) and go on.
                if fresh && !overlaps_pending && offset > *next_expected_offset + REORDER_WINDOW as u64 {
                    let io_err = fill_stream(state, &self.content, next_expected_offset, pending, offset).err();
                    if let Some(e) = io_err {
                        eprintln!("[{}][FUSE] test_write: CAS I/O error while filling a gap: {}", ts(), e);
                        self.open_files.lock().unwrap().insert(fh, file_state);
                        return Err(libc::EIO);
                    }
                    pending_bytes = pending.values().map(Vec::len).sum();
                }

                if fresh && offset == *next_expected_offset && !overlaps_pending {
                    // In order: push, then drain pending writes that now line up.
                    let mut chunks = vec![data.to_vec()];
                    let mut next = end;
                    while let Some((&o, _)) = pending.first_key_value() {
                        if o != next { break; }
                        let v = pending.remove(&o).unwrap();
                        next += v.len() as u64;
                        chunks.push(v);
                    }
                    let io_err = chunks.iter().try_for_each(|c| state.push(&self.content, c)).err();
                    if let Some(e) = io_err {
                        eprintln!("[{}][FUSE] test_write: CAS I/O error during push_bytes: {}", ts(), e);
                        // Re-insert state before returning error
                        self.open_files.lock().unwrap().insert(fh, file_state);
                        return Err(libc::EIO);
                    }
                    *next_expected_offset = next;
                    file_state.byte_count = file_state.byte_count.max(next);
                    file_state.cas_committed = false;                    file_state.modified = Some(now_pair());
                    Ok(data.len() as u32)
                } else if fresh && offset > *next_expected_offset && !overlaps_pending
                    && pending_bytes + data.len() <= REORDER_WINDOW
                {
                    // Ahead of the stream: wait in the reorder window.
                    pending.insert(offset, data.to_vec());
                    file_state.byte_count = file_state.byte_count.max(end);
                    file_state.cas_committed = false;                    file_state.modified = Some(now_pair());
                    Ok(data.len() as u32)
                } else {
                    // Random write: switch to an overlay on the stored content.
                    let ino = file_state.ino;
                    eprintln!("[{}][FUSE] write: fh={} ino={} switches to overlay (offset={} len={} expected={} pending={} fresh={} overlap={})",
                        ts(), fh, ino, offset, data.len(), *next_expected_offset, pending_bytes, fresh, overlaps_pending);
                    // Older pending writes go in first, then this (newest) write.
                    match self.to_overlay(&mut file_state) {
                        Ok(()) => Self::overlay_write(&mut file_state, offset, data),
                        Err(e) => Err(e),
                    }
                }
            }
            WriteMode::Overlay(_) => Self::overlay_write(&mut file_state, offset, data),
        };

        // An overlay past the dirty limit (also right after a switch that
        // moved pending writes in) is stored as a new root now, in this
        // callback. Never at fsync or at a commit.
        let over = matches!(&file_state.write_mode,
            WriteMode::Overlay(ov) if ov.cost() > self.dirty_limit.load(Ordering::Relaxed));
        let result = match result {
            Ok(n) if over => {
                let ino = file_state.ino;
                let t = std::time::Instant::now();
                let r = self.rebase_overlay(ino, &mut file_state).map(|_| n).map_err(|_| libc::EIO);
                eprintln!("[{}][PROBE] rebase in write offset={} len={} total={:.1} ms", ts(), offset, data.len(), t.elapsed().as_secs_f64() * 1e3);
                r
            }
            r => r,
        };

        // Re-insert the file state back into the map.
        self.open_files.lock().unwrap().insert(fh, file_state);
        result
    }

    /// pwrite into the overlay of `s` (no I/O, no allocation by offset).
    fn overlay_write(s: &mut OpenFileState, offset: u64, data: &[u8]) -> Result<u32, i32> {
        let WriteMode::Overlay(ov) = &mut s.write_mode else {
            return Err(libc::EIO);
        };
        ov.write(offset, data);
        s.byte_count = ov.size;
        s.cas_committed = false;
        s.modified = Some(now_pair());
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
        // A handle that shares another's write state: only the handle ends.
        if self.write_alias.lock().unwrap().remove(&fh).is_some() {
            self.close_handle(fh);
            return Ok(());
        }
        // The holder of a shared state closes while other write handles are
        // open: one of them takes the state over (nothing is written yet).
        let heir = {
            let mut alias = self.write_alias.lock().unwrap();
            let heir = alias.iter().find(|(_, o)| **o == fh).map(|(a, _)| *a);
            if let Some(h) = heir {
                alias.remove(&h);
                for owner in alias.values_mut() {
                    if *owner == fh {
                        *owner = h;
                    }
                }
            }
            heir
        };
        if let Some(h) = heir {
            let mut open_files = self.open_files.lock().unwrap();
            if let Some(st) = open_files.remove(&fh) {
                open_files.insert(h, st);
            }
            drop(open_files);
            self.close_handle(fh);
            return Ok(());
        }
        let result = self.finalize_write(ino, fh);
        self.close_handle(fh);
        result
    }

    /// Commit the write state of `fh` (if any) to the inode's manifest.
    fn finalize_write(&self, ino: u64, fh: u64) -> Result<(), i32> {
        self.settle_pending(fh);
        let mut s = match self.open_files.lock().unwrap().remove(&fh) {
            Some(s) => s,
            None => return Ok(()), // Already closed
        };
        // An overlay: store its content as a new root (its base is already
        // in the manifest, so a clean overlay needs nothing).
        if matches!(s.write_mode, WriteMode::Overlay(_)) {
            return self.rebase_overlay(ino, &mut s);
        }
        let OpenFileState { write_mode, byte_count, cas_committed, last_committed_root, min_size, modified, .. } = s;

        // No bytes written through this handle.
        // Only set empty manifest if this is a genuinely new file (no existing manifest).
        // FUSE-T on macOS may open existing files with O_RDWR even for read-only access
        // (issue #15). Setting an empty manifest here would erase the file's content.
        if byte_count == 0 {
            if !cas_committed {
                // Check if the file already has content — if so, don't overwrite it.
                let has_existing = self.meta.get_manifest(ino)
                    .map(|m| !m.is_empty())
                    .unwrap_or(false);
                if !has_existing {
                    self.meta.set_manifest(ino, &[]).map_err(|_| libc::EIO)?;
                }
            }
            return Ok(());
        }

        // Finalize to CAS based on write mode
        let new_digest: ObjectKey = match write_mode {
            WriteMode::Streaming { state, next_expected_offset, .. } => {
                self.end_padded(&state, next_expected_offset, min_size)?
            }
            WriteMode::Overlay(_) => unreachable!("an overlay is rebased above"),
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
        self.stamp_content(ino, byte_count, modified)?;

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
    /// the inode and materializes uncommitted content via clone+end+get_node.
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
        // In Overlay mode, copy only the extent parts of the range under the lock.
        // In Streaming mode, clone State for materialization.
        enum WriterSnapshot {
            Streaming(Stream),
            Overlay(crate::overlay::RangeView),
        }

        let pending_fh = self.open_files.lock().unwrap().iter()
            .find(|(_, s)| s.ino == ino && matches!(&s.write_mode, WriteMode::Streaming { pending, .. } if !pending.is_empty()))
            .map(|(&fh, _)| fh);
        if let Some(fh) = pending_fh {
            self.settle_pending(fh);
        }
        let mut logical_size: Option<u64> = None;
        let writer_snapshot: Option<WriterSnapshot> = {
            let open_files = self.open_files.lock().unwrap();
            match open_files.values().find(|s| s.ino == ino && s.byte_count > 0) {
                // Overlay: copy only the extent parts inside the range.
                Some(OpenFileState { write_mode: WriteMode::Overlay(ov), .. }) => {
                    Some(WriterSnapshot::Overlay(ov.view(offset, size as u64)))
                }
                Some(OpenFileState { write_mode: WriteMode::Streaming { state, .. }, byte_count, .. }) => {
                    logical_size = Some(*byte_count);
                    Some(WriterSnapshot::Streaming(state.clone()))
                }
                None => None,
            }
        };
        // open_files lock released

        let root: ObjectKey = match writer_snapshot {
            Some(WriterSnapshot::Streaming(snapshot)) => {
                // Uncommitted content: finish a clone of the writer to get a root.
                snapshot.finish(&self.content).map_err(|_| libc::EIO)?
            }
            Some(WriterSnapshot::Overlay(view)) => {
                // Base bytes, zeros and the extent parts: memory O(request).
                let base = view.base;
                return view.read(|o, n| match &base {
                    Some(k) => self.read_root(ino, k, o, n),
                    None => Ok(Vec::new()),
                }).map_err(|_| libc::EIO);
            }
            None => {
                // Fall back to committed manifest
                let manifest = self.meta.get_manifest(ino).map_err(|_| libc::EIO)?;
                if manifest.is_empty() { return Ok(vec![]); }
                manifest[0]
            }
        };

        let mut out = self.read_root(ino, &root, offset, size as usize).map_err(|_| libc::EIO)?;
        // Past the streamed bytes but inside a setattr extension: zeros.
        if let Some(logical) = logical_size {
            let want = logical.saturating_sub(offset).min(size as u64) as usize;
            if out.len() < want {
                out.resize(want, 0);
            }
        }
        Ok(out)
    }

    /// Flush the streaming state for `fh` to CAS without closing the handle.
    ///
    /// Uses clone+end pattern: clones the in-progress State, calls end() on the
    /// clone to produce a ObjectKey snapshot, sets manifest, keeps original State
    /// alive for further writes. After fsync, subsequent writes continue appending
    /// to the same State accumulator.
    ///
    /// Refcount lifecycle: decrements old committed root (if any and different),
    /// increments new root. Tracks `last_committed_root` for next decrement.
    ///
    /// If `fh` is not in `open_files` (read-only handle or invalid), returns Ok
    /// without error — fsync is a no-op for read-only handles.
    /// The content of `ino` is now `size` bytes, last changed at `modified`
    /// (now if unknown): set size, mtime and ctime. ctime does not go back
    /// past a later change (a chmod after the last write).
    fn stamp_content(&self, ino: u64, size: u64, modified: Option<(i64, u32)>) -> Result<(), i32> {
        let mut inode = self.meta.get_inode(ino).map_err(|_| libc::EIO)?;
        inode.size = size;
        let (sec, nsec) = modified.unwrap_or_else(now_pair);
        inode.mtime_sec = sec;
        inode.mtime_nsec = nsec;
        if (sec, nsec) > (inode.ctime_sec, inode.ctime_nsec) {
            inode.ctime_sec = sec;
            inode.ctime_nsec = nsec;
        }
        self.meta.update_inode(&inode).map_err(|_| libc::EIO)
    }

    fn flush_buffer_for_fsync(&self, ino: u64, fh: u64) -> Result<(), i32> {
        self.flush_handle(ino, fh)
    }

    /// Commit with a clean cut of the write log (#20): first write the
    /// streamed prefix of each open write handle into its manifest (an
    /// overlay's base is there already), so that the root holds every request
    /// logged before the commit, apart from the bytes logged again after it.
    /// No content is hashed in full here. The caller holds the commit gate
    /// exclusively (no request is in flight).
    pub fn commit_cut(&self) -> Result<ObjectKey, MetaError> {
        use crate::oplog::{Op, Record};
        let t_cut = std::time::Instant::now();
        let p0 = chunk_store::probe::snap();
        let handles: Vec<(u64, u64)> = self.open_files.lock().unwrap()
            .iter().map(|(&fh, s)| (s.ino, fh)).collect();
        // Streamed prefixes go into the manifests (cheap: an ended clone).
        for &(ino, fh) in &handles {
            if let Err(e) = self.flush_handle(ino, fh) {
                return Err(MetaError::Corrupted(format!("cut: flush of open file {ino} failed: errno {e}")));
            }
        }
        // Bytes that are only in RAM (pending out-of-order writes, overlay
        // extents) are not hashed under the gate: they are logged again after
        // the cut, so replay puts them on top of the root.
        // The records carry the time of the last write, not the commit time:
        // replay stamps it as the mtime. A last record puts back a later
        // ctime (the truncate in replay sets ctime to the write time).
        // Before the commit only descriptors are taken (no overlay bytes are
        // copied); the exclusive gate keeps the states as they are until the
        // records are logged.
        enum Relog {
            Pending(Vec<Record>),
            Overlay { fh: u64, base_len: u64, visible: u64, size: u64, extents: Vec<std::ops::Range<u64>> },
        }
        struct Item { ino: u64, time: (i64, u32), ctime: Option<(i64, u32)>, what: Relog }
        let mut items: Vec<Item> = Vec::new();
        {
            let open_files = self.open_files.lock().unwrap();
            for &(_, fh) in &handles {
                let Some(s) = open_files.get(&fh) else { continue };
                let ino = s.ino;
                let time = s.modified.unwrap_or_else(now_pair);
                let what = match &s.write_mode {
                    WriteMode::Streaming { pending, .. } if !pending.is_empty() => Relog::Pending(
                        pending.iter().map(|(&off, data)| Record { time, op: Op::Write { ino, offset: off, data: data.clone() } }).collect(),
                    ),
                    WriteMode::Overlay(ov) if !ov.is_clean() => Relog::Overlay {
                        fh, base_len: ov.base_len, visible: ov.visible, size: ov.size, extents: ov.extent_ranges(),
                    },
                    _ => continue,
                };
                let ctime = self.meta.get_inode(ino).ok()
                    .map(|inode| (inode.ctime_sec, inode.ctime_nsec))
                    .filter(|&c| c > time);
                items.push(Item { ino, time, ctime, what });
            }
        }
        let t_commit = std::time::Instant::now();
        let flush_ms = (t_commit - t_cut).as_secs_f64() * 1e3;
        let root = self.meta.commit()?;
        let commit_ms = t_commit.elapsed().as_secs_f64() * 1e3;
        let t_relog = std::time::Instant::now();
        // The commit deleted the old log segments, so these records are now
        // the only copy of bytes an fsync may already have made durable:
        // sync them before the commit returns (a buffered log keeps them in RAM).
        let mut relog_bytes = 0u64;
        let relogged = !items.is_empty();
        for Item { ino, time, ctime, what } in items {
            match what {
                Relog::Pending(recs) => {
                    for rec in recs {
                        if let Op::Write { data, .. } = &rec.op {
                            relog_bytes += data.len() as u64;
                        }
                        self.log_record(rec);
                    }
                }
                Relog::Overlay { fh, base_len, visible, size, extents } => {
                    // The base is the manifest root. Shrink first (the zeros
                    // past `visible` come back), then the size, then the
                    // extents in pieces of at most RELOG_PIECE, one at a time.
                    let size_rec = |n: u64| Record { time, op: Op::Setattr {
                        ino, mode: None, uid: None, gid: None, size: Some(n), atime: None, mtime: None,
                    } };
                    if visible < base_len {
                        self.log_record(size_rec(visible));
                    }
                    if size != visible {
                        self.log_record(size_rec(size));
                    }
                    for r in extents {
                        let mut off = r.start;
                        while off < r.end {
                            let n = (r.end - off).min(RELOG_PIECE);
                            let data = match self.open_files.lock().unwrap().get(&fh).map(|s| &s.write_mode) {
                                Some(WriteMode::Overlay(ov)) => ov.copy_out(off, n as usize),
                                _ => Vec::new(),
                            };
                            if data.len() as u64 != n {
                                return Err(MetaError::Corrupted(format!("cut: extent of open file {ino} changed during the cut")));
                            }
                            relog_bytes += n;
                            self.log_record(Record { time, op: Op::Write { ino, offset: off, data } });
                            off += n;
                        }
                    }
                }
            }
            if let Some(ctime) = ctime {
                self.log_record(Record { time: ctime, op: Op::Setattr {
                    ino, mode: None, uid: None, gid: None, size: None, atime: None, mtime: Some(time),
                } });
            }
        }
        self.last_relog_bytes.store(relog_bytes, Ordering::Relaxed);
        let t_fw = std::time::Instant::now();
        let relog_ms = (t_fw - t_relog).as_secs_f64() * 1e3;
        if relogged {
            self.meta.flush_wal()
                .map_err(|e| MetaError::Corrupted(format!("cut: sync of the write log failed: {e}")))?;
        }
        eprintln!("[{}][PROBE] commit_cut total={:.1} ms: flush_handles={:.1} meta.commit={:.1} relog={:.1} ms ({:.1} MiB) flush_wal={:.1} ms |{}",
            ts(), t_cut.elapsed().as_secs_f64() * 1e3, flush_ms, commit_ms, relog_ms, relog_bytes as f64 / 1048576.0,
            t_fw.elapsed().as_secs_f64() * 1e3, chunk_store::probe::diff(&p0));
        Ok(root)
    }

    /// Write the streamed content of handle `fh` into the manifest of `ino`.
    /// An overlay needs nothing: its base is in the manifest, and its extents
    /// are durable through the write log (#20). A rebase at each fsync would
    /// cost a write of the changed chunks and nodes per fsync (#3).
    fn flush_handle(&self, ino: u64, fh: u64) -> Result<(), i32> {
        let fh = self.owner_fh(fh);
        // Pending (out-of-order) bytes stay pending: settling them here would
        // stream zeros for the gaps, and the late writes that fill the gaps
        // would then force the handle into an overlay. The write log keeps
        // pending bytes durable (#20), so the manifest takes the streamed prefix.
        // 1. Clone the stream under the open_files lock
        let (snapshot, byte_count, old_root, modified) = {
            let mut open_files = self.open_files.lock().unwrap();
            match open_files.get_mut(&fh) {
                Some(s) => {
                    if s.byte_count == 0 {
                        // Empty file -- nothing to flush
                        return Ok(());
                    }
                    if s.cas_committed && s.last_committed_root.is_some() {
                        // No write since the last fsync: the manifest is current.
                        // (NFS sends a COMMIT per group of writes; skip the re-hash.)
                        return Ok(());
                    }
                    let snap = match &s.write_mode {
                        WriteMode::Streaming { state, next_expected_offset, .. } =>
                            (state.clone(), *next_expected_offset, s.min_size),
                        WriteMode::Overlay(_) => return Ok(()),
                    };
                    s.cas_committed = true;
                    (snap, s.byte_count, s.last_committed_root, s.modified)
                }
                None => return Ok(()), // No write handle -- fsync is a no-op
            }
        };
        // open_files lock released here

        // 2. End the clone (an O(log N) tree finish)
        let (state_snapshot, streamed, min_size) = snapshot;
        let new_digest: ObjectKey = self.end_padded(&state_snapshot, streamed, min_size)?;

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
        self.stamp_content(ino, byte_count, modified)?;

        Ok(())
    }


    /// Truncate/extend a file to `new_size` bytes. If `fh` is Some and open,
    /// operates on the in-flight write state; otherwise streams the committed
    /// content into a new tree in 1 MiB pieces.
    ///
    /// Dual dispatch on WriteMode:
    /// - Streaming: an extension only records the size; 0 starts a fresh
    ///   stream; any other size switches to an overlay (no content read).
    /// - Overlay: only the size and the extents change (no I/O, no allocation
    ///   by size); a resize to 0 goes back to a fresh stream.
    pub fn test_setattr_size(&self, ino: u64, fh: Option<u64>, new_size: u64) -> Result<(), i32> {
        if new_size > MAX_FILE_SIZE {
            return Err(libc::EFBIG);
        }
        // A size change without a handle still belongs to an open write
        // handle of the file (the NFS SETATTR from cp carries no handle).
        let fh = fh.map(|f| self.owner_fh(f)).filter(|f| self.open_files.lock().unwrap().contains_key(f))
            .or_else(|| self.write_handle_of(ino));
        if let Some(fh_val) = fh {
            // Extending a streaming handle: record the size, no materializing.
            let extended = {
                let mut open_files = self.open_files.lock().unwrap();
                match open_files.get_mut(&fh_val) {
                    Some(s) if matches!(s.write_mode, WriteMode::Streaming { .. })
                        && new_size > 0 && new_size >= s.byte_count
                        && (s.byte_count > 0 || !self.has_content(ino)) => {
                        s.min_size = new_size;
                        s.byte_count = new_size;
                        s.cas_committed = false;                        s.modified = Some(now_pair());
                        true
                    }
                    _ => false,
                }
            };
            if extended {
                let mut inode = self.meta.get_inode(ino).map_err(|_| libc::EIO)?;
                inode.size = new_size;
                let (sec, nsec) = now_pair();
                inode.mtime_sec = sec;
                inode.mtime_nsec = nsec;
                inode.ctime_sec = sec;
                inode.ctime_nsec = nsec;
                self.meta.update_inode(&inode).map_err(|_| libc::EIO)?;
                return Ok(());
            }
            self.settle_pending(fh_val);
        }
        // Case A: open file handle — truncate in-flight write state
        if let Some(fh_val) = fh {
            let mut open_files = self.open_files.lock().unwrap();
            if let Some(s) = open_files.get_mut(&fh_val) {
                match &mut s.write_mode {
                    WriteMode::Overlay(ov) => {
                        if new_size == 0 {
                            let old_root = s.last_committed_root.take();
                            s.write_mode = WriteMode::Streaming { state: Stream::default(), next_expected_offset: 0, pending: BTreeMap::new() };
                            s.byte_count = 0;
                            s.min_size = 0;
                            s.cas_committed = false;                            s.modified = Some(now_pair());
                            drop(open_files);

                            if let Some(old) = old_root {
                                self.meta.decrement_refcount(&old);
                            }
                            self.clear_manifest(ino, old_root)?;
                        } else {
                            ov.truncate(new_size);
                            s.byte_count = ov.size;
                            s.min_size = 0;
                            s.cas_committed = false;                            s.modified = Some(now_pair());
                            drop(open_files);
                        }
                    }
                    WriteMode::Streaming { .. } => {
                        if new_size == 0 {
                            // Fast path: reset to empty Streaming State
                            let old_root = s.last_committed_root.take();
                            s.write_mode = WriteMode::Streaming { state: Stream::default(), next_expected_offset: 0, pending: BTreeMap::new() };
                            s.byte_count = 0;
                            s.min_size = 0;
                            s.cas_committed = false;                            s.modified = Some(now_pair());
                            drop(open_files);

                            if let Some(old) = old_root {
                                self.meta.decrement_refcount(&old);
                            }
                            self.clear_manifest(ino, old_root)?;
                        } else {
                            // Not a pure extension: switch to an overlay on
                            // the content so far and cut or extend it there.
                            // No file content is read.
                            let mut st = open_files.remove(&fh_val).expect("state is in the map");
                            drop(open_files); // Release before io
                            let switched = self.to_overlay(&mut st).map(|_| {
                                if let WriteMode::Overlay(ov) = &mut st.write_mode {
                                    ov.truncate(new_size);
                                    st.byte_count = ov.size;
                                }
                                st.cas_committed = false;
                                st.modified = Some(now_pair());
                            });
                            self.open_files.lock().unwrap().insert(fh_val, st);
                            switched?;
                        }
                    }
                }

                // Update inode size immediately
                let mut inode = self.meta.get_inode(ino).map_err(|_| libc::EIO)?;
                inode.size = new_size;
                // truncate changes the content: mtime and ctime both change.
                let (sec, nsec) = now_pair();
                inode.mtime_sec = sec;
                inode.mtime_nsec = nsec;
                inode.ctime_sec = sec;
                inode.ctime_nsec = nsec;
                self.meta.update_inode(&inode).map_err(|_| libc::EIO)?;
                return Ok(());
            }
        }

        // Case B: no open write handle — splice the committed content to the
        // new size, truncated or zero-extended. NotFound from get_manifest means the inode exists but has no content yet
        // (brand-new file created by create() before any write/release). Treat as empty content.
        let old_manifest = match self.meta.get_manifest(ino) {
            Ok(m) => m,
            Err(MetaError::NotFound(_)) => vec![],
            Err(_) => return Err(libc::EIO),
        };

        // Decrement old refcount
        if !old_manifest.is_empty() {
            self.meta.decrement_refcount(&old_manifest[0]);
        }

        // Splice the old content to the new size: only the chunks near the
        // old end (a cut, or the last chunk of an extend) are cut again; the
        // other chunks and whole nodes keep their IDs, and an extend pushes
        // zero chunks without a hash. Same key as a full write.
        if new_size == 0 {
            self.meta.set_manifest(ino, &[]).map_err(|_| libc::EIO)?;
        } else {
            let cs = &*self.content;
            let store_err = |e: std::io::Error| {
                eprintln!("[{}][FUSE] setattr size: content store error: {e}", ts());
                libc::EIO
            };
            let old_root = old_manifest.first().copied();
            let old_len = match &old_root {
                Some(k) => content::Reader::open(cs, k).map_err(store_err)?.len(),
                None => 0,
            };
            let mut ov = Overlay::new(old_root, old_len, old_len);
            ov.truncate(new_size);
            let mut src = OverlaySource::new(cs, &ov).map_err(store_err)?;
            let (new_digest, stats) = content::splice(cs, old_root.as_ref(), ov.visible, &[], new_size, &mut src)
                .map_err(store_err)?;
            self.last_rebase_bytes.store(stats.rechunked, Ordering::Relaxed);
            self.meta.set_manifest(ino, &[new_digest]).map_err(|_| libc::EIO)?;
            self.meta.increment_refcount(&new_digest);
        }

        // Update inode
        let mut inode = self.meta.get_inode(ino).map_err(|_| libc::EIO)?;
        inode.size = new_size;
        // truncate changes the content: mtime and ctime both change.
        let (sec, nsec) = now_pair();
        inode.mtime_sec = sec;
        inode.mtime_nsec = nsec;
        inode.ctime_sec = sec;
        inode.ctime_nsec = nsec;
        self.meta.update_inode(&inode).map_err(|_| libc::EIO)?;
        Ok(())
    }

    /// Update permission bits (preserving file type bits) for an inode.
    pub fn test_setattr_mode(&self, ino: u64, mode: u32) -> Result<(), i32> {
        let mut inode = self.meta.get_inode(ino).map_err(|_| libc::EIO)?;
        inode.mode = (inode.mode & S_IFMT) | (mode & 0o7777);
        let (now_sec, now_nsec) = now_pair();
        inode.ctime_sec = now_sec;
        inode.ctime_nsec = now_nsec;
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
        let (now_sec, now_nsec) = now_pair();
        inode.ctime_sec = now_sec;
        inode.ctime_nsec = now_nsec;
        self.meta.update_inode(&inode).map_err(|_| libc::EIO)?;
        Ok(())
    }

    /// Update mtime for an inode to a specific (sec, nsec).
    pub fn test_setattr_mtime(&self, ino: u64, mtime_sec: i64, mtime_nsec: u32) -> Result<(), i32> {
        let mut inode = self.meta.get_inode(ino).map_err(|_| libc::EIO)?;
        inode.mtime_sec = mtime_sec;
        inode.mtime_nsec = mtime_nsec;
        let (now_sec, now_nsec) = now_pair();
        inode.ctime_sec = now_sec;
        inode.ctime_nsec = now_nsec;
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
    pub fn test_mknod(&self, parent: u64, name: &str, mode: u32, uid: u32, gid: u32, umask: u32) -> Result<u64, i32> {
        self.test_mknod_rdev(parent, name, mode, 0, uid, gid, umask).map(|(ino, _)| ino)
    }

    /// mknod with a device number. Creates regular files, FIFOs, sockets and
    /// character/block device nodes (the kernel implements their I/O; only
    /// the type bits and `rdev` are stored). Returns `(ino, InodeMeta)`.
    #[allow(clippy::too_many_arguments)]
    pub fn test_mknod_rdev(
        &self, parent: u64, name: &str, mode: u32, rdev: u32, uid: u32, gid: u32, umask: u32,
    ) -> Result<(u64, InodeMeta), i32> {
        let file_type = match mode & S_IFMT {
            0 | S_IFREG => S_IFREG,
            t @ (S_IFIFO | S_IFSOCK | S_IFCHR | S_IFBLK) => t,
            _ => return Err(libc::EINVAL),
        };
        let file_mode = file_type | (mode & !umask & 0o7777);
        let mut file_meta = InodeMeta::new_file(0, uid, gid, file_mode);
        Self::stamp_new(&mut file_meta);
        if file_type == S_IFCHR || file_type == S_IFBLK {
            file_meta.rdev = rdev;
        }
        let ino = self.meta.create_inode(&file_meta).map_err(|e| meta_error_to_errno(&e))?;
        if let Err(e) = self.meta.link(parent, name, ino) {
            let _ = self.meta.delete_inode(ino);
            return Err(meta_error_to_errno(&e));
        }
        self.touch_dir(parent)?;
        let meta = self.meta.get_inode(ino).map_err(|e| meta_error_to_errno(&e))?;
        Ok((ino, meta))
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
                let bsize = if sv.f_frsize > 0 { sv.f_frsize as u32 } else { 4096 };
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
                let bsize = if sf.f_bsize > 0 { sf.f_bsize as u32 } else { 4096 };
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
        let mut dir_meta = InodeMeta::new_directory(0, uid, gid, dir_mode);
        Self::stamp_new(&mut dir_meta);
        let ino = self.meta
            .create_directory(parent_ino, name, &dir_meta)
            .map_err(|e| meta_error_to_errno(&e))?;
        self.touch_dir(parent_ino)?;
        Ok(ino)
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
        if inode.mode & S_IFMT != S_IFDIR {
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
        self.touch_dir(parent_ino)?;
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
        if inode.mode & S_IFMT == S_IFDIR {
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
            // Reclaim now, or keep as an orphan while handles are open.
            self.drop_last_link(inode)?;
        } else {
            // Hard links still exist — persist the decremented nlinks; the
            // link count changed, so ctime changes too.
            let (sec, nsec) = now_pair();
            inode.ctime_sec = sec;
            inode.ctime_nsec = nsec;
            self.meta.update_inode(&inode).map_err(|e| meta_error_to_errno(&e))?;
        }
        self.touch_dir(parent_ino)?;
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
        if inode.mode & S_IFMT == S_IFDIR {
            return Err(libc::EPERM);
        }

        // Add new directory entry
        self.meta
            .link(newparent_ino, newname, ino)
            .map_err(|e| meta_error_to_errno(&e))?;

        // Increment nlinks and update ctime
        inode.nlinks += 1;
        let (now_sec, now_nsec) = now_pair();
        inode.ctime_sec = now_sec;
        inode.ctime_nsec = now_nsec;
        self.meta.update_inode(&inode).map_err(|e| meta_error_to_errno(&e))?;
        self.touch_dir(newparent_ino)?;

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
            .map_err(|e| meta_error_to_errno(&e))?;
        let src_is_dir = self.meta.get_inode(src_ino).map_err(|e| meta_error_to_errno(&e))?
            .mode & S_IFMT == S_IFDIR;

        // RENAME_NOREPLACE: fail if target already exists
        let target_ino = match self.meta.lookup(newparent_ino, newname) {
            Ok(ino) => Some(ino),
            Err(MetaError::NameTooLong(n)) => return Err(meta_error_to_errno(&MetaError::NameTooLong(n))),
            Err(_) => None,
        };
        if flags & 1 != 0 && target_ino.is_some() {
            return Err(libc::EEXIST);
        }

        if let Some(dst_ino) = target_ino {
            // POSIX: old and new name the same file -> do nothing.
            if dst_ino == src_ino {
                return Ok(());
            }
            let dst_inode = self.meta.get_inode(dst_ino).map_err(|e| meta_error_to_errno(&e))?;
            let dst_is_dir = dst_inode.mode & S_IFMT == S_IFDIR;
            match (src_is_dir, dst_is_dir) {
                (true, false) => return Err(libc::ENOTDIR),
                (false, true) => return Err(libc::EISDIR),
                _ => {}
            }
            if dst_is_dir {
                let entries = self.meta.list_directory(dst_ino).map_err(|e| meta_error_to_errno(&e))?;
                if entries.iter().any(|e| e.name != "." && e.name != "..") {
                    return Err(libc::ENOTEMPTY);
                }
            }
            self.meta
                .unlink(newparent_ino, newname)
                .map_err(|e| meta_error_to_errno(&e))?;
            if dst_is_dir {
                // The replaced empty directory goes away with its ".." link.
                self.meta.delete_inode(dst_ino).map_err(|e| meta_error_to_errno(&e))?;
                self.adjust_nlinks(newparent_ino, -1)?;
            } else {
                let new_nlinks = dst_inode.nlinks.saturating_sub(1);
                if new_nlinks == 0 {
                    // Reclaim now, or keep as an orphan while handles are open.
                    self.drop_last_link(dst_inode.clone())?;
                } else {
                    let mut updated_dst = dst_inode.clone();
                    updated_dst.nlinks = new_nlinks;
                    let (sec, nsec) = now_pair();
                    updated_dst.ctime_sec = sec;
                    updated_dst.ctime_nsec = nsec;
                    self.meta.update_inode(&updated_dst).map_err(|e| meta_error_to_errno(&e))?;
                }
            }
        }

        // Link source at new location and unlink from old location
        self.meta
            .link(newparent_ino, newname, src_ino)
            .map_err(|e| meta_error_to_errno(&e))?;
        self.meta
            .unlink(parent_ino, name)
            .map_err(|e| meta_error_to_errno(&e))?;

        // A directory moved to another parent: fix its ".." and both parents' nlink.
        if src_is_dir && parent_ino != newparent_ino {
            self.meta.set_parent(src_ino, newparent_ino).map_err(|e| meta_error_to_errno(&e))?;
            self.adjust_nlinks(parent_ino, -1)?;
            self.adjust_nlinks(newparent_ino, 1)?;
        }
        self.touch_dir(parent_ino)?;
        if newparent_ino != parent_ino {
            self.touch_dir(newparent_ino)?;
        }

        // Update ctime on moved inode
        let mut src_inode = self.meta.get_inode(src_ino).map_err(|e| meta_error_to_errno(&e))?;
        let (now_sec, now_nsec) = now_pair();
        src_inode.ctime_sec = now_sec;
        src_inode.ctime_nsec = now_nsec;
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
        let mut symlink_meta = InodeMeta {
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
            rdev: 0, atime_sec: 0, atime_nsec: 0,
        };
        Self::stamp_new(&mut symlink_meta);

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
        self.touch_dir(parent_ino)?;

        // Store target as CAS content (raw bytes — v3 format, no compression)
        let content_digest = content::put_all(&self.content, target_bytes).map_err(|_| libc::EIO)?;
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
        let raw_bytes: Vec<u8> = content::get_all(&self.content, &root_digest).ok_or(libc::EINVAL)?;
        String::from_utf8(raw_bytes).map_err(|_| libc::EINVAL)
    }

    // ── Testable wrappers for FUSE callbacks ─────────────────────────────────

    /// Get inode attributes. Returns `InodeMeta` on success.
    pub fn test_getattr(&self, ino: u64) -> Result<InodeMeta, MetaError> {
        let mut meta = self.meta.get_inode(ino)?;
        self.overlay_open_size(&mut meta);
        Ok(meta)
    }

    /// Bytes written through an open handle stay in its write state until
    /// release; report that size so fstat/stat during a write are correct.
    fn overlay_open_size(&self, meta: &mut InodeMeta) {
        let open_files = self.open_files.lock().unwrap();
        if let Some(s) = open_files.values().find(|s| s.ino == meta.ino && s.byte_count > 0) {
            meta.size = s.byte_count;
        }
        // Unflushed writes: show the mtime (and ctime) the close will stamp.
        if let Some((sec, nsec)) = open_files.values().find(|s| s.ino == meta.ino).and_then(|s| s.modified) {
            meta.mtime_sec = sec;
            meta.mtime_nsec = nsec;
            if (sec, nsec) > (meta.ctime_sec, meta.ctime_nsec) {
                meta.ctime_sec = sec;
                meta.ctime_nsec = nsec;
            }
        }
    }

    /// Lookup a name in a directory. Returns `(child_ino, InodeMeta)` on success.
    pub fn test_lookup(&self, parent: u64, name: &str) -> Result<(u64, InodeMeta), i32> {
        let child_ino = self.meta.lookup(parent, name).map_err(|e| meta_error_to_errno(&e))?;
        let mut meta = self.meta.get_inode(child_ino).map_err(|e| meta_error_to_errno(&e))?;
        self.overlay_open_size(&mut meta);
        Ok((child_ino, meta))
    }

    /// List directory entries starting at `offset`.
    /// Returns `Vec<(ino, kind, name)>` tuples.
    pub fn test_readdir(&self, ino: u64, offset: u64) -> Result<Vec<(u64, FileType, String)>, i32> {
        let entries = self.meta.list_directory(ino).map_err(|e| meta_error_to_errno(&e))?;
        let mut result = Vec::new();
        for entry in entries.iter().skip(offset as usize) {
            let kind = match self.meta.get_inode(entry.ino) {
                Ok(m) => inode_to_file_attr(&m).kind,
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

        let fh = self.open_handle(ino);

        if is_write {
            // Another write handle of this inode is open: share its state.
            let owner = self.open_files.lock().unwrap().iter().find(|(_, st)| st.ino == ino).map(|(&f, _)| f);
            if let Some(owner) = owner {
                self.write_alias.lock().unwrap().insert(fh, owner);
            } else {
            self.open_files.lock().unwrap().insert(
                fh,
                OpenFileState {
                    ino,
                    write_mode: WriteMode::Streaming { state: Stream::default(), next_expected_offset: 0, pending: BTreeMap::new() },
                    byte_count: 0,
                    cas_committed: false,                    modified: None,
                    last_committed_root: None,
                    min_size: 0,
                },
            );
            }

            // FUSE_ATOMIC_O_TRUNC
            if flags & libc::O_TRUNC != 0 {
                if let Err(e) = self.test_setattr_size(ino, Some(fh), 0) {
                    if self.write_alias.lock().unwrap().remove(&fh).is_none() {
                        self.open_files.lock().unwrap().remove(&fh);
                    }
                    self.close_handle(fh);
                    return Err(e);
                }
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
        let names = self.meta.list_xattrs(ino).map_err(|e| meta_error_to_errno(&e))?;
        let mut buf = Vec::new();
        for name in &names {
            buf.extend_from_slice(name.as_bytes());
            buf.push(0);
        }
        Ok(buf)
    }

    /// Set an extended attribute.
    pub fn test_setxattr(&self, ino: u64, name: &str, value: &[u8]) -> Result<(), i32> {
        self.meta.set_xattr(ino, name, value).map_err(|e| meta_error_to_errno(&e))
    }

    /// Remove an extended attribute.
    pub fn test_removexattr(&self, ino: u64, name: &str) -> Result<(), i32> {
        self.meta.remove_xattr(ino, name).map_err(|e| meta_error_to_errno(&e))
    }

    /// Combined setattr: apply mode, uid, gid, mtime, and size changes.
    /// Returns the updated `InodeMeta`.
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
        self.test_setattr_full(ino, mode, uid, gid, size, fh, None, mtime)
    }

    /// Set only access and modification times (utimensat).
    pub fn test_setattr_times(
        &self,
        ino: u64,
        atime: Option<(i64, u32)>,
        mtime: Option<(i64, u32)>,
    ) -> Result<InodeMeta, i32> {
        self.test_setattr_full(ino, None, None, None, None, None, atime, mtime)
    }

    /// setattr with every supported field.
    #[allow(clippy::too_many_arguments)]
    pub fn test_setattr_full(
        &self,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        fh: Option<u64>,
        atime: Option<(i64, u32)>,
        mtime: Option<(i64, u32)>,
    ) -> Result<InodeMeta, i32> {
        // Size first: it rewrites the inode (size, mtime, ctime); the other
        // fields of the same request are applied on top of the result.
        if let Some(new_size) = size {
            self.test_setattr_size(ino, fh, new_size)?;
        }
        let mut inode = self.meta.get_inode(ino).map_err(|e| meta_error_to_errno(&e))?;
        if let Some((sec, nsec)) = atime {
            inode.atime_sec = sec;
            inode.atime_nsec = nsec;
        }

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
            // An explicit mtime (utimens) on a file with unflushed writes
            // wins over the time of those writes: the close stamps this one.
            for s in self.open_files.lock().unwrap().values_mut() {
                if s.ino == ino && s.modified.is_some() {
                    s.modified = Some((sec, nsec));
                }
            }
        }

        // Always update ctime
        let (now_sec, now_nsec) = now_pair();
        inode.ctime_sec = now_sec;
        inode.ctime_nsec = now_nsec;

        self.meta.update_inode(&inode).map_err(|e| meta_error_to_errno(&e))?;
        Ok(inode)
    }

    // ── Higher-level testable wrappers ───────────────────────────────────────
    //
    // These combine the core test_*/simulate_* method with the get_inode
    // call that the FUSE callback needs, moving logic out of untestable
    // Filesystem trait methods into testable pub methods.

    /// Create a file and return `(ino, fh, InodeMeta)`.
    /// Handles `O_TRUNC` flag atomically.
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
        if flags & libc::O_TRUNC != 0 {
            if let Err(e) = self.test_setattr_size(ino, Some(fh), 0) {
                self.open_files.lock().unwrap().remove(&fh);
                self.close_handle(fh);
                return Err(e);
            }
        }
        let meta = self.meta.get_inode(ino).map_err(|e| meta_error_to_errno(&e))?;
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
        let meta = self.meta.get_inode(ino).map_err(|e| meta_error_to_errno(&e))?;
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
        self.test_mknod_rdev(parent, name, mode, 0, uid, gid, umask)
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
        let meta = self.meta.get_inode(ino).map_err(|e| meta_error_to_errno(&e))?;
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
        let meta = self.meta.get_inode(new_ino).map_err(|e| meta_error_to_errno(&e))?;
        Ok((new_ino, meta))
    }

    /// Flush write buffer without closing the handle.
    /// Wrapper around `flush_buffer_for_fsync` exposed for testing.
    pub fn test_flush(&self, ino: u64, fh: u64) -> Result<(), i32> {
        self.flush_buffer_for_fsync(ino, fh)
    }

    /// Release with read-only handle check (matches FUSE release behavior).
    pub fn test_release_full(&self, ino: u64, fh: u64) -> Result<(), i32> {
        let shared = self.write_alias.lock().unwrap().contains_key(&fh);
        if !shared && !self.open_files.lock().unwrap().contains_key(&fh) {
            self.close_handle(fh); // read-only handle, no write state
            return Ok(());
        }
        self.test_release(ino, fh)
    }

    /// Destroy: commit, optionally snapshot, and shut down WAL.
    /// Returns true if commit succeeded.
    pub fn test_destroy(&self) -> bool {
        // Write the content of handles that are still open (e.g. a forced
        // unmount), so the commit below keeps it.
        let open: Vec<(u64, u64)> = self.open_files.lock().unwrap()
            .iter().map(|(&fh, st)| (fh, st.ino)).collect();
        for (fh, ino) in open {
            if let Err(e) = self.finalize_write(ino, fh) {
                eprintln!("[{}][FUSE] destroy: could not write open handle fh={} ino={}: errno {}", ts(), fh, ino, e);
            }
        }
        // No handle survives unmount: unlinked-but-open inodes go away now.
        self.reclaim_all_orphans();
        if let Ok(_root) = self.meta.commit() {
            if self.auto_snapshot {
                let _ = self.meta.create_snapshot(Some("auto-unmount".to_string()));
            }
            let _ = self.meta.shutdown_wal();
            // Seal the active pack (sync, index), so the next open needs no scan.
            if let Err(e) = self.content.close() {
                eprintln!("[{}][FUSE] destroy: content store close failed: {}", ts(), e);
            }
            true
        } else {
            false
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use metadata::store::DictMetadataStore;
    use metadata::store_io::StoreIo;
    use slicefs_traits::metadata::{InodeMeta, MetadataStore};
    use tempfile::TempDir;

    const S_IFDIR_TEST: u32 = 0o040_000;
    const S_IFREG_TEST: u32 = 0o100_000;

    fn fresh_fs() -> (SliceFsFilesystem, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let io = Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap()));
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
        let io = Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap()));
        let content = b"hello, SliceFS content!";
        // Write content into the chunk store

        let meta_store = DictMetadataStore::new(io.clone());
        let file_meta = InodeMeta::new_file(0, 0, 0, S_IFREG_TEST | 0o644);
        let ino = meta_store.create_inode(&file_meta).unwrap();
        meta_store.link(1, "testfile", ino).unwrap();

        let fs = SliceFsFilesystem::new(meta_store, io.clone(), None);
        let root_key = content::put_all(fs.content(), content).unwrap();
        fs.meta.set_manifest(ino, &[root_key]).unwrap();

        let manifest = fs.meta.get_manifest(ino).unwrap();
        assert!(!manifest.is_empty());

        let bytes = fs.read_content(&manifest[0]).unwrap();

        assert_eq!(bytes, content);
    }

    #[test]
    fn test_read_with_offset() {
        let dir = tempfile::tempdir().unwrap();
        let io = Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap()));
        let content = b"hello, SliceFS offset test!";

        let meta_store = DictMetadataStore::new(io.clone());
        let file_meta = InodeMeta::new_file(0, 0, 0, S_IFREG_TEST | 0o644);
        let ino = meta_store.create_inode(&file_meta).unwrap();

        let fs = SliceFsFilesystem::new(meta_store, io.clone(), None);
        let root_key = content::put_all(fs.content(), content).unwrap();
        fs.meta.set_manifest(ino, &[root_key]).unwrap();

        let manifest = fs.meta.get_manifest(ino).unwrap();
        let bytes = fs.read_content(&manifest[0]).unwrap();
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
            metadata::store_io::StoreIo::open(dir.path()).unwrap(),
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

    // ── fresh_fs_with_store: helper that sets store_path ─────────────────────

    fn fresh_fs_with_store() -> (SliceFsFilesystem, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let io = Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap()));
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
        let (ino, _fh) = fs.test_create(1, "hello.txt", 0o644, 0, 1000, 1000).unwrap();
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
        assert!(list.len() > 0);
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
        let meta = fs.test_setattr(ino, Some(0o755), None, None, None, None, None).unwrap();
        assert_eq!(meta.mode & 0o7777, 0o755);
    }

    #[test]
    fn test_setattr_uid_gid() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        let _ = fs.test_release(ino, fh);
        let meta = fs.test_setattr(ino, None, Some(500), Some(600), None, None, None).unwrap();
        assert_eq!(meta.uid, 500);
        assert_eq!(meta.gid, 600);
    }

    #[test]
    fn test_setattr_mtime() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        let _ = fs.test_release(ino, fh);
        let meta = fs.test_setattr(ino, None, None, None, None, None, Some((12345, 678))).unwrap();
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
        let meta = fs.test_setattr(ino, None, None, None, Some(5), None, None).unwrap();
        assert_eq!(meta.size, 5);
        // Read back
        let data = fs.test_read(ino, 0, 100).unwrap();
        assert_eq!(&data, b"hello");
    }

    #[test]
    fn test_sequential_chunked_read_of_large_file() {
        let (fs, _dir) = fresh_fs();
        let data: Vec<u8> = (0..1_500_000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        let (ino, fh) = fs.test_create(1, "big", 0o644, 0, 0, 0).unwrap();
        for (i, c) in data.chunks(65536).enumerate() {
            fs.test_write(fh, (i * 65536) as u64, c).unwrap();
        }
        // Read while the write handle is open (streaming snapshot root).
        assert_eq!(fs.test_read(ino, 1_000_000, 10).unwrap(), &data[1_000_000..1_000_010]);
        fs.test_release(ino, fh).unwrap();
        let mut out = Vec::new();
        loop {
            let chunk = fs.test_read(ino, out.len() as u64, 131072).unwrap();
            if chunk.is_empty() { break; }
            out.extend(chunk);
        }
        assert_eq!(out, data);
        // Backwards seek after the sequential pass.
        assert_eq!(fs.test_read(ino, 7, 5).unwrap(), &data[7..12]);
    }

    #[test]
    fn test_read_sees_new_content_after_rewrite() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"first version").unwrap();
        fs.test_release(ino, fh).unwrap();
        assert_eq!(fs.test_read(ino, 0, 5).unwrap(), b"first");
        let fh = fs.test_open(ino, libc::O_WRONLY | libc::O_TRUNC).unwrap().0;
        fs.test_write(fh, 0, b"second").unwrap();
        fs.test_release(ino, fh).unwrap();
        assert_eq!(fs.test_read(ino, 0, 100).unwrap(), b"second");
    }

    #[test]
    fn test_setattr_size_beyond_max_returns_efbig() {
        // pjdfstest truncate/12: a huge length must fail with EFBIG, not abort the daemon.
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"hi").unwrap();
        assert_eq!(fs.test_setattr_size(ino, Some(fh), 999_999_999_999_999), Err(libc::EFBIG));
        fs.test_release(ino, fh).unwrap();
        assert_eq!(fs.test_setattr_size(ino, None, 999_999_999_999_999), Err(libc::EFBIG));
        assert_eq!(fs.test_read(ino, 0, 100).unwrap(), b"hi");
    }

    #[test]
    fn test_write_beyond_max_returns_efbig() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"hi").unwrap();
        assert_eq!(fs.test_write(fh, 999_999_999_999_999, b"x"), Err(libc::EFBIG));
        // The handle is still usable after the rejected write.
        assert_eq!(fs.test_write(fh, 2, b"!"), Ok(1));
        fs.test_release(ino, fh).unwrap();
        assert_eq!(fs.test_read(ino, 0, 100).unwrap(), b"hi!");
    }

    #[test]
    fn test_setattr_size_extend_closed_file() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"hi").unwrap();
        fs.test_release(ino, fh).unwrap();
        // Extend to 10 bytes
        let meta = fs.test_setattr(ino, None, None, None, Some(10), None, None).unwrap();
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
        let meta = fs.test_setattr(ino, None, None, None, Some(0), None, None).unwrap();
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
        let ino = fs.test_mknod(1, "node", S_IFREG_TEST | 0o644, 0, 0, 0).unwrap();
        let meta = fs.test_getattr(ino).unwrap();
        assert_ne!(meta.mode & S_IFREG, 0);
    }

    #[test]
    fn test_mknod_char_device_succeeds() {
        let (fs, _dir) = fresh_fs();
        // S_IFCHR = 0o020_000
        let (_ino, meta) = fs.test_mknod_rdev(1, "chardev", 0o020_000 | 0o644, 7, 0, 0, 0).unwrap();
        assert_eq!(meta.rdev, 7);
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
            rdev: 0, atime_sec: 0, atime_nsec: 0,
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
            rdev: 0, atime_sec: 0, atime_nsec: 0,
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
            rdev: 0, atime_sec: 0, atime_nsec: 0,
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
        let _ = meta_error_to_fuse_errno(&MetaError::Io(std::io::Error::new(std::io::ErrorKind::Other, "x")));
    }

    // ── meta_error_to_errno full coverage ────────────────────────────────────

    #[test]
    fn test_meta_error_to_errno_all_variants() {
        assert_eq!(meta_error_to_errno(&MetaError::NotEmpty(0)), libc::ENOTEMPTY);
        assert_eq!(meta_error_to_errno(&MetaError::InvalidName("x".into())), libc::EINVAL);
        assert_eq!(meta_error_to_errno(&MetaError::Io(std::io::Error::new(std::io::ErrorKind::Other, "x"))), libc::EIO);
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

    // Two write handles share one overlay (the owner state). Writes through
    // both, in any order, and a release of the owner first, keep every byte.
    #[test]
    fn two_aliased_handles_on_an_overlay_keep_every_write() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        let mut model: Vec<u8> = (0..1u32 << 20).map(|i| (i * 7 + 3) as u8).collect();
        fs.test_write(fh, 0, &model).unwrap();
        fs.test_release(ino, fh).unwrap();

        let (h1, _) = fs.test_open(ino, libc::O_RDWR).unwrap();
        let (h2, _) = fs.test_open(ino, libc::O_RDWR).unwrap();
        let mut write = |h: u64, off: usize, data: &[u8]| {
            fs.test_write(h, off as u64, data).unwrap();
            model[off..off + data.len()].copy_from_slice(data);
        };
        write(h1, 1000, &[0xA1; 100]); // the file has content: switch
        assert!(!fs.test_is_streaming(h1));
        write(h2, 500_000, &[0xB2; 300]);
        write(h1, 2000, &[0xC3; 50]);
        fs.test_release(ino, h1).unwrap(); // h2 takes the state over
        write(h2, 3000, &[0xD4; 10]);
        fs.test_release(ino, h2).unwrap();

        assert_eq!(fs.meta.get_inode(ino).unwrap().size, model.len() as u64);
        assert!(fs.test_read(ino, 0, model.len() as u32 + 1).unwrap() == model, "content after both releases");
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
        fs.simulate_rename(dir_a, "file", dir_b, "moved", 0).unwrap();
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
        let meta = fs.test_setattr(
            ino,
            Some(0o755),
            Some(100),
            Some(200),
            None,
            None,
            Some((1234567890, 42)),
        ).unwrap();
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
        let meta = fs.test_setattr(
            ino,
            None,
            None,
            None,
            Some(5),
            None,
            None,
        ).unwrap();
        assert_eq!(meta.size, 5);
        // Now apply mode/uid/gid separately
        let meta2 = fs.test_setattr(
            ino,
            Some(0o755),
            Some(100),
            Some(200),
            None,
            None,
            None,
        ).unwrap();
        assert_eq!(meta2.mode & 0o7777, 0o755);
        assert_eq!(meta2.uid, 100);
        assert_eq!(meta2.gid, 200);
        assert_eq!(meta2.size, 5);
    }

    #[test]
    fn test_setattr_nonexistent_inode() {
        let (fs, _dir) = fresh_fs();
        let err = fs.test_setattr(999, Some(0o755), None, None, None, None, None).unwrap_err();
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
            uid: 0, gid: 0, nlinks: 1, size: 0,
            mtime_sec: 0, mtime_nsec: 0, ctime_sec: 0, ctime_nsec: 0, rdev: 0, atime_sec: 0, atime_nsec: 0,
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
        let ino = fs.test_mknod(1, "newfile", S_IFREG | 0o644, 0, 0, 0).unwrap();
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
        let (ino, fh, meta) = fs.test_create_full(1, "file.txt", 0o644, 0, 0, 0, 0).unwrap();
        assert!(ino > 1);
        assert!(fh > 0);
        assert_ne!(meta.mode & S_IFREG, 0);
        fs.test_release(ino, fh).unwrap();
    }

    #[test]
    fn test_create_full_with_o_trunc() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh, meta) = fs.test_create_full(1, "f", 0o644, 0, 0, 0, libc::O_TRUNC).unwrap();
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
        let (ino, meta) = fs.test_mknod_full(1, "node", S_IFREG_TEST | 0o644, 0, 0, 0).unwrap();
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
        let io = Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap()));
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

    // ── dirty limit, rebase, relog in pieces (#98) ──────────────────────────

    /// A file system on a disk store with a per-op write log (commit_cut logs).
    fn fs_with_log() -> (SliceFsFilesystem, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("segments")).unwrap();
        let wal = metadata::wal::create_wal(metadata::wal::WalConfig::PerOp, dir.path(), 1).unwrap();
        let io = Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap()));
        let mut meta = DictMetadataStore::new(io.clone());
        meta.set_wal(wal);
        let fs = SliceFsFilesystem::new(meta, io, Some(dir.path().to_path_buf()));
        fs.meta().commit().unwrap();
        (fs, dir)
    }

    /// Seeded bytes (xorshift), so each test gets the same data every run.
    fn seeded(seed: u64, n: usize) -> Vec<u8> {
        let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        (0..n).map(|_| { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x as u8 }).collect()
    }

    /// RAM charged to the overlay of `fh` (0 if it streams).
    fn overlay_cost(fs: &SliceFsFilesystem, fh: u64) -> usize {
        match fs.open_files.lock().unwrap().get(&fs.owner_fh(fh)).map(|s| &s.write_mode) {
            Some(WriteMode::Overlay(ov)) => ov.cost(),
            _ => 0,
        }
    }

    #[test]
    fn an_overlay_rebases_when_it_crosses_the_dirty_limit() {
        let (fs, _dir) = fresh_fs();
        let limit = 64 << 10;
        fs.test_set_dirty_limit(limit);
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        let mut model = seeded(1, 1 << 20);
        for (i, piece) in model.chunks(64 << 10).enumerate() {
            fs.test_write(fh, (i << 16) as u64, piece).unwrap();
        }
        fs.test_release(ino, fh).unwrap();
        assert_eq!(fs.test_rebase_count(), 0, "a streamed file needs no rebase");

        let (fh, _) = fs.test_open(ino, libc::O_RDWR).unwrap();
        let offsets = seeded(2, 200 * 8);
        for i in 0..200 {
            let w = u64::from_le_bytes(offsets[i * 8..i * 8 + 8].try_into().unwrap());
            let off = (w % ((1 << 20) - 1024)) as usize;
            let data = seeded(100 + i as u64, 1024);
            fs.test_write(fh, off as u64, &data).unwrap();
            model[off..off + 1024].copy_from_slice(&data);
            assert!(overlay_cost(&fs, fh) <= limit + 1024 + crate::overlay::EXTENT_OVERHEAD,
                "write {i}: overlay cost {} over the limit", overlay_cost(&fs, fh));
        }
        let random = fs.test_rebase_count();
        assert!(random >= 2, "random writes: {random} rebase(s)");

        // An in-order rewrite merges into one extent; it must not grow to the file size.
        for (i, piece) in seeded(3, 1 << 20).chunks(4096).enumerate() {
            let off = i * 4096;
            fs.test_write(fh, off as u64, piece).unwrap();
            model[off..off + 4096].copy_from_slice(piece);
            assert!(overlay_cost(&fs, fh) <= limit + 4096 + crate::overlay::EXTENT_OVERHEAD,
                "in-order write {i}: overlay cost {} over the limit", overlay_cost(&fs, fh));
        }
        assert!(fs.test_rebase_count() >= random + 8, "in-order rewrite: {} rebase(s)", fs.test_rebase_count() - random);
        // The splice cuts again only near the last extent (at most 2 x limit
        // plus one write), not the whole 1 MiB file.
        let last = fs.test_last_rebase_bytes();
        assert!(last > 0 && last <= 2 * limit as u64 + 4096 + 8 * 32768, "last rebase re-chunked {last} bytes");
        fs.test_release(ino, fh).unwrap();
        assert!(fs.test_read(ino, 0, (2 << 20) as u32).unwrap() == model, "content after release");
    }

    #[test]
    fn a_switch_that_moves_pending_writes_over_the_limit_rebases_at_once() {
        let (fs, _dir) = fresh_fs();
        fs.test_set_dirty_limit(64 << 10);
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        let mut model = vec![0u8; (1 << 20) + (128 << 10)];
        let a = seeded(1, 4096);
        fs.test_write(fh, 0, &a).unwrap();
        model[..4096].copy_from_slice(&a);
        let far = seeded(2, 128 << 10);
        fs.test_write(fh, 1 << 20, &far).unwrap();
        model[1 << 20..].copy_from_slice(&far);
        assert!(fs.test_is_streaming(fh), "the far write waits in the reorder window");
        assert_eq!(fs.test_rebase_count(), 0);
        // The hook runs once, after the new root is finished and before it is
        // installed: it still sees the base of the switch in the manifest.
        let seen: Arc<Mutex<Vec<Option<ObjectKey>>>> = Arc::new(Mutex::new(Vec::new()));
        let (meta, log) = (Arc::clone(&fs.meta), Arc::clone(&seen));
        fs.test_set_rebase_hook(Some(Arc::new(move || {
            log.lock().unwrap().push(meta.get_manifest(ino).ok().and_then(|m| m.first().copied()));
        })));
        let b = seeded(3, 4096);
        fs.test_write(fh, 0, &b).unwrap();
        model[..4096].copy_from_slice(&b);
        assert!(!fs.test_is_streaming(fh));
        assert_eq!(fs.test_rebase_count(), 1, "the switch moved 128 KiB in: one rebase at once");
        // The splice cuts the two written ranges again; the zero gap between
        // them takes the zero shortcut (no read, no hash).
        let last = fs.test_last_rebase_bytes();
        assert!((128 << 10..256 << 10).contains(&last), "rebase re-chunked {last} bytes");
        fs.test_set_rebase_hook(None);
        let seen = seen.lock().unwrap().clone();
        let after = fs.meta.get_manifest(ino).unwrap().first().copied();
        assert_eq!(seen.len(), 1, "one hook call");
        assert!(seen[0].is_some() && seen[0] != after, "the hook ran before install_root");
        assert_eq!(overlay_cost(&fs, fh), 0);
        fs.test_release(ino, fh).unwrap();
        assert!(fs.test_read(ino, 0, (2 << 20) as u32).unwrap() == model, "content after release");
    }

    #[test]
    fn a_commit_relogs_an_overlay_in_pieces_of_at_most_1_mib() {
        let (fs, dir) = fs_with_log();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, &seeded(1, 4096)).unwrap();
        let big = seeded(2, 3 << 20);
        fs.test_write(fh, 0, &big).unwrap(); // a switch: one 3 MiB extent
        assert!(!fs.test_is_streaming(fh));
        let before = fs.test_rebase_count();
        fs.commit_cut().unwrap();
        assert_eq!(fs.test_last_relog_bytes(), 3 << 20);
        assert_eq!(fs.test_rebase_count(), before, "a commit never rebases");

        // The log after the commit: Write records of at most 1 MiB that cover the extent.
        let (_, _, ops) = metadata::segment::load_store_with_log(&dir.path().join("segments")).unwrap();
        let mut covered = 0u64;
        for raw in &ops {
            if let Some(crate::oplog::Record { op: crate::oplog::Op::Write { ino: i, offset, data }, .. }) = crate::oplog::Record::decode(raw) {
                assert_eq!(i, ino);
                assert!(data.len() as u64 <= RELOG_PIECE, "a relog piece of {} bytes", data.len());
                assert!(data[..] == big[offset as usize..offset as usize + data.len()]);
                covered += data.len() as u64;
            }
        }
        assert_eq!(covered, 3 << 20);
        fs.test_release(ino, fh).unwrap();
        assert!(fs.test_read(ino, 0, (4 << 20) as u32).unwrap() == big);
    }

    #[test]
    fn a_streaming_shrink_to_a_nonzero_size_switches_to_an_overlay() {
        let (fs, _dir) = fresh_fs();
        let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
        let old = seeded(1, 300_000);
        fs.test_write(fh, 0, &old).unwrap();
        fs.test_release(ino, fh).unwrap();
        let root = fs.meta.get_manifest(ino).unwrap()[0];
        assert_eq!(fs.meta.get_refcount(&root), 1);

        let (fh, _) = fs.test_open(ino, libc::O_RDWR).unwrap();
        assert!(fs.test_is_streaming(fh));
        let n = 100_000usize;
        fs.test_setattr_size(ino, Some(fh), n as u64).unwrap();
        assert!(!fs.test_is_streaming(fh), "a shrink of a stored file switches to an overlay");
        // The base is the stored root: it stays in the manifest with its count.
        assert_eq!(fs.meta.get_manifest(ino).unwrap(), vec![root]);
        assert_eq!(fs.meta.get_refcount(&root), 1);
        assert_eq!(fs.test_getattr(ino).unwrap().size, n as u64);
        let tail = seeded(2, 5000);
        fs.test_write(fh, 200_000, &tail).unwrap();
        fs.test_release(ino, fh).unwrap();

        let mut model = old[..n].to_vec();
        model.resize(200_000, 0);
        model.extend_from_slice(&tail);
        assert!(fs.test_read(ino, 0, 1 << 20).unwrap() == model, "first n bytes, zeros, the write");
        assert_eq!(fs.test_getattr(ino).unwrap().size, model.len() as u64);
        let new_root = fs.meta.get_manifest(ino).unwrap()[0];
        assert_eq!(fs.meta.get_refcount(&new_root), 1);
        assert_eq!(fs.meta.get_refcount(&root), 0, "the replaced root is released once");

        // A handle that streamed bytes: a shrink keeps the first n of them.
        let (ino2, fh2) = fs.test_create(1, "g", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh2, 0, &old).unwrap();
        fs.test_setattr_size(ino2, Some(fh2), n as u64).unwrap();
        assert!(!fs.test_is_streaming(fh2));
        fs.test_release(ino2, fh2).unwrap();
        assert!(fs.test_read(ino2, 0, 1 << 20).unwrap() == old[..n], "streamed bytes cut at n");
    }
}
