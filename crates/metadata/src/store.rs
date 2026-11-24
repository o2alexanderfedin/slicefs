//! `DictMetadataStore` — MetadataStore whose nodes are in the chunk store.
//!
//! All state is held in-memory as `Mutex`-guarded maps; `commit()` stores the
//! changed nodes, syncs the chunk store, then writes a `RootUpdate` to the WAL
//! for crash-safe ordering.
//!
//! Locking order (always acquire in this order to prevent deadlocks):
//!   `inode_map`, then one `inode_live` shard, then `inode_data`;
//!   one `dir_live` shard, then `dir_data`. The other maps are taken alone.
//!   No hot-path call holds the `io` lock; a commit takes it briefly.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, atomic::{AtomicU64, AtomicUsize, Ordering}};

use chunk_store::lock_stats::{self, Site};
use chunk_store::sync::{self, Rank};
use slicefs_traits::key::{ObjectKey, KEY_BYTES};
use crate::store_io::{NodeRead, NodeReader, NodeWrite, NodeWriter};
use chunk_store::ChunkStore;
use slicefs_traits::metadata::{DirEntry, InodeId, InodeMeta, MetaError, MetadataStore};

use crate::snapshot::SnapshotEntry;
use crate::store_io::StoreIo;
use crate::wal::{Lsn, WalCut, WalEntry, WalError, WalStrategy};

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
    /// Entries that a commit (at its cut, #108) or a cache flush took and now
    /// writes with no lock held, each with the ticket of its take. Until the
    /// writer installs their digests, the digest maps hold an older one, so
    /// these stay in the cache. A change after the take removes the entry
    /// (it is dirty again, and newer); a later take gives it a new ticket.
    /// A writer installs its digest only if the entry still has its ticket.
    flushing: HashMap<u64, u64>,
    /// The last ticket given.
    tickets: u64,
}

impl<T> Default for Live<T> {
    fn default() -> Self {
        Live { map: HashMap::new(), dirty: Default::default(), flushing: HashMap::new(), tickets: 0 }
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
        self.map.retain(|ino, _| dirty.contains(ino) || flushing.contains_key(ino));
    }

    /// A take failed (a commit): what is in flight is dirty again.
    fn requeue(&mut self) {
        let flushing: Vec<u64> = self.flushing.drain().map(|(ino, _)| ino).collect();
        self.dirty.extend(flushing);
    }

    /// True if `ino` still has the ticket `t` of a take (not changed or
    /// removed since): it is no longer in flight, and the caller installs
    /// the digest it wrote.
    fn settle(&mut self, ino: u64, t: u64) -> bool {
        if self.flushing.get(&ino) == Some(&t) {
            self.flushing.remove(&ino);
            true
        } else {
            false
        }
    }
}

impl<T: Clone> Live<T> {
    /// The dirty entries, plus the entries in flight if `in_flight` (a
    /// commit must have them all): each goes in flight with a new ticket.
    fn take(&mut self, in_flight: bool) -> Vec<(u64, u64, T)> {
        let mut inos: Vec<u64> = self.dirty.drain().collect();
        if in_flight {
            inos.extend(self.flushing.keys().copied());
        }
        let mut out = Vec::with_capacity(inos.len());
        for ino in inos {
            if let Some(v) = self.map.get(&ino) {
                self.tickets += 1;
                self.flushing.insert(ino, self.tickets);
                out.push((ino, self.tickets, v.clone()));
            }
        }
        out
    }
}

/// Shards of the inode and the directory cache: threads that work on
/// inodes of different shards take different locks.
const SHARDS: usize = 32;

/// A cache split by inode number into `SHARDS` locks (`Rank::Store`). A
/// thread holds at most one shard of a cache at a time. `len` and `dirty`
/// count over all shards; a shard guard updates them before it unlocks.
struct Sharded<T> {
    shards: Vec<sync::Mutex<Live<T>>>,
    len: AtomicUsize,
    dirty: AtomicUsize,
}

/// One locked shard (derefs to its `Live`).
struct Shard<'a, T> {
    g: sync::MutexGuard<'a, Live<T>>,
    of: &'a Sharded<T>,
    /// `map.len()` and `dirty.len()` when it was locked.
    at: (usize, usize),
}

impl<T> Sharded<T> {
    fn new(site: Site) -> Self {
        Sharded {
            shards: (0..SHARDS).map(|_| lk(Live::default()).timed(site)).collect(),
            len: AtomicUsize::new(0),
            dirty: AtomicUsize::new(0),
        }
    }

    /// The shard of `ino`, locked.
    fn lock(&self, ino: u64) -> Shard<'_, T> {
        self.lock_at((ino % SHARDS as u64) as usize)
    }

    fn lock_at(&self, i: usize) -> Shard<'_, T> {
        let g = self.shards[i].lock();
        let at = (g.map.len(), g.dirty.len());
        Shard { g, of: self, at }
    }

    /// Cached entries, over all shards.
    fn len(&self) -> usize {
        self.len.load(Ordering::Relaxed)
    }

    /// Dirty entries, over all shards.
    fn dirty(&self) -> usize {
        self.dirty.load(Ordering::Relaxed)
    }

    /// `Live::evict_clean` on each shard, one at a time.
    fn evict_clean(&self) {
        for i in 0..SHARDS {
            self.lock_at(i).evict_clean();
        }
    }

    /// `Live::requeue` on each shard, one at a time.
    fn requeue(&self) {
        for i in 0..SHARDS {
            self.lock_at(i).requeue();
        }
    }
}

impl<T: Clone> Sharded<T> {
    /// What a commit writes: `Live::take(true)` of each shard.
    fn take_all(&self) -> Vec<(u64, u64, T)> {
        let mut out = Vec::new();
        for i in 0..SHARDS {
            out.extend(self.lock_at(i).take(true));
        }
        out
    }
}

impl<T> std::ops::Deref for Shard<'_, T> {
    type Target = Live<T>;
    fn deref(&self) -> &Live<T> {
        &self.g
    }
}

impl<T> std::ops::DerefMut for Shard<'_, T> {
    fn deref_mut(&mut self) -> &mut Live<T> {
        &mut self.g
    }
}

impl<T> Drop for Shard<'_, T> {
    // Runs before the guard field unlocks, so the counts move under the lock.
    fn drop(&mut self) {
        fn moved(c: &AtomicUsize, old: usize, new: usize) {
            if new > old {
                c.fetch_add(new - old, Ordering::Relaxed);
            } else if old > new {
                c.fetch_sub(old - new, Ordering::Relaxed);
            }
        }
        moved(&self.of.len, self.at.0, self.g.map.len());
        moved(&self.of.dirty, self.at.1, self.g.dirty.len());
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
    /// With the ticket of each take (see `Live::flushing`).
    inodes: Vec<(u64, u64, InodeMeta)>,
    dirs: Vec<(u64, u64, DirList)>,
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
            self.store.inode_live.requeue();
            self.store.dir_live.requeue();
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
/// Locking order: see the module comment.
/// A lock of the store internals (rank 6, see `chunk_store::sync`).
fn lk<T>(v: T) -> sync::Mutex<T> {
    sync::Mutex::new(Rank::Store, v)
}

/// A sync run before a root is logged (see `DictMetadataStore::set_pre_root_sync`).
pub type PreRootSync = Arc<dyn Fn() -> std::io::Result<()> + Send + Sync>;

pub struct DictMetadataStore {
    /// Node store — used for all put_node/get_node calls.
    io: Arc<Mutex<StoreIo>>,
    /// The chunk store of `io`: hot-path node reads and writes use it with
    /// no lock held (`NodeReader`, a `NodeWriter` per call).
    cs: Arc<ChunkStore>,
    /// Hot-path node puts since the last commit sync, and the first put
    /// error since then: the next commit syncs them, or fails with the error
    /// (the `StoreIo` lock is not on the hot path).
    unsynced_puts: AtomicU64,
    put_error: sync::Mutex<Option<std::io::Error>>,
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
    /// Sharded by inode number. Lock order: a `dir_live` shard, `dir_data`.
    /// No shard lock is held across a node read, a hash or a put.
    dir_live: Sharded<DirList>,
    /// Inodes in use, with a dirty flag. `inode_data` (and `inode_map`) hold the
    /// digest as of the last flush; a dirty inode here is newer. `commit()`
    /// writes the dirty ones. Sharded by inode number. Lock order:
    /// `inode_map`, an `inode_live` shard, `inode_data`. No shard lock is held
    /// across a node read, a hash or a put.
    inode_live: Sharded<InodeMeta>,
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
        let cs = Arc::clone(lock_stats::lock(&io, Site::Io).store());
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
            cs,
            unsynced_puts: AtomicU64::new(0),
            put_error: lk(None),
            inode_map: lk(inode_map).timed(Site::InodeMap),
            inode_data: lk(inode_data).timed(Site::InodeData),
            dir_data: lk(dir_data).timed(Site::DirData),
            dir_live: Sharded::new(Site::DirLive),
            inode_live: Sharded::new(Site::InodeLive),
            changes: AtomicU64::new(0),
            committed_changes: AtomicU64::new(0),
            manifest_data: lk(BTreeMap::new()).timed(Site::ManifestData),
            xattr_data: lk(BTreeMap::new()).timed(Site::XattrData),
            pre_root_sync: lk(None),
            refcounts: lk(BTreeMap::new()).timed(Site::Refcounts),
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
        if let Some(w) = self.wal_handle() {
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

    /// Take the next new inode number and insert nothing, so a caller can
    /// lock the number's stripe before the inode is linked (mkdir, D-08),
    /// then create it with `create_directory_at`. A number reserved and not
    /// used is skipped, as a failed create skips one; replay never sees it.
    pub fn reserve_ino(&self) -> InodeId {
        self.inode_map.lock().allocate_ino()
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
        let lsn = self.log_append(bytes);
        self.log_write_out(lsn);
    }

    /// First half of `log_op` (LOCK-04): put the record in the write log's
    /// memory queue and return its LSN. Memory only (a copy and a short
    /// queue lock), so a request calls it under the locks of its apply.
    /// `None`: no write log, or the append failed (then fsync fails until
    /// the next commit).
    pub fn log_append(&self, bytes: Vec<u8>) -> Option<Lsn> {
        let w = self.wal_handle()?;
        match w.append(&WalEntry::Op { bytes }) {
            Ok(lsn) => Some(lsn),
            Err(e) => {
                self.note_wal_failure(&e);
                None
            }
        }
    }

    /// Second half of `log_op`: write the records up to `lsn` out (PerOp:
    /// into the segment file and the kernel). A request calls it after it
    /// released its apply locks and before it replies. Does nothing for
    /// `None`.
    pub fn log_write_out(&self, lsn: Option<Lsn>) {
        let (Some(lsn), Some(w)) = (lsn, self.wal_handle()) else { return };
        if let Err(e) = w.write_out(lsn) {
            self.note_wal_failure(&e);
        }
    }

    /// The in-memory state is already correct. A failed append or write-out
    /// is reported by every later flush_wal (fsync) until the next commit
    /// starts a new segment (#20).
    fn note_wal_failure(&self, e: &WalError) {
        eprintln!("write log append failed (fsync fails until the next commit): {e}");
        self.wal_fail_seq.fetch_add(1, Ordering::SeqCst);
    }

    fn log_wal_entry(&self, entry: &WalEntry) {
        // A clone of the handle: the `wal` slot lock is not held while the
        // log writes or syncs.
        if let Some(w) = self.wal_handle() {
            if let Err(e) = w.log_mutation(entry) {
                self.note_wal_failure(&e);
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

    /// The shard of directory `ino`, locked, with its entry list cached. A
    /// cold list is read with no lock held and cached only if it is still
    /// absent and its digest did not change meanwhile (see `cached_inode`).
    fn cached_dir(&self, ino: InodeId) -> Result<Shard<'_, DirList>, MetaError> {
        let mut cold: Option<(ObjectKey, DirList)> = None;
        loop {
            let mut live = self.dir_live.lock(ino);
            if live.map.contains_key(&ino) {
                return Ok(live);
            }
            let digest = self.dir_data.lock().get(&ino).copied()
                .ok_or(MetaError::NotADirectory(ino))?;
            if let Some((d, list)) = cold.take() {
                if d == digest {
                    live.map.insert(ino, list);
                    return Ok(live);
                }
            }
            drop(live);
            let list = DirList::load(&mut self.reader(), &digest)?;
            cold = Some((digest, list));
        }
    }

    /// Run `f` on the cached entry list of directory `ino` (read from the store
    /// on first use). `f` returns `(result, changed)`; a changed list is marked
    /// dirty and written by the next flush. The flush and the eviction run
    /// after the shard lock is released.
    fn with_dir<R>(
        &self,
        ino: InodeId,
        f: impl FnOnce(&mut DirList) -> Result<(R, bool), MetaError>,
    ) -> Result<R, MetaError> {
        let mut live = self.cached_dir(ino)?;
        let (r, changed) = f(live.map.get_mut(&ino).expect("cached_dir caches it"))?;
        if changed {
            live.mark_dirty(ino);
        }
        drop(live);
        let (flush, full) = (self.dir_live.dirty() >= DIRTY_FLUSH, self.dir_live.len() > DIR_CACHE_MAX);
        if flush || full {
            self.flush_dirs();
        }
        if full {
            self.dir_live.evict_clean();
        }
        Ok(r)
    }

    /// The inode cache, locked, with `ino` in it. A cold inode is read with
    /// no cache lock held; it is cached only if it is still absent and its
    /// digest did not change meanwhile (else a delete or a newer flush in
    /// between would be undone). Lock order: `inode_map`, `inode_live`,
    /// `inode_data`.
    fn cached_inode(&self, ino: InodeId) -> Result<Shard<'_, InodeMeta>, MetaError> {
        let mut cold: Option<(ObjectKey, InodeMeta)> = None;
        loop {
            let mut live = self.inode_live.lock(ino);
            if live.map.contains_key(&ino) {
                return Ok(live);
            }
            let digest = self.inode_data.lock().get(&ino).copied()
                .ok_or(MetaError::NotFound(ino))?;
            if let Some((d, meta)) = cold.take() {
                if d == digest {
                    live.map.insert(ino, meta);
                    return Ok(live);
                }
            }
            drop(live);
            let meta = load_inode(&mut self.reader(), &digest)?;
            cold = Some((digest, meta));
        }
    }

    /// After a change of the inode cache, with no cache lock held: write the
    /// dirty entries (`flush`), and drop the clean ones when it is `full`.
    fn trim_inodes(&self, flush: bool, full: bool) {
        if flush || full {
            self.flush_inodes();
        }
        if full {
            self.inode_live.evict_clean();
        }
    }

    /// Current metadata of `ino`: the cached copy, or read from the store.
    fn load_meta(&self, ino: InodeId) -> Result<InodeMeta, MetaError> {
        let live = self.cached_inode(ino)?;
        let meta = live.map[&ino].clone();
        drop(live);
        self.trim_inodes(false, self.inode_live.len() > INODE_CACHE_MAX);
        Ok(meta)
    }

    /// Change inode `ino` in one hold of its cache lock (D-05): `f` gets the
    /// current metadata, and what it leaves is the new (dirty) state. Two
    /// threads that change one inode never lose a change. `logical_bytes`
    /// follows the size change in the same hold. `f` must not call the store
    /// or take a lock. `NotFound` if there is no such inode.
    pub fn modify_inode<R>(&self, ino: InodeId, f: impl FnOnce(&mut InodeMeta) -> R) -> Result<R, MetaError> {
        self.changes.fetch_add(1, Ordering::Relaxed);
        let mut live = self.cached_inode(ino)?;
        let m = live.map.get_mut(&ino).expect("cached_inode caches it");
        let old_size = m.size;
        let r = f(m);
        debug_assert_eq!(m.ino, ino, "modify_inode must not change the inode number");
        let new_size = m.size;
        live.mark_dirty(ino);
        self.add_size(old_size, new_size);
        drop(live);
        self.trim_inodes(self.inode_live.dirty() >= DIRTY_FLUSH, self.inode_live.len() > INODE_CACHE_MAX);
        Ok(r)
    }

    /// `logical_bytes` after an inode size went from `old` to `new`
    /// (saturating: never below 0).
    fn add_size(&self, old: u64, new: u64) {
        if new > old {
            self.logical_bytes.fetch_add(new - old, Ordering::Relaxed);
        } else if old > new {
            self.logical_bytes.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                Some(cur.saturating_sub(old - new))
            }).ok();
        }
    }

    /// Cache `meta` as a new dirty inode: number `at` (`AlreadyExists` if
    /// it is taken; new numbers are above it afterwards), or a new number.
    /// The number is taken and the inode cached under `inode_map`, so no
    /// number is given twice. A new inode is only in the cache until a flush.
    fn insert_inode(&self, at: Option<InodeId>, meta: &InodeMeta) -> Result<InodeId, MetaError> {
        let mut map = self.inode_map.lock();
        let ino = at.unwrap_or_else(|| map.allocate_ino());
        let mut live = self.inode_live.lock(ino);
        if at.is_some() {
            if map.get(ino).is_some() || live.map.contains_key(&ino)
                || self.inode_data.lock().contains_key(&ino)
            {
                return Err(MetaError::AlreadyExists(ino));
            }
            if map.next_ino() <= ino {
                map.set_next_ino(ino.saturating_add(1));
            }
        }
        let mut m = meta.clone();
        m.ino = ino;
        let size = m.size;
        live.map.insert(ino, m);
        live.mark_dirty(ino);
        drop(live);
        drop(map);
        self.add_size(0, size);
        self.inode_count.fetch_add(1, Ordering::Relaxed);
        self.trim_inodes(self.inode_live.dirty() >= DIRTY_FLUSH, self.inode_live.len() > INODE_CACHE_MAX);
        Ok(ino)
    }

    /// `create_directory` with number `at`, or a new number.
    fn make_directory(
        &self,
        parent_ino: InodeId,
        name: &str,
        at: Option<InodeId>,
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
        let mut dir_meta = meta.clone();
        // Ensure mode has directory type bit
        if dir_meta.mode & S_IFMT != S_IFDIR {
            dir_meta.mode |= S_IFDIR;
        }
        // Take the number first; `inode_map` is released before the parent
        // directory is read or changed (it may come from disk).
        let ino = self.insert_inode(at, &dir_meta)?;
        // Then the name: if it already exists, the new inode goes again.
        if let Err(e) = self.with_dir(parent_ino, |d| d.add(name, ino).map(|()| ((), true))) {
            let _ = self.delete_inode(ino);
            return Err(e);
        }
        {
            let mut live = self.dir_live.lock(ino);
            live.map.insert(ino, DirList::new_dir(ino, parent_ino));
            live.mark_dirty(ino);
        }
        // The parent gains a link (the ".." of the new directory).
        self.modify_inode(parent_ino, |m| m.nlinks += 1)?;
        Ok(ino)
    }

    /// Write every dirty cached inode; record its digest in `inode_data` and `inode_map`.
    fn flush_inodes(&self) {
        self.flush_inodes_between(|| ());
    }

    /// `flush_inodes`, shard by shard: take the dirty entries (they go in
    /// flight), write them with no shard lock held, then install each digest
    /// only if the entry still has the ticket of this take (it was not
    /// changed or deleted meanwhile; else a late install would bring back
    /// an older or a deleted inode). `between` runs once, after the first
    /// write and before its install (a test hook point).
    fn flush_inodes_between(&self, between: impl FnOnce()) {
        let mut between = Some(between);
        for i in 0..SHARDS {
            let taken = self.inode_live.lock_at(i).take(false);
            if taken.is_empty() {
                continue;
            }
            let mut w = self.writer();
            let written: Vec<(u64, u64, ObjectKey)> =
                taken.into_iter().map(|(ino, t, meta)| (ino, t, intern_inode(&mut w, &meta))).collect();
            self.kept(w);
            if let Some(f) = between.take() {
                f();
            }
            let mut inode_map = self.inode_map.lock();
            let mut live = self.inode_live.lock_at(i);
            let mut inode_data = self.inode_data.lock();
            for (ino, t, d) in written {
                if live.settle(ino, t) {
                    inode_data.insert(ino, d);
                    inode_map.insert(ino, d);
                }
            }
        }
    }

    /// True if `ino` is a directory known to the store.
    fn is_dir(&self, ino: InodeId) -> bool {
        self.dir_live.lock(ino).map.contains_key(&ino)
            || self.dir_data.lock().contains_key(&ino)
    }

    /// Write every dirty cached directory and record its digest in
    /// `dir_data`; shard by shard, as `flush_inodes_between` does.
    fn flush_dirs(&self) {
        for i in 0..SHARDS {
            let taken = self.dir_live.lock_at(i).take(false);
            if taken.is_empty() {
                continue;
            }
            let mut w = self.writer();
            let written: Vec<(u64, u64, ObjectKey)> =
                taken.into_iter().map(|(ino, t, list)| (ino, t, list.intern(&mut w))).collect();
            self.kept(w);
            let mut live = self.dir_live.lock_at(i);
            let mut dir_data = self.dir_data.lock();
            for (ino, t, d) in written {
                if live.settle(ino, t) {
                    dir_data.insert(ino, d);
                }
            }
        }
    }

    /// A node writer for one hot-path call (no lock held); give it back to
    /// `kept` after the puts.
    fn writer(&self) -> NodeWriter {
        NodeWriter::new(Arc::clone(&self.cs))
    }

    /// Node reads with no lock held.
    fn reader(&self) -> NodeReader {
        NodeReader(Arc::clone(&self.cs))
    }

    /// Count the puts of `w` for the next commit sync, and keep its first
    /// put error (the next commit returns it).
    fn kept(&self, mut w: NodeWriter) {
        self.unsynced_puts.fetch_add(w.puts(), Ordering::Relaxed);
        if let Err(e) = w.result() {
            let mut k = self.put_error.lock();
            if k.is_none() {
                *k = Some(e);
            }
        }
    }

    /// For a commit that syncs the chunk store next: the put error kept
    /// since the last commit, else the hot-path puts count as synced.
    fn begin_put_sync(&self) -> std::io::Result<()> {
        if let Some(e) = self.put_error.lock().take() {
            return Err(e);
        }
        self.unsynced_puts.store(0, Ordering::Relaxed);
        Ok(())
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
        let dirs = self.dir_live.take_all();
        let inode_map = self.inode_map.lock().clone();
        let inodes = self.inode_live.take_all();
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
        let cs = Arc::clone(&self.cs);
        let mut w = NodeWriter::new(Arc::clone(&cs));
        // Write the changed directories and inodes, so the digest maps are current.
        let mut new_inodes = Vec::with_capacity(p.inodes.len());
        for (ino, t, meta) in &p.inodes {
            let d = intern_inode(&mut w, meta);
            p.inode_data.insert(*ino, d);
            p.inode_map.insert(*ino, d);
            new_inodes.push((*ino, *t, d));
        }
        let mut new_dirs = Vec::with_capacity(p.dirs.len());
        for (ino, t, list) in &p.dirs {
            let d = list.intern(&mut w);
            p.dir_data.insert(*ino, d);
            new_dirs.push((*ino, *t, d));
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
            for (ino, t, d) in new_inodes {
                let mut live = self.inode_live.lock(ino);
                if live.settle(ino, t) {
                    self.inode_data.lock().insert(ino, d);
                    inode_map.insert(ino, d);
                }
            }
        }
        for (ino, t, d) in new_dirs {
            let mut live = self.dir_live.lock(ino);
            if live.settle(ino, t) {
                self.dir_data.lock().insert(ino, d);
            }
        }

        // Make every object written since the last commit durable before the
        // root that refers to them is logged: after that, log rotation deletes
        // the segments whose records hold the same data (#20). A put error of
        // a request since the last commit fails the commit too: hot-path puts
        // (kept in the store) and puts of other `StoreIo` users.
        self.begin_put_sync()
            .map_err(|e| MetaError::Corrupted(format!("commit: object sync failed: {e}")))?;
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

impl DictMetadataStore {
    /// Store the manifest node of `blocks` and return its digest, with no
    /// lock held (hash and put; the Phase B of a rebase step, D-02). A put
    /// error is kept for the next commit, as for every node write.
    pub fn prepare_manifest(&self, blocks: &[ObjectKey]) -> Result<ObjectKey, MetaError> {
        let mut w = self.writer();
        let digest = intern_manifest(&mut w, blocks);
        self.kept(w);
        Ok(digest)
    }

    /// The manifest digest of `ino`, if it has one: memory only, under the
    /// short manifest map lock.
    pub fn manifest_digest(&self, ino: InodeId) -> Option<ObjectKey> {
        self.manifest_data.lock().get(&ino).copied()
    }

    /// The entries of the manifest node `digest` (a node read, no lock of
    /// this store held across it).
    pub fn manifest_entries(&self, digest: &ObjectKey) -> Result<Vec<ObjectKey>, MetaError> {
        load_manifest(&mut self.reader(), digest)
    }

    /// Make `digest` (from `prepare_manifest`) the manifest of `ino`: memory
    /// only, under the short manifest map lock.
    pub fn set_manifest_digest(&self, ino: InodeId, digest: ObjectKey) -> Result<(), MetaError> {
        self.changes.fetch_add(1, Ordering::Relaxed);
        self.manifest_data.lock().insert(ino, digest);
        Ok(())
    }

    /// Change the xattr set of `ino` with `f`: the set is read and stored
    /// with no lock held; the new digest goes in only if the old one is
    /// still current (else `f` runs again on the newer set), so a change
    /// that ran meanwhile is not lost.
    fn change_xattrs(
        &self,
        ino: InodeId,
        mut f: impl FnMut(&mut Vec<(String, Vec<u8>)>) -> Result<(), MetaError>,
    ) -> Result<(), MetaError> {
        loop {
            let old = self.xattr_data.lock().get(&ino).copied();
            let mut xattrs = match old {
                Some(d) => load_xattrs(&mut self.reader(), &d)?,
                None => vec![],
            };
            f(&mut xattrs)?;
            let mut w = self.writer();
            let new = intern_xattrs(&mut w, &xattrs);
            self.kept(w);
            let mut map = self.xattr_data.lock();
            if map.get(&ino).copied() == old {
                map.insert(ino, new);
                return Ok(());
            }
        }
    }
}

impl MetadataStore for DictMetadataStore {
    fn create_inode(&self, meta: &InodeMeta) -> Result<InodeId, MetaError> {
        self.changes.fetch_add(1, Ordering::Relaxed);
        // The inode is written at the next flush; until then only the cache has it.
        self.insert_inode(None, meta)
    }

    fn create_inode_at(&self, ino: InodeId, meta: &InodeMeta) -> Result<InodeId, MetaError> {
        self.changes.fetch_add(1, Ordering::Relaxed);
        self.insert_inode(Some(ino), meta)
    }

    fn get_inode(&self, ino: InodeId) -> Result<InodeMeta, MetaError> {
        self.load_meta(ino)
    }

    fn update_inode(&self, meta: &InodeMeta) -> Result<(), MetaError> {
        // The whole value is replaced (see `modify_inode` for a change of a
        // few fields that must not lose a concurrent one).
        self.modify_inode(meta.ino, |m| *m = meta.clone())
    }

    fn delete_inode(&self, ino: InodeId) -> Result<(), MetaError> {
        self.changes.fetch_add(1, Ordering::Relaxed);
        // Capture size before removing so we can decrement logical_bytes.
        let size = self.load_meta(ino)?.size;

        // The cache entry and the digest go in one hold of the cache lock:
        // a cold load that runs meanwhile then cannot cache the inode again.
        {
            let mut inode_map = self.inode_map.lock();
            let mut live = self.inode_live.lock(ino);
            live.remove(ino);
            self.inode_data.lock().remove(&ino);
            inode_map.remove(ino);
        }
        // Drop every per-inode map entry: a leftover manifest/xattr/dir entry
        // would keep the deleted content reachable (never garbage-collected).
        self.manifest_data.lock().remove(&ino);
        self.xattr_data.lock().remove(&ino);
        {
            let mut live = self.dir_live.lock(ino);
            live.remove(ino);
            self.dir_data.lock().remove(&ino);
        }

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
        self.make_directory(parent_ino, name, None, meta)
    }

    fn create_directory_at(
        &self,
        parent_ino: InodeId,
        name: &str,
        ino: InodeId,
        meta: &InodeMeta,
    ) -> Result<InodeId, MetaError> {
        self.make_directory(parent_ino, name, Some(ino), meta)
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
        let digest = self.prepare_manifest(blocks)?;
        self.set_manifest_digest(ino, digest)
    }

    fn get_manifest(&self, ino: InodeId) -> Result<Vec<ObjectKey>, MetaError> {
        let manifest_data = self.manifest_data.lock();
        let digest = manifest_data.get(&ino).copied().ok_or(MetaError::NotFound(ino))?;
        drop(manifest_data);
        load_manifest(&mut self.reader(), &digest)
    }

    fn set_xattr(&self, ino: InodeId, name: &str, value: &[u8]) -> Result<(), MetaError> {
        self.changes.fetch_add(1, Ordering::Relaxed);
        self.change_xattrs(ino, |xattrs| {
            set_xattr_entry(xattrs, name, value);
            Ok(())
        })
    }

    fn get_xattr(&self, ino: InodeId, name: &str) -> Result<Vec<u8>, MetaError> {
        let digest = self.xattr_data.lock().get(&ino).copied().ok_or(MetaError::NotFound(ino))?;
        let xattrs = load_xattrs(&mut self.reader(), &digest)?;
        get_xattr_entry(&xattrs, name).ok_or(MetaError::NotFound(ino))
    }

    fn list_xattrs(&self, ino: InodeId) -> Result<Vec<String>, MetaError> {
        let digest = match self.xattr_data.lock().get(&ino).copied() {
            Some(d) => d,
            None => return Ok(vec![]),
        };
        let xattrs = load_xattrs(&mut self.reader(), &digest)?;
        Ok(list_xattr_names(&xattrs))
    }

    fn remove_xattr(&self, ino: InodeId, name: &str) -> Result<(), MetaError> {
        self.changes.fetch_add(1, Ordering::Relaxed);
        if !self.xattr_data.lock().contains_key(&ino) {
            return Err(MetaError::NotFound(ino));
        }
        self.change_xattrs(ino, |xattrs| {
            if remove_xattr_entry(xattrs, name) { Ok(()) } else { Err(MetaError::NotFound(ino)) }
        })
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

        let cs = Arc::clone(io_guard.store());
        drop(io_guard);

        Ok(DictMetadataStore {
            io,
            cs,
            unsynced_puts: AtomicU64::new(0),
            put_error: lk(None),
            inode_map: lk(inode_map).timed(Site::InodeMap),
            inode_data: lk(inode_data).timed(Site::InodeData),
            dir_data: lk(dir_data).timed(Site::DirData),
            dir_live: Sharded::new(Site::DirLive),
            inode_live: Sharded::new(Site::InodeLive),
            changes: AtomicU64::new(0),
            committed_changes: AtomicU64::new(0),
            manifest_data: lk(manifest_data).timed(Site::ManifestData),
            xattr_data: lk(xattr_data).timed(Site::XattrData),
            pre_root_sync: lk(None),
            refcounts: lk(refcounts).timed(Site::Refcounts),
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
        assert!(store.inode_live.len() <= INODE_CACHE_MAX + 1);
        assert!(store.dir_live.len() <= DIR_CACHE_MAX + 1);
        // Dirty entries are written in batches, so the final commit stays small.
        assert!(store.inode_live.dirty() < DIRTY_FLUSH);
        assert!(store.dir_live.dirty() < DIRTY_FLUSH);
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

    // D-05: four threads that each add 1 to nlinks 500 times lose no change.
    #[test]
    fn atomic_modify_inode_loses_no_change() {
        let (_d, store) = make_store();
        let ino = store.create_inode(&new_file_meta()).unwrap();
        let start = store.get_inode(ino).unwrap().nlinks;
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    for _ in 0..500 {
                        store.modify_inode(ino, |m| m.nlinks += 1).unwrap();
                    }
                });
            }
        });
        assert_eq!(store.get_inode(ino).unwrap().nlinks, start + 2000);
    }

    #[test]
    fn missing_inode_modify_inode_is_not_found_and_creates_nothing() {
        let (_d, store) = make_store();
        assert!(matches!(store.modify_inode(99, |m| m.size = 5), Err(MetaError::NotFound(99))));
        assert!(matches!(store.get_inode(99), Err(MetaError::NotFound(99))));
        assert_eq!(store.inode_count(), 1);
    }

    #[test]
    fn size_change_in_modify_inode_moves_logical_bytes() {
        let (_d, store) = make_store();
        let ino = store.create_inode(&new_file_meta()).unwrap();
        store.modify_inode(ino, |m| m.size = 1000).unwrap();
        assert_eq!(store.logical_bytes(), 1000);
        store.modify_inode(ino, |m| m.size = 300).unwrap();
        assert_eq!(store.logical_bytes(), 300);
        assert_eq!(store.modify_inode(ino, |m| m.size).unwrap(), 300);
    }

    // D-08 (store side): a create with a given number.
    #[test]
    fn create_inode_at_takes_the_number_and_raises_the_allocator() {
        let (_d, store) = make_store();
        assert_eq!(store.create_inode_at(7, &new_file_meta()).unwrap(), 7);
        assert_eq!(store.get_inode(7).unwrap().ino, 7);
        assert!(store.create_inode(&new_file_meta()).unwrap() >= 8);
        assert!(matches!(store.create_inode_at(7, &new_file_meta()), Err(MetaError::AlreadyExists(7))));
        assert!(matches!(store.create_inode_at(1, &new_file_meta()), Err(MetaError::AlreadyExists(1))));
        // Taken after a commit and a reload too (the digest maps have it).
        let root = store.commit().unwrap();
        let reloaded = DictMetadataStore::load_from_root(Arc::clone(store.io()), &root).unwrap();
        assert!(matches!(reloaded.create_inode_at(7, &new_file_meta()), Err(MetaError::AlreadyExists(7))));
        assert_eq!(store.inode_count(), 3);
    }

    #[test]
    fn create_directory_at_adds_the_entry_or_nothing() {
        let (_d, store) = make_store();
        let root_links = store.get_inode(1).unwrap().nlinks;
        assert_eq!(store.create_directory_at(1, "d", 9, &new_dir_meta()).unwrap(), 9);
        assert_eq!(store.lookup(1, "d").unwrap(), 9);
        let names: Vec<(String, u64)> = store.list_directory(9).unwrap().into_iter().map(|e| (e.name, e.ino)).collect();
        assert!(names.contains(&(".".into(), 9)) && names.contains(&("..".into(), 1)), "{names:?}");
        assert_eq!(store.get_inode(1).unwrap().nlinks, root_links + 1);
        assert!(store.create_directory(1, "e", &new_dir_meta()).unwrap() >= 10);
        let count = store.inode_count();
        // A taken number: nothing is added.
        assert!(matches!(store.create_directory_at(1, "x", 9, &new_dir_meta()), Err(MetaError::AlreadyExists(9))));
        assert!(store.lookup(1, "x").is_err());
        // An existing name: the number stays free, nothing is added.
        assert!(store.create_directory_at(1, "d", 20, &new_dir_meta()).is_err());
        assert!(matches!(store.get_inode(20), Err(MetaError::NotFound(20))));
        assert_eq!(store.lookup(1, "d").unwrap(), 9);
        assert_eq!(store.inode_count(), count);
        assert_eq!(store.get_inode(1).unwrap().nlinks, root_links + 2);
    }

    /// A store on a `MemIo` chunk store (for its hold knobs); also its `io`.
    fn mem_store(mem: &chunk_store::mem::MemIo) -> (Arc<Mutex<StoreIo>>, DictMetadataStore) {
        let cs = chunk_store::ChunkStore::open_with(Arc::new(mem.clone()), chunk_store::Options::default()).unwrap();
        let io = Arc::new(Mutex::new(StoreIo::new("mem", Arc::new(cs))));
        (Arc::clone(&io), DictMetadataStore::new(io))
    }

    /// The longest wait of a test for one step.
    const LIMIT: std::time::Duration = std::time::Duration::from_secs(5);

    // LOCK-01: thread A loads a directory that is not cached, and its pack
    // read is held. Thread B changes a cached inode, creates one, and looks
    // up a name in another (cached) directory: none of it waits for A.
    #[test]
    fn cold_dir_load_does_not_block_other_inodes() {
        use chunk_store::mem::{Hold, MemIo};
        use std::sync::mpsc;
        let mem = MemIo::new();
        let (io, store) = mem_store(&mem);
        let a_dir = store.create_directory(1, "a", &new_dir_meta()).unwrap();
        let b_dir = store.create_directory(1, "b", &new_dir_meta()).unwrap();
        let f = store.create_inode(&new_file_meta()).unwrap();
        store.link(a_dir, "n", f).unwrap();
        store.link(b_dir, "m", f).unwrap();
        let shard = |ino: u64| ino % SHARDS as u64;
        assert!(shard(a_dir) != shard(b_dir) && shard(a_dir) != shard(f), "other shards");
        let root = store.commit().unwrap();
        // A new store from the root: every cache is empty.
        let store = DictMetadataStore::load_from_root(io, &root).unwrap();
        assert_eq!(store.lookup(b_dir, "m").unwrap(), f);
        store.get_inode(f).unwrap();
        mem.hold_pack_reads(true);
        std::thread::scope(|s| {
            let (ta, ra) = mpsc::channel();
            let store = &store;
            s.spawn(move || {
                let _ = ta.send(store.lookup(a_dir, "n"));
            });
            let parked = mem.wait_parked(Hold::PackReads, 1, LIMIT);
            let (tb, rb) = mpsc::channel();
            s.spawn(move || {
                let changed = store.modify_inode(f, |m| m.nlinks += 1).is_ok();
                let created = store.create_inode(&new_file_meta()).is_ok();
                let _ = tb.send((changed, created, store.lookup(b_dir, "m").ok()));
            });
            let b = rb.recv_timeout(LIMIT);
            let a_waited = ra.try_recv().is_err();
            mem.hold_pack_reads(false);
            assert!(parked, "A is in the held pack read");
            assert_eq!(b.expect("B finished while A loaded a cold directory"), (true, true, Some(f)));
            assert!(a_waited, "A was still in the cold load while B ran");
            assert_eq!(ra.recv_timeout(LIMIT).expect("A ends after the release").unwrap(), f);
        });
    }

    // Pitfall 7: a flush wrote inode X with no lock held, and X is deleted
    // before the flush installs its digest. The late install must not bring
    // X back. The hook point runs on this thread: no timing.
    #[test]
    fn flush_install_does_not_resurrect_a_deleted_inode() {
        let (_d, store) = make_store();
        let x = store.create_inode(&new_file_meta()).unwrap();
        store.flush_inodes_between(|| store.delete_inode(x).unwrap());
        assert!(matches!(store.get_inode(x), Err(MetaError::NotFound(_))));
        assert!(!store.inode_data.lock().contains_key(&x));
        assert!(store.inode_map.lock().get(x).is_none());
        let root = store.commit().unwrap();
        let reloaded = DictMetadataStore::load_from_root(Arc::clone(store.io()), &root).unwrap();
        assert!(matches!(reloaded.get_inode(x), Err(MetaError::NotFound(_))));
    }

    // A commit took X (size 1) at its cut, then X changed (size 2) and a
    // flush took it. Before the flush installs, the commit installs its
    // older digest. The older digest must not end up as the current one.
    #[test]
    fn late_older_install_does_not_overwrite_a_newer_change() {
        let (_d, store) = make_store();
        let x = store.create_inode(&new_file_meta()).unwrap();
        store.modify_inode(x, |m| m.size = 1).unwrap();
        let p = store.begin_commit().unwrap();
        store.modify_inode(x, |m| m.size = 2).unwrap();
        let st = &store;
        st.flush_inodes_between(move || {
            st.finish_commit(p).unwrap();
        });
        // Drop the clean entries: X is read back through its digest.
        store.inode_live.evict_clean();
        assert_eq!(store.get_inode(x).unwrap().size, 2);
        let root = store.commit().unwrap();
        let reloaded = DictMetadataStore::load_from_root(Arc::clone(store.io()), &root).unwrap();
        assert_eq!(reloaded.get_inode(x).unwrap().size, 2);
    }

    // LOCK-01: thread A reads a manifest node and its pack read is held.
    // Thread B changes another inode and stores another manifest: no
    // `StoreIo` lock (or other lock) makes it wait for A.
    #[test]
    fn manifest_read_does_not_block_inode_changes() {
        use chunk_store::mem::{Hold, MemIo};
        use std::sync::mpsc;
        let mem = MemIo::new();
        let (io, store) = mem_store(&mem);
        let f1 = store.create_inode(&new_file_meta()).unwrap();
        let f2 = store.create_inode(&new_file_meta()).unwrap();
        store.set_manifest(f1, &[ObjectKey::example(1)]).unwrap();
        let root = store.commit().unwrap();
        let store = DictMetadataStore::load_from_root(io, &root).unwrap();
        store.get_inode(f2).unwrap();
        mem.hold_pack_reads(true);
        std::thread::scope(|s| {
            let store = &store;
            let (ta, ra) = mpsc::channel();
            s.spawn(move || {
                let _ = ta.send(store.get_manifest(f1));
            });
            let parked = mem.wait_parked(Hold::PackReads, 1, LIMIT);
            let (tb, rb) = mpsc::channel();
            s.spawn(move || {
                let changed = store.modify_inode(f2, |m| m.size = 10).is_ok();
                let stored = store.set_manifest(f2, &[ObjectKey::example(2)]).is_ok();
                let _ = tb.send((changed, stored));
            });
            let b = rb.recv_timeout(LIMIT);
            let a_waited = ra.try_recv().is_err();
            mem.hold_pack_reads(false);
            assert!(parked, "A is in the held pack read");
            assert_eq!(b.expect("B finished while A read a manifest"), (true, true));
            assert!(a_waited, "A was still in the manifest read while B ran");
            let got = ra.recv_timeout(LIMIT).expect("A ends after the release").unwrap();
            assert_eq!(got, vec![ObjectKey::example(1)]);
        });
        assert_eq!(store.get_manifest(f2).unwrap(), vec![ObjectKey::example(2)]);
    }

    // T-19-10: a node put error on the hot path is returned by the next
    // commit; and a commit syncs every node that the hot path put.
    #[test]
    fn hot_path_put_error_fails_the_next_commit_and_puts_are_synced() {
        use chunk_store::mem::MemIo;
        let mem = MemIo::new();
        let (_io, store) = mem_store(&mem);
        let f = store.create_inode(&new_file_meta()).unwrap();
        store.commit().unwrap();
        mem.set_space_left(Some(0));
        store.set_manifest(f, &[ObjectKey::example(1)]).unwrap();
        mem.set_space_left(None);
        assert!(store.commit().is_err(), "the kept put error fails the commit");
        store.set_manifest(f, &[ObjectKey::example(2)]).unwrap();
        let root = store.commit().unwrap();
        // Only what is durable is left after a crash: the root and the
        // manifest that the hot path put must read back.
        mem.crash();
        let cs = chunk_store::ChunkStore::open_with(Arc::new(mem.clone()), chunk_store::Options::default()).unwrap();
        let io = Arc::new(Mutex::new(StoreIo::new("mem", Arc::new(cs))));
        let reloaded = DictMetadataStore::load_from_root(io, &root).unwrap();
        assert_eq!(reloaded.get_manifest(f).unwrap(), vec![ObjectKey::example(2)]);
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

