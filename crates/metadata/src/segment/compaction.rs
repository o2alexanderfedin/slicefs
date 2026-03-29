//! Segment compaction — rewrite a segment file keeping only live entries.
//!
//! The compaction is atomic: data is written to a `.tmp` file, synced, then
//! renamed to the final destination.  If the process crashes before the rename,
//! the original segment is untouched.

use std::collections::HashSet;
use std::path::Path;

use slicefs_traits::digest::Digest224;

use super::{SegmentEntry, SegmentReader, SegmentWriter};

/// Error type for segment compaction.
#[derive(Debug, thiserror::Error)]
pub enum SegmentError {
    #[error("I/O error during compaction: {0}")]
    Io(#[from] std::io::Error),
}

/// Result of a segment compaction pass.
#[derive(Debug, Clone, Copy, Default)]
pub struct CompactionResult {
    /// Number of DictEntry records written to the compacted segment.
    pub entries_kept: usize,
    /// Number of DictEntry records omitted (dead/orphaned).
    pub entries_removed: usize,
}

/// Compact `segment_path`, keeping only `DictEntry` records whose key is in
/// `live_set` (all `RootUpdate` records are always kept).
///
/// The compacted segment is written to `output_dir/segment-{output_segment_id}.seg`.
/// Steps:
///   1. Read input segment via `SegmentReader`.
///   2. Write surviving entries to a `.tmp` file via `SegmentWriter`.
///   3. `sync()` the temp file, close it (writes EOF marker).
///   4. `rename` temp file to the final path (atomic on POSIX).
///
/// The original segment is NOT deleted — callers may choose to delete it after
/// verifying the compacted output is durable.
pub fn compact_segment(
    segment_path: &Path,
    live_set: &HashSet<Digest224>,
    output_dir: &Path,
    output_segment_id: u64,
) -> Result<CompactionResult, SegmentError> {
    // Determine final output path
    let out_filename = format!("segment-{:03}.seg", output_segment_id);
    let out_path = output_dir.join(&out_filename);
    let tmp_path = output_dir.join(format!("{}.tmp", out_filename));

    // Read all entries from the input segment
    let reader = SegmentReader::open(segment_path)?;
    let input_entries: Vec<SegmentEntry> = reader.collect();

    // Write surviving entries to temp file
    let mut writer = SegmentWriter::new(&tmp_path, output_segment_id)?;
    let mut result = CompactionResult::default();

    for entry in input_entries {
        match &entry {
            SegmentEntry::DictEntry { key, .. } => {
                if live_set.contains(key) {
                    writer.write_entry(&entry)?;
                    result.entries_kept += 1;
                } else {
                    result.entries_removed += 1;
                }
            }
            SegmentEntry::RootUpdate { .. } => {
                // Always keep root update records
                writer.write_entry(&entry)?;
            }
        }
    }

    // Sync + close (writes EOF marker)
    writer.close()?;

    // Atomic rename: .tmp → final path
    std::fs::rename(&tmp_path, &out_path)?;

    Ok(result)
}
