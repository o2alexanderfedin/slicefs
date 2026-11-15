//! Mark-and-sweep GC engine for SliceFS metadata.
//!
//! This module provides:
//! - `collect_live_set` + `sweep`: mark-and-sweep of the stored objects (offline)
//! - `OnlineGc`: the same on a mounted store
//! - `GarbageCollector`: compacts the metadata segment files

pub mod background;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use blockset::{file_storage_mark, Io};
use slicefs_traits::digest::Digest224;
use slicefs_traits::metadata::MetaError;

use crate::store::referenced_roots;
use crate::store_io::StoreIo;

use crate::segment::compaction::{compact_segment, SegmentError};

/// Store path (`vt0/...`) of every object reachable from `roots` (committed
/// roots: the current one and each snapshot). This is the GC mark phase.
///
/// Fails if a reachable object is missing: then the caller must not sweep,
/// because the live set is not complete.
pub fn collect_live_set(io: &mut impl Io, roots: &[Digest224]) -> Result<HashSet<String>, MetaError> {
    let mut live = HashSet::new();
    mark_into(io, roots, &[], &mut live, &AtomicBool::new(false))?;
    Ok(live)
}

/// Mark from metadata roots (root records) and file roots (open handles).
/// Returns `Ok(false)` if `stop` was set before the mark completed.
fn mark_into(
    io: &mut impl Io,
    meta_roots: &[Digest224],
    file_roots: &[Digest224],
    live: &mut HashSet<String>,
    stop: &AtomicBool,
) -> Result<bool, MetaError> {
    let mut objects = Vec::new();
    for root in meta_roots {
        if stop.load(Ordering::Relaxed) {
            return Ok(false);
        }
        objects.extend(referenced_roots(io, root)?);
    }
    objects.extend_from_slice(file_roots);
    for object in objects {
        if stop.load(Ordering::Relaxed) {
            return Ok(false);
        }
        if !file_storage_mark(io, &object, live) {
            return Err(MetaError::Corrupted(format!("missing object under root {:?}", object)));
        }
    }
    Ok(true)
}

/// Result of a sweep.
#[derive(Debug, Default, Clone, Copy)]
pub struct SweepStats {
    pub objects_kept: usize,
    pub objects_removed: usize,
    pub bytes_removed: u64,
}

/// Names (`vt0/...`) and paths of the files in `<store>/vt0/` that have the
/// object format: a 45-character base32 name, with a `.` prefix for internal
/// nodes. Other files are not listed, so the GC never touches them.
fn object_files(store: &Path) -> std::io::Result<Vec<(String, PathBuf)>> {
    let entries = match std::fs::read_dir(store.join("vt0")) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let b32 = name.strip_prefix('.').unwrap_or(name);
        if b32.len() == 45 && b32.bytes().all(|b| b.is_ascii_alphanumeric()) {
            out.push((format!("vt0/{name}"), entry.path()));
        }
    }
    Ok(out)
}

/// Delete each object in `<store>/vt0/` that is not in `live`. This is the
/// offline GC sweep phase (the store is not mounted). See [`OnlineGc`] for
/// a mounted store.
pub fn sweep(store: &Path, live: &HashSet<String>) -> std::io::Result<SweepStats> {
    let mut stats = SweepStats::default();
    for (name, path) in object_files(store)? {
        if live.contains(&name) {
            stats.objects_kept += 1;
            continue;
        }
        let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        std::fs::remove_file(&path)?;
        stats.objects_removed += 1;
        stats.bytes_removed += len;
    }
    Ok(stats)
}

/// Objects deleted per hold of the `io` lock in an online sweep. Small, so
/// file system requests wait only a short time.
const SWEEP_BATCH: usize = 256;

/// `Io` for the online mark: each read takes the `io` lock alone and does not
/// record the name, so file system requests run between the reads.
struct UntrackedIo<'a>(&'a Mutex<StoreIo>);

impl Io for UntrackedIo<'_> {
    type Args = std::vec::IntoIter<String>;
    fn read(&mut self, name: &str) -> std::io::Result<Vec<u8>> {
        self.0.lock().unwrap().read_untracked(name)
    }
    fn write(&mut self, _name: &str, _data: &[u8]) -> std::io::Result<()> {
        Err(std::io::Error::other("the GC mark does not write"))
    }
    fn args(&self) -> Self::Args {
        Vec::new().into_iter()
    }
    fn print(&mut self, _text: &str) {}
}

/// A GC run on a mounted store.
///
/// The caller first turns on tracking in the store's `StoreIo`, then takes the
/// roots (a commit, and a root for each open streaming handle), then creates
/// this value. From then on:
/// - an object that is reachable from the roots is in the live set (mark);
/// - an object that is read or written after tracking started is touched.
///
/// The sweep deletes an object only if it is neither live nor touched. It
/// checks and deletes under the `io` lock, and each read and write takes that
/// lock too, so no writer can start to depend on an object between the check
/// and the delete.
///
/// Dropping the value turns off the tracking, also after a panic or an early
/// return.
pub struct OnlineGc {
    io: Arc<Mutex<StoreIo>>,
    meta_roots: Vec<Digest224>,
    file_roots: Vec<Digest224>,
}

impl OnlineGc {
    /// `io` must track already (see the type docs).
    pub fn new(io: Arc<Mutex<StoreIo>>, meta_roots: Vec<Digest224>, file_roots: Vec<Digest224>) -> Self {
        OnlineGc { io, meta_roots, file_roots }
    }

    /// The mark phase. `Ok(None)` if `stop` was set first.
    pub fn mark(&self, stop: &AtomicBool) -> Result<Option<HashSet<String>>, MetaError> {
        let mut live = HashSet::new();
        let mut io = UntrackedIo(&self.io);
        let done = mark_into(&mut io, &self.meta_roots, &self.file_roots, &mut live, stop)?;
        Ok(done.then_some(live))
    }

    /// The sweep phase. Stops early (with partial stats) if `stop` is set.
    pub fn sweep(&self, live: &HashSet<String>, stop: &AtomicBool) -> std::io::Result<SweepStats> {
        let store = self.io.lock().unwrap().root().to_path_buf();
        let mut stats = SweepStats::default();
        let mut candidates = Vec::new();
        for (name, path) in object_files(&store)? {
            if live.contains(&name) {
                stats.objects_kept += 1;
            } else {
                candidates.push((name, path));
            }
        }
        for batch in candidates.chunks(SWEEP_BATCH) {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let io = self.io.lock().unwrap();
            for (name, path) in batch {
                if io.is_touched(name) {
                    stats.objects_kept += 1;
                    continue;
                }
                let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                match std::fs::remove_file(path) {
                    Ok(()) => {
                        stats.objects_removed += 1;
                        stats.bytes_removed += len;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(stats)
    }
}

impl Drop for OnlineGc {
    fn drop(&mut self) {
        let mut io = self.io.lock().unwrap_or_else(|e| e.into_inner());
        io.stop_tracking();
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
