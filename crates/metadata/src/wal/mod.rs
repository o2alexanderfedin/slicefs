//! Pluggable WAL (Write-Ahead Log) strategy trait and implementations.
//!
//! The `WalStrategy` trait is the abstraction through which all Dictionary mutations flow.
//! Four implementations cover the full durability/throughput trade-off spectrum:
//!
//! | Strategy        | Durability          | Throughput |
//! |-----------------|---------------------|------------|
//! | PerOp           | Per mutation        | Lowest     |
//! | FlushOnFsync    | On explicit fsync   | Medium     |
//! | Periodic        | On timer/shutdown   | Highest    |
//! | NoWal           | None (testing only) | N/A        |

pub mod no_wal;
pub mod per_op;
pub mod flush_on_fsync;
pub mod periodic;

pub use no_wal::NoWal;
pub use per_op::PerOpWal;
pub use flush_on_fsync::FlushOnFsyncWal;
pub use periodic::PeriodicWal;

use std::path::Path;
use slicefs_traits::digest::{Branches, Digest224};
use thiserror::Error;

/// A logical WAL entry representing a single Dictionary mutation.
#[derive(Debug, Clone)]
pub enum WalEntry {
    /// Append or update a DictEntry (key → branches).
    DictionaryAppend { key: Digest224, branches: Branches },
    /// Update the filesystem root digest.
    RootUpdate { root: Digest224 },
}

/// Errors returned by WAL operations.
#[derive(Debug, Error)]
pub enum WalError {
    #[error("segment I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Selects which WAL strategy to use.
#[derive(Debug, Clone)]
pub enum WalConfig {
    /// Durably write each mutation before returning.
    PerOp,
    /// Buffer mutations; flush on explicit `flush_and_sync`.
    FlushOnFsync,
    /// Buffer mutations; flush on a background timer (or at shutdown).
    Periodic { interval_secs: u64 },
    /// No-op WAL (testing / in-memory only).
    NoWal,
}

/// Abstraction over durability strategies for Dictionary mutations.
///
/// All implementations must be `Send + Sync` to allow use behind `Arc`.
pub trait WalStrategy: Send + Sync {
    /// Record a single mutation. May write to disk immediately or buffer it.
    fn log_mutation(&self, entry: &WalEntry) -> Result<(), WalError>;
    /// Flush all buffered mutations to disk and call `sync_all`.
    fn flush_and_sync(&self) -> Result<(), WalError>;
    /// Flush remaining mutations and release all resources.
    fn shutdown(&self) -> Result<(), WalError>;
}

/// Factory: create a boxed `WalStrategy` from a `WalConfig`.
///
/// Segment files are written to `<store_path>/segments/segment-{segment_id:06}.seg`.
/// The `segments/` subdirectory must already exist before calling this function.
pub fn create_wal(
    config: WalConfig,
    store_path: &Path,
    segment_id: u64,
) -> Result<Box<dyn WalStrategy>, WalError> {
    let seg_path = store_path
        .join("segments")
        .join(format!("segment-{:06}.seg", segment_id));
    match config {
        WalConfig::PerOp => {
            let wal = PerOpWal::new(&seg_path, segment_id)?;
            Ok(Box::new(wal))
        }
        WalConfig::FlushOnFsync => {
            let wal = FlushOnFsyncWal::new(&seg_path, segment_id)?;
            Ok(Box::new(wal))
        }
        WalConfig::Periodic { .. } => {
            let wal = PeriodicWal::new(&seg_path, segment_id)?;
            Ok(Box::new(wal))
        }
        WalConfig::NoWal => Ok(Box::new(NoWal)),
    }
}
