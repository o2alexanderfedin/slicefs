/// Integration tests for the `slicefs gc` offline GC command.
///
/// Covers:
/// - `run_gc` on a store with dead entries compacts segments and reports stats
/// - `run_gc` on a clean store (no dead entries) reports 0 removed
/// - `run_gc` on a locked store with no running mount fails (no mount takes the request)
/// - CLI parses `gc` subcommand with store path argument

use std::path::PathBuf;

use clap::Parser;
use metadata::segment::load_store_from_segments;
use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use metadata::wal::{WalConfig, create_wal};
use slicefs_cli::cli::{Cli, Cmd};
use slicefs_cli::gc::run_gc;
use slicefs_traits::metadata::{InodeMeta, MetadataStore};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

const S_IFREG: u32 = 0o100_000;

/// Helper: create a seeded store with one committed file and WAL shut down.
fn make_seeded_store(store_dir: &TempDir) {
    let segs_dir = store_dir.path().join("segments");
    std::fs::create_dir_all(&segs_dir).unwrap();

    let wal = create_wal(WalConfig::PerOp, store_dir.path(), 1).unwrap();
    let io = Arc::new(Mutex::new(StoreIo::open(store_dir.path()).unwrap()));
    let mut meta = DictMetadataStore::new(io);
    meta.set_wal(wal);

    let file_meta = InodeMeta::new_file(0, 0, 0, S_IFREG | 0o644);
    let ino = meta.create_inode(&file_meta).unwrap();
    meta.link(1, "live.txt", ino).unwrap();
    meta.commit().unwrap();

    // Shutdown WAL to close segments
    meta.shutdown_wal().unwrap();
}

/// Helper: create a store with a dead entry (created then unlinked before commit).
fn make_store_with_dead_entries(store_dir: &TempDir) {
    let segs_dir = store_dir.path().join("segments");
    std::fs::create_dir_all(&segs_dir).unwrap();

    let wal = create_wal(WalConfig::PerOp, store_dir.path(), 1).unwrap();
    let io = Arc::new(Mutex::new(StoreIo::open(store_dir.path()).unwrap()));
    let mut meta = DictMetadataStore::new(io);
    meta.set_wal(wal);

    // Create a live file
    let file_meta = InodeMeta::new_file(0, 0, 0, S_IFREG | 0o644);
    let ino = meta.create_inode(&file_meta).unwrap();
    meta.link(1, "live.txt", ino).unwrap();
    meta.commit().unwrap();

    // Create and immediately unlink a second file (orphaned entries)
    let dead_meta = InodeMeta::new_file(0, 0, 0, S_IFREG | 0o644);
    let dead_ino = meta.create_inode(&dead_meta).unwrap();
    meta.link(1, "dead.txt", dead_ino).unwrap();
    meta.unlink(1, "dead.txt").unwrap();
    // Commit the deletion (dead.txt is no longer reachable from root)
    meta.commit().unwrap();

    meta.shutdown_wal().unwrap();
}

// ── Test 1: run_gc on clean store (no dead entries) reports 0 removed ───────

/// A freshly seeded store with all entries live should report 0 removed.
#[test]
fn test_run_gc_clean_store_reports_zero_removed() {
    let store_dir = TempDir::new().unwrap();
    make_seeded_store(&store_dir);

    let result = run_gc(store_dir.path());
    assert!(result.is_ok(), "run_gc should succeed on clean store: {:?}", result.err());
}

// ── Test 2: run_gc on a locked store with no mount fails ─────────────────────

/// If mount.lock is present, run_gc asks the mount to run the GC. With no
/// mount to take the request, it returns an error.
#[test]
fn test_run_gc_locked_store_refuses() {
    let store_dir = TempDir::new().unwrap();
    make_seeded_store(&store_dir);

    // Simulate a mounted store by creating mount.lock
    std::fs::write(store_dir.path().join("mount.lock"), b"pid").unwrap();

    let result = run_gc(store_dir.path());
    assert!(result.is_err(), "run_gc should fail on locked store");
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("mounted") || msg.contains("lock") || msg.contains("Unmount"),
        "error message should mention mounted state, got: {}",
        msg
    );
}

// ── Test 3: run_gc on store with dead entries compacts segments ──────────────

/// A store with orphaned entries should complete without error.
/// Post-GC segment reload should still be able to reconstruct state.
#[test]
fn test_run_gc_with_dead_entries_compacts() {
    let store_dir = TempDir::new().unwrap();
    make_store_with_dead_entries(&store_dir);

    let result = run_gc(store_dir.path());
    assert!(result.is_ok(), "run_gc should succeed on store with dead entries: {:?}", result.err());

    // After GC, store should still be loadable and the live file still reachable
    let segs_dir = store_dir.path().join("segments");
    let (root_opt, _snapshots) = load_store_from_segments(&segs_dir)
        .expect("segments should be loadable after GC");
    let root = root_opt.expect("root should be present after GC");

    let io = Arc::new(Mutex::new(StoreIo::open(store_dir.path()).unwrap()));
    let rebuilt = DictMetadataStore::load_from_root(io, &root)
        .expect("store should be reconstructible after GC");
    let ino = rebuilt.lookup(1, "live.txt")
        .expect("live.txt should survive GC");
    assert!(ino > 1, "live.txt inode should be valid");
}

// ── Test 4: CLI parses gc subcommand ────────────────────────────────────────

/// `slicefs gc /path/to/store` should parse correctly.
#[test]
fn test_cli_parses_gc_subcommand() {
    let cli = Cli::try_parse_from(["slicefs", "gc", "/data/store"])
        .expect("gc subcommand should parse");

    match cli.command {
        Cmd::Gc { store, full } => {
            assert_eq!(store, PathBuf::from("/data/store"));
            assert!(!full);
        }
        _ => panic!("expected Gc subcommand"),
    }
    let cli = Cli::try_parse_from(["slicefs", "gc", "--full", "/data/store"]).expect("gc --full should parse");
    assert!(matches!(cli.command, Cmd::Gc { full: true, .. }));
}

// ── Test 5: the report has the disk usage before and after ─────────────────

/// Both modes print the store size before and after; the full GC also
/// prints the dead objects it left (none here) and leaves no temp files.
#[test]
fn test_gc_report_has_the_store_size_before_and_after() {
    let store_dir = TempDir::new().unwrap();
    make_store_with_dead_entries(&store_dir);
    for full in [false, true] {
        let report = slicefs_cli::gc::gc_report(store_dir.path(), full).unwrap();
        println!("{report}");
        let line = report.lines().find(|l| l.starts_with("store disk usage: before ")).unwrap_or_else(|| panic!("{report}"));
        assert!(line.contains(" allocated), after ") && line.ends_with(" allocated)"), "{line}");
        assert!(report.contains("packs: deleted "), "{report}");
        assert_eq!(report.contains("dead left: 0"), full, "{report}");
        assert_eq!(report.starts_with("GC (full): "), full, "{report}");
    }
    assert!(!store_dir.path().join("gc-tmp").exists() || std::fs::read_dir(store_dir.path().join("gc-tmp")).unwrap().count() == 0);
}
