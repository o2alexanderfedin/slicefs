//! Several handles on one inode (POSIX.1 2.9.7, ROADMAP Phase 19 criterion
//! 6): the handle table, writes seen through every handle, fsync through any
//! write handle, one finalize at the last release. Requests go through the
//! handlers, as the FUSE callbacks call them.

use std::path::Path;
use std::sync::{Arc, Mutex};

use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use metadata::wal::{create_wal, WalConfig};
use slicefs_cli::filesystem::SliceFsFilesystem;
use slicefs_cli::handlers::{self as h, CreateResult, DataResult, EmptyResult, OpenResult, WriteResult};
use slicefs_cli::oplog::{Op, Record};
use slicefs_traits::metadata::{MetaError, MetadataStore};
use tempfile::TempDir;

const S_IFREG: u32 = 0o100_000;

fn new_fs(store: &Path) -> SliceFsFilesystem {
    std::fs::create_dir_all(store.join("segments")).unwrap();
    let wal = create_wal(WalConfig::PerOp, store, 1).unwrap();
    let io = Arc::new(Mutex::new(StoreIo::open(store).unwrap()));
    let mut meta = DictMetadataStore::new(io.clone());
    meta.set_wal(wal);
    let fs = SliceFsFilesystem::new(meta, io, Some(store.to_path_buf()));
    fs.meta().commit().unwrap();
    fs
}

fn create(fs: &SliceFsFilesystem, parent: u64, name: &str) -> (u64, u64) {
    match h::handle_create(fs, parent, name, S_IFREG | 0o644, 0o022, 501, 20, libc::O_RDWR) {
        CreateResult::Ok(ino, fh, _) => (ino, fh),
        _ => panic!("create {name}"),
    }
}

fn open(fs: &SliceFsFilesystem, ino: u64, flags: i32) -> u64 {
    match h::handle_open(fs, ino, flags) {
        OpenResult::Ok(fh, _) => fh,
        OpenResult::Error(e) => panic!("open {ino}: errno {e}"),
    }
}

fn write(fs: &SliceFsFilesystem, fh: u64, off: u64, bytes: &[u8]) {
    assert!(matches!(h::handle_write(fs, fh, off, bytes), WriteResult::Ok(_)), "write fh {fh}");
}

fn read(fs: &SliceFsFilesystem, ino: u64, off: u64, size: u32) -> Vec<u8> {
    match h::handle_read(fs, ino, off, size) {
        DataResult::Ok(d) => d,
        DataResult::Error(e) => panic!("read {ino}: errno {e}"),
    }
}

fn release(fs: &SliceFsFilesystem, ino: u64, fh: u64) {
    assert!(matches!(h::handle_release(fs, ino, fh), EmptyResult::Ok), "release fh {fh}");
}

/// The `Release` records of the write log so far.
fn release_records(fs: &SliceFsFilesystem, store: &Path) -> usize {
    fs.meta().flush_wal().unwrap();
    let (_, _, log) = metadata::segment::scan_store_with_log(&store.join("segments")).unwrap();
    log.records()
        .filter(|r| matches!(Record::decode(r.as_ref().unwrap()), Some(Record { op: Op::Release { .. }, .. })))
        .count()
}

#[test]
fn open_count_tracks_handles_and_orphans() {
    let dir = TempDir::new().unwrap();
    let fs = new_fs(dir.path());
    let (ino, fh) = create(&fs, 1, "f");
    write(&fs, fh, 0, b"hello");
    release(&fs, ino, fh);

    let a = open(&fs, ino, libc::O_RDONLY);
    let b = open(&fs, ino, libc::O_RDWR);
    assert!(matches!(h::handle_unlink(&fs, 1, "f"), EmptyResult::Ok));
    release(&fs, ino, a);
    let meta = fs.test_getattr(ino).expect("an orphan stays while a handle is open");
    assert_eq!(meta.nlinks, 0);
    release(&fs, ino, b);
    assert!(matches!(fs.test_getattr(ino), Err(MetaError::NotFound(_))), "the last close reclaims the orphan");
}

#[test]
fn a_read_only_release_logs_no_release_record() {
    let dir = TempDir::new().unwrap();
    let fs = new_fs(dir.path());
    let (ino, fh) = create(&fs, 1, "f");
    write(&fs, fh, 0, b"hello");
    release(&fs, ino, fh);
    let before = release_records(&fs, dir.path());
    assert_eq!(before, 1, "the write handle's release is logged");

    let ro = open(&fs, ino, libc::O_RDONLY);
    assert_eq!(read(&fs, ino, 0, 16), b"hello");
    release(&fs, ino, ro);
    assert_eq!(release_records(&fs, dir.path()), before, "a read-only release is not logged");
}
