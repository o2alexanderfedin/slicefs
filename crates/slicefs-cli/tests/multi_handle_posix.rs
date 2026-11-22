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

fn fsync(fs: &SliceFsFilesystem, ino: u64, fh: u64) {
    assert!(matches!(h::handle_fsync(fs, ino, fh), EmptyResult::Ok), "fsync fh {fh}");
}

fn data(seed: u64, n: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..n).map(|_| { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x as u8 }).collect()
}

/// The committed content of `ino` (its manifest root), or empty.
fn committed(fs: &SliceFsFilesystem, ino: u64) -> Vec<u8> {
    match fs.meta().get_manifest(ino).unwrap_or_default().first() {
        Some(k) => fs.read_content(k).expect("manifest root readable"),
        None => Vec::new(),
    }
}

/// What a mount does: load the last root (and replay the log).
fn reload(store: &Path) -> SliceFsFilesystem {
    let (root, snaps) = metadata::segment::load_store_from_segments(&store.join("segments")).unwrap();
    let io = Arc::new(Mutex::new(StoreIo::open(store).unwrap()));
    let mut meta = DictMetadataStore::load_from_root(io.clone(), &root.unwrap()).unwrap();
    meta.set_snapshots(snaps);
    let fs = SliceFsFilesystem::new(meta, io, Some(store.to_path_buf()));
    slicefs_cli::oplog::replay_log(&fs, &store.join("segments")).unwrap();
    fs
}

const BLOCK: usize = 64 << 10;

#[test]
fn visibility_across_handles() {
    let dir = TempDir::new().unwrap();
    let fs = new_fs(dir.path());
    let (ino, a) = create(&fs, 1, "f");
    let b = open(&fs, ino, libc::O_RDONLY);
    let bytes = data(1, BLOCK);
    write(&fs, a, 0, &bytes);
    assert!(read(&fs, ino, 0, BLOCK as u32) == bytes, "read-only handle opened before the write sees it before any fsync");
    let c = open(&fs, ino, libc::O_RDWR);
    assert!(read(&fs, ino, 0, BLOCK as u32) == bytes, "handle opened after the write sees it");
    for fh in [a, b, c] {
        release(&fs, ino, fh);
    }
    assert!(read(&fs, ino, 0, BLOCK as u32) == bytes, "after the last release");
}

#[test]
fn fsync_through_any_write_handle_commits() {
    let dir = TempDir::new().unwrap();
    let fs = new_fs(dir.path());
    let (ino, a) = create(&fs, 1, "f");
    let b = open(&fs, ino, libc::O_RDWR);
    let bytes = data(2, BLOCK);
    write(&fs, a, 0, &bytes);
    fsync(&fs, ino, b);
    assert!(committed(&fs, ino) == bytes, "fsync through B writes A's bytes into the manifest");
    release(&fs, ino, a);
    release(&fs, ino, b);
    fs.meta().commit().unwrap();
    drop(fs);
    let fs = reload(dir.path());
    assert!(read(&fs, ino, 0, BLOCK as u32 + 1) == bytes, "after a reload of the store");
}

#[test]
fn last_release_finalizes_once() {
    let dir = TempDir::new().unwrap();
    let fs = new_fs(dir.path());
    let (ino, a) = create(&fs, 1, "f");
    let b = open(&fs, ino, libc::O_RDWR);
    let (b0, b1) = (data(3, BLOCK), data(4, BLOCK));
    write(&fs, a, 0, &b0);
    write(&fs, b, BLOCK as u64, &b1);
    release(&fs, ino, a);
    assert!(committed(&fs, ino).is_empty(), "the first release does not finalize");
    release(&fs, ino, b);
    let both = [b0, b1].concat();
    assert!(committed(&fs, ino) == both, "the last release finalizes both blocks");
    assert_eq!(fs.test_getattr(ino).unwrap().size, both.len() as u64);
}

#[test]
fn open_after_last_release_gets_a_new_cell() {
    let dir = TempDir::new().unwrap();
    let fs = new_fs(dir.path());
    let (ino, a) = create(&fs, 1, "f");
    let first = data(5, BLOCK);
    write(&fs, a, 0, &first);
    release(&fs, ino, a);
    let c = open(&fs, ino, libc::O_RDWR);
    let patch = data(6, 4096);
    write(&fs, c, 100, &patch);
    release(&fs, ino, c);
    let mut model = first;
    model[100..100 + patch.len()].copy_from_slice(&patch);
    assert!(committed(&fs, ino) == model, "the write of the new open is kept after its release");
}
