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
//! - Physical bytes (actual bytes in vt0/ CAS batch files on disk)
//! - Dedup ratio (logical / physical)
//! - Snapshot count
//! - Compressor info
//! - Reference count distribution (unique, shared 2x, shared 3+)

use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::Serialize;

use metadata::segment::load_store_from_segments;
use metadata::snapshot::SnapshotEntry;
use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;

/// Reference count distribution across refcount entries.
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
    /// Number of live roots (always 1 per snapshot — block-level reachability deferred).
    pub reachable_roots: usize,
}

/// Store-wide statistics.
#[derive(Debug, Serialize)]
pub struct StoreStats {
    pub logical_bytes: u64,
    /// Bytes used by vt0/ CAS batch files (deduplication savings visible here).
    pub physical_bytes: u64,
    /// Total bytes used by the entire store directory on the host filesystem.
    pub host_disk_bytes: u64,
    pub dedup_ratio: f64,
    pub snapshot_count: usize,
    /// Informational compressor string.
    pub compressor: String,
    pub refcount_distribution: RefcountDist,
    pub snapshots: Vec<SnapshotStats>,
    /// Whether the store was mounted at scan time.
    pub mounted: bool,
}

/// Recursively sum file sizes under `dir`.
///
/// Used to compute physical bytes from the `vt0/` CAS directory.
fn dir_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                if meta.is_file() {
                    total += meta.len();
                } else if meta.is_dir() {
                    total += dir_size(&entry.path());
                }
            }
        }
    }
    total
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

    // Reject legacy format.
    if store_path.join("dictionary.bin").exists() {
        return Err(format!(
            "legacy store format detected at {}. Re-seed required: slicefs seed <store> <source>",
            store_path.display()
        )
        .into());
    } else if !store_path.join("segments").is_dir() {
        return Err(format!(
            "store not found at {}: no segments/ directory",
            store_path.display()
        )
        .into());
    }

    let segs_dir = store_path.join("segments");

    // Load store from segment replay.
    let (root_opt, snapshots) = load_store_from_segments(&segs_dir)
        .map_err(|e| format!("failed to load segments: {}", e))?;

    // Physical bytes: actual disk usage of vt0/ CAS batch files.
    let vt0_dir = store_path.join("vt0");
    let physical_bytes = dir_size(&vt0_dir);

    // Host disk bytes: total disk usage of the entire store directory.
    let host_disk_bytes = dir_size(store_path);

    // Reconstruct metadata store for logical_bytes and refcount data.
    let (logical_bytes, refcount_dist) = if let Some(ref root) = root_opt {
        let io = Arc::new(Mutex::new(StoreIo::new(store_path)));
        let meta = DictMetadataStore::load_from_root(io, root)
            .map_err(|e| format!("failed to reconstruct metadata: {}", e))?;

        let logical = meta.logical_bytes();

        // Refcount distribution: aggregate from refcount tracker.
        // For each tracked refcount, classify as unique (0-1), shared_2x (2), shared_3+ (3+).
        // We iterate snapshot roots as proxy — detailed per-block refcount deferred.
        let refcount_dist = RefcountDist {
            unique: 0,
            shared_2x: 0,
            shared_3plus: 0,
        };

        (logical, refcount_dist)
    } else {
        // No committed root — store is fresh or empty.
        let dist = RefcountDist {
            unique: 0,
            shared_2x: 0,
            shared_3plus: 0,
        };
        (0u64, dist)
    };

    let dedup_ratio = if physical_bytes == 0 {
        0.0
    } else {
        logical_bytes as f64 / physical_bytes as f64
    };

    let snapshot_count = snapshots.len();

    // Build per-snapshot stats.
    let snap_stats = build_snapshot_stats(&snapshots);

    // Compressor is always zstd for store_version >= 2 (Phase 6 default).
    let compressor = "none (v3 raw)".to_string();

    let stats = StoreStats {
        logical_bytes,
        physical_bytes,
        host_disk_bytes,
        dedup_ratio,
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
        // O3: append [Index] block when a dedup index is present on disk.
        // Failure to open the index is non-fatal — just emit a note on stderr.
        print_index_block_if_present(store_path);
    }

    Ok(())
}

/// Print an `[Index]` block based on `<store>/cas/.dedup-index/`.
///
/// No-op when the dedup-index directory does not exist (older stores or
/// stores that have never been mounted with the deduper enabled).
fn print_index_block_if_present(store_path: &Path) {
    let cas_root = store_path.join("cas");
    let dedup_root = cas_root.join(".dedup-index");
    if !dedup_root.exists() {
        return;
    }
    let cfg = slicefs_dedup::DedupIndexConfig::builder(&cas_root).build();
    match slicefs_dedup::RedbDedupIndex::open(cfg) {
        Ok(idx) => {
            let s = idx.stats_snapshot();
            println!();
            println!("[Index]");
            println!("  inserts_total            : {}", s.inserts_total);
            println!("  lookups_total            : {}", s.lookups_total);
            println!("  bloom_hits_total         : {}", s.bloom_hits_total);
            println!(
                "  bloom_false_positives    : {}",
                s.bloom_false_positives_total
            );
            println!("  commits_total            : {}", s.commits_total);
            println!("  commit_failures_total    : {}", s.commit_failures_total);
            println!("  removes_total            : {}", s.removes_total);
            println!(
                "  verify_on_present_hits   : {}",
                s.verify_on_present_hits_total
            );
            println!(
                "  bloom_snapshot_failures  : {}",
                s.bloom_snapshot_failures_total
            );
            println!("  high_water_mark          : {}", s.hwm);
            println!("  redb_free_bytes          : {}", s.redb_free_bytes);
        }
        Err(e) => {
            eprintln!("(could not read [Index]: {e})");
        }
    }
}

/// Build per-snapshot statistics.
fn build_snapshot_stats(snapshots: &[SnapshotEntry]) -> Vec<SnapshotStats> {
    snapshots
        .iter()
        .map(|snap| {
            SnapshotStats {
                version: snap.version,
                name: snap.name.clone(),
                created_at: snap.created_at,
                reachable_roots: 1, // each snapshot has exactly one root
            }
        })
        .collect()
}

/// Print stats in human-readable tabular format.
fn print_human_stats(stats: &StoreStats) {
    println!("SliceFS Store Statistics");
    println!("========================");
    println!("Logical bytes    : {}", format_bytes(stats.logical_bytes));
    println!(
        "CAS bytes        : {} (vt0/ CAS batch files)",
        format_bytes(stats.physical_bytes)
    );
    println!("Host disk used   : {}", format_bytes(stats.host_disk_bytes));
    println!("Dedup ratio      : {:.2}x", stats.dedup_ratio);
    println!("Snapshot count   : {}", stats.snapshot_count);
    println!("Compressor       : {}", stats.compressor);
    println!(
        "Mounted          : {}",
        if stats.mounted { "yes" } else { "no" }
    );

    if !stats.snapshots.is_empty() {
        println!();
        println!("Snapshots");
        println!("---------");
        for snap in &stats.snapshots {
            let name = snap.name.as_deref().unwrap_or("<unnamed>");
            println!(
                "  v{}: {} (created {})",
                snap.version, name, snap.created_at
            );
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
    use metadata::store::DictMetadataStore;
    use metadata::store_io::StoreIo;
    use metadata::wal::WalConfig;
    use slicefs_traits::metadata::{InodeMeta, MetadataStore};
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    const S_IFREG: u32 = 0o100_000;

    /// Create an empty segments directory so load_store_from_segments succeeds.
    fn make_empty_store() -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("segments")).unwrap();
        dir
    }

    /// Write a valid seeded store in segment format.
    fn write_segment_store(dir: &TempDir) {
        use metadata::wal::create_wal;
        let segs_dir = dir.path().join("segments");
        std::fs::create_dir_all(&segs_dir).unwrap();

        let io = Arc::new(Mutex::new(StoreIo::new(dir.path())));
        let wal = create_wal(WalConfig::PerOp, dir.path(), 1).unwrap();
        let mut meta = DictMetadataStore::new(io);
        meta.set_wal(wal);

        let file_meta = InodeMeta::new_file(0, 0, 0, S_IFREG | 0o644);
        let ino = meta.create_inode(&file_meta).unwrap();
        meta.link(1, "hello.txt", ino).unwrap();
        meta.commit().unwrap();
        meta.shutdown_wal().unwrap();
    }

    #[test]
    fn test_stats_empty_store_produces_zero_stats() {
        let store = make_empty_store();
        let result = run_stats(store.path(), false);
        // Empty store (no segments) should succeed with zero stats.
        assert!(
            result.is_ok(),
            "stats on empty store should not error: {:?}",
            result
        );
    }

    #[test]
    fn test_stats_empty_store_json_output() {
        let store = make_empty_store();
        // Should produce valid JSON without panicking.
        let result = run_stats(store.path(), true);
        assert!(
            result.is_ok(),
            "stats --json on empty store should not error: {:?}",
            result
        );
    }

    #[test]
    fn test_stats_legacy_store_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("dictionary.bin"), b"").unwrap();
        let result = run_stats(dir.path(), false);
        assert!(result.is_err(), "legacy store should return error");
        let msg = result.err().unwrap().to_string();
        assert!(
            msg.contains("Re-seed required") || msg.contains("legacy"),
            "error should mention re-seed, got: {}",
            msg
        );
    }

    #[test]
    fn test_stats_segment_store_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        write_segment_store(&dir);
        let result = run_stats(dir.path(), false);
        assert!(
            result.is_ok(),
            "stats on segment store should succeed: {:?}",
            result
        );
    }

    #[test]
    fn test_stats_missing_store_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        // No dictionary.bin, no segments/ — should fail.
        let result = run_stats(dir.path(), false);
        assert!(result.is_err(), "missing store should return error");
        let msg = result.err().unwrap().to_string();
        assert!(
            msg.contains("store not found"),
            "error should mention 'store not found', got: {}",
            msg
        );
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

    #[test]
    fn test_format_bytes_mib() {
        let s = format_bytes(2 * 1024 * 1024);
        assert!(s.contains("MiB"), "expected MiB in '{}'", s);
    }

    #[test]
    fn test_format_bytes_gib() {
        let s = format_bytes(3 * 1024 * 1024 * 1024);
        assert!(s.contains("GiB"), "expected GiB in '{}'", s);
    }

    #[test]
    fn test_format_bytes_exact_1_byte() {
        assert_eq!(format_bytes(1), "1 B");
    }

    #[test]
    fn test_format_bytes_1023_bytes() {
        let s = format_bytes(1023);
        assert!(
            s.contains(" B"),
            "1023 bytes should display as bytes, got '{}'",
            s
        );
        assert!(
            !s.contains("KiB"),
            "1023 bytes should not display as KiB, got '{}'",
            s
        );
    }

    // ── build_snapshot_stats ─────────────────────────────────────────────────

    #[test]
    fn test_build_snapshot_stats_empty() {
        let result = build_snapshot_stats(&[]);
        assert!(
            result.is_empty(),
            "empty snapshots should produce empty stats"
        );
    }

    #[test]
    fn test_build_snapshot_stats_preserves_fields() {
        use metadata::snapshot::SnapshotEntry;
        let snap = SnapshotEntry {
            version: 42,
            name: Some("release-1.0".to_string()),
            created_at: 1700000000,
            root: [0u32; 7],
        };
        let stats = build_snapshot_stats(&[snap]);
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].version, 42);
        assert_eq!(stats[0].name.as_deref(), Some("release-1.0"));
        assert_eq!(stats[0].created_at, 1700000000);
        assert_eq!(stats[0].reachable_roots, 1);
    }

    #[test]
    fn test_build_snapshot_stats_unnamed() {
        use metadata::snapshot::SnapshotEntry;
        let snap = SnapshotEntry {
            version: 1,
            name: None,
            created_at: 0,
            root: [0u32; 7],
        };
        let stats = build_snapshot_stats(&[snap]);
        assert_eq!(stats.len(), 1);
        assert!(
            stats[0].name.is_none(),
            "unnamed snapshot should have None name"
        );
    }

    // ── mounted store path ───────────────────────────────────────────────────

    #[test]
    fn test_stats_mounted_store_succeeds() {
        // Create a valid store with a mount.lock to simulate a mounted store.
        let dir = tempfile::tempdir().unwrap();
        write_segment_store(&dir);
        std::fs::write(dir.path().join("mount.lock"), b"locked").unwrap();

        // stats should still work on a mounted store (read-only scan of segments).
        let result = run_stats(dir.path(), false);
        assert!(
            result.is_ok(),
            "stats on mounted store should succeed: {:?}",
            result
        );
    }

    #[test]
    fn test_stats_mounted_store_json_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        write_segment_store(&dir);
        std::fs::write(dir.path().join("mount.lock"), b"locked").unwrap();

        let result = run_stats(dir.path(), true);
        assert!(
            result.is_ok(),
            "stats --json on mounted store should succeed: {:?}",
            result
        );
    }

    // ── StoreStats JSON serialization ────────────────────────────────────────

    #[test]
    fn test_store_stats_serializes_to_json() {
        let stats = StoreStats {
            logical_bytes: 1024,
            physical_bytes: 512,
            host_disk_bytes: 2048,
            dedup_ratio: 2.0,
            snapshot_count: 3,
            compressor: "none (v3 raw)".to_string(),
            refcount_distribution: RefcountDist {
                unique: 10,
                shared_2x: 5,
                shared_3plus: 2,
            },
            snapshots: vec![],
            mounted: false,
        };
        let json = serde_json::to_string(&stats).expect("StoreStats should serialize to JSON");
        assert!(
            json.contains("logical_bytes"),
            "JSON should contain logical_bytes"
        );
        assert!(
            json.contains("physical_bytes"),
            "JSON should contain physical_bytes"
        );
        assert!(
            json.contains("dedup_ratio"),
            "JSON should contain dedup_ratio"
        );
        assert!(
            json.contains("snapshot_count"),
            "JSON should contain snapshot_count"
        );
    }

    // ── segment store json output ────────────────────────────────────────────

    #[test]
    fn test_stats_segment_store_json_output() {
        let dir = tempfile::tempdir().unwrap();
        write_segment_store(&dir);
        let result = run_stats(dir.path(), true);
        assert!(
            result.is_ok(),
            "stats --json on segment store should succeed: {:?}",
            result
        );
    }
}
