//! A bounded cache of decoded nodes, and `Ctx`, the handle every reader and
//! `apply` takes: the store plus an optional cache.
//!
//! The cache has a fixed byte cap and holds decoded nodes by `ObjectId`.
//! Phase 24 (D-17) forbids a cache that grows with the map; a cache of
//! fixed size is allowed. A node is charged its encoded length, plus 64
//! bytes per entry and 128 (the cost of the decoded form). The oldest node
//! (by last use) goes first when a new one does not fit.
//!
//! Only a node that came from the hash-verified `ChunkStore::read` and
//! passed decode is inserted, and it is keyed by its content ID, so a hit
//! is the node of that ID. The checks of a loaded node against its parent
//! entry run on every load, hit or not (see `node::load_child`).

use crate::node::Node;
use chunk_store::sync::{Mutex, Rank};
use chunk_store::{ChunkStore, ObjectId};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// Fixed cost of a cached node, and the cost of each of its entries.
const NODE_OVERHEAD: usize = 128;
const ENTRY_OVERHEAD: usize = 64;

/// A decoded node with what a load needs to know about it.
#[derive(Clone)]
pub(crate) struct Cached {
    pub node: Arc<Node>,
    /// Encoded bytes of the node.
    pub bytes: usize,
    /// Largest key, and largest encoded entry, in the node: a hit is
    /// checked against the limits of the config that asks.
    pub max_key: usize,
    pub max_entry: usize,
}

struct Slot {
    cached: Cached,
    tick: u64,
    charge: usize,
}

#[derive(Default)]
struct Inner {
    map: HashMap<ObjectId, Slot>,
    /// Use tick to node, oldest first.
    lru: BTreeMap<u64, ObjectId>,
    tick: u64,
    bytes: usize,
    hits: u64,
    misses: u64,
}

/// Counts and size of a `NodeCache`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Lookups that found the node.
    pub hits: u64,
    /// Lookups that did not.
    pub misses: u64,
    /// Nodes held.
    pub entries: usize,
    /// Bytes charged for the nodes held (never above `cap`).
    pub bytes: usize,
    /// The byte cap.
    pub cap: usize,
}

/// A cache of decoded nodes with a fixed byte cap. It is the only shared
/// state of this crate; it holds immutable content-addressed nodes behind
/// one lock, held only for map operations.
pub struct NodeCache {
    cap: usize,
    inner: Mutex<Inner>,
}

impl NodeCache {
    /// A cache that holds at most `cap_bytes` of charged node bytes. With
    /// 0 it keeps nothing.
    pub fn new(cap_bytes: usize) -> Self {
        Self { cap: cap_bytes, inner: Mutex::new(Rank::Store, Inner::default()) }
    }

    /// Counts and size now.
    pub fn stats(&self) -> CacheStats {
        let g = self.inner.lock();
        CacheStats { hits: g.hits, misses: g.misses, entries: g.map.len(), bytes: g.bytes, cap: self.cap }
    }

    /// Drops every node (the counts stay).
    pub fn clear(&self) {
        let mut g = self.inner.lock();
        g.map.clear();
        g.lru.clear();
        g.bytes = 0;
    }

    /// The node `id`, if held; counts a hit or a miss.
    pub(crate) fn get(&self, id: &ObjectId) -> Option<Cached> {
        let mut g = self.inner.lock();
        let g = &mut *g;
        g.tick += 1;
        let tick = g.tick;
        match g.map.get_mut(id) {
            Some(slot) => {
                g.lru.remove(&slot.tick);
                slot.tick = tick;
                g.lru.insert(tick, *id);
                let c = slot.cached.clone();
                g.hits += 1;
                Some(c)
            }
            None => {
                g.misses += 1;
                None
            }
        }
    }

    /// Keeps `cached` under `id`, evicting the oldest nodes to fit. A node
    /// that is larger than the cap is not kept.
    pub(crate) fn insert(&self, id: ObjectId, cached: Cached) {
        let charge = cached.bytes + ENTRY_OVERHEAD * cached.node.len() + NODE_OVERHEAD;
        if charge > self.cap {
            return;
        }
        let mut g = self.inner.lock();
        let g = &mut *g;
        g.tick += 1;
        let tick = g.tick;
        if let Some(slot) = g.map.get_mut(&id) {
            g.lru.remove(&slot.tick);
            slot.tick = tick;
            g.lru.insert(tick, id);
            return;
        }
        while g.bytes + charge > self.cap {
            let Some((_, old)) = g.lru.pop_first() else { break };
            if let Some(slot) = g.map.remove(&old) {
                g.bytes -= slot.charge;
            }
        }
        g.bytes += charge;
        g.lru.insert(tick, id);
        g.map.insert(id, Slot { cached, tick, charge });
    }
}

/// What a read or an `apply` needs: the store, and a node cache if there
/// is one. `Copy`, so it passes down freely.
#[derive(Clone, Copy)]
pub struct Ctx<'s> {
    pub(crate) store: &'s ChunkStore,
    pub(crate) cache: Option<&'s NodeCache>,
}

impl<'s> Ctx<'s> {
    /// A context with an optional cache.
    pub fn new(store: &'s ChunkStore, cache: Option<&'s NodeCache>) -> Self {
        Self { store, cache }
    }

    /// A context without a cache: every load reads the store.
    pub fn uncached(store: &'s ChunkStore) -> Self {
        Self { store, cache: None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{apply_in, build, get_in, iter_in, Config, Edit};
    use chunk_store::mem::MemIo;
    use chunk_store::Options;

    fn mem_store() -> ChunkStore {
        ChunkStore::open_with(Arc::new(MemIo::new()), Options { zstd_level: 0, ..Options::default() }).unwrap()
    }

    fn entries(n: usize, stride: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
        (0..n)
            .map(|i| {
                let k = ((i * stride + 3) as u64).to_be_bytes().to_vec();
                let mut v = vec![0x1e];
                v.extend_from_slice(&blake3::hash(&k).as_bytes()[..32]);
                (k, v)
            })
            .collect()
    }

    fn xorshift(s: &mut u64) -> u64 {
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        *s
    }

    #[test]
    fn second_get_hits() {
        let s = mem_store();
        let es = entries(5_000, 3);
        let (root, _) = build(&s, Config::DEFAULT, es.iter().map(|(k, v)| (k, v))).unwrap();
        let cache = NodeCache::new(1 << 20);
        let ctx = Ctx::new(&s, Some(&cache));
        let (k, v) = &es[1_234];
        assert_eq!(get_in(&ctx, &root, k).unwrap().as_ref(), Some(v));
        let first = cache.stats();
        assert_eq!((first.hits, first.misses > 0), (0, true));
        assert_eq!(get_in(&ctx, &root, k).unwrap().as_ref(), Some(v));
        let second = cache.stats();
        assert!(second.hits > 0, "{second:?}");
        assert_eq!(second.misses, first.misses, "the second get reads nothing");
    }

    #[test]
    fn bytes_stay_under_the_cap() {
        let s = mem_store();
        let es = entries(5_000, 3);
        let (root, _) = build(&s, Config::SMALL, es.iter().map(|(k, v)| (k, v))).unwrap();
        let cache = NodeCache::new(64 * 1024);
        let ctx = Ctx::new(&s, Some(&cache));
        let mut rng = 0x1234_5678_9abc_def1u64;
        for _ in 0..2_000 {
            let (k, v) = &es[(xorshift(&mut rng) % 5_000) as usize];
            assert_eq!(get_in(&ctx, &root, k).unwrap().as_ref(), Some(v));
            assert!(cache.stats().bytes <= 64 * 1024);
        }
        let st = cache.stats();
        assert!(st.entries > 0 && st.hits > 0, "{st:?}");
    }

    #[test]
    fn cap_zero_keeps_nothing() {
        let s = mem_store();
        let es = entries(2_000, 3);
        let (root, _) = build(&s, Config::SMALL, es.iter().map(|(k, v)| (k, v))).unwrap();
        let cache = NodeCache::new(0);
        let ctx = Ctx::new(&s, Some(&cache));
        for (k, v) in es.iter().step_by(37) {
            assert_eq!(get_in(&ctx, &root, k).unwrap().as_ref(), Some(v));
        }
        let st = cache.stats();
        assert_eq!((st.entries, st.bytes, st.hits), (0, 0, 0));
    }

    #[test]
    fn clear_empties_it() {
        let s = mem_store();
        let es = entries(2_000, 3);
        let (root, _) = build(&s, Config::SMALL, es.iter().map(|(k, v)| (k, v))).unwrap();
        let cache = NodeCache::new(1 << 20);
        let ctx = Ctx::new(&s, Some(&cache));
        get_in(&ctx, &root, &es[5].0).unwrap();
        assert!(cache.stats().entries > 0);
        cache.clear();
        let st = cache.stats();
        assert_eq!((st.entries, st.bytes), (0, 0));
        assert_eq!(get_in(&ctx, &root, &es[5].0).unwrap().as_ref(), Some(&es[5].1));
    }

    #[test]
    fn cached_equals_uncached() {
        let s = mem_store();
        let es = entries(5_000, 3);
        let (root, _) = build(&s, Config::SMALL, es.iter().map(|(k, v)| (k, v))).unwrap();
        let cache = NodeCache::new(32 * 1024);
        let cached = Ctx::new(&s, Some(&cache));
        let plain = Ctx::uncached(&s);
        for (k, v) in es.iter().step_by(11) {
            assert_eq!(get_in(&cached, &root, k).unwrap().as_ref(), Some(v));
        }
        let a: Vec<_> = iter_in(&cached, &root).unwrap().collect::<crate::Result<_>>().unwrap();
        let b: Vec<_> = iter_in(&plain, &root).unwrap().collect::<crate::Result<_>>().unwrap();
        assert_eq!(a, b);
        assert_eq!(a, es);
        // 200 edits: puts of new keys, replacements and deletes.
        let mut batch = Vec::new();
        for i in 0..200usize {
            let k = ((i * 75 + 4) as u64).to_be_bytes().to_vec();
            batch.push(match i % 3 {
                0 => Edit::Put { key: k, value: vec![i as u8; 5] },
                1 => Edit::Delete { key: es[i * 20].0.clone() },
                _ => Edit::Put { key: es[i * 20].0.clone(), value: vec![9; 7] },
            });
        }
        batch.sort_by(|x, y| x.key().cmp(y.key()));
        batch.dedup_by(|x, y| x.key() == y.key());
        let (r1, st1) = apply_in(&cached, Config::SMALL, &root, &batch).unwrap();
        let (r2, st2) = apply_in(&plain, Config::SMALL, &root, &batch).unwrap();
        assert_eq!(r1, r2);
        assert_eq!(st1.nodes_read, st2.nodes_read, "nodes_read counts hits too");
        assert_eq!(st2.cache_hits, 0);
        assert!(cache.stats().bytes <= 32 * 1024);
    }

    #[test]
    fn warm_apply_counts_hits_and_same_reads() {
        let s = mem_store();
        let es = entries(5_000, 3);
        let (root, _) = build(&s, Config::SMALL, es.iter().map(|(k, v)| (k, v))).unwrap();
        let cache = NodeCache::new(4 << 20);
        let ctx = Ctx::new(&s, Some(&cache));
        let batch = [Edit::Put { key: es[2_500].0.clone(), value: b"new".to_vec() }];
        let (_, cold) = apply_in(&ctx, Config::SMALL, &root, &batch).unwrap();
        let (_, warm) = apply_in(&ctx, Config::SMALL, &root, &batch).unwrap();
        // A cold apply can hit too: each level pass reads the upper path again.
        assert!(cold.cache_hits < cold.nodes_read, "{cold:?}");
        assert!(warm.cache_hits > cold.cache_hits, "{warm:?} vs {cold:?}");
        assert_eq!(cold.nodes_read, warm.nodes_read);
        assert_eq!(warm.cache_hits, warm.nodes_read, "everything was cached");
    }

    #[test]
    fn a_hit_is_checked_against_the_config_of_the_apply() {
        // A node a reader accepted (Config::READ limits) is still refused by
        // an apply of a smaller config, hit or not.
        let s = mem_store();
        let (root, _) = build(&s, Config::DEFAULT, [(b"k".to_vec(), vec![7; 300])]).unwrap();
        let cache = NodeCache::new(1 << 20);
        let ctx = Ctx::new(&s, Some(&cache));
        assert_eq!(get_in(&ctx, &root, b"k").unwrap(), Some(vec![7; 300]));
        assert_eq!(cache.stats().entries, 1);
        let batch = [Edit::Put { key: b"a".to_vec(), value: b"1".to_vec() }];
        let got = apply_in(&ctx, Config::SMALL, &root, &batch);
        assert!(matches!(got, Err(crate::Error::Corrupt(_))), "{:?}", got.map(|_| ()));
        assert!(apply_in(&ctx, Config::DEFAULT, &root, &batch).is_ok());
    }

    #[test]
    fn a_bad_node_is_not_cached() {
        use crate::node::{encode_internal, Child};
        let s = mem_store();
        let leaf = s.put(&crate::node::encode_leaf(&[(b"a".to_vec(), b"1".to_vec())])).unwrap();
        // The parent entry has the wrong last key, so the leaf fails the check.
        let root = s.put(&encode_internal(1, &[Child { last_key: b"q".to_vec(), id: leaf, count: 1 }])).unwrap();
        let cache = NodeCache::new(1 << 20);
        let ctx = Ctx::new(&s, Some(&cache));
        let got = get_in(&ctx, &root, b"a");
        assert!(matches!(got, Err(crate::Error::Corrupt(_))), "{got:?}");
        // The wrong-level case: the leaf as a level-2 child is refused before insert.
        let wrong = s.put(&encode_internal(2, &[Child { last_key: b"a".to_vec(), id: leaf, count: 1 }])).unwrap();
        let before = cache.stats().entries;
        let got = get_in(&ctx, &wrong, b"a");
        assert!(matches!(got, Err(crate::Error::Corrupt(_))), "{got:?}");
        assert_eq!(cache.stats().entries, before + 1, "only the root of `wrong` (it decoded) was kept");
    }
}
