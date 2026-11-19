//! The RAM of a full GC (mark and sweep) does not grow with the object
//! count: the exact mark is in sorted temp files. A counting global
//! allocator measures the peak heap above the baseline during
//! `gc_begin_full` .. `gc_finish_full` at two object counts (4x apart).
//!
//! This binary has its own allocator, so it holds exactly one test: no other
//! test can allocate at the same time.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use chunk_store::pack_io::FileIo;
use chunk_store::{ChunkStore, ObjectId, Options};
use std::sync::Arc;
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

/// The peak heap above the baseline of a full GC of a store of `n` small
/// objects, one in two live. Returns (peak bytes, merge passes).
fn full_gc_peak(n: u64) -> (usize, u32) {
    let dir = TempDir::new().unwrap();
    let opts = Options {
        pack_limit: 32 << 10,
        zstd_level: 0,
        index_memtable_limit: 2048,
        index_background_merge: false,
        // 2,048 IDs (66 KiB) a chunk, 8 chunks a merge: 6,000 live IDs
        // are 3 chunks, 24,000 are 12 (two merge passes). Small packs:
        // both stores have more than `GC_GROUP_PACKS` (32) packs, the most
        // that one GC group holds open (with a pending run each).
        gc_sort_chunk_ids: 2048,
        gc_merge_fan_in: 8,
        gc_tmp_dir: Some(dir.path().join("gc-tmp")),
        ..Options::default()
    };
    // Real files (not the in-RAM mock, which would count the pack bytes),
    // without the syncs, which this test does not measure.
    let io = Arc::new(FileIo::unsynced(dir.path().join("packs")));
    let s = ChunkStore::open_with(io, opts).unwrap();
    let mut live: Vec<ObjectId> = Vec::new();
    for i in 0..n {
        let id = s.put(format!("object {i} {}", "x".repeat((i % 97) as usize)).as_bytes()).unwrap();
        if i % 2 == 0 {
            live.push(id);
        }
    }
    s.sync().unwrap();
    s.merge_index_all().unwrap();
    // The live list of the test is not part of the GC: it is allocated
    // before the baseline.
    let base = CURRENT.load(Relaxed);
    PEAK.store(base, Relaxed);
    let b = s.gc_begin_full().unwrap();
    let mut m = s.exact_mark().unwrap();
    for id in &live {
        m.insert(id).unwrap();
    }
    let st = s.gc_finish_full(m, b).unwrap();
    let peak = PEAK.load(Relaxed) - base;
    assert_eq!(st.removed_objects, n - live.len() as u64, "{st:?}");
    assert_eq!(st.dead_left, Some(0));
    (peak, st.mark_merge_passes)
}

#[test]
fn full_gc_ram_does_not_grow_with_the_object_count() {
    let (small, p1) = full_gc_peak(12_000);
    let (large, p2) = full_gc_peak(48_000);
    println!("full GC peak heap: 12,000 objects {small} bytes ({p1} merge passes), 48,000 objects {large} bytes ({p2} merge passes)");
    assert!(p2 >= 2, "{p2} merge passes");
    // 4x the objects: an exact set of the live IDs in RAM would add at
    // least 18,000 x 33 bytes (594 KB). Allow half of that for the index
    // tables that do grow with the objects (run filters and fences, about
    // 3 bytes an object).
    assert!(large < small + 297_000, "peak {large} at 48,000 objects vs {small} at 12,000");
}
