//! Mark-and-sweep GC engine for SliceFS.
//!
//! This module provides:
//! - [`live_objects`]: the GC mark. Metadata nodes and file content are both
//!   in the chunk store, so the mark is one set of `ObjectId`s, and the sweep
//!   is `ChunkStore::gc_finish`.
//! - `OnlineGc`: the mark on a mounted store
//! - `GarbageCollector`: compacts the metadata segment files

pub mod background;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use chunk_store::{ChunkStore, ExactMark, GcPacer, LiveFilter, NoPacer, ObjectId};
use slicefs_traits::key::{ObjectKey, KEY_BYTES};
use slicefs_traits::metadata::MetaError;

use crate::store::for_each_referenced;
use crate::store_io::{to_id, NodeRead, NodeReader};

use crate::segment::compaction::{compact_segment, SegmentError};

/// The file content keys (chunk store) of `roots`.
pub fn collect_content_keys(io: &mut impl NodeRead, roots: &[ObjectKey]) -> Result<Vec<ObjectKey>, MetaError> {
    let mut keys = Vec::new();
    for root in roots {
        keys.extend(crate::store::content_keys(io, root)?);
    }
    Ok(keys)
}

/// The GC mark: every chunk-store object that the metadata roots
/// `meta_roots` (the current root and each snapshot) reach, through their
/// nodes and file content, and the objects of the file contents
/// `file_roots` (open write handles). `Ok(None)` if `stop` was set first.
///
/// The mark is a Bloom filter of the live IDs (`ChunkStore::live_filter`,
/// about 1.25 bytes for each object of the store), not a set of them
/// (#113): a false positive only keeps a dead object. The walk never asks
/// the filter, so a false positive cannot hide a live subtree.
///
/// Fails if a reachable object is missing: then the caller must not sweep,
/// because the live set is not complete.
pub fn live_objects(
    cs: &Arc<ChunkStore>,
    meta_roots: &[ObjectKey],
    file_roots: &[ObjectKey],
    stop: &AtomicBool,
) -> Result<Option<LiveFilter>, MetaError> {
    live_objects_paced(cs, meta_roots, file_roots, stop, &NoPacer)
}

/// [`live_objects`], with `pacer` called between roots and between files
/// (no lock held then).
pub fn live_objects_paced(
    cs: &Arc<ChunkStore>,
    meta_roots: &[ObjectKey],
    file_roots: &[ObjectKey],
    stop: &AtomicBool,
    pacer: &dyn GcPacer,
) -> Result<Option<LiveFilter>, MetaError> {
    let mut live = cs.live_filter();
    let done = mark(cs, meta_roots, file_roots, stop, pacer, &mut |id| {
        live.insert(&id);
        Ok(())
    })?;
    Ok(done.then_some(live))
}

/// The mark of a full GC: as [`live_objects`], but exact, in sorted temp
/// files of the chunk store (`ChunkStore::exact_mark`), so its RAM does not
/// grow with the object count. Give it to `ChunkStore::gc_finish_full`.
pub fn live_objects_exact(
    cs: &Arc<ChunkStore>,
    meta_roots: &[ObjectKey],
    file_roots: &[ObjectKey],
    stop: &AtomicBool,
) -> Result<Option<ExactMark>, MetaError> {
    live_objects_exact_paced(cs, meta_roots, file_roots, stop, &NoPacer)
}

/// [`live_objects_exact`], with `pacer` called between roots and between
/// files (no lock held then).
pub fn live_objects_exact_paced(
    cs: &Arc<ChunkStore>,
    meta_roots: &[ObjectKey],
    file_roots: &[ObjectKey],
    stop: &AtomicBool,
    pacer: &dyn GcPacer,
) -> Result<Option<ExactMark>, MetaError> {
    let err = |e: chunk_store::Error| MetaError::Corrupted(format!("GC mark: temp file: {e}"));
    let mut live = cs.exact_mark().map_err(err)?;
    let done = mark(cs, meta_roots, file_roots, stop, pacer, &mut |id| live.insert(&id).map_err(err))?;
    Ok(done.then_some(live))
}

/// The walk of a mark: calls `add` with every live object (an object can
/// come more than once). False if `stop` was set first. `pacer` runs before
/// each root and each value (safe points: nothing is deleted during the
/// mark, and the objects put or read meanwhile go to the touched set).
///
/// RAM does not grow with the file or root count (#207): every key of the
/// walk (each root can give the same keys again) goes to an external sorter
/// in `<store>/gc-tmp` (`ChunkStore::gc_sorter`), and the sorted keys
/// without duplicates are read back one at a time. Disk: 33 bytes per key
/// per root, removed at the end, also on an error or a stop.
fn mark(
    cs: &Arc<ChunkStore>,
    meta_roots: &[ObjectKey],
    file_roots: &[ObjectKey],
    stop: &AtomicBool,
    pacer: &dyn GcPacer,
    add: &mut dyn FnMut(ObjectId) -> Result<(), MetaError>,
) -> Result<bool, MetaError> {
    let mut keys = cs
        .gc_sorter::<KEY_BYTES>("keys")
        .map_err(|e| MetaError::Corrupted(format!("GC mark: temp file: {e}")))?;
    for k in file_roots {
        keys.push(k.0).map_err(MetaError::Io)?;
    }
    let mut io = NodeReader(Arc::clone(cs));
    for root in meta_roots {
        pacer.at_safe_point();
        if stop.load(Ordering::Relaxed) {
            return Ok(false);
        }
        for_each_referenced(&mut io, root, &mut |_, k| keys.push(k.0).map_err(MetaError::Io))?;
    }
    let sorted = keys.finish().map_err(MetaError::Io)?;
    let mut values = sorted.reader().map_err(MetaError::Io)?;
    while let Some(rec) = values.next_rec().map_err(MetaError::Io)? {
        let v = ObjectKey(rec);
        pacer.at_safe_point();
        if stop.load(Ordering::Relaxed) {
            return Ok(false);
        }
        let mut failed = None;
        chunk_store::file::for_each_file_object(cs, to_id(&v), |id| {
            if failed.is_none()
                && let Err(e) = add(id)
            {
                failed = Some(e);
            }
        })
        .map_err(|e| MetaError::Corrupted(format!("GC mark: object {v:?}: {e}")))?;
        if let Some(e) = failed {
            return Err(e);
        }
    }
    Ok(true)
}

/// A GC run on a mounted store.
///
/// The caller first calls `ChunkStore::gc_begin` (from then on the chunk
/// store keeps each object that is put or read), then takes the roots (a
/// commit, and a root for each open streaming handle), then creates this
/// value, marks with [`OnlineGc::live`], and ends with
/// `ChunkStore::gc_finish` (or `gc_abort`).
pub struct OnlineGc {
    cs: Arc<ChunkStore>,
    meta_roots: Vec<ObjectKey>,
    file_roots: Vec<ObjectKey>,
}

impl OnlineGc {
    pub fn new(cs: Arc<ChunkStore>, meta_roots: Vec<ObjectKey>, file_roots: Vec<ObjectKey>) -> Self {
        OnlineGc { cs, meta_roots, file_roots }
    }

    /// The mark. `Ok(None)` if `stop` was set first.
    pub fn live(&self, stop: &AtomicBool) -> Result<Option<LiveFilter>, MetaError> {
        live_objects(&self.cs, &self.meta_roots, &self.file_roots, stop)
    }

    /// The exact mark of a full GC (see [`live_objects_exact`]).
    pub fn live_exact(&self, stop: &AtomicBool) -> Result<Option<ExactMark>, MetaError> {
        live_objects_exact(&self.cs, &self.meta_roots, &self.file_roots, stop)
    }

    /// [`OnlineGc::live`], paced (see [`live_objects_paced`]).
    pub fn live_paced(&self, stop: &AtomicBool, pacer: &dyn GcPacer) -> Result<Option<LiveFilter>, MetaError> {
        live_objects_paced(&self.cs, &self.meta_roots, &self.file_roots, stop, pacer)
    }

    /// [`OnlineGc::live_exact`], paced (see [`live_objects_exact_paced`]).
    pub fn live_exact_paced(&self, stop: &AtomicBool, pacer: &dyn GcPacer) -> Result<Option<ExactMark>, MetaError> {
        live_objects_exact_paced(&self.cs, &self.meta_roots, &self.file_roots, stop, pacer)
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

    /// Compact all `.seg` files using only root anchors (no in-memory Dictionary needed).
    ///
    /// Used by the background GC thread. Segment compaction keeps all
    /// `RootUpdate` and `SnapshotRecord` entries; the chunk store has its own
    /// online GC (`gc::OnlineGc`).
    pub fn run_gc_roots_only(&self, _roots: &[ObjectKey]) -> Result<GcStats, GcError> {
        self.run_gc_inner()
    }

    /// Compact all `.seg` files in `segments_dir`. Same as `run_gc_roots_only`;
    /// the stored objects are swept by `ChunkStore::gc_finish`.
    pub fn run_gc(
        &self,
        io: &mut impl crate::store_io::NodeRead,
        roots: &[ObjectKey],
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;

    use chunk_store::file::file_objects;
    use chunk_store::{LiveSet, Options};
    use slicefs_traits::metadata::{InodeMeta, MetadataStore};

    use super::*;
    use crate::store::{content_keys, referenced_roots, DictMetadataStore};
    use crate::store_io::{put_value, StoreIo};

    /// A chunk store whose GC sorter spills every 3 keys and merges 2 runs
    /// at a time, so the mark makes many runs and merge passes.
    fn open(dir: &std::path::Path) -> Arc<ChunkStore> {
        let opts = Options { gc_sort_chunk_ids: 3, gc_merge_fan_in: 2, ..Options::default() };
        Arc::new(ChunkStore::open(dir, opts).unwrap())
    }

    fn add_file(m: &DictMetadataStore, cs: &ChunkStore, parent: u64, name: &str, bytes: &[u8]) {
        let ino = m.create_inode(&InodeMeta::new_file(0, 0, 0, 0o100644)).unwrap();
        m.link(parent, name, ino).unwrap();
        let key = put_value(cs, bytes).unwrap();
        m.set_manifest(ino, &[key]).unwrap();
        m.increment_refcount(&key);
    }

    /// Four roots (one twice) of one tree: shared values, a removed file, a
    /// changed file, an xattr. And a value that only a file root holds.
    fn build(cs: &Arc<ChunkStore>) -> (Vec<ObjectKey>, ObjectKey) {
        let m = DictMetadataStore::new(Arc::new(Mutex::new(StoreIo::new("/unused", Arc::clone(cs)))));
        let r = m.root_ino();
        let sub = m.create_directory(r, "sub", &InodeMeta::new_directory(0, 0, 0, 0o40755)).unwrap();
        for i in 0..20 {
            let parent = if i % 2 == 0 { r } else { sub };
            add_file(&m, cs, parent, &format!("f{i}"), format!("shared {}", i % 5).as_bytes());
        }
        add_file(&m, cs, r, "big", &vec![7u8; 300 << 10]);
        let r1 = m.commit().unwrap();
        add_file(&m, cs, sub, "late", b"late bytes");
        m.unlink(r, "f0").unwrap();
        let r2 = m.commit().unwrap();
        add_file(&m, cs, r, "f0", b"new f0");
        m.set_xattr(r, "user.x", b"v").unwrap();
        let r3 = m.commit().unwrap();
        let open_file = put_value(cs, b"an open file, in no root").unwrap();
        (vec![r3, r1, r2, r1], open_file)
    }

    /// The distinct values of the per-root key lists (the mark before #207).
    fn values(cs: &Arc<ChunkStore>, roots: &[ObjectKey], files: &[ObjectKey]) -> BTreeSet<ObjectKey> {
        let mut io = NodeReader(Arc::clone(cs));
        let mut keys: BTreeSet<ObjectKey> = files.iter().copied().collect();
        for r in roots {
            keys.extend(referenced_roots(&mut io, r).unwrap());
            keys.extend(content_keys(&mut io, r).unwrap());
        }
        keys
    }

    fn tmp_entries(dir: &std::path::Path) -> usize {
        std::fs::read_dir(dir.join("gc-tmp")).map(|d| d.count()).unwrap_or(0)
    }

    /// Counts safe points; sets `stop` at the `at`-th one.
    struct StopAt<'a> {
        n: AtomicUsize,
        at: usize,
        stop: &'a AtomicBool,
    }

    impl GcPacer for StopAt<'_> {
        fn at_safe_point(&self) {
            if self.n.fetch_add(1, Ordering::Relaxed) + 1 == self.at {
                self.stop.store(true, Ordering::Relaxed);
            }
        }
    }

    #[test]
    fn the_streamed_mark_gives_the_live_set_of_the_key_lists() {
        let dir = tempfile::tempdir().unwrap();
        let cs = open(dir.path());
        let (roots, open_file) = build(&cs);
        let mut got = BTreeSet::new();
        let mut adds = 0u64;
        let stop = AtomicBool::new(false);
        let pacer = StopAt { n: AtomicUsize::new(0), at: usize::MAX, stop: &stop };
        let done = mark(&cs, &roots, &[open_file], &stop, &pacer, &mut |id| {
            adds += 1;
            got.insert(id);
            Ok(())
        })
        .unwrap();
        assert!(done);
        let vals = values(&cs, &roots, &[open_file]);
        let want: BTreeSet<ObjectId> = vals.iter().flat_map(|k| file_objects(&cs, to_id(k)).unwrap()).collect();
        assert_eq!(got, want);
        assert!(got.len() > 50, "{} objects", got.len());
        // Each distinct value once: one safe point per root and per value.
        assert_eq!(pacer.n.load(Ordering::Relaxed), roots.len() + vals.len());
        let per_value: usize = vals.iter().map(|k| file_objects(&cs, to_id(k)).unwrap().len()).sum();
        assert_eq!(adds, per_value as u64, "no value is walked twice");
        assert_eq!(tmp_entries(dir.path()), 0, "the temp files are removed");
        // The public marks agree.
        let live = live_objects(&cs, &roots, &[open_file], &stop).unwrap().unwrap();
        assert!(got.iter().all(|id| live.may_contain(id)));
        let exact = live_objects_exact(&cs, &roots, &[open_file], &stop).unwrap().unwrap();
        assert_eq!(exact.inserted(), adds);
        drop(exact);
        assert_eq!(tmp_entries(dir.path()), 0);
    }

    #[test]
    fn an_error_or_a_stop_removes_the_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let cs = open(dir.path());
        let (roots, _) = build(&cs);
        let stop = AtomicBool::new(false);
        let missing = ObjectKey::example(9);
        // A missing file root: the error comes after the walk and the sort.
        assert!(live_objects(&cs, &roots, &[missing], &stop).is_err());
        assert_eq!(tmp_entries(dir.path()), 0, "after an error");
        // A missing root record: the error comes inside the walk.
        assert!(live_objects(&cs, &[roots[0], missing], &[], &stop).is_err());
        assert_eq!(tmp_entries(dir.path()), 0, "after an error in the walk");
        // A stop in the walk, and one in the read-back of the sorted keys.
        for at in [2, roots.len() + 5] {
            let stop = AtomicBool::new(false);
            let pacer = StopAt { n: AtomicUsize::new(0), at, stop: &stop };
            assert!(live_objects_paced(&cs, &roots, &[], &stop, &pacer).unwrap().is_none(), "stop at {at}");
            assert_eq!(tmp_entries(dir.path()), 0, "after a stop at {at}");
        }
    }
}
