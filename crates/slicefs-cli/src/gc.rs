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
//!    snapshot roots: metadata objects in `vt0/`, file content in the chunk
//!    store (`packs/`). Deletes the other objects in `vt0/`, and deletes or
//!    compacts the packs (#25).
//! 4. Compacts the segment files and prints statistics.
//!
//! On a mounted store, the mount's GC thread does the work.

use std::path::Path;

use metadata::gc::{collect_content_keys, collect_live_set, sweep, GarbageCollector};
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

    crate::content::check_format(store_path)?;
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
    let cs = crate::content::open(store_path)?;
    let mut ids = std::collections::HashSet::new();
    let keys = collect_content_keys(&mut io, &roots)
        .map_err(|e| format!("GC mark failed, nothing deleted: {}", e))?;
    for k in keys {
        let objs = chunk_store::file::file_objects(&cs, crate::content::to_id(&k))
            .map_err(|e| format!("GC mark failed, nothing deleted: {}", e))?;
        ids.extend(objs);
    }
    let boundary = cs.gc_begin()?;
    let content = cs.gc_finish(&ids, boundary)
        .map_err(|e| format!("GC of file content failed: {}", e))?;
    cs.close()?;
    let swept = sweep(store_path, &live)
        .map_err(|e| format!("GC sweep failed: {}", e))?;
    println!(
        "GC objects: kept {}, removed {} ({} bytes)",
        swept.objects_kept as u64 + content.kept_objects,
        swept.objects_removed as u64 + content.removed_objects,
        swept.bytes_removed + content.removed_bytes
    );
    println!(
        "GC packs: deleted {}, compacted {}, punched {} ({} bytes freed in holes)",
        content.packs_deleted, content.packs_compacted, content.packs_punched, content.punched_bytes
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
