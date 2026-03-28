use crate::{error::CasError, hash::ChunkHash};

/// Result of a dedup index lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DedupResult {
    /// Block is definitely not present — bloom filter said no.
    ///
    /// No on-disk lookup was needed. Caller can proceed to store the block.
    /// Bloom filters never produce false negatives, so this is authoritative.
    DefinitelyAbsent,

    /// Block is confirmed present in the on-disk index.
    ///
    /// The block already exists in the store; no re-storage needed.
    Present,

    /// Bloom filter said "maybe present" but the on-disk lookup found nothing.
    ///
    /// This is a false positive — a normal occurrence with bloom filters.
    /// Caller should treat this like `DefinitelyAbsent` and store the block.
    Absent,
}

/// On-disk dedup index with bounded memory usage (CAS-07).
///
/// Separates the fast probabilistic bloom-filter pre-check from the authoritative
/// on-disk index lookup. This two-phase design prevents the ZFS DDT problem where
/// the full dedup table must live entirely in RAM.
///
/// # Design notes
/// - `bloom_check` is the fast path: a single bit-test in a compact in-memory structure.
/// - `lookup` is the slow path: an on-disk index read, only called when bloom says "maybe".
/// - `&self` (not `&mut self`): implementations use internal synchronization.
/// - Production implementations MUST persist bloom filter state across process restarts.
///   The `fastbloom` crate supports serde serialization for this purpose.
///
/// # False positive behavior
/// `remove` does NOT update the bloom filter — false positives are tolerated because
/// they cause only an unnecessary `lookup` call, never data corruption.
pub trait DedupIndex: Send + Sync {
    /// Fast probabilistic existence check using the in-memory bloom filter.
    ///
    /// Returns `false` if the hash is definitely absent (no `lookup` needed).
    /// Returns `true` if the hash *may* be present — caller must call `lookup` to confirm.
    ///
    /// Never produces false negatives: if `bloom_check` returns `false`, the hash
    /// is guaranteed to not be in the index.
    fn bloom_check(&self, hash: &ChunkHash) -> bool;

    /// Authoritative on-disk index lookup.
    ///
    /// Returns [`DedupResult::Present`] or [`DedupResult::Absent`].
    /// Never returns [`DedupResult::DefinitelyAbsent`] — that variant is reserved for
    /// the bloom fast path.
    fn lookup(&self, hash: &ChunkHash) -> Result<DedupResult, CasError>;

    /// Record a hash in both the bloom filter and the on-disk index.
    ///
    /// Idempotent: inserting a hash that already exists is a no-op.
    fn insert(&self, hash: &ChunkHash) -> Result<(), CasError>;

    /// Remove a hash from the on-disk index.
    ///
    /// The bloom filter is NOT updated — false positives from removed entries will
    /// cause an unnecessary `lookup` call on the next check, which is acceptable.
    /// Called only by the garbage collection engine (Phase 5).
    fn remove(&self, hash: &ChunkHash) -> Result<(), CasError>;
}
