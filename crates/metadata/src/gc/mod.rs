//! Mark-and-sweep GC engine for SliceFS metadata.
//!
//! This module provides:
//! - `collect_live_set` + `sweep`: mark-and-sweep of the stored objects (offline)
//! - `GarbageCollector`: compacts the metadata segment files

pub mod background;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use blockset::{file_storage_mark, Io};
use slicefs_traits::digest::Digest224;
use slicefs_traits::metadata::MetaError;

use crate::store::referenced_roots;

use crate::segment::compaction::{compact_segment, SegmentError};

/// Store path (`vt0/...`) of every object reachable from `roots` (committed
/// roots: the current one and each snapshot). This is the GC mark phase.
///
/// Fails if a reachable object is missing: then the caller must not sweep,
/// because the live set is not complete.
pub fn collect_live_set(io: &mut impl Io, roots: &[Digest224]) -> Result<HashSet<String>, MetaError> {
    let mut live = HashSet::new();
    for root in roots {
        for object in referenced_roots(io, root)? {
            if !file_storage_mark(io, &object, &mut live) {
                return Err(MetaError::Corrupted(format!("missing object under root {:?}", object)));
            }
        }
    }
    Ok(live)
}

/// Result of a sweep.
#[derive(Debug, Default, Clone, Copy)]
pub struct SweepStats {
    pub objects_kept: usize,
    pub objects_removed: usize,
    pub bytes_removed: u64,
}

/// Delete each object in `<store>/vt0/` that is not in `live`. This is the GC
/// sweep phase. Only offline: a mounted store writes objects that no committed
/// root refers to yet.
///
/// Only names in the object format are touched (a 45-character base32 name,
/// with a `.` prefix for internal nodes); other files stay.
pub fn sweep(store: &Path, live: &HashSet<String>) -> std::io::Result<SweepStats> {
    let mut stats = SweepStats::default();
    let dir = store.join("vt0");
    let entries = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(stats),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let b32 = name.strip_prefix('.').unwrap_or(name);
        if b32.len() != 45 || !b32.bytes().all(|b| b.is_ascii_alphanumeric()) {
            continue;
        }
        if live.contains(&format!("vt0/{name}")) {
            stats.objects_kept += 1;
            continue;
        }
        let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
        std::fs::remove_file(entry.path())?;
        stats.objects_removed += 1;
        stats.bytes_removed += len;
    }
    Ok(stats)
}

/// Statistics returned by a GC run.
#[derive(Debug, Default, Clone, Copy)]
pub struct GcStats {
    pub entries_scanned: usize,
    pub entries_removed: usize,
    pub segments_compacted: usize,
}

/// Error type for GC operations.
#[derive(Debug, thiserror::Error)]
pub enum GcError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Segment error: {0}")]
    Segment(#[from] SegmentError),
}

/// GC engine: collects the live set and compacts closed segment files.
pub struct GarbageCollector {
    segments_dir: PathBuf,
}

impl GarbageCollector {
    pub fn new(segments_dir: PathBuf) -> Self {
        GarbageCollector { segments_dir }
    }

    /// Compact all `.seg` files using only root anchors (no in-memory Dictionary needed).
    ///
    /// Used by the background GC thread after migration to file-backed storage.
    /// Segment compaction retains all `RootUpdate` and `SnapshotRecord` entries;
    /// live-set filtering from file storage will be added in a future plan.
    pub fn run_gc_roots_only(&self, _roots: &[Digest224]) -> Result<GcStats, GcError> {
        self.run_gc_inner()
    }

    /// Compact all `.seg` files in `segments_dir`. Same as `run_gc_roots_only`;
    /// the stored objects are swept by [`collect_live_set`] and [`sweep`].
    pub fn run_gc(
        &self,
        io: &mut impl Io,
        roots: &[Digest224],
    ) -> Result<GcStats, GcError> {
        let _ = (io, roots);
        self.run_gc_inner()
    }

    fn run_gc_inner(&self) -> Result<GcStats, GcError> {
        let mut stats = GcStats::default();

        // Enumerate all .seg files in segments_dir
        let entries = match std::fs::read_dir(&self.segments_dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(stats),
            Err(e) => return Err(GcError::Io(e)),
        };

        let seg_paths: Vec<_> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("seg"))
            .collect();

        for seg_path in seg_paths {
            // Determine output segment id from filename (e.g. "segment-007.seg" → 7)
            // For compaction, use a temp directory and then replace.
            let seg_id = parse_segment_id(&seg_path).unwrap_or(0);
            let compact_id = seg_id + 100_000; // temp id for compacted output

            let result = compact_segment(&seg_path, &self.segments_dir, compact_id)?;
            stats.entries_scanned += result.entries_kept + result.entries_removed;
            stats.entries_removed += result.entries_removed;
            stats.segments_compacted += 1;
        }

        Ok(stats)
    }
}

/// Parse segment ID from filename pattern "segment-NNN.seg".
fn parse_segment_id(path: &std::path::Path) -> Option<u64> {
    let stem = path.file_stem()?.to_str()?;
    let suffix = stem.strip_prefix("segment-")?;
    suffix.parse().ok()
}
