//! The GC mark and the touched set, in bounded RAM (#113).
//!
//! The mark is a [`LiveSet`]: `gc_finish` only asks it "may this object be
//! live?". A [`LiveFilter`] answers from a Bloom filter of the live IDs (10
//! bits, 1.25 bytes, for each object of the store) instead of a set of
//! 33-byte IDs. A false positive only keeps a dead object (about 1 % of the
//! dead objects at 10 bits for each key, more in packs that are punched,
//! because a hole needs a whole block of dead records); a live object is
//! never removed, because a Bloom filter has no false negatives. A later GC
//! can remove the dead objects that stay: the filter of each GC is new.
//!
//! A mark must never use the filter to skip a part of its walk ("seen
//! already"): a false positive would then lose the live objects under it.

use std::collections::HashSet;

use crate::bloom::Bloom;
use crate::ObjectId;

/// Bits for each key of a [`LiveFilter`] and of a spilled touched set
/// (about 1 % false positives).
pub const LIVE_FILTER_BITS: u8 = 10;

/// The objects that a GC keeps (the mark). `may_contain` may say true for
/// an object that is not in the set, never false for one that is.
pub trait LiveSet: Sync {
    fn may_contain(&self, id: &ObjectId) -> bool;
}

/// An exact mark (tests and small tools): RAM grows with the live objects.
impl LiveSet for HashSet<ObjectId> {
    fn may_contain(&self, id: &ObjectId) -> bool {
        self.contains(id)
    }
}

/// A mark as a Bloom filter of the live IDs: fixed RAM, sized when it is
/// made. Only the digest is a key (see `bloom.rs`).
pub struct LiveFilter {
    bloom: Bloom,
    inserted: u64,
}

impl LiveFilter {
    /// A filter for at most `n` live objects at [`LIVE_FILTER_BITS`] bits
    /// each. More keys than `n` only raise the false-positive rate.
    pub fn new(n: u64) -> LiveFilter {
        Self::with_bits_per_key(n, LIVE_FILTER_BITS)
    }

    /// A filter for `n` keys at `bits_per_key` (at least 1; a tuning and
    /// test knob).
    pub fn with_bits_per_key(n: u64, bits_per_key: u8) -> LiveFilter {
        LiveFilter { bloom: Bloom::new(n, bits_per_key.max(1)), inserted: 0 }
    }

    pub fn insert(&mut self, id: &ObjectId) {
        self.bloom.insert(id.digest());
        self.inserted += 1;
    }

    /// The inserts so far (an ID inserted twice counts twice).
    pub fn inserted(&self) -> u64 {
        self.inserted
    }

    /// No insert so far: the filter keeps nothing.
    pub fn is_empty(&self) -> bool {
        self.inserted == 0
    }

    /// Bytes of the filter.
    pub fn bytes(&self) -> usize {
        self.bloom.len()
    }
}

impl Extend<ObjectId> for LiveFilter {
    fn extend<I: IntoIterator<Item = ObjectId>>(&mut self, ids: I) {
        for id in ids {
            self.insert(&id);
        }
    }
}

impl LiveSet for LiveFilter {
    fn may_contain(&self, id: &ObjectId) -> bool {
        self.bloom.may_contain(id.digest())
    }
}

/// The objects put or read while a GC runs.
///
/// Before `gc_finish` (while the caller marks, which also reads every node
/// it walks) the set is only asked "was this object touched?", so after
/// `spill_at` exact IDs it moves into a Bloom filter (a false positive only
/// keeps a dead object). During `gc_finish` the set must list its IDs (each
/// one is looked up to find its pack), so it stays exact: its size is the
/// distinct objects put or read during `gc_finish`, not the store size.
pub(crate) struct Touched {
    ids: HashSet<ObjectId>,
    filter: Option<Bloom>,
    spill_at: usize,
}

impl Touched {
    /// A set that moves into a filter after `spill_at` IDs.
    pub fn spilling(spill_at: usize) -> Touched {
        Touched { ids: HashSet::new(), filter: None, spill_at: spill_at.max(1) }
    }

    /// A set that stays exact.
    pub fn exact() -> Touched {
        Touched { ids: HashSet::new(), filter: None, spill_at: usize::MAX }
    }

    /// Record `id`. `objects` (the store's object count) sizes the filter
    /// of a spill.
    pub fn insert(&mut self, id: ObjectId, objects: u64) {
        if let Some(f) = self.filter.as_mut() {
            f.insert(id.digest());
            return;
        }
        self.ids.insert(id);
        if self.ids.len() >= self.spill_at {
            let n = objects.max(4 * self.ids.len() as u64);
            let mut f = Bloom::new(n, LIVE_FILTER_BITS);
            for id in self.ids.drain() {
                f.insert(id.digest());
            }
            self.ids = HashSet::new();
            self.filter = Some(f);
        }
    }

    pub fn contains(&self, id: &ObjectId) -> bool {
        self.ids.contains(id) || self.filter.as_ref().is_some_and(|f| f.may_contain(id.digest()))
    }

    /// The exact IDs. Only for a set made with [`Touched::exact`], which
    /// never spills.
    pub fn ids(&self) -> &HashSet<ObjectId> {
        debug_assert!(self.filter.is_none());
        &self.ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(i: u64) -> ObjectId {
        ObjectId::new(1, *blake3::hash(&i.to_le_bytes()).as_bytes())
    }

    #[test]
    fn a_spilled_touched_set_has_no_false_negatives() {
        let mut t = Touched::spilling(8);
        for i in 0..1000 {
            t.insert(id(i), 100);
        }
        assert!(t.filter.is_some() && t.ids.is_empty());
        assert!((0..1000).all(|i| t.contains(&id(i))));
    }

    #[test]
    fn an_exact_touched_set_never_spills() {
        let mut t = Touched::exact();
        for i in 0..1000 {
            t.insert(id(i), 100);
        }
        assert_eq!(t.ids().len(), 1000);
        assert!(!t.contains(&id(5000)));
    }

    #[test]
    fn a_live_filter_has_no_false_negatives() {
        let mut f = LiveFilter::new(10_000);
        f.extend((0..10_000).map(id));
        assert!((0..10_000).all(|i| f.may_contain(&id(i))));
        assert_eq!(f.bytes(), 12_544);
    }
}
