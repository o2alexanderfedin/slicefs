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

use std::path::{Path, PathBuf};
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

// ── Signal handling (macOS only) ──────────────────────────────────────────────

/// Set by SIGTERM/SIGINT handler to coordinate watchdog shutdown.
///
/// Signal-safe: only an atomic store is performed in the handler.
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Register SIGTERM and SIGINT handlers that set SHUTDOWN_REQUESTED.
///
/// This is belt-and-suspenders alongside fuser's own signal handling — it
/// ensures the watchdog thread sees the shutdown flag and stops health checks
/// promptly after the signal arrives.
///
/// # Safety
///
/// `libc::signal` is unsafe. The handler only performs a single atomic store
/// (signal-safe per POSIX).
#[cfg(target_os = "macos")]
unsafe fn register_signal_handlers() {
    extern "C" fn handler(_sig: libc::c_int) {
        SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
    }
    libc::signal(libc::SIGTERM, handler as libc::sighandler_t);
    libc::signal(libc::SIGINT, handler as libc::sighandler_t);
}

// ── Watchdog thread ───────────────────────────────────────────────────────────

/// Spawn a watchdog thread that monitors mount health.
///
/// Polls `std::fs::metadata(mountpoint)` every `interval`. If the metadata
/// call fails (ENOENT, ENOTCONN, etc.), the mount is considered dead and
/// `umount -f <mountpoint>` is invoked as a last resort.
///
/// The watchdog also monitors the `SHUTDOWN_REQUESTED` static and the
/// `shutdown` flag so it exits cleanly after `mount2()` returns.
///
/// Pattern mirrors the existing background GC thread.
fn spawn_watchdog(
    mountpoint: PathBuf,
    interval: Duration,
    shutdown: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(interval);

            if shutdown.load(Ordering::SeqCst) || SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
                break;
            }

            if std::fs::metadata(&mountpoint).is_err() {
                eprintln!(
                    "[watchdog] mount health check failed -- forcing unmount of {}",
                    mountpoint.display()
                );
                let _ = std::process::Command::new("umount")
                    .arg("-f")
                    .arg(&mountpoint)
                    .status();
                break;
            }
        }
    })
}

// ── fuse-t.ini fallback (macOS only) ─────────────────────────────────────────

/// Path to the fuse-t.ini configuration file.
#[cfg(target_os = "macos")]
const FUSE_T_INI_PATH: &str = "/Library/Application Support/fuse-t/cfg/fuse-t.ini";

/// RAII guard that restores the original fuse-t.ini content on drop.
///
/// Created before injecting a temporary backend override; dropped after the
/// mount attempt regardless of success or panic.
#[cfg(target_os = "macos")]
struct FuseTIniGuard {
    path: PathBuf,
    original: Option<Vec<u8>>,
}

#[cfg(target_os = "macos")]
impl Drop for FuseTIniGuard {
    fn drop(&mut self) {
        if let Some(ref content) = self.original {
            let _ = std::fs::write(&self.path, content);
        }
    }
}

/// Temporarily inject `backend=<value>` into fuse-t.ini, call `f()`, then
/// restore the original content (even on panic — via `FuseTIniGuard` Drop).
///
/// Steps:
/// 1. Read existing ini (may not exist — that is OK).
/// 2. Write modified version with `backend=<value>` under `[Default]` to a
///    `.tmp` sibling file.
/// 3. Atomic rename `.tmp` -> original path.
/// 4. Call `f()`.
/// 5. `FuseTIniGuard` drop restores original content.
///
/// This is only called as a fallback when the primary CUSTOM mount-option
/// approach fails. On FUSE-T 1.0.54+ it will never be triggered in practice.
#[cfg(target_os = "macos")]
fn with_fuse_t_ini_backend<F, R>(backend: &str, f: F) -> Result<R, Box<dyn std::error::Error>>
where
    F: FnOnce() -> Result<R, Box<dyn std::error::Error>>,
{
    let ini_path = PathBuf::from(FUSE_T_INI_PATH);
    let original = std::fs::read(&ini_path).ok();

    // Build modified ini content.
    let modified = if let Some(ref content) = original {
        inject_backend_into_ini(std::str::from_utf8(content).unwrap_or(""), backend)
    } else {
        // Create a minimal ini with the backend entry.
        format!("[Default]\nbackend={}\n", backend)
    };

    // Atomic write: write to .tmp then rename.
    let tmp_path = ini_path.with_extension("ini.tmp");
    if let Some(parent) = ini_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&tmp_path, &modified)?;
    std::fs::rename(&tmp_path, &ini_path)?;

    // Guard restores original on drop (even on panic).
    let _guard = FuseTIniGuard { path: ini_path, original };

    f()
}

/// Inject or replace `backend=<value>` under the `[Default]` section in an
/// ini-format string.  Returns the modified string.
#[cfg(target_os = "macos")]
fn inject_backend_into_ini(ini: &str, backend: &str) -> String {
    let new_entry = format!("backend={}", backend);
    let mut result = String::new();
    let mut in_default = false;
    let mut injected = false;

    for line in ini.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with('[') {
            // Entering a new section — if we were in [Default] and haven't
            // injected yet, do it before moving to the next section.
            if in_default && !injected {
                result.push_str(&new_entry);
                result.push('\n');
                injected = true;
            }
            in_default = trimmed.eq_ignore_ascii_case("[Default]");
            result.push_str(line);
            result.push('\n');
        } else if in_default && trimmed.starts_with("backend=") {
            // Replace existing backend line.
            result.push_str(&new_entry);
            result.push('\n');
            injected = true;
        } else {
            result.push_str(line);
            result.push('\n');
        }
    }

    if !injected {
        // No [Default] section existed, or we never saw a backend= line and
        // we're still in [Default] at EOF.
        if in_default {
            result.push_str(&new_entry);
            result.push('\n');
        } else {
            // Append a new [Default] section.
            if !result.is_empty() && !result.ends_with('\n') {
                result.push('\n');
            }
            result.push_str("[Default]\n");
            result.push_str(&new_entry);
            result.push('\n');
        }
    }

    result
}

// ── Store loading ─────────────────────────────────────────────────────────────

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

// ── Mount options ─────────────────────────────────────────────────────────────

/// Build the FUSE mount configuration.
///
/// Always includes: `FSName("slicefs")`, `DefaultPermissions`.
/// Adds `NoAtime` when `noatime` is true.
/// Adds `AllowOther` when `allow_other` is true.
/// On macOS, adds `CUSTOM("backend=<name>")` when a backend is provided.
/// ACL defaults to `Owner` (only the mounting user can access the filesystem).
pub fn build_mount_options(
    noatime: bool,
    allow_other: bool,
    #[cfg(target_os = "macos")] backend: Option<&crate::backend::FuseTBackend>,
    #[cfg(not(target_os = "macos"))] _backend: Option<()>,
) -> Config {
    let mut mount_options = vec![
        MountOption::FSName("slicefs".to_string()),
        MountOption::DefaultPermissions,
    ];
    if noatime {
        mount_options.push(MountOption::NoAtime);
    }
    // On macOS with a known backend: inject CUSTOM("backend=<name>") so FUSE-T
    // uses the correct transport (SMB or FSKit instead of the default NFS).
    #[cfg(target_os = "macos")]
    if let Some(b) = backend {
        mount_options.push(MountOption::CUSTOM(b.as_mount_option().to_string()));
    }

    let mut cfg = Config::default();
    cfg.mount_options = mount_options;
    // fuser 0.17 uses SessionACL to control allow_other: SessionACL::All
    // passes `allow_other` to the kernel, SessionACL::Owner restricts access
    // to the mounting user only.
    cfg.acl = if allow_other { SessionACL::All } else { SessionACL::Owner };
    cfg
}

// ── WAL config parsing ────────────────────────────────────────────────────────

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

// ── Mountpoint guard ──────────────────────────────────────────────────────────

/// Check if a mountpoint is already mounted.
///
/// Uses `mount` command output to detect existing FUSE mounts at the given path.
/// Returns an error if the mountpoint is already in use, preventing stale process
/// accumulation when `slicefs mount` is invoked multiple times on the same path.
fn check_mountpoint_not_in_use(mountpoint: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let canonical = mountpoint.canonicalize().unwrap_or_else(|_| mountpoint.to_path_buf());
    let output = std::process::Command::new("mount")
        .output()
        .map_err(|e| format!("failed to run `mount`: {}", e))?;
    let mount_table = String::from_utf8_lossy(&output.stdout);
    let mount_str = canonical.to_string_lossy();
    for line in mount_table.lines() {
        // mount output format: "<device> on <path> (<options>)"
        if let Some(on_idx) = line.find(" on ") {
            let rest = &line[on_idx + 4..];
            let mount_path = rest.split(" (").next().unwrap_or(rest);
            if mount_path == mount_str.as_ref() {
                return Err(format!(
                    "mountpoint {} is already in use:\n  {}\nRun `slicefs unmount {}` or `umount {}` first.",
                    mountpoint.display(),
                    line,
                    mountpoint.display(),
                    mountpoint.display(),
                ).into());
            }
        }
    }
    Ok(())
}

// ── run_mount ─────────────────────────────────────────────────────────────────

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
/// `backend` selects the FUSE-T backend (macOS only): "smb", "fskit", or "nfs".
/// `force` allows NFS backend to be used despite the macOS kernel bug.
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
    backend: Option<&str>,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // Pre-flight: reject if mountpoint is already in use.
    check_mountpoint_not_in_use(mountpoint)?;

    // ── macOS: Backend selection ──────────────────────────────────────────────
    //
    // Detect FUSE-T version and select the best available backend before
    // touching the store. Errors here abort before any state is modified.
    #[cfg(target_os = "macos")]
    let (selected_backend, fuse_t_version) = {
        use crate::backend::{parse_backend_flag, select_backend_auto};
        let requested: Option<crate::backend::FuseTBackend> = backend
            .map(|s| parse_backend_flag(s))
            .transpose()
            .map_err(|e: String| -> Box<dyn std::error::Error> { e.into() })?;
        select_backend_auto(requested, force)
            .map_err(|e: String| -> Box<dyn std::error::Error> { e.into() })?
    };

    // Suppress "unused variable" warnings on non-macOS for the parameters that
    // exist but are not used without the macos cfg.
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (backend, force);
    }

    let wal_config = parse_wal_config(wal_strategy);

    let (meta, io, _mount_lock) = load_store(store_path, wal_config)?;

    // If mounting a snapshot: resolve it and load from snapshot root (read-only).
    #[cfg(target_os = "macos")]
    let (final_meta, final_io, config) = if let Some(snap_ref) = snapshot_ref {
        let snap = meta.find_snapshot(snap_ref).ok_or_else(|| -> Box<dyn std::error::Error> {
            format!("snapshot not found: {}", snap_ref).into()
        })?;
        println!("Mounting snapshot {} (read-only)", snap.version);
        let snap_io: Arc<Mutex<StoreIo>> = io.clone();
        let snap_meta = DictMetadataStore::load_from_root(snap_io.clone(), &snap.root)
            .map_err(|e| -> Box<dyn std::error::Error> { format!("failed to load snapshot root: {}", e).into() })?;
        let mut cfg = build_mount_options(noatime, allow_other, None);
        cfg.mount_options.push(MountOption::RO);
        (snap_meta, snap_io, cfg)
    } else {
        let cfg = build_mount_options(noatime, allow_other, Some(&selected_backend));
        (meta, io, cfg)
    };

    #[cfg(not(target_os = "macos"))]
    let (final_meta, final_io, config) = if let Some(snap_ref) = snapshot_ref {
        let snap = meta.find_snapshot(snap_ref).ok_or_else(|| -> Box<dyn std::error::Error> {
            format!("snapshot not found: {}", snap_ref).into()
        })?;
        println!("Mounting snapshot {} (read-only)", snap.version);
        let snap_io: Arc<Mutex<StoreIo>> = io.clone();
        let snap_meta = DictMetadataStore::load_from_root(snap_io.clone(), &snap.root)
            .map_err(|e| -> Box<dyn std::error::Error> { format!("failed to load snapshot root: {}", e).into() })?;
        let mut cfg = build_mount_options(noatime, allow_other, None);
        cfg.mount_options.push(MountOption::RO);
        (snap_meta, snap_io, cfg)
    } else {
        let cfg = build_mount_options(noatime, allow_other, None);
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

    // ── macOS: Signal handler + watchdog ──────────────────────────────────────

    #[cfg(target_os = "macos")]
    {
        // Register SIGTERM/SIGINT handlers BEFORE mount2() to coordinate watchdog.
        // SAFETY: handler only does an atomic store (signal-safe).
        unsafe { register_signal_handlers(); }

        // Spawn watchdog thread BEFORE mount2() blocks.
        let watchdog_shutdown = Arc::new(AtomicBool::new(false));
        let watchdog_handle = spawn_watchdog(
            mountpoint.to_path_buf(),
            Duration::from_secs(5),
            Arc::clone(&watchdog_shutdown),
        );

        // ── Startup log (backend + FUSE-T version) ────────────────────────────
        println!(
            "SliceFS mounted at {} (backend: {}, fuse-t: {}.{}.{})",
            mountpoint.display(),
            selected_backend.as_mount_option().trim_start_matches("backend="),
            fuse_t_version.0,
            fuse_t_version.1,
            fuse_t_version.2,
        );

        // Primary mount attempt via CUSTOM mount option.
        let mount_result = mount2(fs, mountpoint, &config);

        // Shut down watchdog after mount2() returns.
        watchdog_shutdown.store(true, Ordering::SeqCst);
        let _ = watchdog_handle.join();

        // mount2 has returned — FUSE session ended, destroy() already called.
        // Shut down the GC thread before the MountLock drops.
        gc_shutdown.store(true, Ordering::SeqCst);
        gc_handle.shutdown();

        // Propagate mount error (possibly via ini fallback below).
        if let Err(e) = mount_result {
            let err_str = e.to_string();
            // Only fall back to ini if the error looks backend-related.
            if err_str.contains("backend") || err_str.contains("option") {
                eprintln!(
                    "[slicefs] CUSTOM mount option failed ({}), trying fuse-t.ini fallback",
                    err_str
                );
                with_fuse_t_ini_backend(
                    selected_backend.as_mount_option().trim_start_matches("backend="),
                    || Err(format!("fuse-t.ini fallback: original error: {}", err_str).into()),
                )?;
            } else {
                return Err(e.into());
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        println!("SliceFS mounted at {}", mountpoint.display());

        mount2(fs, mountpoint, &config)?;

        // mount2 has returned — FUSE session ended, destroy() already called.
        gc_shutdown.store(true, Ordering::SeqCst);
        gc_handle.shutdown();
    }

    // _mount_lock is dropped here, removing mount.lock from the store directory.
    println!("SliceFS unmounted.");

    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

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

    // ── build_mount_options ───────────────────────────────────────────────────

    // Helper: call build_mount_options with a platform-appropriate backend argument.
    #[cfg(target_os = "macos")]
    fn bmo(noatime: bool, allow_other: bool) -> Config {
        build_mount_options(noatime, allow_other, None)
    }
    #[cfg(not(target_os = "macos"))]
    fn bmo(noatime: bool, allow_other: bool) -> Config {
        build_mount_options(noatime, allow_other, None)
    }

    #[test]
    fn test_build_mount_options_with_noatime() {
        let config = bmo(true, false);
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
        let config = bmo(false, false);
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
    fn test_build_mount_options_no_direct_io() {
        // direct_io must NOT be present — it breaks FUSE-T's go-nfsv4 write path.
        let config = bmo(false, false);
        assert!(
            !config.mount_options.contains(&MountOption::CUSTOM("direct_io".to_string())),
            "CUSTOM(direct_io) must NOT be present — breaks FUSE-T write forwarding"
        );
    }

    #[test]
    fn test_build_mount_options_allow_other() {
        let config = bmo(false, true);
        assert!(
            matches!(config.acl, SessionACL::All),
            "SessionACL must be All when allow_other=true"
        );
    }

    #[test]
    fn test_build_mount_options_no_allow_other() {
        let config = bmo(false, false);
        assert!(
            matches!(config.acl, SessionACL::Owner),
            "SessionACL must be Owner when allow_other=false"
        );
    }

    /// Verify CUSTOM("backend=smb") is present when backend=Some(Smb).
    #[test]
    #[cfg(target_os = "macos")]
    fn test_build_mount_options_with_backend_smb() {
        use crate::backend::FuseTBackend;
        let config = build_mount_options(false, false, Some(&FuseTBackend::Smb));
        assert!(
            config.mount_options.contains(&MountOption::CUSTOM("backend=smb".to_string())),
            "CUSTOM(backend=smb) must be present when backend=Some(Smb)"
        );
    }

    /// Verify no CUSTOM("backend=*") present when backend=None.
    #[test]
    #[cfg(target_os = "macos")]
    fn test_build_mount_options_no_backend() {
        let config = build_mount_options(false, false, None);
        let has_backend_custom = config.mount_options.iter().any(|opt| {
            matches!(opt, MountOption::CUSTOM(s) if s.starts_with("backend="))
        });
        assert!(
            !has_backend_custom,
            "No CUSTOM(backend=*) should be present when backend=None"
        );
    }

    // ── WAL config ────────────────────────────────────────────────────────────

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

    // ── inject_backend_into_ini ───────────────────────────────────────────────

    #[test]
    #[cfg(target_os = "macos")]
    fn test_inject_backend_replaces_existing() {
        let ini = "[Default]\nbackend=nfs\nfoo=bar\n";
        let result = inject_backend_into_ini(ini, "smb");
        assert!(result.contains("backend=smb"), "should replace backend=nfs with backend=smb");
        assert!(!result.contains("backend=nfs"), "old backend line should be gone");
        assert!(result.contains("foo=bar"), "unrelated keys should be preserved");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn test_inject_backend_adds_when_absent() {
        let ini = "[Default]\nfoo=bar\n";
        let result = inject_backend_into_ini(ini, "smb");
        assert!(result.contains("backend=smb"), "should add backend=smb entry");
        assert!(result.contains("foo=bar"), "unrelated keys should be preserved");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn test_inject_backend_creates_section_when_missing() {
        let ini = "[Other]\nfoo=bar\n";
        let result = inject_backend_into_ini(ini, "fskit");
        assert!(result.contains("backend=fskit"), "should add [Default] section with backend=fskit");
        assert!(result.contains("[Default]"), "should add [Default] section header");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn test_inject_backend_empty_ini() {
        let result = inject_backend_into_ini("", "smb");
        assert!(result.contains("backend=smb"), "should handle empty ini");
        assert!(result.contains("[Default]"), "should add [Default] section");
    }
}
