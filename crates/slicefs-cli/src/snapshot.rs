//! `snapshot` subcommand — create, list, and switch snapshots in a SliceFS store.
//!
//! ## Usage
//!
//! ```text
//! slicefs snapshot create <store> [--name "tag"]
//! slicefs snapshot list <store>
//! slicefs snapshot switch <store> <version_or_name>
//! ```
//!
//! All snapshot commands require the store to be **unmounted** (no `mount.lock`).

use std::path::Path;

use std::sync::{Arc, Mutex};

use metadata::segment::load_store_from_segments;
use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use metadata::wal::{WalConfig, create_wal};

use crate::cli::SnapshotAction;
use crate::mount::next_segment_id;

/// Dispatch a `snapshot` subcommand to the appropriate handler.
pub fn run_snapshot(action: SnapshotAction) -> Result<(), Box<dyn std::error::Error>> {
    match action {
        SnapshotAction::Create { store, name } => run_snapshot_create(&store, name),
        SnapshotAction::List { store } => run_snapshot_list(&store),
        SnapshotAction::Switch {
            store,
            version_or_name,
        } => run_snapshot_switch(&store, &version_or_name),
    }
}

/// Create a new snapshot of the current committed state.
///
/// # Errors
/// - Returns error if `mount.lock` is present (store is mounted).
/// - Returns error if segment files cannot be read.
/// - Returns error if no committed root is found in segments.
pub fn run_snapshot_create(
    store_path: &Path,
    name: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Refuse to run on a mounted store.
    let lock_path = store_path.join("mount.lock");
    if lock_path.exists() {
        return Err(format!(
            "Store is mounted (mount.lock found at {}). Unmount first before creating snapshots.",
            lock_path.display()
        )
        .into());
    }

    let segs_dir = store_path.join("segments");

    // Load last committed root + snapshot list from segment files.
    let (root_opt, snapshots) = load_store_from_segments(&segs_dir)
        .map_err(|e| format!("failed to load segments: {}", e))?;

    let root = root_opt.ok_or_else(|| {
        format!(
            "no committed state found in segments at {}",
            segs_dir.display()
        )
    })?;

    // Reconstruct DictMetadataStore from file-backed StoreIo.
    let io = Arc::new(Mutex::new(StoreIo::new(store_path)));
    let mut meta = DictMetadataStore::load_from_root(io, &root)
        .map_err(|e| format!("failed to reconstruct metadata store: {}", e))?;

    // Restore snapshot list from segment replay.
    meta.set_snapshots(snapshots);

    // Create WAL to write snapshot record to a new segment.
    let next_id = next_segment_id(&segs_dir);
    std::fs::create_dir_all(&segs_dir)?;
    let wal = create_wal(WalConfig::PerOp, store_path, next_id)
        .map_err(|e| format!("failed to create WAL: {}", e))?;
    meta.set_wal(wal);

    // Create the snapshot.
    let entry = meta
        .create_snapshot(name)
        .map_err(|e| format!("failed to create snapshot: {}", e))?;

    // Shutdown WAL (flushes snapshot record to disk).
    meta.shutdown_wal()
        .map_err(|e| format!("WAL shutdown failed: {}", e))?;

    // Format root as hex string.
    let root_hex: String = entry
        .root
        .iter()
        .flat_map(|word| word.to_le_bytes())
        .map(|b| format!("{b:02x}"))
        .collect();

    println!("Snapshot {} created (root: {})", entry.version, root_hex);

    Ok(())
}

/// List all snapshots in the store.
pub fn run_snapshot_list(store_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let segs_dir = store_path.join("segments");

    // Load snapshot list from segment replay (no need to reconstruct full store).
    let (_root_opt, snapshots) = load_store_from_segments(&segs_dir)
        .map_err(|e| format!("failed to load segments: {}", e))?;

    if snapshots.is_empty() {
        println!("No snapshots found.");
        return Ok(());
    }

    // Print table header.
    println!("{:<8} {:<20} {:<22} Root", "Version", "Name", "Created");
    println!("{}", "-".repeat(80));

    for snap in &snapshots {
        let name_str = snap.name.as_deref().unwrap_or("-");
        let created_str = format_unix_timestamp(snap.created_at);
        let root_hex: String = snap
            .root
            .iter()
            .flat_map(|word| word.to_le_bytes())
            .map(|b| format!("{b:02x}"))
            .collect();
        // Truncate root hex to 16 chars for readability.
        let root_short = &root_hex[..root_hex.len().min(16)];
        println!(
            "{:<8} {:<20} {:<22} {}...",
            snap.version, name_str, created_str, root_short
        );
    }

    println!("\n{} snapshot(s) total.", snapshots.len());

    Ok(())
}

/// Switch the live filesystem root to a snapshot's root.
///
/// Auto-saves the current state as an "auto-before-switch" snapshot first.
///
/// # Errors
/// - Returns error if `mount.lock` is present (store is mounted).
/// - Returns error if snapshot not found by version_or_name.
pub fn run_snapshot_switch(
    store_path: &Path,
    version_or_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // Refuse to run on a mounted store.
    let lock_path = store_path.join("mount.lock");
    if lock_path.exists() {
        return Err(format!(
            "Store is mounted (mount.lock found at {}). Unmount first before switching snapshots.",
            lock_path.display()
        )
        .into());
    }

    let segs_dir = store_path.join("segments");

    // Load last committed root + snapshot list from segment files.
    let (root_opt, snapshots) = load_store_from_segments(&segs_dir)
        .map_err(|e| format!("failed to load segments: {}", e))?;

    let root = root_opt.ok_or_else(|| {
        format!(
            "no committed state found in segments at {}",
            segs_dir.display()
        )
    })?;

    // Reconstruct DictMetadataStore from file-backed StoreIo.
    let io = Arc::new(Mutex::new(StoreIo::new(store_path)));
    let mut meta = DictMetadataStore::load_from_root(io, &root)
        .map_err(|e| format!("failed to reconstruct metadata store: {}", e))?;

    // Restore snapshot list from segment replay.
    meta.set_snapshots(snapshots);

    // Create WAL to write snapshot records and root update.
    let next_id = next_segment_id(&segs_dir);
    std::fs::create_dir_all(&segs_dir)?;
    let wal = create_wal(WalConfig::PerOp, store_path, next_id)
        .map_err(|e| format!("failed to create WAL: {}", e))?;
    meta.set_wal(wal);

    // Find target snapshot.
    let target = meta
        .find_snapshot(version_or_name)
        .ok_or_else(|| format!("snapshot not found: {}", version_or_name))?;

    // Auto-snapshot current state before switching.
    let auto_snap = meta
        .create_snapshot(Some("auto-before-switch".to_string()))
        .map_err(|e| format!("failed to create auto-snapshot: {}", e))?;

    // Write a RootUpdate pointing to the snapshot's root.
    // This is done by calling commit() on a store loaded from the snapshot root.
    // We reconstruct the store from the snapshot root and call commit() to write RootUpdate.
    write_root_update(&meta, &target.root)?;

    // Shutdown WAL (flushes all records to disk).
    meta.shutdown_wal()
        .map_err(|e| format!("WAL shutdown failed: {}", e))?;

    println!(
        "Switched to snapshot {} (auto-snapshot {} saved)",
        target.version, auto_snap.version
    );

    Ok(())
}

/// Write a `RootUpdate` WAL entry pointing at `new_root`.
///
/// The simplest way: use `commit_to_root` via internal WAL logging.
/// We write the new root entry directly to the WAL via the store's log mechanism.
fn write_root_update(
    meta: &DictMetadataStore,
    new_root: &slicefs_traits::digest::Digest224,
) -> Result<(), Box<dyn std::error::Error>> {
    // Use the WAL's log_root_update method if available, or write through the store.
    // DictMetadataStore exposes `commit_root()` for this purpose.
    meta.commit_root(*new_root)
        .map_err(|e| format!("failed to write root update: {}", e).into())
}

/// Format a Unix timestamp (seconds) as an ISO 8601 datetime string.
///
/// Returns "unknown" if the timestamp is zero.
fn format_unix_timestamp(secs: u64) -> String {
    if secs == 0 {
        return "unknown".to_string();
    }
    // Minimal ISO 8601 formatter without external dependencies.
    // Seconds since epoch → (year, month, day, hour, min, sec)
    let s = secs;
    let sec = (s % 60) as u32;
    let min = ((s / 60) % 60) as u32;
    let hour = ((s / 3600) % 24) as u32;
    let days = s / 86400;
    // Compute year/month/day from day count (Gregorian calendar).
    let (year, month, day) = days_to_ymd(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, day, hour, min, sec
    )
}

/// Convert a count of days since Unix epoch (1970-01-01) to (year, month, day).
fn days_to_ymd(days: u64) -> (u32, u32, u32) {
    // Algorithm from https://howardhinnant.github.io/date_algorithms.html
    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y as u32, m as u32, d as u32)
}

// Re-export next_segment_id for internal use — defined in mount.rs as pub(crate).
// (mount.rs needs to expose next_segment_id as pub(crate) or pub.)

#[cfg(test)]
mod tests {
    use super::*;
    use metadata::wal::WalConfig;
    use slicefs_traits::metadata::{InodeMeta, MetadataStore};
    use tempfile::TempDir;

    const S_IFREG: u32 = 0o100_000;

    /// Create a valid segment-format store with a committed file.
    fn make_seeded_store(store_dir: &TempDir) {
        use metadata::store_io::StoreIo;
        use metadata::wal::create_wal;
        use std::sync::{Arc, Mutex};
        let segs_dir = store_dir.path().join("segments");
        std::fs::create_dir_all(&segs_dir).unwrap();

        let io = Arc::new(Mutex::new(StoreIo::new(store_dir.path())));
        let wal = create_wal(WalConfig::PerOp, store_dir.path(), 1).unwrap();
        let mut meta = DictMetadataStore::new(io);
        meta.set_wal(wal);

        let file_meta = InodeMeta::new_file(0, 0, 0, S_IFREG | 0o644);
        let ino = meta.create_inode(&file_meta).unwrap();
        meta.link(1, "hello.txt", ino).unwrap();
        meta.commit().unwrap();
        meta.shutdown_wal().unwrap();
    }

    // ── run_snapshot_create tests ────────────────────────────────────────────

    #[test]
    fn test_snapshot_create_succeeds_on_unmounted_store() {
        let store_dir = tempfile::tempdir().unwrap();
        make_seeded_store(&store_dir);

        let result = run_snapshot_create(store_dir.path(), None);
        assert!(
            result.is_ok(),
            "snapshot create should succeed: {:?}",
            result
        );
    }

    #[test]
    fn test_snapshot_create_with_name_succeeds() {
        let store_dir = tempfile::tempdir().unwrap();
        make_seeded_store(&store_dir);

        let result = run_snapshot_create(store_dir.path(), Some("v1.0".to_string()));
        assert!(
            result.is_ok(),
            "snapshot create with name should succeed: {:?}",
            result
        );
    }

    #[test]
    fn test_snapshot_create_refuses_mounted_store() {
        let store_dir = tempfile::tempdir().unwrap();
        make_seeded_store(&store_dir);
        // Create mount.lock to simulate a mounted store.
        std::fs::write(store_dir.path().join("mount.lock"), "pid=1234").unwrap();

        let result = run_snapshot_create(store_dir.path(), None);
        assert!(result.is_err(), "should reject mounted store");
        assert!(result.unwrap_err().to_string().contains("mount.lock"));
    }

    // ── run_snapshot_list tests ──────────────────────────────────────────────

    #[test]
    fn test_snapshot_list_empty() {
        let store_dir = tempfile::tempdir().unwrap();
        make_seeded_store(&store_dir);

        // No snapshots yet — should print "No snapshots found."
        let result = run_snapshot_list(store_dir.path());
        assert!(
            result.is_ok(),
            "snapshot list should succeed on empty: {:?}",
            result
        );
    }

    #[test]
    fn test_snapshot_list_after_create() {
        let store_dir = tempfile::tempdir().unwrap();
        make_seeded_store(&store_dir);
        run_snapshot_create(store_dir.path(), Some("snap-1".to_string())).unwrap();

        let result = run_snapshot_list(store_dir.path());
        assert!(result.is_ok(), "snapshot list should succeed: {:?}", result);
    }

    // ── run_snapshot_switch tests ────────────────────────────────────────────

    #[test]
    fn test_snapshot_switch_by_version() {
        let store_dir = tempfile::tempdir().unwrap();
        make_seeded_store(&store_dir);
        run_snapshot_create(store_dir.path(), Some("snap-1".to_string())).unwrap();

        let result = run_snapshot_switch(store_dir.path(), "1");
        assert!(
            result.is_ok(),
            "snapshot switch by version should succeed: {:?}",
            result
        );
    }

    #[test]
    fn test_snapshot_switch_by_name() {
        let store_dir = tempfile::tempdir().unwrap();
        make_seeded_store(&store_dir);
        run_snapshot_create(store_dir.path(), Some("snap-1".to_string())).unwrap();

        let result = run_snapshot_switch(store_dir.path(), "snap-1");
        assert!(
            result.is_ok(),
            "snapshot switch by name should succeed: {:?}",
            result
        );
    }

    #[test]
    fn test_snapshot_switch_refuses_mounted_store() {
        let store_dir = tempfile::tempdir().unwrap();
        make_seeded_store(&store_dir);
        run_snapshot_create(store_dir.path(), None).unwrap();
        std::fs::write(store_dir.path().join("mount.lock"), "pid=1234").unwrap();

        let result = run_snapshot_switch(store_dir.path(), "1");
        assert!(result.is_err(), "should reject mounted store");
        assert!(result.unwrap_err().to_string().contains("mount.lock"));
    }

    #[test]
    fn test_snapshot_switch_unknown_ref_fails() {
        let store_dir = tempfile::tempdir().unwrap();
        make_seeded_store(&store_dir);
        run_snapshot_create(store_dir.path(), None).unwrap();

        let result = run_snapshot_switch(store_dir.path(), "nonexistent");
        assert!(result.is_err(), "should fail for unknown snapshot ref");
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    // ── format_unix_timestamp tests ──────────────────────────────────────────

    #[test]
    fn test_format_unix_timestamp_zero_returns_unknown() {
        assert_eq!(format_unix_timestamp(0), "unknown");
    }

    #[test]
    fn test_format_unix_timestamp_epoch() {
        // 1970-01-01T00:00:01Z
        assert_eq!(format_unix_timestamp(1), "1970-01-01T00:00:01Z");
    }

    #[test]
    fn test_format_unix_timestamp_known_date() {
        // 2024-01-01T00:00:00Z = 1704067200
        assert_eq!(format_unix_timestamp(1704067200), "2024-01-01T00:00:00Z");
    }
}
