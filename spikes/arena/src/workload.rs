//! Seeded workloads. `mixed`: objects with FastCDC 2/8/32 KiB sizes, half
//! random bytes, half about 2:1 compressible by zstd-3.

use crate::chunk_ranges;

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// The same generator as the `chunk-store` tests: incompressible bytes.
pub fn random(n: usize, seed: u64) -> Vec<u8> {
    let mut r = Rng::new(seed);
    (0..n).map(|_| r.next() as u8).collect()
}

/// Bytes that zstd-3 shrinks to about half: each byte is one of 16 values
/// (4 bits of entropy per byte).
pub fn compressible(n: usize, seed: u64) -> Vec<u8> {
    let mut r = Rng::new(seed);
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let x = r.next();
        for k in 0..16 {
            if out.len() == n {
                break;
            }
            out.push(b'a' + ((x >> (k * 4)) & 0xf) as u8);
        }
    }
    out
}

/// Object sizes as FastCDC 2/8/32 KiB cuts them from random data.
pub fn cdc_sizes(seed: u64) -> Vec<usize> {
    chunk_ranges(&random(8 << 20, seed)).into_iter().map(|r| r.len()).collect()
}

/// `n` distinct `mixed` objects (even index: random; odd: compressible).
pub fn mixed(n: usize, seed: u64) -> Vec<Vec<u8>> {
    let sizes = cdc_sizes(seed ^ 0x5151);
    (0..n)
        .map(|i| {
            let len = sizes[i % sizes.len()];
            let s = seed.wrapping_mul(1_000_003).wrapping_add(i as u64);
            if i % 2 == 0 {
                random(len, s)
            } else {
                compressible(len, s)
            }
        })
        .collect()
}

/// `mixed` objects until their raw total reaches `bytes`.
pub fn mixed_bytes(bytes: u64, seed: u64) -> Vec<Vec<u8>> {
    let sizes = cdc_sizes(seed ^ 0x5151);
    let mut out = Vec::new();
    let mut total = 0u64;
    let mut i = 0usize;
    while total < bytes {
        let len = sizes[i % sizes.len()];
        let s = seed.wrapping_mul(1_000_003).wrapping_add(i as u64);
        out.push(if i % 2 == 0 { random(len, s) } else { compressible(len, s) });
        total += len as u64;
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compressible_is_about_two_to_one() {
        let d = compressible(64 << 10, 1);
        let c = zstd::bulk::compress(&d, 3).unwrap();
        let ratio = d.len() as f64 / c.len() as f64;
        assert!((1.6..2.4).contains(&ratio), "ratio {ratio}");
    }
}
