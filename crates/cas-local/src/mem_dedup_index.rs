//! `MemDedupIndex`: bloom filter + `HashSet` `DedupIndex`.
//!
//! Two-tier dedup detection (CAS-07):
//!
//! 1. **Fast path** (`bloom_check`): an `AtomicBloomFilter` held in memory.
//!    Returns `false` when the hash is definitely absent — no authoritative lookup needed.
//! 2. **Slow path** (`lookup`): a `HashSet<ChunkHash>` that is the authoritative set.
//!    Called only when the bloom filter says "maybe present".
//!
//! `remove()` removes from the authoritative set but does NOT update the bloom filter —
//! false positives from removed hashes are acceptable (they just cause an extra `lookup`
//! call) and bloom filters do not support deletion.
//!
//! # Serialization
//!
//! Because `fastbloom::AtomicBloomFilter` does not have serde enabled in this workspace,
//! `save_to_writer` / `load_from_reader` serialize only the authoritative `HashSet`.
//! On `load_from_reader`, a fresh bloom filter is rebuilt from the loaded set.
//! This ensures correctness at the cost of one O(n) rebuild on startup.

use std::{
    collections::HashSet,
    io::{Read, Write},
    sync::RwLock,
};

use fastbloom::AtomicBloomFilter;

use slicefs_traits::{
    dedup_index::{DedupIndex, DedupResult},
    error::CasError,
    hash::ChunkHash,
};

/// In-memory dedup index with a bloom filter fast path and an authoritative `HashSet`.
///
/// Thread-safe: the bloom filter uses atomic operations internally; the `HashSet`
/// is protected by an `RwLock`.
pub struct MemDedupIndex {
    bloom: AtomicBloomFilter,
    present: RwLock<HashSet<ChunkHash>>,
    /// Expected item count, stored for serialization round-trips (bloom rebuild).
    expected_items: usize,
    /// Target false positive rate, stored for serialization round-trips.
    false_positive_rate: f64,
}

impl MemDedupIndex {
    /// Create a new `MemDedupIndex`.
    ///
    /// - `expected_items`: approximate number of unique hashes the bloom filter should accommodate.
    /// - `false_positive_rate`: target false positive probability (e.g., `0.01` for 1%).
    pub fn new(expected_items: usize, false_positive_rate: f64) -> Self {
        let bloom =
            AtomicBloomFilter::with_false_pos(false_positive_rate).expected_items(expected_items);
        Self {
            bloom,
            present: RwLock::new(HashSet::new()),
            expected_items,
            false_positive_rate,
        }
    }

    /// Serialize the index state to `writer`.
    ///
    /// Only the authoritative `HashSet` is persisted (not the bloom filter).
    /// On `load_from_reader`, the bloom filter is rebuilt from the set.
    ///
    /// Format: `u64` count followed by `u32` length-prefixed bytes for each hash.
    pub fn save_to_writer(&self, writer: &mut impl Write) -> Result<(), CasError> {
        let guard = self
            .present
            .read()
            .map_err(|e| CasError::Index(e.to_string()))?;
        let count = guard.len() as u64;
        writer
            .write_all(&count.to_le_bytes())
            .map_err(CasError::Io)?;
        for hash in guard.iter() {
            let bytes = hash.as_bytes();
            let len = bytes.len() as u32;
            writer.write_all(&len.to_le_bytes()).map_err(CasError::Io)?;
            writer.write_all(bytes).map_err(CasError::Io)?;
        }
        Ok(())
    }

    /// Deserialize index state from `reader`.
    ///
    /// Rebuilds the bloom filter from the loaded set using the same `expected_items`
    /// and `false_positive_rate` parameters stored in the serialized data's header.
    ///
    /// Format: `u64` expected_items, `f64` false_positive_rate, `u64` count,
    /// then `u32` length-prefixed hash bytes for each entry.
    pub fn load_from_reader(reader: &mut impl Read) -> Result<Self, CasError> {
        // Read header: expected_items (u64) and false_positive_rate (f64)
        let mut buf8 = [0u8; 8];
        reader.read_exact(&mut buf8).map_err(CasError::Io)?;
        let expected_items = u64::from_le_bytes(buf8) as usize;

        reader.read_exact(&mut buf8).map_err(CasError::Io)?;
        let false_positive_rate = f64::from_le_bytes(buf8);

        // Read count
        reader.read_exact(&mut buf8).map_err(CasError::Io)?;
        let count = u64::from_le_bytes(buf8) as usize;

        let bloom = AtomicBloomFilter::with_false_pos(false_positive_rate)
            .expected_items(expected_items.max(count).max(1));

        let mut present = HashSet::with_capacity(count);
        for _ in 0..count {
            let mut len_buf = [0u8; 4];
            reader.read_exact(&mut len_buf).map_err(CasError::Io)?;
            let len = u32::from_le_bytes(len_buf) as usize;
            let mut bytes = vec![0u8; len];
            reader.read_exact(&mut bytes).map_err(CasError::Io)?;
            let hash = ChunkHash::from_bytes(bytes);
            bloom.insert(hash.as_bytes());
            present.insert(hash);
        }

        Ok(Self {
            bloom,
            present: RwLock::new(present),
            expected_items,
            false_positive_rate,
        })
    }

    /// Serialize with header (expected_items + false_positive_rate) for round-trip.
    pub fn save_to_writer_with_header(&self, writer: &mut impl Write) -> Result<(), CasError> {
        // Write header
        writer
            .write_all(&(self.expected_items as u64).to_le_bytes())
            .map_err(CasError::Io)?;
        writer
            .write_all(&self.false_positive_rate.to_le_bytes())
            .map_err(CasError::Io)?;

        // Write count + entries
        let guard = self
            .present
            .read()
            .map_err(|e| CasError::Index(e.to_string()))?;
        let count = guard.len() as u64;
        writer
            .write_all(&count.to_le_bytes())
            .map_err(CasError::Io)?;
        for hash in guard.iter() {
            let bytes = hash.as_bytes();
            let len = bytes.len() as u32;
            writer.write_all(&len.to_le_bytes()).map_err(CasError::Io)?;
            writer.write_all(bytes).map_err(CasError::Io)?;
        }
        Ok(())
    }
}

impl DedupIndex for MemDedupIndex {
    fn bloom_check(&self, hash: &ChunkHash) -> bool {
        self.bloom.contains(hash.as_bytes())
    }

    fn lookup(&self, hash: &ChunkHash) -> Result<DedupResult, CasError> {
        // Fast path: if bloom says definitely absent, skip authoritative lookup.
        if !self.bloom_check(hash) {
            return Ok(DedupResult::DefinitelyAbsent);
        }

        // Slow path: authoritative check.
        let guard = self
            .present
            .read()
            .map_err(|e| CasError::Index(e.to_string()))?;
        if guard.contains(hash) {
            Ok(DedupResult::Present)
        } else {
            // Bloom false positive.
            Ok(DedupResult::Absent)
        }
    }

    fn insert(&self, hash: &ChunkHash) -> Result<(), CasError> {
        // Insert into bloom filter first.
        self.bloom.insert(hash.as_bytes());

        // Then insert into authoritative set.
        let mut guard = self
            .present
            .write()
            .map_err(|e| CasError::Index(e.to_string()))?;
        guard.insert(hash.clone());
        Ok(())
    }

    fn remove(&self, hash: &ChunkHash) -> Result<(), CasError> {
        // Only remove from authoritative set — do NOT update bloom filter.
        let mut guard = self
            .present
            .write()
            .map_err(|e| CasError::Index(e.to_string()))?;
        guard.remove(hash);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blake3_hasher::Blake3Hasher;
    use proptest::prelude::*;
    use slicefs_traits::hash::ContentHasher;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn default_index() -> MemDedupIndex {
        MemDedupIndex::new(10_000, 0.01)
    }

    fn make_hash(data: &[u8]) -> ChunkHash {
        Blake3Hasher.hash(data)
    }

    // -----------------------------------------------------------------------
    // Basic insert / lookup / bloom_check
    // -----------------------------------------------------------------------

    #[test]
    fn insert_then_lookup_returns_present() {
        let idx = default_index();
        let hash = make_hash(b"block-a");

        idx.insert(&hash).unwrap();
        let result = idx.lookup(&hash).unwrap();
        assert_eq!(result, DedupResult::Present);
    }

    #[test]
    fn lookup_unknown_hash_returns_definitely_absent() {
        let idx = default_index();
        let hash = make_hash(b"never-inserted");

        // With overwhelming probability the bloom filter will say absent for a
        // never-inserted key. (False positives could theoretically flip this,
        // but with a fresh 10k-capacity filter and a single probe the probability
        // is negligible.)
        let result = idx.lookup(&hash).unwrap();
        // Either DefinitelyAbsent or Absent are acceptable from the trait contract,
        // but for a fresh index with no entries it must be DefinitelyAbsent.
        assert_eq!(result, DedupResult::DefinitelyAbsent);
    }

    #[test]
    fn bloom_check_returns_false_for_never_inserted() {
        let idx = default_index();
        let hash = make_hash(b"not-inserted");
        // Fresh filter: bloom_check must return false (no false negatives in bloom).
        assert!(!idx.bloom_check(&hash));
    }

    #[test]
    fn bloom_check_returns_true_for_inserted_hash() {
        let idx = default_index();
        let hash = make_hash(b"inserted-block");

        idx.insert(&hash).unwrap();
        assert!(idx.bloom_check(&hash));
    }

    // -----------------------------------------------------------------------
    // Remove behavior
    // -----------------------------------------------------------------------

    #[test]
    fn remove_then_lookup_returns_absent_not_definitely_absent() {
        let idx = default_index();
        let hash = make_hash(b"block-to-remove");

        idx.insert(&hash).unwrap();
        idx.remove(&hash).unwrap();

        let result = idx.lookup(&hash).unwrap();
        // After remove, the bloom still says "maybe present", so we go to the
        // authoritative lookup. The entry is gone → Absent (bloom false positive).
        // DefinitelyAbsent would mean the bloom was updated, which we explicitly do NOT do.
        assert_eq!(
            result,
            DedupResult::Absent,
            "after remove, result must be Absent (bloom still set), not DefinitelyAbsent"
        );
    }

    #[test]
    fn bloom_check_still_true_after_remove() {
        let idx = default_index();
        let hash = make_hash(b"block-to-remove");

        idx.insert(&hash).unwrap();
        idx.remove(&hash).unwrap();

        // Bloom filter is NOT cleared on remove — this is by design.
        assert!(
            idx.bloom_check(&hash),
            "bloom_check must remain true after remove (bloom does not support deletion)"
        );
    }

    // -----------------------------------------------------------------------
    // Dedup detection flow
    // -----------------------------------------------------------------------

    #[test]
    fn dedup_flow_inserted_hash_detected_as_duplicate() {
        let idx = default_index();
        let hash = make_hash(b"chunk-content");

        // First time: not present.
        let first = idx.lookup(&hash).unwrap();
        assert_ne!(
            first,
            DedupResult::Present,
            "hash should not be present yet"
        );

        // Insert.
        idx.insert(&hash).unwrap();

        // Second time: detected as duplicate.
        let second = idx.lookup(&hash).unwrap();
        assert_eq!(
            second,
            DedupResult::Present,
            "hash should be present after insert"
        );
    }

    #[test]
    fn dedup_flow_remove_and_reinsert_works() {
        let idx = default_index();
        let hash = make_hash(b"re-insert-block");

        idx.insert(&hash).unwrap();
        assert_eq!(idx.lookup(&hash).unwrap(), DedupResult::Present);

        idx.remove(&hash).unwrap();
        assert_eq!(idx.lookup(&hash).unwrap(), DedupResult::Absent);

        // Re-insert: should work and be findable again.
        idx.insert(&hash).unwrap();
        assert_eq!(idx.lookup(&hash).unwrap(), DedupResult::Present);
    }

    // -----------------------------------------------------------------------
    // Thread safety
    // -----------------------------------------------------------------------

    #[test]
    fn concurrent_insert_lookup_does_not_panic() {
        use std::sync::Arc;

        let idx = Arc::new(default_index());
        let n = 100;

        std::thread::scope(|s| {
            // 4 inserter threads
            for t in 0..4usize {
                let idx_clone = Arc::clone(&idx);
                s.spawn(move || {
                    for i in 0..n {
                        let data = format!("thread-{}-block-{}", t, i);
                        let hash = make_hash(data.as_bytes());
                        idx_clone.insert(&hash).unwrap();
                    }
                });
            }
            // 4 lookup threads
            for t in 0..4usize {
                let idx_clone = Arc::clone(&idx);
                s.spawn(move || {
                    for i in 0..n {
                        let data = format!("thread-{}-block-{}", t, i);
                        let hash = make_hash(data.as_bytes());
                        // Just call lookup — we don't assert a specific result because
                        // the inserter and lookup threads race. We only care it doesn't panic.
                        let _ = idx_clone.lookup(&hash);
                    }
                });
            }
        });
        // If we get here without panic, the test passes.
    }

    // -----------------------------------------------------------------------
    // Bloom false positive demonstration
    // -----------------------------------------------------------------------

    #[test]
    fn bloom_false_positive_two_tier_design_demonstration() {
        // Insert 10_000 distinct hashes, then probe 10_000 different hashes.
        // With a 1% false positive rate we expect ~100 false positives, so we
        // assert at least one non-inserted hash triggers bloom_check=true but
        // lookup returns Absent (demonstrating the two-tier design).

        let idx = MemDedupIndex::new(10_000, 0.01);

        // Insert 10_000 hashes.
        let inserted: Vec<ChunkHash> = (0..10_000u32)
            .map(|i| make_hash(&i.to_le_bytes()))
            .collect();
        for h in &inserted {
            idx.insert(h).unwrap();
        }

        // Probe 10_000 different hashes (offset by 100_000).
        let probes: Vec<ChunkHash> = (100_000u32..110_000u32)
            .map(|i| make_hash(&i.to_le_bytes()))
            .collect();

        let mut false_positive_count = 0usize;
        for h in &probes {
            if idx.bloom_check(h) {
                // Bloom says "maybe present" — authoritative lookup should say Absent.
                let result = idx.lookup(h).unwrap();
                assert_eq!(
                    result,
                    DedupResult::Absent,
                    "bloom false positive must resolve to Absent via authoritative lookup"
                );
                false_positive_count += 1;
            }
        }

        // With 1% FPR and 10_000 probes we expect ~100; assert at least 1 to
        // prove the two-tier path was exercised.
        assert!(
            false_positive_count >= 1,
            "expected at least one bloom false positive to demonstrate two-tier design (got 0)"
        );
    }

    // -----------------------------------------------------------------------
    // Serialization round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn serialization_round_trip() {
        let idx = MemDedupIndex::new(1_000, 0.01);

        let hashes: Vec<ChunkHash> = (0..50u32).map(|i| make_hash(&i.to_le_bytes())).collect();
        for h in &hashes {
            idx.insert(h).unwrap();
        }

        // Serialize
        let mut buf = Vec::new();
        idx.save_to_writer_with_header(&mut buf)
            .expect("save_to_writer_with_header should succeed");

        // Deserialize
        let mut cursor = std::io::Cursor::new(&buf);
        let restored =
            MemDedupIndex::load_from_reader(&mut cursor).expect("load_from_reader should succeed");

        // All originally inserted hashes must be Present in the restored index.
        for h in &hashes {
            let result = restored.lookup(h).unwrap();
            assert_eq!(
                result,
                DedupResult::Present,
                "hash should be Present in restored index"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Property-based test
    // -----------------------------------------------------------------------

    proptest! {
        /// For any set of random hashes (modeled as random byte vectors), all
        /// inserted hashes must be findable by lookup.
        #[test]
        fn prop_all_inserted_hashes_are_found(
            items in proptest::collection::vec(
                proptest::collection::vec(any::<u8>(), 1..=64),
                1..=100,
            )
        ) {
            let idx = MemDedupIndex::new(200, 0.01);
            let hashes: Vec<ChunkHash> = items.iter().map(|b| make_hash(b)).collect();

            for h in &hashes {
                idx.insert(h).unwrap();
            }

            for h in &hashes {
                let result = idx.lookup(h).unwrap();
                prop_assert_eq!(
                    result,
                    DedupResult::Present,
                    "every inserted hash must be findable"
                );
            }
        }
    }
}
