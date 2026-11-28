//! Integration tests for DictMetadataStore refcount infrastructure.
//!
//! Tests increment/decrement/get_refcount methods and persistence across commit/load_from_root.

use std::sync::{Arc, Mutex};
use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use slicefs_traits::key::ObjectKey;
use slicefs_traits::metadata::MetadataStore;
use tempfile::TempDir;

/// A simple non-zero ObjectKey for testing.
fn make_digest(seed: u32) -> ObjectKey {
    ObjectKey::example(seed)
}

fn make_store() -> (TempDir, DictMetadataStore) {
    let dir = TempDir::new().unwrap();
    let io = Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap()));
    let store = DictMetadataStore::new(io);
    (dir, store)
}

#[test]
fn test_increment_refcount_from_zero() {
    let (_dir, store) = make_store();
    let digest = make_digest(1);

    assert_eq!(store.get_refcount(&digest), 0, "initially 0");
    store.increment_refcount(&digest);
    assert_eq!(store.get_refcount(&digest), 1, "after first increment");
    store.increment_refcount(&digest);
    assert_eq!(store.get_refcount(&digest), 2, "after second increment");
}

#[test]
fn test_decrement_refcount() {
    let (_dir, store) = make_store();
    let digest = make_digest(10);

    store.increment_refcount(&digest);
    store.increment_refcount(&digest);
    assert_eq!(store.get_refcount(&digest), 2);

    store.decrement_refcount(&digest);
    assert_eq!(store.get_refcount(&digest), 1, "after first decrement");

    store.decrement_refcount(&digest);
    assert_eq!(store.get_refcount(&digest), 0, "after second decrement — entry removed");
}

#[test]
fn test_get_refcount_unknown_returns_zero() {
    let (_dir, store) = make_store();
    let digest = make_digest(99);
    assert_eq!(store.get_refcount(&digest), 0);
}

#[test]
fn test_two_inodes_sharing_same_content_digest_have_refcount_two() {
    let (_dir, store) = make_store();
    let shared_content = make_digest(42);

    // Simulate two inodes referencing the same content block
    store.increment_refcount(&shared_content);
    store.increment_refcount(&shared_content);

    assert_eq!(store.get_refcount(&shared_content), 2);
}

#[test]
fn test_refcounts_survive_commit_load_round_trip() {
    let (_dir, store) = make_store();
    let digest_a = make_digest(100);
    let digest_b = make_digest(200);

    store.increment_refcount(&digest_a);
    store.increment_refcount(&digest_a);
    store.increment_refcount(&digest_b);

    // Commit then reload from the same file storage
    let root = store.commit().unwrap();
    let io = Arc::clone(store.io());
    let store2 = DictMetadataStore::load_from_root(io, &root).unwrap();

    assert_eq!(store2.get_refcount(&digest_a), 2, "digest_a refcount must survive reload");
    assert_eq!(store2.get_refcount(&digest_b), 1, "digest_b refcount must survive reload");
    assert_eq!(store2.get_refcount(&make_digest(999)), 0, "unknown digest returns 0");
}

/// PMAP-03: 8 threads increment and decrement 64 shared keys (each thread
/// decrements only a key it incremented before, so no decrement meets a 0)
/// while another thread commits in a loop and a flush runs at each change
/// (when the apply lock is free). The counts are exact, before and after a
/// reload.
#[test]
fn refcounts_from_threads_lose_nothing() {
    const THREADS: u64 = 8;
    const KEYS: u64 = 64;
    let (_dir, store) = make_store();
    store.test_set_tree_flush(1);
    let keys: Vec<ObjectKey> = (0..KEYS as u32).map(|i| make_digest(1000 + i)).collect();
    // Thread t, step j: +1 on a(t, j), +1 on b(t, j), -1 on a(t, j).
    let a = |t: u64, j: u64| ((t * 7 + j) % KEYS) as usize;
    let b = |t: u64, j: u64| ((t * 13 + j * 3) % KEYS) as usize;
    let mut want = vec![0u64; KEYS as usize];
    for t in 0..THREADS {
        for j in 0..1000 {
            want[b(t, j)] += 1;
        }
    }
    let done = std::sync::atomic::AtomicBool::new(false);
    let commits = std::sync::atomic::AtomicU64::new(0);
    std::thread::scope(|s| {
        let committer = s.spawn(|| {
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                store.commit().unwrap();
                commits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        });
        let workers: Vec<_> = (0..THREADS)
            .map(|t| {
                let (store, keys) = (&store, &keys);
                s.spawn(move || {
                    for j in 0..1000 {
                        store.increment_refcount(&keys[a(t, j)]);
                        store.increment_refcount(&keys[b(t, j)]);
                        store.decrement_refcount(&keys[a(t, j)]);
                    }
                })
            })
            .collect();
        for w in workers {
            w.join().unwrap();
        }
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        committer.join().unwrap();
    });
    assert!(commits.load(std::sync::atomic::Ordering::Relaxed) > 0, "commits ran during the changes");
    for (i, k) in keys.iter().enumerate() {
        assert_eq!(store.get_refcount(k), want[i], "key {i}");
    }
    let root = store.commit().unwrap();
    let reloaded = DictMetadataStore::load_from_root(Arc::clone(store.io()), &root).unwrap();
    for (i, k) in keys.iter().enumerate() {
        assert_eq!(reloaded.get_refcount(k), want[i], "key {i} after a reload");
    }
    assert_eq!(reloaded.saturated_refcount_count().unwrap(), 0);
}
