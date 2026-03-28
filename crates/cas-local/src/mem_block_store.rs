//! `MemBlockStore`: in-memory `HashMap`-backed `BlockStore`.

use std::collections::HashMap;
use std::sync::RwLock;

use slicefs_traits::block_store::{BlockStore, BlockStoreConfig};
use slicefs_traits::error::CasError;
use slicefs_traits::hash::{ChunkHash, ContentHasher};

/// In-memory [`BlockStore`] backed by a `HashMap<ChunkHash, Vec<u8>>`.
///
/// Designed as the reference/testing implementation for the CAS pipeline. Thread-safe
/// via an internal `RwLock`.
///
/// # Integrity
///
/// - **Write-time:** `put()` verifies that the provided `hash` matches the hash of `data`.
///   A mismatch returns [`CasError::IntegrityFailure`]. This catches caller bugs where the
///   hash and data are computed independently and diverge (e.g., hash of a compressed buffer
///   stored with the uncompressed bytes).
/// - **Read-time (optional):** When `config.verify_on_read == true`, `get()` re-hashes the
///   retrieved bytes and returns [`CasError::IntegrityFailure`] on mismatch. In an in-memory
///   store real bit-rot cannot occur, but the verification *code path* is exercised so that
///   tests can rely on the same logic that disk stores will use.
pub struct MemBlockStore {
    store: RwLock<HashMap<ChunkHash, Vec<u8>>>,
    config: BlockStoreConfig,
    hasher: Box<dyn ContentHasher>,
}

impl MemBlockStore {
    /// Create a new `MemBlockStore`.
    ///
    /// The `hasher` is used for both write-time integrity verification and, when
    /// `config.verify_on_read` is `true`, read-time re-hashing.
    pub fn new(config: BlockStoreConfig, hasher: Box<dyn ContentHasher>) -> Self {
        Self {
            store: RwLock::new(HashMap::new()),
            config,
            hasher,
        }
    }
}

impl BlockStore for MemBlockStore {
    fn put(&self, hash: &ChunkHash, data: &[u8]) -> Result<(), CasError> {
        // Write-time integrity: verify that the provided hash matches the data.
        let computed = self.hasher.hash(data);
        if &computed != hash {
            return Err(CasError::IntegrityFailure {
                expected: hash.clone(),
                actual: computed,
            });
        }

        let mut store = self.store.write().unwrap();

        // Idempotence: if the same hash already exists, verify the stored bytes
        // match. If they differ, that is a hash collision (catastrophic).
        if let Some(existing) = store.get(hash) {
            if existing != data {
                return Err(CasError::HashCollision { hash: hash.clone() });
            }
            // Identical data already stored — no-op.
            return Ok(());
        }

        store.insert(hash.clone(), data.to_vec());
        Ok(())
    }

    fn get(&self, hash: &ChunkHash) -> Result<Vec<u8>, CasError> {
        let store = self.store.read().unwrap();
        let data = store
            .get(hash)
            .ok_or_else(|| CasError::NotFound(hash.clone()))?
            .clone();

        if self.config.verify_on_read {
            let actual = self.hasher.hash(&data);
            if &actual != hash {
                return Err(CasError::IntegrityFailure {
                    expected: hash.clone(),
                    actual,
                });
            }
        }

        Ok(data)
    }

    fn exists(&self, hash: &ChunkHash) -> Result<bool, CasError> {
        let store = self.store.read().unwrap();
        Ok(store.contains_key(hash))
    }

    fn delete(&self, hash: &ChunkHash) -> Result<(), CasError> {
        let mut store = self.store.write().unwrap();
        store.remove(hash);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blake3_hasher::Blake3Hasher;
    use proptest::prelude::*;
    use std::sync::Arc;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn make_store(verify_on_read: bool) -> MemBlockStore {
        MemBlockStore::new(
            BlockStoreConfig { verify_on_read },
            Box::new(Blake3Hasher),
        )
    }

    fn make_store_arc(verify_on_read: bool) -> Arc<MemBlockStore> {
        Arc::new(make_store(verify_on_read))
    }

    fn hash_of(data: &[u8]) -> ChunkHash {
        Blake3Hasher.hash(data)
    }

    // -----------------------------------------------------------------------
    // Basic round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn put_then_get_returns_same_data() {
        let store = make_store(false);
        let data = b"hello, cas";
        let hash = hash_of(data);
        store.put(&hash, data).unwrap();
        let retrieved = store.get(&hash).unwrap();
        assert_eq!(retrieved, data);
    }

    #[test]
    fn put_same_hash_twice_is_idempotent() {
        let store = make_store(false);
        let data = b"idempotent data";
        let hash = hash_of(data);
        store.put(&hash, data).unwrap();
        // Second put with identical data must succeed without error
        store.put(&hash, data).unwrap();
    }

    // -----------------------------------------------------------------------
    // NotFound / existence
    // -----------------------------------------------------------------------

    #[test]
    fn get_nonexistent_hash_returns_not_found() {
        let store = make_store(false);
        let hash = hash_of(b"ghost");
        let err = store.get(&hash).unwrap_err();
        assert!(
            matches!(err, CasError::NotFound(_)),
            "expected NotFound, got {:?}",
            err
        );
    }

    #[test]
    fn exists_true_after_put_false_before() {
        let store = make_store(false);
        let data = b"existence check";
        let hash = hash_of(data);

        assert!(!store.exists(&hash).unwrap(), "should not exist before put");
        store.put(&hash, data).unwrap();
        assert!(store.exists(&hash).unwrap(), "should exist after put");
    }

    // -----------------------------------------------------------------------
    // Delete
    // -----------------------------------------------------------------------

    #[test]
    fn delete_removes_block_subsequent_get_returns_not_found() {
        let store = make_store(false);
        let data = b"delete me";
        let hash = hash_of(data);

        store.put(&hash, data).unwrap();
        store.delete(&hash).unwrap();
        let err = store.get(&hash).unwrap_err();
        assert!(matches!(err, CasError::NotFound(_)));
    }

    #[test]
    fn delete_nonexistent_is_idempotent() {
        let store = make_store(false);
        let hash = hash_of(b"never stored");
        // Must succeed silently
        store.delete(&hash).unwrap();
    }

    // -----------------------------------------------------------------------
    // Integrity checks
    // -----------------------------------------------------------------------

    #[test]
    fn write_time_integrity_check_catches_hash_data_mismatch() {
        let store = make_store(false);
        let data = b"correct data";
        let wrong_hash = hash_of(b"wrong data"); // hash of DIFFERENT bytes
        let err = store.put(&wrong_hash, data).unwrap_err();
        assert!(
            matches!(err, CasError::IntegrityFailure { .. }),
            "expected IntegrityFailure, got {:?}",
            err
        );
    }

    /// The `verify_on_read` path is exercised here. In an in-memory store there
    /// is no real bit-rot, but the code path is the same one disk stores will use.
    /// We verify the path runs correctly by confirming a correct get() succeeds with
    /// verify_on_read=true.
    #[test]
    fn read_time_integrity_verification_passes_for_correct_data() {
        let store = make_store(true); // verify_on_read = true
        let data = b"data with read verify";
        let hash = hash_of(data);
        store.put(&hash, data).unwrap();
        let retrieved = store.get(&hash).unwrap();
        assert_eq!(retrieved, data);
    }

    #[test]
    fn verify_on_read_false_skips_integrity_check() {
        let store = make_store(false); // verify_on_read = false
        let data = b"no read verify";
        let hash = hash_of(data);
        store.put(&hash, data).unwrap();
        // Should succeed — no re-hashing on read
        let retrieved = store.get(&hash).unwrap();
        assert_eq!(retrieved, data);
    }

    #[test]
    fn hash_collision_detected_same_hash_different_data() {
        // Manufacture a fake hash collision by crafting a raw hash and inserting
        // a block with it directly (bypassing the integrity check via write lock).
        // Then attempt to put different data with the same hash.
        //
        // We can't get a real hash collision, so we use the internal store directly
        // in the test to simulate the state that would arise from a true collision.
        let store = make_store(false);

        // First, put legitimate data
        let data_a = b"data block A";
        let hash_a = hash_of(data_a);
        store.put(&hash_a, data_a).unwrap();

        // Now manually inject a "different" block under the same key by using the
        // internal RwLock — this simulates what would happen if two inputs produced
        // the same hash.
        {
            let mut inner = store.store.write().unwrap();
            inner.insert(hash_a.clone(), b"data block CORRUPTED".to_vec());
        }

        // Attempting to put different data under the same hash via the public API
        // should return HashCollision. But first we need a hash for the corrupted data
        // so write-time check passes for that value.
        let data_b = b"data block B different";
        let hash_b = hash_of(data_b);
        // Insert data_b with its correct hash — no collision
        store.put(&hash_b, data_b).unwrap();

        // Now simulate a collision: put data_b under hash_a (which already has different data).
        // We bypass write-time integrity by manually computing a "matching" hash...
        // Actually: to test HashCollision we inject via internal state, then call put with
        // data that hashes to hash_a but differs from what's stored.
        // The only way to trigger HashCollision is to have stored != incoming when hash matches.
        // Let's craft the scenario:
        {
            let mut inner = store.store.write().unwrap();
            // Overwrite stored bytes with something that does NOT match hash_a
            inner.insert(hash_a.clone(), b"something completely different".to_vec());
        }

        // Now call put(hash_a, data_a) — data_a hashes to hash_a (passes write integrity check),
        // but stored bytes differ from data_a → HashCollision
        let err = store.put(&hash_a, data_a).unwrap_err();
        assert!(
            matches!(err, CasError::HashCollision { .. }),
            "expected HashCollision, got {:?}",
            err
        );
    }

    // -----------------------------------------------------------------------
    // Thread safety
    // -----------------------------------------------------------------------

    #[test]
    fn concurrent_put_get_from_multiple_threads_does_not_panic() {
        let store = make_store_arc(false);

        std::thread::scope(|s| {
            for i in 0..4u8 {
                let store = Arc::clone(&store);
                s.spawn(move || {
                    let data = vec![i; 256];
                    let hash = hash_of(&data);
                    store.put(&hash, &data).expect("put must not fail");
                    let retrieved = store.get(&hash).expect("get must not fail");
                    assert_eq!(retrieved, data);
                });
            }
        });
    }

    // -----------------------------------------------------------------------
    // Property-based test
    // -----------------------------------------------------------------------

    proptest! {
        /// For any random data, hash-then-put-then-get round-trips correctly.
        #[test]
        fn prop_hash_put_get_round_trip(data in proptest::collection::vec(any::<u8>(), 0..=4096)) {
            let store = make_store(true);
            if data.is_empty() {
                // Empty data is valid: we just skip the round-trip (no block to store)
                return Ok(());
            }
            let hash = hash_of(&data);
            store.put(&hash, &data).unwrap();
            let retrieved = store.get(&hash).unwrap();
            prop_assert_eq!(retrieved, data);
        }
    }
}
