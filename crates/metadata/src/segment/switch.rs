//! The switch: the one atomic step that makes a new root and a new root of
//! every snapshot current (step 4 of `slicefs rehash`).
//!
//! The switch writes one segment with a `RootUpdate` and one
//! `SnapshotRecord` per snapshot (same version, name and `created_at`, new
//! root). It writes the segment under a temp name that ends in
//! [`SWITCH_TMP_SUFFIX`]: such a name fails every segment filter (the
//! loaders, `next_segment_id`, the GC), so a crash before the rename leaves
//! the old state. After the rename and a directory sync the new segment has
//! the highest id; the loaders keep the last record of each snapshot version,
//! so its records win over the older ones. The older segments are then
//! removed; a crash while they go changes nothing.

use std::path::Path;

use slicefs_traits::key::ObjectKey;

use super::{SegmentEntry, SegmentError, SegmentWriter};
use crate::snapshot::SnapshotEntry;

/// Name suffix of a switch segment that is not yet renamed.
pub const SWITCH_TMP_SUFFIX: &str = ".seg.tmp";

/// Remove every `segment-*.seg.tmp` file of `dir` (left by a crashed
/// switch). Returns the number of files removed.
pub fn remove_switch_tmps(dir: &Path) -> std::io::Result<usize> {
    let mut n = 0;
    for e in std::fs::read_dir(dir)?.filter_map(|e| e.ok()) {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with("segment-") && name.ends_with(SWITCH_TMP_SUFFIX) {
            std::fs::remove_file(e.path())?;
            n += 1;
        }
    }
    Ok(n)
}

/// The largest id of a `segment-<n>.seg` file of `dir`, of any size (as
/// `next_segment_id` of the mount), or 0.
fn max_segment_id(dir: &Path) -> std::io::Result<u64> {
    let mut max = 0;
    for e in std::fs::read_dir(dir)?.filter_map(|e| e.ok()) {
        let name = e.file_name().to_string_lossy().to_string();
        let id = name.strip_prefix("segment-").and_then(|n| n.strip_suffix(".seg")).and_then(|n| n.parse::<u64>().ok());
        if let Some(id) = id {
            max = max.max(id);
        }
    }
    Ok(max)
}

/// Make `root` and `snapshots` current in one atomic step; return the id of
/// the new segment.
///
/// Steps: remove stale temp files; write `segment-<max id + 1>.seg.tmp` with
/// one `RootUpdate` and one `SnapshotRecord` per entry of `snapshots`; sync
/// it; rename it to `.seg`; sync the directory; remove every older segment;
/// sync the directory again.
///
/// Preconditions: the caller holds the store lock (nothing else writes the
/// segments), and every object that `root` and the snapshot roots reach is
/// durable (synced) before this call.
pub fn write_switch_segment(dir: &Path, root: ObjectKey, snapshots: &[SnapshotEntry]) -> Result<u64, SegmentError> {
    remove_switch_tmps(dir)?;
    let id = max_segment_id(dir)? + 1;
    let tmp = dir.join(format!("segment-{id:06}{SWITCH_TMP_SUFFIX}"));
    let fin = dir.join(format!("segment-{id:06}.seg"));
    let mut w = SegmentWriter::new(&tmp, id)?;
    w.write_entry(&SegmentEntry::RootUpdate { root })?;
    for s in snapshots {
        w.write_entry(&SegmentEntry::SnapshotRecord {
            version: s.version,
            root: s.root,
            created_at: s.created_at,
            name: s.name.clone(),
        })?;
    }
    w.sync()?;
    drop(w);
    std::fs::rename(&tmp, &fin)?;
    std::fs::File::open(dir)?.sync_all()?;
    crate::wal::remove_segments_before(dir, id)?;
    std::fs::File::open(dir)?.sync_all()?;
    Ok(id)
}
