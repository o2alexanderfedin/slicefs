//! `gc` subcommand — offline garbage collection for a SliceFS block store.
//!
//! ## Usage
//!
//! ```text
//! slicefs gc <store>
//! ```
//!
//! The command:
//! 1. If the store is mounted (`mount.lock` present), asks the mount to run
//!    the GC (see `online_gc`) and waits for the outcome.
//! 2. Loads the segment files to reconstruct the dictionary and last committed root.
//! 3. Marks every stored object reachable from the current root and the
//!    snapshot roots, and deletes the other objects in `vt0/` (#25).
//! 4. Compacts the segment files and prints statistics.
//!
//! On a mounted store, the mount's GC thread does the work.

use std::path::Path;

use metadata::gc::{collect_live_set, sweep, GarbageCollector};
use metadata::segment::load_store_from_segments;
use metadata::store_io::StoreIo;

/// Run offline garbage collection on the store at `store_path`.
///
/// # Errors
///
/// Returns an error if:
/// - The store is currently mounted (`mount.lock` present).
/// - Segment files cannot be read.
/// - No committed root is found in segments.
pub fn run_gc(store_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    // Step 1: A mounted store runs the GC itself: ask the mount and wait.
    if store_path.join("mount.lock").exists() {
        let outcome = crate::online_gc::request_gc(store_path)?;
        println!("GC (mounted store): {outcome}");
        return if outcome.starts_with("ok") { Ok(()) } else { Err(outcome.into()) };
    }

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
    let mut io = StoreIo::new(store_path);
    let live = collect_live_set(&mut io, &roots)
        .map_err(|e| format!("GC mark failed, nothing deleted: {}", e))?;
    let swept = sweep(store_path, &live)
        .map_err(|e| format!("GC sweep failed: {}", e))?;
    println!(
        "GC objects: kept {}, removed {} ({} bytes)",
        swept.objects_kept, swept.objects_removed, swept.bytes_removed
    );

    let gc = GarbageCollector::new(segs_dir);
    let stats = gc.run_gc(&mut io, &roots)
        .map_err(|e| format!("GC failed: {}", e))?;

    // Step 4: Report statistics.
    println!(
        "GC complete: scanned {} entries, removed {}, compacted {} segments",
        stats.entries_scanned, stats.entries_removed, stats.segments_compacted
    );

    Ok(())
}
