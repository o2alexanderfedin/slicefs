//! `SliceFsFilesystem` — FUSE adapter for SliceFS.
//!
//! Implements `fuser::Filesystem` using `DictMetadataStore` for inode/directory/manifest
//! lookups and the chunk store for file content reads.
//!
//! Write callbacks (create, write, release, setattr) are fully implemented in Phase 4.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chunk_store::lock_stats::Site;
use chunk_store::pack_io::SyncWait;
use chunk_store::sync::{self, Rank};
use crate::util::ts;

use slicefs_traits::key::ObjectKey;
use crate::content::{self, Content, Stream};
use crate::locks::{NamespaceLocks, NsGuard};
use crate::overlay::{next_gen, Overlay};
use fuser::{Errno, FileAttr, FileType, INodeNo};
use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use metadata::wal::Lsn;
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

/// The log callback of a namespace function (D-08): the function calls it
/// with the op of its record in the lock hold of its apply, once the apply
/// succeeded. A handler appends the record there (`log_append`) and writes
/// it out after the call, before the reply; a `test_*` wrapper passes
/// `&mut |_| {}`.
pub(crate) type LogOp<'a> = &'a mut dyn FnMut(crate::oplog::Op);

/// The `create_inode` of a create: at the logged inode number in replay
/// (`Some`), else the next free one.
fn new_inode(meta: &DictMetadataStore, at: Option<u64>, m: &InodeMeta) -> Result<u64, MetaError> {
    match at {
        Some(ino) => meta.create_inode_at(ino, m),
        None => meta.create_inode(m),
    }
}

/// The new content of a splice over an overlay: reads go through
/// `Overlay::view` (base bytes below `visible`, zeros, extents on top), so a
/// read holds only the requested range.
struct OverlaySource<'a> {
    cs: &'a chunk_store::ChunkStore,
    ov: &'a Overlay,
    reader: Option<content::Reader>,
    /// Base bytes asked so far (for tests).
    base_bytes: u64,
}

impl<'a> OverlaySource<'a> {
    fn new(cs: &'a chunk_store::ChunkStore, ov: &'a Overlay) -> std::io::Result<Self> {
        let reader = match &ov.base {
            Some(k) => Some(content::Reader::open(cs, k)?),
            None => None,
        };
        Ok(OverlaySource { cs, ov, reader, base_bytes: 0 })
    }
}

impl chunk_store::file::SpliceSource for OverlaySource<'_> {
    fn read(&mut self, offset: u64, len: usize) -> chunk_store::Result<Vec<u8>> {
        let (cs, reader, counted) = (self.cs, &mut self.reader, &mut self.base_bytes);
        self.ov.view(offset, len as u64).read(|o, n| {
            *counted += n as u64;
            match reader.as_mut() {
                Some(r) => r.read(cs, o, n),
                None => Ok(Vec::new()),
            }
        }).map_err(chunk_store::Error::Io)
    }
}

/// Largest data size of one `Write` record that a commit logs again (well
/// below the segment payload limit of 64 MiB). Each piece is copied, logged
/// and dropped before the next one is made.
const RELOG_PIECE: u64 = 1 << 20;

/// Dirty limit of one open file: when the RAM of its overlay (extent
/// capacity plus `EXTENT_OVERHEAD` per extent) passes it, the write that
/// passed it stores a part of the overlay as a new root (a rebase step of
/// about `REBASE_STEP` bytes, #108) before it returns. So an overlay that
/// stays busy holds about the limit; release stores the rest.
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

/// Bytes of overlay cost one rebase step frees: this, or the cost its write
/// added if that is more (#108). A write past the dirty limit rebases only this much,
/// so no single write pays for the rebase of the whole overlay.
pub const REBASE_STEP: usize = 1 << 20;

/// Most overlay cost one step of the last release stores (D-03, Claude's
/// discretion): the release goes in steps of this size and never copies the
/// whole overlay at once (#98).
pub const FINAL_STEP: usize = 8 << 20;

/// Budget of dirty bytes of all open files together (#105): when a write
/// makes the sum pass it, the file with the most dirty bytes stores a step
/// (an overlay rebase step; a stream's pending writes first become an
/// overlay). Each file also keeps its own `DIRTY_LIMIT`. The same value as
/// `REORDER_WINDOW`, so one stream can still use its whole window.
pub const DIRTY_BUDGET: usize = 64 << 20;

/// Most time a writer waits for the step in flight of its cell (with the
/// cell unlocked), then EIO. Above the 10 s `STORE_WAIT` of the chunk store,
/// because the step can itself wait that long in a store put.
const STEP_WAIT: Duration = Duration::from_secs(15);

/// Most dropped steps in a row one write tries again (with the I/O
/// unlocked), then EIO.
const STEP_TRIES: usize = 16;

/// Unlocked attempts of the rebase step of a write over the dirty limit. If
/// all are dropped the write returns (its bytes are in the overlay, under
/// the hard cap); the next write, the release or a budget step tries again
/// (D-02 "retries later").
const REBASE_TRIES: usize = 2;

/// Forced drops in a row (a test hook) after which a finalize stops
/// consulting the drop hook for the rest of that finalize, so a test
/// release ends too. With no write handle nothing else drops a finalize
/// step (see `finalize_steps`).
const FINAL_TRIES: usize = 8;

/// The RAM an open file holds in written bytes that are not in a stored
/// root: the overlay cost, or the pending out-of-order writes of a stream.
fn dirty_cost(s: &OpenFileState) -> usize {
    match &s.write_mode {
        WriteMode::Overlay(ov) => ov.cost(),
        WriteMode::Streaming { pending, .. } => pending.values().map(Vec::len).sum(),
    }
}

thread_local! {
    /// Bytes the last splice of this thread cut again (for tests; D-10:
    /// per thread, so writers on other threads do not change it).
    static LAST_REBASE_BYTES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// Bytes the rebase steps of this thread's last write cut again (for tests).
    static WRITE_REBASE_BYTES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// Base bytes the rebase steps of this thread's last write read (for tests).
    static WRITE_BASE_BYTES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Add `n` to a thread-local counter.
fn add_to(counter: &'static std::thread::LocalKey<std::cell::Cell<u64>>, n: u64) {
    counter.with(|c| c.set(c.get() + n));
}

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

/// Open write state of one inode (in its `FileCell`, shared by all its
/// write handles).
///
/// Created at the first `open()` with `O_WRONLY` or `O_RDWR`.
/// Ended at the last write-handle `release()`. Writes are streamed incrementally into FastCDC
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
    /// Generation of the stream (D-02): a new value (from the process-wide
    /// counter of `overlay::next_gen`) at each change of the stream, its
    /// pending writes, `min_size` or `byte_count`, and at the switch to an
    /// overlay. A truncate to 0 makes a new state, so it gets a new value
    /// too. A stream step publishes only if it is unchanged. (An overlay
    /// keeps its own generation.)
    generation: u64,
}

impl OpenFileState {
    /// A fresh stream: nothing written yet.
    fn new(ino: u64) -> Self {
        OpenFileState {
            ino,
            write_mode: WriteMode::Streaming { state: Stream::default(), next_expected_offset: 0, pending: BTreeMap::new() },
            byte_count: 0,
            cas_committed: false,
            last_committed_root: None,
            min_size: 0,
            modified: None,
            generation: next_gen(),
        }
    }
}

/// Phase A of a rebase step (see `SliceFsFilesystem::rebase_unlocked`).
struct StepPlan {
    /// The overlay generation the step starts from.
    generation: u64,
    /// Logical size of the overlay (and of the new root).
    size: u64,
    /// The stored pieces, as `(start, len)` heads of extents.
    pieces: Vec<(u64, usize)>,
    free: usize,
    cost_before: usize,
}

/// How `SliceFsFilesystem::flush_cell` runs its I/O phase.
#[derive(Clone, Copy)]
enum FlushIo {
    /// In the cell hold: only where no request runs (the commit cut under
    /// the exclusive gate, destroy).
    Locked,
    /// With the cell unlocked and no step token (fsync, flush).
    Unlocked,
    /// With the cell unlocked as the cell's step (`CellGuard::run_step`):
    /// the last release. The caller makes sure no step is in flight.
    Step,
}

/// The fields of the debug line of a switch to an overlay: made in the
/// write's cell hold, printed by `write_logged` after it.
struct SwitchNote {
    ino: u64,
    offset: u64,
    expected: u64,
    pending: usize,
    fresh: bool,
    overlap: bool,
}

/// The open write state of one inode, shared by every handle of it (D-03).
/// Requests lock it in place; no request takes the state out.
struct FileCell {
    ino: u64,
    state: sync::Mutex<CellState>,
    /// `dirty_cost` of the state when its guard last synced (see
    /// `CellGuard`); `SliceFsFilesystem::dirty_total` is the sum.
    dirty: AtomicUsize,
}

impl FileCell {
    fn new(ino: u64) -> Self {
        let state = CellState { file: OpenFileState::new(ino), write_handles: 0, retired: false, stepping: None, last_published_gen: 0 };
        FileCell {
            ino,
            state: sync::Mutex::new_keyed(Rank::Cell, ino, state).timed(Site::FileCell),
            dirty: AtomicUsize::new(0),
        }
    }
}

struct CellState {
    file: OpenFileState,
    /// Open write handles of the inode.
    write_handles: usize,
    /// Set at the last write-handle release, under the lock and before the
    /// cell leaves the map. A holder of an old `Arc` that finds it set must
    /// look the inode up again (or, if it only reads, use the manifest).
    retired: bool,
    /// The step token: set while one I/O step of this cell (a stream push,
    /// a gap fill, the first-write content check, the switch to an overlay,
    /// a rebase step) runs with the cell unlocked (D-02); at most one at a
    /// time. Stream writers wait for it with the cell unlocked
    /// (`CellGuard::wait_step`); a writer over the limit skips its rebase
    /// step. Readers and flushes never take it or wait for it.
    stepping: Option<SyncWait>,
    /// The stream generation of the last flush snapshot that was installed,
    /// or a newer value after another install of the manifest (a switch to
    /// an overlay, a truncate to 0): a flush of an older snapshot is not
    /// installed (RESEARCH Pitfall 5).
    last_published_gen: u64,
}

/// A locked cell. When it is dropped (and at `sync_dirty`), it moves
/// `FileCell::dirty` and the global dirty total by the change of the cell's
/// `dirty_cost`, with the cell lock still held.
struct CellGuard<'a> {
    total: &'a AtomicUsize,
    cell: &'a FileCell,
    /// `None` only inside `unlocked`.
    state: Option<sync::MutexGuard<'a, CellState>>,
    /// Debug lines made in the hold; printed when the guard is dropped,
    /// after the cell lock is released (a stderr write can stall).
    notes: Vec<String>,
    /// Set by a finalize after `FINAL_TRIES` forced drops in a row: the
    /// steps of this guard no longer consult the drop hook.
    no_drop_hook: bool,
}

impl CellGuard<'_> {
    /// Run `f` with the cell unlocked (the dirty cost is synced first), then
    /// lock it again. The state can change meanwhile, and the cell can be
    /// retired: the caller checks again.
    fn unlocked<R>(&mut self, f: impl FnOnce() -> R) -> R {
        self.sync_dirty();
        self.state = None;
        let r = f();
        self.state = Some(self.cell.state.lock());
        r
    }

    /// Run `io` as the one step in flight of the cell: set a fresh token,
    /// run `io` with the cell unlocked, lock again, then clear the token and
    /// wake its waiters. `io` returns its error as a value, so the token is
    /// cleared on every path. The caller makes sure no token is set, and
    /// checks `retired`, the mode and the generation after it.
    fn run_step<R>(&mut self, io: impl FnOnce() -> R) -> R {
        debug_assert!(self.stepping.is_none(), "one step in flight per cell");
        let token = SyncWait::default();
        self.stepping = Some(token.clone());
        let r = self.unlocked(io);
        self.stepping = None;
        token.done();
        r
    }

    /// Wait, with the cell unlocked, until no step of the cell is in flight
    /// (`STEP_WAIT` for each step, then EIO). The caller checks `retired`
    /// and the mode again after it.
    fn wait_step(&mut self) -> Result<(), i32> {
        while let Some(token) = self.stepping.clone() {
            if !self.unlocked(|| token.wait_timeout(STEP_WAIT)) {
                return Err(libc::EIO);
            }
        }
        Ok(())
    }

    fn sync_dirty(&self) {
        let Some(state) = &self.state else { return };
        let cost = if state.retired { 0 } else { dirty_cost(&state.file) };
        let old = self.cell.dirty.swap(cost, Ordering::Relaxed);
        if cost >= old {
            self.total.fetch_add(cost - old, Ordering::Relaxed);
        } else {
            self.total.fetch_sub(old - cost, Ordering::Relaxed);
        }
    }
}

impl Deref for CellGuard<'_> {
    type Target = CellState;
    fn deref(&self) -> &CellState {
        self.state.as_ref().expect("cell locked")
    }
}

impl DerefMut for CellGuard<'_> {
    fn deref_mut(&mut self) -> &mut CellState {
        self.state.as_mut().expect("cell locked")
    }
}

impl Drop for CellGuard<'_> {
    fn drop(&mut self) {
        self.sync_dirty();
        self.state = None;
        for line in self.notes.drain(..) {
            eprintln!("{line}");
        }
    }
}

/// Shards of the cell map (Claude's discretion).
const CELL_SHARDS: usize = 16;

/// `ino -> Arc<FileCell>` in `CELL_SHARDS` shards. A shard lock is held only
/// to find, add or remove an `Arc`, never while a cell is locked by the
/// same caller, except to remove a retired cell (cell rank, then shard rank).
struct CellMap([sync::Mutex<HashMap<u64, Arc<FileCell>>>; CELL_SHARDS]);

impl CellMap {
    fn new() -> Self {
        CellMap(std::array::from_fn(|_| sync::Mutex::new(Rank::Short, HashMap::new()).timed(Site::CellMap)))
    }

    fn shard(&self, ino: u64) -> &sync::Mutex<HashMap<u64, Arc<FileCell>>> {
        &self.0[(ino % CELL_SHARDS as u64) as usize]
    }

    fn get(&self, ino: u64) -> Option<Arc<FileCell>> {
        self.shard(ino).lock().get(&ino).cloned()
    }

    fn get_or_insert(&self, ino: u64) -> Arc<FileCell> {
        Arc::clone(self.shard(ino).lock().entry(ino).or_insert_with(|| Arc::new(FileCell::new(ino))))
    }

    /// Add `cell` if the map has no cell of its inode; false if it has one.
    fn insert_new(&self, cell: &Arc<FileCell>) -> bool {
        let mut m = self.shard(cell.ino).lock();
        if m.contains_key(&cell.ino) {
            return false;
        }
        m.insert(cell.ino, Arc::clone(cell));
        true
    }

    /// Remove `cell` (only if the map still holds this very cell).
    fn remove(&self, cell: &Arc<FileCell>) {
        let mut m = self.shard(cell.ino).lock();
        if m.get(&cell.ino).is_some_and(|c| Arc::ptr_eq(c, cell)) {
            m.remove(&cell.ino);
        }
    }

    /// Every cell, collected shard by shard (one shard lock at a time).
    fn all(&self) -> Vec<Arc<FileCell>> {
        self.0.iter().flat_map(|sh| sh.lock().values().cloned().collect::<Vec<_>>()).collect()
    }
}

/// An open file handle: its inode and its mode.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Handle {
    pub(crate) ino: u64,
    /// Opened for writing (`O_WRONLY` or `O_RDWR`).
    pub(crate) write: bool,
    /// Opened (or created) with `O_APPEND`: each write goes to the end of
    /// file taken under the cell lock (D-12).
    pub(crate) append: bool,
}

/// Every open handle (read-only too), with the open count of each inode, so
/// the "is it open" check at unlink and close is O(1).
#[derive(Default)]
struct HandleTable {
    by_fh: HashMap<u64, Handle>,
    /// Open handles per inode (no entry: none).
    open_count: HashMap<u64, u32>,
    /// Inodes whose last link was removed while handles were still open.
    orphans: HashSet<u64>,
}

/// FUSE filesystem adapter backed by a [`DictMetadataStore`].
///
/// `meta` provides all inode/directory/manifest metadata.
/// `io` is the shared `StoreIo` used for file content reads/writes via
/// `get_node` / `put_node`. It is the same `Arc<Mutex<StoreIo>>`
/// held by `meta`, accessed via `meta.io().clone()`.
///
/// `cells` holds the open write state of each inode; `next_fh` allocates unique handles.
/// `store_path` is the on-disk store root used by `destroy()` to persist state.
///
/// Raw bytes flow directly into the Merkle tree (v3 store format — no compression header).
/// ObjectKey is computed on raw bytes, enabling content-addressed deduplication.
/// A commit after its generation switch (`SliceFsFilesystem::commit_cut_begin`),
/// to be finished with the commit gate released (#108).
pub struct CommitCut<'a> {
    prepared: metadata::store::PreparedCommit<'a>,
}

pub struct SliceFsFilesystem {
    pub(crate) meta: Arc<DictMetadataStore>,
    pub(crate) io: Arc<Mutex<StoreIo>>,
    /// One cell per inode with open write handles; every handle of the
    /// inode uses it (POSIX: every handle writes the same file).
    cells: CellMap,
    /// Sum of `FileCell::dirty` of all cells (see `keep_budget`).
    dirty_total: AtomicUsize,
    /// File content store (`<store>/packs/`).
    pub(crate) content: Content,
    /// A content reader per inode (keeps its node and chunk caches), each
    /// behind its own lock; the map lock is held only to find or add one. A
    /// reader whose key no longer matches the file content is replaced on use.
    readers: sync::Mutex<HashMap<u64, Arc<sync::Mutex<content::Reader>>>>,
    /// Every open handle (read-only too), the open count of each inode and
    /// the orphans: keeps an unlinked inode alive until its last handle closes.
    handles: sync::Mutex<HandleTable>,
    /// Namespace stripes and the rename mutex (D-04): the namespace changes
    /// of one inode, its open and last-close decisions, setattr and xattr.
    ns: NamespaceLocks,
    next_fh: AtomicU64,
    pub(crate) store_path: Option<PathBuf>,
    pub(crate) auto_snapshot: bool,
    /// Shared by FUSE callbacks, exclusive for a timed commit (see `commit_gate()`).
    commit_gate: Arc<RwLock<()>>,
    /// True while the write log is replayed: requests are not logged again.
    replaying: std::sync::atomic::AtomicBool,
    /// Dirty limit of one overlay (`DIRTY_LIMIT`; tests set it lower).
    dirty_limit: AtomicUsize,
    /// Dirty budget of all open files (`DIRTY_BUDGET`; tests set it lower).
    dirty_budget: AtomicUsize,
    /// Step of the last release (`FINAL_STEP`; tests set it lower).
    final_step: AtomicUsize,
    /// Rebases done so far (for tests).
    rebase_count: AtomicU64,
    /// `Write` payload bytes the last commit logged again (for tests).
    last_relog_bytes: AtomicU64,
    /// Run inside each rebase, after the new root is finished and before it
    /// is installed, with the cell unlocked unless the step holds it (for
    /// tests: online GC during a rebase, a parked rebase). A size change of
    /// a closed file runs it after its splice, with its transient cell
    /// locked.
    rebase_hook: sync::Mutex<Option<RebaseHook>>,
    /// Run inside each read through the per-inode reader, with it locked
    /// (for tests: a reader parked in the middle of a read).
    read_hook: sync::Mutex<Option<RebaseHook>>,
    /// Run in the I/O phase of each stream step (a push, a settle, the end
    /// of a flush snapshot), with the cell unlocked (for tests: a parked
    /// streaming write or fsync).
    push_hook: sync::Mutex<Option<RebaseHook>>,
    /// Run in each mkdir after the directory is linked and before its
    /// `Mkdir` record is appended, with its stripes held (for tests: a
    /// parked mkdir).
    mkdir_hook: sync::Mutex<Option<RebaseHook>>,
    /// Write requests so far (write, create, setattr): the online GC gives
    /// way to writes by it (`gc_pacing`).
    write_activity: Arc<AtomicU64>,
    /// Test hooks (see `test_drop_steps`, `test_drop_every`): results of
    /// unlocked steps to drop, and the step count they use.
    drop_steps: AtomicUsize,
    drop_every: AtomicUsize,
    steps_seen: AtomicUsize,
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
            cells: CellMap::new(),
            dirty_total: AtomicUsize::new(0),
            readers: sync::Mutex::new(Rank::Short, HashMap::new()).timed(Site::Readers),
            handles: sync::Mutex::new(Rank::Short, HandleTable::default()).timed(Site::Handles),
            ns: NamespaceLocks::new(),
            next_fh: AtomicU64::new(0),
            store_path,
            auto_snapshot: false,
            commit_gate: Arc::new(RwLock::new(())),
            replaying: std::sync::atomic::AtomicBool::new(false),
            dirty_limit: AtomicUsize::new(DIRTY_LIMIT),
            dirty_budget: AtomicUsize::new(DIRTY_BUDGET),
            final_step: AtomicUsize::new(FINAL_STEP),
            rebase_count: AtomicU64::new(0),
            last_relog_bytes: AtomicU64::new(0),
            rebase_hook: sync::Mutex::new(Rank::Short, None),
            read_hook: sync::Mutex::new(Rank::Short, None),
            push_hook: sync::Mutex::new(Rank::Short, None),
            mkdir_hook: sync::Mutex::new(Rank::Short, None),
            write_activity: Arc::new(AtomicU64::new(0)),
            drop_steps: AtomicUsize::new(0),
            drop_every: AtomicUsize::new(0),
            steps_seen: AtomicUsize::new(0),
        }
    }

    /// Count a write request (for the GC pacing).
    pub fn note_write(&self) {
        self.write_activity.fetch_add(1, Ordering::Relaxed);
    }

    /// The counter of write requests (see `note_write`).
    pub fn write_activity(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.write_activity)
    }

    /// Test hook: set the dirty limit of each overlay to `bytes`.
    pub fn test_set_dirty_limit(&self, bytes: usize) {
        self.dirty_limit.store(bytes, Ordering::Relaxed);
    }

    /// Test hook: set the dirty budget of all open files to `bytes`.
    pub fn test_set_dirty_budget(&self, bytes: usize) {
        self.dirty_budget.store(bytes, Ordering::Relaxed);
    }

    /// Test hook: set the step of the last release to `bytes`.
    pub fn test_set_final_step(&self, bytes: usize) {
        self.final_step.store(bytes, Ordering::Relaxed);
    }

    /// Test hook: the number of rebases so far.
    pub fn test_rebase_count(&self) -> u64 {
        self.rebase_count.load(Ordering::Relaxed)
    }

    /// Test hook: the bytes the last splice of this thread cut again with FastCDC (a rebase,
    /// or a size change with no open handle): `SpliceStats::rechunked`.
    pub fn test_last_rebase_bytes(&self) -> u64 {
        LAST_REBASE_BYTES.with(std::cell::Cell::get)
    }

    /// Test hook: the bytes that the rebase work of this thread's last write cut again
    /// with FastCDC (0 if it ran no rebase).
    pub fn test_last_write_rebase_bytes(&self) -> u64 {
        WRITE_REBASE_BYTES.with(std::cell::Cell::get)
    }

    /// Test hook: the base bytes that the rebase work of this thread's last write read.
    pub fn test_last_write_base_bytes(&self) -> u64 {
        WRITE_BASE_BYTES.with(std::cell::Cell::get)
    }

    /// Test hook: the dirty bytes of all open files (overlay costs and the
    /// pending out-of-order writes of streams).
    pub fn test_dirty_total(&self) -> usize {
        self.dirty_total.load(Ordering::Relaxed)
    }

    /// Test hook: the dirty cost of `ino` as its cell last synced it (0 if
    /// it has no open write state).
    pub fn test_dirty_cost(&self, ino: u64) -> usize {
        self.cells.get(ino).map_or(0, |c| c.dirty.load(Ordering::Relaxed))
    }

    /// Test hook: the `Write` payload bytes the last commit logged again.
    pub fn test_last_relog_bytes(&self) -> u64 {
        self.last_relog_bytes.load(Ordering::Relaxed)
    }

    /// Test hook: run `hook` inside each rebase, after the new root is
    /// finished and before it is installed, and after the splice of a size
    /// change of a closed file (`None`: no hook).
    pub fn test_set_rebase_hook(&self, hook: Option<Arc<dyn Fn() + Send + Sync>>) {
        *self.rebase_hook.lock() = hook;
    }

    /// Test hook: run `hook` inside each read through the per-inode reader,
    /// with that reader locked (`None`: no hook).
    pub fn test_set_read_hook(&self, hook: Option<Arc<dyn Fn() + Send + Sync>>) {
        *self.read_hook.lock() = hook;
    }

    /// Test hook: run `hook` in the I/O phase of each stream step (a push,
    /// a settle of pending writes, the end of a flush snapshot), with the
    /// cell unlocked (`None`: no hook).
    pub fn test_set_push_hook(&self, hook: Option<RebaseHook>) {
        *self.push_hook.lock() = hook;
    }

    /// Test hook: run `hook` in each mkdir after the directory is linked and
    /// before its record is appended, with its stripes held (`None`: no hook).
    pub fn test_set_mkdir_hook(&self, hook: Option<RebaseHook>) {
        *self.mkdir_hook.lock() = hook;
    }

    /// Test hook: drop the results of the next `n` unlocked steps (stream
    /// steps, overlay switches, rebase steps) as if the file had changed in
    /// their I/O phase. Flushes are not dropped.
    pub fn test_drop_steps(&self, n: usize) {
        self.drop_steps.store(n, Ordering::Relaxed);
    }

    /// Test hook: drop the result of every `k`-th unlocked step (0: off).
    pub fn test_drop_every(&self, k: usize) {
        self.steps_seen.store(0, Ordering::Relaxed);
        self.drop_every.store(k, Ordering::Relaxed);
    }

    /// Phase C of an unlocked step: true if a test hook drops its result.
    /// With `hook_off` (a finalize after `FINAL_TRIES` drops) the hook is
    /// not consulted.
    fn forced_drop(&self, hook_off: bool) -> bool {
        if hook_off {
            return false;
        }
        let k = self.drop_every.load(Ordering::Relaxed);
        let n = self.steps_seen.fetch_add(1, Ordering::Relaxed) + 1;
        if k > 0 && n % k == 0 {
            return true;
        }
        self.drop_steps.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |d| d.checked_sub(1)).is_ok()
    }

    /// Run the push hook, if one is set.
    fn run_push_hook(&self) {
        let hook = self.push_hook.lock().clone();
        if let Some(hook) = hook {
            hook();
        }
    }

    /// Enable auto-snapshot on clean unmount.
    pub fn set_auto_snapshot(&mut self, enabled: bool) {
        self.auto_snapshot = enabled;
    }

    /// Every FUSE callback holds this gate shared; the commit timer holds it
    /// exclusively, so a timed commit never sees half of an operation.
    /// Append a request to the write log (#20), unless the log is being replayed.
    pub(crate) fn log_record(&self, rec: crate::oplog::Record) {
        let lsn = self.log_append(rec);
        self.log_write_out(lsn);
    }

    /// First half of `log_record` (D-08): encode the record and put it in
    /// the write log's memory queue. A request calls it under the locks of
    /// its apply. `None` while the log is replayed, with no write log, or
    /// when the append failed.
    pub(crate) fn log_append(&self, rec: crate::oplog::Record) -> Option<Lsn> {
        if self.replaying.load(Ordering::Relaxed) {
            return None;
        }
        self.meta.log_append(rec.encode())
    }

    /// Second half of `log_record`: write the records up to `lsn` out. A
    /// request calls it after its apply locks are released and before it
    /// replies (still inside the shared commit gate).
    pub(crate) fn log_write_out(&self, lsn: Option<Lsn>) {
        self.meta.log_write_out(lsn);
    }

    /// Turn off logging until the returned guard is dropped (replay).
    pub(crate) fn replay_guard(&self) -> ReplayGuard<'_> {
        self.replaying.store(true, Ordering::Relaxed);
        ReplayGuard(&self.replaying)
    }

    /// The inode of an open write handle (`None` for a read-only or unknown
    /// handle).
    pub(crate) fn ino_of_fh(&self, fh: u64) -> Option<u64> {
        self.handle(fh).filter(|h| h.write).map(|h| h.ino)
    }

    /// The open handle `fh`, if any.
    pub(crate) fn handle(&self, fh: u64) -> Option<Handle> {
        self.handles.lock().by_fh.get(&fh).copied()
    }

    fn lock_cell<'a>(&'a self, cell: &'a FileCell) -> CellGuard<'a> {
        CellGuard { total: &self.dirty_total, cell, state: Some(cell.state.lock()), notes: Vec::new(), no_drop_hook: false }
    }

    /// Run `f` on the open write state of `ino` under its cell lock; `None`
    /// if `ino` has none (no cell, or a retired one).
    fn with_cell<R>(&self, ino: u64, f: impl FnOnce(&mut OpenFileState) -> R) -> Option<R> {
        let cell = self.cells.get(ino)?;
        let mut g = self.lock_cell(&cell);
        if g.retired {
            return None;
        }
        Some(f(&mut g.file))
    }

    /// Count a new write handle of `ino` in its cell (made if needed). With
    /// `trunc`, the file is truncated to 0 under the same cell lock, and
    /// with `time` its `Truncate` record is appended in that hold (D-08).
    fn open_write(&self, ino: u64, trunc: bool, time: Option<(i64, u32)>) -> Result<Option<Lsn>, i32> {
        loop {
            let cell = self.cells.get_or_insert(ino);
            let mut g = self.lock_cell(&cell);
            if g.retired {
                // The last release retired it before we locked it: it has
                // left the map, so the next lookup gives a live cell.
                continue;
            }
            g.write_handles += 1;
            if !trunc {
                return Ok(None);
            }
            if let Err(e) = self.setattr_size_open(&mut g, 0) {
                let _ = self.leave(&cell, &mut g);
                return Err(e);
            }
            let rec = |time| crate::oplog::Record { time, op: crate::oplog::Op::Truncate { ino: cell.ino } };
            return Ok(time.and_then(|t| self.log_append(rec(t))));
        }
    }

    /// One write handle of the cell ends. The last one finalizes the content
    /// in steps and then retires the cell (see `finalize`).
    fn leave(&self, cell: &Arc<FileCell>, g: &mut CellGuard<'_>) -> Result<(), i32> {
        g.write_handles = g.write_handles.saturating_sub(1);
        if g.write_handles > 0 {
            return Ok(());
        }
        self.finalize(cell, g)
    }

    /// A write handle of `ino` ends (see `leave`). With `time`, a release
    /// that finalized and retired the cell appends its `Release` record in
    /// that last hold (D-08): not the release of a write handle that was
    /// not the last, nor one whose finalize a new writer stopped.
    fn release_write(&self, ino: u64, time: Option<(i64, u32)>) -> (Result<(), i32>, Option<Lsn>) {
        let Some(cell) = self.cells.get(ino) else { return (Ok(()), None) };
        let mut g = self.lock_cell(&cell);
        if g.retired {
            return (Ok(()), None);
        }
        let r = self.leave(&cell, &mut g);
        let rec = |time| crate::oplog::Record { time, op: crate::oplog::Op::Release { ino: cell.ino } };
        let lsn = if g.retired { time.and_then(|t| self.log_append(rec(t))) } else { None };
        (r, lsn)
    }

    /// The last write handle of the cell `g` is gone (D-03): store its
    /// content in bounded steps by the protocol of D-02, each with the I/O
    /// unlocked under the step token (pending writes one stream step each,
    /// an overlay in rebase steps of at most `final_step` bytes, a stream
    /// by a flush; see `finalize_steps`). When the content is clean, retire
    /// the cell and remove it from the map in the same cell-lock hold (cell
    /// rank, then shard rank; RESEARCH Pitfall 4). If a write handle is
    /// opened meanwhile, stop: the cell lives on, and its next last release
    /// finalizes. After an error the cell is retired too, unless a write
    /// handle is open again.
    fn finalize(&self, cell: &Arc<FileCell>, g: &mut CellGuard<'_>) -> Result<(), i32> {
        let r = self.finalize_steps(g);
        g.no_drop_hook = false;
        if g.retired || g.write_handles > 0 {
            return r;
        }
        g.retired = true;
        self.cells.remove(cell);
        r
    }

    /// The steps of `finalize`; returns when the content is clean, the cell
    /// is retired, or a write handle is open again. Each pass runs one step
    /// with the I/O unlocked under the step token (`CellGuard::run_step`),
    /// never one with the cell locked:
    /// - pending writes: one stream step;
    /// - a dirty overlay: one rebase step of at most `final_step`;
    /// - a dirty stream: the flush (`FlushIo::Step`), so no size change
    ///   makes its snapshot stale; if a newer snapshot came first, the next
    ///   pass sees the result;
    /// - nothing written: the empty-file rule (`empty_rule`).
    ///
    /// A finalize needs no give-up state. With no write handle only a
    /// forced drop (a test hook) can drop a step: a size change waits for
    /// the token first, and the flush runs under it. After `FINAL_TRIES`
    /// forced drops in a row this finalize stops consulting the hook.
    fn finalize_steps(&self, g: &mut CellGuard<'_>) -> Result<(), i32> {
        enum Next { Settle, Rebase, Flush, Empty, Clean }
        let step = self.final_step.load(Ordering::Relaxed).max(1);
        // Dropped steps in a row.
        let mut drops = 0usize;
        loop {
            if g.retired || g.write_handles > 0 {
                return Ok(());
            }
            if g.stepping.is_some() {
                // A step of a write that is still in flight, or of a size
                // change: wait for it with the cell unlocked, then look again.
                g.wait_step()?;
                continue;
            }
            if drops >= FINAL_TRIES {
                g.no_drop_hook = true;
            }
            let s = &g.file;
            let next = match &s.write_mode {
                WriteMode::Streaming { pending, .. } if !pending.is_empty() => Next::Settle,
                WriteMode::Overlay(ov) if !ov.is_clean() => Next::Rebase,
                WriteMode::Overlay(_) => Next::Clean,
                WriteMode::Streaming { .. } if s.byte_count == 0 => Next::Empty,
                WriteMode::Streaming { .. } if s.cas_committed && s.last_committed_root.is_some() => Next::Clean,
                WriteMode::Streaming { .. } => Next::Flush,
            };
            let published = match next {
                Next::Settle => {
                    let WriteMode::Streaming { pending, .. } = &g.file.write_mode else { continue };
                    let Some((&o, v)) = pending.first_key_value() else { continue };
                    let v = v.clone();
                    self.stream_step(g, o, &v, Some(o))?
                }
                Next::Rebase => self.rebase_unlocked(g, step)?,
                Next::Flush => {
                    // Not installed: an fsync installed a newer snapshot
                    // first; the next pass sees the stream clean or dirty.
                    self.flush_cell(g, FlushIo::Step)?;
                    true
                }
                Next::Empty => {
                    if self.empty_rule(g)? {
                        return Ok(());
                    }
                    // The state changed in the step: look again.
                    continue;
                }
                Next::Clean => return Ok(()),
            };
            if published {
                drops = 0;
            } else {
                drops += 1;
            }
        }
    }

    /// The empty-file rule of `finalize`: no bytes were written through the
    /// cell `g`. Only set an empty manifest if this is a genuinely new file
    /// (no existing manifest): FUSE-T on macOS may open existing files with
    /// O_RDWR even for read-only access (issue #15). The manifest read and
    /// the store put of the empty manifest node run as the cell's step with
    /// the cell unlocked; the hold only installs the digest, and only if the
    /// state did not change (the manifest of an open inode changes only
    /// under its cell lock, with a new generation or a new
    /// `last_committed_root`). `Ok(false)`: it changed, the caller looks
    /// again. The caller makes sure no step is in flight.
    fn empty_rule(&self, g: &mut CellGuard<'_>) -> Result<bool, i32> {
        if g.file.cas_committed {
            return Ok(true);
        }
        let (ino, generation, committed) = (g.file.ino, g.file.generation, g.file.last_committed_root);
        let digest = g.run_step(|| -> Result<Option<ObjectKey>, i32> {
            if self.meta.get_manifest(ino).is_ok_and(|m| !m.is_empty()) {
                return Ok(None);
            }
            self.meta.prepare_manifest(&[]).map(Some).map_err(|_| libc::EIO)
        })?;
        if g.retired {
            return Ok(true);
        }
        if g.file.generation != generation || g.file.last_committed_root != committed {
            return Ok(false);
        }
        if let Some(d) = digest {
            self.meta.set_manifest_digest(ino, d).map_err(|_| libc::EIO)?;
        }
        Ok(true)
    }

    /// `finalize` for `test_destroy` only: no request runs at destroy, so
    /// the steps keep the cell locked (the settle, rebase steps of at most
    /// `final_step`, a locked flush, the empty-file rule), and the cell is
    /// always retired. Never on a request path.
    fn finalize_locked(&self, cell: &Arc<FileCell>, g: &mut CellGuard<'_>) -> Result<(), i32> {
        let r = self.destroy_steps(g);
        g.retired = true;
        self.cells.remove(cell);
        r
    }

    /// The steps of `finalize_locked`, all in the cell hold.
    fn destroy_steps(&self, g: &mut CellGuard<'_>) -> Result<(), i32> {
        let (ino, step) = (g.file.ino, self.final_step.load(Ordering::Relaxed).max(1));
        self.settle_locked(g)?;
        while matches!(&g.file.write_mode, WriteMode::Overlay(ov) if !ov.is_clean()) {
            if let Some(line) = self.rebase_step(ino, &mut g.file, step)? {
                g.notes.push(line);
            }
        }
        if matches!(g.file.write_mode, WriteMode::Streaming { .. }) && g.file.byte_count == 0 {
            // The empty-file rule (see `empty_rule`), in the hold.
            if !g.file.cas_committed && !self.meta.get_manifest(ino).is_ok_and(|m| !m.is_empty()) {
                self.meta.set_manifest(ino, &[]).map_err(|_| libc::EIO)?;
            }
            return Ok(());
        }
        self.flush_cell(g, FlushIo::Locked).map(|_| ())
    }

    pub fn commit_gate(&self) -> Arc<RwLock<()>> {
        Arc::clone(&self.commit_gate)
    }

    /// Hold the commit gate shared (one request) with a `Rank::Gate` entry
    /// on this thread's rank stack for the same span (D-16). The entry goes
    /// on first, so a second gate hold on one thread panics in a debug build
    /// instead of waiting behind a queued commit. Every request and the
    /// harness take the gate through this, never `commit_gate().read()`.
    pub fn gate_shared(&self) -> GateGuard<'_> {
        let rank = sync::rank::enter(Rank::Gate);
        let hold = GateHold::Shared { _g: self.commit_gate.read().unwrap_or_else(|e| e.into_inner()) };
        GateGuard { _hold: hold, _rank: rank }
    }

    /// Hold the commit gate exclusively (a commit, the start of a GC), with
    /// its `Rank::Gate` entry (see `gate_shared`).
    pub fn gate_exclusive(&self) -> GateGuard<'_> {
        let rank = sync::rank::enter(Rank::Gate);
        let hold = GateHold::Exclusive { _g: self.commit_gate.write().unwrap_or_else(|e| e.into_inner()) };
        GateGuard { _hold: hold, _rank: rank }
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

/// The std commit gate held, shared or exclusive (each guard is kept only
/// for its drop).
enum GateHold<'a> {
    Shared { _g: std::sync::RwLockReadGuard<'a, ()> },
    Exclusive { _g: std::sync::RwLockWriteGuard<'a, ()> },
}

/// A hold of the commit gate and its rank-1 entry (D-16, D-18: the gate
/// stays a std `RwLock`). The gate is released first, then the entry.
pub struct GateGuard<'a> {
    _hold: GateHold<'a>,
    _rank: sync::rank::Marker,
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

    /// Allocate a file handle and record it as open on `ino` (for writing
    /// if `write`, at the end of file if `append`).
    fn open_handle(&self, ino: u64, write: bool, append: bool) -> u64 {
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed) + 1;
        let mut t = self.handles.lock();
        t.by_fh.insert(fh, Handle { ino, write, append });
        *t.open_count.entry(ino).or_insert(0) += 1;
        fh
    }

    /// Forget handle `fh`. If it was the last handle of an orphaned inode,
    /// reclaim the inode. A handle that is not open is ignored. The caller
    /// holds `stripe(ino)` of the handle's inode.
    fn close_handle_held(&self, fh: u64) {
        if let Some(h) = self.take_handle(fh) {
            self.uncount_held(h.ino);
        }
    }

    /// Remove `fh` from the handle table (its inode stays counted as open
    /// until `uncount`). Exactly one caller gets a handle back.
    fn take_handle(&self, fh: u64) -> Option<Handle> {
        self.handles.lock().by_fh.remove(&fh)
    }

    /// One handle of `ino` is gone. If it was the last one of an orphaned
    /// inode, reclaim the inode. Locks `stripe(ino)`, so the last-close and
    /// the last-unlink decisions of one inode cannot interleave.
    fn uncount(&self, ino: u64) {
        let _ns = self.ns.lock(&[ino]);
        self.uncount_held(ino);
    }

    /// `uncount` with `stripe(ino)` already held by the caller.
    fn uncount_held(&self, ino: u64) {
        let reclaim = {
            let mut t = self.handles.lock();
            let n = t.open_count.get_mut(&ino).expect("an open handle is counted");
            *n -= 1;
            *n == 0 && {
                t.open_count.remove(&ino);
                t.orphans.remove(&ino)
            }
        };
        // The store work runs after the table lock is released.
        if reclaim {
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
        self.readers.lock().remove(&ino);
        let _ = self.meta.delete_inode(ino);
    }

    /// The last link of a non-directory inode was removed: reclaim it now, or
    /// keep it as an orphan (nlink 0) while handles are still open. The
    /// caller holds `stripe(ino)` (open and last close take it too).
    fn drop_last_link(&self, inode: InodeMeta) -> Result<(), i32> {
        let ino = inode.ino;
        if self.handles.lock().open_count.contains_key(&ino) {
            self.meta.modify_inode(ino, |m| m.nlinks = 0).map_err(|e| meta_error_to_errno(&e))?;
            // The last handle may have closed meanwhile: then reclaim here.
            let mut t = self.handles.lock();
            if t.open_count.contains_key(&ino) {
                t.orphans.insert(ino);
                return Ok(());
            }
        }
        self.reclaim_inode(ino);
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

    /// End a clone of a streaming state as a stored root, first appending
    /// zeros from `streamed` up to `min_size` (an extension set by setattr).
    fn end_padded(&self, mut st: Stream, streamed: u64, min_size: u64) -> Result<ObjectKey, i32> {
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
        self.gc_start_mode(false)
    }

    /// As [`SliceFsFilesystem::gc_start`], for a full GC
    /// (`ChunkStore::gc_begin_full`: the objects put or read during the GC
    /// go to an exact set in temp files).
    pub fn gc_start_full(&self) -> Result<(metadata::gc::OnlineGc, u64), String> {
        self.gc_start_mode(true)
    }

    fn gc_start_mode(&self, full: bool) -> Result<(metadata::gc::OnlineGc, u64), String> {
        if !Arc::ptr_eq(&self.io, self.meta.io()) {
            return Err("file system and metadata do not share one StoreIo".into());
        }
        if !Arc::ptr_eq(self.io.lock().unwrap().store(), &self.content) {
            return Err("metadata and file content do not share one chunk store".into());
        }
        // A timed commit may still be writing its root with the gate released
        // (#108). The commit below waits for it in any case; waiting here,
        // before the gate, keeps requests from waiting for it too.
        self.meta.wait_for_commit();
        let gate = self.gate_exclusive();
        // From here the chunk store keeps each object put or read (nodes and
        // file content), and the GC changes only packs up to `boundary`.
        let begin = if full { self.content.gc_begin_full() } else { self.content.gc_begin() };
        let boundary = begin.map_err(|e| format!("GC start: chunk store error: {e}"))?;
        let mut file_roots = Vec::new();
        let mut states: Vec<Stream> = Vec::new();
        // One cell lock at a time (the exclusive gate is held).
        for cell in self.cells.all() {
            let g = self.lock_cell(&cell);
            if g.retired {
                continue;
            }
            match &g.file.write_mode {
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

    /// An online GC (a full one if `full`) whose mark calls `mark_pacer` and
    /// whose sweep calls `sweep_pacer` at their safe points (see
    /// `gc_pacing`). `Ok(None)` if `stop` was set first.
    pub fn gc_online_paced(
        &self,
        full: bool,
        stop: &std::sync::atomic::AtomicBool,
        mark_pacer: &dyn chunk_store::GcPacer,
        sweep_pacer: &dyn chunk_store::GcPacer,
    ) -> Result<Option<chunk_store::GcStats>, String> {
        let (gc, boundary) = self.gc_start_mode(full)?;
        let cs = &self.content;
        let r = if full {
            gc.live_exact_paced(stop, mark_pacer)
                .map(|m| m.map(|ids| cs.gc_finish_full_paced(ids, boundary, sweep_pacer)))
        } else {
            gc.live_paced(stop, mark_pacer).map(|m| m.map(|ids| cs.gc_finish_paced(&ids, boundary, sweep_pacer)))
        };
        match r {
            Ok(Some(swept)) => swept.map(Some).map_err(|e| format!("GC failed: {e}")),
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

    /// Run a full online GC (exact mark, no holes: every dead object of the
    /// packs up to the boundary goes; see `ChunkStore::gc_finish_full`).
    /// `Ok(None)` if `stop` was set first (then nothing is deleted).
    pub fn gc_online_full(
        &self,
        stop: &std::sync::atomic::AtomicBool,
    ) -> Result<Option<chunk_store::GcStats>, String> {
        let (gc, boundary) = self.gc_start_full()?;
        let cs = &self.content;
        match gc.live_exact(stop) {
            Ok(Some(ids)) => cs.gc_finish_full(ids, boundary).map(Some).map_err(|e| format!("GC failed: {e}")),
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

    /// True if the committed file has bytes (so a fresh stream would lose them).
    fn has_content(&self, ino: u64) -> bool {
        self.meta.get_inode(ino).map_or(false, |m| m.size > 0)
            && self.meta.get_manifest(ino).map_or(false, |m| !m.is_empty())
    }

    /// Switch the stream of the locked cell `g` to an overlay (D-02). Phase
    /// A clones the stream and reads its generation. Phase B (with the cell
    /// unlocked if `unlocked`) ends the clone as the base (no zero padding),
    /// or takes the committed content if nothing was streamed, and stores
    /// the manifest node. Phase C installs the base in the manifest at once
    /// (online GC keeps it, and replay starts from it) only if the stream
    /// did not change, and moves the pending writes into the extents. The
    /// caller then applies the write that caused the switch. No file
    /// content is read.
    ///
    /// `Ok(true)`: switched (or not a stream); `Ok(false)`: dropped (the
    /// stream changed, or the cell was retired meanwhile).
    ///
    /// Phase B runs as the cell's step (`CellGuard::run_step`): the caller
    /// makes sure no step is in flight.
    fn to_overlay(&self, g: &mut CellGuard<'_>) -> Result<bool, i32> {
        self.to_overlay_in(g, true)
    }

    /// `to_overlay` with Phase B in the cell hold: only for the size change
    /// of an open stream (`setattr_size_open`; 19-15 removes it). Never on
    /// the write or read path.
    fn to_overlay_locked(&self, g: &mut CellGuard<'_>) -> Result<bool, i32> {
        self.to_overlay_in(g, false)
    }

    /// The body of `to_overlay` (`as_step`) and `to_overlay_locked`.
    fn to_overlay_in(&self, g: &mut CellGuard<'_>, as_step: bool) -> Result<bool, i32> {
        let s = &g.file;
        let WriteMode::Streaming { state, next_expected_offset, .. } = &s.write_mode else {
            return Ok(true);
        };
        let (ino, next, generation, committed) = (s.ino, *next_expected_offset, s.generation, s.last_committed_root);
        let clone = (next > 0).then(|| state.clone());
        let io = move || self.overlay_base(ino, clone, next);
        let (base, base_len, old, digest) = if as_step { g.run_step(io) } else { io() }?;
        if g.retired || (as_step && self.forced_drop(g.no_drop_hook)) {
            return Ok(false);
        }
        let s = &mut g.file;
        // The manifest of an open inode changes only under its cell lock,
        // with a new generation or a new `last_committed_root` (a flush):
        // if neither moved, `old` is still the root the manifest names.
        if s.generation != generation || s.last_committed_root != committed {
            return Ok(false);
        }
        let WriteMode::Streaming { pending, .. } = &mut s.write_mode else { return Ok(false) };
        if base != old || base.is_none() {
            self.meta.set_manifest_digest(ino, digest).map_err(|_| libc::EIO)?;
        }
        if base != old {
            if let Some(k) = &base {
                self.meta.increment_refcount(k);
            }
            if let Some(o) = &old {
                self.meta.decrement_refcount(o);
            }
        }
        let pending = std::mem::take(pending);
        let size = base_len.max(s.byte_count).max(s.min_size);
        let mut ov = Overlay::new(base, base_len, size);
        for (o, v) in pending {
            ov.put(o, v);
        }
        s.byte_count = ov.size;
        s.write_mode = WriteMode::Overlay(ov);
        s.last_committed_root = base;
        s.min_size = 0;
        s.cas_committed = false;
        s.generation = next_gen();
        let modified = s.modified;
        // No older flush snapshot installs over this base.
        g.last_published_gen = g.file.generation;
        self.stamp_content(ino, size, modified)?;
        Ok(true)
    }

    /// Phase B of `to_overlay` (no cell lock needed): the base (the ended
    /// `clone` of a stream of `next` bytes, the committed content, or none)
    /// and its length, the root the manifest names now, and the digest of
    /// the stored manifest node of the base.
    #[allow(clippy::type_complexity)]
    fn overlay_base(
        &self,
        ino: u64,
        clone: Option<Stream>,
        next: u64,
    ) -> Result<(Option<ObjectKey>, u64, Option<ObjectKey>, ObjectKey), i32> {
        let cs = &*self.content;
        let store_err = |e: std::io::Error| {
            eprintln!("[{}][FUSE] overlay: content store error: {e}", ts());
            libc::EIO
        };
        let old = self.meta.get_manifest(ino).ok().and_then(|m| m.first().copied());
        let (base, base_len) = match clone {
            Some(st) => (Some(st.finish(cs).map_err(store_err)?), next),
            None if self.has_content(ino) => {
                let k = old.ok_or(libc::EIO)?;
                (Some(k), content::Reader::open(cs, &k).map_err(store_err)?.len())
            }
            None => (None, 0),
        };
        let digest = self.meta.prepare_manifest(base.as_slice()).map_err(|_| libc::EIO)?;
        Ok((base, base_len, old, digest))
    }

    /// One step of the stream of the locked cell `g` (D-02). Phase A clones
    /// the stream (a buffer under 2 x CHUNK_MAX plus the level entries) and
    /// reads its generation. Phase B pushes zeros from the stream end up to
    /// `zeros_to`, then `bytes`, into the clone, as the cell's step with the
    /// cell unlocked (`CellGuard::run_step`; the push hook runs there; the
    /// caller makes sure no step is in flight). Phase C swaps the clone in
    /// only if the stream did not change, and removes the pending entry
    /// `pending_key` that `bytes` came from.
    ///
    /// `Ok(true)`: published; `Ok(false)`: dropped (the stream changed or is
    /// gone, the cell was retired, or a test hook dropped it). On an error
    /// nothing changes.
    fn stream_step(&self, g: &mut CellGuard<'_>, zeros_to: u64, bytes: &[u8], pending_key: Option<u64>) -> Result<bool, i32> {
        self.stream_step_in(g, zeros_to, bytes, pending_key, true)
    }

    /// `stream_step` with Phase B in the cell hold: only for the locked
    /// settle of a size change of an open stream (19-15 Task 2 removes it)
    /// and of `test_destroy`, where no request runs. Never on the write,
    /// read or release path.
    fn stream_step_locked(&self, g: &mut CellGuard<'_>, zeros_to: u64, bytes: &[u8], pending_key: Option<u64>) -> Result<bool, i32> {
        self.stream_step_in(g, zeros_to, bytes, pending_key, false)
    }

    /// The body of `stream_step` (`as_step`) and `stream_step_locked`.
    fn stream_step_in(
        &self,
        g: &mut CellGuard<'_>,
        zeros_to: u64,
        bytes: &[u8],
        pending_key: Option<u64>,
        as_step: bool,
    ) -> Result<bool, i32> {
        let s = &g.file;
        let WriteMode::Streaming { state, next_expected_offset, .. } = &s.write_mode else {
            return Ok(false);
        };
        let (mut st, from, generation) = (state.clone(), *next_expected_offset, s.generation);
        let cs = &*self.content;
        let io = |st: &mut Stream| -> std::io::Result<()> {
            if zeros_to > from {
                st.push_zeros(cs, zeros_to - from)?;
            }
            if !bytes.is_empty() {
                st.push(cs, bytes)?;
            }
            Ok(())
        };
        let r = if as_step {
            g.run_step(|| {
                let r = io(&mut st);
                self.run_push_hook();
                r
            })
        } else {
            io(&mut st)
        };
        if let Err(e) = r {
            eprintln!("[{}][FUSE] stream: content store error: {e}", ts());
            return Err(libc::EIO);
        }
        if g.retired || (as_step && self.forced_drop(g.no_drop_hook)) {
            return Ok(false);
        }
        let s = &mut g.file;
        if s.generation != generation {
            return Ok(false);
        }
        let WriteMode::Streaming { state, next_expected_offset, pending } = &mut s.write_mode else {
            return Ok(false);
        };
        let next = zeros_to.max(from) + bytes.len() as u64;
        *state = st;
        *next_expected_offset = next;
        if let Some(k) = pending_key {
            pending.remove(&k);
        }
        s.byte_count = s.byte_count.max(next);
        s.cas_committed = false;
        s.generation = next_gen();
        Ok(true)
    }

    /// Stream the pending (out-of-order) writes of the locked cell `g` in
    /// offset order, zeros in the gaps, one locked stream step per pending
    /// write. Pending writes exist only on a fresh stream, so the gaps are
    /// zeros; no full-file buffer. Only for the rare size change of an open
    /// stream and for `test_destroy` (19-15 changes the first); never on
    /// the write or read path (a read composes pending writes from memory).
    fn settle_locked(&self, g: &mut CellGuard<'_>) -> Result<(), i32> {
        loop {
            if g.retired {
                return Ok(());
            }
            let WriteMode::Streaming { pending, .. } = &g.file.write_mode else { return Ok(()) };
            let Some((&o, v)) = pending.first_key_value() else { return Ok(()) };
            let v = v.clone();
            self.stream_step_locked(g, o, &v, Some(o))?;
        }
    }

    /// Push the pending writes of the stream of `g` that now start at its
    /// end, one stream step each, with the I/O unlocked. A dropped step is
    /// tried again (at most `STEP_TRIES` in a row, then EIO; the entry then
    /// stays pending).
    fn drain_lined_up(&self, g: &mut CellGuard<'_>) -> Result<(), i32> {
        let mut drops = 0usize;
        loop {
            if g.retired {
                return Ok(());
            }
            let WriteMode::Streaming { next_expected_offset, pending, .. } = &g.file.write_mode else {
                return Ok(());
            };
            let o = *next_expected_offset;
            let Some(v) = pending.get(&o).cloned() else { return Ok(()) };
            if self.stream_step(g, o, &v, Some(o))? {
                drops = 0;
            } else {
                drops += 1;
                if drops >= STEP_TRIES {
                    return Err(libc::EIO);
                }
            }
        }
    }

    /// Store a part of an overlay as a new root (#108), with the cell lock
    /// held across the I/O: the three phases of `rebase_unlocked` in one
    /// lock hold, so the publish always holds. For `usize::MAX` the splice
    /// reads the live overlay (no copy, #98); a smaller step copies its
    /// pieces (about `max(free, MIN_TAKE)` bytes).
    ///
    /// The splice cuts again only the chunks near the stored extents; every
    /// other chunk and whole old node is reused by ID. Fixed memory,
    /// canonical key, CPU in the change (on long repeating non-zero data the
    /// re-chunk can run to the end of the file). A clean overlay is kept.
    ///
    /// GC: a step runs inside one callback under the shared commit gate,
    /// and the old base stays in the manifest until the new root is
    /// installed.
    ///
    /// Only for `finalize_locked` (`test_destroy`, where no request runs);
    /// never on a request path. Returns the rebase trace line, for
    /// the caller to print after its hold.
    fn rebase_step(&self, ino: u64, s: &mut OpenFileState, free: usize) -> Result<Option<String>, i32> {
        let Some((plan, part)) = Self::step_plan(s, free, free != usize::MAX) else {
            return Ok(None);
        };
        let (root, digest, line) = match (&part, &s.write_mode) {
            (Some(p), _) => self.step_io(ino, p, &plan),
            (None, WriteMode::Overlay(ov)) => self.step_io(ino, ov, &plan),
            (None, WriteMode::Streaming { .. }) => unreachable!("step_plan gives a plan for an overlay only"),
        }?;
        drop(part);
        self.step_publish(ino, s, plan, (root, digest)).map(|_| Some(line))
    }

    /// Most dirty cost of one file (#98, RESEARCH Pitfall 1): the limit, the
    /// spare capacity a growing extent can take (1/`GROW_DIV` of its
    /// length: the regrowth of an extent near the limit after a step cut
    /// it), and two rebase steps (Claude's discretion). The write path
    /// gates its apply on it (`write_in_cell`).
    fn hard_cap(&self) -> usize {
        let limit = self.dirty_limit.load(Ordering::Relaxed);
        limit.saturating_add(limit / crate::overlay::GROW_DIV).saturating_add(2 * REBASE_STEP)
    }

    /// A rebase step of the writer's own locked cell `g` (D-02, D-13), with
    /// the I/O unlocked (`rebase_unlocked`): skipped if another step of the
    /// cell is in flight; a dropped result is tried again, at most
    /// `REBASE_TRIES` attempts. If all are dropped this returns Ok: the
    /// write is in the overlay already and under the hard cap (the cap gate
    /// of `write_in_cell`), and the next write, the release or a budget step
    /// tries again. Only this file waits.
    fn rebase_cell(&self, g: &mut CellGuard<'_>, free: usize) -> Result<(), i32> {
        for _ in 0..REBASE_TRIES {
            if g.retired || g.stepping.is_some() || self.rebase_unlocked(g, free)? {
                return Ok(());
            }
        }
        Ok(())
    }

    /// At the hard cap: wait for the step in flight with the cell unlocked
    /// (`wait_step`), or run an own unlocked rebase step that frees at least
    /// `excess` (and at least `REBASE_STEP`). A dropped own step counts in
    /// `drops`, a published one resets it. The caller looks at the cost again.
    fn cap_step(&self, g: &mut CellGuard<'_>, excess: usize, drops: &mut usize) -> Result<(), i32> {
        if g.stepping.is_some() {
            return g.wait_step();
        }
        if self.rebase_unlocked(g, excess.max(REBASE_STEP))? {
            *drops = 0;
        } else {
            *drops += 1;
        }
        Ok(())
    }

    /// One rebase step of the locked cell `g` by the protocol of D-02:
    /// Phase A (cell locked, memory only) copies the pieces and reads the
    /// generation; Phase B (cell unlocked) splices, hashes and puts them and
    /// runs the rebase hook; Phase C (cell locked) publishes only if the
    /// generation did not move. A dropped result is garbage until a GC. On
    /// a Phase B error nothing is put back: the live overlay kept every byte
    /// (RESEARCH Pitfall 2).
    ///
    /// `Ok(false)`: no step ran to the end (another one is in its I/O
    /// phase, or the overlay changed meanwhile); `Ok(true)`: published, or
    /// nothing to do (clean, or the cell was retired meanwhile).
    fn rebase_unlocked(&self, g: &mut CellGuard<'_>, free: usize) -> Result<bool, i32> {
        if g.stepping.is_some() {
            return Ok(false);
        }
        let Some((plan, Some(part))) = Self::step_plan(&g.file, free, true) else {
            return Ok(true);
        };
        let ino = g.file.ino;
        let done = g.run_step(|| -> Result<(Option<ObjectKey>, ObjectKey), i32> {
            let (root, digest, line) = self.step_io(ino, &part, &plan)?;
            eprintln!("{line}");
            Ok((root, digest))
        });
        drop(part);
        if g.retired {
            return Ok(true);
        }
        let done = done?;
        if self.forced_drop(g.no_drop_hook) {
            return Ok(false);
        }
        self.step_publish(ino, &mut g.file, plan, done)
    }

    /// Phase A of a rebase step (memory only): the pieces to store, the
    /// generation, and with `copy` a copy of the overlay with those pieces
    /// only. `None` for a clean overlay or a stream.
    fn step_plan(s: &OpenFileState, free: usize, copy: bool) -> Option<(StepPlan, Option<Overlay>)> {
        let WriteMode::Overlay(ov) = &s.write_mode else { return None };
        if ov.is_clean() {
            return None;
        }
        let (pieces, part) = if copy {
            let copied = ov.copy_step(free);
            let pieces = copied.iter().map(|(o, v)| (*o, v.len())).collect();
            let mut part = ov.empty_like();
            for (o, v) in copied {
                part.put(o, v);
            }
            (pieces, Some(part))
        } else {
            (ov.step_ranges(free), None)
        };
        let plan = StepPlan { generation: ov.generation(), size: ov.size, pieces, free, cost_before: ov.cost() };
        Some((plan, part))
    }

    /// Phase B of a rebase step (no cell lock needed): splice the extents
    /// of `src` (the copy, or the live overlay when every extent is stored)
    /// onto its base, store the manifest node of the new root, count, and
    /// run the rebase hook (after the new root is finished, before it is
    /// installed). Returns the new root (none for size 0), its manifest
    /// digest (the empty manifest for size 0) and the rebase trace line
    /// (the caller prints it with no cell lock held).
    #[allow(clippy::type_complexity)]
    fn step_io(&self, ino: u64, src: &Overlay, plan: &StepPlan) -> Result<(Option<ObjectKey>, ObjectKey, String), i32> {
        let cs = &*self.content;
        let store_err = |e: std::io::Error| {
            eprintln!("[{}][FUSE] rebase: content store error: {e}", ts());
            libc::EIO
        };
        let size = plan.size;
        let t0 = std::time::Instant::now();
        let ranges = src.extent_ranges();
        let (taken_n, taken_bytes) = (ranges.len(), ranges.iter().map(|r| r.end - r.start).sum::<u64>());
        let (root, stats, base_bytes) = if size == 0 {
            (None, chunk_store::file::SpliceStats::default(), 0)
        } else {
            let mut src_io = OverlaySource::new(cs, src).map_err(store_err)?;
            let (k, stats) = content::splice(cs, src.base.as_ref(), src.visible, &ranges, size, &mut src_io)
                .map_err(store_err)?;
            (Some(k), stats, src_io.base_bytes)
        };
        let splice_ms = t0.elapsed().as_secs_f64() * 1000.0;
        // A size-0 root gets the empty manifest node here too, so Phase C
        // only swaps digests.
        let digest = self.meta.prepare_manifest(root.as_slice()).map_err(|_| libc::EIO)?;
        let rechunked = if root.is_some() { stats.rechunked } else { 0 };
        self.rebase_count.fetch_add(1, Ordering::Relaxed);
        LAST_REBASE_BYTES.with(|c| c.set(rechunked));
        add_to(&WRITE_REBASE_BYTES, rechunked);
        add_to(&WRITE_BASE_BYTES, base_bytes);
        let line = format!("[t={}][rebase] ino={ino} size={size} free={} cost_before={} taken={taken_n}/{taken_bytes} \
            rechunked={rechunked} base_read={base_bytes} reused_entries={} reused_nodes={} nodes_loaded={} splice_ms={splice_ms:.1}",
            crate::util::epoch(), if plan.free == usize::MAX { "all".to_string() } else { plan.free.to_string() },
            plan.cost_before, stats.reused_entries, stats.reused_nodes, stats.nodes_loaded);
        let hook = self.rebase_hook.lock().clone();
        if let Some(hook) = hook {
            hook();
        }
        Ok((root, digest, line))
    }

    /// Phase C of a rebase step (cell locked, memory only, also for a
    /// size-0 root): if the overlay is still at the generation of `plan`,
    /// install the digest of the new root (refcounts from
    /// `last_committed_root` as it is now; for an overlay it is the root
    /// the manifest names),
    /// remove the stored pieces, and go on with an overlay on the new root
    /// that holds the rest. Else drop the result (`Ok(false)`).
    fn step_publish(
        &self,
        ino: u64,
        s: &mut OpenFileState,
        plan: StepPlan,
        (root, digest): (Option<ObjectKey>, ObjectKey),
    ) -> Result<bool, i32> {
        let WriteMode::Overlay(ov) = &mut s.write_mode else { return Ok(false) };
        if ov.generation() != plan.generation {
            return Ok(false);
        }
        let old = s.last_committed_root;
        if root != old || root.is_none() {
            self.meta.set_manifest_digest(ino, digest).map_err(|_| libc::EIO)?;
        }
        if root != old {
            if let Some(k) = &root {
                self.meta.increment_refcount(k);
            }
            if let Some(o) = &old {
                self.meta.decrement_refcount(o);
            }
        }
        // The manifest names the new root now: the overlay follows it even
        // if the stamp below fails.
        ov.discard(&plan.pieces);
        let mut next = Overlay::new(root, plan.size, plan.size);
        next.set_sweep(ov.sweep());
        let rest = std::mem::replace(ov, Overlay::new(None, 0, 0));
        for (o, v) in rest.into_extents() {
            next.put(o, v);
        }
        *ov = next;
        s.last_committed_root = root;
        s.byte_count = plan.size;
        self.stamp_content(ino, plan.size, s.modified).map(|_| true)
    }

    /// Read `size` bytes at `offset` of the content `root` of `ino` through
    /// the per-inode reader cache (it keeps its node and chunk caches). The
    /// map lock is held only to clone or add the `Arc`; the per-inode reader
    /// is locked for the read only. If another read holds it, this read
    /// opens a private reader instead of waiting (D-07).
    fn read_root(&self, ino: u64, root: &ObjectKey, offset: u64, size: usize) -> std::io::Result<Vec<u8>> {
        let shared = self.readers.lock().get(&ino).cloned();
        let Some(shared) = shared else {
            // The first read of `ino`: open with no lock, then keep it.
            let mut r = content::Reader::open(&self.content, root)?;
            let out = r.read(&self.content, offset, size);
            let mut readers = self.readers.lock();
            if readers.len() >= 64 && !readers.contains_key(&ino) {
                readers.clear();
            }
            readers.entry(ino).or_insert_with(|| Arc::new(sync::Mutex::new(Rank::Short, r)));
            return out;
        };
        let Some(mut r) = shared.try_lock() else {
            return content::Reader::open(&self.content, root)?.read(&self.content, offset, size);
        };
        if r.key() != *root {
            *r = content::Reader::open(&self.content, root)?;
        }
        let hook = self.read_hook.lock().clone();
        if let Some(hook) = hook {
            hook();
        }
        r.read(&self.content, offset, size)
    }

    /// Test hook: true if the inode of write handle `fh` is still in
    /// Streaming mode.
    pub fn test_is_streaming(&self, fh: u64) -> bool {
        self.ino_of_fh(fh)
            .and_then(|ino| self.with_cell(ino, |s| matches!(s.write_mode, WriteMode::Streaming { .. })))
            .unwrap_or(false)
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
        let (sec, nsec) = now_pair();
        self.meta.modify_inode(ino, |inode| {
            inode.mtime_sec = sec;
            inode.mtime_nsec = nsec;
            inode.ctime_sec = sec;
            inode.ctime_nsec = nsec;
        }).map_err(|e| meta_error_to_errno(&e))
    }

    /// Add `delta` to the link count of `ino` (directory ".." bookkeeping).
    fn adjust_nlinks(&self, ino: u64, delta: i32) -> Result<(), i32> {
        self.meta.modify_inode(ino, |inode| {
            inode.nlinks = (inode.nlinks as i64 + delta as i64).max(0) as u32;
        }).map_err(|e| meta_error_to_errno(&e))
    }

    /// One name of non-directory `ino` went: its link count goes down by one
    /// (not below 0), and while links are left its ctime changes too.
    /// Returns the inode after the change.
    fn drop_link(&self, ino: u64) -> Result<InodeMeta, i32> {
        let (sec, nsec) = now_pair();
        self.meta.modify_inode(ino, |inode| {
            inode.nlinks = inode.nlinks.saturating_sub(1);
            if inode.nlinks > 0 {
                inode.ctime_sec = sec;
                inode.ctime_nsec = nsec;
            }
            inode.clone()
        }).map_err(|e| meta_error_to_errno(&e))
    }

    /// With the parent's stripe held: the parent is still a live directory
    /// (PITFALLS 6c: an rmdir of the parent that ran first).
    fn live_dir(&self, ino: u64) -> Result<(), i32> {
        let m = self.meta.get_inode(ino).map_err(|e| meta_error_to_errno(&e))?;
        if m.mode & S_IFMT != S_IFDIR {
            return Err(libc::ENOTDIR);
        }
        Ok(())
    }

    /// Run `f` with `stripe(ino)` held and then the inode's cell locked, if
    /// it has a live one (RESEARCH Pattern 4 rule 5: stripe, then cell), so
    /// a setattr or xattr change of the inode is ordered with its writes.
    fn with_inode_locked<R>(&self, ino: u64, f: impl FnOnce(Option<&mut OpenFileState>) -> R) -> R {
        let _ns = self.ns.lock(&[ino]);
        let cell = self.cells.get(ino);
        let mut g = cell.as_deref().map(|c| self.lock_cell(c)).filter(|g| !g.retired);
        f(g.as_mut().map(|g| &mut g.file))
    }

    /// Orphans kept for open handles (for tests).
    pub fn test_orphan_count(&self) -> usize {
        self.handles.lock().orphans.len()
    }

    /// Lock the namespace for an operation on the inodes `known` and the
    /// children named by `lookups` (`(parent, name)`), with the rename mutex
    /// first if `cross_dir` (RESEARCH Pattern 4 rule 2). A child is read
    /// from its parent, then all stripes are locked in ascending order, and
    /// the children are read again under them: on a change the stripes are
    /// released and taken again for the new set. Returns the guard and the
    /// lookup results under it (stable while the guard is held, since every
    /// change of a directory's entries holds the directory's stripe).
    pub(crate) fn lock_namespace(
        &self,
        cross_dir: bool,
        known: &[u64],
        lookups: &[(u64, &str)],
    ) -> (NsGuard<'_>, Vec<Result<u64, MetaError>>) {
        let look = || lookups.iter().map(|&(p, n)| self.meta.lookup(p, n)).collect::<Vec<_>>();
        let same = |a: &[Result<u64, MetaError>], b: &[Result<u64, MetaError>]| {
            a.iter().map(|r| r.as_ref().ok()).eq(b.iter().map(|r| r.as_ref().ok()))
        };
        let mut rename = cross_dir.then(|| self.ns.lock_rename());
        let mut seen = look();
        loop {
            let mut inos = known.to_vec();
            inos.extend(seen.iter().filter_map(|r| r.as_ref().ok().copied()));
            let g = self.ns.lock_after(rename.take(), &inos);
            let now = look();
            if same(&seen, &now) {
                return (g, now);
            }
            rename = g.into_rename();
            seen = now;
        }
    }

    /// Reclaim all orphans (used at unmount: no handle survives it).
    fn reclaim_all_orphans(&self) {
        let orphans: Vec<u64> = self.handles.lock().orphans.drain().collect();
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
        self.create_logged(parent_ino, name, mode, umask, uid, gid, 0, None, &mut |_| {}).map(|(ino, fh, _)| (ino, fh))
    }

    /// `test_create` at inode `at` (replay; `None`: the next free one) that
    /// logs (D-08): the `Create` record (with `flags`) goes to `log` in the
    /// hold that also opens the handle and, with `O_TRUNC`, truncates; the
    /// result of the truncate is the third value.
    #[allow(clippy::too_many_arguments)]
    fn create_logged(
        &self,
        parent_ino: u64,
        name: &str,
        mode: u32,
        umask: u32,
        uid: u32,
        gid: u32,
        flags: i32,
        at: Option<u64>,
        log: LogOp<'_>,
    ) -> Result<(u64, u64, Result<(), i32>), i32> {
        let file_mode = S_IFREG | (mode & !umask & 0o7777);
        let mut file_meta = InodeMeta::new_file(0, uid, gid, file_mode);
        Self::stamp_new(&mut file_meta);
        // The parent's stripe and the new inode's are held until the handle
        // is counted and the record appended (`new_entry`).
        let (ino, _ns) = self.new_entry(parent_ino, name, at, &file_meta)?;
        self.touch_dir(parent_ino)?;
        let fh = self.open_handle(ino, true, false);
        self.open_write(ino, false, None)?;
        let trunc = if flags & libc::O_TRUNC != 0 { self.test_setattr_size(ino, Some(fh), 0) } else { Ok(()) };
        log(crate::oplog::Op::Create { parent: parent_ino, name: name.to_string(), mode, umask, uid, gid, flags, ino });
        Ok((ino, fh, trunc))
    }

    /// The new inode `meta` (at `at` in replay) linked as `name` in
    /// `parent`, which must be a live directory; returns it with the stripes
    /// of the parent and the new inode held. The number is taken before the
    /// stripes (nothing can reach an inode with no name), so both stripes
    /// lock in ascending order. While the guard is held, a request that
    /// finds the new name and works on the inode by its number (open, link,
    /// setattr, xattrs) waits for the inode's stripe, so its record comes
    /// after the create's (D-08); an unlink of the name waits for the
    /// parent's, so it cannot reclaim the inode before the handle of a
    /// create is counted.
    fn new_entry(&self, parent: u64, name: &str, at: Option<u64>, meta: &InodeMeta) -> Result<(u64, NsGuard<'_>), i32> {
        let ino = new_inode(&self.meta, at, meta).map_err(|e| meta_error_to_errno(&e))?;
        let ns = self.ns.lock(&[parent, ino]);
        let linked = self.live_dir(parent).and_then(|()| self.meta.link(parent, name, ino).map_err(|e| meta_error_to_errno(&e)));
        if let Err(e) = linked {
            let _ = self.meta.delete_inode(ino);
            return Err(e);
        }
        Ok((ino, ns))
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
    /// The write runs under the cell lock of the inode, but each stream
    /// step (push, zero gap, the switch to an overlay) and the rebase step
    /// of an overlay past its limit do their I/O with the cell unlocked
    /// (D-02): the stream is cloned, pushed with no cell lock, and swapped
    /// in only if it did not change. One step of a cell is in flight at a
    /// time (the step token): another stream writer waits for it with the
    /// cell unlocked. A dropped step is tried again with its I/O unlocked
    /// (at most `STEP_TRIES` in a row, then EIO).
    /// Only a write handle may write (else EBADF).
    pub fn test_write(&self, fh: u64, offset: u64, data: &[u8]) -> Result<u32, i32> {
        self.write_logged(fh, offset, data, 0, None).0.map(|(n, _)| n)
    }

    /// `test_write` that also returns the effective offset (for an append:
    /// the end of file it wrote at).
    pub fn test_write_at(&self, fh: u64, offset: u64, data: &[u8]) -> Result<(u32, u64), i32> {
        self.write_logged(fh, offset, data, 0, None).0
    }

    /// `test_write` that logs (D-08): with `time`, the `Write` record (the
    /// inode of the cell, the effective offset) is appended in the cell-lock
    /// hold that applies the write, exactly once per applied write. Returns
    /// the bytes written and the effective offset, and the LSN of the record
    /// for `log_write_out` (also after an error that came after the apply).
    /// `flags` are the open flags of the request: with `O_APPEND` (or on a
    /// handle opened with it) the write goes to the end of file (D-12).
    pub(crate) fn write_logged(
        &self,
        fh: u64,
        offset: u64,
        data: &[u8],
        flags: i32,
        time: Option<(i64, u32)>,
    ) -> (Result<(u32, u64), i32>, Option<Lsn>) {
        let mut lsn = None;
        let mut switch = None;
        let r = self.write_in_cell(fh, offset, data, flags, time, &mut lsn, &mut switch);
        // The cell guard is gone: print the debug line of a switch now.
        if let Some(SwitchNote { ino, offset, expected, pending, fresh, overlap }) = switch {
            eprintln!("[{}][FUSE] write: fh={} ino={} switches to overlay (offset={} len={} expected={} pending={} fresh={} overlap={})",
                ts(), fh, ino, offset, data.len(), expected, pending, fresh, overlap);
        }
        (r, lsn)
    }

    /// The body of `write_logged`; `lsn` gets the LSN of the record, and
    /// `switch` the fields of the debug line of a switch to an overlay.
    #[allow(clippy::too_many_arguments)]
    fn write_in_cell(
        &self,
        fh: u64,
        mut offset: u64,
        data: &[u8],
        flags: i32,
        time: Option<(i64, u32)>,
        lsn: &mut Option<Lsn>,
        switch: &mut Option<SwitchNote>,
    ) -> Result<(u32, u64), i32> {
        WRITE_REBASE_BYTES.with(|c| c.set(0));
        WRITE_BASE_BYTES.with(|c| c.set(0));
        let Some(h) = self.handle(fh).filter(|h| h.write) else { return Err(libc::EBADF) };
        let ino = h.ino;
        // An append ignores the request offset (D-12, D-17). Replay does not
        // append: each record carries the offset the write used.
        let append = (h.append || flags & libc::O_APPEND != 0) && !self.replaying.load(Ordering::Relaxed);
        if !append && offset.checked_add(data.len() as u64).map_or(true, |end| end > MAX_FILE_SIZE) {
            return Err(libc::EFBIG);
        }
        let Some(cell) = self.cells.get(ino) else { return Err(libc::EBADF) };
        let mut g = self.lock_cell(&cell);
        if g.retired {
            return Err(libc::EBADF);
        }
        // Called in the hold that applies the write (with the cell locked).
        let log = |lsn: &mut Option<Lsn>, at: u64| {
            if let Some(time) = time {
                let op = crate::oplog::Op::Write { ino: cell.ino, offset: at, data: data.to_vec() };
                *lsn = self.log_append(crate::oplog::Record { time, op });
            }
        };
        let mut end = offset.saturating_add(data.len() as u64);
        // Dropped steps in a row; each is tried again with its I/O unlocked.
        let mut drops = 0usize;
        // Dirty cost before this write (after a switch: of the new overlay).
        let mut cost_before;
        let result = loop {
            if g.retired {
                // The handle was released while a step ran unlocked.
                return Err(libc::EBADF);
            }
            if drops >= STEP_TRIES {
                // No other write changes the stream while a step is in flight
                // (they wait for the token), so only a forced drop (a test
                // hook), the retire of the cell, an fsync that installs a newer
                // snapshot (at most once) or a storm of size changes of the
                // open stream (until 19-15 they do not wait for the token) can
                // drop this many steps in a row.
                drop(g);
                eprintln!("[{}][FUSE] write: fh={fh} ino={ino}: {STEP_TRIES} steps in a row were dropped", ts());
                return Err(libc::EIO);
            }
            let streaming = matches!(g.file.write_mode, WriteMode::Streaming { .. });
            if streaming && g.stepping.is_some() {
                // A stream writer waits for the step in flight with the cell
                // unlocked (readers never wait for it), then looks again.
                g.wait_step()?;
                continue;
            }
            if append {
                // The end of file now, under the cell lock. Each turn takes
                // it again: an unlocked step can move it, and then its
                // publish is dropped (the generation moved) and this turn
                // runs again.
                offset = self.append_eof(&g.file);
                end = match offset.checked_add(data.len() as u64) {
                    Some(e) if e <= MAX_FILE_SIZE => e,
                    _ => return Err(libc::EFBIG),
                };
            }
            cost_before = dirty_cost(&g.file);
            if let WriteMode::Overlay(ov) = &g.file.write_mode {
                // The hard cap (#98): apply only if the dirty cost stays
                // under it. Else wait for the step in flight, or run an own
                // unlocked step, with no cell lock held, and look again. At
                // the cap no writer changes the overlay, so a step publishes.
                let (cap, after) = (self.hard_cap(), ov.cost_after(offset, data.len()));
                if cost_before > 0 && after > cap {
                    self.cap_step(&mut g, after - cap, &mut drops)?;
                    continue;
                }
                let r = Self::overlay_write(&mut g.file, offset, data);
                if r.is_ok() {
                    log(lsn, offset);
                }
                break r;
            }
            let WriteMode::Streaming { next_expected_offset, pending, .. } = &g.file.write_mode else { continue };
            let next = *next_expected_offset;
            // Streaming from scratch is only right when the file starts
            // empty; otherwise unwritten parts must keep the old bytes. The
            // first write reads the committed size and manifest as a step
            // with the cell unlocked, then checks that the stream did not
            // change.
            let fresh = if next > 0 || !pending.is_empty() {
                true
            } else {
                let generation = g.file.generation;
                let has = g.run_step(|| self.has_content(ino));
                if g.retired || g.file.generation != generation {
                    drops += 1;
                    continue;
                }
                !has
            };
            let s = &mut g.file;
            let WriteMode::Streaming { pending, .. } = &mut s.write_mode else { continue };
            let overlaps_pending = pending.range(..end).next_back()
                .map_or(false, |(&o, v)| o + v.len() as u64 > offset);
            let pending_bytes: usize = pending.values().map(Vec::len).sum();

            if fresh && !overlaps_pending && offset > next + REORDER_WINDOW as u64 {
                // A jump past the reorder window is a sparse write, not NFS
                // reordering: stream the pending writes and the gap as zeros
                // (O(1) memory), one step at a time, and go on.
                let piece = pending.first_key_value().filter(|(o, _)| **o < offset).map(|(&o, v)| (o, v.clone()));
                let r = match &piece {
                    Some((o, v)) => self.stream_step(&mut g, *o, v, Some(*o)),
                    None => self.stream_step(&mut g, offset, &[], None),
                };
                match r {
                    Ok(true) => drops = 0,
                    Ok(false) => drops += 1,
                    Err(e) => {
                        eprintln!("[{}][FUSE] test_write: CAS I/O error while filling a gap", ts());
                        return Err(e);
                    }
                }
                continue;
            }

            if fresh && offset == next && !overlaps_pending {
                // In order: push, then the pending writes that now line up.
                if !self.stream_step(&mut g, next, data, None)? {
                    drops += 1;
                    continue;
                }
                // Still the hold of Phase C that published the write: log it
                // before `drain_lined_up` can unlock the cell.
                log(lsn, offset);
                g.file.modified = Some(now_pair());
                // This write is in; a pending write that fails here stays
                // pending (a read composes it, the release streams it later).
                if let Err(e) = self.drain_lined_up(&mut g) {
                    eprintln!("[{}][FUSE] test_write: CAS I/O error while draining pending writes: {e}", ts());
                }
                break Ok(data.len() as u32);
            }
            if fresh && offset > next && !overlaps_pending && pending_bytes + data.len() <= REORDER_WINDOW {
                // Ahead of the stream: wait in the reorder window.
                pending.insert(offset, data.to_vec());
                s.byte_count = s.byte_count.max(end);
                s.cas_committed = false;
                s.modified = Some(now_pair());
                s.generation = next_gen();
                log(lsn, offset);
                break Ok(data.len() as u32);
            }
            // Random write: switch to an overlay on the stored content (the
            // debug line is printed after the hold, by `write_logged`).
            *switch = Some(SwitchNote { ino, offset, expected: next, pending: pending_bytes, fresh, overlap: overlaps_pending });
            // Older pending writes go in first; the next turn applies this
            // (newest) write to the overlay.
            if self.to_overlay(&mut g)? {
                drops = 0;
            } else {
                drops += 1;
            }
        };

        // An overlay past the dirty limit (also right after a switch that
        // moved pending writes in) stores a part of itself as a new root now,
        // in this callback (never at fsync or at a commit). One step frees
        // the larger of the cost this write added and `REBASE_STEP`, down
        // to `limit - REBASE_STEP`: the work of one write does not grow with
        // the limit (#108), and the cost never grows while it is above the
        // limit (after a switch it comes down with each write smaller than
        // `REBASE_STEP`). Not the sum of both: bigwrite showed the 1 MiB
        // writes, whose steps freed 2 MiB, as the slowest ones. The added cost
        // counts at most the bytes of the write (and one extent overhead):
        // the spare capacity of a growing extent (up to 1/8 of its length)
        // is freed by the next steps, so a step does not grow with the limit.
        let limit = self.dirty_limit.load(Ordering::Relaxed);
        let added = dirty_cost(&g.file).saturating_sub(cost_before).min(data.len() + crate::overlay::EXTENT_OVERHEAD);
        // A write never returns with the cost above the hard cap: if the
        // apply passed it after all (the gate predicts the extent growth),
        // step or wait as at the gate until the cost is under it.
        let cap = self.hard_cap();
        while result.is_ok() && !g.retired && matches!(&g.file.write_mode, WriteMode::Overlay(_)) && dirty_cost(&g.file) > cap {
            if drops >= STEP_TRIES {
                drop(g);
                eprintln!("[{}][FUSE] write: fh={fh} ino={ino}: {STEP_TRIES} steps in a row at the hard cap were dropped", ts());
                return Err(libc::EIO);
            }
            let excess = dirty_cost(&g.file) - cap;
            self.cap_step(&mut g, excess, &mut drops)?;
        }
        let cost = dirty_cost(&g.file);
        let over = matches!(&g.file.write_mode, WriteMode::Overlay(_)) && cost > limit;
        // The write is in the overlay already (a read sees all of it or
        // none): the step does its I/O with the cell unlocked (D-02).
        let result = match result {
            Ok(n) if over => {
                let free = (cost - limit.saturating_sub(REBASE_STEP)).min(added.max(REBASE_STEP));
                self.rebase_cell(&mut g, free).map(|_| n).map_err(|_| libc::EIO)
            }
            r => r,
        };
        if g.retired {
            // The last release finalized the file (with this write) while
            // the step was unlocked.
            return result.map(|n| (n, offset));
        }
        // Then the budget of all open files (#105), with the same bound.
        match result {
            Ok(n) => self.keep_budget(&mut g, added).map(|_| (n, offset)),
            Err(e) => Err(e),
        }
    }

    /// The end of file for an append to the open state `s` (with its cell
    /// locked): the state's size if it has bytes or is an overlay, else the
    /// committed size (a fresh cell on an existing file).
    fn append_eof(&self, s: &OpenFileState) -> u64 {
        if s.byte_count > 0 || matches!(s.write_mode, WriteMode::Overlay(_)) {
            return s.byte_count;
        }
        self.meta.get_inode(s.ino).map_or(0, |m| m.size)
    }

    /// Keep the dirty bytes of all open files within the budget (#105). `g`
    /// is the locked cell of the file being written, and `added` the dirty
    /// cost its write added. The check reads only the global total. If it
    /// is above the budget, this file (and only this one, D-13: no other
    /// cell is locked) stores steps until the cost `max(added, REBASE_STEP)`
    /// is freed, the total is `REBASE_STEP` below the budget, or the file is
    /// clean. A write can always free the bytes it added, so the total does
    /// not grow while it is above the budget, and a write pays for at most
    /// that much work. Moving work to other files is Phase 20 (flusher).
    fn keep_budget(&self, g: &mut CellGuard<'_>, added: usize) -> Result<(), i32> {
        let budget = self.dirty_budget.load(Ordering::Relaxed);
        let target = budget.saturating_sub(REBASE_STEP);
        let mut cap: Option<usize> = None;
        let mut freed = 0usize;
        loop {
            g.sync_dirty();
            let total = self.dirty_total.load(Ordering::Relaxed);
            if cap.is_none() && total <= budget {
                return Ok(());
            }
            let cap = *cap.get_or_insert((total - target.min(total)).min(added.max(REBASE_STEP)));
            if freed >= cap || total <= target {
                return Ok(());
            }
            if g.retired {
                return Ok(());
            }
            let before = dirty_cost(&g.file);
            self.free_dirty(g, cap - freed)?;
            let step = before.saturating_sub(dirty_cost(&g.file));
            if step == 0 {
                return Ok(());
            }
            freed += step;
        }
    }

    /// Free about `want` bytes of dirty cost of the locked cell `g`: a
    /// rebase step of its overlay (`rebase_cell`, D-02); a stream with
    /// pending writes becomes an overlay first (no content is read or cut
    /// there), as an unlocked step under the token. If a step is in flight,
    /// or the switch is dropped, it frees nothing (as a skipped step).
    fn free_dirty(&self, g: &mut CellGuard<'_>, want: usize) -> Result<(), i32> {
        if matches!(&g.file.write_mode, WriteMode::Streaming { pending, .. } if !pending.is_empty())
            && (g.stepping.is_some() || !self.to_overlay(g)? || g.retired)
        {
            return Ok(());
        }
        self.rebase_cell(g, want).map_err(|_| libc::EIO)
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
    ///
    /// The handle leaves the table first, so a write handle is counted out
    /// of its cell exactly once; only the last write handle of the inode
    /// finalizes. An unknown handle is ignored.
    pub fn test_release(&self, ino: u64, fh: u64) -> Result<(), i32> {
        self.release_logged(ino, fh, None).0
    }

    /// `test_release` that logs (see `release_write`); returns the LSN of
    /// the `Release` record, if one was appended.
    pub(crate) fn release_logged(&self, _ino: u64, fh: u64, time: Option<(i64, u32)>) -> (Result<(), i32>, Option<Lsn>) {
        let Some(h) = self.take_handle(fh) else { return (Ok(()), None) };
        let result = if h.write { self.release_write(h.ino, time) } else { (Ok(()), None) };
        self.uncount(h.ino);
        result
    }

    /// Flush the write buffer for `fh` to CAS, then reset the buffer to empty.
    ///
    /// Unlike `test_release`, the file handle remains open after this call.
    /// Used by `test_fsync` and the FUSE `fsync()` callback.
    pub fn test_fsync(&self, _ino: u64, fh: u64) -> Result<(), i32> {
        self.flush_handle(fh)?;
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
    /// The cell lock is held only for memory copies (D-02): the overlay
    /// view of the range, or for a stream the clone (only if the range
    /// starts in the streamed prefix), the sizes and the parts of pending
    /// writes inside the range. The read then composes the result with no
    /// lock: stream bytes, zeros up to the size, the pending parts on top.
    /// Pending writes overlap neither each other nor the streamed prefix
    /// (the insert rule), so this is the content a settle would give. A
    /// read runs no stream step and never waits for the step token.
    ///
    /// Used by integration tests to bypass the FUSE request/reply layer.
    pub fn test_read(&self, ino: u64, offset: u64, size: u32) -> Result<Vec<u8>, i32> {
        // Check for open write handles on this inode (cross-handle read support).
        enum WriterSnapshot {
            /// The stream clone (if the range starts below `streamed`), the
            /// streamed end, the size, and the pending parts in the range.
            Streaming { clone: Option<Stream>, streamed: u64, size: u64, parts: Vec<(u64, Vec<u8>)> },
            Overlay(crate::overlay::RangeView),
        }

        let writer_snapshot: Option<WriterSnapshot> = match self.cells.get(ino) {
            None => None,
            Some(cell) => {
                let g = self.lock_cell(&cell);
                let s = &g.file;
                if g.retired || s.byte_count == 0 {
                    None
                } else {
                    match &s.write_mode {
                        // Overlay: copy only the extent parts inside the range.
                        WriteMode::Overlay(ov) => Some(WriterSnapshot::Overlay(ov.view(offset, size as u64))),
                        WriteMode::Streaming { state, next_expected_offset, pending } => {
                            let (streamed, size_now) = (*next_expected_offset, s.byte_count);
                            let end = offset.saturating_add(size as u64).min(size_now);
                            // Pending entries do not overlap, so their ends
                            // are in order too: walk back from `end`.
                            let mut parts: Vec<(u64, Vec<u8>)> = pending
                                .range(..end)
                                .rev()
                                .take_while(|(o, v)| *o + v.len() as u64 > offset)
                                .map(|(&o, v)| {
                                    let from = offset.max(o);
                                    let to = end.min(o + v.len() as u64);
                                    (from, v[(from - o) as usize..(to - o) as usize].to_vec())
                                })
                                .collect();
                            parts.reverse();
                            let clone = (offset < streamed).then(|| state.clone());
                            Some(WriterSnapshot::Streaming { clone, streamed, size: size_now, parts })
                        }
                    }
                }
            }
        };
        // cell lock released

        let root: ObjectKey = match writer_snapshot {
            Some(WriterSnapshot::Streaming { clone, streamed, size: logical, parts }) => {
                let end = offset.saturating_add(size as u64).min(logical);
                if offset >= end {
                    return Ok(Vec::new());
                }
                let mut out = match clone {
                    // Uncommitted content: finish a clone of the writer to get a root.
                    Some(st) => {
                        let root = st.finish(&self.content).map_err(|_| libc::EIO)?;
                        let n = (end.min(streamed) - offset) as usize;
                        self.read_root(ino, &root, offset, n).map_err(|_| libc::EIO)?
                    }
                    None => Vec::new(),
                };
                // Past the streamed bytes (gaps, a setattr extension): zeros.
                out.resize((end - offset) as usize, 0);
                for (o, bytes) in parts {
                    let at = (o - offset) as usize;
                    out[at..at + bytes.len()].copy_from_slice(&bytes);
                }
                return Ok(out);
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

        self.read_root(ino, &root, offset, size as usize).map_err(|_| libc::EIO)
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
    /// For a read-only or unknown handle it returns Ok and does nothing.
    /// The content of `ino` is now `size` bytes, last changed at `modified`
    /// (now if unknown): set size, mtime and ctime. ctime does not go back
    /// past a later change (a chmod after the last write).
    fn stamp_content(&self, ino: u64, size: u64, modified: Option<(i64, u32)>) -> Result<(), i32> {
        let (sec, nsec) = modified.unwrap_or_else(now_pair);
        self.meta.modify_inode(ino, |inode| {
            inode.size = size;
            inode.mtime_sec = sec;
            inode.mtime_nsec = nsec;
            if (sec, nsec) > (inode.ctime_sec, inode.ctime_nsec) {
                inode.ctime_sec = sec;
                inode.ctime_nsec = nsec;
            }
        }).map_err(|_| libc::EIO)
    }

    /// Commit with a clean cut of the write log (#20): first write the
    /// streamed prefix of each open write handle into its manifest (an
    /// overlay's base is there already), so that the root holds every request
    /// logged before the commit, apart from the bytes logged again after it.
    /// No content is hashed in full here. The caller holds the commit gate
    /// exclusively (no request is in flight).
    pub fn commit_cut(&self) -> Result<ObjectKey, MetaError> {
        // As the commit timer does: sync the objects so far first. The chunk
        // store keeps the bytes written since its last sync in RAM, and the
        // relog of the cut would add its pieces on top of them. An error
        // here is seen again by the sync of the commit, which then fails.
        let _ = self.content.sync();
        let cut = self.commit_cut_begin()?;
        self.commit_cut_finish(cut)
    }

    /// Second half of [`Self::commit_cut`] (#108): write the metadata, sync,
    /// log the root. Needs no gate: requests after the cut go to the next
    /// generation of the log and of the store.
    pub fn commit_cut_finish(&self, cut: CommitCut<'_>) -> Result<ObjectKey, MetaError> {
        let t = std::time::Instant::now();
        let root = self.meta.finish_commit(cut.prepared)?;
        eprintln!("[t={}][commit-cut] finish_ms={}", crate::util::epoch(), t.elapsed().as_millis());
        Ok(root)
    }

    /// First half of [`Self::commit_cut`] (#108), under the exclusive commit
    /// gate: the generation switch. Puts the streamed prefixes into the
    /// manifests, takes the metadata state (`begin_commit`, which cuts the
    /// write log), and logs again what is only in RAM, into the new
    /// generation of the log. No node of the root is written and nothing is
    /// synced here; [`Self::commit_cut_finish`] does that with the gate released.
    pub fn commit_cut_begin(&self) -> Result<CommitCut<'_>, MetaError> {
        use crate::oplog::{Op, Record};
        // The exclusive gate is held: lock the cells one at a time.
        let cells = self.cells.all();
        // Streamed prefixes go into the manifests (cheap: an ended clone).
        for cell in &cells {
            let r = {
                let mut g = self.lock_cell(cell);
                // Locked: the exclusive gate is held, so no request runs.
                if g.retired { Ok(()) } else { self.flush_cell(&mut g, FlushIo::Locked).map(|_| ()) }
            };
            if let Err(e) = r {
                return Err(MetaError::Corrupted(format!("cut: flush of open file {} failed: errno {e}", cell.ino)));
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
            Overlay { cell: Arc<FileCell>, base_len: u64, visible: u64, size: u64, extents: Vec<std::ops::Range<u64>> },
        }
        struct Item { ino: u64, time: (i64, u32), ctime: Option<(i64, u32)>, what: Relog }
        let mut items: Vec<Item> = Vec::new();
        for cell in &cells {
            {
                let g = self.lock_cell(cell);
                if g.retired {
                    continue;
                }
                let s = &g.file;
                let ino = s.ino;
                let time = s.modified.unwrap_or_else(now_pair);
                let what = match &s.write_mode {
                    WriteMode::Streaming { pending, .. } if !pending.is_empty() => Relog::Pending(
                        pending.iter().map(|(&off, data)| Record { time, op: Op::Write { ino, offset: off, data: data.clone() } }).collect(),
                    ),
                    WriteMode::Overlay(ov) if !ov.is_clean() => Relog::Overlay {
                        cell: Arc::clone(cell), base_len: ov.base_len, visible: ov.visible, size: ov.size, extents: ov.extent_ranges(),
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
        let prepared = self.meta.begin_commit()?;
        let commit_ms = t_commit.elapsed().as_millis();
        let t_relog = std::time::Instant::now();
        // These records go into the new generation of the log, after the cut
        // and before any later request. When the root is logged, the old
        // segments are deleted, so they are then the only copy of bytes an
        // fsync may already have made durable: `finish_commit` syncs them
        // before it logs the root. A crash before that replays them on top
        // of the old root and the old log, where they change nothing.
        let mut relog_bytes = 0u64;
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
                Relog::Overlay { cell, base_len, visible, size, extents } => {
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
                            let data = {
                                let g = self.lock_cell(&cell);
                                match &g.file.write_mode {
                                    WriteMode::Overlay(ov) if !g.retired => ov.copy_out(off, n as usize),
                                    _ => Vec::new(),
                                }
                            };
                            if data.len() as u64 != n {
                                // `prepared` drops: the commit is not made.
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
        let relog_ms = t_relog.elapsed().as_millis();
        eprintln!("[t={}][commit-cut] begin_ms={commit_ms} relog_bytes={relog_bytes} relog_ms={relog_ms}",
            crate::util::epoch());
        Ok(CommitCut { prepared })
    }

    /// Write the streamed content of handle `fh` into the manifest of `ino`.
    /// An overlay needs nothing: its base is in the manifest, and its extents
    /// are durable through the write log (#20). A rebase at each fsync would
    /// cost a write of the changed chunks and nodes per fsync (#3).
    /// Any write handle of the inode flushes the shared cell; a read-only
    /// or unknown handle does nothing. The end of the snapshot runs with
    /// the cell unlocked (see `flush_cell`).
    fn flush_handle(&self, fh: u64) -> Result<(), i32> {
        self.flush_logged(fh, None).0
    }

    /// `flush_handle` that logs: with `time`, a flush that installed (its
    /// Phase C ran) appends its `Flush` record in that hold (D-08); a flush
    /// with nothing to do appends none.
    pub(crate) fn flush_logged(&self, fh: u64, time: Option<(i64, u32)>) -> (Result<(), i32>, Option<Lsn>) {
        let Some(ino) = self.ino_of_fh(fh) else { return (Ok(()), None) };
        let Some(cell) = self.cells.get(ino) else { return (Ok(()), None) };
        let mut g = self.lock_cell(&cell);
        if g.retired {
            return (Ok(()), None);
        }
        match self.flush_cell(&mut g, FlushIo::Unlocked) {
            Ok(true) => {
                let rec = |time| crate::oplog::Record { time, op: crate::oplog::Op::Flush { ino: cell.ino } };
                (Ok(()), time.and_then(|t| self.log_append(rec(t))))
            }
            r => (r.map(|_| ()), None),
        }
    }

    /// Write the streamed content of the locked cell `g` into its manifest
    /// (fsync, flush, the commit cut, the last release; D-02, RESEARCH
    /// Pitfall 5). Phase A clones the stream and reads its generation.
    /// Phase B (by `mode`; with the cell unlocked the push hook runs there)
    /// ends the clone, zero-padded to `min_size`, and stores the manifest
    /// node. Phase C installs it only if no newer snapshot, and no
    /// other install (a switch to an overlay, a truncate to 0), came first
    /// (`last_published_gen`); the refcounts use `last_committed_root` as
    /// it is then. The state is clean (`cas_committed`) only if the stream
    /// did not change after the snapshot.
    ///
    /// Pending (out-of-order) bytes stay pending: settling them here would
    /// stream zeros for the gaps, and the late writes that fill the gaps
    /// would then force the handle into an overlay. The write log keeps
    /// pending bytes durable (#20), so the manifest takes the streamed prefix.
    ///
    /// `Ok(true)`: Phase C ran (the flush did work, with the cell locked).
    fn flush_cell(&self, g: &mut CellGuard<'_>, mode: FlushIo) -> Result<bool, i32> {
        let s = &g.file;
        if s.byte_count == 0 {
            // Empty file -- nothing to flush
            return Ok(false);
        }
        if s.cas_committed && s.last_committed_root.is_some() {
            // No write since the last fsync: the manifest is current.
            // (NFS sends a COMMIT per group of writes; skip the re-hash.)
            return Ok(false);
        }
        let WriteMode::Streaming { state, next_expected_offset, .. } = &s.write_mode else {
            return Ok(false);
        };
        let (ino, snap, streamed) = (s.ino, state.clone(), *next_expected_offset);
        let (min_size, byte_count, modified, generation) = (s.min_size, s.byte_count, s.modified, s.generation);
        let io = || -> Result<(ObjectKey, ObjectKey), i32> {
            let k = self.end_padded(snap, streamed, min_size)?;
            Ok((k, self.meta.prepare_manifest(&[k]).map_err(|_| libc::EIO)?))
        };
        let (new, digest) = match mode {
            FlushIo::Locked => io(),
            FlushIo::Unlocked => g.unlocked(|| {
                let r = io();
                self.run_push_hook();
                r
            }),
            FlushIo::Step => g.run_step(|| {
                let r = io();
                self.run_push_hook();
                r
            }),
        }?;
        // Phase C: a newer snapshot, or another install, came first.
        if g.retired || g.last_published_gen >= generation || !matches!(g.file.write_mode, WriteMode::Streaming { .. }) {
            return Ok(false);
        }
        g.last_published_gen = generation;
        let s = &mut g.file;
        let old = s.last_committed_root;
        if old != Some(new) {
            self.meta.set_manifest_digest(ino, digest).map_err(|_| libc::EIO)?;
            self.meta.increment_refcount(&new);
            if let Some(o) = old {
                self.meta.decrement_refcount(&o);
            }
            s.last_committed_root = Some(new);
        }
        s.cas_committed = s.generation == generation;
        // With no write since the snapshot, `modified` can still have moved:
        // a setattr of the mtime in the I/O phase sets it and wins (as at a
        // close). Stamp the current one then, not the snapshot's.
        let modified = if s.cas_committed { s.modified } else { modified };
        self.stamp_content(ino, byte_count, modified).map(|_| true)
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
    /// A size change goes to the open write state of the inode whatever
    /// handle it names (the NFS SETATTR from cp carries no handle).
    pub fn test_setattr_size(&self, ino: u64, _fh: Option<u64>, new_size: u64) -> Result<(), i32> {
        self.setattr_size_logged(ino, new_size, &mut |_| {})
    }

    /// `test_setattr_size` that logs a size-only `Setattr` in the cell hold
    /// of the change (the open cell or the transient cell).
    fn setattr_size_logged(&self, ino: u64, new_size: u64, log: LogOp<'_>) -> Result<(), i32> {
        if new_size > MAX_FILE_SIZE {
            return Err(libc::EFBIG);
        }
        let size_op = || crate::oplog::Op::Setattr {
            ino, mode: None, uid: None, gid: None, size: Some(new_size), atime: None, mtime: None,
        };
        loop {
            // Case A: the inode has an open write state (also one that its
            // last release is still storing).
            if let Some(cell) = self.cells.get(ino) {
                let mut g = self.lock_cell(&cell);
                if g.retired {
                    continue;
                }
                let r = self.setattr_size_open(&mut g, new_size);
                if r.is_ok() {
                    log(size_op());
                }
                return r;
            }
            // Case B: no open write state. A transient cell, locked before
            // it goes into the map, orders this change against an open or
            // another size change of the inode; its lock is held across the
            // splice (RESEARCH Pattern 1: the one accepted I/O under a cell
            // lock beside the locked retry and the hard cap).
            let cell = Arc::new(FileCell::new(ino));
            let mut g = self.lock_cell(&cell);
            if !self.cells.insert_new(&cell) {
                continue;
            }
            let r = self.setattr_size_closed(ino, new_size);
            if r.is_ok() {
                log(size_op());
            }
            g.retired = true;
            self.cells.remove(&cell);
            return r;
        }
    }

    /// Case B of `test_setattr_size` (the caller holds the transient cell):
    /// splice the committed content to the new size, truncated or
    /// zero-extended. NotFound from get_manifest means the inode exists but
    /// has no content yet (brand-new file created by create() before any
    /// write/release). Treat as empty content. The rebase hook runs after
    /// the splice, before the install (for tests).
    fn setattr_size_closed(&self, ino: u64, new_size: u64) -> Result<(), i32> {
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
            LAST_REBASE_BYTES.with(|c| c.set(stats.rechunked));
            let hook = self.rebase_hook.lock().clone();
            if let Some(hook) = hook {
                hook();
            }
            self.meta.set_manifest(ino, &[new_digest]).map_err(|_| libc::EIO)?;
            self.meta.increment_refcount(&new_digest);
        }

        // Update inode
        self.stamp_size(ino, new_size)
    }

    /// The size change of `test_setattr_size` on the locked cell `g` (held
    /// across the manifest and inode changes, and across the rare stream
    /// steps of a cut of a stream).
    fn setattr_size_open(&self, g: &mut CellGuard<'_>, new_size: u64) -> Result<(), i32> {
        let ino = g.file.ino;
        let s = &mut g.file;
        // Extending a streaming handle: record the size, no materializing.
        if matches!(s.write_mode, WriteMode::Streaming { .. })
            && new_size > 0 && new_size >= s.byte_count
            && (s.byte_count > 0 || !self.has_content(ino))
        {
            s.min_size = new_size;
            s.byte_count = new_size;
            s.cas_committed = false;
            s.modified = Some(now_pair());
            s.generation = next_gen();
            return self.stamp_size(ino, new_size);
        }
        if new_size == 0 {
            // Back to an empty stream (a new state: a new generation).
            let old_root = s.last_committed_root.take();
            *s = OpenFileState { modified: Some(now_pair()), ..OpenFileState::new(ino) };
            // No flush snapshot of the old content installs after this.
            g.last_published_gen = g.file.generation;
            if let Some(old) = old_root {
                self.meta.decrement_refcount(&old);
            }
            self.clear_manifest(ino, old_root)?;
        } else {
            // Pending writes are streamed first. A stream that is not purely
            // extended switches to an overlay on the content so far and is
            // cut or extended there. No file content is read. These steps
            // keep the cell locked (a rare size change of an open stream).
            self.settle_locked(g)?;
            self.to_overlay_locked(g)?;
            let s = &mut g.file;
            if let WriteMode::Overlay(ov) = &mut s.write_mode {
                ov.truncate(new_size);
                s.byte_count = ov.size;
            }
            s.min_size = 0;
            s.cas_committed = false;
            s.modified = Some(now_pair());
        }
        self.stamp_size(ino, new_size)
    }

    /// A truncate or extend to `size`: it changes the content, so mtime and
    /// ctime both change (to now).
    fn stamp_size(&self, ino: u64, size: u64) -> Result<(), i32> {
        let (sec, nsec) = now_pair();
        self.meta.modify_inode(ino, |inode| {
            inode.size = size;
            inode.mtime_sec = sec;
            inode.mtime_nsec = nsec;
            inode.ctime_sec = sec;
            inode.ctime_nsec = nsec;
        }).map_err(|_| libc::EIO)
    }

    /// Update permission bits (preserving file type bits) for an inode.
    pub fn test_setattr_mode(&self, ino: u64, mode: u32) -> Result<(), i32> {
        let _ns = self.ns.lock(&[ino]);
        let (now_sec, now_nsec) = now_pair();
        self.meta.modify_inode(ino, |inode| {
            inode.mode = (inode.mode & S_IFMT) | (mode & 0o7777);
            inode.ctime_sec = now_sec;
            inode.ctime_nsec = now_nsec;
        }).map_err(|_| libc::EIO)
    }

    /// Update uid and/or gid for an inode.
    pub fn test_setattr_uid_gid(
        &self,
        ino: u64,
        uid: Option<u32>,
        gid: Option<u32>,
    ) -> Result<(), i32> {
        let _ns = self.ns.lock(&[ino]);
        let (now_sec, now_nsec) = now_pair();
        self.meta.modify_inode(ino, |inode| {
            if let Some(u) = uid {
                inode.uid = u;
            }
            if let Some(g) = gid {
                inode.gid = g;
            }
            inode.ctime_sec = now_sec;
            inode.ctime_nsec = now_nsec;
        }).map_err(|_| libc::EIO)
    }

    /// Update mtime for an inode to a specific (sec, nsec).
    pub fn test_setattr_mtime(&self, ino: u64, mtime_sec: i64, mtime_nsec: u32) -> Result<(), i32> {
        let _ns = self.ns.lock(&[ino]);
        let (now_sec, now_nsec) = now_pair();
        self.meta.modify_inode(ino, |inode| {
            inode.mtime_sec = mtime_sec;
            inode.mtime_nsec = mtime_nsec;
            inode.ctime_sec = now_sec;
            inode.ctime_nsec = now_nsec;
        }).map_err(|_| libc::EIO)
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
        self.mknod_logged(parent, name, mode, rdev, uid, gid, umask, None, &mut |_| {})
    }

    /// `test_mknod_rdev` at inode `at` (replay) that logs its `Mknod` record
    /// in the parent-stripe hold (D-08).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn mknod_logged(
        &self, parent: u64, name: &str, mode: u32, rdev: u32, uid: u32, gid: u32, umask: u32,
        at: Option<u64>, log: LogOp<'_>,
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
        let (ino, _ns) = self.new_entry(parent, name, at, &file_meta)?;
        self.touch_dir(parent)?;
        let meta = self.meta.get_inode(ino).map_err(|e| meta_error_to_errno(&e))?;
        log(crate::oplog::Op::Mknod { parent, name: name.to_string(), mode, umask, rdev, uid, gid, ino });
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
        self.mkdir_logged(parent_ino, name, mode, umask, uid, gid, None, &mut |_| {}).map(|(ino, _)| ino)
    }

    /// `simulate_mkdir` at inode `at` (replay) that logs its `Mkdir` record
    /// in the stripe hold (D-08). Returns `(ino, InodeMeta)`. As `new_entry`
    /// does, it takes the number first and locks the stripes of the parent
    /// and of the new directory before the directory is linked: a request
    /// that finds the new name (a create in it, a setattr) waits for the new
    /// directory's stripe, so its record comes after the `Mkdir` record.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn mkdir_logged(
        &self,
        parent_ino: u64,
        name: &str,
        mode: u32,
        umask: u32,
        uid: u32,
        gid: u32,
        at: Option<u64>,
        log: LogOp<'_>,
    ) -> Result<(u64, InodeMeta), i32> {
        let dir_mode = S_IFDIR | (mode & !umask & 0o7777);
        let mut dir_meta = InodeMeta::new_directory(0, uid, gid, dir_mode);
        Self::stamp_new(&mut dir_meta);
        let ino = at.unwrap_or_else(|| self.meta.reserve_ino());
        let _ns = self.ns.lock(&[parent_ino, ino]);
        self.live_dir(parent_ino)?;
        let ino = self.meta.create_directory_at(parent_ino, name, ino, &dir_meta).map_err(|e| meta_error_to_errno(&e))?;
        let hook = self.mkdir_hook.lock().clone();
        if let Some(hook) = hook {
            hook();
        }
        self.touch_dir(parent_ino)?;
        let meta = self.meta.get_inode(ino).map_err(|e| meta_error_to_errno(&e))?;
        log(crate::oplog::Op::Mkdir { parent: parent_ino, name: name.to_string(), mode, umask, uid, gid, ino });
        Ok((ino, meta))
    }

    /// Remove empty directory `name` from `parent_ino`.
    ///
    /// Returns ENOTEMPTY if the directory still has entries beyond . and ..
    /// Returns EISDIR if the target is not a directory (though lookup normally guards this).
    pub fn simulate_rmdir(&self, parent_ino: u64, name: &str) -> Result<(), i32> {
        self.rmdir_logged(parent_ino, name, &mut |_| {})
    }

    /// `simulate_rmdir` that logs its record in the stripe hold (D-08).
    pub(crate) fn rmdir_logged(&self, parent_ino: u64, name: &str, log: LogOp<'_>) -> Result<(), i32> {
        // Resolve name to inode, with the parent and child stripes held (a
        // create in the child holds the child's stripe too).
        let (_ns, found) = self.lock_namespace(false, &[parent_ino], &[(parent_ino, name)]);
        let ino = found[0].as_ref().map_err(meta_error_to_errno).copied()?;

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
        self.meta
            .modify_inode(parent_ino, |m| m.nlinks = m.nlinks.saturating_sub(1))
            .map_err(|e| meta_error_to_errno(&e))?;
        self.touch_dir(parent_ino)?;
        log(crate::oplog::Op::Rmdir { parent: parent_ino, name: name.to_string() });
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
        self.unlink_logged(parent_ino, name, &mut |_| {})
    }

    /// `simulate_unlink` that logs its record in the stripe hold (D-08).
    pub(crate) fn unlink_logged(&self, parent_ino: u64, name: &str, log: LogOp<'_>) -> Result<(), i32> {
        // Resolve name, with the parent and child stripes held.
        let (_ns, found) = self.lock_namespace(false, &[parent_ino], &[(parent_ino, name)]);
        let ino = found[0].as_ref().map_err(meta_error_to_errno).copied()?;

        // Must not be a directory
        let inode = self.meta.get_inode(ino).map_err(|e| meta_error_to_errno(&e))?;
        if inode.mode & S_IFMT == S_IFDIR {
            return Err(libc::EISDIR);
        }

        // Remove directory entry
        self.meta
            .unlink(parent_ino, name)
            .map_err(|e| meta_error_to_errno(&e))?;

        let inode = self.drop_link(ino)?;
        if inode.nlinks == 0 {
            // Reclaim now, or keep as an orphan while handles are open.
            self.drop_last_link(inode)?;
        }
        self.touch_dir(parent_ino)?;
        log(crate::oplog::Op::Unlink { parent: parent_ino, name: name.to_string() });
        Ok(())
    }

    /// Create a hard link: add `newname` in `newparent_ino` pointing at `ino`.
    ///
    /// POSIX disallows hard links to directories — returns EPERM.
    /// Returns the new inode number (same as `ino`).
    pub fn simulate_link(&self, ino: u64, newparent_ino: u64, newname: &str) -> Result<u64, i32> {
        self.link_logged(ino, newparent_ino, newname, &mut |_| {}).map(|(ino, _)| ino)
    }

    /// `simulate_link` that logs its record in the stripe hold (D-08).
    /// Returns `(ino, InodeMeta)`.
    pub(crate) fn link_logged(
        &self,
        ino: u64,
        newparent_ino: u64,
        newname: &str,
        log: LogOp<'_>,
    ) -> Result<(u64, InodeMeta), i32> {
        // The inode and the new parent, under their stripes.
        let (_ns, _) = self.lock_namespace(false, &[ino, newparent_ino], &[]);
        let inode = self.meta.get_inode(ino).map_err(|e| meta_error_to_errno(&e))?;
        // An orphan (no link left, kept for open handles) gets no new name:
        // its last close reclaims it.
        if inode.nlinks == 0 {
            return Err(libc::ENOENT);
        }
        self.live_dir(newparent_ino)?;

        // Disallow hard links to directories
        if inode.mode & S_IFMT == S_IFDIR {
            return Err(libc::EPERM);
        }

        // Add new directory entry
        self.meta
            .link(newparent_ino, newname, ino)
            .map_err(|e| meta_error_to_errno(&e))?;

        // Increment nlinks and update ctime
        let (now_sec, now_nsec) = now_pair();
        let meta = self.meta.modify_inode(ino, |inode| {
            inode.nlinks += 1;
            inode.ctime_sec = now_sec;
            inode.ctime_nsec = now_nsec;
            inode.clone()
        }).map_err(|e| meta_error_to_errno(&e))?;
        self.touch_dir(newparent_ino)?;
        log(crate::oplog::Op::Link { ino, newparent: newparent_ino, newname: newname.to_string() });
        Ok((ino, meta))
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
        self.rename_logged(parent_ino, name, newparent_ino, newname, flags, &mut |_| {})
    }

    /// `simulate_rename` that logs its record in the stripe hold (D-08).
    pub(crate) fn rename_logged(
        &self,
        parent_ino: u64,
        name: &str,
        newparent_ino: u64,
        newname: &str,
        flags: u32,
        log: LogOp<'_>,
    ) -> Result<(), i32> {
        let op = || crate::oplog::Op::Rename {
            parent: parent_ino, name: name.to_string(), newparent: newparent_ino, newname: newname.to_string(), flags,
        };
        // RENAME_EXCHANGE not supported
        if flags & 2 != 0 {
            return Err(libc::ENOSYS);
        }

        // Both parents, the source and the replaced target under their
        // stripes; a cross-directory rename takes the rename mutex first.
        let (_ns, found) = self.lock_namespace(
            parent_ino != newparent_ino,
            &[parent_ino, newparent_ino],
            &[(parent_ino, name), (newparent_ino, newname)],
        );
        let mut found = found.into_iter();
        let (src, dst) = (found.next().expect("two lookups"), found.next().expect("two lookups"));

        // Resolve source inode
        let src_ino = src.map_err(|e| meta_error_to_errno(&e))?;
        let src_is_dir = self.meta.get_inode(src_ino).map_err(|e| meta_error_to_errno(&e))?
            .mode & S_IFMT == S_IFDIR;

        // RENAME_NOREPLACE: fail if target already exists
        let target_ino = match dst {
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
                log(op());
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
                let left = self.drop_link(dst_ino)?;
                if left.nlinks == 0 {
                    // Reclaim now, or keep as an orphan while handles are open.
                    self.drop_last_link(left)?;
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
        let (now_sec, now_nsec) = now_pair();
        self.meta.modify_inode(src_ino, |m| {
            m.ctime_sec = now_sec;
            m.ctime_nsec = now_nsec;
        }).map_err(|e| meta_error_to_errno(&e))?;
        log(op());
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
        self.symlink_logged(parent_ino, link_name, target, uid, gid, None, &mut |_| {}).map(|(ino, _)| ino)
    }

    /// `simulate_symlink` at inode `at` (replay) that logs its `Symlink`
    /// record in the parent-stripe hold (D-08). Returns `(ino, InodeMeta)`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn symlink_logged(
        &self,
        parent_ino: u64,
        link_name: &str,
        target: &str,
        uid: u32,
        gid: u32,
        at: Option<u64>,
        log: LogOp<'_>,
    ) -> Result<(u64, InodeMeta), i32> {
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

        // Store target as CAS content (raw bytes — v3 format, no compression),
        // before the parent's stripe: no stripe is held across content I/O.
        // If the create fails, the object is garbage for the GC.
        let content_digest = content::put_all(&self.content, target_bytes).map_err(|_| libc::EIO)?;

        // The inode, linked into the parent, with both stripes held.
        let (ino, _ns) = self.new_entry(parent_ino, link_name, at, &symlink_meta)?;
        self.touch_dir(parent_ino)?;

        self.meta
            .set_manifest(ino, &[content_digest])
            .map_err(|_| libc::EIO)?;
        self.meta.increment_refcount(&content_digest);

        // Update inode size to raw target length (uncompressed)
        let len = target_bytes.len() as u64;
        let meta = self.meta.modify_inode(ino, |m| { m.size = len; m.clone() }).map_err(|_| libc::EIO)?;
        log(crate::oplog::Op::Symlink {
            parent: parent_ino, name: link_name.to_string(), target: target.to_string(), uid, gid, ino,
        });
        Ok((ino, meta))
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
        self.with_cell(meta.ino, |s| {
            if s.byte_count > 0 {
                meta.size = s.byte_count;
            }
            // Unflushed writes: show the mtime (and ctime) the close will stamp.
            if let Some((sec, nsec)) = s.modified {
                meta.mtime_sec = sec;
                meta.mtime_nsec = nsec;
                if (sec, nsec) > (meta.ctime_sec, meta.ctime_nsec) {
                    meta.ctime_sec = sec;
                    meta.ctime_nsec = nsec;
                }
            }
        });
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
        self.open_logged(ino, flags, None).0
    }

    /// `test_open` that logs: with `time`, a write open with `O_TRUNC`
    /// appends its `Truncate` record in the cell-lock hold of the truncate.
    pub(crate) fn open_logged(&self, ino: u64, flags: i32, time: Option<(i64, u32)>) -> (Result<(u64, bool), i32>, Option<Lsn>) {
        let acc_mode = flags & libc::O_ACCMODE;
        let is_write = acc_mode == libc::O_WRONLY || acc_mode == libc::O_RDWR;

        // Under stripe(ino): the inode exists, and the handle is counted
        // before an unlink can decide that nobody has it open.
        let _ns = self.ns.lock(&[ino]);
        if self.meta.get_inode(ino).is_err() {
            return (Err(libc::ENOENT), None);
        }
        let fh = self.open_handle(ino, is_write, flags & libc::O_APPEND != 0);

        let mut lsn = None;
        if is_write {
            // Every write handle of the inode shares its cell.
            // FUSE_ATOMIC_O_TRUNC: the truncate runs under the same cell lock.
            match self.open_write(ino, flags & libc::O_TRUNC != 0, time) {
                Ok(l) => lsn = l,
                Err(e) => {
                    self.close_handle_held(fh);
                    return (Err(e), None);
                }
            }
        }

        (Ok((fh, is_write)), lsn)
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
    /// Under `stripe(ino)` (then its cell): the inode must still exist, so
    /// a set that loses the race with the delete of the inode leaves no
    /// xattr entry behind.
    pub fn test_setxattr(&self, ino: u64, name: &str, value: &[u8]) -> Result<(), i32> {
        self.setxattr_logged(ino, name, value, &mut |_| {})
    }

    /// `test_setxattr` that logs its record in the stripe (and cell) hold.
    pub(crate) fn setxattr_logged(&self, ino: u64, name: &str, value: &[u8], log: LogOp<'_>) -> Result<(), i32> {
        self.with_inode_locked(ino, |_| {
            self.meta.get_inode(ino).map_err(|e| meta_error_to_errno(&e))?;
            self.meta.set_xattr(ino, name, value).map_err(|e| meta_error_to_errno(&e))?;
            log(crate::oplog::Op::Setxattr { ino, name: name.to_string(), value: value.to_vec() });
            Ok(())
        })
    }

    /// Remove an extended attribute.
    pub fn test_removexattr(&self, ino: u64, name: &str) -> Result<(), i32> {
        self.removexattr_logged(ino, name, &mut |_| {})
    }

    /// `test_removexattr` that logs its record in the stripe (and cell) hold.
    pub(crate) fn removexattr_logged(&self, ino: u64, name: &str, log: LogOp<'_>) -> Result<(), i32> {
        self.with_inode_locked(ino, |_| {
            self.meta.get_inode(ino).map_err(|e| meta_error_to_errno(&e))?;
            self.meta.remove_xattr(ino, name).map_err(|e| meta_error_to_errno(&e))?;
            log(crate::oplog::Op::Removexattr { ino, name: name.to_string() });
            Ok(())
        })
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
        self.setattr_logged(ino, mode, uid, gid, size, fh, atime, mtime, &mut |_| {})
    }

    /// `test_setattr_full` that logs (D-08). A size change logs a size-only
    /// `Setattr` in the cell hold of the size change, so it keeps its order
    /// with the writes of the inode; the other fields then log a `Setattr`
    /// without the size in the stripe-and-cell hold of their apply (also
    /// when they are all `None`: that hold sets the ctime).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn setattr_logged(
        &self,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        _fh: Option<u64>,
        atime: Option<(i64, u32)>,
        mtime: Option<(i64, u32)>,
        log: LogOp<'_>,
    ) -> Result<InodeMeta, i32> {
        // Size first: it rewrites the inode (size, mtime, ctime); the other
        // fields of the same request are applied on top of the result. The
        // size change is ordered by the cell lock alone (the open cell, or
        // the transient cell of a closed file), not under the stripe: it can
        // run stream steps or a splice (RESEARCH Pattern 4 rule 1).
        if let Some(new_size) = size {
            self.setattr_size_logged(ino, new_size, &mut *log)?;
        }
        // The other fields: stripe, then the cell (if open), so they are
        // ordered with the writes of the inode.
        self.with_inode_locked(ino, |cell| {
        // Always update ctime
        let (now_sec, now_nsec) = now_pair();
        let inode = self.meta.modify_inode(ino, |inode| {
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
            }
            inode.ctime_sec = now_sec;
            inode.ctime_nsec = now_nsec;
            inode.clone()
        }).map_err(|e| meta_error_to_errno(&e))?;
        if let (Some((sec, nsec)), Some(s)) = (mtime, cell) {
            // An explicit mtime (utimens) on a file with unflushed writes
            // wins over the time of those writes: the close stamps this one.
            if s.modified.is_some() {
                s.modified = Some((sec, nsec));
            }
        }
        log(crate::oplog::Op::Setattr { ino, mode, uid, gid, size: None, atime, mtime });
        Ok(inode)
        })
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
        self.create_full_logged(parent, name, mode, umask, uid, gid, flags, None, &mut |_| {})
    }

    /// `test_create_full` at inode `at` that logs (see `create_logged`).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create_full_logged(
        &self,
        parent: u64,
        name: &str,
        mode: u32,
        umask: u32,
        uid: u32,
        gid: u32,
        flags: i32,
        at: Option<u64>,
        log: LogOp<'_>,
    ) -> Result<(u64, u64, InodeMeta), i32> {
        let (ino, fh, trunc) = self.create_logged(parent, name, mode, umask, uid, gid, flags, at, log)?;
        if let Err(e) = trunc {
            if self.take_handle(fh).is_some() {
                let _ = self.release_write(ino, None);
                self.uncount(ino);
            }
            return Err(e);
        }
        if flags & libc::O_APPEND != 0 {
            if let Some(h) = self.handles.lock().by_fh.get_mut(&fh) {
                h.append = true;
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
        self.mkdir_logged(parent, name, mode, umask, uid, gid, None, &mut |_| {})
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
        self.symlink_logged(parent, link_name, target, uid, gid, None, &mut |_| {})
    }

    /// Create a hard link and return `(ino, InodeMeta)`.
    pub fn test_link_full(
        &self,
        ino: u64,
        newparent: u64,
        newname: &str,
    ) -> Result<(u64, InodeMeta), i32> {
        self.link_logged(ino, newparent, newname, &mut |_| {})
    }

    /// Flush write buffer without closing the handle.
    /// Wrapper around `flush_handle` exposed for testing.
    pub fn test_flush(&self, _ino: u64, fh: u64) -> Result<(), i32> {
        self.flush_handle(fh)
    }

    /// Release with read-only handle check (matches FUSE release behavior).
    /// The handle table says whether `fh` is a write handle.
    pub fn test_release_full(&self, ino: u64, fh: u64) -> Result<(), i32> {
        self.test_release(ino, fh)
    }

    /// Destroy: commit, optionally snapshot, and shut down WAL.
    /// Returns true if commit succeeded.
    pub fn test_destroy(&self) -> bool {
        // Write the content of handles that are still open (e.g. a forced
        // unmount), so the commit below keeps it.
        for cell in self.cells.all() {
            let mut g = self.lock_cell(&cell);
            if g.retired {
                continue;
            }
            // No request runs now: the steps keep the cell locked.
            if let Err(e) = self.finalize_locked(&cell, &mut g) {
                eprintln!("[{}][FUSE] destroy: could not write open file ino={}: errno {}", ts(), cell.ino, e);
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

    // Two write handles share one overlay (one cell). Writes through both,
    // in any order, and a release of the first handle first, keep every byte.
    #[test]
    fn two_write_handles_on_an_overlay_keep_every_write() {
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
        fs.test_release(ino, h1).unwrap(); // the cell stays for h2
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
        // Read from another "handle" (test_read uses the cell of the inode)
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
        let cost = |s: &mut OpenFileState| match &s.write_mode {
            WriteMode::Overlay(ov) => ov.cost(),
            _ => 0,
        };
        fs.ino_of_fh(fh).and_then(|ino| fs.with_cell(ino, cost)).unwrap_or(0)
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
