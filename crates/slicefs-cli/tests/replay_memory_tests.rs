//! Replay of the write log at mount uses memory bounded by one record, not
//! by the bytes written since the last commit (#106). A counting global
//! allocator measures the peak heap while the mount's replay runs over a log
//! of many MiB with no commit after the writes (a crash).
//!
//! This binary has its own allocator, so it holds exactly one test: no other
//! test can allocate at the same time.

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

use chunk_store::ChunkStore;
use metadata::segment::load_store_from_segments;
use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use metadata::wal::{create_wal, WalConfig};
use slicefs_cli::filesystem::SliceFsFilesystem;
use slicefs_cli::handlers::{self as h, CreateResult, EmptyResult, WriteResult};
use slicefs_cli::oplog;
use slicefs_traits::metadata::MetadataStore;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

/// `System`, plus a count of the bytes in use and their peak.
struct Counting;

fn grow(n: usize) {
    let now = CURRENT.fetch_add(n, Relaxed) + n;
    PEAK.fetch_max(now, Relaxed);
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grow(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            grow(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        CURRENT.fetch_sub(layout.size(), Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            if new_size >= layout.size() {
                grow(new_size - layout.size());
            } else {
                CURRENT.fetch_sub(layout.size() - new_size, Relaxed);
            }
        }
        p
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

const MIB: usize = 1 << 20;
/// Size of one write request (and so of one log record).
const PIECE: usize = MIB;
/// Bound on the peak heap of the replay above the baseline: well below the
/// smaller log.
const PEAK_BOUND: usize = 6 << 20;
/// Most the peak may differ between the small and the large log.
const SIZE_SLACK: usize = 1 << 20;

const S_IFREG: u32 = 0o100_000;

/// Piece `i` of the file: incompressible bytes, different for each piece
/// (no dedup).
fn piece(i: usize) -> Vec<u8> {
    let mut x = (i as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..PIECE).map(|_| { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x as u8 }).collect()
}

/// The store with a small pack limit: the chunk store keeps up to one pack
/// of unsynced bytes in RAM, and the default pack (64 MiB) would hide the
/// replay's own memory.
fn open_io(store: &Path) -> Arc<Mutex<StoreIo>> {
    let opts = chunk_store::Options {
        pack_limit: MIB as u64,
        zstd_level: 0,
        index_background_merge: false,
        ..chunk_store::Options::default()
    };
    let cs = ChunkStore::open(store, opts).unwrap();
    Arc::new(Mutex::new(StoreIo::new(store, Arc::new(cs))))
}

/// Write `pieces` MiB to one file with `config`, with no commit after the
/// first one, sync the log, and "crash".
fn write_and_crash(store: &Path, config: WalConfig, pieces: usize) {
    std::fs::create_dir_all(store.join("segments")).unwrap();
    let wal = create_wal(config, store, 1).unwrap();
    let io = open_io(store);
    let mut meta = DictMetadataStore::new(io.clone());
    meta.set_wal(wal);
    let fs = SliceFsFilesystem::new(meta, io, Some(store.to_path_buf()));
    fs.meta().commit().unwrap();
    let (ino, fh) = match h::handle_create(&fs, 1, "big", S_IFREG | 0o644, 0o022, 501, 20, libc::O_RDWR) {
        CreateResult::Ok(ino, fh, _) => (ino, fh),
        _ => panic!("create"),
    };
    for i in 0..pieces {
        let p = piece(i);
        assert!(matches!(h::handle_write(&fs, fh, (i * PIECE) as u64, &p), WriteResult::Ok(_)));
        if i % 8 == 7 {
            assert!(matches!(h::handle_fsync(&fs, ino, fh), EmptyResult::Ok));
        }
    }
    assert!(matches!(h::handle_release(&fs, ino, fh), EmptyResult::Ok));
    fs.meta().flush_wal().unwrap();
    std::mem::forget(fs); // a crash: no commit, no destroy
}

/// What a mount does after a crash: load the last root, then replay the
/// log. Returns the peak heap of the replay above the baseline, after a
/// check of the content.
fn reload_and_replay(store: &Path, pieces: usize) -> usize {
    let segs = store.join("segments");
    let (root, snaps) = load_store_from_segments(&segs).unwrap();
    let io = open_io(store);
    let mut meta = DictMetadataStore::load_from_root(io.clone(), &root.unwrap()).unwrap();
    meta.set_snapshots(snaps);
    meta.set_wal(create_wal(WalConfig::PerOp, store, 99).unwrap());
    let fs = SliceFsFilesystem::new(meta, io, Some(store.to_path_buf()));

    let baseline = CURRENT.load(Relaxed);
    PEAK.store(baseline, Relaxed);
    let stats = oplog::replay_log(&fs, &segs).unwrap();
    let peak = PEAK.load(Relaxed).saturating_sub(baseline);
    assert_eq!(stats.skipped, 0);
    assert!(stats.applied >= pieces, "{} record(s) applied", stats.applied);

    // Content (not measured).
    let ino = fs.meta().lookup(1, "big").unwrap();
    assert_eq!(fs.meta().get_inode(ino).unwrap().size, (pieces * PIECE) as u64);
    for i in 0..pieces {
        let got = fs.test_read(ino, (i * PIECE) as u64, PIECE as u32).unwrap();
        assert!(got == piece(i), "content of piece {i}");
    }
    drop(fs);
    peak
}

#[test]
fn replay_memory_is_bounded_by_a_record_not_by_the_log() {
    let mut peaks = Vec::new();
    for config in [WalConfig::PerOp, WalConfig::FlushOnFsync] {
        for pieces in [8usize, 24] {
            let dir = tempfile::tempdir().unwrap();
            let t = std::time::Instant::now();
            write_and_crash(dir.path(), config.clone(), pieces);
            let peak = reload_and_replay(dir.path(), pieces);
            eprintln!(
                "replay memory: {config:?}, {pieces} MiB log: peak {} KiB above the baseline ({:.1} s)",
                peak / 1024,
                t.elapsed().as_secs_f64()
            );
            peaks.push((config.clone(), pieces, peak));
        }
    }
    for (config, pieces, peak) in &peaks {
        assert!(*peak <= PEAK_BOUND, "{config:?} {pieces} MiB: peak {} KiB > {} KiB", peak / 1024, PEAK_BOUND / 1024);
    }
    for pair in peaks.chunks(2) {
        let (config, a, b) = (&pair[0].0, pair[0].2, pair[1].2);
        assert!(a.abs_diff(b) <= SIZE_SLACK, "{config:?}: peaks {} KiB and {} KiB differ by more than {} KiB",
            a / 1024, b / 1024, SIZE_SLACK / 1024);
    }
}
