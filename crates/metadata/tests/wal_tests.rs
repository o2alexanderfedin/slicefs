/// Tests for WalStrategy trait implementations.
use metadata::segment::{SegmentEntry, SegmentReader};
use metadata::wal::{FlushOnFsyncWal, NoWal, PerOpWal, PeriodicWal, WalEntry, WalStrategy};
use slicefs_traits::digest::Digest224;
use tempfile::TempDir;

fn make_key(v: u32) -> Digest224 {
    [v, v + 1, v + 2, v + 3, v + 4, v + 5, v + 6]
}

// ─── NoWal ──────────────────────────────────────────────────────────────────

/// NoWal::log_mutation returns Ok.
#[test]
fn test_no_wal_log_mutation() {
    let wal = NoWal;
    let entry = WalEntry::RootUpdate { root: make_key(1) };
    assert!(wal.log_mutation(&entry).is_ok());
}

/// NoWal::flush_and_sync returns Ok.
#[test]
fn test_no_wal_flush_and_sync() {
    let wal = NoWal;
    assert!(wal.flush_and_sync().is_ok());
}

/// NoWal::shutdown returns Ok.
#[test]
fn test_no_wal_shutdown() {
    let wal = NoWal;
    assert!(wal.shutdown().is_ok());
}

// ─── PerOpWal ────────────────────────────────────────────────────────────────

/// PerOpWal::log_mutation with RootUpdate writes RootUpdate to segment.
#[test]
fn test_per_op_wal_log_mutation_root_update() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("wal0.seg");

    let wal = PerOpWal::new(&path, 0).unwrap();
    let root = make_key(10);
    wal.log_mutation(&WalEntry::RootUpdate { root }).unwrap();
    wal.shutdown().unwrap();

    let entries: Vec<SegmentEntry> = SegmentReader::open(&path).unwrap().collect();
    assert_eq!(entries.len(), 1);
    match &entries[0] {
        SegmentEntry::RootUpdate { root: r } => {
            assert_eq!(r, &root);
        }
        _ => panic!("expected RootUpdate"),
    }
}

/// PerOpWal::log_mutation with Snapshot writes SnapshotRecord to segment.
#[test]
fn test_per_op_wal_log_mutation_snapshot() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("wal1.seg");

    let wal = PerOpWal::new(&path, 1).unwrap();
    let root = make_key(99);
    wal.log_mutation(&WalEntry::Snapshot {
        version: 1,
        root,
        created_at: 12345,
        name: Some("v1".to_string()),
    })
    .unwrap();
    wal.shutdown().unwrap();

    let entries: Vec<SegmentEntry> = SegmentReader::open(&path).unwrap().collect();
    assert_eq!(entries.len(), 1);
    match &entries[0] {
        SegmentEntry::SnapshotRecord {
            version,
            root: r,
            name,
            ..
        } => {
            assert_eq!(*version, 1);
            assert_eq!(r, &root);
            assert_eq!(name.as_deref(), Some("v1"));
        }
        _ => panic!("expected SnapshotRecord"),
    }
}

/// PerOpWal::flush_and_sync — entries written before flush are readable.
#[test]
fn test_per_op_wal_flush_and_sync() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("wal2.seg");

    let wal = PerOpWal::new(&path, 2).unwrap();
    wal.log_mutation(&WalEntry::RootUpdate { root: make_key(20) })
        .unwrap();
    wal.flush_and_sync().unwrap();
    wal.shutdown().unwrap();

    let entries: Vec<SegmentEntry> = SegmentReader::open(&path).unwrap().collect();
    assert_eq!(entries.len(), 1);
}

// ─── FlushOnFsyncWal ─────────────────────────────────────────────────────────

/// FlushOnFsyncWal::log_mutation alone does NOT write to segment.
#[test]
fn test_flush_on_fsync_wal_buffers_entries() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("wal3.seg");

    let wal = FlushOnFsyncWal::new(&path, 3).unwrap();
    wal.log_mutation(&WalEntry::RootUpdate { root: make_key(30) })
        .unwrap();
    // Do NOT call flush_and_sync; just read the segment
    // We can't safely open the file while wal holds it, so read after shutdown
    // without flush_and_sync first — entries should be missing from segment
    wal.shutdown_without_flush();

    let entries: Vec<SegmentEntry> = SegmentReader::open(&path).unwrap().collect();
    // Without flush, no entries on disk
    assert_eq!(
        entries.len(),
        0,
        "buffered entries must not be on disk before flush"
    );
}

/// FlushOnFsyncWal::flush_and_sync writes all buffered entries to segment.
#[test]
fn test_flush_on_fsync_wal_flush_writes_entries() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("wal4.seg");

    let wal = FlushOnFsyncWal::new(&path, 4).unwrap();
    wal.log_mutation(&WalEntry::RootUpdate { root: make_key(40) })
        .unwrap();
    wal.log_mutation(&WalEntry::RootUpdate { root: make_key(41) })
        .unwrap();
    wal.flush_and_sync().unwrap();
    wal.shutdown().unwrap();

    let entries: Vec<SegmentEntry> = SegmentReader::open(&path).unwrap().collect();
    assert_eq!(entries.len(), 2);
}

// ─── PeriodicWal ─────────────────────────────────────────────────────────────

/// PeriodicWal::shutdown writes all pending entries.
#[test]
fn test_periodic_wal_shutdown_flushes_entries() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("wal5.seg");

    let wal = PeriodicWal::new(&path, 5).unwrap();
    wal.log_mutation(&WalEntry::RootUpdate { root: make_key(50) })
        .unwrap();
    wal.log_mutation(&WalEntry::RootUpdate { root: make_key(51) })
        .unwrap();
    wal.shutdown().unwrap();

    let entries: Vec<SegmentEntry> = SegmentReader::open(&path).unwrap().collect();
    assert_eq!(entries.len(), 2);
}
