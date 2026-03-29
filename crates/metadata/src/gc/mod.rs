//! Mark-and-sweep GC engine for SliceFS metadata.
//!
//! This module provides:
//! - `collect_live_set`: walk all Dictionary entries reachable from a set of roots
//! - `GarbageCollector`: orchestrates live-set collection + segment compaction

pub mod background;

use std::collections::HashSet;
use std::path::PathBuf;

use blockset::Dictionary;
use slicefs_traits::digest::Digest224;

use crate::segment::compaction::{compact_segment, SegmentError};

/// Walk the Merkle tree in `dict` starting from every root in `roots`,
/// collecting all reachable `Digest224` keys into a `HashSet`.
///
/// Cycle/duplicate protection: if a key is already in the set, it is skipped.
/// Leaf nodes (children whose `Digest256` does not convert to a valid `Digest224`)
/// are silently ignored.
pub fn collect_live_set(dict: &Dictionary, roots: &[Digest224]) -> HashSet<Digest224> {
    let mut live = HashSet::new();
    for &root in roots {
        mark_reachable(dict, root, &mut live);
    }
    live
}

/// Recursively mark `key` and all of its children as reachable.
fn mark_reachable(dict: &Dictionary, key: Digest224, live: &mut HashSet<Digest224>) {
    if !live.insert(key) {
        return; // already visited — prevents infinite loops on shared subtrees
    }
    if let Some(branches) = dict.get(&key) {
        // Branches = [Digest256; 2] — each child is a Digest256.
        // If a child has the hash suffix (is_hash), it's a tree node stored in the Dictionary
        // as a Digest224 key (first 7 words of the Digest256).
        for child256 in branches {
            if let Some(child_key) = blockset::to_digest224(&child256) {
                mark_reachable(dict, child_key, live);
            }
        }
    }
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

    /// Collect the live set from `dict` + `roots`, then compact all `.seg` files in
    /// `segments_dir`.
    ///
    /// Segment files are compacted in place: each segment is replaced by a compacted
    /// version that retains only the entries present in the live set.
    pub fn run_gc(
        &self,
        dict: &Dictionary,
        roots: &[Digest224],
    ) -> Result<GcStats, GcError> {
        let live_set = collect_live_set(dict, roots);
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

            let result = compact_segment(&seg_path, &live_set, &self.segments_dir, compact_id)?;
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
