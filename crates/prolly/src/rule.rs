//! The boundary rule: where a node ends.
//!
//! This is Dolt's size-aware key splitter with a Weibull CDF (K = 4), see
//! the crate doc. Every cut decision uses integer math over a committed
//! table; there is no floating point. The decision after an entry reads
//! only the bytes and the entries since the last cut and the hash of the
//! entry's key. It never reads value bytes and never looks at the next
//! entry, so a rebuild may restart at the start of any node.

use crate::rule_table::{EXP_FRAC, EXP_INT};
use crate::{Config, NODE_HEADER};

/// The 32-bit boundary hash of `key` at tree level `level`: the first 4
/// bytes (little endian) of BLAKE3 over the byte `level + 1` and then the
/// key. It is computed over the key only, never the value. The level byte
/// is a salt per level: the keys that end a leaf have low level-0 hashes,
/// so without a salt the level above would cut too early.
pub fn boundary_hash(level: u8, key: &[u8]) -> u32 {
    let mut h = blake3::Hasher::new();
    h.update(&[level.wrapping_add(1)]);
    h.update(key);
    let d = h.finalize();
    let b = d.as_bytes();
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// Largest node size that the threshold math takes as is. A node is at
/// most `MAX_NODE_MAX` (32 KiB); above this value `delta` is far above 23
/// for every config, so the clamp changes no result and keeps the fourth
/// powers and the shift inside u128.
const SIZE_CLAMP: usize = 1 << 20;

/// `e^-x` scaled by 2^32, for `x` in units of 2^-32 (`x >= 0`): integer
/// math over the committed table (`rule_table.rs`), no floating point.
/// The integer part `x >> 32` picks `EXP_INT`; the top 10 bits of the
/// fraction pick two neighbours of `EXP_FRAC`, and the low 22 bits
/// interpolate between them linearly. From 23 on the result is 0.
pub(crate) fn exp_neg_fp(x: u128) -> u64 {
    let int = (x >> 32) as usize;
    if int >= EXP_INT.len() {
        return 0;
    }
    let frac = (x & 0xFFFF_FFFF) as u64;
    let j = (frac >> 22) as usize;
    let r = frac & ((1 << 22) - 1);
    let lo = EXP_FRAC[j];
    let hi = EXP_FRAC[j + 1];
    let f = lo - (((lo - hi) * r) >> 22);
    // Both factors can be 2^32, so the product needs u128.
    ((EXP_INT[int] as u128 * f as u128) >> 32) as u64
}

/// The cut threshold: a node that grows from `before` to `after` bytes
/// ends after the entry iff the 32-bit boundary hash is below this value.
/// It is `2^32 - e^-delta` in units of 2^-32 with
/// `delta = (after^4 - before^4) / L^4` (Dolt's conditional Weibull CDF
/// with K = 4 and `L = cfg.weibull_l()`), computed with integers only:
/// `delta` is `((after^4 - before^4) << 32) / L^4` in u128. The result is
/// in `0..=2^32`: 0 when the node does not grow, 2^32 (always cut) when
/// `delta >= 23`. Callers compare it as u64.
pub fn threshold(cfg: &Config, before: usize, after: usize) -> u64 {
    let p4 = |s: usize| {
        let s = s.min(SIZE_CLAMP) as u128;
        s * s * s * s
    };
    let num = p4(after).saturating_sub(p4(before)) << 32;
    let l = cfg.weibull_l() as u128;
    let delta = num / (l * l * l * l);
    (1u64 << 32) - exp_neg_fp(delta)
}

/// The cut state of one node in progress: its byte size (header included)
/// and its entry count. Build and apply both use this one type, so they
/// cannot cut differently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Splitter {
    size: usize,
    count: usize,
}

impl Default for Splitter {
    fn default() -> Self {
        Self::new()
    }
}

impl Splitter {
    /// The state of an empty node: the header only.
    pub fn new() -> Self {
        Self { size: NODE_HEADER, count: 0 }
    }

    /// Adds an entry of `entry_len` encoded bytes with key `key` at level
    /// `level`; true if the node ends after it.
    pub fn push(&mut self, cfg: &Config, level: u8, key: &[u8], entry_len: usize) -> bool {
        let before = self.size;
        self.size += entry_len;
        self.count += 1;
        // An internal node never ends after its first entry.
        if level > 0 && self.count < 2 {
            return false;
        }
        // Forced: the next entry (at most max_entry) could pass node_max.
        if self.size > cfg.force_at() {
            return true;
        }
        if self.size < cfg.node_min() {
            return false;
        }
        (boundary_hash(level, key) as u64) < threshold(cfg, before, self.size)
    }

    /// A node in progress of `size` bytes and `count` entries (tests only).
    #[cfg(test)]
    pub(crate) fn at(size: usize, count: usize) -> Self {
        Self { size, count }
    }

    /// Back to the state of an empty node.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Encoded bytes of the node so far, header included.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Entries of the node so far.
    pub fn count(&self) -> usize {
        self.count
    }
}
