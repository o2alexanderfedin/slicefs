//! `DictMetadataStore` — MetadataStore backed by file-based FileStorage (blockset Io).
//!
//! All state is held in-memory as `Mutex`-guarded maps; `commit()` serializes
//! everything via `FileStorageAdd` (flushed to disk) then writes a `RootUpdate`
//! to the WAL for crash-safe ordering.
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
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};

use blockset::{FileStorageAdd, State, Tree, file_storage_get};
use slicefs_traits::digest::Digest224;
use slicefs_traits::metadata::{DirEntry, InodeId, InodeMeta, MetaError, MetadataStore};

use crate::snapshot::SnapshotEntry;
use crate::store_io::StoreIo;
use crate::wal::{WalEntry, WalError, WalStrategy};

use crate::directory::{
    add_dir_entry, create_dir_entries, list_dir_entries, lookup_dir_entry, remove_dir_entry,
};
use crate::inode::{intern_inode, load_inode};
use crate::inode_map::{InodeMap, intern_inode_map, load_inode_map};
use crate::manifest::{intern_manifest, load_manifest};
use crate::xattr::{
    get_xattr_entry, intern_xattrs, list_xattr_names, load_xattrs, remove_xattr_entry,
    set_xattr_entry,
};

// S_IFDIR bit mask (POSIX directory type)
const S_IFDIR: u32 = 0o040_000;

/// Concrete `MetadataStore` backed by file-based FileStorage (blockset Io).
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
pub struct DictMetadataStore {
    /// File-backed Io implementation — used for all FileStorageAdd/file_storage_get calls.
    io: Arc<Mutex<StoreIo>>,
    /// Inode-number allocator + mapping table.
    inode_map: Mutex<InodeMap>,
    /// Maps inode number → current inode data `Digest224`.
    inode_data: Mutex<BTreeMap<u64, Digest224>>,
    /// Maps directory inode number → current entry list `Digest224`.
    dir_data: Mutex<BTreeMap<u64, Digest224>>,
    /// Maps file inode number → current manifest `Digest224`.
    manifest_data: Mutex<BTreeMap<u64, Digest224>>,
    /// Maps inode number → xattr set `Digest224` (only for inodes with xattrs).
    xattr_data: Mutex<BTreeMap<u64, Digest224>>,
    /// Reference counts for content `Digest224`s.
    /// Incremented when a manifest references a block; decremented on manifest replacement or inode deletion.
    refcounts: Mutex<BTreeMap<Digest224, u64>>,
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
    wal: Mutex<Option<Box<dyn WalStrategy>>>,
    /// Last committed root digest — updated by `commit()`, used by GC to determine live roots.
    last_root: Mutex<Option<Digest224>>,
    /// Snapshot index by version number — O(1) lookup (FIX-03).
    snapshots_by_version: Mutex<HashMap<u64, SnapshotEntry>>,
    /// Snapshot name → version mapping — O(1) name lookup (FIX-04).
    snapshots_by_name: Mutex<HashMap<String, u64>>,
}

impl DictMetadataStore {
    /// Create a new store with a root directory at inode 1.
    pub fn new(io: Arc<Mutex<StoreIo>>) -> Self {
        let mut inode_map = InodeMap::new();

        // Build root inode + dir entries via FileStorageAdd, then flush
        let (root_inode_digest, root_dir_digest) = {
            let mut io_guard = io.lock().unwrap();
            let mut fsa = FileStorageAdd::new(&mut *io_guard);
            // Use current process uid/gid so the mounting user can write to root.
            // On non-unix, falls back to 0/0 (callers like `seed` update root inode
            // from the source directory metadata afterwards).
            #[cfg(unix)]
            let (uid, gid) = (unsafe { libc::getuid() }, unsafe { libc::getgid() });
            #[cfg(not(unix))]
            let (uid, gid) = (0u32, 0u32);
            let root_meta = InodeMeta::new_directory(1, uid, gid, S_IFDIR | 0o755);
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
            inode_map: Mutex::new(inode_map),
            inode_data: Mutex::new(inode_data),
            dir_data: Mutex::new(dir_data),
            manifest_data: Mutex::new(BTreeMap::new()),
            xattr_data: Mutex::new(BTreeMap::new()),
            refcounts: Mutex::new(BTreeMap::new()),
            logical_bytes: AtomicU64::new(0),
            inode_count: AtomicU64::new(1),
            wal: Mutex::new(None),
            last_root: Mutex::new(None),
            snapshots_by_version: Mutex::new(HashMap::new()),
            snapshots_by_name: Mutex::new(HashMap::new()),
        }
    }
}

// Note: Default is no longer impl'd — DictMetadataStore::new() requires an io: Arc<Mutex<StoreIo>>

impl DictMetadataStore {
    /// Access the underlying Io backend (for content operations like `State::push_all`
    /// via `FileStorageAdd::new(&mut *io_guard)`).
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
    pub fn increment_refcount(&self, digest: &Digest224) {
        let mut rc = self.refcounts.lock().unwrap();
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
    pub fn decrement_refcount(&self, digest: &Digest224) {
        let mut rc = self.refcounts.lock().unwrap();
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
    pub fn get_refcount(&self, digest: &Digest224) -> u64 {
        let rc = self.refcounts.lock().unwrap();
        rc.get(digest).copied().unwrap_or(0)
    }

    /// Return the number of blocks whose reference count has saturated at `u64::MAX`.
    ///
    /// Saturated blocks are *immortal* — they will never be garbage-collected.
    /// A non-zero count here indicates the GC dead-letter queue has accumulated
    /// blocks; this is surfaced as a warning in `slicefs scrub`.
    pub fn saturated_refcount_count(&self) -> usize {
        let rc = self.refcounts.lock().unwrap();
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
        *self.wal.lock().unwrap() = Some(wal);
    }

    /// Flush all buffered WAL mutations to disk and call sync_all.
    ///
    /// This is called by the fsync FUSE callback to ensure durability.
    /// Returns Ok(()) when no WAL is configured.
    pub fn flush_wal(&self) -> Result<(), WalError> {
        if let Some(ref w) = *self.wal.lock().unwrap() {
            w.flush_and_sync()?;
        }
        Ok(())
    }

    /// Flush remaining mutations and shut down the WAL cleanly.
    ///
    /// Should be called before dropping the store (e.g., in `destroy()`).
    pub fn shutdown_wal(&self) -> Result<(), WalError> {
        if let Some(ref w) = *self.wal.lock().unwrap() {
            w.shutdown()?;
        }
        Ok(())
    }

    /// Log a single WAL entry if a WAL strategy is configured.
    ///
    /// IMPORTANT: Do NOT hold any other Mutex when calling this — WAL I/O
    /// may block and holding dict/inode_map locks would cause deadlocks.
    fn log_wal_entry(&self, entry: &WalEntry) {
        if let Some(ref w) = *self.wal.lock().unwrap() {
            // Ignore WAL errors during mutation logging — the in-memory state
            // is already correct. WAL failures are surfaced via flush_wal/shutdown_wal.
            let _ = w.log_mutation(entry);
        }
    }

    /// Return the last committed root digest, or `None` if `commit()` has not been called yet.
    ///
    /// Used by the background GC thread to determine which root to use as the live-set anchor.
    pub fn current_root(&self) -> Option<Digest224> {
        *self.last_root.lock().unwrap()
    }

    /// Load a snapshot list after store reconstruction from segment replay.
    ///
    /// Called by `mount.rs::load_store` after `load_store_from_segments` returns
    /// the snapshot list. Replaces whatever is in memory (typically empty).
    pub fn set_snapshots(&mut self, snapshots: Vec<SnapshotEntry>) {
        let mut by_ver = self.snapshots_by_version.lock().unwrap();
        let mut by_name = self.snapshots_by_name.lock().unwrap();
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
            let by_ver = self.snapshots_by_version.lock().unwrap();
            by_ver.keys().max().copied().unwrap_or(0) + 1
        };

        // Capture creation timestamp.
        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let entry = SnapshotEntry {
            version,
            name: name.clone(),
            root,
            created_at,
        };

        // Write SnapshotRecord to WAL (same path as commit writes RootUpdate).
        self.log_wal_entry(&WalEntry::Snapshot {
            version,
            root,
            created_at,
            name,
        });

        // Insert into both indexes.
        {
            let mut by_ver = self.snapshots_by_version.lock().unwrap();
            by_ver.insert(entry.version, entry.clone());
        }
        if let Some(ref n) = entry.name {
            let mut by_name = self.snapshots_by_name.lock().unwrap();
            by_name.insert(n.clone(), entry.version);
        }

        Ok(entry)
    }

    /// Return all snapshots sorted by version ascending.
    pub fn list_snapshots(&self) -> Vec<SnapshotEntry> {
        let by_ver = self.snapshots_by_version.lock().unwrap();
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
            self.snapshots_by_version
                .lock()
                .unwrap()
                .get(&version)
                .cloned()
        } else {
            let by_name = self.snapshots_by_name.lock().unwrap();
            let version = *by_name.get(reference)?;
            drop(by_name);
            self.snapshots_by_version
                .lock()
                .unwrap()
                .get(&version)
                .cloned()
        }
    }

    /// Return all roots that GC must treat as live-set anchors.
    ///
    /// Includes every snapshot root plus the current committed root (if any).
    /// Used by both background GC and offline `slicefs gc`.
    pub fn snapshot_roots(&self) -> Vec<Digest224> {
        let by_ver = self.snapshots_by_version.lock().unwrap();
        let mut roots: Vec<Digest224> = by_ver.values().map(|s| s.root).collect();
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
    pub fn commit_root(&self, root: Digest224) -> Result<(), MetaError> {
        *self.last_root.lock().unwrap() = Some(root);
        self.log_wal_entry(&WalEntry::RootUpdate { root });
        Ok(())
    }
}

impl MetadataStore for DictMetadataStore {
    fn create_inode(&self, meta: &InodeMeta) -> Result<InodeId, MetaError> {
        let mut inode_map = self.inode_map.lock().unwrap();
        let ino = inode_map.allocate_ino();
        let mut full_meta = meta.clone();
        full_meta.ino = ino;

        let digest = {
            let mut io_guard = self.io.lock().unwrap();
            let mut fsa = FileStorageAdd::new(&mut *io_guard);
            intern_inode(&mut fsa, &full_meta)
        };

        inode_map.insert(ino, digest);
        self.inode_data.lock().unwrap().insert(ino, digest);

        // Track logical bytes: add this inode's size to running total.
        if full_meta.size > 0 {
            self.logical_bytes
                .fetch_add(full_meta.size, Ordering::Relaxed);
        }

        // Track inode count.
        self.inode_count.fetch_add(1, Ordering::Relaxed);

        Ok(ino)
    }

    fn get_inode(&self, ino: InodeId) -> Result<InodeMeta, MetaError> {
        let inode_data = self.inode_data.lock().unwrap();
        let digest = inode_data
            .get(&ino)
            .copied()
            .ok_or(MetaError::NotFound(ino))?;
        drop(inode_data);

        let mut io_guard = self.io.lock().unwrap();
        load_inode(&mut *io_guard, &digest)
    }

    fn update_inode(&self, meta: &InodeMeta) -> Result<(), MetaError> {
        let ino = meta.ino;

        // Capture old size before replacing the digest, so we can adjust logical_bytes.
        let old_size = {
            let inode_data = self.inode_data.lock().unwrap();
            let old_digest = inode_data
                .get(&ino)
                .copied()
                .ok_or(MetaError::NotFound(ino))?;
            drop(inode_data);
            let mut io_guard = self.io.lock().unwrap();
            load_inode(&mut *io_guard, &old_digest)
                .map(|m| m.size)
                .unwrap_or(0)
        };

        let new_digest = {
            let mut io_guard = self.io.lock().unwrap();
            let mut fsa = FileStorageAdd::new(&mut *io_guard);
            intern_inode(&mut fsa, meta)
        };

        self.inode_data.lock().unwrap().insert(ino, new_digest);
        self.inode_map.lock().unwrap().insert(ino, new_digest);

        // Adjust logical_bytes by the size delta (saturating arithmetic prevents underflow).
        let new_size = meta.size;
        if new_size > old_size {
            self.logical_bytes
                .fetch_add(new_size - old_size, Ordering::Relaxed);
        } else if old_size > new_size {
            self.logical_bytes
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                    Some(cur.saturating_sub(old_size - new_size))
                })
                .ok();
        }

        Ok(())
    }

    fn delete_inode(&self, ino: InodeId) -> Result<(), MetaError> {
        // Capture size before removing so we can decrement logical_bytes.
        let size = {
            let inode_data = self.inode_data.lock().unwrap();
            if let Some(digest) = inode_data.get(&ino).copied() {
                drop(inode_data);
                let mut io_guard = self.io.lock().unwrap();
                load_inode(&mut *io_guard, &digest)
                    .map(|m| m.size)
                    .unwrap_or(0)
            } else {
                return Err(MetaError::NotFound(ino));
            }
        };

        self.inode_data.lock().unwrap().remove(&ino);
        self.inode_map.lock().unwrap().remove(ino);

        // Decrement logical_bytes by the deleted inode's size.
        if size > 0 {
            self.logical_bytes
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                    Some(cur.saturating_sub(size))
                })
                .ok();
        }

        // Decrement inode count (saturating to avoid underflow).
        self.inode_count
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |c| {
                Some(c.saturating_sub(1))
            })
            .ok();

        Ok(())
    }

    fn create_directory(
        &self,
        parent_ino: InodeId,
        name: &str,
        meta: &InodeMeta,
    ) -> Result<InodeId, MetaError> {
        // Validate parent exists and is a directory
        let parent_meta = self.get_inode(parent_ino)?;
        if parent_meta.mode & S_IFDIR == 0 {
            return Err(MetaError::NotADirectory(parent_ino));
        }
        if !self.dir_data.lock().unwrap().contains_key(&parent_ino) {
            return Err(MetaError::NotADirectory(parent_ino));
        }

        // Allocate new inode number
        let mut inode_map = self.inode_map.lock().unwrap();
        let ino = inode_map.allocate_ino();
        let mut dir_meta = meta.clone();
        dir_meta.ino = ino;
        // Ensure mode has directory type bit
        if dir_meta.mode & S_IFDIR == 0 {
            dir_meta.mode |= S_IFDIR;
        }

        // Read parent_dir_digest before acquiring io — avoids panic-under-lock
        // if the entry is unexpectedly missing (TOCTOU with concurrent unlink).
        let parent_dir_digest = match self.dir_data.lock().unwrap().get(&parent_ino).copied() {
            Some(d) => d,
            None => return Err(MetaError::NotADirectory(parent_ino)),
        };

        let (dir_inode_digest, dir_entry_digest, new_parent_dir_digest) = {
            let mut io_guard = self.io.lock().unwrap();

            // Create new directory's inode, dir entries, and update parent — all via one FSA
            let mut fsa = FileStorageAdd::new(&mut *io_guard);
            let dir_inode_digest = intern_inode(&mut fsa, &dir_meta);
            let dir_entry_digest = create_dir_entries(&mut fsa, ino, parent_ino);
            drop(fsa);

            // add_dir_entry needs &mut impl Io (reads then writes)
            let new_parent_dir_digest =
                add_dir_entry(&mut *io_guard, &parent_dir_digest, name, ino)?;

            (dir_inode_digest, dir_entry_digest, new_parent_dir_digest)
        };

        // Update maps
        inode_map.insert(ino, dir_inode_digest);
        drop(inode_map);

        self.inode_data
            .lock()
            .unwrap()
            .insert(ino, dir_inode_digest);
        self.dir_data.lock().unwrap().insert(ino, dir_entry_digest);
        self.dir_data
            .lock()
            .unwrap()
            .insert(parent_ino, new_parent_dir_digest);

        // Increment parent nlinks (for the .. backlink from new subdir)
        let mut parent_meta = self.get_inode(parent_ino)?;
        parent_meta.nlinks += 1;
        self.update_inode(&parent_meta)?;

        // Track inode count for the new directory inode.
        self.inode_count.fetch_add(1, Ordering::Relaxed);

        Ok(ino)
    }

    fn list_directory(&self, ino: InodeId) -> Result<Vec<DirEntry>, MetaError> {
        let dir_data = self.dir_data.lock().unwrap();
        let dir_digest = dir_data
            .get(&ino)
            .copied()
            .ok_or(MetaError::NotADirectory(ino))?;
        drop(dir_data);

        let mut io_guard = self.io.lock().unwrap();
        list_dir_entries(&mut *io_guard, &dir_digest)
    }

    fn lookup(&self, parent_ino: InodeId, name: &str) -> Result<InodeId, MetaError> {
        let dir_data = self.dir_data.lock().unwrap();
        let dir_digest = dir_data
            .get(&parent_ino)
            .copied()
            .ok_or(MetaError::NotADirectory(parent_ino))?;
        drop(dir_data);

        let mut io_guard = self.io.lock().unwrap();
        lookup_dir_entry(&mut *io_guard, &dir_digest, name).map_err(|_| MetaError::NotFound(0))
    }

    fn link(&self, parent_ino: InodeId, name: &str, ino: InodeId) -> Result<(), MetaError> {
        let dir_data = self.dir_data.lock().unwrap();
        let dir_digest = dir_data
            .get(&parent_ino)
            .copied()
            .ok_or(MetaError::NotADirectory(parent_ino))?;
        drop(dir_data);

        let new_dir_digest = {
            let mut io_guard = self.io.lock().unwrap();
            add_dir_entry(&mut *io_guard, &dir_digest, name, ino)?
        };

        self.dir_data
            .lock()
            .unwrap()
            .insert(parent_ino, new_dir_digest);
        Ok(())
    }

    fn unlink(&self, parent_ino: InodeId, name: &str) -> Result<(), MetaError> {
        let dir_data = self.dir_data.lock().unwrap();
        let dir_digest = dir_data
            .get(&parent_ino)
            .copied()
            .ok_or(MetaError::NotADirectory(parent_ino))?;
        drop(dir_data);

        let new_dir_digest = {
            let mut io_guard = self.io.lock().unwrap();
            remove_dir_entry(&mut *io_guard, &dir_digest, name)?
        };

        self.dir_data
            .lock()
            .unwrap()
            .insert(parent_ino, new_dir_digest);
        Ok(())
    }

    fn set_manifest(&self, ino: InodeId, blocks: &[Digest224]) -> Result<(), MetaError> {
        let digest = {
            let mut io_guard = self.io.lock().unwrap();
            let mut fsa = FileStorageAdd::new(&mut *io_guard);
            intern_manifest(&mut fsa, blocks)
        };
        self.manifest_data.lock().unwrap().insert(ino, digest);
        Ok(())
    }

    fn get_manifest(&self, ino: InodeId) -> Result<Vec<Digest224>, MetaError> {
        let manifest_data = self.manifest_data.lock().unwrap();
        let digest = manifest_data
            .get(&ino)
            .copied()
            .ok_or(MetaError::NotFound(ino))?;
        drop(manifest_data);

        let mut io_guard = self.io.lock().unwrap();
        load_manifest(&mut *io_guard, &digest)
    }

    fn set_xattr(&self, ino: InodeId, name: &str, value: &[u8]) -> Result<(), MetaError> {
        // Load existing xattrs for this inode (or empty vec if none yet)
        let mut xattrs = {
            let xattr_data = self.xattr_data.lock().unwrap();
            if let Some(digest) = xattr_data.get(&ino).copied() {
                drop(xattr_data);
                let mut io_guard = self.io.lock().unwrap();
                load_xattrs(&mut *io_guard, &digest)?
            } else {
                vec![]
            }
        };

        set_xattr_entry(&mut xattrs, name, value);

        let new_digest = {
            let mut io_guard = self.io.lock().unwrap();
            let mut fsa = FileStorageAdd::new(&mut *io_guard);
            intern_xattrs(&mut fsa, &xattrs)
        };

        self.xattr_data.lock().unwrap().insert(ino, new_digest);
        Ok(())
    }

    fn get_xattr(&self, ino: InodeId, name: &str) -> Result<Vec<u8>, MetaError> {
        let xattr_data = self.xattr_data.lock().unwrap();
        let digest = match xattr_data.get(&ino).copied() {
            Some(d) => d,
            None => return Err(MetaError::NotFound(ino)),
        };
        drop(xattr_data);

        let mut io_guard = self.io.lock().unwrap();
        let xattrs = load_xattrs(&mut *io_guard, &digest)?;
        drop(io_guard);

        get_xattr_entry(&xattrs, name).ok_or(MetaError::NotFound(ino))
    }

    fn list_xattrs(&self, ino: InodeId) -> Result<Vec<String>, MetaError> {
        let xattr_data = self.xattr_data.lock().unwrap();
        let digest = match xattr_data.get(&ino).copied() {
            Some(d) => d,
            None => return Ok(vec![]),
        };
        drop(xattr_data);

        let mut io_guard = self.io.lock().unwrap();
        let xattrs = load_xattrs(&mut *io_guard, &digest)?;
        drop(io_guard);

        Ok(list_xattr_names(&xattrs))
    }

    fn remove_xattr(&self, ino: InodeId, name: &str) -> Result<(), MetaError> {
        let xattr_data = self.xattr_data.lock().unwrap();
        let digest = match xattr_data.get(&ino).copied() {
            Some(d) => d,
            None => return Err(MetaError::NotFound(ino)),
        };
        drop(xattr_data);

        let mut xattrs = {
            let mut io_guard = self.io.lock().unwrap();
            load_xattrs(&mut *io_guard, &digest)?
        };

        if !remove_xattr_entry(&mut xattrs, name) {
            return Err(MetaError::NotFound(ino));
        }

        let new_digest = {
            let mut io_guard = self.io.lock().unwrap();
            let mut fsa = FileStorageAdd::new(&mut *io_guard);
            intern_xattrs(&mut fsa, &xattrs)
        };

        self.xattr_data.lock().unwrap().insert(ino, new_digest);
        Ok(())
    }

    fn root_ino(&self) -> InodeId {
        1
    }

    fn commit(&self) -> Result<Digest224, MetaError> {
        // Serialize full in-memory state into a root record (184 bytes):
        //   [inode_map_digest:     28 bytes (Digest224)]
        //   [root_dir_ino:          8 bytes (u64 LE)]
        //   [next_ino:              8 bytes (u64 LE)]
        //   [inode_data_digest:    28 bytes (Digest224)] -- serialized BTreeMap<u64, Digest224>
        //   [dir_data_digest:      28 bytes (Digest224)] -- serialized BTreeMap<u64, Digest224>
        //   [manifest_data_digest: 28 bytes (Digest224)]
        //   [xattr_data_digest:    28 bytes (Digest224)]
        //   [refcount_data_digest: 28 bytes (Digest224)] -- NEW in v2
        //
        // Backward compat: root records of exactly 156 bytes are old format (no refcounts).
        let inode_map = self.inode_map.lock().unwrap();
        let mut io_guard = self.io.lock().unwrap();
        let mut fsa = FileStorageAdd::new(&mut *io_guard);

        let inode_map_digest = intern_inode_map(&mut fsa, &inode_map);
        let next_ino = inode_map.next_ino();
        drop(inode_map);

        let inode_data_digest = {
            let map = self.inode_data.lock().unwrap();
            intern_u64_digest_map(&mut fsa, &map)
        };
        let dir_data_digest = {
            let map = self.dir_data.lock().unwrap();
            intern_u64_digest_map(&mut fsa, &map)
        };
        let manifest_data_digest = {
            let map = self.manifest_data.lock().unwrap();
            intern_u64_digest_map(&mut fsa, &map)
        };
        let xattr_data_digest = {
            let map = self.xattr_data.lock().unwrap();
            intern_u64_digest_map(&mut fsa, &map)
        };
        let refcount_data_digest = {
            let rc = self.refcounts.lock().unwrap();
            intern_digest224_u64_map(&mut fsa, &rc)
        };

        let mut root_bytes = Vec::with_capacity(184);
        // inode_map_digest: 28 bytes
        for word in &inode_map_digest {
            root_bytes.extend_from_slice(&word.to_le_bytes());
        }
        // root_dir_ino: 8 bytes (always 1)
        root_bytes.extend_from_slice(&1u64.to_le_bytes());
        // next_ino: 8 bytes
        root_bytes.extend_from_slice(&next_ino.to_le_bytes());
        // inode_data_digest: 28 bytes
        for word in &inode_data_digest {
            root_bytes.extend_from_slice(&word.to_le_bytes());
        }
        // dir_data_digest: 28 bytes
        for word in &dir_data_digest {
            root_bytes.extend_from_slice(&word.to_le_bytes());
        }
        // manifest_data_digest: 28 bytes
        for word in &manifest_data_digest {
            root_bytes.extend_from_slice(&word.to_le_bytes());
        }
        // xattr_data_digest: 28 bytes
        for word in &xattr_data_digest {
            root_bytes.extend_from_slice(&word.to_le_bytes());
        }
        // refcount_data_digest: 28 bytes (new in v2 — 184-byte format)
        for word in &refcount_data_digest {
            root_bytes.extend_from_slice(&word.to_le_bytes());
        }

        assert_eq!(root_bytes.len(), 184, "root record must be 184 bytes");
        let root_digest = State::push_all(&mut fsa, &root_bytes);
        // Drop fsa first: this flushes all FileStorage blob files to disk via StoreIo
        drop(fsa);
        // Drop io_guard to release the lock before WAL logging
        drop(io_guard);

        // Update last_root so background GC can use it
        *self.last_root.lock().unwrap() = Some(root_digest);

        // Log RootUpdate to WAL AFTER all nodes are flushed to disk (crash-safe ordering).
        // If we crash between flush and WAL write, the next mount will just re-commit.
        self.log_wal_entry(&WalEntry::RootUpdate { root: root_digest });

        Ok(root_digest)
    }
}

// ─── helpers for BTreeMap<u64, Digest224> serialization ─────────────────────

/// Serialize and store a `BTreeMap<u64, Digest224>` via a StorageAdd backend.
///
/// Format: `[count: u64 LE][for each entry: u64 LE ino + 28 bytes Digest224]`
/// Each entry is 36 bytes; total = 8 + count × 36.
fn intern_u64_digest_map(
    storage: &mut impl blockset::StorageAdd,
    map: &BTreeMap<u64, Digest224>,
) -> Digest224 {
    let mut bytes = Vec::with_capacity(8 + map.len() * 36);
    bytes.extend_from_slice(&(map.len() as u64).to_le_bytes());
    for (ino, digest) in map {
        bytes.extend_from_slice(&ino.to_le_bytes());
        for word in digest {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
    }
    State::push_all(storage, &bytes)
}

/// Retrieve and deserialize a `BTreeMap<u64, Digest224>` from file-backed storage.
fn load_u64_digest_map(
    io: &mut impl blockset::Io,
    key: &Digest224,
) -> Result<BTreeMap<u64, Digest224>, MetaError> {
    let bytes = file_storage_get(io, key)
        .ok_or_else(|| MetaError::Corrupted(format!("missing u64-digest map node {:?}", key)))?;

    if bytes.len() < 8 {
        return Err(MetaError::Corrupted(format!(
            "u64-digest map: expected at least 8 bytes, got {}",
            bytes.len()
        )));
    }
    let count = u64::from_le_bytes(bytes[0..8].try_into().unwrap()) as usize;
    let expected = 8 + count * 36;
    if bytes.len() != expected {
        return Err(MetaError::Corrupted(format!(
            "u64-digest map: expected {} bytes for {} entries, got {}",
            expected,
            count,
            bytes.len()
        )));
    }

    let mut map = BTreeMap::new();
    for i in 0..count {
        let off = 8 + i * 36;
        let ino = u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap());
        let mut digest: Digest224 = [0u32; 7];
        for (j, word) in digest.iter_mut().enumerate() {
            let o = off + 8 + j * 4;
            *word = u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
        }
        map.insert(ino, digest);
    }
    Ok(map)
}

impl DictMetadataStore {
    /// Reconstruct a `DictMetadataStore` from a file-backed Io and a root digest
    /// previously returned by `commit()`.
    ///
    /// Restores all in-memory maps (inode_data, dir_data, manifest_data, xattr_data)
    /// and inode numbering state so that subsequent operations continue seamlessly.
    pub fn load_from_root(io: Arc<Mutex<StoreIo>>, root: &Digest224) -> Result<Self, MetaError> {
        let mut io_guard = io.lock().unwrap();

        // Read root record bytes
        let bytes = file_storage_get(&mut *io_guard, root)
            .ok_or_else(|| MetaError::Corrupted("missing root node".into()))?;

        // Accept both old (156-byte) and new (184-byte) formats.
        let has_refcounts = match bytes.len() {
            156 => false, // v1 format — no refcounts field
            184 => true,  // v2 format — includes refcount_data_digest
            n => {
                return Err(MetaError::Corrupted(format!(
                    "root record: expected 156 or 184 bytes, got {}",
                    n
                )));
            }
        };

        // Parse the 7 fixed-width fields
        let mut off = 0;

        let inode_map_digest = parse_digest224(&bytes[off..off + 28]);
        off += 28;

        let root_dir_ino = u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap());
        off += 8;

        let next_ino = u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap());
        off += 8;

        let inode_data_digest = parse_digest224(&bytes[off..off + 28]);
        off += 28;

        let dir_data_digest = parse_digest224(&bytes[off..off + 28]);
        off += 28;

        let manifest_data_digest = parse_digest224(&bytes[off..off + 28]);
        off += 28;

        let xattr_data_digest = parse_digest224(&bytes[off..off + 28]);
        off += 28;

        // v2 format: read refcount_data_digest at offset 156-184
        let refcount_data_digest_opt: Option<Digest224> = if has_refcounts {
            Some(parse_digest224(&bytes[off..off + 28]))
        } else {
            None
        };

        // Load and patch inode_map so next_ino is exactly restored.
        let mut inode_map = load_inode_map(&mut *io_guard, &inode_map_digest)?;
        inode_map.set_next_ino(next_ino);

        let inode_data = load_u64_digest_map(&mut *io_guard, &inode_data_digest)?;
        let dir_data = load_u64_digest_map(&mut *io_guard, &dir_data_digest)?;
        let manifest_data = load_u64_digest_map(&mut *io_guard, &manifest_data_digest)?;
        let xattr_data = load_u64_digest_map(&mut *io_guard, &xattr_data_digest)?;
        let refcounts = if let Some(ref rc_digest) = refcount_data_digest_opt {
            load_digest224_u64_map(&mut *io_guard, rc_digest)?
        } else {
            BTreeMap::new()
        };

        // Sanity: root directory inode must exist
        if !inode_data.contains_key(&root_dir_ino) {
            return Err(MetaError::Corrupted(format!(
                "root dir ino {} not found in inode_data after reload",
                root_dir_ino
            )));
        }

        // Recompute logical_bytes as the sum of all loaded inode sizes.
        let initial_logical_bytes: u64 = inode_data
            .values()
            .map(|digest| {
                load_inode(&mut *io_guard, digest)
                    .map(|m| m.size)
                    .unwrap_or(0)
            })
            .sum();

        // Recompute inode_count from inode_data length.
        let initial_inode_count = inode_data.len() as u64;

        drop(io_guard);

        Ok(DictMetadataStore {
            io,
            inode_map: Mutex::new(inode_map),
            inode_data: Mutex::new(inode_data),
            dir_data: Mutex::new(dir_data),
            manifest_data: Mutex::new(manifest_data),
            xattr_data: Mutex::new(xattr_data),
            refcounts: Mutex::new(refcounts),
            logical_bytes: AtomicU64::new(initial_logical_bytes),
            inode_count: AtomicU64::new(initial_inode_count),
            wal: Mutex::new(None),
            last_root: Mutex::new(Some(*root)),
            snapshots_by_version: Mutex::new(HashMap::new()),
            snapshots_by_name: Mutex::new(HashMap::new()),
        })
    }
}

/// Serialize and store a `BTreeMap<Digest224, u64>` via a StorageAdd backend.
///
/// Format: `[count: u64 LE][for each entry: 28 bytes Digest224 + 8 bytes u64 LE]`
/// Each entry is 36 bytes; total = 8 + count × 36.
fn intern_digest224_u64_map(
    storage: &mut impl blockset::StorageAdd,
    map: &BTreeMap<Digest224, u64>,
) -> Digest224 {
    let mut bytes = Vec::with_capacity(8 + map.len() * 36);
    bytes.extend_from_slice(&(map.len() as u64).to_le_bytes());
    for (digest, count) in map {
        for word in digest {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        bytes.extend_from_slice(&count.to_le_bytes());
    }
    State::push_all(storage, &bytes)
}

/// Retrieve and deserialize a `BTreeMap<Digest224, u64>` from file-backed storage.
fn load_digest224_u64_map(
    io: &mut impl blockset::Io,
    key: &Digest224,
) -> Result<BTreeMap<Digest224, u64>, MetaError> {
    let bytes = file_storage_get(io, key)
        .ok_or_else(|| MetaError::Corrupted(format!("missing digest224-u64 map node {:?}", key)))?;

    if bytes.len() < 8 {
        return Err(MetaError::Corrupted(format!(
            "digest224-u64 map: expected at least 8 bytes, got {}",
            bytes.len()
        )));
    }
    let count = u64::from_le_bytes(bytes[0..8].try_into().unwrap()) as usize;
    let expected = 8 + count * 36;
    if bytes.len() != expected {
        return Err(MetaError::Corrupted(format!(
            "digest224-u64 map: expected {} bytes for {} entries, got {}",
            expected,
            count,
            bytes.len()
        )));
    }

    let mut map = BTreeMap::new();
    for i in 0..count {
        let off = 8 + i * 36;
        let mut digest: Digest224 = [0u32; 7];
        for (j, word) in digest.iter_mut().enumerate() {
            let o = off + j * 4;
            *word = u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
        }
        let count_val = u64::from_le_bytes(bytes[off + 28..off + 36].try_into().unwrap());
        map.insert(digest, count_val);
    }
    Ok(map)
}

/// Parse a `Digest224` from a 28-byte slice.
fn parse_digest224(bytes: &[u8]) -> Digest224 {
    assert_eq!(bytes.len(), 28);
    let mut d = [0u32; 7];
    for (i, word) in d.iter_mut().enumerate() {
        let off = i * 4;
        *word = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
    }
    d
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
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
        let io = Arc::new(Mutex::new(StoreIo::new(dir.path())));
        let store = DictMetadataStore::new(io);
        (dir, store)
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
        let found = parent_entries
            .iter()
            .any(|e| e.name == "subdir" && e.ino == ino);
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

        let blocks: Vec<Digest224> = (0..5).map(|i| [i as u32; 7]).collect();
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
        assert_ne!(digest, [0u32; 7], "commit should return non-zero digest");
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
        let blocks: Vec<Digest224> = vec![[1u32; 7], [2u32; 7]];
        store.set_manifest(file_ino, &blocks).unwrap();

        // Add a subdirectory
        let dir_ino = store
            .create_directory(1, "subdir", &new_dir_meta())
            .unwrap();

        // Add xattrs
        store
            .set_xattr(file_ino, "user.meta", b"mymetavalue")
            .unwrap();
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
        assert!(
            entries
                .iter()
                .any(|e| e.name == "myfile" && e.ino == file_ino)
        );
        assert!(
            entries
                .iter()
                .any(|e| e.name == "subdir" && e.ino == dir_ino)
        );

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
            assert!(
                reloaded.get_inode(ino).is_ok(),
                "ino {} missing after reload",
                ino
            );
        }

        // Next allocated inode continues from where it left off (no reuse)
        let next_ino = reloaded.create_inode(&new_file_meta()).unwrap();
        for &old_ino in &inos {
            assert_ne!(next_ino, old_ino, "inode {} was reused!", next_ino);
        }
        // next_ino must be > max of previous inos
        let max_ino = *inos.iter().max().unwrap();
        assert!(
            next_ino > max_ino,
            "next_ino {} not > max_ino {}",
            next_ino,
            max_ino
        );
    }

    #[test]
    fn test_directory_stable_across_reload() {
        let (_dir, store) = make_store();
        let sub1 = store.create_directory(1, "alpha", &new_dir_meta()).unwrap();
        let sub2 = store.create_directory(1, "beta", &new_dir_meta()).unwrap();
        let _sub3 = store
            .create_directory(sub1, "gamma", &new_dir_meta())
            .unwrap();

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
        let blocks: Vec<Digest224> = (0..10).map(|i| [i as u32; 7]).collect();
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
        let blocks: Vec<Digest224> = vec![[0xABu32; 7]];
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

    /// Verify that content pushed into `store.io()` via `FileStorageAdd` is
    /// accessible after a commit + `load_from_root` cycle.
    ///
    /// This proves the seed command's design is sound: file content and metadata
    /// can share the same Io backend, and nothing is lost on a round-trip.
    #[test]
    fn test_io_accessor_content_survives_reload() {
        let (_dir, store) = make_store();

        // Push content bytes into the store's io via FileStorageAdd.
        let content = b"hello, SliceFS content round-trip test!";
        let content_digest: Digest224 = {
            let mut io_guard = store.io().lock().unwrap();
            let mut fsa = FileStorageAdd::new(&mut *io_guard);
            State::push_all(&mut fsa, content)
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

        // Verify the content bytes are retrievable from the file storage via file_storage_get.
        let mut io_guard = reloaded.io().lock().unwrap();
        let read_back = file_storage_get(&mut *io_guard, &content_digest).expect("content missing");
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

        let snap = store
            .create_snapshot(None)
            .expect("create_snapshot should succeed");
        assert_eq!(snap.version, 1, "first snapshot must have version 1");
        assert!(snap.name.is_none(), "no name expected");
        assert_ne!(snap.root, [0u32; 7], "snapshot root must not be zero");
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
        store
            .create_snapshot(Some("production".to_string()))
            .unwrap();

        let found = store
            .find_snapshot("2")
            .expect("should find by version string");
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

        store
            .create_snapshot(Some("release-1.0".to_string()))
            .unwrap();

        let found = store
            .find_snapshot("release-1.0")
            .expect("should find by name");
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
        assert!(
            roots.len() >= 2,
            "should include at least 2 roots: {:?}",
            roots
        );
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
        assert_eq!(
            roots.len(),
            1,
            "with no snapshots, roots should contain only current_root"
        );
    }

    #[test]
    fn test_snapshots_survive_segment_replay() {
        use crate::segment::load_store_from_segments;
        use crate::wal::{WalConfig, create_wal};
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("segments")).unwrap();

        // Create a store with a snapshot
        {
            let wal = create_wal(WalConfig::PerOp, dir.path(), 1).unwrap();
            let (_dir, mut store) = make_store();
            store.set_wal(wal);
            store
                .create_snapshot(Some("replay-test".to_string()))
                .unwrap();
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
            root: [1u32; 7],
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
            root: [version as u32; 7],
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
        assert!(
            store.find_snapshot("99").is_none(),
            "version 99 must not exist"
        );
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
        let snaps: Vec<_> = (1u64..=10_000).map(|v| make_snapshot(v, None)).collect();
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
        assert!(store.find_snapshot("twenty").is_none());
        assert_eq!(store.find_snapshot("ten").unwrap().version, 10);
        assert_eq!(store.find_snapshot("thirty").unwrap().version, 30);
        assert!(store.find_snapshot("20").is_some()); // unnamed, find by version
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
        let digest: Digest224 = [0xDEu32; 7];

        // Manually set refcount to u64::MAX - 1
        {
            let mut rc = store.refcounts.lock().unwrap();
            rc.insert(digest, u64::MAX - 1);
        }

        // One more increment brings it to u64::MAX
        store.increment_refcount(&digest);
        assert_eq!(
            store.get_refcount(&digest),
            u64::MAX,
            "refcount should be u64::MAX after increment from MAX-1"
        );

        // Another increment must stay at u64::MAX (not wrap to 0)
        store.increment_refcount(&digest);
        assert_eq!(
            store.get_refcount(&digest),
            u64::MAX,
            "refcount at u64::MAX must not wrap on increment"
        );
    }

    #[test]
    fn test_refcount_decrement_saturated() {
        let (_dir, store) = make_store();
        let digest: Digest224 = [0xABu32; 7];

        // Set refcount to u64::MAX directly
        {
            let mut rc = store.refcounts.lock().unwrap();
            rc.insert(digest, u64::MAX);
        }

        // Decrement must be a no-op for saturated blocks
        store.decrement_refcount(&digest);
        assert_eq!(
            store.get_refcount(&digest),
            u64::MAX,
            "decrement on saturated (u64::MAX) refcount must be a no-op — block is immortal"
        );
    }

    #[test]
    fn test_saturated_refcount_count() {
        let (_dir, store) = make_store();
        let d1: Digest224 = [0x01u32; 7];
        let d2: Digest224 = [0x02u32; 7];
        let d3: Digest224 = [0x03u32; 7];
        let d4: Digest224 = [0x04u32; 7];
        let d5: Digest224 = [0x05u32; 7];

        {
            let mut rc = store.refcounts.lock().unwrap();
            rc.insert(d1, u64::MAX); // saturated
            rc.insert(d2, u64::MAX); // saturated
            rc.insert(d3, 5); // normal
            rc.insert(d4, 2); // normal
            rc.insert(d5, 1); // normal
        }

        assert_eq!(
            store.saturated_refcount_count(),
            2,
            "should count exactly 2 saturated refcounts"
        );
    }

    // ── inode_count AtomicU64 tests (FIX-02 store layer) ─────────────────────

    #[test]
    fn test_inode_count_empty() {
        let (_dir, store) = make_store();
        // Fresh store has root inode (ino=1) — count should be 1
        assert_eq!(
            store.inode_count(),
            1,
            "fresh store should have inode_count == 1 (root inode)"
        );
    }

    #[test]
    fn test_inode_count_tracks_lifecycle() {
        let (_dir, store) = make_store();
        // Create 3 inodes (inos 2, 3, 4) — total = 1 root + 3 = 4
        store.create_inode(&new_file_meta()).unwrap();
        store.create_inode(&new_file_meta()).unwrap();
        let ino3 = store.create_inode(&new_file_meta()).unwrap();
        assert_eq!(
            store.inode_count(),
            4,
            "after creating 3 inodes, count should be 4 (root + 3)"
        );

        // Delete one -> should be 3
        store.delete_inode(ino3).unwrap();
        assert_eq!(
            store.inode_count(),
            3,
            "after deleting 1 inode, count should be 3"
        );
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

        assert_eq!(
            reloaded.inode_count(),
            count_before,
            "inode_count should match original after load_from_root round-trip"
        );
    }
}
