//! `LocalDiskStore`: flat-file `BlockStore` with 2-byte directory sharding.
//!
//! Blocks are stored at `<root>/<XX>/<YYYYYYYY...>` where `XX` is the first
//! two hex characters of the block hash and `YYYYYYYY...` is the rest.
//!
//! Writes are atomic: data is first written to a temporary `.tmp` file then
//! renamed to its final path, preventing partial writes from appearing as
//! valid blocks.
//!
//! Integrity verification on read (CAS-05): when `BlockStoreConfig::verify_on_read`
//! is `true`, the block bytes are re-hashed after reading and compared to the
//! requested hash. Any discrepancy returns `CasError::IntegrityFailure`.

use std::{
    fs,
    path::PathBuf,
};

use slicefs_traits::{
    block_store::{BlockStore, BlockStoreConfig},
    error::CasError,
    hash::{ChunkHash, ContentHasher},
};

/// A `BlockStore` implementation that persists blocks to disk.
///
/// Files are stored at `<root>/<XX>/<rest>` where `XX` is the first two
/// hex characters of the block hash, providing a 256-way directory sharding
/// that prevents any single directory from becoming a performance bottleneck
/// with large block counts.
pub struct LocalDiskStore {
    root: PathBuf,
    config: BlockStoreConfig,
    hasher: Box<dyn ContentHasher>,
}

impl LocalDiskStore {
    /// Create (or open) a `LocalDiskStore` rooted at `root`.
    ///
    /// Creates the root directory if it does not exist.
    pub fn new(
        root: PathBuf,
        config: BlockStoreConfig,
        hasher: Box<dyn ContentHasher>,
    ) -> Result<Self, CasError> {
        fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            config,
            hasher,
        })
    }

    /// Map a `ChunkHash` to its on-disk path.
    ///
    /// Uses the `Display` impl (`{:02x}` per byte) to guarantee leading zeros
    /// are preserved. Splits at position 2 for the directory prefix.
    fn hash_to_path(&self, hash: &ChunkHash) -> PathBuf {
        let hex = hash.to_string();
        // Ensure the hex string is at least 2 characters; extremely short hashes
        // (edge case: 0-byte or 1-byte hash) are stored under "00".
        let (prefix, rest) = if hex.len() >= 2 {
            (&hex[..2], &hex[2..])
        } else {
            ("00", hex.as_str())
        };
        self.root.join(prefix).join(rest)
    }
}

impl BlockStore for LocalDiskStore {
    fn put(&self, hash: &ChunkHash, data: &[u8]) -> Result<(), CasError> {
        // Verify that the provided hash actually matches the data.
        let computed = self.hasher.hash(data);
        if computed != *hash {
            return Err(CasError::IntegrityFailure {
                expected: hash.clone(),
                actual: computed,
            });
        }

        let path = self.hash_to_path(hash);

        // Idempotent: if the file already exists, skip the write.
        if path.exists() {
            return Ok(());
        }

        // Create the shard directory if needed.
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Atomic write: write to a .tmp file then rename to final path.
        let tmp_path = path.with_extension("tmp");
        fs::write(&tmp_path, data)?;
        fs::rename(&tmp_path, &path)?;

        Ok(())
    }

    fn get(&self, hash: &ChunkHash) -> Result<Vec<u8>, CasError> {
        let path = self.hash_to_path(hash);

        if !path.exists() {
            return Err(CasError::NotFound(hash.clone()));
        }

        let data = fs::read(&path)?;

        if self.config.verify_on_read {
            let computed = self.hasher.hash(&data);
            if computed != *hash {
                return Err(CasError::IntegrityFailure {
                    expected: hash.clone(),
                    actual: computed,
                });
            }
        }

        Ok(data)
    }

    fn exists(&self, hash: &ChunkHash) -> Result<bool, CasError> {
        Ok(self.hash_to_path(hash).exists())
    }

    fn delete(&self, hash: &ChunkHash) -> Result<(), CasError> {
        let path = self.hash_to_path(hash);
        if path.exists() {
            fs::remove_file(&path)?;
        }
        // Idempotent: not-found is not an error.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blake3_hasher::Blake3Hasher;
    use proptest::prelude::*;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn store_with_verify(dir: &TempDir) -> LocalDiskStore {
        LocalDiskStore::new(
            dir.path().to_path_buf(),
            BlockStoreConfig { verify_on_read: true },
            Box::new(Blake3Hasher),
        )
        .expect("failed to create LocalDiskStore")
    }

    fn store_no_verify(dir: &TempDir) -> LocalDiskStore {
        LocalDiskStore::new(
            dir.path().to_path_buf(),
            BlockStoreConfig { verify_on_read: false },
            Box::new(Blake3Hasher),
        )
        .expect("failed to create LocalDiskStore")
    }

    fn valid_hash_and_data() -> (ChunkHash, Vec<u8>) {
        let data = b"hello, cas world".to_vec();
        let hash = Blake3Hasher.hash(&data);
        (hash, data)
    }

    // -----------------------------------------------------------------------
    // Basic put / get / exists / delete
    // -----------------------------------------------------------------------

    #[test]
    fn put_and_get_round_trip() {
        let dir = TempDir::new().unwrap();
        let store = store_with_verify(&dir);
        let (hash, data) = valid_hash_and_data();

        store.put(&hash, &data).expect("put should succeed");
        let retrieved = store.get(&hash).expect("get should succeed");
        assert_eq!(retrieved, data);
    }

    #[test]
    fn exists_returns_true_after_put() {
        let dir = TempDir::new().unwrap();
        let store = store_with_verify(&dir);
        let (hash, data) = valid_hash_and_data();

        assert!(!store.exists(&hash).unwrap(), "should not exist before put");
        store.put(&hash, &data).unwrap();
        assert!(store.exists(&hash).unwrap(), "should exist after put");
    }

    #[test]
    fn get_absent_returns_not_found() {
        let dir = TempDir::new().unwrap();
        let store = store_with_verify(&dir);
        let (hash, _) = valid_hash_and_data();

        let err = store.get(&hash).unwrap_err();
        assert!(
            matches!(err, CasError::NotFound(_)),
            "expected NotFound, got {:?}",
            err
        );
    }

    #[test]
    fn delete_removes_block() {
        let dir = TempDir::new().unwrap();
        let store = store_with_verify(&dir);
        let (hash, data) = valid_hash_and_data();

        store.put(&hash, &data).unwrap();
        store.delete(&hash).expect("delete should succeed");
        let err = store.get(&hash).unwrap_err();
        assert!(matches!(err, CasError::NotFound(_)));
    }

    #[test]
    fn delete_nonexistent_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let store = store_with_verify(&dir);
        let (hash, _) = valid_hash_and_data();

        // Should not return an error
        store.delete(&hash).expect("delete of absent block should succeed");
    }

    #[test]
    fn put_same_hash_twice_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let store = store_with_verify(&dir);
        let (hash, data) = valid_hash_and_data();

        store.put(&hash, &data).unwrap();
        // Second put should succeed without error
        store.put(&hash, &data).expect("second put must be idempotent");
        let retrieved = store.get(&hash).unwrap();
        assert_eq!(retrieved, data);
    }

    // -----------------------------------------------------------------------
    // Integrity checks
    // -----------------------------------------------------------------------

    #[test]
    fn put_with_mismatched_hash_returns_integrity_failure() {
        let dir = TempDir::new().unwrap();
        let store = store_with_verify(&dir);

        let data = b"some data".to_vec();
        let wrong_hash = Blake3Hasher.hash(b"different data");

        let err = store.put(&wrong_hash, &data).unwrap_err();
        assert!(
            matches!(err, CasError::IntegrityFailure { .. }),
            "expected IntegrityFailure, got {:?}",
            err
        );
    }

    #[test]
    fn get_with_verify_on_read_detects_corruption() {
        let dir = TempDir::new().unwrap();
        let store = store_with_verify(&dir);
        let (hash, data) = valid_hash_and_data();

        store.put(&hash, &data).unwrap();

        // Manually overwrite the block file with garbage bytes.
        let path = store.hash_to_path(&hash);
        fs::write(&path, b"corrupted garbage bytes!!!").unwrap();

        let err = store.get(&hash).unwrap_err();
        assert!(
            matches!(err, CasError::IntegrityFailure { .. }),
            "expected IntegrityFailure after corruption, got {:?}",
            err
        );
    }

    #[test]
    fn get_without_verify_on_read_returns_corrupted_data() {
        let dir = TempDir::new().unwrap();
        let store_write = store_with_verify(&dir);
        let store_read = store_no_verify(&dir);
        let (hash, data) = valid_hash_and_data();

        store_write.put(&hash, &data).unwrap();

        // Corrupt the file.
        let path = store_write.hash_to_path(&hash);
        let corrupted = b"corrupted garbage bytes!!!";
        fs::write(&path, corrupted).unwrap();

        // Without verify_on_read, the corrupted bytes are returned as-is.
        let result = store_read.get(&hash).expect("get without verify should not error");
        assert_eq!(result, corrupted.to_vec());
    }

    // -----------------------------------------------------------------------
    // Directory sharding layout
    // -----------------------------------------------------------------------

    #[test]
    fn block_stored_under_two_byte_sharded_path() {
        let dir = TempDir::new().unwrap();
        let store = store_with_verify(&dir);
        let (hash, data) = valid_hash_and_data();

        store.put(&hash, &data).unwrap();

        let hex = hash.to_string();
        let prefix = &hex[..2];
        let rest = &hex[2..];
        let expected_path = dir.path().join(prefix).join(rest);
        assert!(
            expected_path.exists(),
            "block should be stored at {}, but path does not exist",
            expected_path.display()
        );
    }

    #[test]
    fn subdirectory_created_on_first_write() {
        let dir = TempDir::new().unwrap();
        let store = store_with_verify(&dir);
        let (hash, data) = valid_hash_and_data();

        let hex = hash.to_string();
        let shard_dir = dir.path().join(&hex[..2]);
        assert!(!shard_dir.exists(), "shard directory should not exist before put");

        store.put(&hash, &data).unwrap();
        assert!(shard_dir.exists(), "shard directory should exist after put");
    }

    #[test]
    fn leading_zero_byte_hash_no_truncation() {
        // Construct a hash with a leading zero byte (0x00 → "00" prefix).
        // The Display impl uses {:02x} so 0x00 produces "00", not "0".
        let hash = ChunkHash::from_bytes(vec![0x00, 0xab, 0xcd, 0xef]);
        let dir = TempDir::new().unwrap();
        // Use a no-verify store so we can write with an arbitrary hash.
        // We must bypass put()'s integrity check by using the hash of our data.
        // Instead, directly call hash_to_path and verify the path string.
        let store = LocalDiskStore::new(
            dir.path().to_path_buf(),
            BlockStoreConfig { verify_on_read: false },
            Box::new(Blake3Hasher),
        )
        .unwrap();

        let path = store.hash_to_path(&hash);
        let path_str = path.to_string_lossy();

        // The path should contain "00" as the first directory component after root,
        // not "0" (which would indicate truncation of the leading zero).
        assert!(
            path_str.contains("/00/"),
            "leading zero byte should produce '00' shard prefix, got: {}",
            path_str
        );
    }

    // -----------------------------------------------------------------------
    // Property-based round-trip test
    // -----------------------------------------------------------------------

    proptest! {
        /// For any random data, put() followed by get() returns the original bytes.
        #[test]
        fn prop_round_trip(data in proptest::collection::vec(any::<u8>(), 0..=4096)) {
            let dir = TempDir::new().unwrap();
            let store = store_with_verify(&dir);
            let hash = Blake3Hasher.hash(&data);

            store.put(&hash, &data).unwrap();
            let retrieved = store.get(&hash).unwrap();
            prop_assert_eq!(retrieved, data);
        }
    }
}
