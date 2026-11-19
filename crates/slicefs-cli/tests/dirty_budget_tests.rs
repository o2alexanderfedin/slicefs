//! One budget of dirty bytes for all open files (#105). Each file alone stays
//! under its dirty limit, but many open files with random writes would hold
//! many times that limit. With the budget, the file with the most dirty bytes
//! rebases a bounded step when the sum passes the budget, so the sum and the
//! heap stay near one budget, and a write to one file never pays for the
//! whole rebase of another.
//!
//! A counting global allocator measures the peak heap (the pattern of
//! `memory_bound_tests.rs`). This binary has its own allocator, so it holds
//! exactly one test: no other test can allocate at the same time.

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use metadata::wal::{create_wal, WalConfig};
use slicefs_cli::filesystem::{SliceFsFilesystem, REBASE_STEP};
use slicefs_traits::metadata::MetadataStore;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

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

const KIB: u64 = 1 << 10;
const MIB: u64 = 1 << 20;
const FILES: u64 = 6;
const FILE_SIZE: u64 = 8 * MIB;
/// Dirty limit of each file: alone, the six files could hold 24 MiB.
const LIMIT: usize = 4 << 20;
/// The budget of all files together.
const BUDGET: usize = 4 << 20;
/// Writes per file: about 4.7 MiB of 4 KiB writes, above the limit.
const WRITES: u64 = 1200;
/// Peak heap above the baseline: the budget, the chunk store's unsynced
/// bytes (up to 8 MiB, see `memory_bound_tests.rs`) and the step work
/// space. Without the budget the six overlays alone hold about 24 MiB.
const PEAK_BOUND: usize = BUDGET + (12 << 20);
/// Most work of one write (see `rebase_step_tests.rs`).
const WORK_BOUND: u64 = 6 * REBASE_STEP as u64;

fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

fn byte_of(seed: u64, pos: u64) -> u8 {
    (splitmix(seed.wrapping_mul(0x1000_0000_01B3) ^ (pos >> 3)) >> ((pos & 7) * 8)) as u8
}

fn bytes_of(seed: u64, pos: u64, n: usize) -> Vec<u8> {
    (0..n as u64).map(|i| byte_of(seed, pos + i)).collect()
}

fn new_fs(store: &Path) -> SliceFsFilesystem {
    std::fs::create_dir_all(store.join("segments")).unwrap();
    let wal = create_wal(WalConfig::FlushOnFsync, store, 1).unwrap();
    let io = Arc::new(Mutex::new(StoreIo::open(store).unwrap()));
    let mut meta = DictMetadataStore::new(io.clone());
    meta.set_wal(wal);
    let fs = SliceFsFilesystem::new(meta, io, Some(store.to_path_buf()));
    fs.meta().commit().unwrap();
    fs
}

/// The write `i` of file `f`: (offset, seed).
fn write_of(f: u64, i: u64) -> (u64, u64) {
    (splitmix(f * 100_000 + i) % (FILE_SIZE - 4 * KIB), 1_000_000 + f * 100_000 + i)
}

#[test]
fn many_open_files_stay_within_one_dirty_budget() {
    let dir = tempfile::tempdir().unwrap();
    let fs = new_fs(dir.path());
    // Setup (not measured): the files, streamed and closed.
    let mut inos = Vec::new();
    for f in 0..FILES {
        let (ino, fh) = fs.test_create(1, &format!("f{f}"), 0o644, 0, 0, 0).unwrap();
        let mut at = 0;
        while at < FILE_SIZE {
            fs.test_write(fh, at, &bytes_of(f, at, MIB as usize)).unwrap();
            at += MIB;
        }
        fs.test_release(ino, fh).unwrap();
        inos.push(ino);
    }
    fs.meta().commit().unwrap();
    fs.test_set_dirty_limit(LIMIT);
    fs.test_set_dirty_budget(BUDGET);
    let fhs: Vec<u64> = inos.iter().map(|&ino| fs.test_open(ino, libc::O_RDWR).unwrap().0).collect();

    let baseline = CURRENT.load(Relaxed);
    PEAK.store(baseline, Relaxed);
    let mut max_total = 0;
    let mut max_work = 0;
    for i in 0..WRITES {
        for f in 0..FILES {
            let (off, seed) = write_of(f, i);
            let data = bytes_of(seed, 0, 4 * KIB as usize);
            fs.test_write(fhs[f as usize], off, &data).unwrap();
            drop(data);
            max_total = max_total.max(fs.test_dirty_total());
            max_work = max_work.max(fs.test_last_write_rebase_bytes());
        }
    }
    let peak = PEAK.load(Relaxed).saturating_sub(baseline);
    eprintln!(
        "dirty budget: {FILES} files, max dirty total {} KiB (budget {} KiB), max work of one write {} KiB, peak {} KiB",
        max_total / 1024, BUDGET / 1024, max_work / 1024, peak / 1024
    );
    assert!(max_total <= BUDGET, "dirty total {max_total} > budget {BUDGET}");
    assert!(max_work <= WORK_BOUND, "one write cut {max_work} bytes again");
    assert!(peak <= PEAK_BOUND, "peak {} KiB > {} KiB", peak / 1024, PEAK_BOUND / 1024);

    // Content (not measured): every file, after release.
    for f in 0..FILES {
        fs.test_release(inos[f as usize], fhs[f as usize]).unwrap();
        let mut model = bytes_of(f, 0, FILE_SIZE as usize);
        for i in 0..WRITES {
            let (off, seed) = write_of(f, i);
            model[off as usize..(off + 4 * KIB) as usize].copy_from_slice(&bytes_of(seed, 0, 4 * KIB as usize));
        }
        let got = fs.test_read(inos[f as usize], 0, FILE_SIZE as u32).unwrap();
        assert!(got == model, "file {f}: content");
    }
}
