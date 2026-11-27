//! The GC mark and the touched set, in bounded RAM (#113).
//!
//! The mark is a [`LiveSet`]: `gc_finish` only asks it "may this object be
//! live?". A [`LiveFilter`] answers from a Bloom filter of the live IDs (10
//! bits, 1.25 bytes, for each object of the store) instead of a set of
//! 33-byte IDs. A false positive only keeps a dead object (about 1 % of the
//! dead objects at 10 bits for each key, more in packs that are punched,
//! because a hole needs a whole block of dead records); a live object is
//! never removed, because a Bloom filter has no false negatives.
//!
//! Each filter has its own random salt, mixed into the probe positions
//! (see [`Salt`]). Without it the probes of an ID would be the same in
//! every GC, so a dead object that is a false positive once would be one
//! in every later GC and never go (a space leak). With it, a false
//! positive of one GC is a false positive of the next one only by chance.
//!
//! A mark must never use the filter to skip a part of its walk ("seen
//! already"): a false positive would then lose the live objects under it.

use std::collections::HashSet;

use crate::bloom::Bloom;
use crate::extsort::Sorter;
use crate::{ObjectId, DIGEST_LEN, ID_LEN};

/// A per-filter salt. The Bloom filter takes its probe positions from the
/// digest bytes 0..24 (see `bloom.rs`); a salted filter gives it these
/// bytes after a mix with the salt (each 64-bit word: XOR with its own
/// salt word, then the SplitMix64 finalizer). The digests are BLAKE3, so
/// the mixed words stay uniform; another salt gives other positions.
#[derive(Clone, Copy)]
struct Salt([u64; 3]);

impl Salt {
    fn new(seed: u64) -> Salt {
        let mut x = seed;
        Salt([0, 1, 2].map(|_| {
            x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
            mix(x)
        }))
    }

    /// A new random salt (from the random keys of the standard hasher).
    fn random() -> Salt {
        use std::hash::{BuildHasher, Hasher};
        Salt::new(std::collections::hash_map::RandomState::new().build_hasher().finish())
    }

    /// The bytes that the filter probes with, for `id`.
    #[inline]
    fn key(&self, id: &ObjectId) -> [u8; DIGEST_LEN] {
        let d = id.digest();
        let mut k = *d;
        for (i, s) in self.0.iter().enumerate() {
            let w = u64::from_le_bytes(d[i * 8..i * 8 + 8].try_into().unwrap());
            k[i * 8..i * 8 + 8].copy_from_slice(&mix(w ^ s).to_le_bytes());
        }
        k
    }
}

/// The SplitMix64 finalizer (a bijection of u64).
#[inline]
fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

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
    salt: Salt,
    inserted: u64,
}

impl LiveFilter {
    /// A filter for at most `n` live objects at [`LIVE_FILTER_BITS`] bits
    /// each. More keys than `n` only raise the false-positive rate.
    pub fn new(n: u64) -> LiveFilter {
        Self::with_bits_per_key(n, LIVE_FILTER_BITS)
    }

    /// A filter for `n` keys at `bits_per_key` (at least 1; a tuning and
    /// test knob), with a new random salt.
    pub fn with_bits_per_key(n: u64, bits_per_key: u8) -> LiveFilter {
        Self::with_salt(n, bits_per_key, Salt::random())
    }

    /// As [`LiveFilter::with_bits_per_key`] with the salt from `seed`
    /// (tests: a fixed salt gives the same false positives again).
    pub fn with_seed(n: u64, bits_per_key: u8, seed: u64) -> LiveFilter {
        Self::with_salt(n, bits_per_key, Salt::new(seed))
    }

    fn with_salt(n: u64, bits_per_key: u8, salt: Salt) -> LiveFilter {
        LiveFilter { bloom: Bloom::new(n, bits_per_key.max(1)), salt, inserted: 0 }
    }

    pub fn insert(&mut self, id: &ObjectId) {
        self.bloom.insert(&self.salt.key(id));
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
        self.bloom.may_contain(&self.salt.key(id))
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
///
/// A full GC (`gc_begin_full`) records the IDs before `gc_finish_full` in an
/// external sorter instead (temp files, bounded RAM, exact).
pub(crate) struct Touched {
    ids: HashSet<ObjectId>,
    filter: Option<(Bloom, Salt)>,
    spill_at: usize,
    disk: Option<Sorter<ID_LEN>>,
    /// A write of `disk` failed: the set is not complete.
    disk_err: Option<String>,
}

impl Touched {
    /// A set that moves into a filter after `spill_at` IDs.
    pub fn spilling(spill_at: usize) -> Touched {
        Touched { ids: HashSet::new(), filter: None, spill_at: spill_at.max(1), disk: None, disk_err: None }
    }

    /// A set in an external sorter (a full GC).
    pub fn on_disk(sorter: Sorter<ID_LEN>) -> Touched {
        Touched { ids: HashSet::new(), filter: None, spill_at: usize::MAX, disk: Some(sorter), disk_err: None }
    }

    /// The sorter of a set made with [`Touched::on_disk`], or the exact IDs
    /// of a set that never spilled. Err if the set is not exact (a Bloom
    /// filter, or a failed write of the sorter).
    pub fn into_exact(self) -> Result<Result<Sorter<ID_LEN>, HashSet<ObjectId>>, String> {
        if let Some(e) = self.disk_err {
            return Err(format!("the touched set of the GC could not be written: {e}"));
        }
        if self.filter.is_some() {
            return Err("the touched set of the GC is a Bloom filter (start a full GC with gc_begin_full)".into());
        }
        Ok(match self.disk {
            Some(s) => Ok(s),
            None => Err(self.ids),
        })
    }

    /// A set that stays exact.
    pub fn exact() -> Touched {
        Touched { ids: HashSet::new(), filter: None, spill_at: usize::MAX, disk: None, disk_err: None }
    }

    /// Record `id`. `objects` (the store's object count) sizes the filter
    /// of a spill.
    pub fn insert(&mut self, id: ObjectId, objects: u64) {
        if let Some(s) = self.disk.as_mut() {
            if self.disk_err.is_none() {
                if let Err(e) = s.push(id.0) {
                    self.disk_err = Some(e.to_string());
                }
            }
            return;
        }
        if let Some((f, salt)) = self.filter.as_mut() {
            f.insert(&salt.key(&id));
            return;
        }
        self.ids.insert(id);
        if self.ids.len() >= self.spill_at {
            let n = objects.max(4 * self.ids.len() as u64);
            let mut f = Bloom::new(n, LIVE_FILTER_BITS);
            let salt = Salt::random();
            for id in self.ids.drain() {
                f.insert(&salt.key(&id));
            }
            self.ids = HashSet::new();
            self.filter = Some((f, salt));
        }
    }

    /// (A set on disk answers true: only a full GC reads it, as a stream.)
    pub fn contains(&self, id: &ObjectId) -> bool {
        self.disk.is_some()
            || self.ids.contains(id) || self.filter.as_ref().is_some_and(|(f, salt)| f.may_contain(&salt.key(id)))
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
    #[cfg_attr(miri, ignore = "too slow under Miri (over 300 s in the probe)")]
    fn a_live_filter_has_no_false_negatives() {
        let mut f = LiveFilter::new(10_000);
        f.extend((0..10_000).map(id));
        assert!((0..10_000).all(|i| f.may_contain(&id(i))));
        assert_eq!(f.bytes(), 12_544);
    }

    /// The false positives of a filter, among 100,000 non-keys.
    fn false_positives(f: &LiveFilter) -> HashSet<u64> {
        (10_000..110_000).filter(|&i| f.may_contain(&id(i))).collect()
    }

    #[test]
    #[cfg_attr(miri, ignore = "too slow under Miri (over 300 s in the probe)")]
    fn another_salt_gives_other_false_positives() {
        let make = |seed: Option<u64>| {
            let mut f = match seed {
                Some(s) => LiveFilter::with_seed(10_000, 10, s),
                None => LiveFilter::new(10_000),
            };
            f.extend((0..10_000).map(id));
            assert!((0..10_000).all(|i| f.may_contain(&id(i))));
            false_positives(&f)
        };
        let (a, a2, b, r1, r2) = (make(Some(1)), make(Some(1)), make(Some(2)), make(None), make(None));
        assert!((500..=2000).contains(&a.len()), "{} false positives", a.len());
        assert_eq!(a, a2, "one seed, the same false positives");
        // Independent filters at about 1 %: about 1 % of them in common.
        for (x, y) in [(&a, &b), (&r1, &r2)] {
            let both = x.intersection(y).count();
            assert!(both * 10 < x.len(), "{both} of {} false positives repeat", x.len());
        }
    }
}
