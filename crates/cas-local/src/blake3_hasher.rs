//! `Blake3Hasher`: implements `ContentHasher` using the BLAKE3 hash function.

use dedupfs_traits::hash::{ChunkHash, ContentHasher};

/// Stateless BLAKE3 content hasher.
///
/// Implements [`ContentHasher`] using the `blake3` crate. This is the primary
/// hash function used in all in-memory and disk CAS implementations.
///
/// Because [`ContentHasher`] requires `Send + Sync` and this struct has no
/// fields, it is trivially safe to share across threads.
pub struct Blake3Hasher;

impl ContentHasher for Blake3Hasher {
    fn hash(&self, data: &[u8]) -> ChunkHash {
        let hash = blake3::hash(data);
        ChunkHash::from_bytes(hash.as_bytes().to_vec())
    }

    fn algorithm_id(&self) -> &'static str {
        "blake3"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Minimal second hasher: same algorithm, different algorithm_id.
    /// Used to prove trait pluggability (`&dyn ContentHasher` dispatch).
    struct AltHasher;
    impl ContentHasher for AltHasher {
        fn hash(&self, data: &[u8]) -> ChunkHash {
            // Uses blake3 internally but reports a different id — the point is
            // that the caller code accepts `&dyn ContentHasher` without knowing
            // which concrete type is behind the reference.
            let hash = blake3::hash(data);
            ChunkHash::from_bytes(hash.as_bytes().to_vec())
        }
        fn algorithm_id(&self) -> &'static str {
            "alt-hasher"
        }
    }

    fn hash_with(hasher: &dyn ContentHasher, data: &[u8]) -> ChunkHash {
        hasher.hash(data)
    }

    fn algorithm_id_of(hasher: &dyn ContentHasher) -> &'static str {
        hasher.algorithm_id()
    }

    // -----------------------------------------------------------------------
    // Blake3Hasher tests
    // -----------------------------------------------------------------------

    #[test]
    fn deterministic_same_input_same_hash() {
        let h = Blake3Hasher;
        let data = b"hello, world";
        let hash1 = h.hash(data);
        let hash2 = h.hash(data);
        assert_eq!(hash1, hash2, "hash() must be deterministic");
    }

    #[test]
    fn different_inputs_produce_different_hashes() {
        let h = Blake3Hasher;
        let hash_a = h.hash(b"input-a");
        let hash_b = h.hash(b"input-b");
        assert_ne!(hash_a, hash_b, "different inputs must produce different hashes");
    }

    #[test]
    fn empty_input_does_not_panic() {
        let h = Blake3Hasher;
        let hash = h.hash(b"");
        // Just needs to be non-empty — BLAKE3 always produces 32 bytes
        assert!(!hash.as_bytes().is_empty());
    }

    #[test]
    fn algorithm_id_is_blake3() {
        let h = Blake3Hasher;
        assert_eq!(h.algorithm_id(), "blake3");
    }

    #[test]
    fn hash_output_is_32_bytes() {
        let h = Blake3Hasher;
        let hash = h.hash(b"some data");
        assert_eq!(hash.as_bytes().len(), 32, "BLAKE3 hash must be 32 bytes");
    }

    /// Swap test: proves `&dyn ContentHasher` dispatch works — both Blake3Hasher
    /// and AltHasher pass through the same function without any changes to that
    /// function (CAS-01 pluggability).
    #[test]
    fn swap_test_trait_object_dispatch() {
        let blake3: Blake3Hasher = Blake3Hasher;
        let alt: AltHasher = AltHasher;

        // Both pass through the same `hash_with` and `algorithm_id_of` helpers
        // which accept `&dyn ContentHasher` — proving that the concrete type is
        // invisible to the caller.
        let hash_blake3 = hash_with(&blake3, b"payload");
        let hash_alt = hash_with(&alt, b"payload");

        assert_eq!(algorithm_id_of(&blake3), "blake3");
        assert_eq!(algorithm_id_of(&alt), "alt-hasher");

        // Both return valid 32-byte outputs (blake3 under the hood in both cases)
        assert_eq!(hash_blake3.as_bytes().len(), 32);
        assert_eq!(hash_alt.as_bytes().len(), 32);
    }
}
