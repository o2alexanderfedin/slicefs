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
//!   dictionary.bin         # Legacy format — refused at mount; re-seed needed
//!   root.bin               # Legacy format — not read
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

use fuser::{mount, Config, MountOption, SessionACL};
use metadata::gc::background::spawn_background_gc;
use metadata::mount_lock::{acquire_mount_lock, MountLock, MountLockError};
use metadata::segment::load_store_from_segments;
use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use metadata::wal::{WalConfig, create_wal};
use crate::filesystem::{DirtyConfig, SliceFsFilesystem};
use crate::util::ts;

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
///
/// `hash` is `--hash`: a new write hash, recorded under the mount lock
/// before the store opens (`None` keeps the recorded one).
pub fn load_store(
    store_path: &Path,
    wal_config: WalConfig,
    hash: Option<&str>,
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

    // Step 4: Reconstruct the metadata store on the chunk store (StoreIo), with the
    // write hash of `--hash` (recorded now, under the lock).
    crate::content::select_hash(store_path, hash)?;
    chunk_store::retry::LOG_SYNCS.store(true, std::sync::atomic::Ordering::Relaxed);
    let io = Arc::new(Mutex::new(crate::content::store_io(store_path)?));
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
/// Sets the ACL to `SessionACL::All` when `allow_other` is true.
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
    // macOS FUSE-T: pass the backend as `backend=<name>`; libfuse-t turns it into
    // `go-nfsv4 --backend <name>`. (The earlier "Invalid request" on FUSE-T 1.2.x
    // was not caused by this option but by fuser's INIT reply size — fixed in
    // vendor/fuser, see PATCHES.md.)
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

/// Debug builds only (POOL-03 live check): with the variable below set to
/// a number k of 1 or more, the k-th new entry of this mount panics at its
/// half-done point (linked, its record not appended). Creates, mknods,
/// symlinks and mkdirs count together. Any other value is ignored.
#[cfg(debug_assertions)]
fn set_test_panic_create(fs: &SliceFsFilesystem) {
    let Ok(v) = std::env::var("SLICEFS_TEST_PANIC_CREATE") else { return };
    let k = match v.trim().parse::<u64>() {
        Ok(k) if k >= 1 => k,
        _ => {
            eprintln!("[{}] WARNING: SLICEFS_TEST_PANIC_CREATE={v:?} is not a number of 1 or more; ignored", ts());
            return;
        }
    };
    eprintln!(
        "[{}] WARNING: test panic set: new entry number {k} panics (creates, mknods, symlinks and mkdirs count together)",
        ts()
    );
    let seen = std::sync::atomic::AtomicU64::new(0);
    fs.test_set_mkdir_hook(Some(Arc::new(move || {
        if seen.fetch_add(1, Ordering::SeqCst) + 1 == k {
            panic!("SLICEFS_TEST_PANIC_CREATE: test panic in new entry number {k}");
        }
    })));
}

// ── run_mount ─────────────────────────────────────────────────────────────────

/// Run the `mount` subcommand.
///
/// Loads the store, constructs the FUSE filesystem, and starts a blocking
/// `fuser::mount` session. Blocks until the session ends (SIGTERM, Ctrl+C,
/// or `slicefs unmount`).
///
/// `_cache_size` is accepted but unused (there is no read cache).
/// `allow_other` passes the `allow_other` FUSE mount option (multi-user access).
/// `snapshot_ref` mounts a specific snapshot read-only (by version number or name).
/// `auto_snapshot` enables auto-snapshot on clean unmount (--auto-snapshot flag).
/// `backend` is the FUSE-T backend (macOS only): "fskit", the default and only one.
/// `force` is accepted but currently unused.
/// `dirty` holds the dirty-data values; they are checked before any side
/// effect and set before the write-log replay.
/// `workers` is the number of request worker threads (`None`: the default
/// of `pool::default_workers`); with 1 there is no pool.
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
    commit_interval_secs: u64,
    gc_interval_secs: u64,
    zstd_level: i32,
    hash: Option<&str>,
    dirty: DirtyConfig,
    workers: Option<usize>,
) -> Result<(), Box<dyn std::error::Error>> {
    // The allocator first, before any thread of the mount starts (Phase 21 [Allocator]).
    let mmap_threshold = crate::alloc_tune::tune_at_start();
    // Pre-flight: refuse bad dirty-data values before anything changes.
    dirty.check()?;

    // The request worker count, read once (Phase 21).
    let (n_workers, how) = match workers {
        Some(n) => (n, "set"),
        None => (crate::pool::default_workers(), "default"),
    };
    if n_workers == 0 {
        return Err("--workers must be at least 1".into());
    }
    println!("Workers: {n_workers} ({how})");
    match mmap_threshold {
        Some(b) => println!("Allocator: mmap threshold {b} bytes (fixed)"),
        None => println!("Allocator: system default"),
    }

    // Pre-flight: create mountpoint directory if it doesn't exist.
    if !mountpoint.exists() {
        std::fs::create_dir_all(mountpoint)
            .map_err(|e| format!("failed to create mountpoint {}: {}", mountpoint.display(), e))?;
    }

    // Pre-flight: reject if mountpoint is already in use.
    check_mountpoint_not_in_use(mountpoint)?;

    // Pre-flight: refuse a store of an older format (metadata in vt0/).
    crate::content::check_format(store_path)?;

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

    let (meta, io, _mount_lock) = load_store(store_path, wal_config, hash)?;

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
    if fs.content().punches_holes() {
        println!("Chunk store: GC frees space with hole punching and compaction.");
    } else {
        println!("Chunk store: this file system cannot punch holes; GC frees space by compaction only.");
    }
    fs.content().set_zstd_level(zstd_level);
    if zstd_level > 0 {
        println!("Compression of new data: zstd level {zstd_level}.");
    } else {
        println!("Compression of new data: none.");
    }

    fs.set_dirty_config(&dirty);

    // Replay the write log (#20): requests after the last root, made durable
    // by fsync before a crash. Then commit, which cuts the log. Not on a
    // snapshot mount (read-only; the log stays for the next normal mount).
    if snapshot_ref.is_none() {
        let stats = crate::oplog::replay_log(&fs, &store_path.join("segments"))
            .map_err(|e| format!("write log replay failed: {}", e))?;
        if stats.applied + stats.skipped > 0 {
            slicefs_traits::metadata::MetadataStore::commit(fs.meta().as_ref())
                .map_err(|e| format!("commit after replay failed: {}", e))?;
            fs.meta().flush_wal().map_err(|e| format!("sync after replay failed: {}", e))?;
            println!(
                "Replayed {} request(s) from the write log ({} skipped).",
                stats.applied, stats.skipped
            );
        }
    }
    let fs = Arc::new(fs);

    // The dirty-data flusher (Phase 20). Not on a snapshot mount (read-only).
    // The clone is for the stop after mount() returns.
    let flusher_fs = snapshot_ref.is_none().then(|| {
        fs.start_flusher();
        Arc::clone(&fs)
    });

    // GC of the stored objects while mounted (#25). Not on a snapshot mount:
    // its metadata is the snapshot, not the current root.
    let online_gc_shutdown = Arc::new(AtomicBool::new(false));
    let online_gc_handle = snapshot_ref.is_none().then(|| {
        crate::online_gc::spawn_online_gc(
            Arc::downgrade(&fs),
            store_path.to_path_buf(),
            Duration::from_secs(gc_interval_secs),
            Arc::clone(&online_gc_shutdown),
        )
    });

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

    // Commit metadata periodically, so a crash does not lose everything since mount (#18).
    let commit_shutdown = Arc::new(AtomicBool::new(false));
    let commit_handle = (commit_interval_secs > 0).then(|| {
        crate::commit_timer::spawn_commit_timer(
            Arc::downgrade(&fs),
            fs.commit_gate(),
            Duration::from_secs(commit_interval_secs),
            Arc::clone(&commit_shutdown),
        )
    });

    // ── Mount + startup log ────────────────────────────────────────────────────

    // macOS: the unmount watcher prints this when the mount really appears.
    #[cfg(target_os = "macos")]
    let mounted_msg = format!(
        "SliceFS mounted at {} (backend: {}, fuse-t: {}.{}.{})",
        mountpoint.display(),
        selected_backend.as_mount_option().trim_start_matches("backend="),
        fuse_t_version.0,
        fuse_t_version.1,
        fuse_t_version.2,
    );

    #[cfg(not(target_os = "macos"))]
    println!("SliceFS mounted at {}", mountpoint.display());

    // mount blocks until the FUSE session ends. SIGTERM, SIGINT and SIGHUP
    // unmount the mount point, so the session ends and destroy() runs (the
    // vendored fuser registers no signal handlers).
    {
        let mp = mountpoint.to_path_buf();
        if let Err(e) = crate::signals::install(move |_| crate::unmount::try_soft_unmount(&mp)) {
            eprintln!("[{}] WARNING: no signal handlers ({e}); stop the mount with `slicefs unmount`", ts());
        }
    }
    // The FUSE-T server may not exit at unmount (its SMB server did not), so
    // the session would never end (no destroy, no final commit): end it when
    // the mount is gone.
    #[cfg(target_os = "macos")]
    let unmount_watch_shutdown = Arc::new(AtomicBool::new(false));
    #[cfg(target_os = "macos")]
    let unmount_watch =
        crate::fuset_watch::spawn_unmount_watcher(mountpoint, mounted_msg, Arc::clone(&unmount_watch_shutdown));

    // POOL-03: a panic on a request thread (the reader or a worker) aborts
    // the daemon before the unwind: no reply for the half-done request and
    // no destroy() commit of it; the write log replays at the next mount.
    crate::pool::install_abort_hook();
    // Debug builds only: the test panic of the POOL-03 live check. It is set
    // after the write-log replay, so the entries of the replay do not count.
    #[cfg(debug_assertions)]
    set_test_panic_create(&fs);

    // The request pool: only for 2 or more workers (1 runs each request on
    // the reader thread). A spawn error is a mount error, and goes through
    // the stop steps below.
    #[allow(unused_mut)]
    let mut mounted = match (n_workers >= 2).then(|| crate::pool::Pool::new(n_workers)).transpose() {
        Ok(pool) => mount(crate::fuse_callbacks::MountedFs::new(fs, pool), mountpoint, &config),
        Err(e) => Err(std::io::Error::other(format!("cannot start {n_workers} request workers: {e}"))),
    };

    #[cfg(target_os = "macos")]
    {
        unmount_watch_shutdown.store(true, Ordering::SeqCst);
        let seen = unmount_watch.join().unwrap_or(true);
        if mounted.is_ok() && !seen {
            mounted = Err(std::io::Error::other(format!(
                "the FUSE-T session ended, but no volume was mounted at {} (backend: {}). \
                 If the FUSE-T extension is not enabled: {}",
                mountpoint.display(),
                selected_backend.as_mount_option().trim_start_matches("backend="),
                crate::backend::ENABLE_FSKIT_HINT,
            )));
        }
    }

    // Stop the flusher, also when mount failed (after a clean unmount
    // destroy() stopped it already; a second stop does nothing). The clone
    // is dropped at once, so it does not keep the file system alive.
    if let Some(fs) = flusher_fs {
        fs.stop_flusher();
    }

    // Stop the GC thread before mount.lock is released, also when mount failed.
    online_gc_shutdown.store(true, Ordering::SeqCst);
    if let Some(h) = online_gc_handle {
        let _ = h.join();
    }
    mounted?;

    // mount has returned — FUSE session ended, destroy() already called.
    commit_shutdown.store(true, Ordering::SeqCst);
    if let Some(h) = commit_handle {
        let _ = h.join();
    }
    gc_shutdown.store(true, Ordering::SeqCst);
    gc_handle.shutdown();

    print_lock_rows();

    // _mount_lock is dropped here, removing mount.lock from the store directory.
    println!("SliceFS unmounted.");

    Ok(())
}

/// A lock-stats build (feature `lock-stats`) prints one `lock_row` line per
/// lock site that was taken during the mount; a normal build prints nothing.
fn print_lock_rows() {
    if !chunk_store::lock_stats::ENABLED {
        return;
    }
    let us = |ns: u64| ns as f64 / 1000.0;
    for st in chunk_store::lock_stats::snapshot().iter().filter(|s| s.count > 0) {
        println!(
            "lock_row site={} count={} wait_p99_us={:.1} wait_max_us={:.1} hold_p99_us={:.1} hold_max_us={:.1}",
            st.label,
            st.count,
            us(st.wait_p99_ns),
            us(st.wait_max_ns),
            us(st.hold_p99_ns),
            us(st.hold_max_ns)
        );
    }
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

        let io = Arc::new(Mutex::new(StoreIo::open(store_dir.path()).unwrap()));
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
    fn dirty_config_check_refuses_a_limit_above_the_budget() {
        let mib = 1 << 20;
        let cfg = DirtyConfig { budget: 8 * mib, limit: 8 * mib, ..DirtyConfig::default() };
        assert_eq!(cfg.check(), Ok(()), "a limit equal to the budget is Ok");
        assert_eq!(DirtyConfig::default().check(), Ok(()));
        let cfg = DirtyConfig { budget: 8 * mib, limit: 16 * mib, ..DirtyConfig::default() };
        let err = cfg.check().expect_err("a limit above the budget is refused");
        assert!(err.contains("16777216") && err.contains("8388608"), "the message has both values: {err}");
        for bad in [
            DirtyConfig { budget: 0, limit: 0, ..DirtyConfig::default() },
            DirtyConfig { limit: 4 * mib - 1, ..DirtyConfig::default() },
            DirtyConfig { idle_secs: 0, ..DirtyConfig::default() },
            DirtyConfig { expire_secs: 0, ..DirtyConfig::default() },
            DirtyConfig { expire_secs: u64::MAX / 1000 + 1, ..DirtyConfig::default() },
        ] {
            assert!(bad.check().is_err(), "{bad:?} is refused");
        }
    }

    /// A limit above the budget stops the mount before the mount point is
    /// made (no mount is tried).
    #[test]
    fn run_mount_refuses_a_dirty_limit_above_the_budget_first() {
        let dir = tempfile::tempdir().unwrap();
        let mnt = dir.path().join("mnt");
        let cfg = DirtyConfig { budget: 8 << 20, limit: 16 << 20, ..DirtyConfig::default() };
        let r = run_mount(&dir.path().join("no-store"), &mnt, true, false, 0, None, None, false, None, false, 0, 0, 0, None, cfg, None);
        let err = r.expect_err("run_mount refuses the dirty values").to_string();
        assert!(!mnt.exists(), "the mount point was not made ({err})");
        assert!(err.contains("16777216") && err.contains("8388608"), "{err}");
    }

    #[test]
    fn test_load_store_returns_correct_inode_1() {
        let store_dir = tempfile::tempdir().unwrap();
        write_seeded_store_segments(&store_dir);

        let (meta, _io, _lock) = load_store(store_dir.path(), WalConfig::NoWal, None).expect("load_store failed");

        // Inode 1 must exist and be a directory (root)
        let root_meta = meta.get_inode(1).expect("root inode missing");
        let kind = root_meta.mode & 0o170_000;
        assert_eq!(kind, S_IFDIR, "inode 1 should be a directory");
    }

    #[test]
    fn test_load_store_finds_seeded_file() {
        let store_dir = tempfile::tempdir().unwrap();
        write_seeded_store_segments(&store_dir);

        let (meta, _io, _lock) = load_store(store_dir.path(), WalConfig::NoWal, None).expect("load_store failed");

        // hello.txt was seeded
        let ino = meta.lookup(1, "hello.txt").expect("hello.txt not found");
        assert!(ino > 1, "file inode should be > 1");
    }

    #[test]
    fn test_load_store_rejects_legacy_format() {
        let store_dir = tempfile::tempdir().unwrap();
        // Write a dictionary.bin to simulate legacy format
        std::fs::write(store_dir.path().join("dictionary.bin"), b"").unwrap();

        let result = load_store(store_dir.path(), WalConfig::NoWal, None);
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
        let result = load_store(store_dir.path(), WalConfig::NoWal, None);
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

        let (meta, _io, _lock) = load_store(store_dir.path(), WalConfig::NoWal, None)
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

    /// Verify CUSTOM("backend=fskit") is injected when backend=Some(Fskit).
    #[test]
    #[cfg(target_os = "macos")]
    fn test_build_mount_options_passes_backend_on_macos() {
        use crate::backend::FuseTBackend;
        let config = build_mount_options(false, false, Some(&FuseTBackend::Fskit));
        let customs: Vec<_> = config.mount_options.iter().filter_map(|opt| match opt {
            MountOption::CUSTOM(s) => Some(s.as_str()),
            _ => None,
        }).collect();
        assert_eq!(customs, vec!["backend=fskit"], "backend must reach FUSE-T as backend=<name>");
        let none = build_mount_options(false, false, None);
        assert!(!none.mount_options.iter().any(|o| matches!(o, MountOption::CUSTOM(_))));
    }

    /// The backend goes to FUSE-T as one custom option, and nothing else.
    #[cfg(target_os = "macos")]
    #[test]
    fn test_build_mount_options_backend_only_custom_option() {
        use crate::backend::FuseTBackend;
        let customs = |b| -> Vec<String> {
            build_mount_options(false, false, Some(b)).mount_options.iter().filter_map(|o| match o {
                MountOption::CUSTOM(s) => Some(s.clone()),
                _ => None,
            }).collect()
        };
        assert_eq!(customs(&FuseTBackend::Fskit), vec!["backend=fskit"]);
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

    // ── next_segment_id ───────────────────────────────────────────────────────

    #[test]
    fn test_next_segment_id_empty_dir_returns_1() {
        let dir = tempfile::tempdir().unwrap();
        let id = next_segment_id(dir.path());
        assert_eq!(id, 1, "empty dir should return segment id 1");
    }

    #[test]
    fn test_next_segment_id_with_single_segment() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("segment-000005.seg"), b"").unwrap();
        let id = next_segment_id(dir.path());
        assert_eq!(id, 6, "next id after segment 5 should be 6");
    }

    #[test]
    fn test_next_segment_id_with_multiple_segments() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("segment-000001.seg"), b"").unwrap();
        std::fs::write(dir.path().join("segment-000002.seg"), b"").unwrap();
        std::fs::write(dir.path().join("segment-000042.seg"), b"").unwrap();
        let id = next_segment_id(dir.path());
        assert_eq!(id, 43, "next id should be max + 1");
    }

    #[test]
    fn test_next_segment_id_ignores_non_segment_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("mount.lock"), b"").unwrap();
        std::fs::write(dir.path().join("dictionary.bin"), b"").unwrap();
        std::fs::write(dir.path().join("segment-000003.seg"), b"").unwrap();
        let id = next_segment_id(dir.path());
        assert_eq!(id, 4, "should ignore non-segment files");
    }

    #[test]
    fn test_next_segment_id_nonexistent_dir_returns_1() {
        let id = next_segment_id(std::path::Path::new("/nonexistent/path/that/cannot/exist"));
        assert_eq!(id, 1, "nonexistent dir should return 1");
    }

    #[test]
    fn test_next_segment_id_only_malformed_files_returns_1() {
        let dir = tempfile::tempdir().unwrap();
        // Files that look like segments but aren't parseable
        std::fs::write(dir.path().join("segment-abc.seg"), b"").unwrap();
        std::fs::write(dir.path().join("segment-.seg"), b"").unwrap();
        std::fs::write(dir.path().join("notsegment-000001.seg"), b"").unwrap();
        let id = next_segment_id(dir.path());
        assert_eq!(id, 1, "malformed segment files should be ignored");
    }

    // ── load_store dirty mount recovery ──────────────────────────────────────

    #[test]
    fn test_load_store_dirty_mount_recovery() {
        // Simulate a dirty mount: write a seeded store, then create a stale
        // mount.lock file before loading. load_store should recover gracefully.
        let store_dir = tempfile::tempdir().unwrap();
        write_seeded_store_segments(&store_dir);

        // Create a stale mount.lock to simulate a crashed previous mount.
        let lock_path = store_dir.path().join("mount.lock");
        std::fs::write(&lock_path, b"stale").unwrap();

        // load_store should detect DirtyMount, remove the stale lock, and recover.
        let result = load_store(store_dir.path(), WalConfig::NoWal, None);
        assert!(result.is_ok(), "dirty mount recovery should succeed");

        // The store should be loadable — hello.txt was seeded.
        let (meta, _io, _lock) = result.unwrap();
        let ino = meta.lookup(1, "hello.txt");
        assert!(ino.is_ok(), "hello.txt should be accessible after dirty mount recovery");
    }

    // ── parse_wal_config unknown strategy fallback ────────────────────────────

    #[test]
    fn test_parse_wal_config_unknown_defaults_to_per_op() {
        // Unknown strategy strings should fall through to PerOp default.
        assert!(matches!(parse_wal_config(Some("invalid-strategy")), WalConfig::PerOp));
        assert!(matches!(parse_wal_config(Some("FLUSH-ON-FSYNC")), WalConfig::PerOp));
        assert!(matches!(parse_wal_config(Some("")), WalConfig::PerOp));
    }

    // ── build_mount_options additional combinations ───────────────────────────

    #[test]
    fn test_build_mount_options_noatime_and_allow_other() {
        #[cfg(target_os = "macos")]
        let config = build_mount_options(true, true, None);
        #[cfg(not(target_os = "macos"))]
        let config = build_mount_options(true, true, None);
        assert!(config.mount_options.contains(&MountOption::NoAtime));
        assert!(matches!(config.acl, SessionACL::All));
    }

    #[test]
    fn test_build_mount_options_always_has_fsname() {
        #[cfg(target_os = "macos")]
        let config = build_mount_options(false, false, None);
        #[cfg(not(target_os = "macos"))]
        let config = build_mount_options(false, false, None);
        let has_fsname = config.mount_options.iter().any(|opt| {
            matches!(opt, MountOption::FSName(s) if s == "slicefs")
        });
        assert!(has_fsname, "FSName(slicefs) must always be present");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn test_build_mount_options_no_noappledouble_on_macos() {
        // FUSE-T 1.2.0 rejects unknown CUSTOM options. noappledouble is handled
        // internally by go-nfsv4's --namedattr flag.
        let config = build_mount_options(false, false, None);
        let has_custom = config.mount_options.iter().any(|opt| {
            matches!(opt, MountOption::CUSTOM(_))
        });
        assert!(!has_custom, "No CUSTOM options on macOS — FUSE-T rejects them");
    }
}
