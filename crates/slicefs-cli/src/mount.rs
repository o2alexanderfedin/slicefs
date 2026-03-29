//! `mount` subcommand — loads a seeded SliceFS store and starts a FUSE session.
//!
//! ## Store layout
//!
//! ```text
//! <store>/
//!   segments/              # Segment files (new format)
//!     segment-000001.seg   # Append-only WAL segment
//!     segment-000002.seg   # ...
//!   mount.lock             # Created on mount, removed on clean unmount
//!   dictionary.bin         # Legacy format — auto-migrated to segments/ on first mount
//!   root.bin               # Legacy format — auto-migrated to segments/ on first mount
//! ```
//!
//! ## Usage
//!
//! ```text
//! slicefs mount <mountpoint> --store <store> [--noatime] [--cache-size <bytes>] [--wal-strategy <strategy>]
//! ```
//!
//! The command blocks until the FUSE session ends (SIGTERM, Ctrl+C, or `slicefs unmount`).

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use blockset::Dictionary;
use fuser::{mount2, Config, MountOption, SessionACL};
use metadata::gc::background::spawn_background_gc;
use metadata::mount_lock::{acquire_mount_lock, MountLock, MountLockError};
use metadata::segment::{load_store_from_segments, migrate_legacy_store};
use metadata::store::DictMetadataStore;
use metadata::wal::{WalConfig, create_wal};

use crate::filesystem::SliceFsFilesystem;

/// Load a seeded store from disk, returning `(DictMetadataStore, Dictionary, MountLock)`.
///
/// The returned `MountLock` must be kept alive for the duration of the mount;
/// dropping it removes `mount.lock` from the store directory.
///
/// Steps:
/// 1. If `dictionary.bin` exists, auto-migrate to segment format.
/// 2. Acquire `mount.lock` (returns `DirtyMount` if previous mount crashed).
///    On dirty mount, segment-based WAL replay is implicit (re-loading segments
///    replays all mutations that were durably written before the crash).
/// 3. Load state from segment files.
/// 4. Reconstruct `DictMetadataStore` via `load_from_root`.
/// 5. Create WAL with `wal_config` and attach it to the store.
pub fn load_store(
    store_path: &Path,
    wal_config: WalConfig,
) -> Result<(DictMetadataStore, Dictionary, MountLock), Box<dyn std::error::Error>> {
    // Step 1: Migrate legacy dictionary.bin if present
    if store_path.join("dictionary.bin").exists() {
        migrate_legacy_store(store_path)
            .map_err(|e| format!("migration failed: {}", e))?;
    } else if !store_path.join("segments").is_dir() {
        // Neither legacy nor segment format found — check for partial store state
        if store_path.join("root.bin").exists() {
            return Err(format!(
                "failed to read dictionary.bin in {}: No such file or directory (os error 2)",
                store_path.display()
            ).into());
        }
        return Err(format!(
            "store not found at {}: no dictionary.bin or segments/ directory",
            store_path.display()
        ).into());
    }

    // Step 2: Acquire mount lock.
    // DirtyMount means a previous mount crashed — segments already contain all
    // durably written mutations, so re-loading segments is idempotent WAL replay.
    let lock_result = acquire_mount_lock(store_path);
    let mount_lock = match lock_result {
        Ok(lock) => lock,
        Err(MountLockError::DirtyMount) => {
            // Dirty mount — proceed with segment replay (re-loading is idempotent).
            // Remove the stale lock file and create a fresh one.
            let _ = std::fs::remove_file(store_path.join("mount.lock"));
            acquire_mount_lock(store_path)
                .map_err(|e| format!("failed to acquire mount lock after dirty mount: {}", e))?
        }
        Err(e) => return Err(format!("mount lock error: {}", e).into()),
    };

    // Step 3: Load state from segment files.
    let segs_dir = store_path.join("segments");
    let (dict, last_root, snapshots) = load_store_from_segments(&segs_dir)
        .map_err(|e| format!("failed to load segments: {}", e))?;

    let root = last_root.ok_or_else(|| {
        format!("no committed state found in segments at {}", segs_dir.display())
    })?;

    // Step 4: Reconstruct metadata store.
    // Clone dict before load_from_root (which consumes it) — the clone is used
    // for content reads in SliceFsFilesystem.
    let content_dict = dict.clone();
    let mut meta = DictMetadataStore::load_from_root(dict, &root)
        .map_err(|e| format!("failed to reconstruct metadata store: {}", e))?;

    // Restore snapshot list from segment replay.
    meta.set_snapshots(snapshots);

    // Step 5: Create WAL and attach to store.
    // Determine next segment_id from existing segments.
    let next_segment_id = next_segment_id(&segs_dir);
    std::fs::create_dir_all(&segs_dir)?;
    let wal = create_wal(wal_config, store_path, next_segment_id)
        .map_err(|e| format!("failed to create WAL: {}", e))?;
    meta.set_wal(wal);

    Ok((meta, content_dict, mount_lock))
}

/// Determine the next segment ID by scanning existing segment files.
///
/// Returns `max_existing_id + 1`, or `1` if no segments exist.
fn next_segment_id(segs_dir: &Path) -> u64 {
    let max_id = std::fs::read_dir(segs_dir)
        .ok()
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    if name.starts_with("segment-") && name.ends_with(".seg") {
                        // segment-000042.seg → parse "000042"
                        let num_str = &name[8..name.len() - 4];
                        num_str.parse::<u64>().ok()
                    } else {
                        None
                    }
                })
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0);
    max_id + 1
}

/// Build the FUSE mount configuration.
///
/// Always includes: `FSName("slicefs")`, `DefaultPermissions`.
/// Adds `NoAtime` when `noatime` is true.
/// ACL defaults to `Owner` (only the mounting user can access the filesystem).
pub fn build_mount_options(noatime: bool) -> Config {
    let mut mount_options = vec![
        MountOption::FSName("slicefs".to_string()),
        MountOption::DefaultPermissions,
    ];
    if noatime {
        mount_options.push(MountOption::NoAtime);
    }
    let mut cfg = Config::default();
    cfg.mount_options = mount_options;
    cfg.acl = SessionACL::Owner;
    cfg
}

/// Parse a WAL strategy string to a `WalConfig`.
///
/// Accepted values: "per-op" (default), "flush-on-fsync", "periodic", "no-wal".
pub fn parse_wal_config(strategy: Option<&str>) -> WalConfig {
    match strategy.unwrap_or("per-op") {
        "flush-on-fsync" => WalConfig::FlushOnFsync,
        "periodic" => WalConfig::Periodic { interval_secs: 5 },
        "no-wal" => WalConfig::NoWal,
        _ => WalConfig::PerOp,
    }
}

/// Run the `mount` subcommand.
///
/// Loads the store, constructs the FUSE filesystem, and starts a blocking
/// `fuser::mount2` session. Blocks until the session ends (SIGTERM, Ctrl+C,
/// or `slicefs unmount`).
///
/// `_cache_size` is accepted but unused in Phase 3. Placeholder for Phase 4.
pub fn run_mount(
    store_path: &Path,
    mountpoint: &Path,
    noatime: bool,
    _cache_size: usize,
    wal_strategy: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let wal_config = parse_wal_config(wal_strategy);
    let (meta, content_dict, _mount_lock) = load_store(store_path, wal_config)?;
    let fs = SliceFsFilesystem::new(meta, content_dict, Some(store_path.to_path_buf()));
    let config = build_mount_options(noatime);

    // Spawn background GC thread.
    // The GC thread holds a Weak<DictMetadataStore> so it exits automatically when
    // the filesystem is dropped. We also use an explicit shutdown flag to stop it
    // gracefully before the MountLock drops.
    let weak_meta = Arc::downgrade(fs.meta());
    let segments_dir = store_path.join("segments");
    let gc_shutdown = Arc::new(AtomicBool::new(false));
    let gc_handle = spawn_background_gc(
        weak_meta,
        segments_dir,
        Duration::from_secs(60),
        1000,
        Arc::clone(&gc_shutdown),
    );

    println!("SliceFS mounted at {}", mountpoint.display());

    mount2(fs, mountpoint, &config)?;

    // mount2 has returned — FUSE session ended, destroy() already called.
    // destroy() calls shutdown_wal() which flushes and closes the WAL segment.
    // Shut down the GC thread before the MountLock drops.
    gc_shutdown.store(true, Ordering::SeqCst);
    gc_handle.shutdown();

    // _mount_lock is dropped here, removing mount.lock from the store directory.
    println!("SliceFS unmounted.");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use metadata::store::{serialize_dictionary, DictMetadataStore};
    use metadata::wal::WalConfig;
    use slicefs_traits::metadata::{InodeMeta, MetadataStore};
    use tempfile::TempDir;

    const S_IFREG: u32 = 0o100_000;
    const S_IFDIR: u32 = 0o040_000;

    /// Write a valid seeded store in legacy format (dictionary.bin + root.bin).
    fn write_seeded_store_legacy(store_dir: &TempDir) {
        let meta = DictMetadataStore::new();
        // Add a test file
        let file_meta = InodeMeta::new_file(0, 0, 0, S_IFREG | 0o644);
        let ino = meta.create_inode(&file_meta).unwrap();
        meta.link(1, "hello.txt", ino).unwrap();
        let root = meta.commit().unwrap();

        let dict_bytes = {
            let dict = meta.dict().lock().unwrap();
            serialize_dictionary(&*dict)
        };
        std::fs::write(store_dir.path().join("dictionary.bin"), &dict_bytes).unwrap();

        let mut root_bytes = Vec::with_capacity(28);
        for word in &root {
            root_bytes.extend_from_slice(&word.to_le_bytes());
        }
        std::fs::write(store_dir.path().join("root.bin"), &root_bytes).unwrap();
    }

    /// Write a valid seeded store in segment format.
    fn write_seeded_store_segments(store_dir: &TempDir) {
        use metadata::wal::create_wal;
        let segs_dir = store_dir.path().join("segments");
        std::fs::create_dir_all(&segs_dir).unwrap();

        let wal = create_wal(WalConfig::PerOp, store_dir.path(), 1).unwrap();
        let mut meta = DictMetadataStore::new();
        meta.set_wal(wal);

        let file_meta = InodeMeta::new_file(0, 0, 0, S_IFREG | 0o644);
        let ino = meta.create_inode(&file_meta).unwrap();
        meta.link(1, "hello.txt", ino).unwrap();
        meta.commit().unwrap();
        meta.shutdown_wal().unwrap();
    }

    #[test]
    fn test_load_store_returns_correct_inode_1() {
        let store_dir = tempfile::tempdir().unwrap();
        write_seeded_store_legacy(&store_dir);

        let (meta, _dict, _lock) = load_store(store_dir.path(), WalConfig::NoWal).expect("load_store failed");

        // Inode 1 must exist and be a directory (root)
        let root_meta = meta.get_inode(1).expect("root inode missing");
        let kind = root_meta.mode & 0o170_000;
        assert_eq!(kind, S_IFDIR, "inode 1 should be a directory");
    }

    #[test]
    fn test_load_store_finds_seeded_file() {
        let store_dir = tempfile::tempdir().unwrap();
        write_seeded_store_legacy(&store_dir);

        let (meta, _dict, _lock) = load_store(store_dir.path(), WalConfig::NoWal).expect("load_store failed");

        // hello.txt was seeded in write_seeded_store_legacy
        let ino = meta.lookup(1, "hello.txt").expect("hello.txt not found");
        assert!(ino > 1, "file inode should be > 1");
    }

    #[test]
    fn test_load_store_rejects_missing_root_bin() {
        let store_dir = tempfile::tempdir().unwrap();
        // Write only dictionary.bin, no root.bin — migration will fail with missing root.bin
        std::fs::write(store_dir.path().join("dictionary.bin"), b"").unwrap();

        let result = load_store(store_dir.path(), WalConfig::NoWal);
        assert!(result.is_err(), "should fail with missing root.bin");
        let msg = result.err().unwrap().to_string();
        assert!(
            msg.contains("root.bin"),
            "error should mention root.bin, got: {}",
            msg
        );
    }

    #[test]
    fn test_load_store_rejects_missing_dictionary_bin() {
        let store_dir = tempfile::tempdir().unwrap();
        // Write only root.bin (valid 28 bytes), no dictionary.bin or segments/
        let root_bytes = [0u8; 28];
        std::fs::write(store_dir.path().join("root.bin"), &root_bytes).unwrap();

        let result = load_store(store_dir.path(), WalConfig::NoWal);
        assert!(result.is_err(), "should fail with missing dictionary.bin");
        let msg = result.err().unwrap().to_string();
        assert!(
            msg.contains("dictionary.bin"),
            "error should mention dictionary.bin, got: {}",
            msg
        );
    }

    #[test]
    fn test_load_store_rejects_root_bin_wrong_size() {
        let store_dir = tempfile::tempdir().unwrap();
        // Write root.bin with wrong size (e.g., 16 bytes instead of 28)
        std::fs::write(store_dir.path().join("root.bin"), &[0u8; 16]).unwrap();
        std::fs::write(store_dir.path().join("dictionary.bin"), b"").unwrap();

        let result = load_store(store_dir.path(), WalConfig::NoWal);
        assert!(result.is_err(), "should fail with wrong root.bin size");
        let msg = result.err().unwrap().to_string();
        assert!(
            msg.contains("28 bytes") || msg.contains("16 bytes"),
            "error should mention size, got: {}",
            msg
        );
    }

    #[test]
    fn test_load_store_from_segment_format() {
        let store_dir = tempfile::tempdir().unwrap();
        write_seeded_store_segments(&store_dir);

        let (meta, _dict, _lock) = load_store(store_dir.path(), WalConfig::NoWal)
            .expect("load_store from segments failed");

        // hello.txt was seeded
        let ino = meta.lookup(1, "hello.txt").expect("hello.txt not found in segment store");
        assert!(ino > 1, "file inode should be > 1");
    }

    #[test]
    fn test_load_store_migrates_legacy_format() {
        let store_dir = tempfile::tempdir().unwrap();
        write_seeded_store_legacy(&store_dir);

        // Load — should migrate dictionary.bin to segments/
        let (_meta, _dict, _lock) = load_store(store_dir.path(), WalConfig::NoWal)
            .expect("load_store migration failed");

        // After migration, dictionary.bin should be gone
        assert!(
            !store_dir.path().join("dictionary.bin").exists(),
            "dictionary.bin should be removed after migration"
        );
        assert!(
            store_dir.path().join("segments").is_dir(),
            "segments/ should exist after migration"
        );
    }

    #[test]
    fn test_build_mount_options_with_noatime() {
        let config = build_mount_options(true);
        assert!(
            !config.mount_options.contains(&MountOption::RO),
            "RO must NOT be present (mount is read-write)"
        );
        assert!(
            config.mount_options.contains(&MountOption::NoAtime),
            "NoAtime must be present when noatime=true"
        );
        assert!(
            config.mount_options.contains(&MountOption::DefaultPermissions),
            "DefaultPermissions must always be present"
        );
    }

    #[test]
    fn test_build_mount_options_without_noatime() {
        let config = build_mount_options(false);
        assert!(
            !config.mount_options.contains(&MountOption::RO),
            "RO must NOT be present (mount is read-write)"
        );
        assert!(
            !config.mount_options.contains(&MountOption::NoAtime),
            "NoAtime must NOT be present when noatime=false"
        );
        assert!(
            config.mount_options.contains(&MountOption::DefaultPermissions),
            "DefaultPermissions must always be present"
        );
    }

    #[test]
    fn test_parse_wal_config_defaults_to_per_op() {
        assert!(matches!(parse_wal_config(None), WalConfig::PerOp));
        assert!(matches!(parse_wal_config(Some("per-op")), WalConfig::PerOp));
    }

    #[test]
    fn test_parse_wal_config_flush_on_fsync() {
        assert!(matches!(parse_wal_config(Some("flush-on-fsync")), WalConfig::FlushOnFsync));
    }

    #[test]
    fn test_parse_wal_config_no_wal() {
        assert!(matches!(parse_wal_config(Some("no-wal")), WalConfig::NoWal));
    }

    #[test]
    fn test_parse_wal_config_periodic() {
        assert!(matches!(parse_wal_config(Some("periodic")), WalConfig::Periodic { .. }));
    }
}
