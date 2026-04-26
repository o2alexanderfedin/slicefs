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

/// Outcome of a [`DedupIndex::verify`] integrity scan.
///
/// Returned by the scrubber to summarize an on-disk integrity sweep:
/// how many pages were inspected, how many anomalies were found,
/// the bloom filter's current load factor, and how long the scan took.
///
/// `anomalies == 0` means the index is healthy; use [`VerifyReport::ok`]
/// for a boolean check. The default-impl on [`DedupIndex::verify`] returns
/// an empty report (zero pages, zero anomalies, ok), which is correct for
/// in-memory impls that have nothing on disk to verify.
#[derive(Debug, Default, Clone)]
pub struct VerifyReport {
    pub pages_scanned: u64,
    pub anomalies: u64,
    pub bloom_load_factor: f64,
    pub elapsed_ms: u64,
}

impl VerifyReport {
    /// Returns `true` when the verify scan found no anomalies.
    pub fn ok(&self) -> bool {
        self.anomalies == 0
    }
}

/// Trait-level coarse stats: a small, stable shape returned by
/// [`DedupIndex::stats`]'s default-impl. Applicable to all
/// `DedupIndex` impls (`MemDedupIndex`, `RedbDedupIndex`, …).
///
/// Richer, impl-specific snapshots (e.g., `RedbDedupIndex::stats_snapshot`)
/// expose engine-internal counters that don't belong on the trait surface.
#[derive(Debug, Default, Clone, Copy)]
pub struct IndexStats {
    pub entries: u64,
    pub bloom_load_factor: f64,
    pub redb_free_bytes: u64,
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

    /// Force pending writes / bloom snapshot to durable storage.
    ///
    /// In-memory impls have nothing to flush; the default no-op is correct.
    /// Persistent impls must ensure all queued inserts are committed and any
    /// bloom-filter snapshot is written to disk before returning `Ok(())`.
    fn flush(&self) -> Result<(), CasError> {
        Ok(())
    }

    /// On-disk integrity scan. Used by the scrubber.
    ///
    /// Returns a [`VerifyReport`] summarizing pages scanned, anomalies found,
    /// and the bloom load factor. The default-impl returns an empty report,
    /// which is correct for in-memory impls with nothing on disk to verify.
    fn verify(&self) -> Result<VerifyReport, CasError> {
        Ok(VerifyReport::default())
    }

    /// Coarse trait-level stats; richer snapshot on `RedbDedupIndex` directly.
    ///
    /// The default-impl returns a zeroed [`IndexStats`]. Persistent impls
    /// should override to expose live entry counts, bloom load factor, and
    /// free space.
    fn stats(&self) -> IndexStats {
        IndexStats::default()
    }
}
