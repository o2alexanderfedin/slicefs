//! The heap of the xattr calls does not grow with the xattr set of a
//! file (plan 26-15, T-26-42): get, the size query, offset reads, set,
//! remove and a streamed name scan, at 2,000 names with a 2 MiB value and
//! at 20,000 names with a 16 MiB value, on a store loaded from its root with
//! a 1 MiB node cache. A counting global allocator measures the peak heap
//! above the start of the measured calls.
//!
//! This binary has its own allocator, so it holds exactly one test: no other
//! test can allocate at the same time.

use std::alloc::{GlobalAlloc, Layout, System};
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use slicefs_traits::metadata::{InodeMeta, MetadataStore};
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

/// Seeded bytes (xorshift).
fn noise(seed: u64, n: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

fn name(i: usize) -> String {
    format!("user.n{i:05}")
}

/// Build a store with `names` names of 200 bytes and one `big`-byte value
/// on one file, commit it, load it again with a 1 MiB node cache, then
/// measure the xattr calls. Gives (peak of the calls, peak of the set and
/// remove alone), in bytes above the start.
fn xattr_peak(names: usize, big: usize) -> (usize, usize) {
    let dir = TempDir::new().unwrap();
    let (root, ino) = {
        let io = Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap()));
        let store = DictMetadataStore::new(io);
        let ino = store.create_inode(&InodeMeta::new_file(0, 0, 0, 0o100644)).unwrap();
        for i in 0..names {
            store.set_xattr(ino, &name(i), &noise(i as u64 + 1, 200)).unwrap();
        }
        store.set_xattr(ino, "user.big", &noise(99, big)).unwrap();
        (store.commit().unwrap(), ino)
    };
    let mut io = StoreIo::open(dir.path()).unwrap();
    io.set_node_cache_bytes(1 << 20);
    let store = DictMetadataStore::load_from_root(Arc::new(Mutex::new(io)), &root).unwrap();
    let tail = (big - 4096) as u64;

    let base = CURRENT.load(Relaxed);
    PEAK.store(base, Relaxed);
    for k in 0..100 {
        let i = k * names / 100 + 7;
        assert_eq!(store.get_xattr(ino, &name(i)).unwrap().len(), 200);
    }
    assert_eq!(store.xattr_len(ino, "user.big").unwrap(), big as u64);
    assert_eq!(store.read_xattr(ino, "user.big", (big / 2) as u64, 4096).unwrap().len(), 4096);
    assert_eq!(store.read_xattr(ino, "user.big", tail, 4096).unwrap().len(), 4096);
    let set_base = CURRENT.load(Relaxed);
    let set_peak_before = PEAK.swap(set_base, Relaxed);
    store.set_xattr(ino, "user.new", b"one more").unwrap();
    store.remove_xattr(ino, &name(3)).unwrap();
    let set_peak = PEAK.load(Relaxed) - set_base;
    PEAK.fetch_max(set_peak_before, Relaxed);
    let mut count = 0usize;
    store
        .for_each_xattr_name(ino, &mut |_| {
            count += 1;
            ControlFlow::Continue(())
        })
        .unwrap();
    let peak = PEAK.load(Relaxed) - base;
    assert_eq!(count, names + 1, "the names, the big one and the new one, less the removed one");
    (peak, set_peak)
}

#[test]
fn xattr_calls_do_not_grow_with_the_set() {
    let (small, set_small) = xattr_peak(2_000, 2 << 20);
    let (large, set_large) = xattr_peak(20_000, 16 << 20);
    println!("xattr peak small={small} large={large} set_small={set_small} set_large={set_large}");
    assert!(large <= small + small / 4 + (1 << 20), "peak {large} at 20,000 names vs {small} at 2,000");
    assert!(large <= 4 << 20, "peak {large} above 4 MiB");
}
