//! Mark-and-sweep GC engine for SliceFS metadata.
//!
//! This module provides:
//! - `collect_live_set`: simplified stub — FileStorage orphan file cleanup deferred
//! - `GarbageCollector`: orchestrates live-set collection + segment compaction

pub mod background;

use std::collections::HashSet;
use std::path::PathBuf;

use blockset::Io;
use slicefs_traits::digest::Digest224;

use crate::segment::compaction::{compact_segment, SegmentError};

/// Collect the live set of `Digest224` keys reachable from `roots`.
///
/// With FileStorage, batch files are named by their root `Digest224`.
/// Segment compaction no longer needs a live set (no DictEntry to filter).
/// FileStorage orphan file cleanup is deferred to a future phase.
///
/// Returns an empty `HashSet` — all live-set logic is no-op until the
/// FileStorage orphan GC phase is implemented.
pub fn collect_live_set(_io: &mut impl Io, _roots: &[Digest224]) -> HashSet<Digest224> {
    HashSet::new()
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

    /// Collect the live set from `io` + `roots`, then compact all `.seg` files in
    /// `segments_dir`.
    ///
    /// Segment files are compacted in place: each segment is replaced by a compacted
    /// version that retains only the entries present in the live set.
    ///
    /// With FileStorage, `collect_live_set` is a no-op (returns empty set) so
    /// this is equivalent to `run_gc_roots_only`. FileStorage orphan file cleanup
    /// is deferred to a future phase.
    pub fn run_gc(
        &self,
        io: &mut impl Io,
        roots: &[Digest224],
    ) -> Result<GcStats, GcError> {
        let _live_set = collect_live_set(io, roots);
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
