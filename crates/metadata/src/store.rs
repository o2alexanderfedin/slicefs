//! `DictMetadataStore` — MetadataStore whose nodes are in the chunk store.
//!
//! All state is held in-memory as `Mutex`-guarded maps; `commit()` stores the
//! changed nodes, syncs the chunk store, then writes a `RootUpdate` to the WAL
//! for crash-safe ordering.
//!
//! Locking order (always acquire in this order to prevent deadlocks):
//!   `inode_alloc`, then one `inode_live` shard, then `inode_data`;
//!   one `dir_live` shard, then `dir_data`. The other maps are taken alone.
//!   No hot-path call holds the `io` lock; a commit takes it briefly.
//!   The `prolly::NodeCache` lock (`Rank::Store`) is a leaf: no other lock, read,
//!   decode or callback runs under it, so it may be taken under any lock above.
//!   Map trees (`pmap`): `commit_lock`, then `apply_lock`, then one overlay or
//!   live shard, then a tree root lock. `begin_commit` holds `apply_lock` only
//!   briefly: never across the write-log cut or a return to the file system.
//!   A flush is never called with a shard lock held; it holds `apply_lock`
//!   (`try_lock`) across its node reads and puts.
//!   A refcount change (read-modify-write) holds one `refcounts` overlay
//!   shard only for its check and edit (`MapTree::modify`); its tree read
//!   runs with no lock held.
//!   An xattr call holds one `xattrs` overlay shard (the shard of its
//!   inode) only to read or write one edit, or to copy the pending keys of
//!   the inode for a scan (`MapTree::scan_keys`); the tree reads, the spill
//!   of a large value and its ranged reads run with no lock held.
//!   The test flush hook of a tree (`pmap`, `Rank::Store`) is a leaf: it is
//!   cloned out and called with no tree lock held.
//!   The commit stats lock (`last_stats`, `Rank::Store`, untimed) is a leaf: it
//!   is set once at the end of a successful commit and cloned by
//!   `last_commit_stats`; no other store-rank lock and no I/O runs under it.

use std::collections::{BTreeMap, HashMap};
use std::ops::ControlFlow;
use std::sync::{Arc, Mutex, atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering}};

use chunk_store::lock_stats::{self, Site};
use chunk_store::sync::{self, Rank};
use slicefs_traits::key::{ObjectKey, KEY_BYTES};
use crate::store_io::{NodeRead, NodeReader, NodeWrite, NodeWriter};
use chunk_store::ChunkStore;
use slicefs_traits::metadata::{DirEntry, InodeId, InodeMeta, MetaError, MetadataStore};

use crate::snapshot::SnapshotEntry;
use crate::store_io::StoreIo;
use crate::pmap::{MapTree, Taken, TreeIo};
use crate::wal::{Lsn, WalCut, WalEntry, WalError, WalStrategy};

use crate::inode::{intern_inode, load_inode};
use crate::inode_alloc::InodeAlloc;
use crate::directory::{create_dir_entries, DirList};
use crate::manifest::{intern_manifest, load_manifest};
use crate::xattr::{fits_inline, xattr_range, XattrKey, XattrValue};

// S_IFDIR bit mask (POSIX directory type)
const S_IFMT: u32 = 0o0_170_000;

/// Byte layout of the root record that `commit` writes: fixed-width fields,
/// little-endian. Each `*_DATA` field is the key of a serialized map.
pub(crate) mod root_rec {
    use super::KEY_BYTES as D;
    pub const ROOT_DIR_INO: usize = 0;
    pub const NEXT_INO: usize = ROOT_DIR_INO + 8;
    pub const INODE_DATA: usize = NEXT_INO + 8;
    pub const DIR_DATA: usize = INODE_DATA + D;
    pub const MANIFEST_DATA: usize = DIR_DATA + D;
    pub const XATTR_DATA: usize = MANIFEST_DATA + D;
    pub const REFCOUNTS: usize = XATTR_DATA + D;
    pub const LOGICAL_BYTES: usize = REFCOUNTS + D;
    pub const INODE_COUNT: usize = LOGICAL_BYTES + 8;
    /// Total size in bytes (197 with 33-byte keys).
    pub const SIZE: usize = INODE_COUNT + 8;
}

/// One entry of a `u64` <-> digest map: a `u64` and a digest.
pub(crate) const MAP_ENTRY: usize = 8 + KEY_BYTES;

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
/// A map tree is flushed (its overlay applied to the working root) once
/// this many of its entries are dirty.
#[cfg(not(test))]
const TREE_FLUSH: usize = 16_384;
#[cfg(test)]
const TREE_FLUSH: usize = 8;

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
    /// The nodes are written, nothing is synced. The root record is put; the
    /// trees have no new roots yet (the commit fails, its edits are requeued).
    BeforeSync = 1,
    /// The nodes and the content are synced, the root is not logged.
    BeforeRoot = 2,
    /// Every tree has applied its edits (nodes written), the root record is
    /// not put. The trees have no new roots yet.
    AfterApply = 3,
    /// The trees have their new roots and the overlays are settled; the chunk
    /// store is not synced. The edits are in the working roots: a failure here
    /// loses nothing, and the next commit syncs and logs them.
    AfterInstall = 4,
    /// The chunk store is synced, the file content (pre-root sync) is not.
    BeforePreRootSync = 5,
}

/// The work one commit did on one map tree (see [`CommitStats`]).
#[derive(Clone, Debug, Default)]
pub struct TreeStats {
    /// The map name (`manifest_data`, ...).
    pub name: &'static str,
    /// Levels of the tree after the commit (1 for a leaf root).
    pub height: u8,
    /// Entries of the tree after the commit.
    pub entries: u64,
    /// Counts of the apply of the commit's edits (all zero if there were none).
    pub apply: prolly::ApplyStats,
}

/// The work of the last successful commit (`DictMetadataStore::last_commit_stats`).
/// It counts the commit's own tree applies and the root record. Edits that a
/// flush wrote to a tree before the commit are not in it.
#[derive(Clone, Debug, Default)]
pub struct CommitStats {
    /// Tree nodes loaded by the commit's applies (cache hits included).
    pub nodes_read: u64,
    /// Nodes written: the tree applies plus the root record.
    pub nodes_written: u64,
    /// Bytes written: the tree applies plus the 197-byte root record.
    pub bytes_written: u64,
    /// Of `nodes_read`, the nodes the cache gave.
    pub cache_hits: u64,
    /// Wall time of `begin_commit` plus the time of `finish_commit` up to the
    /// chunk store sync, in milliseconds (the time between the two calls is
    /// not in it).
    pub presync_ms: f64,
    /// One entry per map tree.
    pub trees: Vec<TreeStats>,
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
    /// The map trees have their new roots (`finish_inner`). Else (an error
    /// before that, or dropped) their overlays are requeued and the
    /// prepared state ends.
    installed: bool,
    changes: u64,
    /// `wal_fail_seq` at the cut: failures up to it end with this root.
    fail_seq: u64,
    /// With the ticket of each take (see `Live::flushing`).
    inodes: Vec<(u64, u64, InodeMeta)>,
    dirs: Vec<(u64, u64, DirList)>,
    next_ino: u64,
    inode_data: BTreeMap<u64, ObjectKey>,
    dir_data: BTreeMap<u64, ObjectKey>,
    /// The manifest tree at the cut and the edits taken from its overlay.
    manifest_root: chunk_store::ObjectId,
    manifest: Taken<u64, ObjectKey>,
    /// The refcount tree at the cut and the edits taken from its overlay.
    refcount_root: chunk_store::ObjectId,
    refcounts: Taken<ObjectKey, u64>,
    /// The xattr tree at the cut and the edits taken from its overlay.
    xattr_root: chunk_store::ObjectId,
    xattrs: Taken<XattrKey, XattrValue>,
    logical_bytes: u64,
    inode_count: u64,
    snapshots: Vec<SnapshotEntry>,
    wal: Option<Arc<dyn WalStrategy>>,
    cut: Option<WalCut>,
    /// Wall time of `begin_commit`, in milliseconds (for `CommitStats::presync_ms`).
    begin_ms: f64,
}

impl Drop for PreparedCommit<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.store.requeue_all(self.installed);
        }
    }
}

fn check_name(name: &str) -> Result<(), MetaError> {
    if name.len() > NAME_MAX {
        return Err(MetaError::NameTooLong(name.len()));
    }
    Ok(())
}
const S_IFDIR: u32 = 0o0_040_000;

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
    /// Dirty entries of a map tree at which the hot path flushes it
    /// (`TREE_FLUSH`; tests lower it with `test_set_tree_flush`).
    tree_flush: AtomicUsize,
    put_error: sync::Mutex<Option<std::io::Error>>,
    /// Inode-number allocator.
    inode_alloc: sync::Mutex<InodeAlloc>,
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
    /// Inodes in use, with a dirty flag. `inode_data` holds the
    /// digest as of the last flush; a dirty inode here is newer. `commit()`
    /// writes the dirty ones. Sharded by inode number. Lock order:
    /// `inode_alloc`, an `inode_live` shard, `inode_data`. No shard lock is held
    /// across a node read, a hash or a put.
    inode_live: Sharded<InodeMeta>,
    /// Count of mutations so far, and its value at the last commit: the
    /// commit timer commits only when they differ.
    changes: AtomicU64,
    committed_changes: AtomicU64,
    /// Maps file inode number → current manifest `ObjectKey`: a prolly tree
    /// (key: the ino, 8 bytes big endian; value: the 33-byte key) and an
    /// overlay of the changes not in the tree yet (see `pmap`). Its locks
    /// are timed as `Site::ManifestData`.
    manifest: MapTree<u64, ObjectKey>,
    /// The chunk store, node cache and config of the map trees.
    trees: TreeIo,
    /// One tree apply at a time: a hot-path flush holds it (`try_lock`)
    /// across its node reads and puts; `begin_commit` takes it briefly to
    /// wait out a flush and set `prepared`; `finish_commit` takes it to
    /// install the new roots. Rank::Store, untimed.
    apply_lock: sync::Mutex<()>,
    /// A commit is between its cut and the install of its roots: flushes
    /// skip, so no edit after the cut reaches the committed roots. Read and
    /// written under `apply_lock`.
    prepared: AtomicBool,
    /// The xattrs, one entry per name: a prolly tree (key: the ino, 8 bytes
    /// big endian, then the name bytes; value: a tag, then the value inline
    /// or the key of a spilled value, see `xattr`) and an overlay sharded by
    /// the ino. Its locks are timed as `Site::XattrData`.
    xattrs: MapTree<XattrKey, XattrValue>,
    /// Called in `commit()` after the metadata objects are synced and before
    /// the root is logged: makes the file content (chunk store) durable.
    pre_root_sync: sync::Mutex<Option<PreRootSync>>,
    /// Reference counts for content `ObjectKey`s: a prolly tree (key: the
    /// 33-byte content key; value: the count, 8 bytes little endian) and an
    /// overlay (see `pmap`). Incremented when a manifest references a block;
    /// decremented on manifest replacement or inode deletion. Its locks are
    /// timed as `Site::Refcounts`.
    refcounts: MapTree<ObjectKey, u64>,
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
    /// The work of the last successful commit. A leaf lock (see the lock-order comment).
    last_stats: sync::Mutex<CommitStats>,
    /// Snapshot index by version number — O(1) lookup (FIX-03).
    snapshots_by_version: sync::Mutex<HashMap<u64, SnapshotEntry>>,
    /// Snapshot name → version mapping — O(1) name lookup (FIX-04).
    snapshots_by_name: sync::Mutex<HashMap<String, u64>>,
}

impl DictMetadataStore {
    /// Create a new store with a root directory at inode 1.
    pub fn new(io: Arc<Mutex<StoreIo>>) -> Self {
        // Build root inode + dir entries via put_node, then flush
        let (cs, trees) = {
            let g = lock_stats::lock(&io, Site::Io);
            (Arc::clone(g.store()), TreeIo::new(Arc::clone(g.store()), g.node_cache(), g.tree_config()))
        };
        // The empty manifest tree; a put error fails the next commit (as a
        // hot-path put error does).
        let (empty, put_error) = match prolly::empty_root(&cs) {
            Ok(r) => (r, None),
            Err(e) => (chunk_store::ObjectId(ObjectKey::ZERO.0), Some(std::io::Error::other(e))),
        };
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

        let mut inode_data = BTreeMap::new();
        inode_data.insert(1, root_inode_digest);

        let mut dir_data = BTreeMap::new();
        dir_data.insert(1, root_dir_digest);

        DictMetadataStore {
            io,
            cs,
            unsynced_puts: AtomicU64::new(0),
            tree_flush: AtomicUsize::new(TREE_FLUSH),
            put_error: lk(put_error),
            inode_alloc: lk(InodeAlloc::new()).timed(Site::InodeAlloc),
            inode_data: lk(inode_data).timed(Site::InodeData),
            dir_data: lk(dir_data).timed(Site::DirData),
            dir_live: Sharded::new(Site::DirLive),
            inode_live: Sharded::new(Site::InodeLive),
            changes: AtomicU64::new(0),
            committed_changes: AtomicU64::new(0),
            manifest: MapTree::new("manifest_data", Site::ManifestData, empty),
            trees,
            apply_lock: lk(()),
            prepared: AtomicBool::new(false),
            xattrs: MapTree::new("xattr_data", Site::XattrData, empty),
            pre_root_sync: lk(None),
            refcounts: MapTree::new("refcounts", Site::Refcounts, empty),
            logical_bytes: AtomicU64::new(0),
            inode_count: AtomicU64::new(1),
            wal: lk(None).timed(Site::Wal),
            wal_fail_seq: AtomicU64::new(0),
            wal_fail_cleared: AtomicU64::new(0),
            commit_lock: sync::Mutex::new(Rank::Rename, ()),
            commit_fault: std::sync::atomic::AtomicU8::new(0),
            last_root: lk(None),
            last_stats: lk(CommitStats::default()),
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
        // A saturated count does not change.
        if self.change_refcount(digest, |c| (c != u64::MAX).then(|| c + 1)) == Some(u64::MAX) {
            tracing::warn!(
                "refcount saturated for block — block is now immortal and will not be GC'd"
            );
        }
    }

    /// Decrement the reference count for `digest` by 1.
    ///
    /// Removes the entry entirely when the count reaches 0.
    /// Does nothing if `digest` is not tracked (no edit is written).
    ///
    /// If the current count is `u64::MAX` (saturated / immortal), this is a
    /// no-op — immortal blocks are never freed.
    pub fn decrement_refcount(&self, digest: &ObjectKey) {
        self.changes.fetch_add(1, Ordering::Relaxed);
        self.change_refcount(digest, |c| (c != u64::MAX && c != 0).then(|| c - 1));
    }

    /// The read-modify-write of a count (the `pmap` protocol): `step` gets
    /// the count of `digest` (0: no entry) and gives the new count, or
    /// `None` to change nothing. A count of 0 is deleted. Gives the new
    /// count if it changed. A tree read error is logged and kept for the
    /// next commit (as a hot-path put error); the count does not change.
    fn change_refcount(&self, digest: &ObjectKey, step: impl Fn(u64) -> Option<u64>) -> Option<u64> {
        let mut tree = None;
        loop {
            let done = self.refcounts.modify(*digest, tree.take(), |cur| match step(cur.unwrap_or(0)) {
                Some(0) => (Some(None), Some(0)),
                Some(n) => (Some(Some(n)), Some(n)),
                None => (None, None),
            });
            if let Some(changed) = done {
                if changed.is_some() {
                    self.flush_trees();
                }
                return changed;
            }
            // Not in the overlay: read the tree with no lock held, then
            // check again (a flush or commit between them retries).
            match self.refcounts.read_with_generation(&self.trees, digest) {
                Ok(t) => tree = Some(t),
                Err(e) => {
                    tracing::warn!("refcount of {digest:?}: {e}");
                    let mut k = self.put_error.lock();
                    if k.is_none() {
                        *k = Some(std::io::Error::other(e.to_string()));
                    }
                    return None;
                }
            }
        }
    }

    /// Return the current reference count for `digest`, or `0` if not
    /// tracked (or if the tree read fails: the error is logged).
    pub fn get_refcount(&self, digest: &ObjectKey) -> u64 {
        match self.refcounts.read(&self.trees, digest) {
            Ok(c) => c.unwrap_or(0),
            Err(e) => {
                tracing::warn!("refcount of {digest:?}: {e}");
                0
            }
        }
    }

    /// Return the number of blocks whose reference count has saturated at `u64::MAX`.
    ///
    /// Saturated blocks are *immortal* — they will never be garbage-collected.
    /// A non-zero count here indicates the GC dead-letter queue has accumulated
    /// blocks; this is surfaced as a warning in `slicefs scrub`.
    ///
    /// A scrub-time scan: one cursor over the whole refcount tree plus the
    /// pending edits (`MapTree::for_each_value`), O(map) time and bounded
    /// memory. Exact when no request changes the store meanwhile.
    pub fn saturated_refcount_count(&self) -> Result<usize, MetaError> {
        let mut n = 0;
        self.refcounts.for_each_value(&self.trees, |c| n += usize::from(c == u64::MAX))?;
        Ok(n)
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
    /// may block and holding dict/inode_alloc locks would cause deadlocks.
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
        self.inode_alloc.lock().allocate_ino()
    }

    /// Make the next new inode number at least `ino` (never lower). Replay
    /// of the write log calls this before each create: a request that failed
    /// after it took an inode number (for example EEXIST) logged nothing, so
    /// replay skips that number the same way the live mount did.
    pub fn advance_next_ino(&self, ino: InodeId) {
        let mut m = self.inode_alloc.lock();
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
        if let Some(w) = self.wal_handle()
            && let Err(e) = w.log_mutation(entry)
        {
            self.note_wal_failure(&e);
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
            if let Some((d, list)) = cold.take()
                && d == digest
            {
                live.map.insert(ino, list);
                return Ok(live);
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
    /// between would be undone). Lock order: `inode_alloc`, `inode_live`,
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
            if let Some((d, meta)) = cold.take()
                && d == digest
            {
                live.map.insert(ino, meta);
                return Ok(live);
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
            self.logical_bytes.try_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                Some(cur.saturating_sub(old - new))
            }).ok();
        }
    }

    /// Cache `meta` as a new dirty inode: number `at` (`AlreadyExists` if
    /// it is taken; new numbers are above it afterwards), or a new number.
    /// The number is taken and the inode cached under `inode_alloc`, so no
    /// number is given twice. A new inode is only in the cache until a flush.
    fn insert_inode(&self, at: Option<InodeId>, meta: &InodeMeta) -> Result<InodeId, MetaError> {
        let mut alloc = self.inode_alloc.lock();
        let ino = at.unwrap_or_else(|| alloc.allocate_ino());
        let mut live = self.inode_live.lock(ino);
        if at.is_some() {
            if live.map.contains_key(&ino) || self.inode_data.lock().contains_key(&ino) {
                return Err(MetaError::AlreadyExists(ino));
            }
            if alloc.next_ino() <= ino {
                alloc.set_next_ino(ino.saturating_add(1));
            }
        }
        let mut m = meta.clone();
        m.ino = ino;
        let size = m.size;
        live.map.insert(ino, m);
        live.mark_dirty(ino);
        drop(live);
        drop(alloc);
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
        // Take the number first; `inode_alloc` is released before the parent
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

    /// Write every dirty cached inode; record its digest in `inode_data`.
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
            let mut live = self.inode_live.lock_at(i);
            let mut inode_data = self.inode_data.lock();
            for (ino, t, d) in written {
                if live.settle(ino, t) {
                    inode_data.insert(ino, d);
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

    /// Flush a map tree on the hot path at `n` dirty entries instead of
    /// `TREE_FLUSH` (integration tests build the library without `cfg(test)`,
    /// where the limit is 16,384).
    #[doc(hidden)]
    pub fn test_set_tree_flush(&self, n: usize) {
        self.tree_flush.store(n.max(1), Ordering::Relaxed);
    }

    /// Make the next hot-path flush of a map tree fail (its entries are
    /// requeued and the next commit fails with the error).
    #[doc(hidden)]
    pub fn test_fail_next_flush(&self) {
        self.manifest.fail_next_flush();
    }

    /// Run `f` in every flush of a map tree, between the install of the new
    /// root and the settle of the overlay (with the apply lock held).
    #[doc(hidden)]
    pub fn test_set_flush_hook(&self, f: impl Fn() + Send + Sync + 'static) {
        self.manifest.set_flush_hook(Some(Arc::new(f)));
    }

    /// Remove the hook of `test_set_flush_hook`.
    #[doc(hidden)]
    pub fn test_clear_flush_hook(&self) {
        self.manifest.set_flush_hook(None);
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

    /// A commit that took the overlays and live caches failed (or was
    /// dropped). What it took is dirty again, so the next commit writes it.
    /// `trees_installed`: the commit already installed the roots of the map
    /// trees (their edits are in the working roots, nothing to requeue, and
    /// the prepared state ended). Every map with an overlay is requeued here:
    /// plans that add a tree add it to this function.
    fn requeue_all(&self, trees_installed: bool) {
        if !trees_installed {
            self.manifest.requeue();
            self.refcounts.requeue();
            self.xattrs.requeue();
            self.end_prepared();
        }
        self.inode_live.requeue();
        self.dir_live.requeue();
    }

    /// End the prepared state of a commit (flushes may run again).
    fn end_prepared(&self) {
        let _apply = self.apply_lock.lock();
        self.prepared.store(false, Ordering::Relaxed);
    }

    /// Hot path, with no store lock held: flush each map tree that has
    /// enough dirty entries, all under one `apply_lock` hold. Skipped while
    /// another flush runs or a commit is prepared (the commit takes the
    /// entries). A failed apply leaves the entries dirty and fails the next
    /// commit.
    fn flush_trees(&self) {
        let limit = self.tree_flush.load(Ordering::Relaxed);
        if self.manifest.dirty() < limit && self.refcounts.dirty() < limit && self.xattrs.dirty() < limit {
            return;
        }
        let Some(apply) = self.apply_lock.try_lock() else { return };
        if self.prepared.load(Ordering::Relaxed) {
            return;
        }
        let mut results = Vec::with_capacity(3);
        if self.manifest.dirty() >= limit {
            results.push(self.manifest.flush(&self.trees, &apply));
        }
        if self.refcounts.dirty() >= limit {
            results.push(self.refcounts.flush(&self.trees, &apply));
        }
        if self.xattrs.dirty() >= limit {
            results.push(self.xattrs.flush(&self.trees, &apply));
        }
        for r in results {
            match r {
                Ok(Some(stats)) => {
                    self.unsynced_puts.fetch_add(stats.nodes_written, Ordering::Relaxed);
                }
                Ok(None) => {}
                Err(e) => {
                    let mut k = self.put_error.lock();
                    if k.is_none() {
                        *k = Some(std::io::Error::other(e.to_string()));
                    }
                }
            }
        }
    }

    /// First half of a commit (#108), run while no request changes the store
    /// (the file system holds its commit gate exclusively): take the dirty
    /// inodes and directories and a copy of every map, and cut the write log
    /// (`WalStrategy::cut`), so requests after this go to the next
    /// generation. Fast: no node is written and nothing is synced. Waits for
    /// a commit still in `finish_commit`.
    pub fn begin_commit(&self) -> Result<PreparedCommit<'_>, MetaError> {
        let started = std::time::Instant::now();
        let serial = self.commit_lock.lock();
        // Wait out a running tree flush, and stop new ones until the roots of
        // this commit are installed. The lock is not held across the cut: the
        // cut takes locks of the file system that rank below it.
        {
            let _apply = self.apply_lock.lock();
            self.prepared.store(true, Ordering::Relaxed);
        }
        let wal = self.wal_handle();
        // The log first: if it cannot cut, nothing has changed.
        let cut = match &wal {
            Some(w) => match w.cut() {
                Ok(c) => c,
                Err(e) => {
                    self.end_prepared();
                    return Err(MetaError::Corrupted(format!("commit: write-log cut failed: {e}")));
                }
            },
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
        let inodes = self.inode_live.take_all();
        let (manifest_root, manifest) = self.manifest.take_for_commit();
        // After the takes (IN-01).
        let next_ino = self.inode_alloc.lock().next_ino();
        let inode_data = self.inode_data.lock().clone();
        let dir_data = self.dir_data.lock().clone();
        let (refcount_root, refcounts) = self.refcounts.take_for_commit();
        let (xattr_root, xattrs) = self.xattrs.take_for_commit();
        Ok(PreparedCommit {
            _serial: serial,
            store: self,
            done: false,
            installed: false,
            changes,
            fail_seq,
            inodes,
            dirs,
            next_ino,
            inode_data,
            dir_data,
            manifest_root,
            manifest,
            refcount_root,
            refcounts,
            xattr_root,
            xattrs,
            logical_bytes: self.logical_bytes.load(Ordering::Relaxed),
            inode_count: self.inode_count.load(Ordering::Relaxed),
            snapshots: self.list_snapshots(),
            wal,
            cut,
            begin_ms: started.elapsed().as_secs_f64() * 1000.0,
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
        let finish_started = std::time::Instant::now();
        let cs = Arc::clone(&self.cs);
        let mut w = NodeWriter::new(Arc::clone(&cs));
        // Write the changed directories and inodes, so the digest maps are current.
        let mut new_inodes = Vec::with_capacity(p.inodes.len());
        for (ino, t, meta) in &p.inodes {
            let d = intern_inode(&mut w, meta);
            p.inode_data.insert(*ino, d);
            new_inodes.push((*ino, *t, d));
        }
        let mut new_dirs = Vec::with_capacity(p.dirs.len());
        for (ino, t, list) in &p.dirs {
            let d = list.intern(&mut w);
            p.dir_data.insert(*ino, d);
            new_dirs.push((*ino, *t, d));
        }

        // Serialize the state into a root record (layout: `root_rec`):
        //   root_dir_ino (u64), next_ino (u64), the keys of the
        //   inode, dir, manifest, xattr and refcount maps, logical_bytes (u64)
        //   and inode_count (u64).
        let next_ino = p.next_ino;
        let inode_data_digest = intern_u64_digest_map(&mut w, &p.inode_data);
        let dir_data_digest = intern_u64_digest_map(&mut w, &p.dir_data);
        // The map trees: the taken edits on the cut roots (no flush ran
        // since the cut, so the cut roots are still the working roots).
        let (manifest_root, manifest_apply) = self.manifest.apply_batch(&self.trees, &p.manifest_root, &p.manifest)?;
        let (refcount_root, refcount_apply) = self.refcounts.apply_batch(&self.trees, &p.refcount_root, &p.refcounts)?;
        let (xattr_root, xattr_apply) = self.xattrs.apply_batch(&self.trees, &p.xattr_root, &p.xattrs)?;
        let tree_stats = |name: &'static str, root: &chunk_store::ObjectId, apply: prolly::ApplyStats| {
            let ctx = self.trees.ctx();
            let tree_err = |e| crate::pmap::tree_err(name, e);
            Ok::<_, MetaError>(TreeStats {
                name,
                height: prolly::height(&ctx, root).map_err(tree_err)?,
                entries: prolly::count(&ctx, root).map_err(tree_err)?,
                apply,
            })
        };
        let manifest_stats = tree_stats("manifest_data", &manifest_root, manifest_apply)?;
        let refcount_stats = tree_stats("refcounts", &refcount_root, refcount_apply)?;
        let xattr_stats = tree_stats("xattr_data", &xattr_root, xattr_apply)?;
        self.injected_fault(CommitFault::AfterApply)?;
        let manifest_data_digest = crate::store_io::to_key(manifest_root);
        let xattr_data_digest = crate::store_io::to_key(xattr_root);
        let refcount_data_digest = crate::store_io::to_key(refcount_root);

        let mut root_bytes = Vec::with_capacity(root_rec::SIZE);
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

        // The trees get their new roots, and flushes may run again.
        {
            let apply = self.apply_lock.lock();
            self.manifest.install_and_settle(manifest_root, &p.manifest, &apply);
            self.refcounts.install_and_settle(refcount_root, &p.refcounts, &apply);
            self.xattrs.install_and_settle(xattr_root, &p.xattrs, &apply);
            p.installed = true;
            self.prepared.store(false, Ordering::Relaxed);
        }

        // The nodes are stored: the caches may forget the entries that were
        // not changed again since the cut.
        {
            for (ino, t, d) in new_inodes {
                let mut live = self.inode_live.lock(ino);
                if live.settle(ino, t) {
                    self.inode_data.lock().insert(ino, d);
                }
            }
        }
        for (ino, t, d) in new_dirs {
            let mut live = self.dir_live.lock(ino);
            if live.settle(ino, t) {
                self.dir_data.lock().insert(ino, d);
            }
        }

        self.injected_fault(CommitFault::AfterInstall)?;
        let presync_ms = p.begin_ms + finish_started.elapsed().as_secs_f64() * 1000.0;
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
        self.injected_fault(CommitFault::BeforePreRootSync)?;
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
        let trees = vec![manifest_stats, refcount_stats, xattr_stats];
        let stats = CommitStats {
            nodes_read: trees.iter().map(|t| t.apply.nodes_read).sum(),
            nodes_written: trees.iter().map(|t| t.apply.nodes_written).sum::<u64>() + 1,
            bytes_written: trees.iter().map(|t| t.apply.bytes_written).sum::<u64>() + root_rec::SIZE as u64,
            cache_hits: trees.iter().map(|t| t.apply.cache_hits).sum(),
            presync_ms,
            trees,
        };
        *self.last_stats.lock() = stats;
        Ok(root_digest)
    }

    /// The work of the last successful commit (all zero before the first).
    pub fn last_commit_stats(&self) -> CommitStats {
        self.last_stats.lock().clone()
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

    /// The manifest digest of `ino`, if it has one: the overlay, else a
    /// read of the manifest tree (often a cached node). Callers may hold
    /// namespace stripes across it. A read error is logged and gives
    /// `None` (a release that is lost so only leaks a refcount; GC is
    /// mark-based).
    pub fn manifest_digest(&self, ino: InodeId) -> Option<ObjectKey> {
        match self.manifest.read(&self.trees, &ino) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("manifest of inode {ino}: {e}");
                None
            }
        }
    }

    /// The entries of the manifest node `digest` (a node read, no lock of
    /// this store held across it).
    pub fn manifest_entries(&self, digest: &ObjectKey) -> Result<Vec<ObjectKey>, MetaError> {
        load_manifest(&mut self.reader(), digest)
    }

    /// Make `digest` (from `prepare_manifest`) the manifest of `ino`: in the
    /// overlay of the manifest tree (a flush may follow, see `flush_trees`).
    pub fn set_manifest_digest(&self, ino: InodeId, digest: ObjectKey) -> Result<(), MetaError> {
        self.changes.fetch_add(1, Ordering::Relaxed);
        self.manifest.write(ino, Some(digest));
        self.flush_trees();
        Ok(())
    }

    /// The entry of the xattr `name` of `ino`: the overlay, then the tree
    /// (one path of nodes). A name that `xattr::check_name` refuses is an
    /// error (it can have no entry, and its key could exceed `MAX_KEY`).
    fn xattr_entry(&self, ino: InodeId, name: &str) -> Result<XattrValue, MetaError> {
        crate::xattr::check_name(name)?;
        self.xattrs.read(&self.trees, &XattrKey::new(ino, name))?.ok_or(MetaError::NotFound(ino))
    }

    /// The length of the xattr `name` of `ino` (a size query), from its
    /// entry (inline) or the manifest root of its value (spilled): no value
    /// byte is read. Memory: one path of tree nodes and one manifest node.
    /// An absent name is `MetaError::NotFound(ino)`.
    pub fn xattr_len(&self, ino: InodeId, name: &str) -> Result<u64, MetaError> {
        let v = self.xattr_entry(ino, name)?;
        crate::xattr::value_len(&self.cs, &v)
    }

    /// Up to `size` bytes of the xattr `name` of `ino` from `offset` (fewer
    /// at the end, none past it). A spilled value reads only the manifest
    /// nodes and chunks of the range. Memory: the bytes asked for, one path
    /// of tree nodes and the reader of the range. An absent name is
    /// `MetaError::NotFound(ino)`.
    pub fn read_xattr(&self, ino: InodeId, name: &str, offset: u64, size: usize) -> Result<Vec<u8>, MetaError> {
        let v = self.xattr_entry(ino, name)?;
        crate::xattr::read_range(&self.cs, &v, offset, size)
    }

    /// Call `f` with each xattr name of `ino` in name byte order, streamed
    /// from a range scan of `ino BE .. (ino + 1) BE` merged with the pending
    /// edits (`MapTree::scan_keys`); `Break` stops it. Memory: the pending
    /// keys of the inode and one path of tree nodes, never a list of names.
    pub fn for_each_xattr_name(&self, ino: InodeId, f: &mut dyn FnMut(&str) -> ControlFlow<()>) -> Result<(), MetaError> {
        let (start, end) = xattr_range(ino);
        let mut bad = None;
        self.xattrs.scan_keys(&self.trees, crate::pmap::shard_of(ino), &start, end.as_deref(), &mut |k| {
            match crate::xattr::name_of(k) {
                Ok(n) => f(n),
                Err(e) => {
                    bad = Some(e);
                    ControlFlow::Break(())
                }
            }
        })?;
        bad.map_or(Ok(()), Err)
    }

    /// Delete every xattr of `ino`, pending and in the tree: scans of at
    /// most `tree_flush` keys from a resume key, a delete for each, a flush
    /// between them; no lock is held across a scan.
    fn delete_xattrs(&self, ino: InodeId) -> Result<(), MetaError> {
        let (mut from, end) = xattr_range(ino);
        let batch = self.tree_flush.load(Ordering::Relaxed).max(1);
        loop {
            let mut keys: Vec<Vec<u8>> = Vec::new();
            self.xattrs.scan_keys(&self.trees, crate::pmap::shard_of(ino), &from, end.as_deref(), &mut |k| {
                keys.push(k.to_vec());
                if keys.len() >= batch { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
            })?;
            let Some(last) = keys.last() else { return Ok(()) };
            // The next scan starts just after the last key.
            from = last.clone();
            from.push(0);
            for k in &keys {
                self.xattrs.write(XattrKey::from_encoded(k)?, None);
            }
            self.flush_trees();
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
            let mut live = self.inode_live.lock(ino);
            live.remove(ino);
            self.inode_data.lock().remove(&ino);
        }
        // Drop every per-inode map entry: a leftover manifest/xattr/dir entry
        // would keep the deleted content reachable (never garbage-collected).
        self.manifest.write(ino, None);
        self.delete_xattrs(ino)?;
        {
            let mut live = self.dir_live.lock(ino);
            live.remove(ino);
            self.dir_data.lock().remove(&ino);
        }

        // Decrement logical_bytes by the deleted inode's size.
        if size > 0 {
            self.logical_bytes.try_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                Some(cur.saturating_sub(size))
            }).ok();
        }

        // Decrement inode count (saturating to avoid underflow).
        self.inode_count.try_update(Ordering::Relaxed, Ordering::Relaxed, |c| {
            Some(c.saturating_sub(1))
        }).ok();
        self.flush_trees();

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
        let digest = self.manifest.read(&self.trees, &ino)?.ok_or(MetaError::NotFound(ino))?;
        load_manifest(&mut self.reader(), &digest)
    }

    /// A blind put of one entry (no read-modify-write; the file system
    /// orders the calls of one inode with its stripe). A value that does not
    /// fit the leaf entry is spilled first, with no lock held: the commit
    /// syncs the chunk store before the root that refers to it is logged.
    fn set_xattr(&self, ino: InodeId, name: &str, value: &[u8]) -> Result<(), MetaError> {
        crate::xattr::check_name(name)?;
        let v = if fits_inline(name.len(), value.len()) {
            XattrValue::Inline(value.to_vec())
        } else {
            XattrValue::Spilled(crate::xattr::spill(&self.cs, value)?)
        };
        self.changes.fetch_add(1, Ordering::Relaxed);
        self.xattrs.write(XattrKey::new(ino, name), Some(v));
        self.flush_trees();
        Ok(())
    }

    /// The whole value (the trait returns it). One entry read, then the
    /// value of that same entry; the file system uses the bounded calls
    /// (`xattr_len`, `read_xattr`, plan 26-16).
    fn get_xattr(&self, ino: InodeId, name: &str) -> Result<Vec<u8>, MetaError> {
        let v = self.xattr_entry(ino, name)?;
        crate::xattr::read_range(&self.cs, &v, 0, usize::MAX)
    }

    /// The names of `ino` in name byte order (a list: the file system uses
    /// `for_each_xattr_name`).
    fn list_xattrs(&self, ino: InodeId) -> Result<Vec<String>, MetaError> {
        let mut out = Vec::new();
        self.for_each_xattr_name(ino, &mut |n| {
            out.push(n.to_string());
            ControlFlow::Continue(())
        })?;
        Ok(out)
    }

    fn remove_xattr(&self, ino: InodeId, name: &str) -> Result<(), MetaError> {
        self.xattr_entry(ino, name)?;
        self.changes.fetch_add(1, Ordering::Relaxed);
        self.xattrs.write(XattrKey::new(ino, name), None);
        self.flush_trees();
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

/// The metadata values that the root record `root` refers to, each a file
/// value: the record itself and its flat maps, the inode and directory
/// nodes, the manifest nodes and the spilled xattr values (metadata, with no
/// refcount). The nodes of the map trees are not file values and are not
/// here (see [`tree_nodes`]); nor are file content keys (see
/// [`content_keys`]). The GC mark phase (#25) walks the object tree of each.
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
    let (mut nodes, mut content) = (Vec::new(), Vec::new());
    for_each_referenced(io, root, &mut |r, k| {
        match r {
            Ref::Node => nodes.push(k),
            Ref::Content => content.push(k),
            // Not a file value: see `tree_nodes`.
            Ref::Tree => {}
        }
        Ok(())
    })?;
    Ok((nodes, content))
}

/// What a key of [`for_each_referenced`] is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Ref {
    /// A metadata node (see [`referenced_roots`]).
    Node,
    /// A file content key (see [`content_keys`]).
    Content,
    /// A node of a map tree (see [`tree_nodes`]): one chunk-store object,
    /// not a file value.
    Tree,
}

/// The node IDs of the map trees of the root record `root` (each node once
/// per parent that refers to it), as keys. [`referenced_roots`] has only
/// file values; a tree node is one object (never open it as a file).
pub fn tree_nodes(io: &mut impl crate::store_io::NodeRead, root: &ObjectKey) -> Result<Vec<ObjectKey>, MetaError> {
    let cs = io.chunk_store();
    let bytes = read_root(cs, root)?;
    let at = |o: usize| parse_key(&bytes[o..o + KEY_BYTES]);
    let mut out = Vec::new();
    // The order of `for_each_referenced`: the manifest tree, the refcounts,
    // then the xattrs.
    for (name, o) in [
        ("manifest_data", root_rec::MANIFEST_DATA),
        ("refcounts", root_rec::REFCOUNTS),
        ("xattr_data", root_rec::XATTR_DATA),
    ] {
        walk_tree(cs, name, &at(o), &mut |id| {
            out.push(id);
            Ok(())
        }, &mut |_, _| Ok(()))?;
    }
    Ok(out)
}

/// A callback of [`walk_tree`] for each leaf entry (key, value).
type LeafFn<'a> = dyn FnMut(&[u8], &[u8]) -> Result<(), MetaError> + 'a;

/// Walk the tree `root` (map `name`) with one path of nodes and no cache
/// (the GC mark must not fill the node cache): `node` gets each node ID,
/// parent first; `leaf` each entry of each leaf. An error of a callback
/// stops the walk and is returned.
fn walk_tree(
    cs: &ChunkStore,
    name: &str,
    root: &ObjectKey,
    node: &mut dyn FnMut(ObjectKey) -> Result<(), MetaError>,
    leaf: &mut LeafFn<'_>,
) -> Result<(), MetaError> {
    let mut failed = None;
    let ctx = prolly::Ctx::uncached(cs);
    let done = prolly::visit(&ctx, &crate::store_io::to_id(root), &mut |v| {
        let r = node(crate::store_io::to_key(v.id))
            .and_then(|()| v.entries.iter().try_for_each(|(k, val)| leaf(k, val)));
        match r {
            Ok(()) => true,
            Err(e) => {
                failed = Some(e);
                false
            }
        }
    })
    .map_err(|e| crate::pmap::tree_err(name, e))?;
    match failed {
        Some(e) => Err(e),
        None if done => Ok(()),
        None => Err(MetaError::Corrupted(format!("{name} tree: walk stopped"))),
    }
}

/// The manifest key of a leaf entry of the manifest tree (an 8-byte ino
/// and a 33-byte key, else corrupt).
fn manifest_leaf(k: &[u8], v: &[u8]) -> Result<ObjectKey, MetaError> {
    if k.len() != 8 || v.len() != KEY_BYTES {
        return Err(MetaError::Corrupted(format!(
            "manifest_data tree: entry of a {}-byte key and a {}-byte value", k.len(), v.len()
        )));
    }
    Ok(parse_key(v))
}

/// Check a leaf entry of the refcount tree (a 33-byte key and an 8-byte
/// count, else corrupt).
fn refcount_leaf(k: &[u8], v: &[u8]) -> Result<(), MetaError> {
    if k.len() != KEY_BYTES || v.len() != 8 {
        return Err(MetaError::Corrupted(format!(
            "refcounts tree: entry of a {}-byte key and a {}-byte value", k.len(), v.len()
        )));
    }
    Ok(())
}

/// The spilled value key of a leaf entry of the xattr tree, if any (a key
/// of at least 9 bytes and a value that `XattrValue::decode` takes, else
/// corrupt).
fn xattr_leaf(k: &[u8], v: &[u8]) -> Result<Option<ObjectKey>, MetaError> {
    use crate::pmap::TreeValue;
    if k.len() < 9 {
        return Err(MetaError::Corrupted(format!("xattr_data tree: entry of a {}-byte key", k.len())));
    }
    match XattrValue::decode(v).map_err(|m| MetaError::Corrupted(format!("xattr_data tree: {m}")))? {
        XattrValue::Spilled(key) => Ok(Some(key)),
        XattrValue::Inline(_) => Ok(None),
    }
}

/// Call `f` with each key that the root record `root` refers to, without a
/// list of them (the GC mark, #207). A key can come more than once. The
/// map and manifest nodes are read in windows of [`WALK_WINDOW`] bytes of
/// whole entries, never whole, and the trees with one path of nodes: RAM
/// does not grow with the entry count. The tree nodes are `Ref::Tree`; a
/// spilled xattr value is `Ref::Node` (its manifest nodes and chunks).
pub(crate) fn for_each_referenced(
    io: &mut impl crate::store_io::NodeRead,
    root: &ObjectKey,
    f: &mut dyn FnMut(Ref, ObjectKey) -> Result<(), MetaError>,
) -> Result<(), MetaError> {
    let cs = io.chunk_store();
    let bytes = read_root(cs, root)?;
    let at = |o: usize| parse_key(&bytes[o..o + KEY_BYTES]);
    let (inodes, dirs, manifests) = (
        at(root_rec::INODE_DATA), at(root_rec::DIR_DATA), at(root_rec::MANIFEST_DATA),
    );
    for k in [*root, inodes, dirs] {
        f(Ref::Node, k)?;
    }
    for map in [inodes, dirs] {
        for_each_map_value(cs, &map, &mut |k| f(Ref::Node, k))?;
    }
    // The manifest tree: its nodes, and per leaf entry the manifest node and
    // its content keys. `f` is used by both callbacks, one at a time.
    let f = std::cell::RefCell::new(f);
    walk_tree(cs, "manifest_data", &manifests, &mut |id| (f.borrow_mut())(Ref::Tree, id), &mut |k, v| {
        let manifest = manifest_leaf(k, v)?;
        let mut f = f.borrow_mut();
        f(Ref::Node, manifest)?;
        for_each_manifest_key(cs, &manifest, &mut |k| f(Ref::Content, k))
    })?;
    // The refcount tree: its nodes only (a counted content key is not a
    // reference; the manifests give the live content).
    walk_tree(cs, "refcounts", &at(root_rec::REFCOUNTS), &mut |id| (f.borrow_mut())(Ref::Tree, id), &mut refcount_leaf)?;
    // The xattr tree: its nodes, and each spilled value.
    walk_tree(cs, "xattr_data", &at(root_rec::XATTR_DATA), &mut |id| (f.borrow_mut())(Ref::Tree, id), &mut |k, v| {
        match xattr_leaf(k, v)? {
            Some(key) => (f.borrow_mut())(Ref::Node, key),
            None => Ok(()),
        }
    })
}

/// About how many keys [`for_each_referenced`] gives to the sorter for
/// `root` (tree nodes go straight to the live set), from the sizes of its
/// maps (no map is read whole): the 3 file-value nodes of the root (the
/// root record and its flat maps), one key per map entry, for each entry
/// of the manifest tree (`prolly::count`, the root node only) its manifest
/// and one content key (a file with more extents gives more), and at most
/// one spilled value per entry of the xattr tree. For the temp space check
/// of the GC mark.
pub(crate) fn referenced_estimate(io: &mut impl crate::store_io::NodeRead, root: &ObjectKey) -> Result<u64, MetaError> {
    const WHAT: &str = "u64-digest map node";
    let cs = io.chunk_store();
    let bytes = read_root(cs, root)?;
    let at = |o: usize| parse_key(&bytes[o..o + KEY_BYTES]);
    let mut keys = 3u64;
    for off in [root_rec::INODE_DATA, root_rec::DIR_DATA] {
        let key = at(off);
        let r = chunk_store::file::FileReader::open(cs, crate::store_io::to_id(&key))
            .map_err(|e| crate::store_io::node_err(WHAT, &key, e))?;
        keys = keys.saturating_add(r.len().saturating_sub(8) / MAP_ENTRY as u64);
    }
    let manifests = prolly::count(&prolly::Ctx::uncached(cs), &crate::store_io::to_id(&at(root_rec::MANIFEST_DATA)))
        .map_err(|e| crate::pmap::tree_err("manifest_data", e))?;
    let xattrs = prolly::count(&prolly::Ctx::uncached(cs), &crate::store_io::to_id(&at(root_rec::XATTR_DATA)))
        .map_err(|e| crate::pmap::tree_err("xattr_data", e))?;
    Ok(keys.saturating_add(manifests.saturating_mul(2)).saturating_add(xattrs))
}

/// Bytes of a map or manifest node that [`for_each_referenced`] reads at a
/// time (rounded down to whole entries). Tiny in tests, so that a small
/// node takes many windows.
const WALK_WINDOW: usize = if cfg!(test) { 100 } else { 1 << 20 };

/// The bytes of the root record `root`; its size is checked before the read.
fn read_root(cs: &ChunkStore, root: &ObjectKey) -> Result<Vec<u8>, MetaError> {
    let err = |e| crate::store_io::node_err("root node", root, e);
    let mut r = chunk_store::file::FileReader::open(cs, crate::store_io::to_id(root)).map_err(err)?;
    check_root_len(usize::try_from(r.len()).unwrap_or(usize::MAX))?;
    let bytes = r.read(cs, 0, root_rec::SIZE).map_err(err)?;
    check_root_len(bytes.len())?;
    Ok(bytes)
}

/// Call `f` with each `rec`-byte record of the node `r` (key `key`) from
/// `off` to its end, read in windows of whole records. The caller checks
/// that the length fits.
fn for_each_window(
    cs: &ChunkStore,
    r: &mut chunk_store::file::FileReader,
    what: &str,
    mut off: u64,
    rec: usize,
    f: &mut dyn FnMut(&[u8]) -> Result<(), MetaError>,
) -> Result<(), MetaError> {
    let key = crate::store_io::to_key(r.key());
    let per = WALK_WINDOW.max(rec) / rec * rec;
    let end = r.len();
    while off < end {
        let n = (per as u64).min(end - off) as usize;
        let buf = r.read(cs, off, n).map_err(|e| crate::store_io::node_err(what, &key, e))?;
        if buf.len() != n {
            return Err(MetaError::Corrupted(format!("{what} {key:?}: short read at {off}")));
        }
        for one in buf.chunks_exact(rec) {
            f(one)?;
        }
        off += n as u64;
    }
    Ok(())
}

/// Call `f` with the key of each entry of the u64-digest map node `key`, in
/// the stored order. The checks of [`load_u64_digest_map`], but the node
/// is read in windows.
fn for_each_map_value(
    cs: &ChunkStore,
    key: &ObjectKey,
    f: &mut dyn FnMut(ObjectKey) -> Result<(), MetaError>,
) -> Result<(), MetaError> {
    const WHAT: &str = "u64-digest map node";
    let mut r = chunk_store::file::FileReader::open(cs, crate::store_io::to_id(key))
        .map_err(|e| crate::store_io::node_err(WHAT, key, e))?;
    let len = r.len();
    let head = if len >= 8 { r.read(cs, 0, 8).map_err(|e| crate::store_io::node_err(WHAT, key, e))? } else { Vec::new() };
    if head.len() < 8 {
        return Err(MetaError::Corrupted(format!("u64-digest map: expected at least 8 bytes, got {len}")));
    }
    let (count, expected) = map_size(&head);
    if expected.map(|e| e as u64) != Some(len) {
        return Err(MetaError::Corrupted(format!("u64-digest map: {len} bytes cannot hold {count} entries")));
    }
    for_each_window(cs, &mut r, WHAT, 8, MAP_ENTRY, &mut |e| f(parse_key(&e[8..MAP_ENTRY])))
}

/// Call `f` with each key of the manifest node `key` (the checks of
/// [`load_manifest`], but the node is read in windows).
fn for_each_manifest_key(
    cs: &ChunkStore,
    key: &ObjectKey,
    f: &mut dyn FnMut(ObjectKey) -> Result<(), MetaError>,
) -> Result<(), MetaError> {
    const WHAT: &str = "manifest node";
    let mut r = chunk_store::file::FileReader::open(cs, crate::store_io::to_id(key))
        .map_err(|e| crate::store_io::node_err(WHAT, key, e))?;
    let len = r.len();
    if !len.is_multiple_of(KEY_BYTES as u64) {
        return Err(MetaError::Corrupted(format!(
            "manifest: expected a multiple of {KEY_BYTES} bytes, got {len}"
        )));
    }
    for_each_window(cs, &mut r, WHAT, 0, KEY_BYTES, &mut |e| f(ObjectKey::read(e, 0)))
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
    /// Restores the in-memory maps (inode_data, dir_data) and the
    /// working roots of the manifest, refcount and xattr trees
    /// and inode numbering state so that subsequent operations continue seamlessly.
    pub fn load_from_root(io: Arc<Mutex<StoreIo>>, root: &ObjectKey) -> Result<Self, MetaError> {
        let mut io_guard = lock_stats::lock(&io, Site::Io);

        // Read root record bytes
        let bytes = (*io_guard).get_node(root)
            .map_err(|e| crate::store_io::node_err("root node", root, e))?;

        check_root_len(bytes.len())?;
        let digest_at = |o: usize| parse_key(&bytes[o..o + KEY_BYTES]);
        let u64_at = |o: usize| u64::from_le_bytes(bytes[o..o + 8].try_into().unwrap());

        let root_dir_ino = u64_at(root_rec::ROOT_DIR_INO);
        let next_ino = u64_at(root_rec::NEXT_INO);
        let inode_data_digest = digest_at(root_rec::INODE_DATA);
        let dir_data_digest = digest_at(root_rec::DIR_DATA);
        let manifest_data_digest = digest_at(root_rec::MANIFEST_DATA);
        let xattr_data_digest = digest_at(root_rec::XATTR_DATA);
        let refcount_data_digest = digest_at(root_rec::REFCOUNTS);
        let initial_logical_bytes = u64_at(root_rec::LOGICAL_BYTES);
        let initial_inode_count = u64_at(root_rec::INODE_COUNT);

        // Restore next_ino exactly.
        let mut inode_alloc = InodeAlloc::new();
        inode_alloc.set_next_ino(next_ino);

        let inode_data = load_u64_digest_map(&mut *io_guard, &inode_data_digest)?;
        let dir_data = load_u64_digest_map(&mut *io_guard, &dir_data_digest)?;

        // Sanity: root directory inode must exist
        if !inode_data.contains_key(&root_dir_ino) {
            return Err(MetaError::Corrupted(format!(
                "root dir ino {} not found in inode_data after reload",
                root_dir_ino
            )));
        }

        let cs = Arc::clone(io_guard.store());
        let trees = TreeIo::new(Arc::clone(&cs), io_guard.node_cache(), io_guard.tree_config());
        drop(io_guard);

        Ok(DictMetadataStore {
            io,
            cs,
            unsynced_puts: AtomicU64::new(0),
            tree_flush: AtomicUsize::new(TREE_FLUSH),
            put_error: lk(None),
            inode_alloc: lk(inode_alloc).timed(Site::InodeAlloc),
            inode_data: lk(inode_data).timed(Site::InodeData),
            dir_data: lk(dir_data).timed(Site::DirData),
            dir_live: Sharded::new(Site::DirLive),
            inode_live: Sharded::new(Site::InodeLive),
            changes: AtomicU64::new(0),
            committed_changes: AtomicU64::new(0),
            // The working root; no manifest is read here.
            manifest: MapTree::new("manifest_data", Site::ManifestData, crate::store_io::to_id(&manifest_data_digest)),
            trees,
            apply_lock: lk(()),
            prepared: AtomicBool::new(false),
            // The working root; no xattr is read here.
            xattrs: MapTree::new("xattr_data", Site::XattrData, crate::store_io::to_id(&xattr_data_digest)),
            pre_root_sync: lk(None),
            refcounts: MapTree::new("refcounts", Site::Refcounts, crate::store_io::to_id(&refcount_data_digest)),
            logical_bytes: AtomicU64::new(initial_logical_bytes),
            inode_count: AtomicU64::new(initial_inode_count),
            wal: lk(None).timed(Site::Wal),
            wal_fail_seq: AtomicU64::new(0),
            wal_fail_cleared: AtomicU64::new(0),
            commit_lock: sync::Mutex::new(Rank::Rename, ()),
            commit_fault: std::sync::atomic::AtomicU8::new(0),
            last_root: lk(Some(*root)),
            last_stats: lk(CommitStats::default()),
            snapshots_by_version: lk(HashMap::new()),
            snapshots_by_name: lk(HashMap::new()),
        })
    }
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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
    fn a_map_with_an_overflowing_count_is_corrupt() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut io = StoreIo::open(dir.path()).unwrap();
        for n in [u64::MAX, 1u64 << 63, u64::MAX / MAP_ENTRY as u64 + 1, 1] {
            let key = io.put_node(&n.to_le_bytes());
            let a = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| load_u64_digest_map(&mut io, &key)));
            assert!(matches!(a, Ok(Err(MetaError::Corrupted(_)))), "u64-key map, count {n}");
        }
    }
}

#[cfg(test)]
mod tests {

    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
    fn new_store_root_dir_has_current_times() {
        let dir = tempfile::TempDir::new().unwrap();
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        let store = DictMetadataStore::new(Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap())));
        let root = store.get_inode(1).unwrap();
        assert!(root.mtime_sec >= before, "root mtime {} (epoch 0 shows as 1969)", root.mtime_sec);
        assert!(root.ctime_sec >= before, "root ctime {}", root.ctime_sec);
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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

    /// The stored form of the xattr `name` of `ino` (overlay, then tree).
    fn xattr_form(store: &DictMetadataStore, ino: u64, name: &str) -> Option<XattrValue> {
        store.xattrs.read(&store.trees, &XattrKey::new(ino, name)).unwrap()
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
    fn a_value_that_fits_is_inline_and_a_larger_one_spills() {
        let (_d, store) = make_store();
        let ino = store.create_inode(&new_file_meta()).unwrap();
        let mut want = Vec::new();
        for n in [1usize, 255] {
            let fits = prolly::MAX_ENTRY - 13 - n;
            for (name, len) in [("a".repeat(n), fits), ("b".repeat(n), fits + 1)] {
                let v: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
                store.set_xattr(ino, &name, &v).unwrap();
                want.push((name, v));
            }
        }
        let check = |store: &DictMetadataStore| {
            for (name, v) in &want {
                let inline = name.starts_with('a');
                match xattr_form(store, ino, name).unwrap() {
                    XattrValue::Inline(b) => assert!(inline && b == *v, "{} bytes inline", v.len()),
                    XattrValue::Spilled(_) => assert!(!inline, "{} bytes spilled", v.len()),
                }
                assert_eq!(store.get_xattr(ino, name).unwrap(), *v);
                assert_eq!(store.xattr_len(ino, name).unwrap(), v.len() as u64);
            }
        };
        check(&store);
        let root = store.commit().unwrap();
        let reloaded = DictMetadataStore::load_from_root(Arc::clone(store.io()), &root).unwrap();
        check(&reloaded);
        // A short value over a spilled one is inline.
        let big = "b".to_string();
        reloaded.set_xattr(ino, &big, b"short").unwrap();
        assert_eq!(xattr_form(&reloaded, ino, &big), Some(XattrValue::Inline(b"short".to_vec())));
        assert_eq!(reloaded.get_xattr(ino, &big).unwrap(), b"short");
        // Names the checks refuse.
        assert!(matches!(store.set_xattr(ino, "", b"v"), Err(MetaError::InvalidName(_))));
        assert!(matches!(store.set_xattr(ino, &"n".repeat(256), b"v"), Err(MetaError::NameTooLong(256))));
        assert!(store.get_xattr(ino, &"n".repeat(2000)).is_err(), "no key above MAX_KEY");
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
    fn an_offset_read_reads_the_range() {
        let (_d, store) = make_store();
        let ino = store.create_inode(&new_file_meta()).unwrap();
        let big: Vec<u8> = (0..100u32 << 10).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        let small: Vec<u8> = (0..50u8).collect();
        store.set_xattr(ino, "user.big", &big).unwrap();
        store.set_xattr(ino, "user.small", &small).unwrap();
        assert!(matches!(xattr_form(&store, ino, "user.big"), Some(XattrValue::Spilled(_))));
        for (name, v) in [("user.big", &big), ("user.small", &small)] {
            let n = v.len();
            assert_eq!(store.xattr_len(ino, name).unwrap(), n as u64);
            assert_eq!(store.read_xattr(ino, name, 0, 16).unwrap(), &v[..16]);
            assert_eq!(store.read_xattr(ino, name, (n / 2) as u64, 20).unwrap(), &v[n / 2..n / 2 + 20]);
            assert_eq!(store.read_xattr(ino, name, (n - 5) as u64, 100).unwrap(), &v[n - 5..], "short");
            assert!(store.read_xattr(ino, name, n as u64 + 1, 10).unwrap().is_empty(), "past the end");
        }
        assert!(matches!(store.xattr_len(ino, "user.none"), Err(MetaError::NotFound(i)) if i == ino));
        assert!(matches!(store.read_xattr(ino, "user.none", 0, 1), Err(MetaError::NotFound(_))));
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
    fn delete_inode_drops_pending_and_tree_xattrs() {
        let (_d, store) = make_store();
        let before = store.create_inode(&new_file_meta()).unwrap();
        let ino = store.create_inode(&new_file_meta()).unwrap();
        let after = store.create_inode(&new_file_meta()).unwrap();
        for n in [before, after] {
            for i in 0..3 {
                store.set_xattr(n, &format!("user.n{i}"), b"keep").unwrap();
            }
        }
        // 30 names: past TREE_FLUSH (8), so some are in the tree.
        for i in 0..30 {
            store.set_xattr(ino, &format!("user.x{i:02}"), &[i as u8; 10]).unwrap();
        }
        assert!(store.xattrs.dirty() < 30, "flushes ran");
        assert_eq!(store.list_xattrs(ino).unwrap().len(), 30);
        store.delete_inode(ino).unwrap();
        let names = |s: &DictMetadataStore, n: u64| s.list_xattrs(n).unwrap();
        assert!(names(&store, ino).is_empty());
        let three: Vec<String> = (0..3).map(|i| format!("user.n{i}")).collect();
        assert_eq!(names(&store, before), three);
        assert_eq!(names(&store, after), three);
        let root = store.commit().unwrap();
        let reloaded = DictMetadataStore::load_from_root(Arc::clone(store.io()), &root).unwrap();
        assert!(names(&reloaded, ino).is_empty());
        assert_eq!(names(&reloaded, before), three);
        assert_eq!(names(&reloaded, after), three);
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
    fn test_new_has_root_dir() {
        let (_dir, store) = make_store();
        assert_eq!(store.root_ino(), 1);
        let entries = store.list_directory(1).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"."), ". not found in root: {:?}", names);
        assert!(names.contains(&".."), ".. not found in root: {:?}", names);
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
    fn test_manifest_round_trip() {
        let (_dir, store) = make_store();
        let file_ino = store.create_inode(&new_file_meta()).unwrap();

        let blocks: Vec<ObjectKey> = (0..5).map(|i| ObjectKey::example(i as u32)).collect();
        store.set_manifest(file_ino, &blocks).unwrap();
        let recovered = store.get_manifest(file_ino).unwrap();
        assert_eq!(recovered, blocks);
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
    fn test_root_ino_is_one() {
        let (_dir, store) = make_store();
        assert_eq!(store.root_ino(), 1);
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
    fn test_commit_returns_nonzero_digest() {
        let (_dir, store) = make_store();
        let digest = store.commit().unwrap();
        assert_ne!(digest, ObjectKey::ZERO, "commit should return non-zero digest");
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
    fn test_delete_nonexistent_returns_not_found() {
        let (_dir, store) = make_store();
        let result = store.delete_inode(9999);
        assert!(matches!(result, Err(MetaError::NotFound(9999))));
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
    fn test_create_dir_under_nonexistent_parent_returns_not_found() {
        let (_dir, store) = make_store();
        let result = store.create_directory(9999, "sub", &new_dir_meta());
        // Should be NotFound since ino 9999 does not exist
        assert!(result.is_err());
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
    fn test_xattr_set_get() {
        let (_dir, store) = make_store();
        let file_ino = store.create_inode(&new_file_meta()).unwrap();
        store.set_xattr(file_ino, "user.test", b"myvalue").unwrap();
        let val = store.get_xattr(file_ino, "user.test").unwrap();
        assert_eq!(val, b"myvalue");
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
    fn test_xattr_remove() {
        let (_dir, store) = make_store();
        let file_ino = store.create_inode(&new_file_meta()).unwrap();
        store.set_xattr(file_ino, "user.k", b"v").unwrap();
        store.remove_xattr(file_ino, "user.k").unwrap();
        let result = store.get_xattr(file_ino, "user.k");
        assert!(result.is_err(), "get after remove should error");
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
    fn test_xattr_overwrite() {
        let (_dir, store) = make_store();
        let file_ino = store.create_inode(&new_file_meta()).unwrap();
        store.set_xattr(file_ino, "user.x", b"first").unwrap();
        store.set_xattr(file_ino, "user.x", b"second").unwrap();
        let val = store.get_xattr(file_ino, "user.x").unwrap();
        assert_eq!(val, b"second");
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
    fn test_xattr_on_directory() {
        let (_dir, store) = make_store();
        // Set xattr on the root directory inode (ino=1)
        store.set_xattr(1, "user.dir_attr", b"dir_val").unwrap();
        let val = store.get_xattr(1, "user.dir_attr").unwrap();
        assert_eq!(val, b"dir_val");
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
    fn test_xattr_list_empty() {
        let (_dir, store) = make_store();
        let file_ino = store.create_inode(&new_file_meta()).unwrap();
        let names = store.list_xattrs(file_ino).unwrap();
        assert!(names.is_empty());
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
    fn test_xattr_get_not_found() {
        let (_dir, store) = make_store();
        let file_ino = store.create_inode(&new_file_meta()).unwrap();
        let result = store.get_xattr(file_ino, "user.missing");
        assert!(matches!(result, Err(MetaError::NotFound(_))));
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
    fn test_xattr_remove_not_found() {
        let (_dir, store) = make_store();
        let file_ino = store.create_inode(&new_file_meta()).unwrap();
        let result = store.remove_xattr(file_ino, "user.nonexistent");
        assert!(result.is_err());
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
    fn test_update_nonexistent_returns_not_found() {
        let (_dir, store) = make_store();
        let mut meta = new_file_meta();
        meta.ino = 9999;
        let result = store.update_inode(&meta);
        assert!(matches!(result, Err(MetaError::NotFound(9999))));
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
    fn test_inode_numbers_are_monotonic() {
        let (_dir, store) = make_store();
        let ino1 = store.create_inode(&new_file_meta()).unwrap();
        let ino2 = store.create_inode(&new_file_meta()).unwrap();
        let ino3 = store.create_inode(&new_file_meta()).unwrap();
        assert!(ino1 < ino2);
        assert!(ino2 < ino3);
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
    fn test_link_duplicate_name_rejected() {
        let (_dir, store) = make_store();
        let ino = store.create_inode(&new_file_meta()).unwrap();
        store.link(1, "dup", ino).unwrap();
        let result = store.link(1, "dup", ino);
        assert!(matches!(result, Err(MetaError::AlreadyExists(_))));
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
    fn test_list_directory_non_dir_returns_error() {
        let (_dir, store) = make_store();
        // Inode 9999 doesn't exist as a directory
        let result = store.list_directory(9999);
        assert!(result.is_err());
    }

    // ── persistence round-trip tests (Task 2) ─────────────────────────────────

    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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

    /// The root record has one inode map and is 197 bytes; `next_ino` in it
    /// is the allocator value.
    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
    fn root_record_layout() {
        assert_eq!(root_rec::ROOT_DIR_INO, 0);
        assert_eq!(root_rec::NEXT_INO, 8);
        assert_eq!(root_rec::INODE_DATA, 16);
        assert_eq!(root_rec::DIR_DATA, 49);
        assert_eq!(root_rec::MANIFEST_DATA, 82);
        assert_eq!(root_rec::XATTR_DATA, 115);
        assert_eq!(root_rec::REFCOUNTS, 148);
        assert_eq!(root_rec::LOGICAL_BYTES, 181);
        assert_eq!(root_rec::INODE_COUNT, 189);
        assert_eq!(root_rec::SIZE, 197);

        let (_dir, store) = make_store();
        let ino = store.create_inode(&new_file_meta()).unwrap();
        store.link(store.root_ino(), "f", ino).unwrap();
        let root = store.commit().unwrap();
        let mut io = NodeReader(Arc::clone(store.io().lock().unwrap().store()));
        let bytes = io.get_node(&root).unwrap();
        assert_eq!(bytes.len(), 197);
        let next = u64::from_le_bytes(bytes[root_rec::NEXT_INO..root_rec::NEXT_INO + 8].try_into().unwrap());
        assert_eq!(next, store.inode_alloc.lock().next_ino());
        assert!(next > ino);
    }

    /// A root record of the old size (two inode maps) is refused, not misread.
    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
    fn a_230_byte_root_record_is_refused() {
        let (_dir, store) = make_store();
        let old = crate::store_io::put_value(store.io().lock().unwrap().store(), &[0u8; 230]).unwrap();
        let r = DictMetadataStore::load_from_root(Arc::clone(store.io()), &old);
        assert!(matches!(&r, Err(MetaError::Corrupted(m)) if m.contains("expected 197 bytes, got 230")));
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
        let read_back = (*io_guard).get_node(&content_digest).expect("content missing");
        assert_eq!(read_back, content.as_slice());
    }

    // ── Snapshot method tests ──────────────────────────────────────────────

    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
    fn test_set_snapshots_populates_both_indexes() {
        let snaps = vec![
            make_snapshot(10, Some("ten")),
            make_snapshot(20, None),
            make_snapshot(30, Some("thirty")),
        ];
        let (_dir, mut store) = make_store();
        store.set_snapshots(snaps);

        assert_eq!(store.find_snapshot("10").unwrap().version, 10);
        assert!(store.find_snapshot("twenty").is_none());
        assert_eq!(store.find_snapshot("ten").unwrap().version, 10);
        assert_eq!(store.find_snapshot("thirty").unwrap().version, 30);
        assert!(store.find_snapshot("20").is_some());  // unnamed, find by version
        assert!(store.find_snapshot("20_name").is_none());
    }

    /// list_snapshots returns sorted by version (existing behavior preserved).
    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
    fn test_refcount_saturates() {
        let (_dir, store) = make_store();
        let digest: ObjectKey = ObjectKey::example(0xDE);

        // Manually set refcount to u64::MAX - 1
        store.refcounts.write(digest, Some(u64::MAX - 1));

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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
    fn test_refcount_decrement_saturated() {
        let (_dir, store) = make_store();
        let digest: ObjectKey = ObjectKey::example(0xAB);

        // Set refcount to u64::MAX directly
        store.refcounts.write(digest, Some(u64::MAX));

        // Decrement must be a no-op for saturated blocks
        store.decrement_refcount(&digest);
        assert_eq!(store.get_refcount(&digest), u64::MAX,
            "decrement on saturated (u64::MAX) refcount must be a no-op — block is immortal");
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
    fn test_saturated_refcount_count() {
        let (_dir, store) = make_store();
        let d1: ObjectKey = ObjectKey::example(0x01);
        let d2: ObjectKey = ObjectKey::example(0x02);
        let d3: ObjectKey = ObjectKey::example(0x03);
        let d4: ObjectKey = ObjectKey::example(0x04);
        let d5: ObjectKey = ObjectKey::example(0x05);

        store.refcounts.write(d1, Some(u64::MAX)); // saturated
        store.refcounts.write(d2, Some(u64::MAX)); // saturated
        store.refcounts.write(d3, Some(5));        // normal
        store.refcounts.write(d4, Some(2));        // normal
        store.refcounts.write(d5, Some(1));        // normal

        assert_eq!(store.saturated_refcount_count().unwrap(), 2,
            "should count exactly 2 saturated refcounts");
        // The same from the tree after a commit, and with pending edits on
        // top: d1 is hidden by a delete, d2 by a normal count, d3 saturates
        // in the overlay only.
        store.commit().unwrap();
        assert_eq!(store.refcounts.dirty(), 0);
        assert_eq!(store.saturated_refcount_count().unwrap(), 2, "from the tree");
        store.refcounts.write(d1, None);
        store.refcounts.write(d2, Some(7));
        store.refcounts.write(d3, Some(u64::MAX));
        assert_eq!(store.saturated_refcount_count().unwrap(), 1, "pending edits hide tree entries");
        let d6 = ObjectKey::example(0x06);
        store.refcounts.write(d6, Some(u64::MAX));
        assert_eq!(store.saturated_refcount_count().unwrap(), 2);
    }

    /// A refcount change reads the tree once and writes one edit; a
    /// decrement of an absent key, or of a saturated one, writes none; a
    /// count of 0 is deleted from the tree.
    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
    fn refcount_changes_follow_the_tree() {
        let (_dir, store) = make_store();
        store.test_set_tree_flush(usize::MAX);
        let (a, b) = (ObjectKey::example(0x10), ObjectKey::example(0x11));
        store.decrement_refcount(&a);
        assert_eq!(store.refcounts.dirty(), 0, "no edit for an absent key");
        store.increment_refcount(&a);
        store.increment_refcount(&a);
        store.increment_refcount(&b);
        store.commit().unwrap();
        assert_eq!(store.refcounts.dirty(), 0);
        let rec = store.io().lock().unwrap().get_node(&store.current_root().unwrap()).unwrap();
        let tree = crate::store_io::to_id(&parse_key(&rec[root_rec::REFCOUNTS..root_rec::REFCOUNTS + KEY_BYTES]));
        let cs = Arc::clone(&store.cs);
        assert_eq!(prolly::get(&cs, &tree, &a.0).unwrap(), Some(2u64.to_le_bytes().to_vec()), "8 bytes LE");
        // From the tree: one edit each.
        store.decrement_refcount(&a);
        store.decrement_refcount(&b);
        assert_eq!(store.refcounts.dirty(), 2);
        assert_eq!((store.get_refcount(&a), store.get_refcount(&b)), (1, 0));
        // A tombstone counts as 0: a decrement changes nothing.
        store.decrement_refcount(&b);
        assert_eq!(store.get_refcount(&b), 0);
        store.commit().unwrap();
        let rec = store.io().lock().unwrap().get_node(&store.current_root().unwrap()).unwrap();
        let tree = crate::store_io::to_id(&parse_key(&rec[root_rec::REFCOUNTS..root_rec::REFCOUNTS + KEY_BYTES]));
        assert_eq!(prolly::count(&prolly::Ctx::uncached(&cs), &tree).unwrap(), 1, "the count of 0 is deleted");
        // Saturated in the tree: a decrement writes nothing.
        store.refcounts.write(b, Some(u64::MAX));
        store.commit().unwrap();
        store.decrement_refcount(&b);
        store.increment_refcount(&b);
        assert_eq!(store.refcounts.dirty(), 0);
        assert_eq!(store.get_refcount(&b), u64::MAX);
    }

    // ── inode_count AtomicU64 tests (FIX-02 store layer) ─────────────────────

    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
    fn test_inode_count_empty() {
        let (_dir, store) = make_store();
        // Fresh store has root inode (ino=1) — count should be 1
        assert_eq!(store.inode_count(), 1,
            "fresh store should have inode_count == 1 (root inode)");
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
    fn missing_inode_modify_inode_is_not_found_and_creates_nothing() {
        let (_d, store) = make_store();
        assert!(matches!(store.modify_inode(99, |m| m.size = 5), Err(MetaError::NotFound(99))));
        assert!(matches!(store.get_inode(99), Err(MetaError::NotFound(99))));
        assert_eq!(store.inode_count(), 1);
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support getgid (DictMetadataStore::new)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
    fn flush_install_does_not_resurrect_a_deleted_inode() {
        let (_d, store) = make_store();
        let x = store.create_inode(&new_file_meta()).unwrap();
        store.flush_inodes_between(|| store.delete_inode(x).unwrap());
        assert!(matches!(store.get_inode(x), Err(MetaError::NotFound(_))));
        assert!(!store.inode_data.lock().contains_key(&x));
        assert!(!store.inode_live.lock(x).map.contains_key(&x));
        let root = store.commit().unwrap();
        let reloaded = DictMetadataStore::load_from_root(Arc::clone(store.io()), &root).unwrap();
        assert!(matches!(reloaded.get_inode(x), Err(MetaError::NotFound(_))));
    }

    // A commit took X (size 1) at its cut, then X changed (size 2) and a
    // flush took it. Before the flush installs, the commit installs its
    // older digest. The older digest must not end up as the current one.
    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support getgid (DictMetadataStore::new)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support getgid (DictMetadataStore::new)")]
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
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
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
    /// The walk of the GC mark reads the map and manifest nodes in tiny
    /// windows (in tests) and gives the keys of the whole-node loads.
    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
    fn the_windowed_walk_gives_the_keys_of_the_whole_nodes() {
        let (_dir, store) = make_store();
        let r = store.root_ino();
        for i in 0..40u32 {
            let ino = store.create_inode(&new_file_meta()).unwrap();
            store.link(r, &format!("f{i}"), ino).unwrap();
            let parts: Vec<_> = (0..(i % 7)).map(|j| ObjectKey::example(1000 + i * 10 + j)).collect();
            store.set_manifest(ino, &parts).unwrap();
            if i % 3 == 0 {
                store.set_xattr(ino, "user.a", b"x").unwrap();
            }
            if i % 5 == 0 {
                // A spilled value: a file value of the xattr tree.
                store.set_xattr(ino, "user.big", &vec![i as u8; 3000]).unwrap();
            }
            for p in &parts {
                store.increment_refcount(p);
            }
        }
        let root = store.commit().unwrap();
        let mut io = NodeReader(Arc::clone(store.io().lock().unwrap().store()));
        // The keys of whole-node loads, as the mark had them before #207.
        let bytes = io.get_node(&root).unwrap();
        let at = |o: usize| parse_key(&bytes[o..o + KEY_BYTES]);
        let mut want = vec![root, at(root_rec::INODE_DATA), at(root_rec::DIR_DATA)];
        for o in [root_rec::INODE_DATA, root_rec::DIR_DATA] {
            want.extend(load_u64_digest_map(&mut io, &at(o)).unwrap().into_values());
        }
        // The manifests: the values of the manifest tree, in key order.
        let mut want_content = Vec::new();
        let tree = crate::store_io::to_id(&at(root_rec::MANIFEST_DATA));
        let cs = Arc::clone(&io.0);
        for e in prolly::iter(&cs, &tree).unwrap() {
            let m = parse_key(&e.unwrap().1);
            want.push(m);
            want_content.extend(load_manifest(&mut io, &m).unwrap());
        }
        // Then the spilled xattr values, in key order.
        let xattr_tree = crate::store_io::to_id(&at(root_rec::XATTR_DATA));
        let mut spilled = 0;
        for e in prolly::iter(&cs, &xattr_tree).unwrap() {
            if let XattrValue::Spilled(k) = <XattrValue as crate::pmap::TreeValue>::decode(&e.unwrap().1).unwrap() {
                want.push(k);
                spilled += 1;
            }
        }
        assert_eq!(spilled, 8, "one per fifth file");
        let (nodes, content) = referenced(&mut io, &root).unwrap();
        assert_eq!(nodes, want);
        assert_eq!(content, want_content);
        assert!(want.len() > 3 * (100 / MAP_ENTRY) && want_content.len() > 100 / KEY_BYTES, "many windows");
        // The tree nodes come apart from the file values (`Ref::Tree`).
        let mut trees = Vec::new();
        for_each_referenced(&mut io, &root, &mut |r, k| {
            if r == Ref::Tree {
                trees.push(k);
            }
            Ok(())
        }).unwrap();
        assert_eq!(trees, tree_nodes(&mut io, &root).unwrap());
        assert_eq!(trees[0], at(root_rec::MANIFEST_DATA), "the tree root first");
        // The refcount tree root is a tree node, not a file value.
        let refcounts = at(root_rec::REFCOUNTS);
        assert!(prolly::count(&prolly::Ctx::uncached(&cs), &crate::store_io::to_id(&refcounts)).unwrap() > 100);
        assert!(trees.contains(&refcounts), "the refcount tree is in the tree nodes");
        assert!(!nodes.contains(&refcounts), "and not a file value");
        // The xattr tree root too, after the refcount tree nodes.
        let xattrs = at(root_rec::XATTR_DATA);
        let xi = trees.iter().position(|k| *k == xattrs).expect("the xattr tree is in the tree nodes");
        assert!(trees.iter().position(|k| *k == refcounts).unwrap() < xi, "manifest, refcounts, then xattrs");
        assert!(!nodes.contains(&xattrs));
        // A bad xattr leaf entry is corrupt.
        assert!(xattr_leaf(&[1, 2, 3], &[0]).is_err(), "short key");
        assert!(xattr_leaf(&[0; 9], &[2]).is_err(), "bad tag");
        assert_eq!(xattr_leaf(&[0; 9], &[0, 1]).unwrap(), None);

        // A map node whose count does not fit its length is refused.
        let mut bad = 3u64.to_le_bytes().to_vec();
        bad.extend_from_slice(&[0u8; MAP_ENTRY * 2]);
        let bad = crate::store_io::put_value(io.chunk_store(), &bad).unwrap();
        let e = for_each_map_value(io.chunk_store(), &bad, &mut |_| Ok(())).unwrap_err();
        assert!(e.to_string().contains("cannot hold 3 entries"), "{e}");
        let short = crate::store_io::put_value(io.chunk_store(), &[1, 2, 3]).unwrap();
        assert!(for_each_map_value(io.chunk_store(), &short, &mut |_| Ok(())).is_err());
        assert!(for_each_manifest_key(io.chunk_store(), &short, &mut |_| Ok(())).is_err());
    }

    /// A manifest read answers from the overlay while the change is dirty,
    /// from the tree after a flush and after a commit, and a delete hides
    /// the manifest at each of these steps.
    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support getgid (DictMetadataStore::new)")]
    fn manifest_read_sees_the_overlay_then_the_tree() {
        let mem = chunk_store::mem::MemIo::new();
        let (_io, store) = mem_store(&mem);
        let files: Vec<u64> = (0..TREE_FLUSH as u32 + 3).map(|_| store.create_inode(&new_file_meta()).unwrap()).collect();
        let empty = store.manifest.root();
        // Dirty: below the flush limit, the tree is still empty.
        for (i, f) in files.iter().take(TREE_FLUSH - 1).enumerate() {
            store.set_manifest(*f, &[ObjectKey::example(i as u32)]).unwrap();
        }
        assert_eq!(store.manifest.root(), empty);
        assert_eq!(store.get_manifest(files[0]).unwrap(), vec![ObjectKey::example(0)]);
        // The next write reaches the limit: a flush moves the entries to the tree.
        for (i, f) in files.iter().enumerate().skip(TREE_FLUSH - 1) {
            store.set_manifest(*f, &[ObjectKey::example(i as u32)]).unwrap();
        }
        assert_ne!(store.manifest.root(), empty);
        assert!(store.manifest.dirty() < TREE_FLUSH);
        assert_eq!(store.get_manifest(files[0]).unwrap(), vec![ObjectKey::example(0)], "from the tree");
        let root = store.commit().unwrap();
        assert_eq!(store.manifest.dirty(), 0);
        for (i, f) in files.iter().enumerate() {
            assert_eq!(store.get_manifest(*f).unwrap(), vec![ObjectKey::example(i as u32)]);
            assert_eq!(store.manifest_digest(*f), store.manifest.read(&store.trees, f).unwrap());
        }
        // A delete: hidden while dirty, and after the commit.
        store.delete_inode(files[1]).unwrap();
        assert!(store.get_manifest(files[1]).is_err());
        assert_eq!(store.manifest_digest(files[1]), None);
        let root2 = store.commit().unwrap();
        assert!(store.get_manifest(files[1]).is_err());
        let old = DictMetadataStore::load_from_root(Arc::clone(store.io()), &root).unwrap();
        assert_eq!(old.get_manifest(files[1]).unwrap(), vec![ObjectKey::example(1)], "the old root keeps it");
        let new = DictMetadataStore::load_from_root(Arc::clone(store.io()), &root2).unwrap();
        assert!(new.get_manifest(files[1]).is_err());
        assert_eq!(new.get_manifest(files[2]).unwrap(), vec![ObjectKey::example(2)]);
    }

    /// While a commit is prepared (between `begin_commit` and the install),
    /// writes past the flush limit do not move the working root; the
    /// commit holds the state at its cut and the later writes go to the
    /// next commit.
    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support getgid (DictMetadataStore::new)")]
    fn a_flush_does_not_run_while_a_commit_is_prepared() {
        let mem = chunk_store::mem::MemIo::new();
        let (_io, store) = mem_store(&mem);
        let files: Vec<u64> = (0..3 * TREE_FLUSH).map(|_| store.create_inode(&new_file_meta()).unwrap()).collect();
        store.set_manifest(files[0], &[ObjectKey::example(1)]).unwrap();
        let p = store.begin_commit().unwrap();
        let at_cut = store.manifest.root();
        for f in &files[1..] {
            store.set_manifest(*f, &[ObjectKey::example(2)]).unwrap();
        }
        assert_eq!(store.manifest.root(), at_cut, "no flush while prepared");
        assert_eq!(store.manifest.dirty(), files.len() - 1);
        let root = store.finish_commit(p).unwrap();
        let cut = DictMetadataStore::load_from_root(Arc::clone(store.io()), &root).unwrap();
        assert_eq!(cut.get_manifest(files[0]).unwrap(), vec![ObjectKey::example(1)]);
        assert!(cut.get_manifest(files[1]).is_err(), "a write after the cut is not in the root");
        // After the install, flushes run again.
        store.set_manifest(files[1], &[ObjectKey::example(3)]).unwrap();
        assert!(store.manifest.dirty() < TREE_FLUSH);
        assert_eq!(store.get_manifest(files[1]).unwrap(), vec![ObjectKey::example(3)]);
        // A dropped (unfinished) commit requeues and ends the prepared state.
        store.set_manifest(files[2], &[ObjectKey::example(4)]).unwrap();
        drop(store.begin_commit().unwrap());
        let root = store.commit().unwrap();
        let again = DictMetadataStore::load_from_root(Arc::clone(store.io()), &root).unwrap();
        assert_eq!(again.get_manifest(files[2]).unwrap(), vec![ObjectKey::example(4)]);
        assert_eq!(again.get_manifest(files[1]).unwrap(), vec![ObjectKey::example(3)]);
    }

    /// The manifest key of file `ino` at version `v` (the tests set these).
    fn mkey(ino: u64, v: u32) -> ObjectKey {
        ObjectKey::example(ino as u32 * 1000 + v)
    }

    /// `n` files linked under the root as `f<ino>`, each with its manifest at version 0.
    fn files_with_manifests(store: &DictMetadataStore, n: usize) -> Vec<u64> {
        (0..n)
            .map(|_| {
                let ino = store.create_inode(&new_file_meta()).unwrap();
                store.link(1, &format!("f{ino}"), ino).unwrap();
                store.set_manifest(ino, &[mkey(ino, 0)]).unwrap();
                ino
            })
            .collect()
    }

    /// A store of the root `root` in the same chunk store as `store`.
    fn reload(store: &DictMetadataStore, root: &ObjectKey) -> DictMetadataStore {
        DictMetadataStore::load_from_root(Arc::clone(store.io()), root).unwrap()
    }

    // A commit that fails before its roots are installed (at AfterApply and
    // at BeforeSync) leaves every change dirty again: the manifest tree
    // overlay, the inodes and the directories. The next commit writes them.
    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support getgid (DictMetadataStore::new)")]
    fn a_failed_commit_before_install_requeues_every_overlay() {
        for fault in [CommitFault::AfterApply, CommitFault::BeforeSync] {
            let mem = chunk_store::mem::MemIo::new();
            let (_io, store) = mem_store(&mem);
            let files = files_with_manifests(&store, 12);
            store.commit().unwrap();
            // Changes of every kind: a manifest, a delete, a create, an inode.
            for f in &files[..5] {
                store.set_manifest(*f, &[mkey(*f, 1)]).unwrap();
            }
            store.unlink(1, &format!("f{}", files[11])).unwrap();
            store.delete_inode(files[11]).unwrap();
            let new = store.create_inode(&new_file_meta()).unwrap();
            store.link(1, "new", new).unwrap();
            store.set_manifest(new, &[mkey(new, 1)]).unwrap();
            store.modify_inode(files[7], |m| m.size = 77).unwrap();
            let (rc_a, rc_b) = (mkey(1, 9), mkey(2, 9));
            store.increment_refcount(&rc_a);
            store.increment_refcount(&rc_a);
            store.increment_refcount(&rc_b);
            // An inline and a spilled xattr.
            store.set_xattr(files[3], "user.small", b"inline").unwrap();
            store.set_xattr(files[4], "user.big", &vec![9u8; 3000]).unwrap();

            store.test_fail_next_commit_at(fault);
            assert!(store.commit().is_err(), "{fault:?}");
            assert!(store.has_uncommitted_changes(), "{fault:?}");
            assert!(store.manifest.dirty() > 0, "{fault:?}: the manifest overlay is dirty again");
            assert!(store.refcounts.dirty() > 0, "{fault:?}: the refcount overlay is dirty again");
            assert!(store.xattrs.dirty() > 0, "{fault:?}: the xattr overlay is dirty again");
            assert!(store.inode_live.dirty() > 0 && store.dir_live.dirty() > 0, "{fault:?}");
            assert!(!store.prepared.load(Ordering::Relaxed), "{fault:?}: flushes may run again");
            // The state is still readable.
            assert_eq!(store.get_manifest(files[0]).unwrap(), vec![mkey(files[0], 1)]);

            let root = store.commit().unwrap();
            assert_eq!(store.manifest.dirty(), 0, "{fault:?}");
            let re = reload(&store, &root);
            for f in &files[..5] {
                assert_eq!(re.get_manifest(*f).unwrap(), vec![mkey(*f, 1)], "{fault:?}");
            }
            for f in &files[5..11] {
                assert_eq!(re.get_manifest(*f).unwrap(), vec![mkey(*f, 0)], "{fault:?}");
            }
            assert!(re.get_manifest(files[11]).is_err(), "{fault:?}: deleted");
            assert!(re.get_inode(files[11]).is_err(), "{fault:?}: deleted");
            assert!(re.lookup(1, &format!("f{}", files[11])).is_err(), "{fault:?}");
            assert_eq!(re.lookup(1, "new").unwrap(), new, "{fault:?}");
            assert_eq!(re.get_manifest(new).unwrap(), vec![mkey(new, 1)], "{fault:?}");
            assert_eq!(re.get_inode(files[7]).unwrap().size, 77, "{fault:?}");
            assert_eq!((re.get_refcount(&rc_a), re.get_refcount(&rc_b)), (2, 1), "{fault:?}");
            assert_eq!(re.get_xattr(files[3], "user.small").unwrap(), b"inline", "{fault:?}");
            assert_eq!(re.get_xattr(files[4], "user.big").unwrap(), vec![9u8; 3000], "{fault:?}");
        }
    }

    // A commit that fails after its roots are installed (AfterInstall, and
    // BeforePreRootSync after the sync) loses nothing: the edits are in the
    // working roots, and the next commit writes the root again.
    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support getgid (DictMetadataStore::new)")]
    fn a_failed_commit_after_install_keeps_the_state() {
        for fault in [CommitFault::AfterInstall, CommitFault::BeforePreRootSync, CommitFault::BeforeRoot] {
            let mem = chunk_store::mem::MemIo::new();
            let (_io, store) = mem_store(&mem);
            let files = files_with_manifests(&store, 12);
            store.commit().unwrap();
            for f in &files[..5] {
                store.set_manifest(*f, &[mkey(*f, 1)]).unwrap();
            }
            store.unlink(1, &format!("f{}", files[11])).unwrap();
            store.delete_inode(files[11]).unwrap();
            let new = store.create_inode(&new_file_meta()).unwrap();
            store.link(1, "new", new).unwrap();
            store.modify_inode(files[7], |m| m.size = 77).unwrap();
            let (rc_a, rc_b) = (mkey(1, 9), mkey(2, 9));
            store.increment_refcount(&rc_a);
            store.increment_refcount(&rc_a);
            store.increment_refcount(&rc_b);
            store.set_xattr(files[3], "user.small", b"inline").unwrap();
            store.set_xattr(files[4], "user.big", &vec![9u8; 3000]).unwrap();

            store.test_fail_next_commit_at(fault);
            assert!(store.commit().is_err(), "{fault:?}");
            assert!(store.has_uncommitted_changes(), "{fault:?}: no root was logged");
            assert!(!store.prepared.load(Ordering::Relaxed), "{fault:?}");
            assert_eq!(store.manifest.dirty(), 0, "{fault:?}: the edits are in the tree");
            assert_eq!(store.refcounts.dirty(), 0, "{fault:?}: the counts are in the tree");
            assert_eq!(store.xattrs.dirty(), 0, "{fault:?}: the xattrs are in the tree");
            assert_eq!(store.get_manifest(files[0]).unwrap(), vec![mkey(files[0], 1)], "{fault:?}");
            assert!(store.get_manifest(files[11]).is_err(), "{fault:?}");

            let root = store.commit().unwrap();
            let re = reload(&store, &root);
            for f in &files[..5] {
                assert_eq!(re.get_manifest(*f).unwrap(), vec![mkey(*f, 1)], "{fault:?}");
            }
            for f in &files[5..11] {
                assert_eq!(re.get_manifest(*f).unwrap(), vec![mkey(*f, 0)], "{fault:?}");
            }
            assert!(re.get_manifest(files[11]).is_err(), "{fault:?}");
            assert_eq!(re.lookup(1, "new").unwrap(), new, "{fault:?}");
            assert_eq!(re.get_inode(files[7]).unwrap().size, 77, "{fault:?}");
            assert_eq!((re.get_refcount(&rc_a), re.get_refcount(&rc_b)), (2, 1), "{fault:?}");
            assert_eq!(re.get_xattr(files[3], "user.small").unwrap(), b"inline", "{fault:?}");
            assert_eq!(re.get_xattr(files[4], "user.big").unwrap(), vec![9u8; 3000], "{fault:?}");
        }
    }

    // PMAP-03: writers set manifests (flushes run at TREE_FLUSH = 8) while a
    // commit thread commits in a loop. Each writer reads back what it just
    // wrote (never an older value, also in the window of a flush or commit
    // install); at the end every inode has its last version, before and
    // after a final commit and a reload.
    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support getgid (DictMetadataStore::new)")]
    fn a_manifest_set_racing_flushes_and_commits_loses_nothing() {
        const THREADS: usize = 4;
        const PER: usize = 64;
        const VERSIONS: u32 = 40;
        let mem = chunk_store::mem::MemIo::new();
        let (_io, store) = mem_store(&mem);
        let inos: Vec<u64> = (0..THREADS * PER).map(|_| store.create_inode(&new_file_meta()).unwrap()).collect();
        let writers_left = std::sync::atomic::AtomicUsize::new(THREADS);
        let commits = AtomicU64::new(0);
        std::thread::scope(|s| {
            let (store, writers_left, commits) = (&store, &writers_left, &commits);
            for mine in inos.chunks(PER) {
                s.spawn(move || {
                    // Counts the thread out also on a panic, so the commit thread ends.
                    struct Done<'a>(&'a std::sync::atomic::AtomicUsize);
                    impl Drop for Done<'_> {
                        fn drop(&mut self) {
                            self.0.fetch_sub(1, Ordering::SeqCst);
                        }
                    }
                    let _done = Done(writers_left);
                    for v in 1..=VERSIONS {
                        for ino in mine {
                            store.set_manifest(*ino, &[mkey(*ino, v)]).unwrap();
                            let got = store.get_manifest(*ino).unwrap();
                            assert_eq!(got, vec![mkey(*ino, v)], "inode {ino} read back an older version than {v}");
                        }
                    }
                });
            }
            s.spawn(move || {
                while writers_left.load(Ordering::SeqCst) > 0 {
                    store.commit().unwrap();
                    commits.fetch_add(1, Ordering::Relaxed);
                }
            });
        });
        assert!(commits.load(Ordering::Relaxed) > 0, "commits ran during the writes");
        let check = |s: &DictMetadataStore, what: &str| {
            for ino in &inos {
                assert_eq!(s.get_manifest(*ino).unwrap(), vec![mkey(*ino, VERSIONS)], "{what}: inode {ino}");
            }
        };
        check(&store, "before the final commit");
        let root = store.commit().unwrap();
        check(&store, "after the final commit");
        check(&reload(&store, &root), "after a reload");
    }

    // RESEARCH Pitfall 1: a read in the window of a flush (the new root is
    // installed, the overlay is not settled yet) sees the flushed value.
    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support getgid (DictMetadataStore::new)")]
    fn a_read_in_the_flush_window_sees_the_new_value() {
        let mem = chunk_store::mem::MemIo::new();
        let (_io, store) = mem_store(&mem);
        let store = Arc::new(store);
        let files: Vec<u64> = (0..TREE_FLUSH).map(|_| store.create_inode(&new_file_meta()).unwrap()).collect();
        let empty = store.manifest.root();
        type Seen = Vec<(bool, Vec<Option<ObjectKey>>)>;
        let seen: Arc<Mutex<Seen>> = Arc::default();
        {
            let (weak, seen, files) = (Arc::downgrade(&store), Arc::clone(&seen), files.clone());
            store.test_set_flush_hook(move || {
                let st = weak.upgrade().unwrap();
                let moved = st.manifest.root() != empty;
                let values = files.iter().map(|f| st.manifest.read(&st.trees, f).unwrap()).collect();
                seen.lock().unwrap().push((moved, values));
            });
        }
        // The last write reaches TREE_FLUSH and flushes.
        for f in &files {
            store.set_manifest(*f, &[mkey(*f, 1)]).unwrap();
        }
        store.test_clear_flush_hook();
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "one flush ran");
        let (moved, values) = &seen[0];
        assert!(*moved, "the hook ran after the install of the new root");
        // Every key reads the flushed value in the window, not an older one
        // and not a miss.
        for (f, v) in files.iter().zip(values) {
            assert_eq!(*v, Some(store.prepare_manifest(&[mkey(*f, 1)]).unwrap()), "key {f}");
        }
        assert_eq!(store.manifest.dirty(), 0);
    }

    // RESEARCH Pitfall 2: a change after the cut is not in the root of that
    // commit, also when writes pass the flush limit before the root is
    // installed, and it is in the next root.
    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support getgid (DictMetadataStore::new)")]
    fn a_post_cut_change_is_not_in_the_committed_root() {
        let mem = chunk_store::mem::MemIo::new();
        let (_io, store) = mem_store(&mem);
        let before = files_with_manifests(&store, 10);
        let after: Vec<u64> = (0..20).map(|_| store.create_inode(&new_file_meta()).unwrap()).collect();
        let p = store.begin_commit().unwrap();
        for f in &after {
            store.set_manifest(*f, &[mkey(*f, 1)]).unwrap();
        }
        store.set_manifest(before[0], &[mkey(before[0], 9)]).unwrap();
        assert!(after.len() > TREE_FLUSH);
        let root = store.finish_commit(p).unwrap();
        let cut = reload(&store, &root);
        for f in &after {
            assert!(cut.get_manifest(*f).is_err(), "inode {f}: a write after the cut is not in the root");
        }
        assert_eq!(cut.get_manifest(before[0]).unwrap(), vec![mkey(before[0], 0)]);
        for f in &after {
            assert_eq!(store.get_manifest(*f).unwrap(), vec![mkey(*f, 1)], "the live store has it");
        }
        let next = store.commit().unwrap();
        let re = reload(&store, &next);
        for f in &after {
            assert_eq!(re.get_manifest(*f).unwrap(), vec![mkey(*f, 1)], "the next root has it");
        }
        assert_eq!(re.get_manifest(before[0]).unwrap(), vec![mkey(before[0], 9)]);
    }

    // The same, with the writes made while the commit runs on another
    // thread (anywhere between its cut and its end): the root never has a
    // write that came after the cut. `cut_done` is sent after the cut.
    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support getgid (DictMetadataStore::new)")]
    fn writes_during_the_finish_are_not_in_the_committed_root() {
        let mem = chunk_store::mem::MemIo::new();
        let (_io, store) = mem_store(&mem);
        let base = files_with_manifests(&store, 10);
        for round in 1..=10u32 {
            let late: Vec<u64> = (0..3 * TREE_FLUSH).map(|_| store.create_inode(&new_file_meta()).unwrap()).collect();
            let (cut_done, cut_rx) = std::sync::mpsc::channel();
            let root = std::thread::scope(|s| {
                let store = &store;
                let committer = s.spawn(move || {
                    let p = store.begin_commit().unwrap();
                    cut_done.send(()).unwrap();
                    store.finish_commit(p).unwrap()
                });
                cut_rx.recv_timeout(LIMIT).unwrap();
                for f in &late {
                    store.set_manifest(*f, &[mkey(*f, round)]).unwrap();
                }
                committer.join().unwrap()
            });
            let cut = reload(&store, &root);
            for f in &late {
                assert!(cut.get_manifest(*f).is_err(), "round {round}: inode {f} is not in the root of the cut");
            }
            for f in &base {
                assert_eq!(cut.get_manifest(*f).unwrap(), vec![mkey(*f, 0)]);
            }
            let next = store.commit().unwrap();
            let re = reload(&store, &next);
            for f in &late {
                assert_eq!(re.get_manifest(*f).unwrap(), vec![mkey(*f, round)], "round {round}");
            }
        }
    }

    // A flush that fails requeues its entries (they stay readable) and keeps
    // the error: the next commit fails with it, and the commit after that
    // writes every change.
    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support getgid (DictMetadataStore::new)")]
    fn a_failed_flush_requeues_and_fails_the_next_commit() {
        let mem = chunk_store::mem::MemIo::new();
        let (_io, store) = mem_store(&mem);
        let files: Vec<u64> = (0..2 * TREE_FLUSH).map(|_| store.create_inode(&new_file_meta()).unwrap()).collect();
        let empty = store.manifest.root();
        store.test_fail_next_flush();
        // The write that reaches TREE_FLUSH flushes, and the flush fails.
        for f in &files[..TREE_FLUSH] {
            store.set_manifest(*f, &[mkey(*f, 1)]).unwrap();
        }
        assert_eq!(store.manifest.root(), empty, "the failed flush installed nothing");
        assert_eq!(store.manifest.dirty(), TREE_FLUSH, "the entries are dirty again");
        for f in &files[..TREE_FLUSH] {
            assert_eq!(store.get_manifest(*f).unwrap(), vec![mkey(*f, 1)], "still readable");
        }
        // The next write flushes again (the fault is used up).
        for f in &files[TREE_FLUSH..] {
            store.set_manifest(*f, &[mkey(*f, 1)]).unwrap();
        }
        assert_ne!(store.manifest.root(), empty);
        let e = store.commit().unwrap_err();
        assert!(e.to_string().contains("injected flush fault"), "{e}");
        let root = store.commit().unwrap();
        let re = reload(&store, &root);
        for f in &files {
            assert_eq!(re.get_manifest(*f).unwrap(), vec![mkey(*f, 1)]);
        }
    }
}
