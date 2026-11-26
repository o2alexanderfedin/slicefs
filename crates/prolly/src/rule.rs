//! The boundary rule: where a node ends.
//!
//! This is Dolt's size-aware key splitter with a Weibull CDF (K = 4), see
//! the crate doc. The decision after an entry reads only the bytes and the
//! entries since the last cut and the hash of the entry's key. It never
//! reads value bytes and never looks at the next entry, so a rebuild may
//! restart at the start of any node.

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

/// e to the power of minus `x`, for `x >= 0`, with only the four basic
/// float operations, so the result is the same bits on every platform
/// (node IDs depend on every cut). Halve `x` below 0.5, sum 16 Taylor
/// terms in a fixed order, then square once per halving.
pub(crate) fn exp_neg(x: f64) -> f64 {
    if x >= 40.0 {
        return 0.0;
    }
    let mut y = x;
    let mut k = 0u32;
    while y >= 0.5 {
        y /= 2.0;
        k += 1;
    }
    let mut sum = 1.0f64;
    let mut term = 1.0f64;
    for n in 1..16 {
        term = term * -y / n as f64;
        sum += term;
    }
    for _ in 0..k {
        sum *= sum;
    }
    sum
}

/// `s` to the fourth power in u64 (saturating; node sizes are at most
/// 2^14, so the value is at most 2^56 and exact).
fn s4(s: usize) -> u64 {
    let s = s as u64;
    s.saturating_mul(s).saturating_mul(s).saturating_mul(s)
}

/// The probability that a node ends after an entry that grows it from
/// `before` to `after` bytes, given that it did not end before: Dolt's
/// (F(after) - F(before)) / (1 - F(before)) for the Weibull CDF F with
/// K = 4 and scale L. That is 1 - e^-delta with
/// delta = (after^4 - before^4) / L^4; the difference is exact in u64.
pub fn cut_probability(cfg: &Config, before: usize, after: usize) -> f64 {
    let delta = s4(after).saturating_sub(s4(before)) as f64 / s4(cfg.weibull_l()) as f64;
    1.0 - exp_neg(delta)
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
        (boundary_hash(level, key) as f64 / 4_294_967_295.0) < cut_probability(cfg, before, self.size)
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
