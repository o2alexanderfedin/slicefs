//! Persistent on-disk DedupIndex for SliceFS, backed by redb 4.x.
//!
//! See `.planning/research/dedup-index/ARCHITECTURE.md` (the binding
//! spec) and the per-spec docs under
//! `.planning/research/dedup-index/architecture/`.
//!
//! # Panic policy
//! No panic ever crosses the [`slicefs_traits::DedupIndex`] trait
//! boundary. Engine panics are caught and surfaced as
//! [`CasError::Index("engine-panic: …")`]. See ARCHITECTURE §5.3.

mod error;
pub use error::DedupIndexError;

mod config;
pub use config::{BloomConfig, DedupIndexConfig, DedupIndexConfigBuilder, DurabilityMode};

mod stats;
mod verify;
pub use stats::{IndexStats, StatsCounters, StatsSnapshot};
pub use verify::VerifyReport;

mod paths;
pub use paths::DedupRoot;

// platform: durable_sync / fsync_parent_dir consumed by manifest.rs (D3) and
// bloom_snapshot.rs (D4); see ARCHITECTURE §3 I10. Submodules import directly
// via `crate::platform::...`, so no crate-level re-export is needed.
mod platform;

mod manifest;
pub use manifest::Manifest;

mod bloom_snapshot;

mod atomic_bloom;
pub use atomic_bloom::AtomicBloomFilter;

mod redb_dedup_index;
pub use redb_dedup_index::{DEDUP_TABLE, MountState, RedbDedupIndex};

// G1: BatchWriter is crate-internal infrastructure consumed only by
// `RedbDedupIndex` (G2). No public re-export.
mod batch_writer;

// K1: operator-facing recovery — rebuild redb from the CAS shards.
// Surface lives on RedbDedupIndex via `rebuild_from_cas`.
mod recovery;
