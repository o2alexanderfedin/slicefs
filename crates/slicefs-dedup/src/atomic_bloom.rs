//! `AtomicBloomFilter` — concurrent wrapper around `fastbloom::BloomFilter`.
//!
//! `RwLock`-protected so lookups can proceed in parallel while the
//! single-writer batcher (G1) inserts in batches. Serialize/load is the
//! payload that the bloom-snapshot codec (D4) writes to disk.
//!
//! # fastbloom 0.14 API notes
//! - `BloomFilter::with_false_pos(fpr).expected_items(cap)` exists.
//! - `as_slice()` returns `&[u64]` (not `&[u8]`); we encode each `u64`
//!   little-endian into a `Vec<u8>` for `to_bytes()`.
//! - `from_slice(&[u8])` does **not** exist. We reconstruct via
//!   `BloomFilter::from_vec(Vec<u64>).seed(&BLOOM_SEED).expected_items(cap)`,
//!   which preserves the bit vector and recomputes the same `num_hashes`
//!   when the same `(capacity, fpr)` config is supplied.
//! - `DefaultHasher::default()` randomises its seed per construction; we
//!   pin a deterministic seed (`BLOOM_SEED`) on every builder so a
//!   serialised filter can be reloaded with identical hashing semantics.

use crate::config::BloomConfig;
use fastbloom::BloomFilter;
use parking_lot::RwLock;
use std::sync::Arc;

/// Deterministic seed for the bloom hasher so on-disk snapshots can be
/// reloaded with identical bit positions across processes.
// 128-bit deterministic seed; ASCII-ish hex spelling "SLICEFS DEDB ...".
const BLOOM_SEED: u128 = 0x511C_EF5D_EDBF_15EE_D511_CEF5_DEDB_BEEF;

/// Build an empty `BloomFilter` sized for `cfg`, pinned to `BLOOM_SEED`.
fn build_empty(cfg: &BloomConfig) -> BloomFilter {
    BloomFilter::with_false_pos(cfg.fpr)
        .seed(&BLOOM_SEED)
        .expected_items(cfg.capacity)
}

/// Encode a `&[u64]` slice as little-endian bytes.
fn u64_slice_to_bytes(words: &[u64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(words.len() * 8);
    for w in words {
        out.extend_from_slice(&w.to_le_bytes());
    }
    out
}

/// Decode little-endian bytes into a `Vec<u64>`. Returns `None` if
/// `bytes.len()` is not a multiple of 8.
fn bytes_to_u64_vec(bytes: &[u8]) -> Option<Vec<u64>> {
    if !bytes.len().is_multiple_of(8) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 8);
    for chunk in bytes.chunks_exact(8) {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(chunk);
        out.push(u64::from_le_bytes(buf));
    }
    Some(out)
}

/// Concurrent bloom-filter handle. Cheap to clone — clones share the
/// same underlying filter via `Arc`.
pub struct AtomicBloomFilter {
    inner: Arc<RwLock<BloomFilter>>,
    capacity: u64,
}

impl AtomicBloomFilter {
    /// Create an empty filter sized for `cfg`.
    pub fn new(cfg: &BloomConfig) -> Self {
        Self {
            inner: Arc::new(RwLock::new(build_empty(cfg))),
            capacity: cfg.capacity as u64,
        }
    }

    /// Reconstruct a filter from a serialised payload (as produced by
    /// [`Self::to_bytes`]). On any decode mismatch, returns a fresh
    /// empty filter sized for `cfg` — the caller (J2) will rebuild it
    /// from redb. We **do not** silently change behaviour for a
    /// well-formed-but-wrong-shape payload: a length mismatch with the
    /// expected bit-vector size also falls back, which is the only
    /// safe option without a per-payload header (D4 owns the header).
    pub fn from_serialized(bytes: &[u8], cfg: &BloomConfig) -> Self {
        let bf = match bytes_to_u64_vec(bytes) {
            Some(words) => {
                // Sanity-check: the saved bit-vec length must match what
                // the same (capacity, fpr) config would produce now. If
                // not, the snapshot was written with different params —
                // refuse to load and fall back to empty.
                let expected = build_empty(cfg);
                if expected.as_slice().len() == words.len() {
                    BloomFilter::from_vec(words)
                        .seed(&BLOOM_SEED)
                        .hashes(expected.num_hashes())
                } else {
                    expected
                }
            }
            None => build_empty(cfg),
        };
        Self {
            inner: Arc::new(RwLock::new(bf)),
            capacity: cfg.capacity as u64,
        }
    }

    /// Probabilistic membership test. False positives possible; false
    /// negatives are not.
    pub fn contains(&self, hash: &[u8]) -> bool {
        self.inner.read().contains(hash)
    }

    /// Insert a single hash. Takes the write lock briefly.
    pub fn set(&self, hash: &[u8]) {
        self.inner.write().insert(hash);
    }

    /// Batch-insert. Takes the write lock once for the whole batch —
    /// preferred path from the G1 batcher.
    pub fn set_all(&self, hashes: &[&[u8]]) {
        let mut g = self.inner.write();
        for h in hashes {
            g.insert(h);
        }
    }

    /// Snapshot the underlying bit array as little-endian bytes.
    /// Pair with [`Self::from_serialized`] to roundtrip.
    pub fn to_bytes(&self) -> Vec<u8> {
        u64_slice_to_bytes(self.inner.read().as_slice())
    }

    /// Configured capacity (used by stats / drift checks).
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// Cheap handle clone — shares the same underlying filter via
    /// `Arc`. Use for spawning the batcher / stats threads.
    pub fn clone_handle(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            capacity: self.capacity,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(cap: usize) -> BloomConfig {
        BloomConfig {
            capacity: cap,
            fpr: 0.01,
            snapshot_every: 100,
            snapshot_interval: std::time::Duration::from_secs(1),
            stale_ratio: 0.9,
            drift_rebuild_ratio: 0.2,
            effective_fpr_rebuild_multiplier: 4.0,
        }
    }

    #[test]
    fn set_then_contains_is_true() {
        let bf = AtomicBloomFilter::new(&cfg(1000));
        bf.set(b"alpha");
        assert!(bf.contains(b"alpha"));
    }

    #[test]
    fn set_all_inserts_each() {
        let bf = AtomicBloomFilter::new(&cfg(1000));
        let h1 = b"a".as_slice();
        let h2 = b"b".as_slice();
        bf.set_all(&[h1, h2]);
        assert!(bf.contains(h1));
        assert!(bf.contains(h2));
    }

    #[test]
    fn unset_hash_is_false_for_small_n() {
        let bf = AtomicBloomFilter::new(&cfg(1_000_000));
        assert!(!bf.contains(b"never-inserted"));
    }

    #[test]
    fn serialize_then_load() {
        let bf = AtomicBloomFilter::new(&cfg(1000));
        bf.set(b"keep-me");
        let bytes = bf.to_bytes();
        let bf2 = AtomicBloomFilter::from_serialized(&bytes, &cfg(1000));
        assert!(bf2.contains(b"keep-me"));
    }

    #[test]
    fn clone_handle_shares_state() {
        let bf = AtomicBloomFilter::new(&cfg(1000));
        let bf2 = bf.clone_handle();
        bf.set(b"shared");
        // Inserts via `bf` are visible through `bf2` because the inner
        // Arc<RwLock<_>> is shared.
        assert!(bf2.contains(b"shared"));
        assert_eq!(bf.capacity(), bf2.capacity());
    }

    #[test]
    fn from_serialized_with_garbage_length_falls_back_empty() {
        // 3 bytes is not a multiple of 8 — cannot be a u64 vec at all.
        let bf = AtomicBloomFilter::from_serialized(&[0u8; 3], &cfg(1000));
        assert!(!bf.contains(b"anything"));
    }
}
