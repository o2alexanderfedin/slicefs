//! A blocked Bloom filter over object digests, saved in each run file.
//!
//! The keys are BLAKE3 digests, which are already uniform, so the probe
//! positions come straight from the digest bytes, with no hashing:
//!
//! - the block: the high 64 bits of (u64 LE of digest bytes 0..8) x (block
//!   count), as a u128 product;
//! - h1 = u64 LE of digest bytes 8..16, h2 = (u64 LE of digest bytes 16..24)
//!   OR 1;
//! - bit i (i in 0..k) of the 512-bit block: (h1 + i x h2) mod 512
//!   (Kirsch-Mitzenmacher).
//!
//! k = round(bits per key x 0.693), clamped to 1..=16 (7 at 10 bits per
//! key), so the run footer stores only the bits per key. The bytes are the
//! 64-bit words of the blocks, little-endian. This layout is part of the run
//! format: a change is a new filter type in the footer.
//!
//! Only the digest is a key, not the tag: two IDs with the same digest and
//! another tag share their bits, which is only a false positive.

use crate::DIGEST_LEN;

/// Bits in one block (one 64-byte cache line).
pub(crate) const BLOCK_BITS: u64 = 512;
/// Bytes in one block.
pub(crate) const BLOCK_BYTES: usize = 64;
const BLOCK_WORDS: usize = BLOCK_BYTES / 8;

fn le_u64(d: &[u8; DIGEST_LEN], at: usize) -> u64 {
    u64::from_le_bytes(d[at..at + 8].try_into().unwrap())
}

/// The number of probes for `bits_per_key` (at least 1).
pub(crate) fn probes(bits_per_key: u8) -> u32 {
    ((bits_per_key as f64 * 0.693).round() as u32).clamp(1, 16)
}

/// The block count for `n` keys at `bits_per_key`: at least one block.
/// `None` if the size overflows.
pub(crate) fn blocks_for(n: u64, bits_per_key: u8) -> Option<u64> {
    Some(n.checked_mul(bits_per_key as u64)?.div_ceil(BLOCK_BITS).max(1))
}

/// See the module comment.
pub(crate) struct Bloom {
    words: Vec<u64>,
    blocks: u64,
    k: u32,
}

impl Bloom {
    /// An empty filter for `n` keys at `bits_per_key` (at least 1).
    pub fn new(n: u64, bits_per_key: u8) -> Bloom {
        let blocks = blocks_for(n, bits_per_key).expect("filter size overflows");
        let words = usize::try_from(blocks).ok().and_then(|b| b.checked_mul(BLOCK_WORDS)).expect("filter size overflows");
        Bloom { words: vec![0; words], blocks, k: probes(bits_per_key) }
    }

    /// The first word of the block of `d`, and h1, h2.
    #[inline]
    fn probe(&self, d: &[u8; DIGEST_LEN]) -> (usize, u64, u64) {
        let block = ((le_u64(d, 0) as u128 * self.blocks as u128) >> 64) as usize;
        (block * BLOCK_WORDS, le_u64(d, 8), le_u64(d, 16) | 1)
    }

    pub fn insert(&mut self, d: &[u8; DIGEST_LEN]) {
        let (at, h1, h2) = self.probe(d);
        for i in 0..self.k as u64 {
            let bit = h1.wrapping_add(i.wrapping_mul(h2)) % BLOCK_BITS;
            self.words[at + (bit / 64) as usize] |= 1 << (bit % 64);
        }
    }

    /// False if `d` is surely not a key; true if it may be one.
    #[inline]
    pub fn may_contain(&self, d: &[u8; DIGEST_LEN]) -> bool {
        let (at, h1, h2) = self.probe(d);
        let block = &self.words[at..at + BLOCK_WORDS];
        (0..self.k as u64).all(|i| {
            let bit = h1.wrapping_add(i.wrapping_mul(h2)) % BLOCK_BITS;
            block[(bit / 64) as usize] & (1 << (bit % 64)) != 0
        })
    }

    /// Bytes of the filter (in RAM and in the run file).
    pub fn len(&self) -> usize {
        self.words.len() * 8
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        self.words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    /// The filter in `b` with `bits_per_key`. A length that is zero or not
    /// a whole number of blocks is refused.
    pub fn from_bytes(b: &[u8], bits_per_key: u8) -> Result<Bloom, String> {
        if b.is_empty() || !b.len().is_multiple_of(BLOCK_BYTES) {
            return Err(format!("filter length {} is not a whole, non-zero number of blocks", b.len()));
        }
        if bits_per_key == 0 {
            return Err("a filter with 0 bits per key".into());
        }
        let words: Vec<u64> = b.as_chunks::<8>().0.iter().map(|c| u64::from_le_bytes(*c)).collect();
        let blocks = (b.len() / BLOCK_BYTES) as u64;
        Ok(Bloom { words, blocks, k: probes(bits_per_key) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A uniform digest (as BLAKE3 gives), from `i` and a seed.
    fn digest(seed: u64, i: u64) -> [u8; DIGEST_LEN] {
        let mut h = blake3::Hasher::new();
        h.update(&seed.to_le_bytes());
        h.update(&i.to_le_bytes());
        *h.finalize().as_bytes()
    }

    fn filled(n: u64, bits: u8) -> Bloom {
        let mut f = Bloom::new(n, bits);
        for i in 0..n {
            f.insert(&digest(1, i));
        }
        f
    }

    #[test]
    #[cfg_attr(miri, ignore = "too slow under Miri (over 300 s in the probe)")]
    fn no_false_negatives() {
        let f = filled(100_000, 10);
        assert!((0..100_000).all(|i| f.may_contain(&digest(1, i))));
    }

    #[test]
    #[cfg_attr(miri, ignore = "too slow under Miri (over 300 s in the probe)")]
    fn the_false_positive_rate_is_about_one_percent() {
        let f = filled(100_000, 10);
        let fp = (0..100_000).filter(|&i| f.may_contain(&digest(2, i))).count();
        assert!((500..=2000).contains(&fp), "{fp} false positives of 100,000");
    }

    #[test]
    fn the_size_is_ten_bits_per_key_in_whole_blocks() {
        assert_eq!(probes(10), 7);
        for n in [0u64, 1, 51, 52, 100_000, 1_000_000] {
            let want = (n * 10).div_ceil(512).max(1) * 64;
            assert_eq!(Bloom::new(n, 10).len() as u64, want, "n {n}");
        }
        assert_eq!(blocks_for(u64::MAX, 10), None);
    }

    #[test]
    #[cfg_attr(miri, ignore = "too slow under Miri (over 300 s in the probe)")]
    fn bytes_round_trip() {
        let f = filled(10_000, 10);
        let back = Bloom::from_bytes(&f.to_bytes(), 10).unwrap();
        for i in 0..10_000 {
            for seed in [1, 3] {
                assert_eq!(back.may_contain(&digest(seed, i)), f.may_contain(&digest(seed, i)));
            }
        }
        let b = f.to_bytes();
        assert!(Bloom::from_bytes(&[], 10).is_err(), "zero length");
        assert!(Bloom::from_bytes(&b[..b.len() - 8], 10).is_err(), "not whole blocks");
        assert!(Bloom::from_bytes(&b[..63], 10).is_err());
        assert!(Bloom::from_bytes(&b, 0).is_err(), "0 bits per key");
    }
}
