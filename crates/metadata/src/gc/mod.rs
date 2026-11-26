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

use chunk_store::{ChunkStore, ExactMark, GcPacer, LiveFilter, NoPacer, ObjectId, ID_LEN};
use slicefs_traits::key::{ObjectKey, KEY_BYTES};
use slicefs_traits::metadata::MetaError;

use crate::store::{for_each_referenced, referenced_estimate};
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
    check_temp_space(cs, meta_roots, file_roots, false)?;
    let mut live = cs.live_filter();
    let done = mark(cs, meta_roots, file_roots, &|| stop.load(Ordering::Relaxed), pacer, &mut |id| {
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
    check_temp_space(cs, meta_roots, file_roots, true)?;
    let mut live = cs.exact_mark().map_err(tmp_err)?;
    let stop = || stop.load(Ordering::Relaxed);
    let done = mark(cs, meta_roots, file_roots, &stop, pacer, &mut |id| live.insert(&id).map_err(tmp_err))?;
    Ok(done.then_some(live))
}

/// Before the walk of a mark: error (an I/O error of kind `StorageFull`,
/// nothing deleted) if the temp files of the mark may not fit on the file
/// system of the store (`ChunkStore::gc_temp_space_check`). The estimate:
/// the keys of each root (`referenced_estimate`) at `KEY_BYTES` each, twice
/// (the last merge has its inputs and its output), and for a full GC
/// (`exact`) the IDs of the exact mark at `ID_LEN` each, twice. A sort that
/// fits in RAM (`ChunkStore::gc_sort_chunk_records`) needs nothing. The
/// estimate can be low (a file with many extents); each write of a temp
/// file checks the space again, so the mark stops before the disk is full.
fn check_temp_space(
    cs: &Arc<ChunkStore>,
    meta_roots: &[ObjectKey],
    file_roots: &[ObjectKey],
    exact: bool,
) -> Result<(), MetaError> {
    let mut io = NodeReader(Arc::clone(cs));
    let mut keys = file_roots.len() as u64;
    for root in meta_roots {
        keys = keys.saturating_add(referenced_estimate(&mut io, root)?);
    }
    let ram = cs.gc_sort_chunk_records() as u64;
    let objects = cs.stats().objects;
    let mut need = 0u64;
    if keys > ram {
        need = need.saturating_add((KEY_BYTES as u64).saturating_mul(keys.saturating_add(keys.min(objects))));
    }
    if exact && objects > ram {
        need = need.saturating_add((ID_LEN as u64).saturating_mul(objects).saturating_mul(2));
    }
    cs.gc_temp_space_check(need).map_err(tmp_err)
}

/// An error of the temp files of the mark: an I/O error stays one (a full
/// disk, a removed or read-only `gc-tmp` is not damaged metadata).
fn tmp_err(e: chunk_store::Error) -> MetaError {
    match e {
        chunk_store::Error::Io(e) => MetaError::Io(e),
        e => MetaError::Corrupted(format!("GC mark: temp file: {e}")),
    }
}

/// Keys of the walk of one root between two asks of the stop of the mark.
const WALK_STOP_EVERY: u64 = 1 << 12;

/// The walk of a mark: calls `add` with every live object (an object can
/// come more than once). False if `stop` gave true first: it is asked
/// before each root, every [`WALK_STOP_EVERY`] keys of a root's walk, in
/// the merge passes of the key sort (`Sorter::finish_or_stop`), and before
/// each value of the read-back, so a stop (unmount) does not wait for the
/// sort of a large store. `pacer` runs before each root and each value (safe points: nothing is deleted during the
/// mark, and the objects put or read meanwhile go to the touched set).
///
/// RAM does not grow with the file or root count (#207): every key of the
/// walk (each root can give the same keys again) goes to an external sorter
/// in `<store>/gc-tmp` (`ChunkStore::gc_sorter`), and the sorted keys
/// without duplicates are read back one at a time. Up to
/// `ChunkStore::gc_sort_chunk_records` keys (4 MiB) the sort stays in RAM and
/// makes no file. Disk above that: 33 bytes per key per root, at the last
/// merge up to twice that; removed at the end, also on an error or a stop.
fn mark(
    cs: &Arc<ChunkStore>,
    meta_roots: &[ObjectKey],
    file_roots: &[ObjectKey],
    stop: &dyn Fn() -> bool,
    pacer: &dyn GcPacer,
    add: &mut dyn FnMut(ObjectId) -> Result<(), MetaError>,
) -> Result<bool, MetaError> {
    let mut keys = cs
        .gc_sorter::<KEY_BYTES>("keys")
        .map_err(tmp_err)?;
    for k in file_roots {
        keys.push(k.0).map_err(MetaError::Io)?;
    }
    let mut io = NodeReader(Arc::clone(cs));
    for root in meta_roots {
        pacer.at_safe_point();
        if stop() {
            return Ok(false);
        }
        let (mut n, mut stopped) = (0u64, false);
        let walked = for_each_referenced(&mut io, root, &mut |_, k| {
            n += 1;
            if n.is_multiple_of(WALK_STOP_EVERY) && stop() {
                stopped = true;
                return Err(MetaError::Io(std::io::Error::other("GC stopped")));
            }
            keys.push(k.0).map_err(MetaError::Io)
        });
        if stopped {
            return Ok(false);
        }
        walked?;
    }
    let Some(sorted) = keys.finish_or_stop(stop).map_err(MetaError::Io)? else {
        return Ok(false);
    };
    let mut values = sorted.reader().map_err(MetaError::Io)?;
    while let Some(rec) = values.next_rec().map_err(MetaError::Io)? {
        let v = ObjectKey(rec);
        pacer.at_safe_point();
        if stop() {
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
        let done = mark(&cs, &roots, &[open_file], &|| stop.load(Ordering::Relaxed), &pacer, &mut |id| {
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
    fn a_mark_that_fits_in_ram_makes_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let cs = Arc::new(ChunkStore::open(dir.path(), Options::default()).unwrap());
        let (roots, open_file) = build(&cs);
        /// The most entries of `gc-tmp` at a safe point.
        struct Peek<'a>(&'a std::path::Path, AtomicUsize);
        impl GcPacer for Peek<'_> {
            fn at_safe_point(&self) {
                self.1.fetch_max(tmp_entries(self.0), Ordering::Relaxed);
            }
        }
        let stop = AtomicBool::new(false);
        let pacer = Peek(dir.path(), AtomicUsize::new(0));
        assert!(live_objects_paced(&cs, &roots, &[open_file], &stop, &pacer).unwrap().is_some());
        assert!(live_objects_exact_paced(&cs, &roots, &[open_file], &stop, &pacer).unwrap().is_some());
        assert_eq!(pacer.1.load(Ordering::Relaxed), 0, "a temp file during the mark");
        assert!(!dir.path().join("gc-tmp").exists(), "the temp directory was made");
    }

    /// 1 MiB free of 1 TiB: below the reserve.
    fn no_space(_: &std::path::Path) -> Option<(u64, u64)> {
        Some((1 << 20, 1 << 40))
    }

    fn plenty(_: &std::path::Path) -> Option<(u64, u64)> {
        Some((1 << 39, 1 << 40))
    }

    #[test]
    fn a_mark_without_space_for_its_temp_files_does_not_start() {
        let dir = tempfile::tempdir().unwrap();
        let cs = open(dir.path());
        let (roots, open_file) = build(&cs);
        cs.close().unwrap();
        drop(cs);
        let stop = AtomicBool::new(false);
        for (free, ok) in [(no_space as chunk_store::FreeSpaceFn, false), (plenty, true)] {
            // Spills every 3 keys, so the mark needs temp files.
            let opts = Options { gc_sort_chunk_ids: 3, gc_merge_fan_in: 2, gc_free_space: Some(free), ..Options::default() };
            let cs = Arc::new(ChunkStore::open(dir.path(), opts).unwrap());
            for exact in [false, true] {
                let r = if exact {
                    live_objects_exact(&cs, &roots, &[open_file], &stop).map(|m| m.is_some())
                } else {
                    live_objects(&cs, &roots, &[open_file], &stop).map(|m| m.is_some())
                };
                match r {
                    Ok(done) => assert!(ok && done, "exact {exact}"),
                    Err(MetaError::Io(e)) => {
                        assert!(!ok, "exact {exact}: {e}");
                        assert_eq!(e.kind(), std::io::ErrorKind::StorageFull);
                        assert!(e.to_string().contains("not enough free space for the GC temp files"), "{e}");
                    }
                    Err(e) => panic!("exact {exact}: {e}"),
                }
                assert_eq!(tmp_entries(dir.path()), 0, "temp files left");
            }
            cs.close().unwrap();
        }
    }

    /// A stop in the merge passes of the key sort (unmount): the mark ends
    /// there, removes its temp files, and the GC deletes nothing.
    #[test]
    fn a_stop_in_the_key_merge_ends_the_mark_and_deletes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let cs = open(dir.path());
        let (roots, _) = build(&cs);
        // Only the newest root: the older trees are garbage.
        let roots = &roots[..1];
        let before = cs.stats().objects;
        cs.gc_begin().unwrap();
        // Asked once before the root (and not in its short walk); the next
        // ask is the first of the merge passes.
        let asked = std::cell::Cell::new(0usize);
        let stop = || {
            asked.set(asked.get() + 1);
            asked.get() > roots.len()
        };
        let done = mark(&cs, roots, &[], &stop, &NoPacer, &mut |_| Ok(())).unwrap();
        assert!(!done, "the mark did not stop");
        assert_eq!(asked.get(), roots.len() + 1);
        cs.gc_abort();
        assert_eq!(tmp_entries(dir.path()), 0, "temp files left");
        assert_eq!(cs.stats().objects, before, "nothing is deleted");
        // The same GC without a stop frees the garbage.
        let stop = AtomicBool::new(false);
        let live = live_objects(&cs, roots, &[], &stop).unwrap().unwrap();
        let boundary = cs.gc_begin().unwrap();
        let st = cs.gc_finish(&live, boundary).unwrap();
        assert!(st.removed_objects > 0, "{st:?}");
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
