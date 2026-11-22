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
//! lose the object. So the cache takes the entries of an *exact* `.idx`
//! only: one where every listed object has its current index entry at the
//! listed place. A bit in the header of the `.idx` (`IDX_EXACT`) says so on
//! disk, so a later process can trust the packs of an earlier one:
//!
//! - The seal writes the `.idx` of a new pack as exact: it lists the records
//!   of the pack, each one the current entry of its ID.
//! - Before a GC changes anything of a group of packs (its copies, its
//!   tombstones), it writes the `.idx` of each of them again without the
//!   bit, durably (`ChunkStore::gc_distrust`), and calls
//!   [`LocalityCache::untrust`]. A crash after that leaves the bit clear;
//!   a crash before it changed nothing. The GC also calls `untrust` again
//!   under the store lock before its install.
//! - The punch of a GC writes the new `.idx` of the pack as exact: it lists
//!   only the entries that the index has at that place after the install.
//! - Every other `.idx` writer (the scans at open) writes it as not exact.
//!
//! A failed index lookup in `put` writes the object again, so the pack of
//! the older copy keeps it in its `.idx` although a later GC that removes
//! the new copy does not select that pack. Before that write, the store
//! moves the *trust floor* (the `TRUST` file, durable) to the next pack
//! number: no pack below the floor is trusted, in this process
//! ([`LocalityCache::forget_below`]) or a later one. The put fails if the
//! floor cannot be written.
//!
//! A fill reads the `.idx` outside every lock. It records the epoch first;
//! `untrust` moves the epoch, so a fill that overlaps a GC is dropped.
//!
//! ## RAM
//!
//! At most [`MAX_PACKS`] packs and [`MAX_ENTRIES`] entries in all (a pack
//! with more entries is never cached; the store checks the length of a
//! `.idx` before it reads it, see [`max_idx_len`]). The set of packs that
//! are never filled again has one number per refused pack. A table slot is an ID and a `Loc`
//! (72 bytes) and a control byte, so the table is at most about 9.1 MiB
//! (131,072 slots); a fill holds one `.idx` file and its decoded entries
//! (at most `MAX_ENTRIES`) for a moment.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::sync::{Mutex, MutexGuard, Rank};
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
    /// Moves at each `untrust` and `forget_below`: a fill that started
    /// before is dropped.
    epoch: u64,
    /// No pack below this number is filled (see the module doc).
    floor: u64,
    /// No pack at or above this number is filled: it has no `.idx` yet
    /// (the active pack). `None`: not told (tests of the cache alone).
    unsealed: Option<u64>,
    /// Packs that are never filled again in this process: a GC selected
    /// them, or their `.idx` was not exact, damaged or too big.
    never: HashSet<u64>,
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
pub(crate) struct LocalityCache {
    state: Mutex<State>,
    hits: AtomicU64,
    misses: AtomicU64,
    fills: AtomicU64,
    fills_dropped: AtomicU64,
}

/// The longest `.idx` that a fill reads: one with [`MAX_ENTRIES`] entries
/// of `entry` bytes, a header of `header` bytes and a CRC.
pub(crate) const fn max_idx_len(header: usize, entry: usize) -> u64 {
    (header + MAX_ENTRIES * entry + 4) as u64
}

impl Default for LocalityCache {
    fn default() -> Self {
        LocalityCache {
            state: Mutex::new(Rank::Store, State::default()),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            fills: AtomicU64::new(0),
            fills_dropped: AtomicU64::new(0),
        }
    }
}

impl LocalityCache {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock()
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
    /// it must call [`LocalityCache::fill`], [`LocalityCache::refuse`] or
    /// [`LocalityCache::fill_failed`]).
    pub(crate) fn index_hit(&self, pack: u64) -> Option<u64> {
        let mut st = self.state();
        st.streak = if st.streak.0 == pack { (pack, st.streak.1.saturating_add(1)) } else { (pack, 1) };
        if st.streak.1 < FILL_STREAK
            || st.filling.is_some()
            || pack < st.floor
            || st.unsealed.is_some_and(|u| pack >= u)
            || st.never.contains(&pack)
            || st.lru.iter().any(|p| p.0 == pack)
        {
            return None;
        }
        st.filling = Some(pack);
        Some(st.epoch)
    }

    /// The entries of the exact `.idx` of `pack`, read after
    /// [`LocalityCache::index_hit`] gave `epoch`.
    pub(crate) fn fill(&self, pack: u64, epoch: u64, entries: Vec<(ObjectId, Loc)>) {
        let mut st = self.state();
        st.filling = None;
        let n = entries.len();
        let fits = n <= MAX_ENTRIES && entries.iter().all(|(_, l)| l.pack == pack);
        if !fits {
            st.never.insert(pack);
        }
        let ok = fits
            && st.epoch == epoch
            && pack >= st.floor
            && !st.never.contains(&pack)
            && !st.lru.iter().any(|p| p.0 == pack);
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

    /// The `.idx` of `pack` is not exact, damaged or too big: this process
    /// never reads it again.
    pub(crate) fn refuse(&self, pack: u64) {
        let mut st = self.state();
        st.filling = None;
        st.never.insert(pack);
        drop(st);
        self.fills_dropped.fetch_add(1, Ordering::Relaxed);
    }

    /// A fill that could not read the `.idx` (a later one may: a seal that
    /// has not written it yet, a transient error).
    pub(crate) fn fill_failed(&self) {
        let mut st = self.state();
        st.filling = None;
        // A new streak before the next try.
        st.streak = (0, 0);
        drop(st);
        self.fills_dropped.fetch_add(1, Ordering::Relaxed);
    }

    /// A GC is about to change (or delete) `packs`: never cache them again,
    /// drop what is cached of them, and drop the fills that run now.
    pub(crate) fn untrust(&self, packs: impl IntoIterator<Item = u64>) {
        let mut st = self.state();
        st.epoch += 1;
        let gone: HashSet<u64> = packs.into_iter().collect();
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
        st.never.extend(gone);
    }

    /// No pack below `floor` is trusted (the trust floor of the store, at
    /// open or after a failed lookup in `put`, see the module doc). Empties
    /// the cache and drops the fills that run now.
    pub(crate) fn forget_below(&self, floor: u64) {
        let mut st = self.state();
        st.epoch += 1;
        st.floor = st.floor.max(floor);
        st.lru.clear();
        st.map = IdMap::default();
        st.total = 0;
    }

    /// Every pack below `num` is sealed: its `.idx` is written (or is
    /// being written by the seal).
    pub(crate) fn sealed_below(&self, num: u64) {
        let mut st = self.state();
        st.unsealed = Some(st.unsealed.map_or(num, |u| u.max(num)));
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
    fn a_streak_of_hits_fills_the_pack() {
        let c = LocalityCache::default();
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
    fn a_refused_untrusted_or_low_pack_is_never_filled() {
        let c = LocalityCache::default();
        streak(&c, 1).unwrap();
        c.refuse(1);
        assert_eq!(streak(&c, 1), None, "refused (not exact)");
        let e = streak(&c, 2).unwrap();
        c.fill(2, e, entries(2, MAX_ENTRIES + 1));
        assert_eq!(streak(&c, 2), None, "too many entries");
        c.untrust([3]);
        assert_eq!(streak(&c, 3), None, "selected by a GC");
        c.forget_below(10);
        assert_eq!(streak(&c, 9), None, "below the floor");
        assert!(streak(&c, 10).is_some());
    }

    #[test]
    fn a_gc_during_a_fill_drops_it_and_the_cached_pack() {
        let c = LocalityCache::default();
        let e = streak(&c, 1).unwrap();
        c.fill(1, e, entries(1, 10));
        let e = streak(&c, 2).unwrap();
        c.untrust([1]); // a GC selects pack 1 while pack 2 fills
        c.fill(2, e, entries(2, 10));
        let s = c.stats();
        assert_eq!((s.entries, s.packs, s.fills_dropped), (0, 0, 1));
        assert!(entries(1, 10).iter().all(|(i, _)| c.get(i).is_none()));
        // Pack 2 was not selected: the next streak fills it.
        let e = streak(&c, 2).unwrap();
        c.fill(2, e, entries(2, 10));
        assert_eq!(c.stats().entries, 10);
    }

    #[test]
    fn a_floor_during_a_fill_drops_it_and_empties_the_cache() {
        let c = LocalityCache::default();
        let e = streak(&c, 5).unwrap();
        c.fill(5, e, entries(5, 10));
        let e = streak(&c, 7).unwrap();
        c.forget_below(3);
        c.fill(7, e, entries(7, 10));
        assert_eq!(c.stats().entries, 0);
    }

    #[test]
    fn a_fill_with_entries_of_another_pack_is_dropped() {
        let c = LocalityCache::default();
        let e = streak(&c, 1).unwrap();
        c.fill(1, e, entries(2, 9));
        assert_eq!(c.stats().entries, 0);
    }

    #[test]
    fn the_ram_stays_within_the_bounds() {
        let c = LocalityCache::default();
        let per = MAX_ENTRIES / 4 + 1;
        for p in 1..=40u64 {
            let n = if p % 2 == 0 { per } else { 100 };
            let e = streak(&c, p).unwrap();
            c.fill(p, e, entries(p, n));
            let s = c.stats();
            assert!(s.entries as usize <= MAX_ENTRIES && s.packs as usize <= MAX_PACKS, "{s:?}");
            // The newest pack is always in.
            assert!(c.get(&entries(p, 1)[0].0).is_some());
        }
    }
}
