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
use slicefs_traits::digest::Digest256;
use thiserror::Error;

/// A logical WAL entry representing a metadata state change.
#[derive(Debug, Clone)]
pub enum WalEntry {
    /// Update the filesystem root digest.
    RootUpdate { root: Digest256 },
    /// Persist a snapshot record with version, root, timestamp, and optional name.
    Snapshot { version: u64, root: Digest256, created_at: u64, name: Option<String> },
    /// A file system request since the last root (#20 write log). The bytes
    /// are encoded by the file system; the store only keeps them in order.
    /// Not synced on its own: `flush_and_sync` (fsync, commit) makes it durable.
    Op { bytes: Vec<u8> },
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
    /// After a commit: continue in a new segment that starts with `seed`
    /// (the new root and all snapshot records), and delete the older
    /// segments, whose records are now in the root or dead (#20). Keeps the
    /// write log at one segment. Default: nothing.
    fn rotate(&self, _seed: &[WalEntry]) -> Result<(), WalError> {
        Ok(())
    }
}

/// Start segment `id + 1` next to `path` with `seed`, sync it and the
/// directory, then delete the older segment files. Returns the new writer,
/// its path and id. On error before the new segment is synced, nothing is
/// deleted (the current segment stays valid).
pub(crate) fn rotate_segment(
    path: &Path,
    id: u64,
    seed: &[WalEntry],
) -> Result<(crate::segment::SegmentWriter, std::path::PathBuf, u64), WalError> {
    let dir = path.parent().ok_or_else(|| std::io::Error::other("segment has no directory"))?;
    let new_id = id + 1;
    let new_path = dir.join(format!("segment-{:06}.seg", new_id));
    let mut writer = crate::segment::SegmentWriter::new(&new_path, new_id)?;
    for entry in seed {
        writer.write_entry(&per_op::wal_entry_to_segment(entry))?;
    }
    writer.sync()?;
    std::fs::File::open(dir)?.sync_all()?;
    for e in std::fs::read_dir(dir)?.filter_map(|e| e.ok()) {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        let old = name.strip_prefix("segment-").and_then(|n| n.strip_suffix(".seg")).and_then(|n| n.parse::<u64>().ok());
        if matches!(old, Some(o) if o < new_id) {
            std::fs::remove_file(&p)?;
        }
    }
    std::fs::File::open(dir)?.sync_all()?;
    Ok((writer, new_path, new_id))
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
