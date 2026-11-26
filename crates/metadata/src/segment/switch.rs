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

/// The ids of the `segment-<n>.seg` files of `dir`, of any size.
fn segment_ids(dir: &Path) -> std::io::Result<Vec<u64>> {
    let mut ids = Vec::new();
    for e in std::fs::read_dir(dir)?.filter_map(|e| e.ok()) {
        let name = e.file_name().to_string_lossy().to_string();
        let id = name.strip_prefix("segment-").and_then(|n| n.strip_suffix(".seg")).and_then(|n| n.parse::<u64>().ok());
        ids.extend(id);
    }
    Ok(ids)
}

/// The largest id of a `segment-<n>.seg` file of `dir`, of any size (as
/// `next_segment_id` of the mount), or 0.
fn max_segment_id(dir: &Path) -> std::io::Result<u64> {
    Ok(segment_ids(dir)?.into_iter().max().unwrap_or(0))
}

/// The number of `segment-<n>.seg` files of `dir`. More than one after a
/// switch: a crash came before the older segments were removed.
pub fn segment_file_count(dir: &Path) -> std::io::Result<usize> {
    Ok(segment_ids(dir)?.len())
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
    // The ids come from file names: a name of id u64::MAX must not wrap the
    // switch to the lowest id.
    let id = max_segment_id(dir)?.checked_add(1).ok_or_else(|| SegmentError::from(std::io::Error::other("segment id overflow")))?;
    let tmp = dir.join(format!("segment-{id:06}{SWITCH_TMP_SUFFIX}"));
    let fin = dir.join(format!("segment-{id:06}.seg"));
    let write = || -> Result<(), SegmentError> {
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
        Ok(())
    };
    if let Err(e) = write() {
        // Before the rename: the old state stays; the temp file goes (best
        // effort; the next switch removes it too).
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::File::open(dir)?.sync_all()?;
    crate::wal::remove_segments_before(dir, id)?;
    std::fs::File::open(dir)?.sync_all()?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::segment::{load_store_from_segments, scan_store_with_log, SegmentReader};

    fn k(n: u32) -> ObjectKey {
        ObjectKey::example(n)
    }

    fn snap(version: u64, root: ObjectKey, created_at: u64, name: Option<&str>) -> SnapshotEntry {
        SnapshotEntry { version, root, created_at, name: name.map(String::from) }
    }

    fn rec(s: &SnapshotEntry) -> SegmentEntry {
        SegmentEntry::SnapshotRecord { version: s.version, root: s.root, created_at: s.created_at, name: s.name.clone() }
    }

    fn seg(dir: &Path, id: u64) -> PathBuf {
        dir.join(format!("segment-{id:06}.seg"))
    }

    fn write_seg(dir: &Path, id: u64, entries: &[SegmentEntry]) {
        let mut w = SegmentWriter::new(&seg(dir, id), id).unwrap();
        for e in entries {
            w.write_entry(e).unwrap();
        }
        w.sync().unwrap();
    }

    /// Root and snapshots of both loaders (they must agree), and the number
    /// of records in the write log.
    fn load(dir: &Path) -> (Option<ObjectKey>, Vec<SnapshotEntry>, usize) {
        let (root, snaps) = load_store_from_segments(dir).unwrap();
        let (root2, snaps2, log) = scan_store_with_log(dir).unwrap();
        assert_eq!((root, &snaps), (root2, &snaps2), "both loaders agree");
        (root, snaps, log.len())
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> =
            std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_string()).collect();
        v.sort();
        v
    }

    /// Old store: segment 1 has root A, v1 -> A, v2 -> B; segment 2 (a mount
    /// segment) has one Op after that root. Returns the old snapshots.
    fn old_store(dir: &Path) -> Vec<SnapshotEntry> {
        let old = vec![snap(1, k(1), 100, Some("x")), snap(2, k(2), 200, None)];
        write_seg(dir, 1, &[SegmentEntry::RootUpdate { root: k(1) }, rec(&old[0]), rec(&old[1])]);
        write_seg(dir, 2, &[SegmentEntry::Op { bytes: vec![1, 2, 3] }]);
        assert_eq!(load(dir), (Some(k(1)), old.clone(), 1));
        old
    }

    /// The same versions, names and times with the new roots N1 and N2.
    fn new_snaps(old: &[SnapshotEntry]) -> Vec<SnapshotEntry> {
        vec![SnapshotEntry { root: k(11), ..old[0].clone() }, SnapshotEntry { root: k(12), ..old[1].clone() }]
    }

    fn save(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        [1, 2].iter().map(|&i| (seg(dir, i), std::fs::read(seg(dir, i)).unwrap())).collect()
    }

    fn restore(saved: &[(PathBuf, Vec<u8>)]) {
        for (p, b) in saved {
            std::fs::write(p, b).unwrap();
        }
    }

    #[test]
    fn both_loaders_keep_the_last_record_of_each_version() {
        let d = tempfile::TempDir::new().unwrap();
        let x = snap(1, k(1), 100, Some("x"));
        let y = snap(2, k(2), 200, None);
        write_seg(d.path(), 1, &[SegmentEntry::RootUpdate { root: k(1) }, rec(&x), rec(&y)]);
        let x2 = snap(1, k(3), 100, Some("x"));
        write_seg(d.path(), 2, &[SegmentEntry::RootUpdate { root: k(3) }, rec(&x2)]);
        assert_eq!(load(d.path()), (Some(k(3)), vec![x2, y], 0));
    }

    #[test]
    fn a_switch_makes_the_new_roots_current() {
        let d = tempfile::TempDir::new().unwrap();
        let new = new_snaps(&old_store(d.path()));
        assert_eq!(write_switch_segment(d.path(), k(10), &new).unwrap(), 3);
        assert_eq!(names(d.path()), ["segment-000003.seg"]);
        let records: Vec<_> = SegmentReader::open(&seg(d.path(), 3)).unwrap().collect();
        assert_eq!(records, vec![SegmentEntry::RootUpdate { root: k(10) }, rec(&new[0]), rec(&new[1])]);
        assert_eq!(load(d.path()), (Some(k(10)), new, 0));
    }

    #[test]
    fn a_switch_tmp_before_the_rename_is_ignored() {
        let d = tempfile::TempDir::new().unwrap();
        let old = old_store(d.path());
        let saved = save(d.path());
        write_switch_segment(d.path(), k(10), &new_snaps(&old)).unwrap();
        // A complete temp file (a crash after its sync, before the rename).
        restore(&saved);
        let tmp = d.path().join(format!("segment-000003{SWITCH_TMP_SUFFIX}"));
        std::fs::rename(seg(d.path(), 3), &tmp).unwrap();
        assert_eq!(load(d.path()), (Some(k(1)), old.clone(), 1));
        // A partial temp file (a crash while it is written).
        std::fs::OpenOptions::new().write(true).open(&tmp).unwrap().set_len(20).unwrap();
        assert_eq!(load(d.path()), (Some(k(1)), old, 1));
    }

    #[test]
    fn old_segments_left_after_the_rename_lose_to_the_switch() {
        let d = tempfile::TempDir::new().unwrap();
        let old = old_store(d.path());
        let saved = save(d.path());
        let new = new_snaps(&old);
        write_switch_segment(d.path(), k(10), &new).unwrap();
        // A crash before the removal of the older segments.
        restore(&saved);
        assert_eq!(load(d.path()), (Some(k(10)), new.clone(), 0));
        // A crash after the removal of some of them.
        std::fs::remove_file(seg(d.path(), 1)).unwrap();
        assert_eq!(load(d.path()), (Some(k(10)), new, 0));
    }

    #[test]
    fn a_stale_switch_tmp_is_removed_by_the_next_switch() {
        let d = tempfile::TempDir::new().unwrap();
        let junk = d.path().join("segment-000077.seg.tmp");
        std::fs::write(&junk, b"junk").unwrap();
        std::fs::write(d.path().join("other.tmp"), b"kept").unwrap();
        assert_eq!(remove_switch_tmps(d.path()).unwrap(), 1);
        assert!(!junk.exists());
        assert!(d.path().join("other.tmp").exists());

        let d = tempfile::TempDir::new().unwrap();
        let old = old_store(d.path());
        std::fs::write(d.path().join("segment-000077.seg.tmp"), b"junk").unwrap();
        assert_eq!(write_switch_segment(d.path(), k(10), &new_snaps(&old)).unwrap(), 3);
        assert_eq!(names(d.path()), ["segment-000003.seg"]);
    }

    #[test]
    fn the_switch_id_counts_short_segment_files() {
        let d = tempfile::TempDir::new().unwrap();
        let old = old_store(d.path());
        std::fs::write(seg(d.path(), 9), b"").unwrap();
        assert_eq!(write_switch_segment(d.path(), k(10), &new_snaps(&old)).unwrap(), 10);
        assert_eq!(names(d.path()), ["segment-000010.seg"]);
    }

    #[test]
    fn a_switch_after_the_largest_id_is_an_error() {
        let d = tempfile::TempDir::new().unwrap();
        let old = old_store(d.path());
        std::fs::write(d.path().join(format!("segment-{}.seg", u64::MAX)), b"").unwrap();
        let before = names(d.path());
        let e = write_switch_segment(d.path(), k(10), &new_snaps(&old)).unwrap_err();
        assert!(e.to_string().contains("segment id overflow"), "{e}");
        assert_eq!(names(d.path()), before, "nothing is written or removed");
    }

    #[test]
    fn a_mount_segment_after_the_switch_is_the_write_log() {
        let d = tempfile::TempDir::new().unwrap();
        let new = new_snaps(&old_store(d.path()));
        assert_eq!(write_switch_segment(d.path(), k(10), &new).unwrap(), 3);
        // A mount starts segment 4 with no Cut and logs one request.
        write_seg(d.path(), 4, &[SegmentEntry::Op { bytes: vec![4, 5, 6] }]);
        assert_eq!(load(d.path()), (Some(k(10)), new, 1));
    }
}
