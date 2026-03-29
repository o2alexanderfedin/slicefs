/// Tests for WalStrategy trait implementations.
use metadata::segment::{SegmentEntry, SegmentReader};
use metadata::wal::{NoWal, PerOpWal, FlushOnFsyncWal, PeriodicWal, WalEntry, WalStrategy};
use slicefs_traits::digest::{Branches, Digest224, Digest256};
use tempfile::TempDir;

fn make_key(v: u32) -> Digest224 {
    [v, v + 1, v + 2, v + 3, v + 4, v + 5, v + 6]
}

fn make_branches(v: u32) -> Branches {
    let d: Digest256 = [v; 8];
    [d, d]
}

// ─── NoWal ──────────────────────────────────────────────────────────────────

/// NoWal::log_mutation returns Ok.
#[test]
fn test_no_wal_log_mutation() {
    let wal = NoWal;
    let entry = WalEntry::DictionaryAppend {
        key: make_key(1),
        branches: make_branches(1),
    };
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

/// PerOpWal::log_mutation with DictionaryAppend writes DictEntry to segment.
#[test]
fn test_per_op_wal_log_mutation_dict_entry() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("wal0.seg");

    let wal = PerOpWal::new(&path, 0).unwrap();
    let key = make_key(10);
    let branches = make_branches(10);
    wal.log_mutation(&WalEntry::DictionaryAppend { key, branches }).unwrap();
    wal.shutdown().unwrap();

    let entries: Vec<SegmentEntry> = SegmentReader::open(&path).unwrap().collect();
    assert_eq!(entries.len(), 1);
    match &entries[0] {
        SegmentEntry::DictEntry { key: k, branches: b } => {
            assert_eq!(k, &key);
            assert_eq!(b, &branches);
        }
        _ => panic!("expected DictEntry"),
    }
}

/// PerOpWal::log_mutation with RootUpdate writes RootUpdate to segment.
#[test]
fn test_per_op_wal_log_mutation_root_update() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("wal1.seg");

    let wal = PerOpWal::new(&path, 1).unwrap();
    let root = make_key(99);
    wal.log_mutation(&WalEntry::RootUpdate { root }).unwrap();
    wal.shutdown().unwrap();

    let entries: Vec<SegmentEntry> = SegmentReader::open(&path).unwrap().collect();
    assert_eq!(entries.len(), 1);
    match &entries[0] {
        SegmentEntry::RootUpdate { root: r } => assert_eq!(r, &root),
        _ => panic!("expected RootUpdate"),
    }
}

/// PerOpWal::flush_and_sync — entries written before flush are readable.
#[test]
fn test_per_op_wal_flush_and_sync() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("wal2.seg");

    let wal = PerOpWal::new(&path, 2).unwrap();
    wal.log_mutation(&WalEntry::DictionaryAppend {
        key: make_key(20),
        branches: make_branches(20),
    })
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
    wal.log_mutation(&WalEntry::DictionaryAppend {
        key: make_key(30),
        branches: make_branches(30),
    })
    .unwrap();
    // Do NOT call flush_and_sync; just read the segment
    // We can't safely open the file while wal holds it, so read after shutdown
    // without flush_and_sync first — entries should be missing from segment
    wal.shutdown_without_flush();

    let entries: Vec<SegmentEntry> = SegmentReader::open(&path).unwrap().collect();
    // Without flush, no entries on disk
    assert_eq!(entries.len(), 0, "buffered entries must not be on disk before flush");
}

/// FlushOnFsyncWal::flush_and_sync writes all buffered entries to segment.
#[test]
fn test_flush_on_fsync_wal_flush_writes_entries() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("wal4.seg");

    let wal = FlushOnFsyncWal::new(&path, 4).unwrap();
    wal.log_mutation(&WalEntry::DictionaryAppend {
        key: make_key(40),
        branches: make_branches(40),
    })
    .unwrap();
    wal.log_mutation(&WalEntry::RootUpdate { root: make_key(41) }).unwrap();
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
    wal.log_mutation(&WalEntry::DictionaryAppend {
        key: make_key(50),
        branches: make_branches(50),
    })
    .unwrap();
    wal.log_mutation(&WalEntry::RootUpdate { root: make_key(51) }).unwrap();
    wal.shutdown().unwrap();

    let entries: Vec<SegmentEntry> = SegmentReader::open(&path).unwrap().collect();
    assert_eq!(entries.len(), 2);
}
