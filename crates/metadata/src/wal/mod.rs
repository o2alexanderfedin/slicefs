//! Pluggable WAL (Write-Ahead Log) strategy trait and implementations.
//!
//! The `WalStrategy` trait is the abstraction through which all Dictionary mutations flow.
//! Four implementations cover the full durability/throughput trade-off spectrum:
//!
//! | Strategy     | Survives a crash of the daemon         | Survives a power loss      |
//! |--------------|----------------------------------------|----------------------------|
//! | PerOp        | Each completed request                 | After fsync or commit      |
//! | FlushOnFsync | All but at most the last 4 MiB of records | After fsync or commit   |
//! | Periodic     | As FlushOnFsync                        | After fsync, commit, timer |
//! | NoWal        | None (testing only)                    | None                       |
//!
//! PerOp writes each operation record into the segment file at once (one
//! `write()`, no disk sync). FlushOnFsync and Periodic hold records in RAM,
//! at most `BUFFER_CAP_BYTES`; past it they write them with no sync. After a
//! crash with no fsync, these two modes can bring back a file as a correct
//! first part of its data (#98).

pub mod no_wal;
pub mod per_op;
pub mod flush_on_fsync;
pub mod periodic;
mod buffered;

pub use no_wal::NoWal;
pub use per_op::PerOpWal;
pub use flush_on_fsync::FlushOnFsyncWal;
pub use periodic::PeriodicWal;

use std::path::Path;
use slicefs_traits::key::ObjectKey;
use thiserror::Error;

/// Flush-on-fsync and periodic write their held records to the segment (no
/// fsync) when the records pass this size, so the RAM of the write log stays
/// at most this size (#98).
pub const BUFFER_CAP_BYTES: usize = 4 * 1024 * 1024;

/// A logical WAL entry representing a metadata state change.
#[derive(Debug, Clone)]
pub enum WalEntry {
    /// Update the filesystem root digest.
    RootUpdate { root: ObjectKey },
    /// Persist a snapshot record with version, root, timestamp, and optional name.
    Snapshot { version: u64, root: ObjectKey, created_at: u64, name: Option<String> },
    /// A file system request since the last root (#20 write log). The bytes
    /// are encoded by the file system; the store only keeps them in order.
    /// Not synced on its own: `flush_and_sync` (fsync, commit) makes it
    /// survive a power loss. PerOp writes it into the file at once.
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
    /// Sync each root and snapshot record; write each operation record into
    /// the file at once with no sync.
    PerOp,
    /// Hold up to `BUFFER_CAP_BYTES` of records in RAM; write and sync them
    /// on an explicit `flush_and_sync`.
    FlushOnFsync,
    /// As `FlushOnFsync`, and a timer also syncs every `interval_secs`
    /// (0: no timer).
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
    /// Start a new generation of the log (#108), for a commit whose root is
    /// written later: write the records so far into the current segment
    /// (no disk sync) and go on in a new segment, two ids on, that starts
    /// with a `Cut` record. The id between is kept for the root segment
    /// (`install_root`), so the root sorts after every record of the cut and
    /// before every record after it. Fast: no disk sync. `None`: no log.
    fn cut(&self) -> Result<Option<WalCut>, WalError> {
        Ok(None)
    }
    /// After `cut`, when every object the root refers to is durable: sync
    /// the records logged since the cut (they hold the relog of the open
    /// files), write the root segment with `seed` (the root and the snapshot
    /// records), sync it and the directory, then delete the segments before
    /// it. Holds no lock of the log across a disk sync, so requests can log
    /// meanwhile.
    fn install_root(&self, _cut: &WalCut, _seed: &[WalEntry]) -> Result<(), WalError> {
        Ok(())
    }
}

/// A generation switch of the write log (see [`WalStrategy::cut`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalCut {
    /// Id kept for the root segment of this commit.
    pub root_id: u64,
}

/// Segments that a cut left behind, not yet synced (see [`WalStrategy::cut`]).
/// An fsync after the cut syncs them first, in order, and the directory with
/// the new segment's entry: the log after the cut is good only on top of
/// every record before it.
#[derive(Default)]
pub(crate) struct Behind {
    old: Vec<crate::segment::SegmentWriter>,
    dir: Option<std::path::PathBuf>,
}

impl Behind {
    /// Sync the segments left behind and the directory, then forget them.
    pub(crate) fn settle(&mut self) -> std::io::Result<()> {
        while let Some(w) = self.old.first_mut() {
            w.sync()?;
            self.old.remove(0);
        }
        if let Some(dir) = &self.dir {
            std::fs::File::open(dir)?.sync_all()?;
            self.dir = None;
        }
        Ok(())
    }
}

/// The cut of [`WalStrategy::cut`] on `writer` (segment `loc`): the caller
/// has written its held records into `writer`. Opens the new segment,
/// moves the old writer to `behind` and returns the new location.
pub(crate) fn cut_segment(
    writer: &mut crate::segment::SegmentWriter,
    loc: &mut (std::path::PathBuf, u64),
    behind: &std::sync::Mutex<Behind>,
) -> Result<WalCut, WalError> {
    writer.flush()?;
    let dir = loc.0.parent().ok_or_else(|| std::io::Error::other("segment has no directory"))?.to_path_buf();
    let (old_id, records) = (loc.1, writer.records());
    let new_id = old_id + 2;
    let new_path = dir.join(format!("segment-{:06}.seg", new_id));
    let mut new_writer = crate::segment::SegmentWriter::new(&new_path, new_id)?;
    new_writer.write_entry(&crate::segment::SegmentEntry::Cut { prev_id: old_id, records })?;
    new_writer.flush()?;
    let old = std::mem::replace(writer, new_writer);
    let mut b = behind.lock().unwrap();
    b.old.push(old);
    b.dir = Some(dir);
    *loc = (new_path, new_id);
    Ok(WalCut { root_id: old_id + 1 })
}

/// The second half of [`WalStrategy::install_root`], after the records of
/// the current segment are durable: write segment `cut.root_id` with `seed`,
/// sync it and the directory, delete the segments before it, sync the
/// directory again, and forget the segments left behind (they are deleted).
pub(crate) fn write_root_segment(
    current: &Path,
    cut: &WalCut,
    seed: &[WalEntry],
    behind: &std::sync::Mutex<Behind>,
) -> Result<(), WalError> {
    let dir = current.parent().ok_or_else(|| std::io::Error::other("segment has no directory"))?;
    let path = dir.join(format!("segment-{:06}.seg", cut.root_id));
    let mut writer = crate::segment::SegmentWriter::new(&path, cut.root_id)?;
    for entry in seed {
        writer.write_entry(&per_op::wal_entry_to_segment(entry))?;
    }
    writer.sync()?;
    std::fs::File::open(dir)?.sync_all()?;
    remove_segments_before(dir, cut.root_id)?;
    std::fs::File::open(dir)?.sync_all()?;
    *behind.lock().unwrap() = Behind::default();
    Ok(())
}

/// Delete the segment files of `dir` whose id is lower than `id`.
fn remove_segments_before(dir: &Path, id: u64) -> std::io::Result<()> {
    for e in std::fs::read_dir(dir)?.filter_map(|e| e.ok()) {
        let name = e.file_name().to_string_lossy().to_string();
        let old = name.strip_prefix("segment-").and_then(|n| n.strip_suffix(".seg")).and_then(|n| n.parse::<u64>().ok());
        if matches!(old, Some(o) if o < id) {
            std::fs::remove_file(e.path())?;
        }
    }
    Ok(())
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
    remove_segments_before(dir, new_id)?;
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
        WalConfig::Periodic { interval_secs } => {
            let interval = std::time::Duration::from_secs(interval_secs);
            let wal = PeriodicWal::with_interval(&seg_path, segment_id, interval)?;
            Ok(Box::new(wal))
        }
        WalConfig::NoWal => Ok(Box::new(NoWal)),
    }
}
