//! `gc` subcommand — offline garbage collection for a SliceFS block store.
//!
//! ## Usage
//!
//! ```text
//! slicefs gc [--full] <store>
//! ```
//!
//! `--full` runs a full GC: an exact mark in sorted temp files (bounded
//! RAM) and no holes, so every dead object goes (see
//! `ChunkStore::gc_finish_full`). Both modes print the disk usage of the
//! store before and after.
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
    let no_stop = std::sync::atomic::AtomicBool::new(false);
    let content = if full {
        let boundary = cs.gc_begin_full()?;
        let marked = live_objects_exact(&cs, &roots, &[], &no_stop);
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
        let ids = live_objects(&cs, &roots, &[], &no_stop)
            .map_err(|e| format!("GC mark failed, nothing deleted: {}", e))?
            .ok_or("GC mark stopped, nothing deleted")?;
        let boundary = cs.gc_begin()?;
        cs.gc_finish(&ids, boundary)
    }
    .map_err(|e| format!("GC of file content failed: {}", e))?;
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
