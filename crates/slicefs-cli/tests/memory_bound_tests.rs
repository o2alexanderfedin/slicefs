//! Memory of an open file is bounded by the dirty limit, not by the file
//! size (#98). A counting global allocator measures the peak heap of the
//! process while one handle goes through every inventory site of the old
//! full-file buffer: switch, random writes, limit rebases, fsync, a commit
//! with the handle open, GC start, reads, release, a write far past the end,
//! a huge extend, and a shrink of a streaming handle.
//!
//! This binary has its own allocator, so it holds exactly one test: no other
//! test can allocate at the same time.

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use metadata::wal::{create_wal, WalConfig};
use slicefs_cli::filesystem::SliceFsFilesystem;
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

const KIB: u64 = 1 << 10;
const MIB: u64 = 1 << 20;
/// A write this far past the end of the file (site 4).
const FAR_WRITE: u64 = 256 << 30;
/// An extend to this size through the handle (site 9).
const HUGE_SIZE: u64 = 512 << 30;
/// Dirty limit of the test (the production start value is 32 MiB).
const LIMIT: usize = 1 << 20;
/// Bound on the peak heap above the baseline: below the smaller file size.
const PEAK_BOUND: usize = 12 << 20;
/// Most the peak may differ between the 16 MiB and the 48 MiB file.
const SIZE_SLACK: usize = 2 << 20;

fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Byte `pos` of the random stream `seed` (random access, no buffer).
fn byte_of(seed: u64, pos: u64) -> u8 {
    (splitmix(seed.wrapping_mul(0x1000_0000_01B3) ^ (pos >> 3)) >> ((pos & 7) * 8)) as u8
}

/// `n` bytes of stream `seed` from `pos`.
fn bytes_of(seed: u64, pos: u64, n: usize) -> Vec<u8> {
    (0..n as u64).map(|i| byte_of(seed, pos + i)).collect()
}

#[derive(Clone, Copy, Debug)]
enum BaseKind {
    /// Random bytes over the whole file.
    Random,
    /// One 4 KiB random pattern, repeated (FastCDC finds few cuts in it).
    Repeating,
}

fn base_byte(kind: BaseKind, pos: u64) -> u8 {
    match kind {
        BaseKind::Random => byte_of(7, pos),
        BaseKind::Repeating => byte_of(7, pos % 4096),
    }
}

/// One write of the workload: `len` bytes of stream `seed` at `off`.
#[derive(Clone, Copy)]
struct W {
    off: u64,
    len: u64,
    seed: u64,
}

/// Expected content of `[off, off + n)` (`n` at most 1 MiB): the base, then
/// every write in order.
fn expected(kind: BaseKind, writes: &[W], off: u64, n: usize) -> Vec<u8> {
    let end = off + n as u64;
    let mut out: Vec<u8> = (off..end).map(|p| base_byte(kind, p)).collect();
    for w in writes {
        let (a, b) = (w.off.max(off), (w.off + w.len).min(end));
        for p in a..b {
            out[(p - off) as usize] = byte_of(w.seed, p - w.off);
        }
    }
    out
}

/// A disk store with a per-op write log (as the mount does by default).
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

/// One case: returns the peak heap above the baseline, in bytes.
fn run_case(kind: BaseKind, size: u64) -> usize {
    let dir = tempfile::tempdir().unwrap();
    let fs = new_fs(dir.path());

    // Setup (not measured): stream the base in 1 MiB pieces, close.
    let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
    let mut at = 0;
    while at < size {
        let piece: Vec<u8> = (at..at + MIB).map(|p| base_byte(kind, p)).collect();
        fs.test_write(fh, at, &piece).unwrap();
        at += MIB;
    }
    fs.test_release(ino, fh).unwrap();
    // Sync the store before the baseline. The chunk store keeps every byte
    // written since its last sync in RAM (to write it again if a sync fails;
    // at most one pack). Without this sync the base bytes would be in the
    // baseline, the commit below would free them, and the rest of the
    // workload would run that far below the baseline: a whole-file buffer
    // there would not be seen.
    fs.meta().commit().unwrap();
    fs.test_set_dirty_limit(LIMIT);

    // The workload: 256 random 4 KiB writes with two 1 MiB writes among them.
    let mut writes: Vec<W> = Vec::with_capacity(300);
    for i in 0..258u64 {
        let len = if i == 100 || i == 200 { MIB } else { 4 * KIB };
        let off = splitmix(1000 + i) % (size - len);
        writes.push(W { off, len, seed: 5000 + i });
    }
    let reads: Vec<u64> = (0..16u64).map(|i| splitmix(9000 + i) % (size - 64 * KIB)).collect();

    let (fh, _) = fs.test_open(ino, libc::O_RDWR).unwrap();
    let baseline = CURRENT.load(Relaxed);
    PEAK.store(baseline, Relaxed);

    // Step 1: switch, random writes, limit rebases (sites 1-3), fsync (site 7),
    // a commit with the handle open (site 8), GC start (site 11), reads
    // (site 5), release (site 6).
    let c0 = fs.test_rebase_count();
    let mut relog = 0;
    for (i, w) in writes.iter().enumerate() {
        let data = bytes_of(w.seed, 0, w.len as usize);
        assert_eq!(fs.test_write(fh, w.off, &data).unwrap() as u64, w.len);
        drop(data);
        if i == 0 {
            assert!(!fs.test_is_streaming(fh), "the first write switches to an overlay");
        }
        if i == 129 {
            let c = fs.test_rebase_count();
            fs.test_fsync(ino, fh).unwrap();
            assert_eq!(fs.test_rebase_count(), c, "an fsync never rebases");
        }
        if i == 230 {
            let c = fs.test_rebase_count();
            fs.commit_cut().unwrap();
            assert_eq!(fs.test_rebase_count(), c, "a commit never rebases");
            relog = fs.test_last_relog_bytes();
            let (_gc, _boundary) = fs.gc_start().unwrap();
            fs.content().gc_abort();
        }
    }
    for &off in &reads {
        let got = fs.test_read(ino, off, (64 * KIB) as u32).unwrap();
        assert!(got == expected(kind, &writes, off, (64 * KIB) as usize), "read at {off} with the handle open");
    }
    fs.test_release(ino, fh).unwrap();
    let c1 = fs.test_rebase_count();
    assert!(c1 >= c0 + 3, "step 1: {} rebase(s)", c1 - c0);
    assert!(relog > 0, "the commit ran while extents were dirty");

    // Step 2: a fresh overlay; a write far past the end (site 4), a huge
    // extend (site 9), each cut back. No rebase may run over the far size.
    let (fh, _) = fs.test_open(ino, libc::O_RDWR).unwrap();
    let c = fs.test_rebase_count();
    let far = bytes_of(77, 0, (4 * KIB) as usize);
    fs.test_write(fh, size + FAR_WRITE, &far).unwrap();
    drop(far);
    assert!(!fs.test_is_streaming(fh));
    assert_eq!(fs.test_rebase_count(), c);
    assert_eq!(fs.test_getattr(ino).unwrap().size, size + FAR_WRITE + 4 * KIB);
    fs.test_setattr_size(ino, Some(fh), size).unwrap();
    assert_eq!(fs.test_rebase_count(), c);
    fs.test_setattr_size(ino, Some(fh), HUGE_SIZE).unwrap();
    assert_eq!(fs.test_rebase_count(), c);
    fs.test_setattr_size(ino, Some(fh), size).unwrap();
    assert_eq!(fs.test_rebase_count(), c);
    fs.test_release(ino, fh).unwrap();
    assert_eq!(fs.test_rebase_count(), c, "a clean overlay needs no rebase at release");

    // Step 3: a shrink through a streaming handle (site 10).
    let (fh, _) = fs.test_open(ino, libc::O_RDWR).unwrap();
    assert!(fs.test_is_streaming(fh));
    fs.test_setattr_size(ino, Some(fh), size - MIB).unwrap();
    assert!(!fs.test_is_streaming(fh), "the shrink switches to an overlay");
    fs.test_release(ino, fh).unwrap();
    assert_eq!(fs.test_rebase_count(), c + 1);

    let peak = PEAK.load(Relaxed).saturating_sub(baseline);

    // Content (not measured): 16 seeded ranges, the last MiB, the size.
    let final_size = size - MIB;
    assert_eq!(fs.test_getattr(ino).unwrap().size, final_size);
    let mut checks: Vec<u64> = (0..16u64).map(|i| splitmix(12_000 + i) % (final_size - 64 * KIB)).collect();
    checks.extend(writes.iter().filter(|w| w.len == MIB).map(|w| w.off.min(final_size - 64 * KIB)));
    for off in checks {
        let got = fs.test_read(ino, off, (64 * KIB) as u32).unwrap();
        assert!(got == expected(kind, &writes, off, (64 * KIB) as usize), "content at {off}");
    }
    let got = fs.test_read(ino, final_size - MIB, (2 * MIB) as u32).unwrap();
    assert!(got == expected(kind, &writes, final_size - MIB, MIB as usize), "the last MiB");
    let meta_size = fs.meta().get_inode(ino).unwrap().size;
    assert_eq!(meta_size, final_size);
    drop(fs);
    drop(dir);
    peak
}

#[test]
fn an_open_file_uses_memory_bounded_by_the_dirty_limit_not_the_file_size() {
    let mut peaks = Vec::new();
    for kind in [BaseKind::Random, BaseKind::Repeating] {
        for size in [16 * MIB, 48 * MIB] {
            let t = std::time::Instant::now();
            let peak = run_case(kind, size);
            eprintln!(
                "memory bound: {kind:?} base, {} MiB file: peak {} KiB above the baseline ({:.1} s)",
                size / MIB,
                peak / 1024,
                t.elapsed().as_secs_f64()
            );
            peaks.push((kind, size, peak));
        }
    }
    for &(kind, size, peak) in &peaks {
        assert!(peak <= PEAK_BOUND, "{kind:?} {} MiB: peak {} KiB > {} KiB", size / MIB, peak / 1024, PEAK_BOUND / 1024);
    }
    for pair in peaks.chunks(2) {
        let (kind, a, b) = (pair[0].0, pair[0].2, pair[1].2);
        assert!(a.abs_diff(b) <= SIZE_SLACK, "{kind:?}: peaks {} KiB and {} KiB differ by more than {} KiB",
            a / 1024, b / 1024, SIZE_SLACK / 1024);
    }
}
