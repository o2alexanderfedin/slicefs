use std::path::{Path, PathBuf};
use std::time::Duration;

/// Three durability tiers per ARCHITECTURE §13.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurabilityMode {
    /// `Durability::None`; bulk seed only — reload from CAS on crash.
    Seed,
    /// `Durability::Eventual` + 200 ms group-commit — daily driver.
    Default,
    /// `Durability::Immediate` per insert (one F_FULLFSYNC each).
    Paranoid,
}

#[derive(Debug, Clone, Copy)]
pub struct BloomConfig {
    pub capacity: usize,
    pub fpr: f64,
    pub snapshot_every: usize,
    pub snapshot_interval: Duration,
    pub stale_ratio: f64,
    pub drift_rebuild_ratio: f64,
    pub effective_fpr_rebuild_multiplier: f64,
}

impl BloomConfig {
    pub fn for_mode(mode: DurabilityMode) -> Self {
        match mode {
            DurabilityMode::Seed => Self {
                capacity: 100_000_000,
                fpr: 0.01,
                snapshot_every: usize::MAX,
                snapshot_interval: Duration::from_secs(u64::MAX / 2),
                stale_ratio: 0.90,
                drift_rebuild_ratio: 0.20,
                effective_fpr_rebuild_multiplier: 4.0,
            },
            DurabilityMode::Default => Self {
                capacity: 100_000_000,
                fpr: 0.01,
                snapshot_every: 100_000,
                snapshot_interval: Duration::from_secs(600),
                stale_ratio: 0.90,
                drift_rebuild_ratio: 0.20,
                effective_fpr_rebuild_multiplier: 4.0,
            },
            DurabilityMode::Paranoid => Self {
                capacity: 100_000_000,
                fpr: 0.01,
                snapshot_every: 10_000,
                snapshot_interval: Duration::from_secs(60),
                stale_ratio: 0.90,
                drift_rebuild_ratio: 0.10,
                effective_fpr_rebuild_multiplier: 2.0,
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct DedupIndexConfig {
    pub cas_root: PathBuf,
    pub dedup_root: PathBuf,
    pub durability: DurabilityMode,
    pub bloom: BloomConfig,
    pub batcher_coalesce_window: Duration,
    pub redb_group_commit_window: Duration,
    pub batch_size_default: usize,
    pub batch_size_seed: usize,
    pub mpsc_capacity: usize,
    pub verify_on_present: bool,
    pub use_f_fullfsync: bool,
    pub redb_cache_bytes: usize,
    pub page_size: usize,
}

impl DedupIndexConfig {
    pub fn builder(cas_root: impl AsRef<Path>) -> DedupIndexConfigBuilder {
        DedupIndexConfigBuilder::new(cas_root.as_ref().to_path_buf())
    }
}

pub struct DedupIndexConfigBuilder {
    cas_root: PathBuf,
    durability: DurabilityMode,
}

impl DedupIndexConfigBuilder {
    pub fn new(cas_root: PathBuf) -> Self {
        Self {
            cas_root,
            durability: DurabilityMode::Default,
        }
    }

    pub fn mode(mut self, mode: DurabilityMode) -> Self {
        self.durability = mode;
        self
    }

    pub fn build(self) -> DedupIndexConfig {
        let dedup_root = self.cas_root.join(".dedup-index");
        let (coalesce, batch_default, batch_seed, verify_on_present) = match self.durability {
            DurabilityMode::Seed => (Duration::from_millis(20), 10_000, 100_000, false),
            DurabilityMode::Default => (Duration::from_millis(2), 10_000, 100_000, false),
            DurabilityMode::Paranoid => (Duration::ZERO, 1, 1, true),
        };
        DedupIndexConfig {
            cas_root: self.cas_root,
            dedup_root,
            durability: self.durability,
            bloom: BloomConfig::for_mode(self.durability),
            batcher_coalesce_window: coalesce,
            redb_group_commit_window: Duration::from_millis(200),
            batch_size_default: batch_default,
            batch_size_seed: batch_seed,
            mpsc_capacity: 16_384,
            verify_on_present,
            use_f_fullfsync: true, // I10 — never disabled
            redb_cache_bytes: 256 * 1024 * 1024,
            page_size: 4096,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_mode_has_2ms_coalesce() {
        let c = DedupIndexConfig::builder("/x").build();
        assert_eq!(c.batcher_coalesce_window, Duration::from_millis(2));
        assert!(!c.verify_on_present);
    }

    #[test]
    fn paranoid_enables_verify_on_present() {
        let c = DedupIndexConfig::builder("/x")
            .mode(DurabilityMode::Paranoid)
            .build();
        assert!(c.verify_on_present);
        assert_eq!(c.batch_size_default, 1);
    }

    #[test]
    fn seed_uses_large_batches() {
        let c = DedupIndexConfig::builder("/x")
            .mode(DurabilityMode::Seed)
            .build();
        assert_eq!(c.batch_size_seed, 100_000);
    }

    #[test]
    fn dedup_root_is_under_cas() {
        let c = DedupIndexConfig::builder("/store/cas").build();
        assert_eq!(
            c.dedup_root,
            std::path::Path::new("/store/cas/.dedup-index")
        );
    }
}
