//! Namespace locks of the file system core (Phase 19, D-04).
//!
//! [`NamespaceLocks`] is a fixed array of [`STRIPES`] stripe locks. An inode
//! number is hashed to a stripe ([`stripe`]). An operation on several inodes
//! locks the sorted, deduplicated set of their stripes in ascending stripe
//! order (two inodes in one stripe lock it once), which is the LOCK-03 order.
//! A cross-directory rename takes the rename mutex before any stripe.
//!
//! Ranks (`chunk_store::sync::Rank`): the rename mutex is `Rename` (2), a
//! stripe is `Stripe` (3) keyed by its index, so the debug checker refuses a
//! stripe taken below a held one. A stripe covers namespace metadata only
//! (directory entries, link counts, the open-count and orphan decisions,
//! setattr and xattr order); it is never held across content I/O, hashing of
//! file content or a splice.

use chunk_store::lock_stats::Site;
use chunk_store::sync::{Mutex, MutexGuard, Rank};

/// Number of stripes (a power of two; [`stripe`] takes the top 8 bits).
pub const STRIPES: usize = 256;

/// The stripe of inode `ino` (Fibonacci hashing of the inode number).
pub fn stripe(ino: u64) -> usize {
    (ino.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - 8)) as usize
}

/// One stripe, on its own cache line.
#[repr(align(64))]
struct Stripe(Mutex<()>);

/// The stripe array and the rename mutex.
pub struct NamespaceLocks {
    stripes: Box<[Stripe]>,
    rename: Mutex<()>,
}

/// The held rename mutex.
pub type RenameGuard<'a> = MutexGuard<'a, ()>;

/// Held namespace locks: the rename mutex (if taken) and stripes in
/// ascending order. Dropping it releases them.
pub struct NsGuard<'a> {
    rename: Option<RenameGuard<'a>>,
    stripes: Vec<MutexGuard<'a, ()>>,
}

impl NsGuard<'_> {
    /// The number of held stripes.
    pub fn held(&self) -> usize {
        self.stripes.len()
    }
}

impl<'a> NsGuard<'a> {
    /// Release the stripes and keep the rename mutex (for a retry).
    pub fn into_rename(self) -> Option<RenameGuard<'a>> {
        self.rename
    }
}

impl Default for NamespaceLocks {
    fn default() -> Self {
        Self::new()
    }
}

impl NamespaceLocks {
    pub fn new() -> Self {
        NamespaceLocks {
            stripes: (0..STRIPES)
                .map(|i| Stripe(Mutex::new_keyed(Rank::Stripe, i as u64, ()).timed(Site::Stripe)))
                .collect(),
            rename: Mutex::new(Rank::Rename, ()),
        }
    }

    /// Lock the stripes of `inos` in ascending stripe order.
    pub fn lock(&self, inos: &[u64]) -> NsGuard<'_> {
        self.lock_after(None, inos)
    }

    /// Take the rename mutex (before any stripe).
    pub fn lock_rename(&self) -> RenameGuard<'_> {
        self.rename.lock()
    }

    /// Lock the stripes of `inos` in ascending stripe order, with `rename`
    /// (already held) kept in the guard.
    pub fn lock_after<'a>(&'a self, rename: Option<RenameGuard<'a>>, inos: &[u64]) -> NsGuard<'a> {
        let mut idx: Vec<usize> = inos.iter().map(|&i| stripe(i)).collect();
        idx.sort_unstable();
        idx.dedup();
        let stripes = idx.into_iter().map(|i| self.stripes[i].0.lock()).collect();
        NsGuard { rename, stripes }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stripe_is_in_range_and_spreads() {
        let mut seen = std::collections::HashSet::new();
        for ino in 1..=4096u64 {
            let s = stripe(ino);
            assert!(s < STRIPES);
            seen.insert(s);
        }
        assert_eq!(seen.len(), STRIPES);
        assert_eq!(stripe(u64::MAX), stripe(u64::MAX)); // no overflow panic
    }

    #[test]
    fn two_inodes_in_one_stripe_lock_once() {
        let l = NamespaceLocks::new();
        let a = 1u64;
        let b = (2..).find(|&i| stripe(i) == stripe(a)).unwrap();
        let g = l.lock(&[b, a, a]);
        assert_eq!(g.held(), 1);
    }

    #[test]
    fn unsorted_inodes_lock_in_ascending_stripe_order() {
        let l = NamespaceLocks::new();
        let mut inos: Vec<u64> = (1..40).collect();
        inos.reverse();
        let g = l.lock(&inos);
        let mut want: Vec<usize> = inos.iter().map(|&i| stripe(i)).collect();
        want.sort_unstable();
        want.dedup();
        assert_eq!(g.held(), want.len());
        #[cfg(debug_assertions)]
        {
            let keys: Vec<u64> = chunk_store::sync::rank::held().iter().map(|e| e.1).collect();
            assert_eq!(keys, want.iter().map(|&i| i as u64).collect::<Vec<_>>());
        }
        drop(g);
        // The rename mutex comes before stripes.
        let r = l.lock_rename();
        let g = l.lock_after(Some(r), &[7, 3]);
        let r = g.into_rename();
        assert!(r.is_some());
    }
}

// Kani proof (TSAFE-04). Run with scripts/safety/kani.sh.
#[cfg(kani)]
mod proofs {
    use super::*;

    // Bound: none (any u64 inode number).
    #[kani::proof]
    fn stripe_in_range() {
        assert!(stripe(kani::any()) < STRIPES);
    }
}
