//! A metadata map as a prolly tree (`prolly`) with an overlay of the
//! changes that are not in the tree yet (#215, store format 8).
//!
//! A `MapTree` is the working root of one tree (`Tree`: the root and a
//! generation, bumped at each install) and an `Overlay`: the pending
//! edits (`Some(value)` or a delete), sharded by key, with the ticket scheme
//! of `store::Live`. No whole map is in RAM: reads go to the overlay and
//! then to the tree through the node cache of the store (`TreeIo`).
//!
//! # Protocol (every map tree follows it)
//!
//! - **Read.** Lock the key's overlay shard; a pending edit answers (a
//!   value, or none for a delete). Else unlock, read the working root, then
//!   `prolly::get_in`. Never read the root first and the overlay after: an
//!   entry leaves the overlay only after its tree root is installed, so a
//!   miss followed by a root read sees the value or a newer one.
//! - **Write.** Overlay only (`put` marks the entry dirty and drops a ticket
//!   in flight). No node I/O under a shard lock.
//! - **Read-modify-write.** `read_with_generation` gives the value and the tree
//!   generation read before the tree read. The caller locks again, checks the
//!   overlay and the generation, and retries on a change (no lost update).
//! - **Flush** (hot path, with no shard lock held, when `dirty()` reaches
//!   the store's limit). The caller holds the store's apply lock
//!   (`try_lock`, skipped when busy or while a commit is prepared). Take the
//!   dirty entries (each with a ticket), sort them by encoded key, apply them
//!   to the working root, install the new root (generation + 1), then settle: an
//!   entry leaves the overlay only if its ticket is still current (a change
//!   after the take keeps the newer value). On an error the taken entries
//!   are dirty again, and the caller keeps the error for the next commit.
//! - **Commit.** `begin_commit` waits out a running flush on the apply lock
//!   and marks the commit prepared, so no flush applies after the cut; it
//!   releases the lock before the write-log cut. After the cut it takes the
//!   dirty and in-flight entries with new tickets and reads the cut root
//!   (`take_for_commit`). `finish_commit` applies the batch to the cut root
//!   (`apply_batch`), writes the root record, then under the apply lock
//!   installs the new root and settles (`install_and_settle`) and ends the
//!   prepared state. A failure before the install (or a drop of an
//!   unfinished commit) requeues the overlay; after the install there is
//!   nothing to requeue: the state is in the working root.
//!
//! Tree errors are `MetaError::Corrupted("<map> tree: ...")` (an I/O error
//! stays `MetaError::Io`).

use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use chunk_store::lock_stats::Site;
use chunk_store::sync::{self, Rank};
use chunk_store::{ChunkStore, ObjectId};
use slicefs_traits::key::{ObjectKey, KEY_BYTES};
use slicefs_traits::metadata::MetaError;

/// Shards of an overlay.
const SHARDS: usize = 32;

/// The chunk store, node cache and tree config of the trees of a store.
pub(crate) struct TreeIo {
    cs: Arc<ChunkStore>,
    cache: Arc<prolly::NodeCache>,
    cfg: prolly::Config,
}

impl TreeIo {
    pub(crate) fn new(cs: Arc<ChunkStore>, cache: Arc<prolly::NodeCache>, cfg: prolly::Config) -> Self {
        TreeIo { cs, cache, cfg }
    }

    /// A context that reads through the node cache.
    pub(crate) fn ctx(&self) -> prolly::Ctx<'_> {
        prolly::Ctx::new(&self.cs, Some(&self.cache))
    }
}

/// A key of a map tree.
pub(crate) trait TreeKey: Copy + Eq + Hash {
    /// The tree key bytes (their order is the tree order).
    fn encode(&self) -> Vec<u8>;
    /// The overlay shard of the key (below `SHARDS`).
    fn shard(&self) -> usize;
}

/// A value of a map tree.
pub(crate) trait TreeValue: Clone + Sized {
    fn encode(&self) -> Vec<u8>;
    /// The value of the stored bytes; an error text if they are not one.
    fn decode(bytes: &[u8]) -> Result<Self, String>;
}

/// An inode number: 8 bytes big endian (tree order is number order).
impl TreeKey for u64 {
    fn encode(&self) -> Vec<u8> {
        self.to_be_bytes().to_vec()
    }

    fn shard(&self) -> usize {
        (*self % SHARDS as u64) as usize
    }
}

/// An object key: its 33 bytes.
impl TreeValue for ObjectKey {
    fn encode(&self) -> Vec<u8> {
        self.0.to_vec()
    }

    fn decode(bytes: &[u8]) -> Result<Self, String> {
        bytes.try_into().map(ObjectKey).map_err(|_| format!("value of {} bytes, not a {KEY_BYTES}-byte key", bytes.len()))
    }
}

/// The metadata error of a tree error of the map `name`.
pub(crate) fn tree_err(name: &str, e: prolly::Error) -> MetaError {
    match e {
        prolly::Error::Store(chunk_store::Error::Io(io)) => MetaError::Io(io),
        e => MetaError::Corrupted(format!("{name} tree: {e}")),
    }
}

/// Edits taken from an overlay: key, ticket of the take, value (`None`: a
/// delete), sorted by encoded key.
pub(crate) type Taken<K, V> = Vec<(K, u64, Option<V>)>;

/// One shard of an overlay (the scheme of `store::Live`).
struct OvShard<K, V> {
    /// The pending edits: dirty, or taken and in flight.
    map: HashMap<K, Option<V>>,
    dirty: HashSet<K>,
    /// Taken entries with the ticket of their take.
    flushing: HashMap<K, u64>,
    /// The last ticket given.
    tickets: u64,
}

/// The pending edits of a map, in `SHARDS` locks (`Rank::Store`). A
/// thread holds at most one shard at a time, never across node I/O.
pub(crate) struct Overlay<K, V> {
    shards: Vec<sync::Mutex<OvShard<K, V>>>,
    /// Dirty entries over all shards (moved under the shard locks).
    dirty: AtomicUsize,
}

impl<K: TreeKey, V: TreeValue> Overlay<K, V> {
    fn new(site: Site) -> Self {
        let shard = || OvShard { map: HashMap::new(), dirty: HashSet::new(), flushing: HashMap::new(), tickets: 0 };
        Overlay {
            shards: (0..SHARDS).map(|_| sync::Mutex::new(Rank::Store, shard()).timed(site)).collect(),
            dirty: AtomicUsize::new(0),
        }
    }

    /// Run `f` on shard `i` locked; the dirty count follows.
    fn with<R>(&self, i: usize, f: impl FnOnce(&mut OvShard<K, V>) -> R) -> R {
        let mut g = self.shards[i].lock();
        let before = g.dirty.len();
        let r = f(&mut g);
        let after = g.dirty.len();
        if after > before {
            self.dirty.fetch_add(after - before, Ordering::Relaxed);
        } else if before > after {
            self.dirty.fetch_sub(before - after, Ordering::Relaxed);
        }
        r
    }

    /// The pending edit of `k`, if any.
    fn get(&self, k: &K) -> Option<Option<V>> {
        self.shards[k.shard()].lock().map.get(k).cloned()
    }

    /// Set the pending edit of `k` (`None`: a delete).
    fn put(&self, k: K, v: Option<V>) {
        self.with(k.shard(), |s| {
            s.map.insert(k, v);
            s.dirty.insert(k);
            s.flushing.remove(&k);
        });
    }

    /// The dirty entries, plus the entries in flight if `in_flight` (a
    /// commit takes them all): each goes in flight with a new ticket.
    fn take(&self, in_flight: bool) -> Taken<K, V> {
        let mut out = Vec::new();
        for i in 0..SHARDS {
            self.with(i, |s| {
                let mut keys: Vec<K> = s.dirty.drain().collect();
                if in_flight {
                    keys.extend(s.flushing.keys().copied());
                }
                for k in keys {
                    if let Some(v) = s.map.get(&k) {
                        s.tickets += 1;
                        s.flushing.insert(k, s.tickets);
                        out.push((k, s.tickets, v.clone()));
                    }
                }
            });
        }
        out.sort_by_cached_key(|(k, _, _)| k.encode());
        out
    }

    /// After the install of a root that holds `taken`: each entry that
    /// still has its ticket leaves the overlay.
    fn settle(&self, taken: &Taken<K, V>) {
        for (k, t, _) in taken {
            self.with(k.shard(), |s| {
                if s.flushing.get(k) == Some(t) {
                    s.flushing.remove(k);
                    s.map.remove(k);
                }
            });
        }
    }

    /// A take failed: what is in flight is dirty again.
    fn requeue(&self) {
        for i in 0..SHARDS {
            self.with(i, |s| {
                let keys: Vec<K> = s.flushing.drain().map(|(k, _)| k).collect();
                s.dirty.extend(keys);
            });
        }
    }

    /// Dirty entries over all shards.
    fn dirty(&self) -> usize {
        self.dirty.load(Ordering::Relaxed)
    }
}

/// The working root of a tree and its generation.
pub(crate) struct Tree {
    root: sync::Mutex<ObjectId>,
    /// Bumped at each install, under the root lock.
    generation: AtomicU64,
}

impl Tree {
    fn new(site: Site, root: ObjectId) -> Self {
        Tree { root: sync::Mutex::new(Rank::Store, root).timed(site), generation: AtomicU64::new(0) }
    }

    /// The working root.
    pub(crate) fn root(&self) -> ObjectId {
        *self.root.lock()
    }

    /// The working root and its generation, read together.
    fn root_generation(&self) -> (ObjectId, u64) {
        let r = self.root.lock();
        (*r, self.generation.load(Ordering::Acquire))
    }

    /// Make `root` the working root: the root first, then the generation + 1.
    fn install(&self, root: ObjectId) {
        let mut r = self.root.lock();
        *r = root;
        self.generation.fetch_add(1, Ordering::AcqRel);
    }
}

/// A map as a tree with an overlay (see the module doc).
pub(crate) struct MapTree<K, V> {
    /// The map name for errors (`manifest_data`, ...).
    name: &'static str,
    overlay: Overlay<K, V>,
    tree: Tree,
    /// Test hooks (see `fail_next_flush` and `set_flush_hook`).
    fail_flush: AtomicBool,
    flush_hook: sync::Mutex<Option<FlushHook>>,
}

/// A function that tests run inside a flush (see `MapTree::set_flush_hook`).
pub(crate) type FlushHook = Arc<dyn Fn() + Send + Sync>;

impl<K: TreeKey, V: TreeValue> MapTree<K, V> {
    /// A map whose working root is `root`; its locks are timed as `site`.
    pub(crate) fn new(name: &'static str, site: Site, root: ObjectId) -> Self {
        MapTree {
            name,
            overlay: Overlay::new(site),
            tree: Tree::new(site, root),
            fail_flush: AtomicBool::new(false),
            flush_hook: sync::Mutex::new(Rank::Store, None),
        }
    }

    /// Make the next flush fail after its take (test hook): the entries are
    /// requeued and the flush returns an error.
    pub(crate) fn fail_next_flush(&self) {
        self.fail_flush.store(true, Ordering::SeqCst);
    }

    /// Run `f` in each flush, between the install of the new root and the
    /// settle of the overlay (test hook); `None` removes it. It runs with the
    /// caller's apply lock held and no overlay or root lock held.
    pub(crate) fn set_flush_hook(&self, f: Option<FlushHook>) {
        *self.flush_hook.lock() = f;
    }

    /// The working root.
    #[cfg(test)]
    pub(crate) fn root(&self) -> ObjectId {
        self.tree.root()
    }

    /// The value of `k`: the overlay, else the tree.
    pub(crate) fn read(&self, io: &TreeIo, k: &K) -> Result<Option<V>, MetaError> {
        self.read_with_generation(io, k).map(|(v, _)| v)
    }

    /// The value of `k` and the tree generation read before the tree read
    /// (for a read-modify-write: see the module doc).
    pub(crate) fn read_with_generation(&self, io: &TreeIo, k: &K) -> Result<(Option<V>, u64), MetaError> {
        if let Some(edit) = self.overlay.get(k) {
            return Ok((edit, self.generation()));
        }
        let (root, generation) = self.tree.root_generation();
        let got = prolly::get_in(&io.ctx(), &root, &k.encode()).map_err(|e| tree_err(self.name, e))?;
        let v = got
            .map(|b| V::decode(&b).map_err(|m| MetaError::Corrupted(format!("{} tree: {m}", self.name))))
            .transpose()?;
        Ok((v, generation))
    }

    /// The current tree generation.
    pub(crate) fn generation(&self) -> u64 {
        self.tree.generation.load(Ordering::Acquire)
    }

    /// Set `k` to `v` (`None`: delete it), in the overlay only.
    pub(crate) fn write(&self, k: K, v: Option<V>) {
        self.overlay.put(k, v);
    }

    /// Dirty entries of the overlay.
    pub(crate) fn dirty(&self) -> usize {
        self.overlay.dirty()
    }

    /// Hot-path flush (see the module doc): the caller holds the apply lock
    /// (`_apply`) and no shard lock, and no commit is prepared. The stats of
    /// the apply, or `None` if nothing was dirty.
    pub(crate) fn flush(&self, io: &TreeIo, _apply: &sync::MutexGuard<'_, ()>) -> Result<Option<prolly::ApplyStats>, MetaError> {
        let taken = self.overlay.take(false);
        if taken.is_empty() {
            return Ok(None);
        }
        if self.fail_flush.swap(false, Ordering::SeqCst) {
            self.overlay.requeue();
            return Err(MetaError::Corrupted(format!("{} tree: injected flush fault", self.name)));
        }
        match self.apply_batch(io, &self.tree.root(), &taken) {
            Ok((root, stats)) => {
                self.tree.install(root);
                let hook = self.flush_hook.lock().clone();
                if let Some(h) = hook {
                    h();
                }
                self.overlay.settle(&taken);
                Ok(Some(stats))
            }
            Err(e) => {
                self.overlay.requeue();
                Err(e)
            }
        }
    }

    /// For `begin_commit` (no flush runs): the cut root and every pending
    /// edit, with new tickets.
    pub(crate) fn take_for_commit(&self) -> (ObjectId, Taken<K, V>) {
        let taken = self.overlay.take(true);
        (self.tree.root(), taken)
    }

    /// Apply `taken` to `root`: the new root and the stats of the apply.
    /// Writes nodes, installs nothing.
    pub(crate) fn apply_batch(&self, io: &TreeIo, root: &ObjectId, taken: &Taken<K, V>) -> Result<(ObjectId, prolly::ApplyStats), MetaError> {
        if taken.is_empty() {
            return Ok((*root, prolly::ApplyStats::default()));
        }
        let edits: Vec<prolly::Edit> = taken
            .iter()
            .map(|(k, _, v)| match v {
                Some(v) => prolly::Edit::Put { key: k.encode(), value: v.encode() },
                None => prolly::Edit::Delete { key: k.encode() },
            })
            .collect();
        prolly::apply_in(&io.ctx(), io.cfg, root, &edits).map_err(|e| tree_err(self.name, e))
    }

    /// For `finish_commit`, under the apply lock: `root` (the result of
    /// `apply_batch` on the cut root) becomes the working root, then the
    /// entries of `taken` that kept their ticket leave the overlay.
    pub(crate) fn install_and_settle(&self, root: ObjectId, taken: &Taken<K, V>, _apply: &sync::MutexGuard<'_, ()>) {
        self.tree.install(root);
        self.overlay.settle(taken);
    }

    /// A commit failed before its install: what it took is dirty again.
    pub(crate) fn requeue(&self) {
        self.overlay.requeue();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chunk_store::mem::MemIo;

    fn io() -> TreeIo {
        let cs = ChunkStore::open_with(Arc::new(MemIo::new()), chunk_store::Options::default()).unwrap();
        TreeIo::new(Arc::new(cs), Arc::new(prolly::NodeCache::new(1 << 20)), prolly::Config::DEFAULT)
    }

    fn map(io: &TreeIo) -> MapTree<u64, ObjectKey> {
        MapTree::new("test", Site::ManifestData, prolly::empty_root(&io.cs).unwrap())
    }

    fn lock() -> sync::Mutex<()> {
        sync::Mutex::new(Rank::Store, ())
    }

    #[test]
    fn reads_see_the_overlay_then_the_tree() {
        let io = io();
        let m = map(&io);
        let apply = lock();
        for i in 0..100u64 {
            m.write(i, Some(ObjectKey::example(i as u32)));
        }
        assert_eq!(m.dirty(), 100);
        assert_eq!(m.read(&io, &7).unwrap(), Some(ObjectKey::example(7)));
        let empty = m.root();
        let stats = m.flush(&io, &apply.lock()).unwrap().unwrap();
        assert!(stats.nodes_written >= 1);
        assert_ne!(m.root(), empty);
        assert_eq!((m.dirty(), m.generation()), (0, 1));
        assert_eq!(m.read(&io, &7).unwrap(), Some(ObjectKey::example(7)), "from the tree");
        assert_eq!(prolly::count(&io.ctx(), &m.root()).unwrap(), 100);
        // A delete answers from the overlay before the flush, and from the
        // tree after it.
        m.write(7, None);
        assert_eq!(m.read(&io, &7).unwrap(), None);
        m.flush(&io, &apply.lock()).unwrap().unwrap();
        assert_eq!(m.read(&io, &7).unwrap(), None);
        assert_eq!(m.read(&io, &8).unwrap(), Some(ObjectKey::example(8)));
        assert!(m.flush(&io, &apply.lock()).unwrap().is_none(), "nothing dirty");
    }

    // A change between the take and the settle keeps the newer value in the
    // overlay; the install of the older one does not hide it.
    #[test]
    fn a_change_after_the_take_survives_the_settle() {
        let io = io();
        let m = map(&io);
        let apply = lock();
        m.write(1, Some(ObjectKey::example(1)));
        m.write(2, Some(ObjectKey::example(2)));
        let (root, taken) = m.take_for_commit();
        m.write(1, Some(ObjectKey::example(10)));
        m.write(2, None);
        let (new, _) = m.apply_batch(&io, &root, &taken).unwrap();
        m.install_and_settle(new, &taken, &apply.lock());
        assert_eq!(m.read(&io, &1).unwrap(), Some(ObjectKey::example(10)));
        assert_eq!(m.read(&io, &2).unwrap(), None);
        assert_eq!(m.dirty(), 2);
        // The tree has the taken values.
        assert_eq!(prolly::get_in(&io.ctx(), &new, &1u64.encode()).unwrap(), Some(ObjectKey::example(1).encode()));
        let (root, taken) = m.take_for_commit();
        let (new, _) = m.apply_batch(&io, &root, &taken).unwrap();
        m.install_and_settle(new, &taken, &apply.lock());
        assert_eq!(m.dirty(), 0);
        assert_eq!(m.read(&io, &1).unwrap(), Some(ObjectKey::example(10)));
        assert_eq!(m.read(&io, &2).unwrap(), None);
        assert_eq!(prolly::count(&io.ctx(), &m.root()).unwrap(), 1);
    }

    // A failed commit requeues: the next take has every entry again.
    #[test]
    fn a_requeue_makes_the_taken_entries_dirty_again() {
        let io = io();
        let m = map(&io);
        for i in 0..5u64 {
            m.write(i, Some(ObjectKey::example(i as u32)));
        }
        let (_, taken) = m.take_for_commit();
        assert_eq!((taken.len(), m.dirty()), (5, 0));
        let keys: Vec<u64> = taken.iter().map(|(k, _, _)| *k).collect();
        assert_eq!(keys, vec![0, 1, 2, 3, 4], "sorted by encoded key");
        m.requeue();
        assert_eq!(m.dirty(), 5);
        assert_eq!(m.take_for_commit().1.len(), 5);
    }

    #[test]
    fn read_with_gen_reports_the_generation_of_the_tree_read() {
        let io = io();
        let m = map(&io);
        let apply = lock();
        m.write(3, Some(ObjectKey::example(3)));
        m.flush(&io, &apply.lock()).unwrap();
        let (v, generation) = m.read_with_generation(&io, &3).unwrap();
        assert_eq!((v, generation), (Some(ObjectKey::example(3)), 1));
        m.write(3, Some(ObjectKey::example(4)));
        m.flush(&io, &apply.lock()).unwrap();
        assert_eq!(m.read_with_generation(&io, &3).unwrap(), (Some(ObjectKey::example(4)), 2));
    }

    #[test]
    fn a_bad_value_in_the_tree_is_corrupt() {
        let io = io();
        let (root, _) = prolly::build(&io.cs, prolly::Config::DEFAULT, [(5u64.encode(), vec![1u8, 2, 3])]).unwrap();
        let m: MapTree<u64, ObjectKey> = MapTree::new("test", Site::ManifestData, root);
        let e = m.read(&io, &5).unwrap_err();
        assert!(e.to_string().contains("test tree"), "{e}");
    }
}
