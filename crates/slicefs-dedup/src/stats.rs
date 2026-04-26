use std::sync::atomic::{AtomicU64, Ordering};

/// Trait-level coarse stats: a small, stable shape returned by
/// [`slicefs_traits::DedupIndex::stats`]'s default-impl. Applicable to all
/// `DedupIndex` impls (`MemDedupIndex`, `RedbDedupIndex`, …). Task C1 moves
/// this type into `slicefs-traits` and re-exports it from this crate.
#[derive(Debug, Default, Clone, Copy)]
pub struct IndexStats {
    pub entries: u64,
    pub bloom_load_factor: f64,
    pub redb_free_bytes: u64,
}

#[derive(Debug, Default)]
pub struct StatsCounters {
    pub inserts_total: AtomicU64,
    pub lookups_total: AtomicU64,
    pub bloom_hits_total: AtomicU64,
    pub bloom_false_positives_total: AtomicU64,
    pub commits_total: AtomicU64,
    pub commit_failures_total: AtomicU64,
    pub removes_total: AtomicU64,
    pub backpressure_rejects_total: AtomicU64,
    pub verify_on_present_hits_total: AtomicU64,
    pub bloom_snapshot_failures_total: AtomicU64,
}

impl StatsCounters {
    pub fn inc(&self, c: &AtomicU64) {
        c.fetch_add(1, Ordering::Relaxed);
    }
}

/// Rich, crate-internal stats snapshot produced by
/// `RedbDedupIndex::stats_snapshot`. Includes redb-specific fields
/// (`redb_free_bytes`, `hwm`) and queue-depth instrumentation that don't
/// belong on the trait surface. Use [`IndexStats`] for the trait-level
/// shape.
#[derive(Debug, Default, Clone)]
pub struct StatsSnapshot {
    pub inserts_total: u64,
    pub lookups_total: u64,
    pub bloom_hits_total: u64,
    pub bloom_false_positives_total: u64,
    pub commits_total: u64,
    pub commit_failures_total: u64,
    pub removes_total: u64,
    pub backpressure_rejects_total: u64,
    pub verify_on_present_hits_total: u64,
    pub bloom_snapshot_failures_total: u64,
    pub bloom_load_factor: f64,
    pub redb_free_bytes: u64,
    pub queue_depth: u64,
    pub hwm: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_increment_relaxed() {
        let c = StatsCounters::default();
        c.inserts_total.fetch_add(7, Ordering::Relaxed);
        assert_eq!(c.inserts_total.load(Ordering::Relaxed), 7);
    }
}
