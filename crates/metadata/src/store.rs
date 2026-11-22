//! `DictMetadataStore` — MetadataStore whose nodes are in the chunk store.
//!
//! All state is held in-memory as `Mutex`-guarded maps; `commit()` stores the
//! changed nodes, syncs the chunk store, then writes a `RootUpdate` to the WAL
//! for crash-safe ordering.
//!
//! Locking order (always acquire in this order to prevent deadlocks):
//!   1. `inode_map`
//!   2. `io`
//!   3. `inode_data`
//!   4. `dir_data`
//!   5. `manifest_data`
//!   6. `xattr_data`
//!   7. `refcounts`

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, atomic::{AtomicU64, Ordering}};

use chunk_store::lock_stats::{self, Site};
use chunk_store::sync::{self, Rank};
use slicefs_traits::key::{ObjectKey, KEY_BYTES};
use crate::store_io::{NodeRead, NodeWrite};
use slicefs_traits::metadata::{DirEntry, InodeId, InodeMeta, MetaError, MetadataStore};

use crate::snapshot::SnapshotEntry;
use crate::store_io::StoreIo;
use crate::wal::{WalCut, WalEntry, WalError, WalStrategy};

use crate::inode::{intern_inode, load_inode};
use crate::inode_map::{InodeMap, intern_inode_map, load_inode_map};
use crate::directory::{create_dir_entries, DirList};
use crate::manifest::{intern_manifest, load_manifest};
use crate::xattr::{intern_xattrs, load_xattrs, set_xattr_entry, get_xattr_entry,
                   list_xattr_names, remove_xattr_entry};

// S_IFDIR bit mask (POSIX directory type)
const S_IFMT: u32 = 0o0170_000;

/// Byte layout of the root record that `commit` writes: fixed-width fields,
/// little-endian. Each `*_DATA` field is the key of a serialized map.
mod root_rec {
    use super::KEY_BYTES as D;
    pub const INODE_MAP: usize = 0;
    pub const ROOT_DIR_INO: usize = INODE_MAP + D;
    pub const NEXT_INO: usize = ROOT_DIR_INO + 8;
    pub const INODE_DATA: usize = NEXT_INO + 8;
    pub const DIR_DATA: usize = INODE_DATA + D;
    pub const MANIFEST_DATA: usize = DIR_DATA + D;
    pub const XATTR_DATA: usize = MANIFEST_DATA + D;
    pub const REFCOUNTS: usize = XATTR_DATA + D;
    pub const LOGICAL_BYTES: usize = REFCOUNTS + D;
    pub const INODE_COUNT: usize = LOGICAL_BYTES + 8;
    /// Total size in bytes (230 with 33-byte keys).
    pub const SIZE: usize = INODE_COUNT + 8;
}

/// One entry of a `u64` <-> digest map: a `u64` and a digest.
const MAP_ENTRY: usize = 8 + KEY_BYTES;

/// Longest directory entry name in bytes (statfs reports namelen 255).
const NAME_MAX: usize = 255;

/// When a cache holds more entries than this, its dirty entries are written
/// and the clean ones dropped.
#[cfg(not(test))]
const DIR_CACHE_MAX: usize = 16_384;
#[cfg(not(test))]
const INODE_CACHE_MAX: usize = 262_144;
// Small in unit tests, so the flush-and-evict path runs.
#[cfg(test)]
const DIR_CACHE_MAX: usize = 4;
#[cfg(test)]
const INODE_CACHE_MAX: usize = 16;
/// Dirty cache entries are written once this many have built up, so the
/// final commit (at unmount) stays small.
#[cfg(not(test))]
const DIRTY_FLUSH: usize = 1024;
#[cfg(test)]
const DIRTY_FLUSH: usize = 8;

/// Cached decoded objects plus the set of those changed since the last flush.
struct Live<T> {
    map: HashMap<u64, T>,
    dirty: std::collections::HashSet<u64>,
    /// Entries that a commit took at its cut (#108) and writes with the gate
    /// released. Until it installs their digests, the digest maps hold an
    /// older one, so these stay in the cache. A change after the cut takes
    /// the entry out (it is dirty again, and newer).
    flushing: std::collections::HashSet<u64>,
}

impl<T> Default for Live<T> {
    fn default() -> Self {
        Live { map: HashMap::new(), dirty: Default::default(), flushing: Default::default() }
    }
}

impl<T> Live<T> {
    fn remove(&mut self, ino: u64) {
        self.map.remove(&ino);
        self.dirty.remove(&ino);
        self.flushing.remove(&ino);
    }

    fn mark_dirty(&mut self, ino: u64) {
        self.dirty.insert(ino);
        self.flushing.remove(&ino);
    }

    /// Drop the entries that are already written.
    fn evict_clean(&mut self) {
        let (dirty, flushing) = (&self.dirty, &self.flushing);
        self.map.retain(|ino, _| dirty.contains(ino) || flushing.contains(ino));
    }
}

impl<T: Clone> Live<T> {
    /// The dirty entries, for a commit: they move to `flushing`.
    fn take_dirty(&mut self) -> Vec<(u64, T)> {
        let dirty: Vec<u64> = self.dirty.drain().collect();
        let mut out = Vec::with_capacity(dirty.len());
        for ino in dirty {
            if let Some(v) = self.map.get(&ino) {
                out.push((ino, v.clone()));
                self.flushing.insert(ino);
            }
        }
        out
    }

    /// A commit failed: what it took is dirty again.
    fn requeue(&mut self) {
        let flushing: Vec<u64> = self.flushing.drain().collect();
        self.dirty.extend(flushing);
    }
}

/// Points in `finish_commit` for `DictMetadataStore::test_fail_next_commit_at`.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CommitFault {
    /// The nodes are written, nothing is synced.
    BeforeSync = 1,
    /// The nodes and the content are synced, the root is not logged.
    BeforeRoot = 2,
}

/// What a commit took at its cut (`DictMetadataStore::begin_commit`): the
/// whole state that its root record holds. `finish_commit` writes it with the
/// gate released and reads no live map, so the root never has half of an
/// operation that came after the cut (#108).
pub struct PreparedCommit<'a> {
    /// One commit at a time, from the cut to the root: roots stay in order.
    _serial: sync::MutexGuard<'a, ()>,
    store: &'a DictMetadataStore,
    /// The root is logged. Else (an error, or dropped unfinished) the inodes
    /// and directories it took are dirty again.
    done: bool,
    changes: u64,
    /// `wal_fail_seq` at the cut: failures up to it end with this root.
    fail_seq: u64,
    inodes: Vec<(u64, InodeMeta)>,
    dirs: Vec<(u64, DirList)>,
    inode_map: InodeMap,
    inode_data: BTreeMap<u64, ObjectKey>,
    dir_data: BTreeMap<u64, ObjectKey>,
    manifest_data: BTreeMap<u64, ObjectKey>,
    xattr_data: BTreeMap<u64, ObjectKey>,
    refcounts: BTreeMap<ObjectKey, u64>,
    logical_bytes: u64,
    inode_count: u64,
    snapshots: Vec<SnapshotEntry>,
    wal: Option<Arc<dyn WalStrategy>>,
    cut: Option<WalCut>,
}

impl Drop for PreparedCommit<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.store.inode_live.lock().requeue();
            self.store.dir_live.lock().requeue();
        }
    }
}

fn check_name(name: &str) -> Result<(), MetaError> {
    if name.len() > NAME_MAX {
        return Err(MetaError::NameTooLong(name.len()));
    }
    Ok(())
}
const S_IFDIR: u32 = 0o0040_000;

/// Concrete `MetadataStore`; its nodes are in the chunk store (`StoreIo`).
///
/// All operations are lock-safe and `Send + Sync`.
///
/// Locking order (always acquire in this order to prevent deadlocks):
///   1. `inode_map`
///   2. `io`
///   3. `inode_data`
///   4. `dir_data`
///   5. `manifest_data`
///   6. `xattr_data`
///   7. `refcounts`
/// A lock of the store internals (rank 6, see `chunk_store::sync`).
fn lk<T>(v: T) -> sync::Mutex<T> {
    sync::Mutex::new(Rank::Store, v)
}

/// A sync run before a root is logged (see `DictMetadataStore::set_pre_root_sync`).
pub type PreRootSync = Arc<dyn Fn() -> std::io::Result<()> + Send + Sync>;

pub struct DictMetadataStore {
    /// Node store — used for all put_node/get_node calls.
    io: Arc<Mutex<StoreIo>>,
    /// Inode-number allocator + mapping table.
    inode_map: sync::Mutex<InodeMap>,
    /// Maps inode number → current inode data `ObjectKey`.
    inode_data: sync::Mutex<BTreeMap<u64, ObjectKey>>,
    /// Maps directory inode number → entry list `ObjectKey` as of the last flush.
    /// A directory in `dir_live` can be newer than its digest here.
    dir_data: sync::Mutex<BTreeMap<u64, ObjectKey>>,
    /// Decoded entry lists of the directories in use, with a dirty flag.
    /// Changes go here; `commit()` writes the dirty ones and updates `dir_data`.
    /// Without this, each create re-wrote the whole entry list (O(n) per entry).
    /// Lock order: `dir_live` before `io`.
    dir_live: sync::Mutex<Live<DirList>>,
    /// Inodes in use, with a dirty flag. `inode_data` (and `inode_map`) hold the
    /// digest as of the last flush; a dirty inode here is newer. `commit()`
    /// writes the dirty ones. Lock order: `inode_map`, `inode_live`, `io`, `inode_data`.
    inode_live: sync::Mutex<Live<InodeMeta>>,
    /// Count of mutations so far, and its value at the last commit: the
    /// commit timer commits only when they differ.
    changes: AtomicU64,
    committed_changes: AtomicU64,
    /// Maps file inode number → current manifest `ObjectKey`.
    manifest_data: sync::Mutex<BTreeMap<u64, ObjectKey>>,
    /// Maps inode number → xattr set `ObjectKey` (only for inodes with xattrs).
    xattr_data: sync::Mutex<BTreeMap<u64, ObjectKey>>,
    /// Called in `commit()` after the metadata objects are synced and before
    /// the root is logged: makes the file content (chunk store) durable.
    pre_root_sync: sync::Mutex<Option<PreRootSync>>,
    /// Reference counts for content `ObjectKey`s.
    /// Incremented when a manifest references a block; decremented on manifest replacement or inode deletion.
    refcounts: sync::Mutex<BTreeMap<ObjectKey, u64>>,
    /// Running total of logical bytes — sum of all inode `size` fields.
    ///
    /// Updated atomically in `create_inode` (+size), `update_inode` (delta),
    /// and `delete_inode` (-size).  Never goes below 0.
    logical_bytes: AtomicU64,
    /// Running count of all inodes (including the root directory).
    ///
    /// Incremented in `create_inode` and `create_directory`, decremented in
    /// `delete_inode`.  Initialized to `1` (root inode) on `new()`, recomputed
    /// from `inode_data.len()` on `load_from_root()`.
    inode_count: AtomicU64,
    /// Optional WAL strategy — routes all Dictionary mutations to durable storage.
    ///
    /// Set via `set_wal()` after construction. When None, mutations are not logged.
    wal: sync::Mutex<Option<Arc<dyn WalStrategy>>>,
    /// Count of failed appends to the write log, and the count that the last
    /// root covers. While they differ, the log may have a torn record, and
    /// records after it cannot be read. Until the next commit (which starts
    /// a new segment from the root), every `flush_wal` fails, so an fsync
    /// never reports durable what is not. A commit clears only the failures
    /// before its cut (#108).
    wal_fail_seq: AtomicU64,
    wal_fail_cleared: AtomicU64,
    /// One commit at a time (see `PreparedCommit`). Lock order: the commit
    /// gate of the file system, then this. The cut runs with this lock held
    /// and takes the file system's own locks (cells, short maps), so it has
    /// `Rank::Rename` (rank 2: below them, above the gate), not the store rank.
    commit_lock: sync::Mutex<()>,
    /// Test fault: the next `finish_commit` stops with an error at this
    /// point (see `test_fail_next_commit_at`); 0: none.
    commit_fault: std::sync::atomic::AtomicU8,
    /// Last committed root digest — updated by `commit()`, used by GC to determine live roots.
    last_root: sync::Mutex<Option<ObjectKey>>,
    /// Snapshot index by version number — O(1) lookup (FIX-03).
    snapshots_by_version: sync::Mutex<HashMap<u64, SnapshotEntry>>,
    /// Snapshot name → version mapping — O(1) name lookup (FIX-04).
    snapshots_by_name: sync::Mutex<HashMap<String, u64>>,
}

impl DictMetadataStore {
    /// Create a new store with a root directory at inode 1.
    pub fn new(io: Arc<Mutex<StoreIo>>) -> Self {
        let mut inode_map = InodeMap::new();

        // Build root inode + dir entries via put_node, then flush
        let (root_inode_digest, root_dir_digest) = {
            let mut io_guard = lock_stats::lock(&io, Site::Io);
            let mut fsa = &mut *io_guard;
            // Use current process uid/gid so the mounting user can write to root.
            // On non-unix, falls back to 0/0 (callers like `seed` update root inode
            // from the source directory metadata afterwards).
            #[cfg(unix)]
            let (uid, gid) = (unsafe { libc::getuid() }, unsafe { libc::getgid() });
            #[cfg(not(unix))]
            let (uid, gid) = (0u32, 0u32);
            let mut root_meta = InodeMeta::new_directory(1, uid, gid, S_IFDIR | 0o755);
            // A new file system starts now, not at the epoch (shown as 1969).
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            root_meta.mtime_sec = now.as_secs() as _;
            root_meta.mtime_nsec = now.subsec_nanos() as _;
            root_meta.ctime_sec = root_meta.mtime_sec;
            root_meta.ctime_nsec = root_meta.mtime_nsec;
            let root_inode_digest = intern_inode(&mut fsa, &root_meta);
            let root_dir_digest = create_dir_entries(&mut fsa, 1, 1);
            (root_inode_digest, root_dir_digest)
            // fsa drops here, flushing files to io
        };

        inode_map.insert(1, root_inode_digest);

        let mut inode_data = BTreeMap::new();
        inode_data.insert(1, root_inode_digest);

        let mut dir_data = BTreeMap::new();
        dir_data.insert(1, root_dir_digest);

        DictMetadataStore {
            io,
            inode_map: lk(inode_map),
            inode_data: lk(inode_data),
            dir_data: lk(dir_data),
            dir_live: lk(Live::default()),
            inode_live: lk(Live::default()),
            changes: AtomicU64::new(0),
            committed_changes: AtomicU64::new(0),
            manifest_data: lk(BTreeMap::new()),
            xattr_data: lk(BTreeMap::new()),
            pre_root_sync: lk(None),
            refcounts: lk(BTreeMap::new()),
            logical_bytes: AtomicU64::new(0),
            inode_count: AtomicU64::new(1),
            wal: lk(None).timed(Site::Wal),
            wal_fail_seq: AtomicU64::new(0),
            wal_fail_cleared: AtomicU64::new(0),
            commit_lock: sync::Mutex::new(Rank::Rename, ()),
            commit_fault: std::sync::atomic::AtomicU8::new(0),
            last_root: lk(None),
            snapshots_by_version: lk(HashMap::new()),
            snapshots_by_name: lk(HashMap::new()),
        }
    }
}

// Note: Default is no longer impl'd — DictMetadataStore::new() requires an io: Arc<Mutex<StoreIo>>

impl DictMetadataStore {
    /// Access the node store (for content operations like `put_node`
    /// via `&mut *io_guard`).
    ///
    /// # Warning
    /// Do NOT hold this lock while calling any other `DictMetadataStore` method.
    /// Those methods acquire the same lock internally; double-locking will deadlock.
    pub fn io(&self) -> &Arc<Mutex<StoreIo>> {
        &self.io
    }

    /// Increment the reference count for `digest` by 1.
    ///
    /// Creates the entry (starting at 1) if it does not yet exist.
    ///
    /// Saturates at `u64::MAX` — blocks at `u64::MAX` are *immortal* and will
    /// never be garbage-collected.  A tracing warning is emitted the first time
    /// a block reaches saturation.
    pub fn increment_refcount(&self, digest: &ObjectKey) {
        self.changes.fetch_add(1, Ordering::Relaxed);
        let mut rc = self.refcounts.lock();
        let val = rc.entry(*digest).or_insert(0);
        if *val == u64::MAX {
            // Already saturated — no-op.
            return;
        }
        *val += 1;
        if *val == u64::MAX {
            tracing::warn!(
                "refcount saturated for block — block is now immortal and will not be GC'd"
            );
        }
    }

    /// Decrement the reference count for `digest` by 1.
    ///
    /// Removes the entry entirely when the count reaches 0.
    /// Does nothing if `digest` is not tracked.
    ///
    /// If the current count is `u64::MAX` (saturated / immortal), this is a
    /// no-op — immortal blocks are never freed.
    pub fn decrement_refcount(&self, digest: &ObjectKey) {
        self.changes.fetch_add(1, Ordering::Relaxed);
        let mut rc = self.refcounts.lock();
        if let Some(count) = rc.get_mut(digest) {
            // Saturated blocks are immortal — decrement is a no-op.
            if *count == u64::MAX {
                return;
            }
            if *count <= 1 {
                rc.remove(digest);
            } else {
                *count -= 1;
            }
        }
    }

    /// Return the current reference count for `digest`, or `0` if not tracked.
    pub fn get_refcount(&self, digest: &ObjectKey) -> u64 {
        let rc = self.refcounts.lock();
        rc.get(digest).copied().unwrap_or(0)
    }

    /// Return the number of blocks whose reference count has saturated at `u64::MAX`.
    ///
    /// Saturated blocks are *immortal* — they will never be garbage-collected.
    /// A non-zero count here indicates the GC dead-letter queue has accumulated
    /// blocks; this is surfaced as a warning in `slicefs scrub`.
    pub fn saturated_refcount_count(&self) -> usize {
        let rc = self.refcounts.lock();
        rc.values().filter(|&&v| v == u64::MAX).count()
    }

    /// Return the current logical byte total — the sum of all inode `size` fields.
    ///
    /// This is the "logical" space consumed by the filesystem, before deduplication.
    /// Dividing logical_bytes by (dict.len() * 92) gives the dedup ratio.
    pub fn logical_bytes(&self) -> u64 {
        self.logical_bytes.load(Ordering::Relaxed)
    }

    /// Return the current inode count (total number of live inodes including root).
    ///
    /// Updated atomically in `create_inode`/`create_directory` (+1) and
    /// `delete_inode` (-1, saturating).  Recomputed from `inode_data.len()` after
    /// `load_from_root()`.
    pub fn inode_count(&self) -> u64 {
        self.inode_count.load(Ordering::Relaxed)
    }

    /// Set the WAL strategy. Must be called before any mutations if durability is desired.
    ///
    /// Replaces any previously set WAL (the old one is dropped; not flushed).
    pub fn set_wal(&mut self, wal: Box<dyn WalStrategy>) {
        *self.wal.lock() = Some(Arc::from(wal));
    }

    /// Set the sync that `commit()` runs before it logs a root (see
    /// `pre_root_sync`).
    pub fn set_pre_root_sync(&self, f: PreRootSync) {
        *self.pre_root_sync.lock() = Some(f);
    }

    /// Flush all buffered WAL mutations to disk and call sync_all.
    ///
    /// This is called by the fsync FUSE callback to ensure durability.
    /// Returns Ok(()) when no WAL is configured.
    pub fn flush_wal(&self) -> Result<(), WalError> {
        if self.wal_fail_seq.load(Ordering::SeqCst) != self.wal_fail_cleared.load(Ordering::SeqCst) {
            return Err(WalError::Io(std::io::Error::other(
                "an earlier write-log append failed; nothing is durable until the next commit",
            )));
        }
        if let Some(w) = self.wal_handle() {
            w.flush_and_sync()?;
        }
        Ok(())
    }

    /// Flush remaining mutations and shut down the WAL cleanly.
    ///
    /// Should be called before dropping the store (e.g., in `destroy()`).
    pub fn shutdown_wal(&self) -> Result<(), WalError> {
        if let Some(ref w) = *self.wal.lock() {
            w.shutdown()?;
        }
        Ok(())
    }

    /// Log a single WAL entry if a WAL strategy is configured.
    ///
    /// IMPORTANT: Do NOT hold any other Mutex when calling this — WAL I/O
    /// may block and holding dict/inode_map locks would cause deadlocks.
    /// Continue the WAL in a new segment that starts with `root` and every
    /// snapshot record, and delete the older segments (#20). A failure leaves
    /// the current segment in use, which is still correct (only larger).
    fn rotate_wal(&self, root: ObjectKey) {
        if let Some(w) = self.wal_handle() {
            let fails = self.wal_fail_seq.load(Ordering::SeqCst);
            let mut seed = vec![WalEntry::RootUpdate { root }];
            for s in self.list_snapshots() {
                seed.push(WalEntry::Snapshot { version: s.version, root: s.root, created_at: s.created_at, name: s.name });
            }
            match w.rotate(&seed) {
                // A new segment that starts with the root: the log is whole.
                Ok(()) => { self.wal_fail_cleared.fetch_max(fails, Ordering::SeqCst); }
                Err(e) => eprintln!("WAL rotation failed (the log keeps growing in the current segment): {e}"),
            }
        }
    }

    /// Make the next new inode number at least `ino` (never lower). Replay
    /// of the write log calls this before each create: a request that failed
    /// after it took an inode number (for example EEXIST) logged nothing, so
    /// replay skips that number the same way the live mount did.
    pub fn advance_next_ino(&self, ino: InodeId) {
        let mut m = self.inode_map.lock();
        if m.next_ino() < ino {
            m.set_next_ino(ino);
        }
    }

    /// Append a file system request to the write log (#20). Durable after the
    /// next `flush_wal` (fsync) or commit; a commit makes it part of a root.
    pub fn log_op(&self, bytes: Vec<u8>) {
        self.log_wal_entry(&WalEntry::Op { bytes });
    }

    fn log_wal_entry(&self, entry: &WalEntry) {
        if let Some(ref w) = *self.wal.lock() {
            // The in-memory state is already correct. A failed append is
            // reported by every later flush_wal (fsync) until the next
            // commit rotates the log (#20).
            if let Err(e) = w.log_mutation(entry) {
                eprintln!("write log append failed (fsync fails until the next commit): {e}");
                self.wal_fail_seq.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    /// Return the last committed root digest, or `None` if `commit()` has not been called yet.
    ///
    /// Used by the background GC thread to determine which root to use as the live-set anchor.
    /// Point directory `dir_ino`'s `..` entry at `parent_ino` (directory move).
    pub fn set_parent(&self, dir_ino: InodeId, parent_ino: InodeId) -> Result<(), MetaError> {
        self.changes.fetch_add(1, Ordering::Relaxed);
        self.with_dir(dir_ino, |d| {
            d.set_dotdot(parent_ino);
            Ok(((), true))
        })
    }

    /// Run `f` on the cached entry list of directory `ino` (read from the store
    /// on first use). `f` returns `(result, changed)`; a changed list is marked
    /// dirty and written by the next flush.
    fn with_dir<R>(
        &self,
        ino: InodeId,
        f: impl FnOnce(&mut DirList) -> Result<(R, bool), MetaError>,
    ) -> Result<R, MetaError> {
        let mut live = self.dir_live.lock();
        if !live.map.contains_key(&ino) {
            let digest = self.dir_data.lock().get(&ino).copied()
                .ok_or(MetaError::NotADirectory(ino))?;
            let list = {
                let mut io_guard = lock_stats::lock(&self.io, Site::Io);
                DirList::load(&mut *io_guard, &digest)?
            };
            live.map.insert(ino, list);
        }
        let (r, changed) = f(live.map.get_mut(&ino).expect("inserted above"))?;
        if changed {
            live.mark_dirty(ino);
        }
        let (flush, full) = (live.dirty.len() >= DIRTY_FLUSH, live.map.len() > DIR_CACHE_MAX);
        drop(live);
        if flush || full {
            self.flush_dirs();
        }
        if full {
            self.dir_live.lock().evict_clean();
        }
        Ok(r)
    }

    /// Current metadata of `ino`: the cached copy, or read from the store.
    fn load_meta(&self, ino: InodeId) -> Result<InodeMeta, MetaError> {
        if let Some(m) = self.inode_live.lock().map.get(&ino) {
            return Ok(m.clone());
        }
        let digest = self.inode_data.lock().get(&ino).copied()
            .ok_or(MetaError::NotFound(ino))?;
        let meta = {
            let mut io_guard = lock_stats::lock(&self.io, Site::Io);
            load_inode(&mut *io_guard, &digest)?
        };
        let full = {
            let mut live = self.inode_live.lock();
            // A concurrent update may have cached a newer copy meanwhile; keep it.
            live.map.entry(ino).or_insert_with(|| meta.clone());
            live.map.len() > INODE_CACHE_MAX
        };
        if full {
            self.flush_inodes();
            self.inode_live.lock().evict_clean();
        }
        Ok(meta)
    }

    /// Cache `meta` as the new (dirty) state of its inode.
    fn store_meta(&self, meta: &InodeMeta) {
        let (flush, full) = {
            let mut live = self.inode_live.lock();
            live.map.insert(meta.ino, meta.clone());
            live.mark_dirty(meta.ino);
            (live.dirty.len() >= DIRTY_FLUSH, live.map.len() > INODE_CACHE_MAX)
        };
        if flush || full {
            self.flush_inodes();
        }
        if full {
            self.inode_live.lock().evict_clean();
        }
    }

    /// Write every dirty cached inode; record its digest in `inode_data` and `inode_map`.
    fn flush_inodes(&self) {
        let mut inode_map = self.inode_map.lock();
        let mut live = self.inode_live.lock();
        if live.dirty.is_empty() {
            return;
        }
        let mut io_guard = lock_stats::lock(&self.io, Site::Io);
        let mut fsa = &mut *io_guard;
        let mut inode_data = self.inode_data.lock();
        let live = &mut *live;
        for ino in live.dirty.drain() {
            if let Some(meta) = live.map.get(&ino) {
                let digest = intern_inode(&mut fsa, meta);
                inode_data.insert(ino, digest);
                inode_map.insert(ino, digest);
            }
        }
    }

    /// True if `ino` is a directory known to the store.
    fn is_dir(&self, ino: InodeId) -> bool {
        self.dir_live.lock().map.contains_key(&ino)
            || self.dir_data.lock().contains_key(&ino)
    }

    /// Write every dirty cached directory and record its digest in `dir_data`.
    fn flush_dirs(&self) {
        let mut live = self.dir_live.lock();
        if live.dirty.is_empty() {
            return;
        }
        let mut io_guard = lock_stats::lock(&self.io, Site::Io);
        let mut fsa = &mut *io_guard;
        let mut dir_data = self.dir_data.lock();
        let live = &mut *live;
        for ino in live.dirty.drain() {
            if let Some(list) = live.map.get(&ino) {
                dir_data.insert(ino, list.intern(&mut fsa));
            }
        }
    }

    /// The write log, if any (a clone: no lock is held while it works).
    fn wal_handle(&self) -> Option<Arc<dyn WalStrategy>> {
        self.wal.lock().as_ref().map(Arc::clone)
    }

    /// Make the next `finish_commit` stop with an error at `at`, as a crash
    /// there would (for crash tests: nothing after that point reaches disk).
    #[doc(hidden)]
    pub fn test_fail_next_commit_at(&self, at: CommitFault) {
        self.commit_fault.store(at as u8, Ordering::SeqCst);
    }

    fn injected_fault(&self, at: CommitFault) -> Result<(), MetaError> {
        if self.commit_fault.compare_exchange(at as u8, 0, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
            return Err(MetaError::Corrupted(format!("injected commit fault {at:?}")));
        }
        Ok(())
    }

    /// Wait until no commit is between `begin_commit` and the end of
    /// `finish_commit`.
    pub fn wait_for_commit(&self) {
        drop(self.commit_lock.lock());
    }

    /// First half of a commit (#108), run while no request changes the store
    /// (the file system holds its commit gate exclusively): take the dirty
    /// inodes and directories and a copy of every map, and cut the write log
    /// (`WalStrategy::cut`), so requests after this go to the next
    /// generation. Fast: no node is written and nothing is synced. Waits for
    /// a commit still in `finish_commit`.
    pub fn begin_commit(&self) -> Result<PreparedCommit<'_>, MetaError> {
        let serial = self.commit_lock.lock();
        let wal = self.wal_handle();
        // The log first: if it cannot cut, nothing has changed.
        let cut = match &wal {
            Some(w) => w.cut().map_err(|e| MetaError::Corrupted(format!("commit: write-log cut failed: {e}")))?,
            None => None,
        };
        if cut.is_some_and(|c| c.old_failed) {
            self.wal_fail_seq.fetch_add(1, Ordering::SeqCst);
        }
        // Failures up to here are of records before the cut: the root of
        // this commit holds their requests. Later ones are not cleared by it.
        let fail_seq = self.wal_fail_seq.load(Ordering::SeqCst);
        let changes = self.changes.load(Ordering::Relaxed);
        let dirs = self.dir_live.lock().take_dirty();
        let inode_map = self.inode_map.lock().clone();
        let inodes = self.inode_live.lock().take_dirty();
        let inode_data = self.inode_data.lock().clone();
        let dir_data = self.dir_data.lock().clone();
        let manifest_data = self.manifest_data.lock().clone();
        let xattr_data = self.xattr_data.lock().clone();
        let refcounts = self.refcounts.lock().clone();
        Ok(PreparedCommit {
            _serial: serial,
            store: self,
            done: false,
            changes,
            fail_seq,
            inodes,
            dirs,
            inode_map,
            inode_data,
            dir_data,
            manifest_data,
            xattr_data,
            refcounts,
            logical_bytes: self.logical_bytes.load(Ordering::Relaxed),
            inode_count: self.inode_count.load(Ordering::Relaxed),
            snapshots: self.list_snapshots(),
            wal,
            cut,
        })
    }

    /// Second half of a commit (#108); requests may run meanwhile. Writes
    /// the nodes of `p`, syncs the chunk store, then logs the root
    /// (`WalStrategy::install_root`). Takes no store lock across a write
    /// or a sync of more than one node. On an error, the inodes and
    /// directories that `p` took are dirty again.
    pub fn finish_commit(&self, mut p: PreparedCommit<'_>) -> Result<ObjectKey, MetaError> {
        let r = self.finish_inner(&mut p);
        p.done = r.is_ok();
        r
    }

    fn finish_inner(&self, p: &mut PreparedCommit<'_>) -> Result<ObjectKey, MetaError> {
        let cs = Arc::clone(lock_stats::lock(&self.io, Site::Io).store());
        let mut w = crate::store_io::NodeWriter::new(Arc::clone(&cs));
        // Write the changed directories and inodes, so the digest maps are current.
        let mut new_inodes = Vec::with_capacity(p.inodes.len());
        for (ino, meta) in &p.inodes {
            let d = intern_inode(&mut w, meta);
            p.inode_data.insert(*ino, d);
            p.inode_map.insert(*ino, d);
            new_inodes.push((*ino, d));
        }
        let mut new_dirs = Vec::with_capacity(p.dirs.len());
        for (ino, list) in &p.dirs {
            let d = list.intern(&mut w);
            p.dir_data.insert(*ino, d);
            new_dirs.push((*ino, d));
        }

        // Serialize the state into a root record (layout: `root_rec`):
        //   inode_map, root_dir_ino (u64), next_ino (u64), the keys of the
        //   inode, dir, manifest, xattr and refcount maps, logical_bytes (u64)
        //   and inode_count (u64).
        let inode_map_digest = intern_inode_map(&mut w, &p.inode_map);
        let next_ino = p.inode_map.next_ino();
        let inode_data_digest = intern_u64_digest_map(&mut w, &p.inode_data);
        let dir_data_digest = intern_u64_digest_map(&mut w, &p.dir_data);
        let manifest_data_digest = intern_u64_digest_map(&mut w, &p.manifest_data);
        let xattr_data_digest = intern_u64_digest_map(&mut w, &p.xattr_data);
        let refcount_data_digest = intern_key_u64_map(&mut w, &p.refcounts);

        let mut root_bytes = Vec::with_capacity(root_rec::SIZE);
        put_digest(&mut root_bytes, &inode_map_digest);
        // root_dir_ino: always 1
        root_bytes.extend_from_slice(&1u64.to_le_bytes());
        root_bytes.extend_from_slice(&next_ino.to_le_bytes());
        for d in [&inode_data_digest, &dir_data_digest, &manifest_data_digest,
                  &xattr_data_digest, &refcount_data_digest] {
            put_digest(&mut root_bytes, d);
        }
        // logical_bytes + inode_count: without them, loading would read every
        // inode object just to sum the sizes (O(files) reads).
        root_bytes.extend_from_slice(&p.logical_bytes.to_le_bytes());
        root_bytes.extend_from_slice(&p.inode_count.to_le_bytes());
        assert_eq!(root_bytes.len(), root_rec::SIZE, "root record size");
        let root_digest = w.put_node(&root_bytes);
        w.result().map_err(|e| MetaError::Corrupted(format!("commit: node write failed: {e}")))?;
        self.injected_fault(CommitFault::BeforeSync)?;

        // The nodes are stored: the caches may forget the entries that were
        // not changed again since the cut.
        {
            let mut inode_map = self.inode_map.lock();
            let mut live = self.inode_live.lock();
            let mut inode_data = self.inode_data.lock();
            for (ino, d) in new_inodes {
                if live.flushing.remove(&ino) {
                    inode_data.insert(ino, d);
                    inode_map.insert(ino, d);
                }
            }
        }
        {
            let mut live = self.dir_live.lock();
            let mut dir_data = self.dir_data.lock();
            for (ino, d) in new_dirs {
                if live.flushing.remove(&ino) {
                    dir_data.insert(ino, d);
                }
            }
        }

        // Make every object written since the last commit durable before the
        // root that refers to them is logged: after that, log rotation deletes
        // the segments whose records hold the same data (#20). A put error of
        // a request since the last commit fails the commit too.
        lock_stats::lock(&self.io, Site::Io).begin_commit_sync()
            .map_err(|e| MetaError::Corrupted(format!("commit: object sync failed: {e}")))?;
        cs.sync().map_err(|e| MetaError::Corrupted(format!("commit: object sync failed: {e}")))?;
        // The file content that the root refers to must be durable too.
        let pre = self.pre_root_sync.lock().clone();
        if let Some(f) = pre {
            f().map_err(|e| MetaError::Corrupted(format!("commit: content sync failed: {e}")))?;
        }

        self.injected_fault(CommitFault::BeforeRoot)?;
        // Log the root only now, after all nodes are durable (crash-safe
        // ordering): a crash before it leaves the last root plus the log.
        if let Some(wal) = &p.wal {
            let mut seed = vec![WalEntry::RootUpdate { root: root_digest }];
            for s in &p.snapshots {
                seed.push(WalEntry::Snapshot { version: s.version, root: s.root, created_at: s.created_at, name: s.name.clone() });
            }
            match &p.cut {
                Some(cut) => {
                    wal.install_root(cut, &seed)
                        .map_err(|e| MetaError::Corrupted(format!("commit: root write failed: {e}")))?;
                    self.wal_fail_cleared.fetch_max(p.fail_seq, Ordering::SeqCst);
                }
                // A log with no generations (`cut` gave None): the root goes
                // at its end, then the log rotates.
                None => {
                    self.log_wal_entry(&WalEntry::RootUpdate { root: root_digest });
                    match wal.rotate(&seed) {
                        Ok(()) => { self.wal_fail_cleared.fetch_max(p.fail_seq, Ordering::SeqCst); }
                        Err(e) => eprintln!("WAL rotation failed (the log keeps growing in the current segment): {e}"),
                    }
                }
            }
        }

        // Update last_root so background GC can use it
        *self.last_root.lock() = Some(root_digest);
        self.committed_changes.store(p.changes, Ordering::Relaxed);
        Ok(root_digest)
    }

    /// True if a mutation happened after the last `commit()`.
    pub fn has_uncommitted_changes(&self) -> bool {
        self.changes.load(Ordering::Relaxed) != self.committed_changes.load(Ordering::Relaxed)
    }

    pub fn current_root(&self) -> Option<ObjectKey> {
        *self.last_root.lock()
    }

    /// Load a snapshot list after store reconstruction from segment replay.
    ///
    /// Called by `mount.rs::load_store` after `load_store_from_segments` returns
    /// the snapshot list. Replaces whatever is in memory (typically empty).
    pub fn set_snapshots(&mut self, snapshots: Vec<SnapshotEntry>) {
        let mut by_ver = self.snapshots_by_version.lock();
        let mut by_name = self.snapshots_by_name.lock();
        *by_ver = snapshots.iter().map(|s| (s.version, s.clone())).collect();
        *by_name = snapshots
            .iter()
            .filter_map(|s| s.name.as_ref().map(|n| (n.clone(), s.version)))
            .collect();
    }

    /// Create a snapshot of the current committed state.
    ///
    /// Calls `commit()` first to flush all in-memory mutations into the Dictionary,
    /// then writes a `SnapshotRecord` to the WAL, and returns the new `SnapshotEntry`.
    ///
    /// Version numbers auto-increment from the max existing version + 1 (starting at 1).
    pub fn create_snapshot(&self, name: Option<String>) -> Result<SnapshotEntry, MetaError> {
        // Flush in-memory state to get a stable root.
        let root = self.commit()?;

        // Compute next version — O(1) from HashMap keys.
        let version = {
            let by_ver = self.snapshots_by_version.lock();
            by_ver.keys().max().copied().unwrap_or(0) + 1
        };

        // Capture creation timestamp.
        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let entry = SnapshotEntry { version, name: name.clone(), root, created_at };

        // Write SnapshotRecord to WAL (same path as commit writes RootUpdate).
        self.log_wal_entry(&WalEntry::Snapshot {
            version,
            root,
            created_at,
            name,
        });

        // Insert into both indexes.
        {
            let mut by_ver = self.snapshots_by_version.lock();
            by_ver.insert(entry.version, entry.clone());
        }
        if let Some(ref n) = entry.name {
            let mut by_name = self.snapshots_by_name.lock();
            by_name.insert(n.clone(), entry.version);
        }

        Ok(entry)
    }

    /// Return all snapshots sorted by version ascending.
    pub fn list_snapshots(&self) -> Vec<SnapshotEntry> {
        let by_ver = self.snapshots_by_version.lock();
        let mut snaps: Vec<SnapshotEntry> = by_ver.values().cloned().collect();
        snaps.sort_by_key(|s| s.version);
        snaps
    }

    /// Find a snapshot by version number or name. O(1) lookup (FIX-03, FIX-04).
    ///
    /// - If `reference` parses as `u64`, search by version using HashMap.
    /// - Otherwise search by name using the name→version HashMap.
    ///
    /// Returns the matching entry, or `None`.
    pub fn find_snapshot(&self, reference: &str) -> Option<SnapshotEntry> {
        if let Ok(version) = reference.parse::<u64>() {
            self.snapshots_by_version.lock().get(&version).cloned()
        } else {
            let by_name = self.snapshots_by_name.lock();
            let version = *by_name.get(reference)?;
            drop(by_name);
            self.snapshots_by_version.lock().get(&version).cloned()
        }
    }

    /// Return all roots that GC must treat as live-set anchors.
    ///
    /// Includes every snapshot root plus the current committed root (if any).
    /// Used by both background GC and offline `slicefs gc`.
    pub fn snapshot_roots(&self) -> Vec<ObjectKey> {
        let by_ver = self.snapshots_by_version.lock();
        let mut roots: Vec<ObjectKey> = by_ver.values().map(|s| s.root).collect();
        drop(by_ver);
        if let Some(root) = self.current_root() {
            roots.push(root);
        }
        roots
    }

    /// Write a `RootUpdate` WAL entry for an already-committed root.
    ///
    /// Used by `snapshot switch` to redirect the live root to a snapshot's root
    /// without re-serializing all inode data. On next mount, segment replay will
    /// see this `RootUpdate` as the last root, so `load_store_from_segments` will
    /// return this root and the store will be reconstructed from the snapshot state.
    ///
    /// Also updates `last_root` so `current_root()` reflects the change immediately.
    pub fn commit_root(&self, root: ObjectKey) -> Result<(), MetaError> {
        let _serial = self.commit_lock.lock();
        *self.last_root.lock() = Some(root);
        self.log_wal_entry(&WalEntry::RootUpdate { root });
        self.rotate_wal(root);
        Ok(())
    }
}

impl MetadataStore for DictMetadataStore {
    fn create_inode(&self, meta: &InodeMeta) -> Result<InodeId, MetaError> {
        self.changes.fetch_add(1, Ordering::Relaxed);
        let mut inode_map = self.inode_map.lock();
        let ino = inode_map.allocate_ino();
        let mut full_meta = meta.clone();
        full_meta.ino = ino;
        drop(inode_map);
        // The inode is written at the next flush; until then only the cache has it.
        self.store_meta(&full_meta);

        // Track logical bytes: add this inode's size to running total.
        if full_meta.size > 0 {
            self.logical_bytes.fetch_add(full_meta.size, Ordering::Relaxed);
        }

        // Track inode count.
        self.inode_count.fetch_add(1, Ordering::Relaxed);

        Ok(ino)
    }

    fn get_inode(&self, ino: InodeId) -> Result<InodeMeta, MetaError> {
        self.load_meta(ino)
    }

    fn update_inode(&self, meta: &InodeMeta) -> Result<(), MetaError> {
        self.changes.fetch_add(1, Ordering::Relaxed);
        let ino = meta.ino;

        // Capture old size before replacing the digest, so we can adjust logical_bytes.
        let old_size = self.load_meta(ino)?.size;
        self.store_meta(meta);

        // Adjust logical_bytes by the size delta (saturating arithmetic prevents underflow).
        let new_size = meta.size;
        if new_size > old_size {
            self.logical_bytes.fetch_add(new_size - old_size, Ordering::Relaxed);
        } else if old_size > new_size {
            self.logical_bytes.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                Some(cur.saturating_sub(old_size - new_size))
            }).ok();
        }

        Ok(())
    }

    fn delete_inode(&self, ino: InodeId) -> Result<(), MetaError> {
        self.changes.fetch_add(1, Ordering::Relaxed);
        // Capture size before removing so we can decrement logical_bytes.
        let size = self.load_meta(ino)?.size;

        self.inode_live.lock().remove(ino);
        self.inode_data.lock().remove(&ino);
        self.inode_map.lock().remove(ino);
        // Drop every per-inode map entry: a leftover manifest/xattr/dir entry
        // would keep the deleted content reachable (never garbage-collected).
        self.manifest_data.lock().remove(&ino);
        self.xattr_data.lock().remove(&ino);
        self.dir_live.lock().remove(ino);
        self.dir_data.lock().remove(&ino);

        // Decrement logical_bytes by the deleted inode's size.
        if size > 0 {
            self.logical_bytes.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                Some(cur.saturating_sub(size))
            }).ok();
        }

        // Decrement inode count (saturating to avoid underflow).
        self.inode_count.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |c| {
            Some(c.saturating_sub(1))
        }).ok();

        Ok(())
    }

    fn create_directory(
        &self,
        parent_ino: InodeId,
        name: &str,
        meta: &InodeMeta,
    ) -> Result<InodeId, MetaError> {
        self.changes.fetch_add(1, Ordering::Relaxed);
        // Validate parent exists and is a directory
        check_name(name)?;
        let parent_meta = self.get_inode(parent_ino)?;
        if parent_meta.mode & S_IFMT != S_IFDIR {
            return Err(MetaError::NotADirectory(parent_ino));
        }
        if !self.is_dir(parent_ino) {
            return Err(MetaError::NotADirectory(parent_ino));
        }

        // Allocate new inode number
        let mut inode_map = self.inode_map.lock();
        let ino = inode_map.allocate_ino();
        let mut dir_meta = meta.clone();
        dir_meta.ino = ino;
        // Ensure mode has directory type bit
        if dir_meta.mode & S_IFMT != S_IFDIR {
            dir_meta.mode |= S_IFDIR;
        }

        // Add the name to the parent first: if it already exists, nothing changed.
        self.with_dir(parent_ino, |d| d.add(name, ino).map(|()| ((), true)))?;

        drop(inode_map);
        self.store_meta(&dir_meta);
        {
            let mut live = self.dir_live.lock();
            live.map.insert(ino, DirList::new_dir(ino, parent_ino));
            live.mark_dirty(ino);
        }

        // Increment parent nlinks (for the .. backlink from new subdir)
        let mut parent_meta = self.get_inode(parent_ino)?;
        parent_meta.nlinks += 1;
        self.update_inode(&parent_meta)?;

        // Track inode count for the new directory inode.
        self.inode_count.fetch_add(1, Ordering::Relaxed);

        Ok(ino)
    }

    fn list_directory(&self, ino: InodeId) -> Result<Vec<DirEntry>, MetaError> {
        self.with_dir(ino, |d| Ok((d.list(), false)))
    }

    fn lookup(&self, parent_ino: InodeId, name: &str) -> Result<InodeId, MetaError> {
        check_name(name)?;
        self.with_dir(parent_ino, |d| Ok((d.lookup(name), false)))?
            .map_err(|_| MetaError::NotFound(0))
    }

    fn link(&self, parent_ino: InodeId, name: &str, ino: InodeId) -> Result<(), MetaError> {
        self.changes.fetch_add(1, Ordering::Relaxed);
        check_name(name)?;
        self.with_dir(parent_ino, |d| d.add(name, ino).map(|()| ((), true)))
    }

    fn unlink(&self, parent_ino: InodeId, name: &str) -> Result<(), MetaError> {
        self.changes.fetch_add(1, Ordering::Relaxed);
        self.with_dir(parent_ino, |d| d.remove(name).map(|()| ((), true)))
    }

    fn set_manifest(&self, ino: InodeId, blocks: &[ObjectKey]) -> Result<(), MetaError> {
        self.changes.fetch_add(1, Ordering::Relaxed);
        let digest = {
            let mut io_guard = lock_stats::lock(&self.io, Site::Io);
            let mut fsa = &mut *io_guard;
            intern_manifest(&mut fsa, blocks)
        };
        self.manifest_data.lock().insert(ino, digest);
        Ok(())
    }

    fn get_manifest(&self, ino: InodeId) -> Result<Vec<ObjectKey>, MetaError> {
        let manifest_data = self.manifest_data.lock();
        let digest = manifest_data.get(&ino).copied().ok_or(MetaError::NotFound(ino))?;
        drop(manifest_data);

        let mut io_guard = lock_stats::lock(&self.io, Site::Io);
        load_manifest(&mut *io_guard, &digest)
    }

    fn set_xattr(&self, ino: InodeId, name: &str, value: &[u8]) -> Result<(), MetaError> {
        self.changes.fetch_add(1, Ordering::Relaxed);
        // Load existing xattrs for this inode (or empty vec if none yet)
        let mut xattrs = {
            let xattr_data = self.xattr_data.lock();
            if let Some(digest) = xattr_data.get(&ino).copied() {
                drop(xattr_data);
                let mut io_guard = lock_stats::lock(&self.io, Site::Io);
                load_xattrs(&mut *io_guard, &digest)?
            } else {
                vec![]
            }
        };

        set_xattr_entry(&mut xattrs, name, value);

        let new_digest = {
            let mut io_guard = lock_stats::lock(&self.io, Site::Io);
            let mut fsa = &mut *io_guard;
            intern_xattrs(&mut fsa, &xattrs)
        };

        self.xattr_data.lock().insert(ino, new_digest);
        Ok(())
    }

    fn get_xattr(&self, ino: InodeId, name: &str) -> Result<Vec<u8>, MetaError> {
        let xattr_data = self.xattr_data.lock();
        let digest = match xattr_data.get(&ino).copied() {
            Some(d) => d,
            None => return Err(MetaError::NotFound(ino)),
        };
        drop(xattr_data);

        let mut io_guard = lock_stats::lock(&self.io, Site::Io);
        let xattrs = load_xattrs(&mut *io_guard, &digest)?;
        drop(io_guard);

        get_xattr_entry(&xattrs, name).ok_or(MetaError::NotFound(ino))
    }

    fn list_xattrs(&self, ino: InodeId) -> Result<Vec<String>, MetaError> {
        let xattr_data = self.xattr_data.lock();
        let digest = match xattr_data.get(&ino).copied() {
            Some(d) => d,
            None => return Ok(vec![]),
        };
        drop(xattr_data);

        let mut io_guard = lock_stats::lock(&self.io, Site::Io);
        let xattrs = load_xattrs(&mut *io_guard, &digest)?;
        drop(io_guard);

        Ok(list_xattr_names(&xattrs))
    }

    fn remove_xattr(&self, ino: InodeId, name: &str) -> Result<(), MetaError> {
        self.changes.fetch_add(1, Ordering::Relaxed);
        let xattr_data = self.xattr_data.lock();
        let digest = match xattr_data.get(&ino).copied() {
            Some(d) => d,
            None => return Err(MetaError::NotFound(ino)),
        };
        drop(xattr_data);

        let mut xattrs = {
            let mut io_guard = lock_stats::lock(&self.io, Site::Io);
            load_xattrs(&mut *io_guard, &digest)?
        };

        if !remove_xattr_entry(&mut xattrs, name) {
            return Err(MetaError::NotFound(ino));
        }

        let new_digest = {
            let mut io_guard = lock_stats::lock(&self.io, Site::Io);
            let mut fsa = &mut *io_guard;
            intern_xattrs(&mut fsa, &xattrs)
        };

        self.xattr_data.lock().insert(ino, new_digest);
        Ok(())
    }

    fn root_ino(&self) -> InodeId {
        1
    }

    fn commit(&self) -> Result<ObjectKey, MetaError> {
        // Changes made after the cut are left for the next commit.
        let p = self.begin_commit()?;
        self.finish_commit(p)
    }
}

/// Roots of every stored object that the metadata at `root` reads back: the
/// root record, its maps, each inode, directory list and xattr set, each file
/// manifest, and each file root in the manifests. The GC mark phase (#25)
/// walks the object tree of each of them.
/// The metadata nodes that the root record `root` refers to:
/// the record itself and its maps, the inode, directory and xattr nodes, and
/// the manifest nodes. File content keys are not here; see [`content_keys`].
pub fn referenced_roots(
    io: &mut impl crate::store_io::NodeRead,
    root: &ObjectKey,
) -> Result<Vec<ObjectKey>, MetaError> {
    referenced(io, root).map(|(meta, _)| meta)
}

/// The file content keys (in the chunk store) that the root record `root`
/// refers to: the entries of the manifests.
pub fn content_keys(io: &mut impl crate::store_io::NodeRead, root: &ObjectKey) -> Result<Vec<ObjectKey>, MetaError> {
    referenced(io, root).map(|(_, content)| content)
}

pub(crate) fn referenced(
    io: &mut impl crate::store_io::NodeRead,
    root: &ObjectKey,
) -> Result<(Vec<ObjectKey>, Vec<ObjectKey>), MetaError> {
    let bytes = (io).get_node(root)
        .map_err(|e| crate::store_io::node_err("root node", root, e))?;
    check_root_len(bytes.len())?;
    let at = |o: usize| parse_key(&bytes[o..o + KEY_BYTES]);
    let (inodes, dirs, manifests, xattrs) = (
        at(root_rec::INODE_DATA), at(root_rec::DIR_DATA),
        at(root_rec::MANIFEST_DATA), at(root_rec::XATTR_DATA),
    );
    let mut out = vec![*root, at(root_rec::INODE_MAP), inodes, dirs, manifests, xattrs,
                       at(root_rec::REFCOUNTS)];
    for map in [inodes, dirs, xattrs] {
        out.extend(load_u64_digest_map(io, &map)?.into_values());
    }
    let mut content = Vec::new();
    for manifest in load_u64_digest_map(io, &manifests)?.into_values() {
        out.push(manifest);
        content.extend(load_manifest(io, &manifest)?);
    }
    Ok((out, content))
}

/// Error unless `len` is the size of a root record.
fn check_root_len(len: usize) -> Result<(), MetaError> {
    if len != root_rec::SIZE {
        return Err(MetaError::Corrupted(format!(
            "root record: expected {} bytes, got {}",
            root_rec::SIZE, len
        )));
    }
    Ok(())
}

/// Append the `KEY_BYTES` bytes of `d`.
fn put_digest(buf: &mut Vec<u8>, d: &ObjectKey) {
    buf.extend_from_slice(&d.0);
}

// ─── helpers for BTreeMap<u64, ObjectKey> serialization ─────────────────────

/// Serialize a `BTreeMap<u64, ObjectKey>` and store it as a node (`NodeWrite`).
///
/// Format: `[count: u64 LE][for each entry: u64 LE ino + ObjectKey]`
/// The entry count of a map node and the size it needs (`None` if the size
/// overflows: the count is untrusted, see #64). The caller checks
/// `bytes.len() >= 8` first.
fn map_size(bytes: &[u8]) -> (u64, Option<usize>) {
    let count = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
    let size = usize::try_from(count).ok().and_then(|n| n.checked_mul(MAP_ENTRY)).and_then(|s| s.checked_add(8));
    (count, size)
}

/// Each entry is `MAP_ENTRY` bytes; total = 8 + count × `MAP_ENTRY`.
fn intern_u64_digest_map(storage: &mut impl crate::store_io::NodeWrite, map: &BTreeMap<u64, ObjectKey>) -> ObjectKey {
    let mut bytes = Vec::with_capacity(8 + map.len() * MAP_ENTRY);
    bytes.extend_from_slice(&(map.len() as u64).to_le_bytes());
    for (ino, digest) in map {
        bytes.extend_from_slice(&ino.to_le_bytes());
        put_digest(&mut bytes, digest);
    }
    (storage).put_node(&bytes)
}

/// Read and deserialize a `BTreeMap<u64, ObjectKey>` node (`NodeRead`).
fn load_u64_digest_map(
    io: &mut impl crate::store_io::NodeRead,
    key: &ObjectKey,
) -> Result<BTreeMap<u64, ObjectKey>, MetaError> {
    let bytes = (io).get_node(key)
        .map_err(|e| crate::store_io::node_err("u64-digest map node", key, e))?;

    if bytes.len() < 8 {
        return Err(MetaError::Corrupted(format!(
            "u64-digest map: expected at least 8 bytes, got {}",
            bytes.len()
        )));
    }
    let (count, expected) = map_size(&bytes);
    if Some(bytes.len()) != expected {
        return Err(MetaError::Corrupted(format!(
            "u64-digest map: {} bytes cannot hold {} entries",
            bytes.len(), count
        )));
    }

    let mut map = BTreeMap::new();
    for i in 0..count as usize { // checked by map_size
        let off = 8 + i * MAP_ENTRY;
        let ino = u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap());
        let digest = parse_key(&bytes[off + 8..off + MAP_ENTRY]);
        map.insert(ino, digest);
    }
    Ok(map)
}

impl DictMetadataStore {
    /// Reconstruct a `DictMetadataStore` from the node store and a root key
    /// previously returned by `commit()`.
    ///
    /// Restores all in-memory maps (inode_data, dir_data, manifest_data, xattr_data)
    /// and inode numbering state so that subsequent operations continue seamlessly.
    pub fn load_from_root(io: Arc<Mutex<StoreIo>>, root: &ObjectKey) -> Result<Self, MetaError> {
        let mut io_guard = lock_stats::lock(&io, Site::Io);

        // Read root record bytes
        let bytes = (&mut *io_guard).get_node(root)
            .map_err(|e| crate::store_io::node_err("root node", root, e))?;

        check_root_len(bytes.len())?;
        let digest_at = |o: usize| parse_key(&bytes[o..o + KEY_BYTES]);
        let u64_at = |o: usize| u64::from_le_bytes(bytes[o..o + 8].try_into().unwrap());

        let inode_map_digest = digest_at(root_rec::INODE_MAP);
        let root_dir_ino = u64_at(root_rec::ROOT_DIR_INO);
        let next_ino = u64_at(root_rec::NEXT_INO);
        let inode_data_digest = digest_at(root_rec::INODE_DATA);
        let dir_data_digest = digest_at(root_rec::DIR_DATA);
        let manifest_data_digest = digest_at(root_rec::MANIFEST_DATA);
        let xattr_data_digest = digest_at(root_rec::XATTR_DATA);
        let refcount_data_digest = digest_at(root_rec::REFCOUNTS);
        let initial_logical_bytes = u64_at(root_rec::LOGICAL_BYTES);
        let initial_inode_count = u64_at(root_rec::INODE_COUNT);

        // Load and patch inode_map so next_ino is exactly restored.
        let mut inode_map = load_inode_map(&mut *io_guard, &inode_map_digest)?;
        inode_map.set_next_ino(next_ino);

        let inode_data = load_u64_digest_map(&mut *io_guard, &inode_data_digest)?;
        let dir_data = load_u64_digest_map(&mut *io_guard, &dir_data_digest)?;
        let manifest_data = load_u64_digest_map(&mut *io_guard, &manifest_data_digest)?;
        let xattr_data = load_u64_digest_map(&mut *io_guard, &xattr_data_digest)?;
        let refcounts = load_key_u64_map(&mut *io_guard, &refcount_data_digest)?;

        // Sanity: root directory inode must exist
        if !inode_data.contains_key(&root_dir_ino) {
            return Err(MetaError::Corrupted(format!(
                "root dir ino {} not found in inode_data after reload",
                root_dir_ino
            )));
        }

        drop(io_guard);

        Ok(DictMetadataStore {
            io,
            inode_map: lk(inode_map),
            inode_data: lk(inode_data),
            dir_data: lk(dir_data),
            dir_live: lk(Live::default()),
            inode_live: lk(Live::default()),
            changes: AtomicU64::new(0),
            committed_changes: AtomicU64::new(0),
            manifest_data: lk(manifest_data),
            xattr_data: lk(xattr_data),
            pre_root_sync: lk(None),
            refcounts: lk(refcounts),
            logical_bytes: AtomicU64::new(initial_logical_bytes),
            inode_count: AtomicU64::new(initial_inode_count),
            wal: lk(None).timed(Site::Wal),
            wal_fail_seq: AtomicU64::new(0),
            wal_fail_cleared: AtomicU64::new(0),
            commit_lock: sync::Mutex::new(Rank::Rename, ()),
            commit_fault: std::sync::atomic::AtomicU8::new(0),
            last_root: lk(Some(*root)),
            snapshots_by_version: lk(HashMap::new()),
            snapshots_by_name: lk(HashMap::new()),
        })
    }
}

/// Serialize a `BTreeMap<ObjectKey, u64>` and store it as a node (`NodeWrite`).
///
/// Format: `[count: u64 LE][for each entry: ObjectKey + u64 LE]`
/// Each entry is `MAP_ENTRY` bytes; total = 8 + count × `MAP_ENTRY`.
fn intern_key_u64_map(storage: &mut impl crate::store_io::NodeWrite, map: &BTreeMap<ObjectKey, u64>) -> ObjectKey {
    let mut bytes = Vec::with_capacity(8 + map.len() * MAP_ENTRY);
    bytes.extend_from_slice(&(map.len() as u64).to_le_bytes());
    for (digest, count) in map {
        put_digest(&mut bytes, digest);
        bytes.extend_from_slice(&count.to_le_bytes());
    }
    (storage).put_node(&bytes)
}

/// Read and deserialize a `BTreeMap<ObjectKey, u64>` node (`NodeRead`).
fn load_key_u64_map(
    io: &mut impl crate::store_io::NodeRead,
    key: &ObjectKey,
) -> Result<BTreeMap<ObjectKey, u64>, MetaError> {
    let bytes = (io).get_node(key)
        .map_err(|e| crate::store_io::node_err("key-u64 map node", key, e))?;

    if bytes.len() < 8 {
        return Err(MetaError::Corrupted(format!(
            "key-u64 map: expected at least 8 bytes, got {}",
            bytes.len()
        )));
    }
    let (count, expected) = map_size(&bytes);
    if Some(bytes.len()) != expected {
        return Err(MetaError::Corrupted(format!(
            "key-u64 map: {} bytes cannot hold {} entries",
            bytes.len(), count
        )));
    }

    let mut map = BTreeMap::new();
    for i in 0..count as usize { // checked by map_size
        let off = 8 + i * MAP_ENTRY;
        let digest = parse_key(&bytes[off..off + KEY_BYTES]);
        let count_val = u64::from_le_bytes(bytes[off + KEY_BYTES..off + MAP_ENTRY].try_into().unwrap());
        map.insert(digest, count_val);
    }
    Ok(map)
}

/// Parse an `ObjectKey` from a `KEY_BYTES`-byte slice.
fn parse_key(bytes: &[u8]) -> ObjectKey {
    ObjectKey(bytes.try_into().expect("a KEY_BYTES slice"))
}


// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod map_decode_tests {
    use super::*;
    use crate::store_io::{NodeWrite, StoreIo};

    // A map node with an entry count whose size overflows is corrupt
    // metadata: an error, never a panic or a wrapped length.
    #[test]
    fn a_map_with_an_overflowing_count_is_corrupt() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut io = StoreIo::open(dir.path()).unwrap();
        for n in [u64::MAX, 1u64 << 63, u64::MAX / MAP_ENTRY as u64 + 1, 1] {
            let key = io.put_node(&n.to_le_bytes());
            let a = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| load_u64_digest_map(&mut io, &key)));
            assert!(matches!(a, Ok(Err(MetaError::Corrupted(_)))), "u64-key map, count {n}");
            let b = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| load_key_u64_map(&mut io, &key)));
            assert!(matches!(b, Ok(Err(MetaError::Corrupted(_)))), "key-u64 map, count {n}");
        }
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn new_store_root_dir_has_current_times() {
        let dir = tempfile::TempDir::new().unwrap();
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        let store = DictMetadataStore::new(Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap())));
        let root = store.get_inode(1).unwrap();
        assert!(root.mtime_sec as i64 >= before, "root mtime {} (epoch 0 shows as 1969)", root.mtime_sec);
        assert!(root.ctime_sec as i64 >= before, "root ctime {}", root.ctime_sec);
    }

    #[test]
    fn cached_dirs_and_inodes_survive_eviction_commit_and_reload() {
        let tmp = tempfile::tempdir().unwrap();
        let io = Arc::new(Mutex::new(StoreIo::open(tmp.path()).unwrap()));
        let store = DictMetadataStore::new(io.clone());
        let mut expect = Vec::new();
        for d in 0..10u64 {
            let dir = store
                .create_directory(1, &format!("d{d}"), &InodeMeta::new_directory(0, 0, 0, 0o040_755))
                .unwrap();
            for f in 0..10u64 {
                let ino = store.create_inode(&InodeMeta::new_file(0, 0, 0, 0o100_644)).unwrap();
                store.link(dir, &format!("f{f}"), ino).unwrap();
                let mut m = store.get_inode(ino).unwrap();
                m.size = d * 100 + f;
                store.update_inode(&m).unwrap();
                expect.push((dir, format!("f{f}"), ino, m.size));
            }
            store.unlink(dir, "f0").unwrap();
        }
        // The caches stayed bounded, and every change is still visible.
        assert!(store.inode_live.lock().map.len() <= INODE_CACHE_MAX + 1);
        assert!(store.dir_live.lock().map.len() <= DIR_CACHE_MAX + 1);
        // Dirty entries are written in batches, so the final commit stays small.
        assert!(store.inode_live.lock().dirty.len() < DIRTY_FLUSH);
        assert!(store.dir_live.lock().dirty.len() < DIRTY_FLUSH);
        let check = |s: &DictMetadataStore| {
            for (dir, name, ino, size) in &expect {
                if name == "f0" {
                    assert!(s.lookup(*dir, name).is_err());
                    continue;
                }
                assert_eq!(s.lookup(*dir, name).unwrap(), *ino);
                assert_eq!(s.get_inode(*ino).unwrap().size, *size);
            }
            assert_eq!(s.list_directory(1).unwrap().len(), 12); // . .. d0..d9
        };
        check(&store);
        let root = store.commit().unwrap();
        check(&store);
        let reloaded = DictMetadataStore::load_from_root(io, &root).unwrap();
        check(&reloaded);
        assert_eq!(reloaded.logical_bytes(), store.logical_bytes());
    }
    use super::*;
    use slicefs_traits::metadata::{InodeMeta, MetadataStore};
    use tempfile::TempDir;

    fn new_dir_meta() -> InodeMeta {
        InodeMeta::new_directory(0, 1000, 1000, S_IFDIR | 0o755)
    }

    fn new_file_meta() -> InodeMeta {
        InodeMeta::new_file(0, 1000, 1000, 0o644)
    }

    /// Create a new DictMetadataStore backed by a temporary directory.
    /// Returns (TempDir, store) — TempDir must be kept alive while store is used.
    fn make_store() -> (TempDir, DictMetadataStore) {
        let dir = TempDir::new().unwrap();
        let io = Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap()));
        let store = DictMetadataStore::new(io);
        (dir, store)
    }

    #[test]
    fn test_delete_inode_drops_manifest_xattrs_and_dir_entries() {
        // Leftover per-inode map entries keep deleted content reachable for GC.
        let (_d, store) = make_store();
        let ino = store.create_inode(&new_file_meta()).unwrap();
        store.set_manifest(ino, &[ObjectKey::example(9)]).unwrap();
        store.set_xattr(ino, "user.k", b"v").unwrap();
        store.delete_inode(ino).unwrap();
        assert!(store.get_manifest(ino).is_err());
        assert!(store.get_xattr(ino, "user.k").is_err());

        let d = store.create_directory(1, "d", &new_dir_meta()).unwrap();
        store.unlink(1, "d").unwrap();
        store.delete_inode(d).unwrap();
        assert!(store.list_directory(d).is_err());
    }

    #[test]
    fn test_names_longer_than_name_max_are_rejected() {
        let (_d, store) = make_store();
        let ok = "a".repeat(255);
        let long = "a".repeat(256);
        let ino = store.create_inode(&new_file_meta()).unwrap();
        assert!(matches!(store.link(1, &long, ino), Err(MetaError::NameTooLong(256))));
        assert!(matches!(store.lookup(1, &long), Err(MetaError::NameTooLong(256))));
        assert!(matches!(store.create_directory(1, &long, &new_dir_meta()), Err(MetaError::NameTooLong(256))));
        store.link(1, &ok, ino).unwrap();
        assert_eq!(store.lookup(1, &ok).unwrap(), ino);
    }

    #[test]
    fn test_new_has_root_dir() {
        let (_dir, store) = make_store();
        assert_eq!(store.root_ino(), 1);
        let entries = store.list_directory(1).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"."), ". not found in root: {:?}", names);
        assert!(names.contains(&".."), ".. not found in root: {:?}", names);
    }

    #[test]
    fn test_inode_crud() {
        let (_dir, store) = make_store();

        // Create
        let meta = new_file_meta();
        let ino = store.create_inode(&meta).unwrap();
        assert!(ino >= 2, "allocated ino should be >= 2, got {}", ino);

        // Get
        let retrieved = store.get_inode(ino).unwrap();
        assert_eq!(retrieved.ino, ino);
        assert_eq!(retrieved.mode, meta.mode);
        assert_eq!(retrieved.uid, meta.uid);

        // Update
        let mut updated = retrieved.clone();
        updated.size = 4096;
        store.update_inode(&updated).unwrap();
        let after_update = store.get_inode(ino).unwrap();
        assert_eq!(after_update.size, 4096);

        // Delete
        store.delete_inode(ino).unwrap();
        let result = store.get_inode(ino);
        assert!(matches!(result, Err(MetaError::NotFound(_))));
    }

    #[test]
    fn test_create_directory_shows_in_parent() {
        let (_dir, store) = make_store();
        let dir_meta = new_dir_meta();
        let ino = store.create_directory(1, "subdir", &dir_meta).unwrap();
        assert!(ino >= 2);

        // Parent should contain the new dir
        let parent_entries = store.list_directory(1).unwrap();
        let found = parent_entries.iter().any(|e| e.name == "subdir" && e.ino == ino);
        assert!(found, "subdir not found in parent: {:?}", parent_entries);

        // New dir should have . and ..
        let sub_entries = store.list_directory(ino).unwrap();
        let names: Vec<&str> = sub_entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"."), ". missing in new dir");
        assert!(names.contains(&".."), ".. missing in new dir");
    }

    #[test]
    fn test_lookup_and_link() {
        let (_dir, store) = make_store();

        // Create a file inode
        let file_ino = store.create_inode(&new_file_meta()).unwrap();

        // Link it into root
        store.link(1, "myfile", file_ino).unwrap();

        // Lookup should find it
        let found_ino = store.lookup(1, "myfile").unwrap();
        assert_eq!(found_ino, file_ino);
    }

    #[test]
    fn test_unlink_removes_entry() {
        let (_dir, store) = make_store();

        let file_ino = store.create_inode(&new_file_meta()).unwrap();
        store.link(1, "toremove", file_ino).unwrap();
        assert!(store.lookup(1, "toremove").is_ok());

        store.unlink(1, "toremove").unwrap();
        let result = store.lookup(1, "toremove");
        assert!(result.is_err(), "entry should be gone after unlink");
    }

    #[test]
    fn test_manifest_round_trip() {
        let (_dir, store) = make_store();
        let file_ino = store.create_inode(&new_file_meta()).unwrap();

        let blocks: Vec<ObjectKey> = (0..5).map(|i| ObjectKey::example(i as u32)).collect();
        store.set_manifest(file_ino, &blocks).unwrap();
        let recovered = store.get_manifest(file_ino).unwrap();
        assert_eq!(recovered, blocks);
    }

    #[test]
    fn test_root_ino_is_one() {
        let (_dir, store) = make_store();
        assert_eq!(store.root_ino(), 1);
    }

    #[test]
    fn test_commit_returns_nonzero_digest() {
        let (_dir, store) = make_store();
        let digest = store.commit().unwrap();
        assert_ne!(digest, ObjectKey::ZERO, "commit should return non-zero digest");
    }

    #[test]
    fn test_delete_nonexistent_returns_not_found() {
        let (_dir, store) = make_store();
        let result = store.delete_inode(9999);
        assert!(matches!(result, Err(MetaError::NotFound(9999))));
    }

    #[test]
    fn test_create_dir_under_nonexistent_parent_returns_not_found() {
        let (_dir, store) = make_store();
        let result = store.create_directory(9999, "sub", &new_dir_meta());
        // Should be NotFound since ino 9999 does not exist
        assert!(result.is_err());
    }

    #[test]
    fn test_create_dir_under_file_returns_not_a_directory() {
        let (_dir, store) = make_store();
        let file_ino = store.create_inode(&new_file_meta()).unwrap();
        let result = store.create_directory(file_ino, "sub", &new_dir_meta());
        assert!(
            matches!(result, Err(MetaError::NotADirectory(_))),
            "expected NotADirectory, got {:?}",
            result.err()
        );
    }

    #[test]
    fn test_xattr_set_get() {
        let (_dir, store) = make_store();
        let file_ino = store.create_inode(&new_file_meta()).unwrap();
        store.set_xattr(file_ino, "user.test", b"myvalue").unwrap();
        let val = store.get_xattr(file_ino, "user.test").unwrap();
        assert_eq!(val, b"myvalue");
    }

    #[test]
    fn test_xattr_list() {
        let (_dir, store) = make_store();
        let file_ino = store.create_inode(&new_file_meta()).unwrap();
        store.set_xattr(file_ino, "user.a", b"1").unwrap();
        store.set_xattr(file_ino, "user.b", b"2").unwrap();
        store.set_xattr(file_ino, "security.x", b"3").unwrap();
        let mut names = store.list_xattrs(file_ino).unwrap();
        names.sort();
        assert!(names.contains(&"user.a".to_string()));
        assert!(names.contains(&"user.b".to_string()));
        assert!(names.contains(&"security.x".to_string()));
        assert_eq!(names.len(), 3);
    }

    #[test]
    fn test_xattr_remove() {
        let (_dir, store) = make_store();
        let file_ino = store.create_inode(&new_file_meta()).unwrap();
        store.set_xattr(file_ino, "user.k", b"v").unwrap();
        store.remove_xattr(file_ino, "user.k").unwrap();
        let result = store.get_xattr(file_ino, "user.k");
        assert!(result.is_err(), "get after remove should error");
    }

    #[test]
    fn test_xattr_overwrite() {
        let (_dir, store) = make_store();
        let file_ino = store.create_inode(&new_file_meta()).unwrap();
        store.set_xattr(file_ino, "user.x", b"first").unwrap();
        store.set_xattr(file_ino, "user.x", b"second").unwrap();
        let val = store.get_xattr(file_ino, "user.x").unwrap();
        assert_eq!(val, b"second");
    }

    #[test]
    fn test_xattr_on_directory() {
        let (_dir, store) = make_store();
        // Set xattr on the root directory inode (ino=1)
        store.set_xattr(1, "user.dir_attr", b"dir_val").unwrap();
        let val = store.get_xattr(1, "user.dir_attr").unwrap();
        assert_eq!(val, b"dir_val");
    }

    #[test]
    fn test_xattr_large_value() {
        let (_dir, store) = make_store();
        let file_ino = store.create_inode(&new_file_meta()).unwrap();
        // Value > 31 bytes exercises CAS tree storage
        let large_value: Vec<u8> = (0u8..=127u8).collect();
        store.set_xattr(file_ino, "user.big", &large_value).unwrap();
        let recovered = store.get_xattr(file_ino, "user.big").unwrap();
        assert_eq!(recovered, large_value);
    }

    #[test]
    fn test_xattr_list_empty() {
        let (_dir, store) = make_store();
        let file_ino = store.create_inode(&new_file_meta()).unwrap();
        let names = store.list_xattrs(file_ino).unwrap();
        assert!(names.is_empty());
    }

    #[test]
    fn test_xattr_get_not_found() {
        let (_dir, store) = make_store();
        let file_ino = store.create_inode(&new_file_meta()).unwrap();
        let result = store.get_xattr(file_ino, "user.missing");
        assert!(matches!(result, Err(MetaError::NotFound(_))));
    }

    #[test]
    fn test_xattr_remove_not_found() {
        let (_dir, store) = make_store();
        let file_ino = store.create_inode(&new_file_meta()).unwrap();
        let result = store.remove_xattr(file_ino, "user.nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn test_update_nonexistent_returns_not_found() {
        let (_dir, store) = make_store();
        let mut meta = new_file_meta();
        meta.ino = 9999;
        let result = store.update_inode(&meta);
        assert!(matches!(result, Err(MetaError::NotFound(9999))));
    }

    #[test]
    fn test_inode_numbers_are_monotonic() {
        let (_dir, store) = make_store();
        let ino1 = store.create_inode(&new_file_meta()).unwrap();
        let ino2 = store.create_inode(&new_file_meta()).unwrap();
        let ino3 = store.create_inode(&new_file_meta()).unwrap();
        assert!(ino1 < ino2);
        assert!(ino2 < ino3);
    }

    #[test]
    fn test_link_duplicate_name_rejected() {
        let (_dir, store) = make_store();
        let ino = store.create_inode(&new_file_meta()).unwrap();
        store.link(1, "dup", ino).unwrap();
        let result = store.link(1, "dup", ino);
        assert!(matches!(result, Err(MetaError::AlreadyExists(_))));
    }

    #[test]
    fn test_list_directory_non_dir_returns_error() {
        let (_dir, store) = make_store();
        // Inode 9999 doesn't exist as a directory
        let result = store.list_directory(9999);
        assert!(result.is_err());
    }

    // ── persistence round-trip tests (Task 2) ─────────────────────────────────

    #[test]
    fn test_commit_and_reload() {
        let (_dir, store) = make_store();

        // Add a file
        let file_ino = store.create_inode(&new_file_meta()).unwrap();
        store.link(1, "myfile", file_ino).unwrap();
        let blocks: Vec<ObjectKey> = vec![ObjectKey::example(1), ObjectKey::example(2)];
        store.set_manifest(file_ino, &blocks).unwrap();

        // Add a subdirectory
        let dir_ino = store.create_directory(1, "subdir", &new_dir_meta()).unwrap();

        // Add xattrs
        store.set_xattr(file_ino, "user.meta", b"mymetavalue").unwrap();
        store.set_xattr(dir_ino, "user.tag", b"important").unwrap();

        // Commit — nodes flushed to disk
        let root_digest = store.commit().unwrap();
        // Clone the Arc<Mutex<StoreIo>> to reload from the same on-disk files
        let io = Arc::clone(store.io());

        // Reload from the same file storage + root digest
        let reloaded = DictMetadataStore::load_from_root(io, &root_digest).unwrap();

        // Verify file inode
        let file_meta = reloaded.get_inode(file_ino).unwrap();
        assert_eq!(file_meta.ino, file_ino);
        assert_eq!(file_meta.mode, 0o644);

        // Verify manifest
        let recovered_blocks = reloaded.get_manifest(file_ino).unwrap();
        assert_eq!(recovered_blocks, blocks);

        // Verify directory
        let entries = reloaded.list_directory(1).unwrap();
        assert!(entries.iter().any(|e| e.name == "myfile" && e.ino == file_ino));
        assert!(entries.iter().any(|e| e.name == "subdir" && e.ino == dir_ino));

        // Verify xattrs
        let val = reloaded.get_xattr(file_ino, "user.meta").unwrap();
        assert_eq!(val, b"mymetavalue");
        let val2 = reloaded.get_xattr(dir_ino, "user.tag").unwrap();
        assert_eq!(val2, b"important");
    }

    #[test]
    fn test_inode_stability_across_reload() {
        let (_dir, store) = make_store();

        // Allocate 5 file inodes (inos 2-6)
        let inos: Vec<u64> = (0..5)
            .map(|_| store.create_inode(&new_file_meta()).unwrap())
            .collect();

        let root = store.commit().unwrap();
        let io = Arc::clone(store.io());

        let reloaded = DictMetadataStore::load_from_root(io, &root).unwrap();

        // All 5 inodes still exist
        for &ino in &inos {
            assert!(reloaded.get_inode(ino).is_ok(), "ino {} missing after reload", ino);
        }

        // Next allocated inode continues from where it left off (no reuse)
        let next_ino = reloaded.create_inode(&new_file_meta()).unwrap();
        for &old_ino in &inos {
            assert_ne!(next_ino, old_ino, "inode {} was reused!", next_ino);
        }
        // next_ino must be > max of previous inos
        let max_ino = *inos.iter().max().unwrap();
        assert!(next_ino > max_ino, "next_ino {} not > max_ino {}", next_ino, max_ino);
    }

    #[test]
    fn test_directory_stable_across_reload() {
        let (_dir, store) = make_store();
        let sub1 = store.create_directory(1, "alpha", &new_dir_meta()).unwrap();
        let sub2 = store.create_directory(1, "beta", &new_dir_meta()).unwrap();
        let _sub3 = store.create_directory(sub1, "gamma", &new_dir_meta()).unwrap();

        let root = store.commit().unwrap();
        let io = Arc::clone(store.io());
        let reloaded = DictMetadataStore::load_from_root(io, &root).unwrap();

        let entries = reloaded.list_directory(1).unwrap();
        assert!(entries.iter().any(|e| e.name == "alpha" && e.ino == sub1));
        assert!(entries.iter().any(|e| e.name == "beta" && e.ino == sub2));
    }

    #[test]
    fn test_manifest_stable_across_reload() {
        let (_dir, store) = make_store();
        let file_ino = store.create_inode(&new_file_meta()).unwrap();
        let blocks: Vec<ObjectKey> = (0..10).map(|i| ObjectKey::example(i as u32)).collect();
        store.set_manifest(file_ino, &blocks).unwrap();

        let root = store.commit().unwrap();
        let io = Arc::clone(store.io());
        let reloaded = DictMetadataStore::load_from_root(io, &root).unwrap();

        let recovered = reloaded.get_manifest(file_ino).unwrap();
        assert_eq!(recovered, blocks);
    }

    #[test]
    fn test_xattr_stable_across_reload() {
        let (_dir, store) = make_store();
        let file_ino = store.create_inode(&new_file_meta()).unwrap();
        store.set_xattr(file_ino, "user.k1", b"val1").unwrap();
        store.set_xattr(file_ino, "user.k2", b"val2").unwrap();

        let root = store.commit().unwrap();
        let io = Arc::clone(store.io());
        let reloaded = DictMetadataStore::load_from_root(io, &root).unwrap();

        assert_eq!(reloaded.get_xattr(file_ino, "user.k1").unwrap(), b"val1");
        assert_eq!(reloaded.get_xattr(file_ino, "user.k2").unwrap(), b"val2");
    }

    #[test]
    fn test_filestorage_persistence_round_trip() {
        // This test proves POSIX-10: inode numbers are stable across a full
        // commit + load_from_root cycle backed by file storage.

        let (_dir, store) = make_store();
        let file_ino = store.create_inode(&new_file_meta()).unwrap();
        store.link(1, "file.txt", file_ino).unwrap();
        let blocks: Vec<ObjectKey> = vec![ObjectKey::example(0xAB)];
        store.set_manifest(file_ino, &blocks).unwrap();
        store.set_xattr(file_ino, "user.tag", b"important").unwrap();

        let root = store.commit().unwrap();
        let io = Arc::clone(store.io());

        // Load metadata from the same on-disk file storage
        let reloaded = DictMetadataStore::load_from_root(io, &root).unwrap();

        // Verify all data intact
        let meta = reloaded.get_inode(file_ino).unwrap();
        assert_eq!(meta.ino, file_ino);

        let found_ino = reloaded.lookup(1, "file.txt").unwrap();
        assert_eq!(found_ino, file_ino);

        let recovered = reloaded.get_manifest(file_ino).unwrap();
        assert_eq!(recovered, blocks);

        let tag = reloaded.get_xattr(file_ino, "user.tag").unwrap();
        assert_eq!(tag, b"important");
    }

    #[test]
    fn test_empty_store_commit_reload() {
        let (_dir, store) = make_store();
        let root = store.commit().unwrap();
        let io = Arc::clone(store.io());

        let reloaded = DictMetadataStore::load_from_root(io, &root).unwrap();

        // Root dir still exists with . and ..
        assert_eq!(reloaded.root_ino(), 1);
        let entries = reloaded.list_directory(1).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"."), ". missing after reload");
        assert!(names.contains(&".."), ".. missing after reload");
    }

    /// Verify that content pushed into `store.io()` via `put_node` is
    /// accessible after a commit + `load_from_root` cycle.
    ///
    /// This proves the seed command's design is sound: file content and metadata
    /// can share the same Io backend, and nothing is lost on a round-trip.
    #[test]
    fn test_io_accessor_content_survives_reload() {
        let (_dir, store) = make_store();

        // Push content bytes into the store's io via put_node.
        let content = b"hello, SliceFS content round-trip test!";
        let content_digest: ObjectKey = {
            let mut io_guard = store.io().lock().unwrap();
            let mut fsa = &mut *io_guard;
            (&mut fsa).put_node(content)
        };

        // Create a file inode and attach the content digest as its manifest.
        let file_ino = store.create_inode(&new_file_meta()).unwrap();
        store.link(1, "content.txt", file_ino).unwrap();
        store.set_manifest(file_ino, &[content_digest]).unwrap();

        // Commit then reload from same file storage.
        let root = store.commit().unwrap();
        let io = Arc::clone(store.io());

        // Reload the metadata store from the same file storage.
        let reloaded = DictMetadataStore::load_from_root(io, &root).unwrap();

        // Verify manifest lookup still returns the same digest.
        let manifest = reloaded.get_manifest(file_ino).unwrap();
        assert_eq!(manifest, vec![content_digest]);

        // Verify the content bytes are retrievable from the node store via get_node.
        let mut io_guard = reloaded.io().lock().unwrap();
        let read_back = (&mut *io_guard).get_node(&content_digest).expect("content missing");
        assert_eq!(read_back, content.as_slice());
    }

    // ── Snapshot method tests ──────────────────────────────────────────────

    #[test]
    fn test_create_snapshot_returns_entry() {
        use crate::wal::{WalConfig, create_wal};
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("segments")).unwrap();
        let wal = create_wal(WalConfig::PerOp, dir.path(), 1).unwrap();
        let (_dir, mut store) = make_store();
        store.set_wal(wal);

        let snap = store.create_snapshot(None).expect("create_snapshot should succeed");
        assert_eq!(snap.version, 1, "first snapshot must have version 1");
        assert!(snap.name.is_none(), "no name expected");
        assert_ne!(snap.root, ObjectKey::ZERO, "snapshot root must not be zero");
    }

    #[test]
    fn test_create_snapshot_auto_increments_version() {
        use crate::wal::{WalConfig, create_wal};
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("segments")).unwrap();
        let wal = create_wal(WalConfig::PerOp, dir.path(), 1).unwrap();
        let (_dir, mut store) = make_store();
        store.set_wal(wal);

        let snap1 = store.create_snapshot(None).unwrap();
        let snap2 = store.create_snapshot(Some("v2".to_string())).unwrap();
        assert_eq!(snap1.version, 1);
        assert_eq!(snap2.version, 2);
        assert_eq!(snap2.name.as_deref(), Some("v2"));
    }

    #[test]
    fn test_list_snapshots_sorted_by_version() {
        use crate::wal::{WalConfig, create_wal};
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("segments")).unwrap();
        let wal = create_wal(WalConfig::PerOp, dir.path(), 1).unwrap();
        let (_dir, mut store) = make_store();
        store.set_wal(wal);

        store.create_snapshot(None).unwrap();
        store.create_snapshot(None).unwrap();
        store.create_snapshot(None).unwrap();

        let list = store.list_snapshots();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].version, 1);
        assert_eq!(list[1].version, 2);
        assert_eq!(list[2].version, 3);
    }

    #[test]
    fn test_find_snapshot_by_version_string() {
        use crate::wal::{WalConfig, create_wal};
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("segments")).unwrap();
        let wal = create_wal(WalConfig::PerOp, dir.path(), 1).unwrap();
        let (_dir, mut store) = make_store();
        store.set_wal(wal);

        store.create_snapshot(None).unwrap();
        store.create_snapshot(Some("production".to_string())).unwrap();

        let found = store.find_snapshot("2").expect("should find by version string");
        assert_eq!(found.version, 2);
        assert_eq!(found.name.as_deref(), Some("production"));
    }

    #[test]
    fn test_find_snapshot_by_name() {
        use crate::wal::{WalConfig, create_wal};
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("segments")).unwrap();
        let wal = create_wal(WalConfig::PerOp, dir.path(), 1).unwrap();
        let (_dir, mut store) = make_store();
        store.set_wal(wal);

        store.create_snapshot(Some("release-1.0".to_string())).unwrap();

        let found = store.find_snapshot("release-1.0").expect("should find by name");
        assert_eq!(found.name.as_deref(), Some("release-1.0"));
    }

    #[test]
    fn test_find_snapshot_returns_none_for_unknown() {
        use crate::wal::{WalConfig, create_wal};
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("segments")).unwrap();
        let wal = create_wal(WalConfig::PerOp, dir.path(), 1).unwrap();
        let (_dir, mut store) = make_store();
        store.set_wal(wal);

        store.create_snapshot(None).unwrap();
        assert!(store.find_snapshot("nonexistent").is_none());
        assert!(store.find_snapshot("99").is_none());
    }

    #[test]
    fn test_snapshot_roots_includes_all_roots() {
        use crate::wal::{WalConfig, create_wal};
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("segments")).unwrap();
        let wal = create_wal(WalConfig::PerOp, dir.path(), 1).unwrap();
        let (_dir, mut store) = make_store();
        store.set_wal(wal);

        store.create_snapshot(None).unwrap();
        store.create_snapshot(None).unwrap();

        let roots = store.snapshot_roots();
        // snapshot_roots must include snapshot roots + current live root
        assert!(roots.len() >= 2, "should include at least 2 roots: {:?}", roots);
    }

    #[test]
    fn test_snapshot_roots_no_snapshots_returns_current_root() {
        use crate::wal::{WalConfig, create_wal};
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("segments")).unwrap();
        let wal = create_wal(WalConfig::PerOp, dir.path(), 1).unwrap();
        let (_dir, mut store) = make_store();
        store.set_wal(wal);

        // Commit to establish a root, but no snapshots
        store.commit().unwrap();
        let roots = store.snapshot_roots();
        assert_eq!(roots.len(), 1, "with no snapshots, roots should contain only current_root");
    }

    #[test]
    fn test_snapshots_survive_segment_replay() {
        use crate::wal::{WalConfig, create_wal};
        use crate::segment::load_store_from_segments;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("segments")).unwrap();

        // Create a store with a snapshot
        {
            let wal = create_wal(WalConfig::PerOp, dir.path(), 1).unwrap();
            let (_dir, mut store) = make_store();
            store.set_wal(wal);
            store.create_snapshot(Some("replay-test".to_string())).unwrap();
            store.shutdown_wal().unwrap();
        }

        // Reload from segments
        let segs_dir = dir.path().join("segments");
        let (_root, snapshots) = load_store_from_segments(&segs_dir).unwrap();
        assert_eq!(snapshots.len(), 1, "snapshot must survive WAL replay");
        assert_eq!(snapshots[0].version, 1);
        assert_eq!(snapshots[0].name.as_deref(), Some("replay-test"));
    }

    #[test]
    fn test_set_snapshots_loads_snapshot_list() {
        use crate::snapshot::SnapshotEntry;
        let (_dir, store) = make_store();
        let snap = SnapshotEntry {
            version: 5,
            name: Some("loaded".to_string()),
            root: ObjectKey::example(1),
            created_at: 12345,
        };
        // set_snapshots must load them so list_snapshots returns them
        let mut store = store;
        store.set_snapshots(vec![snap.clone()]);
        let list = store.list_snapshots();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].version, 5);
    }

    // ── O(1) HashMap snapshot tests (FIX-03, FIX-04) ──────────────────────

    fn make_snapshot(version: u64, name: Option<&str>) -> crate::snapshot::SnapshotEntry {
        crate::snapshot::SnapshotEntry {
            version,
            name: name.map(|s| s.to_string()),
            root: ObjectKey::example(version as u32),
            created_at: version * 1000,
        }
    }

    /// FIX-03: find_snapshot by version string uses O(1) lookup.
    #[test]
    fn test_hashmap_find_by_version_o1() {
        let (_dir, mut store) = make_store();
        store.set_snapshots(vec![
            make_snapshot(1, None),
            make_snapshot(2, Some("beta")),
            make_snapshot(3, Some("gamma")),
        ]);
        let found = store.find_snapshot("1").expect("must find version 1");
        assert_eq!(found.version, 1);
        let found2 = store.find_snapshot("2").expect("must find version 2");
        assert_eq!(found2.version, 2);
        assert!(store.find_snapshot("99").is_none(), "version 99 must not exist");
    }

    /// FIX-04: find_snapshot by name uses O(1) lookup.
    #[test]
    fn test_hashmap_find_by_name_o1() {
        let (_dir, mut store) = make_store();
        store.set_snapshots(vec![
            make_snapshot(1, Some("alpha")),
            make_snapshot(2, Some("beta")),
        ]);
        let found = store.find_snapshot("alpha").expect("must find 'alpha'");
        assert_eq!(found.version, 1);
        let found2 = store.find_snapshot("beta").expect("must find 'beta'");
        assert_eq!(found2.version, 2);
        assert!(store.find_snapshot("nonexistent").is_none());
    }

    /// Scale test: 10,000 snapshots — find_snapshot must complete in under 1ms.
    #[test]
    fn test_hashmap_10k_snapshots_under_1ms() {
        let snaps: Vec<_> = (1u64..=10_000)
            .map(|v| make_snapshot(v, None))
            .collect();
        let (_dir, mut store) = make_store();
        store.set_snapshots(snaps);

        let start = std::time::Instant::now();
        let found = store.find_snapshot("9999").expect("must find version 9999");
        let elapsed = start.elapsed();

        assert_eq!(found.version, 9999);
        assert!(
            elapsed.as_millis() < 1,
            "find_snapshot with 10k entries must complete in < 1ms, took {:?}",
            elapsed
        );
    }

    /// create_snapshot adds to both indexes.
    #[test]
    fn test_create_snapshot_adds_to_both_indexes() {
        use crate::wal::{WalConfig, create_wal};
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("segments")).unwrap();
        let wal = create_wal(WalConfig::PerOp, dir.path(), 1).unwrap();
        let (_dir, mut store) = make_store();
        store.set_wal(wal);

        store.create_snapshot(Some("my-snap".to_string())).unwrap();

        // Lookup by version
        let by_ver = store.find_snapshot("1").expect("must find by version=1");
        assert_eq!(by_ver.version, 1);
        // Lookup by name
        let by_name = store.find_snapshot("my-snap").expect("must find by name");
        assert_eq!(by_name.name.as_deref(), Some("my-snap"));
    }

    /// set_snapshots populates both by_version and by_name indexes correctly.
    #[test]
    fn test_set_snapshots_populates_both_indexes() {
        let snaps = vec![
            make_snapshot(10, Some("ten")),
            make_snapshot(20, None),
            make_snapshot(30, Some("thirty")),
        ];
        let (_dir, mut store) = make_store();
        store.set_snapshots(snaps);

        assert_eq!(store.find_snapshot("10").unwrap().version, 10);
        assert_eq!(store.find_snapshot("twenty").is_none(), true);
        assert_eq!(store.find_snapshot("ten").unwrap().version, 10);
        assert_eq!(store.find_snapshot("thirty").unwrap().version, 30);
        assert!(store.find_snapshot("20").is_some());  // unnamed, find by version
        assert!(store.find_snapshot("20_name").is_none());
    }

    /// list_snapshots returns sorted by version (existing behavior preserved).
    #[test]
    fn test_hashmap_list_snapshots_sorted() {
        let snaps = vec![
            make_snapshot(3, None),
            make_snapshot(1, None),
            make_snapshot(2, None),
        ];
        let (_dir, mut store) = make_store();
        store.set_snapshots(snaps);

        let list = store.list_snapshots();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].version, 1);
        assert_eq!(list[1].version, 2);
        assert_eq!(list[2].version, 3);
    }

    // ── Saturating refcount tests (FIX-01) ────────────────────────────────────

    #[test]
    fn test_refcount_saturates() {
        let (_dir, store) = make_store();
        let digest: ObjectKey = ObjectKey::example(0xDE);

        // Manually set refcount to u64::MAX - 1
        {
            let mut rc = store.refcounts.lock();
            rc.insert(digest, u64::MAX - 1);
        }

        // One more increment brings it to u64::MAX
        store.increment_refcount(&digest);
        assert_eq!(store.get_refcount(&digest), u64::MAX,
            "refcount should be u64::MAX after increment from MAX-1");

        // Another increment must stay at u64::MAX (not wrap to 0)
        store.increment_refcount(&digest);
        assert_eq!(store.get_refcount(&digest), u64::MAX,
            "refcount at u64::MAX must not wrap on increment");
    }

    #[test]
    fn test_refcount_decrement_saturated() {
        let (_dir, store) = make_store();
        let digest: ObjectKey = ObjectKey::example(0xAB);

        // Set refcount to u64::MAX directly
        {
            let mut rc = store.refcounts.lock();
            rc.insert(digest, u64::MAX);
        }

        // Decrement must be a no-op for saturated blocks
        store.decrement_refcount(&digest);
        assert_eq!(store.get_refcount(&digest), u64::MAX,
            "decrement on saturated (u64::MAX) refcount must be a no-op — block is immortal");
    }

    #[test]
    fn test_saturated_refcount_count() {
        let (_dir, store) = make_store();
        let d1: ObjectKey = ObjectKey::example(0x01);
        let d2: ObjectKey = ObjectKey::example(0x02);
        let d3: ObjectKey = ObjectKey::example(0x03);
        let d4: ObjectKey = ObjectKey::example(0x04);
        let d5: ObjectKey = ObjectKey::example(0x05);

        {
            let mut rc = store.refcounts.lock();
            rc.insert(d1, u64::MAX); // saturated
            rc.insert(d2, u64::MAX); // saturated
            rc.insert(d3, 5);        // normal
            rc.insert(d4, 2);        // normal
            rc.insert(d5, 1);        // normal
        }

        assert_eq!(store.saturated_refcount_count(), 2,
            "should count exactly 2 saturated refcounts");
    }

    // ── inode_count AtomicU64 tests (FIX-02 store layer) ─────────────────────

    #[test]
    fn test_inode_count_empty() {
        let (_dir, store) = make_store();
        // Fresh store has root inode (ino=1) — count should be 1
        assert_eq!(store.inode_count(), 1,
            "fresh store should have inode_count == 1 (root inode)");
    }

    #[test]
    fn test_inode_count_tracks_lifecycle() {
        let (_dir, store) = make_store();
        // Create 3 inodes (inos 2, 3, 4) — total = 1 root + 3 = 4
        store.create_inode(&new_file_meta()).unwrap();
        store.create_inode(&new_file_meta()).unwrap();
        let ino3 = store.create_inode(&new_file_meta()).unwrap();
        assert_eq!(store.inode_count(), 4,
            "after creating 3 inodes, count should be 4 (root + 3)");

        // Delete one -> should be 3
        store.delete_inode(ino3).unwrap();
        assert_eq!(store.inode_count(), 3,
            "after deleting 1 inode, count should be 3");
    }

    #[test]
    fn test_inode_count_after_reload() {
        let (_dir, store) = make_store();
        // Create 2 inodes
        store.create_inode(&new_file_meta()).unwrap();
        store.create_inode(&new_file_meta()).unwrap();
        let count_before = store.inode_count();
        assert_eq!(count_before, 3, "should have 3 inodes (root + 2)");

        // Commit and reload
        let root = store.commit().unwrap();
        let io = Arc::clone(store.io());
        let reloaded = DictMetadataStore::load_from_root(io, &root).unwrap();

        assert_eq!(reloaded.inode_count(), count_before,
            "inode_count should match original after load_from_root round-trip");
    }
}

