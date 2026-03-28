use crate::{error::CasError, hash::ChunkHash};

/// Configuration for a [`BlockStore`] instance.
#[derive(Debug, Clone)]
pub struct BlockStoreConfig {
    /// Re-hash the block data on every `get()` and compare against the requested hash.
    ///
    /// When `true`, any on-disk corruption that changes block bytes is detected and
    /// returned as [`CasError::IntegrityFailure`]. This satisfies CAS-05.
    ///
    /// Disable only in performance-critical paths where the storage medium provides
    /// its own integrity guarantees (e.g., ZFS, ECC storage).
    pub verify_on_read: bool,
}

impl Default for BlockStoreConfig {
    fn default() -> Self {
        Self {
            verify_on_read: true,
        }
    }
}

/// Pluggable storage backend trait for CAS blocks (CAS-03).
///
/// The FUSE integration path (Phase 3) calls this trait synchronously from fuser's
/// thread-per-request callbacks. The owner's algorithm adapter will implement this
/// trait to bridge the owner's block storage API to the filesystem layer.
///
/// # Design notes
/// - Sync: matches fuser's thread-per-request model and owner's existing sync algorithms.
/// - `&self` (not `&mut self`): implementations use internal synchronization
///   (`Mutex`, `RwLock`) to allow sharing via `Arc<dyn BlockStore>`.
/// - Buffered I/O: whole block as `&[u8]` / `Vec<u8>` — no streaming.
/// - No implementation details (file paths, directory layout) in the trait interface.
///   Those are implementation concerns of `LocalDiskStore` and the owner's adapter.
pub trait BlockStore: Send + Sync {
    /// Persist a block keyed by its hash.
    ///
    /// Idempotent: writing the same hash twice is a no-op. The `data` bytes MUST
    /// match `hash`; callers are responsible for computing the hash correctly before
    /// calling `put`.
    fn put(&self, hash: &ChunkHash, data: &[u8]) -> Result<(), CasError>;

    /// Retrieve a block by its hash.
    ///
    /// When `verify_on_read` is enabled in the store config, re-hashes the returned
    /// bytes and returns [`CasError::IntegrityFailure`] if they do not match `hash`.
    /// This is the primary integrity verification hook for CAS-05.
    fn get(&self, hash: &ChunkHash) -> Result<Vec<u8>, CasError>;

    /// Check whether a block exists without reading its content.
    ///
    /// Used by the dedup pipeline to avoid reading blocks that are only being
    /// checked for existence. Cheaper than `get()` for existence checks.
    fn exists(&self, hash: &ChunkHash) -> Result<bool, CasError>;

    /// Remove a block from the store.
    ///
    /// Called only by the garbage collection engine (Phase 5). Idempotent: deleting
    /// a block that does not exist is not an error.
    fn delete(&self, hash: &ChunkHash) -> Result<(), CasError>;
}
