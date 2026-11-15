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
//! - Unique content bytes (each distinct file content counted once)
//! - Stored bytes (apparent size of vt0/ objects) and on-disk bytes (allocated blocks)
//! - File dedup ratio (logical / unique content) — whole-file duplicates only
//! - Overall ratio (logical / stored) — all savings, including block-level
//!   dedup inside and across files, minus the object-format cost
//! - Content sharing (distinct file contents referenced by 1, 2, or 3+ files)
//! - Snapshot count

use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::Serialize;

use metadata::segment::load_store_from_segments;
use metadata::snapshot::SnapshotEntry;
use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use slicefs_traits::metadata::{MetaError, MetadataStore};

const S_IFMT: u32 = 0o170_000;
const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;

/// How many files (inodes) share each distinct file content.
#[derive(Debug, Default, Serialize)]
pub struct RefcountDist {
    /// Distinct contents referenced by exactly one file.
    pub unique: usize,
    /// Distinct contents referenced by exactly two files.
    pub shared_2x: usize,
    /// Distinct contents referenced by three or more files.
    pub shared_3plus: usize,
}

/// Content-level view of the live tree (hard links count once).
#[derive(Debug, Default)]
pub struct ContentStats {
    /// Sum of sizes of distinct file contents.
    pub unique_content_bytes: u64,
    /// Sharing distribution of distinct contents.
    pub dist: RefcountDist,
}

/// Walk the live tree from the root and group regular files by content
/// (manifest). Each inode is visited once, so hard links are not double-counted.
pub fn content_stats(meta: &DictMetadataStore) -> Result<ContentStats, MetaError> {
    use std::collections::{HashMap, HashSet};
    let mut seen: HashSet<u64> = HashSet::new();
    let mut by_content: HashMap<Vec<[u32; 7]>, (u64, usize)> = HashMap::new();
    let mut stack = vec![1u64];
    seen.insert(1);
    while let Some(dir) = stack.pop() {
        for entry in meta.list_directory(dir)? {
            if entry.name == "." || entry.name == ".." || !seen.insert(entry.ino) {
                continue;
            }
            let inode = meta.get_inode(entry.ino)?;
            match inode.mode & S_IFMT {
                S_IFDIR => stack.push(entry.ino),
                S_IFREG if inode.size > 0 => {
                    let manifest = meta.get_manifest(entry.ino).unwrap_or_default();
                    let slot = by_content.entry(manifest).or_insert((inode.size, 0));
                    slot.1 += 1;
                }
                _ => {}
            }
        }
    }
    let mut cs = ContentStats::default();
    for (size, refs) in by_content.values() {
        cs.unique_content_bytes += size;
        match refs {
            1 => cs.dist.unique += 1,
            2 => cs.dist.shared_2x += 1,
            _ => cs.dist.shared_3plus += 1,
        }
    }
    Ok(cs)
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
    /// Sum of sizes of distinct file contents in the live tree.
    pub unique_content_bytes: u64,
    /// Apparent size of vt0/ objects.
    pub physical_bytes: u64,
    /// Allocated size of vt0/ objects on the host filesystem (st_blocks).
    pub physical_disk_bytes: u64,
    /// Allocated size of the whole store directory (st_blocks).
    pub host_disk_bytes: u64,
    /// Whole-file dedup: logical / unique content.
    pub file_dedup_ratio: f64,
    /// Everything together: logical / CAS bytes (block-level dedup and the
    /// object-format cost included; > 1.0 means the store is smaller).
    pub overall_ratio: f64,
    pub snapshot_count: usize,
    /// How many files share each distinct content.
    pub refcount_distribution: RefcountDist,
    pub snapshots: Vec<SnapshotStats>,
    /// Whether the store was mounted at scan time.
    pub mounted: bool,
}

/// Recursively sum file sizes under `dir`: `(apparent bytes, allocated bytes)`.
fn dir_size(dir: &Path) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    let (mut apparent, mut allocated) = (0u64, 0u64);
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                if meta.is_file() {
                    apparent += meta.len();
                    allocated += meta.blocks() * 512;
                } else if meta.is_dir() {
                    let (a, b) = dir_size(&entry.path());
                    apparent += a;
                    allocated += b;
                }
            }
        }
    }
    (apparent, allocated)
}

fn ratio(num: u64, den: u64) -> f64 {
    if den == 0 { 0.0 } else { num as f64 / den as f64 }
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
        ).into());
    } else if !store_path.join("segments").is_dir() {
        return Err(format!(
            "store not found at {}: no segments/ directory",
            store_path.display()
        ).into());
    }

    let segs_dir = store_path.join("segments");

    // Load store from segment replay.
    let (root_opt, snapshots) = load_store_from_segments(&segs_dir)
        .map_err(|e| format!("failed to load segments: {}", e))?;

    // Physical bytes: actual disk usage of vt0/ CAS batch files.
    let vt0_dir = store_path.join("vt0");
    let (physical_bytes, physical_disk_bytes) = dir_size(&vt0_dir);

    // Host disk bytes: allocated size of the entire store directory.
    let host_disk_bytes = dir_size(store_path).1;

    // Reconstruct the live tree for logical bytes and content sharing.
    let (logical_bytes, content) = if let Some(ref root) = root_opt {
        let io = Arc::new(Mutex::new(StoreIo::new(store_path)));
        let meta = DictMetadataStore::load_from_root(io, root)
            .map_err(|e| format!("failed to reconstruct metadata: {}", e))?;
        let content = content_stats(&meta)
            .map_err(|e| format!("failed to walk metadata: {}", e))?;
        (meta.logical_bytes(), content)
    } else {
        // No committed root — store is fresh or empty.
        (0u64, ContentStats::default())
    };

    let file_dedup_ratio = ratio(logical_bytes, content.unique_content_bytes);
    let overall_ratio = ratio(logical_bytes, physical_bytes);

    let snapshot_count = snapshots.len();

    // Build per-snapshot stats.
    let snap_stats = build_snapshot_stats(&snapshots);

    let stats = StoreStats {
        logical_bytes,
        unique_content_bytes: content.unique_content_bytes,
        physical_bytes,
        physical_disk_bytes,
        host_disk_bytes,
        file_dedup_ratio,
        overall_ratio,
        snapshot_count,
        refcount_distribution: content.dist,
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
    println!("Unique content   : {} (each distinct file content once)", format_bytes(stats.unique_content_bytes));
    println!("CAS bytes        : {} (vt0/ objects, apparent size)", format_bytes(stats.physical_bytes));
    println!("CAS on disk      : {} (vt0/ objects, allocated blocks)", format_bytes(stats.physical_disk_bytes));
    println!("Host disk used   : {} (whole store, allocated blocks)", format_bytes(stats.host_disk_bytes));
    println!("File dedup ratio : {:.2}x (logical / unique content; whole-file duplicates)", stats.file_dedup_ratio);
    println!("Overall ratio    : {:.2}x (logical / CAS bytes; block dedup and format cost included)", stats.overall_ratio);
    let d = &stats.refcount_distribution;
    println!("Content sharing  : {} unique, {} shared by 2, {} shared by 3+", d.unique, d.shared_2x, d.shared_3plus);
    println!("Snapshot count   : {}", stats.snapshot_count);
    println!("Mounted          : {}", if stats.mounted { "yes" } else { "no" });

    if !stats.snapshots.is_empty() {
        println!();
        println!("Snapshots");
        println!("---------");
        for snap in &stats.snapshots {
            let name = snap.name.as_deref().unwrap_or("<unnamed>");
            println!("  v{}: {} (created {})",
                snap.version, name, snap.created_at);
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
    use tempfile::TempDir;
    use std::sync::{Arc, Mutex};

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
    fn test_content_stats_groups_files_by_content() {
        let dir = tempfile::tempdir().unwrap();
        let io = Arc::new(Mutex::new(StoreIo::new(dir.path())));
        let meta = DictMetadataStore::new(io);
        let d1 = [1u32; 7];
        let d2 = [2u32; 7];
        let mk = |parent: u64, name: &str, size: u64, manifest: &[[u32; 7]]| {
            let mut m = InodeMeta::new_file(0, 0, 0, S_IFREG | 0o644);
            m.size = size;
            let ino = meta.create_inode(&m).unwrap();
            meta.link(parent, name, ino).unwrap();
            meta.set_manifest(ino, manifest).unwrap();
            ino
        };
        mk(1, "a", 100, &[d1]);
        mk(1, "b", 100, &[d1]);
        let c = mk(1, "c", 50, &[d2]);
        meta.link(1, "c-hardlink", c).unwrap(); // same inode: counted once
        mk(1, "empty", 0, &[]);
        let sub = meta.create_directory(1, "sub", &InodeMeta::new_directory(0, 0, 0, 0o040_755)).unwrap();
        mk(sub, "d", 100, &[d1]);

        let cs = content_stats(&meta).unwrap();
        assert_eq!(cs.unique_content_bytes, 150);
        assert_eq!((cs.dist.unique, cs.dist.shared_2x, cs.dist.shared_3plus), (1, 0, 1));
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
        assert!(result.is_ok(), "stats on segment store should succeed: {:?}", result);
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
        assert!(s.contains(" B"), "1023 bytes should display as bytes, got '{}'", s);
        assert!(!s.contains("KiB"), "1023 bytes should not display as KiB, got '{}'", s);
    }

    // ── build_snapshot_stats ─────────────────────────────────────────────────

    #[test]
    fn test_build_snapshot_stats_empty() {
        let result = build_snapshot_stats(&[]);
        assert!(result.is_empty(), "empty snapshots should produce empty stats");
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
        assert!(stats[0].name.is_none(), "unnamed snapshot should have None name");
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
        assert!(result.is_ok(), "stats on mounted store should succeed: {:?}", result);
    }

    #[test]
    fn test_stats_mounted_store_json_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        write_segment_store(&dir);
        std::fs::write(dir.path().join("mount.lock"), b"locked").unwrap();

        let result = run_stats(dir.path(), true);
        assert!(result.is_ok(), "stats --json on mounted store should succeed: {:?}", result);
    }

    // ── StoreStats JSON serialization ────────────────────────────────────────

    #[test]
    fn test_store_stats_serializes_to_json() {
        let stats = StoreStats {
            logical_bytes: 1024,
            physical_bytes: 512,
            host_disk_bytes: 2048,
            file_dedup_ratio: 2.0,
            overall_ratio: 1.5,
            unique_content_bytes: 0,
            physical_disk_bytes: 0,
            snapshot_count: 3,
            refcount_distribution: RefcountDist { unique: 10, shared_2x: 5, shared_3plus: 2 },
            snapshots: vec![],
            mounted: false,
        };
        let json = serde_json::to_string(&stats).expect("StoreStats should serialize to JSON");
        assert!(json.contains("logical_bytes"), "JSON should contain logical_bytes");
        assert!(json.contains("physical_bytes"), "JSON should contain physical_bytes");
        assert!(json.contains("file_dedup_ratio"), "JSON should contain file_dedup_ratio");
        assert!(json.contains("snapshot_count"), "JSON should contain snapshot_count");
    }

    // ── segment store json output ────────────────────────────────────────────

    #[test]
    fn test_stats_segment_store_json_output() {
        let dir = tempfile::tempdir().unwrap();
        write_segment_store(&dir);
        let result = run_stats(dir.path(), true);
        assert!(result.is_ok(), "stats --json on segment store should succeed: {:?}", result);
    }
}
