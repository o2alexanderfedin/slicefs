//! `gc` subcommand — offline garbage collection for a SliceFS block store.
//!
//! ## Usage
//!
//! ```text
//! slicefs gc [--full] <store>
//! ```
//!
//! `--full` runs a full GC: an exact mark in sorted temp files and no
//! holes, so every dead object goes (see `ChunkStore::gc_finish_full`).
//! In both modes the mark walk sorts its keys in temp files
//! (`<store>/gc-tmp`), so its RAM does not grow with the file or snapshot
//! count (#207). Both modes print the disk usage of the store before and
//! after.
//!
//! The command:
//! 1. If the store is mounted (`mount.lock` present), asks the mount to run
//!    the GC (see `online_gc`) and waits for the outcome.
//! 2. Loads the segment files to reconstruct the dictionary and last committed root.
//! 3. Marks every object in the chunk store (`packs/`) that the current
//!    root and the snapshot roots reach: metadata nodes and file content.
//!    Deletes, compacts or punches the packs (#25).
//! 4. Compacts the segment files and prints statistics.
//!
//! On a mounted store, the mount's GC thread does the work.

use std::path::Path;
use std::sync::Arc;

use chunk_store::ChunkStore;
use slicefs_traits::key::ObjectKey;

use metadata::gc::{live_objects, live_objects_exact, GarbageCollector};
use metadata::segment::load_store_from_segments;

/// Bytes of all files under `path`: (apparent size, allocated on disk).
pub fn store_usage(path: &Path) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    let mut sum = (0, 0);
    let mut todo = vec![path.to_path_buf()];
    while let Some(dir) = todo.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let Ok(m) = std::fs::symlink_metadata(e.path()) else { continue };
            if m.is_dir() {
                todo.push(e.path());
            } else if m.is_file() {
                sum.0 += m.len();
                sum.1 += m.blocks() * 512;
            }
        }
    }
    sum
}

/// "store disk usage: before A bytes (B allocated), after C bytes (D allocated)".
pub fn usage_line(before: (u64, u64), after: (u64, u64)) -> String {
    format!(
        "store disk usage: before {} bytes ({} allocated), after {} bytes ({} allocated)",
        before.0, before.1, after.0, after.1
    )
}

/// The counts of a chunk-store GC, in one line (also the online result).
pub fn stats_line(s: &chunk_store::GcStats) -> String {
    let mut line = format!(
        "kept {} objects, removed {} ({} bytes); packs: deleted {}, compacted {}, punched {} ({} bytes freed in holes)",
        s.kept_objects, s.removed_objects, s.removed_bytes, s.packs_deleted, s.packs_compacted, s.packs_punched, s.punched_bytes
    );
    if let Some(d) = s.dead_left {
        line += &format!("; dead left: {d}");
    }
    line
}

/// Run offline garbage collection on the store at `store_path` and print
/// the report.
///
/// # Errors
///
/// Returns an error if:
/// - The store is mounted and no mount takes the request, or its GC fails.
/// - Segment files cannot be read.
/// - No committed root is found in segments.
pub fn run_gc(store_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    run_gc_mode(store_path, false)
}

/// [`run_gc`], or a full GC if `full`.
pub fn run_gc_mode(store_path: &Path, full: bool) -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", gc_report(store_path, full)?);
    Ok(())
}

/// The GC of the chunk store (mark and sweep, a full GC if `full`) on a
/// store that the caller has open and holds the store lock for (`gc_report`,
/// rehash). It takes no lock, opens nothing and does not touch the segments.
/// `roots` must be every live root (the current root and every snapshot
/// root): an object that no root reaches is deleted. A failed mark stops
/// before anything is deleted.
pub fn gc_on_open_store(
    cs: &Arc<ChunkStore>,
    roots: &[ObjectKey],
    full: bool,
) -> Result<chunk_store::GcStats, Box<dyn std::error::Error>> {
    let no_stop = std::sync::atomic::AtomicBool::new(false);
    let content = if full {
        let boundary = cs.gc_begin_full()?;
        let marked = live_objects_exact(cs, roots, &[], &no_stop);
        let ids = match marked {
            Ok(Some(ids)) => ids,
            Ok(None) => {
                cs.gc_abort();
                return Err("GC mark stopped, nothing deleted".into());
            }
            Err(e) => {
                cs.gc_abort();
                return Err(format!("GC mark failed, nothing deleted: {}", e).into());
            }
        };
        cs.gc_finish_full(ids, boundary)
    } else {
        let ids = live_objects(cs, roots, &[], &no_stop)
            .map_err(|e| format!("GC mark failed, nothing deleted: {}", e))?
            .ok_or("GC mark stopped, nothing deleted")?;
        let boundary = cs.gc_begin()?;
        cs.gc_finish(&ids, boundary)
    }
    .map_err(|e| format!("GC of file content failed: {}", e))?;
    Ok(content)
}

/// Run the GC and return its report (the lines that `slicefs gc` prints).
pub fn gc_report(store_path: &Path, full: bool) -> Result<String, Box<dyn std::error::Error>> {
    // An existing store only, then the store header, before anything writes
    // (also a GC request).
    crate::content::require_store(store_path)?;
    crate::header::check(store_path, crate::header::Want::Write)?;
    // Step 1: A mounted store runs the GC itself: ask the mount and wait.
    if store_path.join("mount.lock").exists() {
        let outcome = crate::online_gc::request_gc_mode(store_path, full)?;
        let report = format!("GC (mounted store): {outcome}");
        return if outcome.starts_with("ok") { Ok(report) } else { Err(report.into()) };
    }

    // An offline GC: refused while a read-only mount runs (it holds the
    // store lock shared and makes no mount.lock). Held to the end.
    let _store_lock = crate::header::lock_offline(store_path)?;

    let before = store_usage(store_path);
    let segs_dir = store_path.join("segments");

    // Step 2: Load last committed root from segment files (no Dictionary needed).
    let (root_opt, snapshots) = load_store_from_segments(&segs_dir)
        .map_err(|e| format!("failed to load segments: {}", e))?;

    // Collect roots: current live root + all snapshot roots.
    // Snapshot roots must be included so GC does not reclaim blocks still
    // referenced by a snapshot that predates the most recent commit.
    let mut roots: Vec<_> = root_opt.into_iter().collect();
    for snap in &snapshots {
        roots.push(snap.root);
    }

    // Step 3: Mark and sweep the stored objects. A failed mark (missing
    // object, bad record) stops here, before anything is deleted.
    let mut io = crate::content::store_io(store_path)?;
    let cs = std::sync::Arc::clone(io.store());
    let content = gc_on_open_store(&cs, &roots, full)?;
    cs.close()?;

    let gc = GarbageCollector::new(segs_dir);
    let stats = gc.run_gc(&mut io, &roots)
        .map_err(|e| format!("GC failed: {}", e))?;
    drop(io);
    drop(cs);

    // Step 4: Report statistics.
    Ok(format!(
        "GC{}: {}\n{}\nGC complete: scanned {} entries, removed {}, compacted {} segments",
        if full { " (full)" } else { "" },
        stats_line(&content),
        usage_line(before, store_usage(store_path)),
        stats.entries_scanned, stats.entries_removed, stats.segments_compacted
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use chunk_store::{ChunkStore, Options};
    use metadata::store::DictMetadataStore;
    use metadata::store_io::{put_value, StoreIo};
    use slicefs_traits::metadata::{InodeMeta, MetadataStore};

    use super::gc_on_open_store;

    /// IN-06: the temp files of the mark cannot be written (`gc-tmp` is a
    /// file, so the first spill fails with an I/O error in the middle of
    /// the walk). The GC fails with an I/O error and deletes nothing; with
    /// `gc-tmp` gone, the same GC frees the garbage.
    #[test]
    fn a_sorter_io_error_in_the_mark_fails_the_gc_and_deletes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        // Spills every 3 keys.
        let opts = Options { gc_sort_chunk_ids: 3, gc_merge_fan_in: 2, ..Options::default() };
        let cs = Arc::new(ChunkStore::open(dir.path(), opts).unwrap());
        let m = DictMetadataStore::new(Arc::new(Mutex::new(StoreIo::new("/unused", Arc::clone(&cs)))));
        let r = m.root_ino();
        for i in 0..20 {
            let ino = m.create_inode(&InodeMeta::new_file(0, 0, 0, 0o100644)).unwrap();
            m.link(r, &format!("f{i}"), ino).unwrap();
            let key = put_value(&cs, format!("bytes of f{i}").as_bytes()).unwrap();
            m.set_manifest(ino, &[key]).unwrap();
        }
        m.commit().unwrap();
        for i in 0..10 {
            m.unlink(r, &format!("f{i}")).unwrap();
        }
        let root = m.commit().unwrap();
        cs.sync().unwrap();
        let objects = cs.stats().objects;
        let tmp = dir.path().join("gc-tmp");
        std::fs::write(&tmp, b"not a directory").unwrap();
        for full in [false, true] {
            let e = gc_on_open_store(&cs, &[root], full).unwrap_err().to_string();
            assert!(e.contains("GC mark failed, nothing deleted") && !e.contains("orrupt"), "full {full}: {e}");
            assert_eq!(cs.stats().objects, objects, "full {full}: something was deleted");
        }
        std::fs::remove_file(&tmp).unwrap();
        let st = gc_on_open_store(&cs, &[root], false).unwrap();
        assert!(st.removed_objects > 0, "no garbage to free: {st:?}");
    }
}
