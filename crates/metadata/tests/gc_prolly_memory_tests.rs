//! The heap of a full GC mark does not grow with the map (ROADMAP
//! criterion 3, T-26-33): the mark walks each tree with one path of nodes
//! and sorts the other keys on disk. A counting global allocator measures
//! the peak heap above the start of `live_objects_exact` on bulk stores of
//! 50,000 and 200,000 files (4 x the entries of each map).
//!
//! This binary has its own allocator, so it holds exactly one test: no other
//! test can allocate at the same time.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

use metadata::store_io::StoreIo;
use metadata::testkit::{bulk_store, BulkSpec};
use tempfile::TempDir;

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

/// The peak heap above the start of a full GC mark of a bulk store of
/// `files` files (the store is made before the start). The sweep after it
/// (not measured) removes nothing: the mark found every object.
fn mark_peak(files: u64) -> usize {
    let dir = TempDir::new().unwrap();
    // Sort chunks of 4,096 keys (135 KB): the keys of both sizes (2 per
    // file) spill to temp files, so the fixed sort buffer does not hide a
    // part that grows with the map.
    let opts = chunk_store::Options { gc_sort_chunk_ids: 4_096, ..chunk_store::Options::default() };
    let cs = Arc::new(chunk_store::ChunkStore::open(dir.path(), opts).unwrap());
    let io = Arc::new(Mutex::new(StoreIo::new(dir.path(), cs)));
    let spec = BulkSpec { files, dirs: files / 100, xattr_every: 10, refcounts_per_file: 1 };
    let root = bulk_store(&io, &spec).unwrap();
    let cs = Arc::clone(io.lock().unwrap().store());
    let boundary = cs.gc_begin_full().unwrap();
    let stop = AtomicBool::new(false);
    let base = CURRENT.load(Relaxed);
    PEAK.store(base, Relaxed);
    let live = metadata::gc::live_objects_exact(&cs, &[root], &[], &stop).unwrap().unwrap();
    let peak = PEAK.load(Relaxed) - base;
    let st = cs.gc_finish_full(live, boundary).unwrap();
    assert_eq!(st.removed_objects, 0, "{files} files: {st:?}");
    peak
}

#[test]
#[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
fn gc_mark_heap_does_not_grow_with_the_map() {
    const MIB: usize = 1 << 20;
    let small = mark_peak(50_000);
    let large = mark_peak(200_000);
    println!("GC mark peak heap: 50,000 files {small} bytes, 200,000 files {large} bytes");
    assert!(large <= small + small / 4 + 2 * MIB, "peak {large} at 200,000 files vs {small} at 50,000");
    // 2 x the 50,000-file peak measured on macOS (debug build): 2,086,967
    // bytes (200,000 files: 2,641,095 bytes).
    const CAP: usize = 2 * 2_086_967;
    assert!(large <= CAP, "peak {large} above the cap {CAP}");
}
