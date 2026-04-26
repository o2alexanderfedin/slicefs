//! `gc` subcommand — offline garbage collection for a SliceFS block store.
//!
//! ## Usage
//!
//! ```text
//! slicefs gc <store>
//! ```
//!
//! The command:
//! 1. Refuses to run if the store is currently mounted (`mount.lock` present).
//! 2. Loads the segment files to reconstruct the dictionary and last committed root.
//! 3. Runs the GC engine to compact segment files, removing unreachable entries.
//! 4. Prints statistics: entries scanned, entries removed, segments compacted.
//!
//! This command is safe to run when the filesystem is **not** mounted.
//! For in-process GC during active mounts, use the background GC thread
//! spawned by `run_mount`.

use std::path::Path;

use metadata::gc::GarbageCollector;
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
    // Step 1: Refuse to run on a mounted store.
    let lock_path = store_path.join("mount.lock");
    if lock_path.exists() {
        return Err(format!(
            "Store is mounted (mount.lock found at {}). Unmount first or use background GC.",
            lock_path.display()
        )
        .into());
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

    // Step 3: Run GC engine using file-backed Io (Dictionary-free).
    let mut io = StoreIo::new(store_path);
    let gc = GarbageCollector::new(segs_dir);
    let stats = gc
        .run_gc(&mut io, &roots)
        .map_err(|e| format!("GC failed: {}", e))?;

    // Step 4: Report statistics.
    println!(
        "GC complete: scanned {} entries, removed {}, compacted {} segments",
        stats.entries_scanned, stats.entries_removed, stats.segments_compacted
    );

    Ok(())
}
