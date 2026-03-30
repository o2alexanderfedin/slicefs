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
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use fuser::{mount2, Config, MountOption, SessionACL};
use metadata::gc::background::spawn_background_gc;
use metadata::mount_lock::{acquire_mount_lock, MountLock, MountLockError};
use metadata::segment::load_store_from_segments;
use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use metadata::wal::{WalConfig, create_wal};
use crate::filesystem::SliceFsFilesystem;

/// Load a seeded store from disk, returning `(DictMetadataStore, Arc<Mutex<StoreIo>>, MountLock)`.
///
/// The returned `MountLock` must be kept alive for the duration of the mount;
/// dropping it removes `mount.lock` from the store directory.
///
/// Steps:
/// 1. Reject legacy `dictionary.bin` stores — re-seed required.
/// 2. Acquire `mount.lock` (returns `DirtyMount` if previous mount crashed).
///    On dirty mount, segment-based WAL replay is implicit (re-loading segments
///    replays all mutations that were durably written before the crash).
/// 3. Load state from segment files.
/// 4. Reconstruct `DictMetadataStore` via `load_from_root`.
/// 5. Create WAL with `wal_config` and attach it to the store.
pub fn load_store(
    store_path: &Path,
    wal_config: WalConfig,
) -> Result<(DictMetadataStore, Arc<Mutex<StoreIo>>, MountLock), Box<dyn std::error::Error>> {
    // Step 1: Reject legacy format — migration path removed.
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
    let (last_root, snapshots) = load_store_from_segments(&segs_dir)
        .map_err(|e| format!("failed to load segments: {}", e))?;

    let root = last_root.ok_or_else(|| {
        format!("no committed state found in segments at {}", segs_dir.display())
    })?;

    // Step 4: Reconstruct metadata store using file-backed StoreIo.
    let io = Arc::new(Mutex::new(StoreIo::new(store_path)));
    let mut meta = DictMetadataStore::load_from_root(io.clone(), &root)
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

    Ok((meta, io, mount_lock))
}

/// Determine the next segment ID by scanning existing segment files.
///
/// Returns `max_existing_id + 1`, or `1` if no segments exist.
pub(crate) fn next_segment_id(segs_dir: &Path) -> u64 {
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
/// Adds `AllowOther` when `allow_other` is true.
/// On macOS, adds `CUSTOM("direct_io")` to bypass the NFS page cache that
/// FUSE-T uses internally; without this, reads return stale data after writes
/// (FUSE-T issue #45).
/// ACL defaults to `Owner` (only the mounting user can access the filesystem).
pub fn build_mount_options(noatime: bool, allow_other: bool) -> Config {
    let mut mount_options = vec![
        MountOption::FSName("slicefs".to_string()),
        MountOption::DefaultPermissions,
    ];
    if noatime {
        mount_options.push(MountOption::NoAtime);
    }
    // FUSE-T (macOS) translates FUSE operations to NFSv4. The NFS client
    // caches reads and can return stale data after a write because the NFS
    // server (FUSE-T) has not yet committed the data. direct_io bypasses
    // the NFS page cache, making every read/write go directly to the
    // FUSE handler.  This is harmless on other FUSE implementations.
    if cfg!(target_os = "macos") {
        mount_options.push(MountOption::CUSTOM("direct_io".to_string()));
    }
    let mut cfg = Config::default();
    cfg.mount_options = mount_options;
    // fuser 0.17 uses SessionACL to control allow_other: SessionACL::All
    // passes `allow_other` to the kernel, SessionACL::Owner restricts access
    // to the mounting user only.
    cfg.acl = if allow_other { SessionACL::All } else { SessionACL::Owner };
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
/// `allow_other` passes the `allow_other` FUSE mount option (multi-user access).
/// `snapshot_ref` mounts a specific snapshot read-only (by version number or name).
/// `auto_snapshot` enables auto-snapshot on clean unmount (--auto-snapshot flag).
#[allow(clippy::too_many_arguments)]
pub fn run_mount(
    store_path: &Path,
    mountpoint: &Path,
    noatime: bool,
    allow_other: bool,
    _cache_size: usize,
    wal_strategy: Option<&str>,
    snapshot_ref: Option<&str>,
    auto_snapshot: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let wal_config = parse_wal_config(wal_strategy);

    let (meta, io, _mount_lock) = load_store(store_path, wal_config)?;

    // If mounting a snapshot: resolve it and load from snapshot root (read-only).
    let (final_meta, final_io, config) = if let Some(snap_ref) = snapshot_ref {
        let snap = meta.find_snapshot(snap_ref).ok_or_else(|| -> Box<dyn std::error::Error> {
            format!("snapshot not found: {}", snap_ref).into()
        })?;
        println!("Mounting snapshot {} (read-only)", snap.version);
        let snap_io: Arc<Mutex<StoreIo>> = io.clone();
        let snap_meta = DictMetadataStore::load_from_root(snap_io.clone(), &snap.root)
            .map_err(|e| -> Box<dyn std::error::Error> { format!("failed to load snapshot root: {}", e).into() })?;
        let mut cfg = build_mount_options(noatime, allow_other);
        cfg.mount_options.push(MountOption::RO);
        (snap_meta, snap_io, cfg)
    } else {
        let cfg = build_mount_options(noatime, allow_other);
        (meta, io, cfg)
    };

    let mut fs = SliceFsFilesystem::new(
        final_meta,
        final_io,
        Some(store_path.to_path_buf()),
    );
    fs.set_auto_snapshot(auto_snapshot);

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
    use metadata::store::DictMetadataStore;
    use metadata::store_io::StoreIo;
    use metadata::wal::WalConfig;
    use slicefs_traits::metadata::{InodeMeta, MetadataStore};
    use tempfile::TempDir;

    const S_IFREG: u32 = 0o100_000;
    const S_IFDIR: u32 = 0o040_000;

    /// Write a valid seeded store in segment format (new format only).
    fn write_seeded_store_segments(store_dir: &TempDir) {
        use metadata::wal::create_wal;
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

    #[test]
    fn test_load_store_returns_correct_inode_1() {
        let store_dir = tempfile::tempdir().unwrap();
        write_seeded_store_segments(&store_dir);

        let (meta, _io, _lock) = load_store(store_dir.path(), WalConfig::NoWal).expect("load_store failed");

        // Inode 1 must exist and be a directory (root)
        let root_meta = meta.get_inode(1).expect("root inode missing");
        let kind = root_meta.mode & 0o170_000;
        assert_eq!(kind, S_IFDIR, "inode 1 should be a directory");
    }

    #[test]
    fn test_load_store_finds_seeded_file() {
        let store_dir = tempfile::tempdir().unwrap();
        write_seeded_store_segments(&store_dir);

        let (meta, _io, _lock) = load_store(store_dir.path(), WalConfig::NoWal).expect("load_store failed");

        // hello.txt was seeded
        let ino = meta.lookup(1, "hello.txt").expect("hello.txt not found");
        assert!(ino > 1, "file inode should be > 1");
    }

    #[test]
    fn test_load_store_rejects_legacy_format() {
        let store_dir = tempfile::tempdir().unwrap();
        // Write a dictionary.bin to simulate legacy format
        std::fs::write(store_dir.path().join("dictionary.bin"), b"").unwrap();

        let result = load_store(store_dir.path(), WalConfig::NoWal);
        assert!(result.is_err(), "should reject legacy format");
        let msg = result.err().unwrap().to_string();
        assert!(
            msg.contains("Re-seed required") || msg.contains("legacy"),
            "error should mention re-seed, got: {}",
            msg
        );
    }

    #[test]
    fn test_load_store_rejects_missing_store() {
        let store_dir = tempfile::tempdir().unwrap();
        // No dictionary.bin, no segments/ — should fail.
        let result = load_store(store_dir.path(), WalConfig::NoWal);
        assert!(result.is_err(), "should fail with missing store");
        let msg = result.err().unwrap().to_string();
        assert!(
            msg.contains("segments/") || msg.contains("store not found"),
            "error should mention segments/, got: {}",
            msg
        );
    }

    #[test]
    fn test_load_store_from_segment_format() {
        let store_dir = tempfile::tempdir().unwrap();
        write_seeded_store_segments(&store_dir);

        let (meta, _io, _lock) = load_store(store_dir.path(), WalConfig::NoWal)
            .expect("load_store from segments failed");

        // hello.txt was seeded
        let ino = meta.lookup(1, "hello.txt").expect("hello.txt not found in segment store");
        assert!(ino > 1, "file inode should be > 1");
    }

    #[test]
    fn test_build_mount_options_with_noatime() {
        let config = build_mount_options(true, false);
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
        let config = build_mount_options(false, false);
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
    #[cfg(target_os = "macos")]
    fn test_build_mount_options_macos_direct_io() {
        // On macOS, direct_io must be present to bypass FUSE-T NFS page cache.
        let config = build_mount_options(false, false);
        assert!(
            config.mount_options.contains(&MountOption::CUSTOM("direct_io".to_string())),
            "CUSTOM(direct_io) must be present on macOS to fix FUSE-T write visibility"
        );
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn test_build_mount_options_linux_no_direct_io() {
        // On Linux, direct_io must NOT be present (it is a macOS-only workaround).
        let config = build_mount_options(false, false);
        assert!(
            !config.mount_options.contains(&MountOption::CUSTOM("direct_io".to_string())),
            "CUSTOM(direct_io) must NOT be present on Linux"
        );
    }

    #[test]
    fn test_build_mount_options_allow_other() {
        // fuser 0.17 uses SessionACL::All to implement allow_other.
        let config = build_mount_options(false, true);
        assert!(
            matches!(config.acl, SessionACL::All),
            "SessionACL must be All when allow_other=true"
        );
    }

    #[test]
    fn test_build_mount_options_no_allow_other() {
        let config = build_mount_options(false, false);
        assert!(
            matches!(config.acl, SessionACL::Owner),
            "SessionACL must be Owner when allow_other=false"
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
