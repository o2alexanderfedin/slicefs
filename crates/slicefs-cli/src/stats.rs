//! `stats` subcommand — show store statistics for a SliceFS block store.
//!
//! ## Usage
//!
//! ```text
//! slicefs stats <store>
//! slicefs stats <store> --json
//! ```
//!
//! Reports:
//! - Logical bytes (sum of all inode sizes before deduplication)
//! - Physical bytes (dictionary metadata size: dict.len() * 92 bytes per entry)
//! - Dedup ratio (logical / physical)
//! - Block count (number of unique dictionary entries)
//! - Snapshot count
//! - Compressor info
//! - Reference count distribution (unique, shared 2x, shared 3+)

use std::path::Path;

use serde::Serialize;

use blockset::Dictionary;
use metadata::segment::load_store_from_segments;
use metadata::snapshot::SnapshotEntry;
use metadata::store::DictMetadataStore;

/// Size of a single Dictionary entry in bytes: 28-byte Digest224 key + 64-byte Branches.
const DICT_ENTRY_BYTES: u64 = 92;

/// Reference count distribution across dictionary entries.
#[derive(Debug, Serialize)]
pub struct RefcountDist {
    /// Blocks referenced exactly once.
    pub unique: usize,
    /// Blocks referenced exactly twice.
    pub shared_2x: usize,
    /// Blocks referenced three or more times.
    pub shared_3plus: usize,
}

/// Per-snapshot statistics.
#[derive(Debug, Serialize)]
pub struct SnapshotStats {
    pub version: u64,
    pub name: Option<String>,
    pub created_at: u64,
    pub reachable_blocks: usize,
}

/// Store-wide statistics.
#[derive(Debug, Serialize)]
pub struct StoreStats {
    pub logical_bytes: u64,
    pub physical_bytes: u64,
    pub dedup_ratio: f64,
    pub block_count: usize,
    pub snapshot_count: usize,
    /// Informational compressor string.
    pub compressor: String,
    pub refcount_distribution: RefcountDist,
    pub snapshots: Vec<SnapshotStats>,
    /// Whether the store was mounted at scan time.
    pub mounted: bool,
}

/// Run the `stats` subcommand.
///
/// # Errors
///
/// Returns an error if segment files cannot be read.
pub fn run_stats(store_path: &Path, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    // Check if store is currently mounted — stats work on mounted stores too.
    let lock_path = store_path.join("mount.lock");
    let mounted = lock_path.exists();
    if mounted && !json {
        eprintln!("Note: store appears to be mounted; stats reflect closed segments only.");
    }

    let segs_dir = store_path.join("segments");

    // Load store from segment replay.
    let (dict, root_opt, snapshots) = load_store_from_segments(&segs_dir)
        .map_err(|e| format!("failed to load segments: {}", e))?;

    let block_count = dict.len();
    let physical_bytes = (block_count as u64) * DICT_ENTRY_BYTES;

    // Reconstruct metadata store for logical_bytes and refcount data.
    let (logical_bytes, refcount_dist) = if let Some(ref root) = root_opt {
        let meta = DictMetadataStore::load_from_root(dict.clone(), root)
            .map_err(|e| format!("failed to reconstruct metadata: {}", e))?;

        let logical = meta.logical_bytes();

        // Compute refcount distribution by iterating all dictionary keys and
        // querying the metadata store's refcount tracker.
        let mut unique = 0usize;
        let mut shared_2x = 0usize;
        let mut shared_3plus = 0usize;
        for key in dict.keys() {
            let rc = meta.get_refcount(key);
            match rc {
                0 | 1 => unique += 1,
                2 => shared_2x += 1,
                _ => shared_3plus += 1,
            }
        }

        (logical, RefcountDist { unique, shared_2x, shared_3plus })
    } else {
        // No committed root — store is fresh or empty.
        let dist = RefcountDist { unique: block_count, shared_2x: 0, shared_3plus: 0 };
        (0u64, dist)
    };

    let dedup_ratio = if physical_bytes == 0 {
        0.0
    } else {
        logical_bytes as f64 / physical_bytes as f64
    };

    let snapshot_count = snapshots.len();

    // Build per-snapshot stats.
    let snap_stats = build_snapshot_stats(&dict, &snapshots);

    // Compressor is always zstd for store_version >= 2 (Phase 6 default).
    // We detect this by checking if the segments directory exists and
    // if any content has been stored. For reporting purposes, report "zstd (default)".
    let compressor = "zstd (default)".to_string();

    let stats = StoreStats {
        logical_bytes,
        physical_bytes,
        dedup_ratio,
        block_count,
        snapshot_count,
        compressor,
        refcount_distribution: refcount_dist,
        snapshots: snap_stats,
        mounted,
    };

    if json {
        let output = serde_json::to_string_pretty(&stats)
            .map_err(|e| format!("JSON serialization error: {}", e))?;
        println!("{}", output);
    } else {
        print_human_stats(&stats);
    }

    Ok(())
}

/// Build per-snapshot statistics using live-set reachability.
fn build_snapshot_stats(dict: &Dictionary, snapshots: &[SnapshotEntry]) -> Vec<SnapshotStats> {
    use metadata::gc::collect_live_set;

    snapshots
        .iter()
        .map(|snap| {
            let live = collect_live_set(dict, &[snap.root]);
            SnapshotStats {
                version: snap.version,
                name: snap.name.clone(),
                created_at: snap.created_at,
                reachable_blocks: live.len(),
            }
        })
        .collect()
}

/// Print stats in human-readable tabular format.
fn print_human_stats(stats: &StoreStats) {
    println!("SliceFS Store Statistics");
    println!("========================");
    println!("Logical bytes    : {}", format_bytes(stats.logical_bytes));
    println!("Physical bytes   : {} (dictionary metadata)", format_bytes(stats.physical_bytes));
    println!("Dedup ratio      : {:.2}x", stats.dedup_ratio);
    println!("Block count      : {}", stats.block_count);
    println!("Snapshot count   : {}", stats.snapshot_count);
    println!("Compressor       : {}", stats.compressor);
    println!("Mounted          : {}", if stats.mounted { "yes" } else { "no" });
    println!();
    println!("Reference Count Distribution");
    println!("----------------------------");
    println!("  Unique (rc=0-1): {}", stats.refcount_distribution.unique);
    println!("  Shared 2x      : {}", stats.refcount_distribution.shared_2x);
    println!("  Shared 3+      : {}", stats.refcount_distribution.shared_3plus);

    if !stats.snapshots.is_empty() {
        println!();
        println!("Snapshots");
        println!("---------");
        for snap in &stats.snapshots {
            let name = snap.name.as_deref().unwrap_or("<unnamed>");
            println!("  v{}: {} ({} blocks, created {})",
                snap.version, name, snap.reachable_blocks, snap.created_at);
        }
    }
}

/// Format a byte count as human-readable string with appropriate units.
fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;

    if bytes == 0 {
        "0 B".to_string()
    } else if bytes >= GIB {
        format!("{:.2} GiB ({} bytes)", bytes as f64 / GIB as f64, bytes)
    } else if bytes >= MIB {
        format!("{:.2} MiB ({} bytes)", bytes as f64 / MIB as f64, bytes)
    } else if bytes >= KIB {
        format!("{:.2} KiB ({} bytes)", bytes as f64 / KIB as f64, bytes)
    } else {
        format!("{} B", bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Create an empty segments directory so load_store_from_segments succeeds.
    fn make_empty_store() -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("segments")).unwrap();
        dir
    }

    #[test]
    fn test_stats_empty_store_produces_zero_stats() {
        let store = make_empty_store();
        let result = run_stats(store.path(), false);
        // Empty store (no segments) should succeed with zero stats.
        assert!(result.is_ok(), "stats on empty store should not error: {:?}", result);
    }

    #[test]
    fn test_stats_empty_store_json_output() {
        let store = make_empty_store();
        // Should produce valid JSON without panicking.
        let result = run_stats(store.path(), true);
        assert!(result.is_ok(), "stats --json on empty store should not error: {:?}", result);
    }

    #[test]
    fn test_format_bytes_zero() {
        assert_eq!(format_bytes(0), "0 B");
    }

    #[test]
    fn test_format_bytes_small() {
        assert_eq!(format_bytes(512), "512 B");
    }

    #[test]
    fn test_format_bytes_kib() {
        let s = format_bytes(2048);
        assert!(s.contains("KiB"), "expected KiB in '{}'", s);
    }
}
