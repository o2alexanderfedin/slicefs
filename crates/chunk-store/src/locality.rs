//! The locality cache of `put` (#112): the `.idx` entries of a few packs that
//! recent dedup hits found, so the next dedup checks of the same file are
//! answered from RAM, without a run page read.
//!
//! The runs of the index are sorted by digest, so the chunks of one file are
//! spread over all their pages. A file that is written again often has its
//! chunks in one pack, in the same order as before, and that pack's `.idx`
//! holds their entries together. After [`FILL_STREAK`] index hits in a row
//! in one pack, `put` reads the `.idx` of that pack and gives its entries to
//! the cache. A streak, not one hit: a single dedup hit (a file that shares
//! one chunk) must not cost a read of a whole `.idx`.
//!
//! ## Only exact `.idx` files
//!
//! A `.idx` is not the index. A GC writes the tombstones of its victims
//! before it changes their packs, and it can leave the `.idx` of a pack as it
//! was: a punch that is skipped (no whole block to punch, a read error), a
//! pack removal that fails, a crash between the install and the change. That
//! `.idx` then lists removed objects, and a dedup hit on one of them would
//! lose the object. So the cache takes the entries of a *trusted* pack only:
//! a pack that this process sealed (its `.idx` is the list of its records,
//! all with a current index entry or a newer copy), and that no GC install
//! has selected since. The GC calls [`LocalityCache::untrust`] under the
//! store lock, before its install: the packs that it changes (or deletes)
//! leave the trusted set and the cache for the rest of the process. Packs of
//! an earlier process are never trusted (no trace on disk says whether a GC
//! left their `.idx` stale).
//!
//! A fill reads the `.idx` outside every lock. It records the epoch first;
//! `untrust` moves the epoch, so a fill that overlaps a GC install is
//! dropped.
//!
//! ## RAM
//!
//! At most [`MAX_PACKS`] packs and [`MAX_ENTRIES`] entries in all (a pack
//! with more entries is never cached). A table slot is an ID and a `Loc`
//! (72 bytes) and a control byte, so the table is at most about 9.1 MiB
//! (131,072 slots); a fill holds one `.idx` file and its decoded entries
//! (at most `MAX_ENTRIES`) for a moment.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::{Loc, ObjectId};

/// The packs in the cache (least recently used goes first).
pub(crate) const MAX_PACKS: usize = 16;
/// The entries in the cache, all packs together; a pack with more entries
/// is never cached.
pub(crate) const MAX_ENTRIES: usize = 1 << 16;
/// Index hits in a row in one pack before its `.idx` is read.
pub(crate) const FILL_STREAK: u32 = 4;

/// IDs are digests: the last 8 bytes are a good hash.
#[derive(Default)]
struct IdHasher(u64);

impl Hasher for IdHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, b: &[u8]) {
        if b.len() >= 8 {
            self.0 ^= u64::from_le_bytes(b[b.len() - 8..].try_into().unwrap());
        } else {
            for &x in b {
                self.0 = self.0.rotate_left(8) ^ u64::from(x);
            }
        }
    }
}

type IdMap = HashMap<ObjectId, Loc, BuildHasherDefault<IdHasher>>;

/// Counters of the cache (for tests and benchmarks).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LocalityStats {
    /// `put` dedup checks answered by the cache.
    pub hits: u64,
    /// `put` dedup checks that went to the index.
    pub misses: u64,
    /// `.idx` files read into the cache.
    pub fills: u64,
    /// Fills dropped (a GC install during the read, a damaged `.idx`, too
    /// many entries).
    pub fills_dropped: u64,
    /// Entries in the cache now.
    pub entries: u64,
    /// Packs in the cache now.
    pub packs: u64,
}

#[derive(Default)]
struct State {
    /// Moves at each `untrust`: a fill that started before is dropped.
    epoch: u64,
    /// Packs that this process sealed, with their entry counts, minus the
    /// packs that a GC selected since.
    trusted: HashMap<u64, usize>,
    /// The cached packs and their entry counts, the most recent first.
    lru: VecDeque<(u64, usize)>,
    map: IdMap,
    total: usize,
    /// The pack of the last index hits and their count in a row.
    streak: (u64, u32),
    /// A pack whose fill runs now (one at a time).
    filling: Option<u64>,
}

/// See the module doc.
#[derive(Default)]
pub(crate) struct LocalityCache {
    state: Mutex<State>,
    hits: AtomicU64,
    misses: AtomicU64,
    fills: AtomicU64,
    fills_dropped: AtomicU64,
}

impl LocalityCache {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The cached place of `id`, if a cached pack has it.
    pub(crate) fn get(&self, id: &ObjectId) -> Option<Loc> {
        let mut st = self.state();
        let found = st.map.get(id).copied();
        match found {
            Some(l) => {
                if st.lru.front().map(|p| p.0) != Some(l.pack) {
                    if let Some(i) = st.lru.iter().position(|p| p.0 == l.pack) {
                        let p = st.lru.remove(i).unwrap();
                        st.lru.push_front(p);
                    }
                }
                drop(st);
                self.hits.fetch_add(1, Ordering::Relaxed);
            }
            None => {
                drop(st);
                self.misses.fetch_add(1, Ordering::Relaxed);
            }
        }
        found
    }

    /// An index hit in `pack` (a cache miss before it). Returns the epoch
    /// for a fill of `pack` if the caller should read its `.idx` now (then
    /// it must call [`LocalityCache::fill`] or [`LocalityCache::fill_failed`]).
    pub(crate) fn index_hit(&self, pack: u64) -> Option<u64> {
        let mut st = self.state();
        st.streak = if st.streak.0 == pack { (pack, st.streak.1.saturating_add(1)) } else { (pack, 1) };
        if st.streak.1 < FILL_STREAK || st.filling.is_some() || st.lru.iter().any(|p| p.0 == pack) {
            return None;
        }
        match st.trusted.get(&pack) {
            Some(&n) if n <= MAX_ENTRIES => {
                st.filling = Some(pack);
                Some(st.epoch)
            }
            _ => None,
        }
    }

    /// The entries of the `.idx` of `pack`, read after
    /// [`LocalityCache::index_hit`] gave `epoch`.
    pub(crate) fn fill(&self, pack: u64, epoch: u64, entries: Vec<(ObjectId, Loc)>) {
        let mut st = self.state();
        st.filling = None;
        let n = entries.len();
        let ok = st.epoch == epoch
            && st.trusted.get(&pack) == Some(&n)
            && n <= MAX_ENTRIES
            && !st.lru.iter().any(|p| p.0 == pack)
            && entries.iter().all(|(_, l)| l.pack == pack);
        if !ok {
            drop(st);
            self.fills_dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        while !st.lru.is_empty() && (st.lru.len() >= MAX_PACKS || st.total + n > MAX_ENTRIES) {
            let (old, k) = st.lru.pop_back().unwrap();
            st.map.retain(|_, l| l.pack != old);
            st.total -= k;
        }
        st.map.reserve(n);
        st.map.extend(entries);
        st.total += n;
        st.lru.push_front((pack, n));
        drop(st);
        self.fills.fetch_add(1, Ordering::Relaxed);
    }

    /// A fill that could not read the `.idx`.
    pub(crate) fn fill_failed(&self) {
        self.state().filling = None;
        self.fills_dropped.fetch_add(1, Ordering::Relaxed);
    }

    /// This process sealed `pack` with `entries` records: its `.idx` is
    /// exact. The caller holds the store lock.
    pub(crate) fn trust(&self, pack: u64, entries: usize) {
        self.state().trusted.insert(pack, entries);
    }

    /// A GC is about to change (or delete) `packs`: never cache them again,
    /// drop what is cached of them, and drop the fills that run now. The
    /// caller holds the store lock and calls this before the install.
    pub(crate) fn untrust(&self, packs: impl IntoIterator<Item = u64>) {
        let mut st = self.state();
        st.epoch += 1;
        let gone: HashSet<u64> = packs.into_iter().collect();
        for p in &gone {
            st.trusted.remove(p);
        }
        if st.lru.iter().any(|p| gone.contains(&p.0)) {
            let mut dropped = 0;
            st.lru.retain(|p| {
                let out = gone.contains(&p.0);
                if out {
                    dropped += p.1;
                }
                !out
            });
            st.total -= dropped;
            st.map.retain(|_, l| !gone.contains(&l.pack));
        }
    }

    pub(crate) fn stats(&self) -> LocalityStats {
        let st = self.state();
        LocalityStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            fills: self.fills.load(Ordering::Relaxed),
            fills_dropped: self.fills_dropped.load(Ordering::Relaxed),
            entries: st.map.len() as u64,
            packs: st.lru.len() as u64,
        }
    }

    /// Every cached entry (for tests).
    #[cfg(test)]
    pub(crate) fn entries(&self) -> Vec<(ObjectId, Loc)> {
        self.state().map.iter().map(|(k, v)| (*k, *v)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(i: u64) -> ObjectId {
        let mut b = [0u8; crate::ID_LEN];
        b[1..9].copy_from_slice(&i.to_le_bytes());
        b[crate::ID_LEN - 8..].copy_from_slice(&i.wrapping_mul(0x9e37_79b9_7f4a_7c15).to_le_bytes());
        ObjectId(b)
    }

    fn entries(pack: u64, n: usize) -> Vec<(ObjectId, Loc)> {
        (0..n as u64)
            .map(|i| (id(pack << 32 | i), Loc { pack, offset: 16 + i * 64, stored: 8, raw: 8, flags: 0 }))
            .collect()
    }

    fn streak(c: &LocalityCache, pack: u64) -> Option<u64> {
        (0..FILL_STREAK).find_map(|_| c.index_hit(pack))
    }

    #[test]
    fn a_streak_of_hits_in_a_trusted_pack_fills_it() {
        let c = LocalityCache::default();
        c.trust(1, 10);
        for _ in 1..FILL_STREAK {
            assert_eq!(c.index_hit(1), None);
        }
        let e = c.index_hit(1).expect("a fill after the streak");
        let es = entries(1, 10);
        c.fill(1, e, es.clone());
        for (i, l) in &es {
            assert_eq!(c.get(i), Some(*l));
        }
        assert_eq!(c.get(&id(999_999)), None);
        let s = c.stats();
        assert_eq!((s.fills, s.entries, s.packs, s.hits, s.misses), (1, 10, 1, 10, 1));
    }

    #[test]
    fn an_untrusted_pack_is_never_filled() {
        let c = LocalityCache::default();
        assert_eq!(streak(&c, 1), None, "not sealed by this process");
        c.trust(2, MAX_ENTRIES + 1);
        assert_eq!(streak(&c, 2), None, "too many entries");
        c.trust(3, 10);
        c.untrust([3]);
        assert_eq!(streak(&c, 3), None, "selected by a GC");
    }

    #[test]
    fn a_gc_install_during_a_fill_drops_it_and_the_cached_pack() {
        let c = LocalityCache::default();
        c.trust(1, 10);
        c.trust(2, 10);
        let e = streak(&c, 1).unwrap();
        c.fill(1, e, entries(1, 10));
        let e = streak(&c, 2).unwrap();
        c.untrust([1]); // a GC install selects pack 1 while pack 2 fills
        c.fill(2, e, entries(2, 10));
        let s = c.stats();
        assert_eq!((s.entries, s.packs, s.fills_dropped), (0, 0, 1));
        assert!(entries(1, 10).iter().all(|(i, _)| c.get(i).is_none()));
        // Pack 2 is still trusted: the next streak fills it.
        let e = streak(&c, 2).unwrap();
        c.fill(2, e, entries(2, 10));
        assert_eq!(c.stats().entries, 10);
    }

    #[test]
    fn a_fill_with_other_entries_than_the_seal_is_dropped() {
        let c = LocalityCache::default();
        c.trust(1, 10);
        let e = streak(&c, 1).unwrap();
        c.fill(1, e, entries(1, 9));
        assert_eq!(c.stats().entries, 0);
    }

    #[test]
    fn the_ram_stays_within_the_bounds() {
        let c = LocalityCache::default();
        let per = MAX_ENTRIES / 4 + 1;
        for p in 1..=40u64 {
            let n = if p % 2 == 0 { per } else { 100 };
            c.trust(p, n);
            let e = streak(&c, p).unwrap();
            c.fill(p, e, entries(p, n));
            let s = c.stats();
            assert!(s.entries as usize <= MAX_ENTRIES && s.packs as usize <= MAX_PACKS, "{s:?}");
            // The newest pack is always in.
            assert!(c.get(&entries(p, 1)[0].0).is_some());
        }
    }
}
