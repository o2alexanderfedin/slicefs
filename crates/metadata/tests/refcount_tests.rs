//! Integration tests for DictMetadataStore refcount infrastructure.
//!
//! Tests increment/decrement/get_refcount methods and persistence across commit/load_from_root.

use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use slicefs_traits::digest::Digest224;
use slicefs_traits::metadata::MetadataStore;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

/// A simple non-zero Digest224 for testing.
fn make_digest(seed: u32) -> Digest224 {
    [
        seed,
        seed + 1,
        seed + 2,
        seed + 3,
        seed + 4,
        seed + 5,
        seed + 6,
    ]
}

fn make_store() -> (TempDir, DictMetadataStore) {
    let dir = TempDir::new().unwrap();
    let io = Arc::new(Mutex::new(StoreIo::new(dir.path())));
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
    assert_eq!(
        store.get_refcount(&digest),
        0,
        "after second decrement — entry removed"
    );
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

    assert_eq!(
        store2.get_refcount(&digest_a),
        2,
        "digest_a refcount must survive reload"
    );
    assert_eq!(
        store2.get_refcount(&digest_b),
        1,
        "digest_b refcount must survive reload"
    );
    assert_eq!(
        store2.get_refcount(&make_digest(999)),
        0,
        "unknown digest returns 0"
    );
}
