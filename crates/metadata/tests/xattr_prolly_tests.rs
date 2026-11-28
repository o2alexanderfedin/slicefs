//! Xattrs by name in the `xattr_data` prolly tree (plan 26-15), through the
//! product API: many names on one file, one large spilled value read by
//! range, list order, remove, a reused inode number, and races with commits.

use std::ops::ControlFlow;
use std::sync::{Arc, Mutex};

use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use slicefs_traits::metadata::{InodeMeta, MetaError, MetadataStore};
use tempfile::TempDir;

fn make_store() -> (TempDir, Arc<Mutex<StoreIo>>, DictMetadataStore) {
    let dir = TempDir::new().unwrap();
    let io = Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap()));
    let store = DictMetadataStore::new(Arc::clone(&io));
    (dir, io, store)
}

fn file(store: &DictMetadataStore) -> u64 {
    store.create_inode(&InodeMeta::new_file(0, 0, 0, 0o100644)).unwrap()
}

/// The names of `ino` from the streamed scan.
fn names(store: &DictMetadataStore, ino: u64) -> Vec<String> {
    let mut out = Vec::new();
    store
        .for_each_xattr_name(ino, &mut |n| {
            out.push(n.to_string());
            ControlFlow::Continue(())
        })
        .unwrap();
    out
}

fn commit_reload(io: &Arc<Mutex<StoreIo>>, store: &DictMetadataStore) -> DictMetadataStore {
    let root = store.commit().unwrap();
    DictMetadataStore::load_from_root(Arc::clone(io), &root).unwrap()
}

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

/// The value of name `i` of `many_xattrs_on_one_file`: 100 bytes, or 3,000
/// (spilled) for every 1,000th name.
fn value_of(i: usize) -> Vec<u8> {
    let n = if i.is_multiple_of(1_000) { 3_000 } else { 100 };
    noise(i as u64 + 1, n)
}

#[test]
#[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
fn many_xattrs_on_one_file() {
    const N: usize = 20_000;
    let (_d, io, store) = make_store();
    let before = file(&store);
    let ino = file(&store);
    let after = file(&store);
    let three: Vec<String> = (0..3).map(|i| format!("user.m{i}")).collect();
    for n in [before, after] {
        for name in &three {
            store.set_xattr(n, name, b"neighbor").unwrap();
        }
    }
    // Past TREE_FLUSH (16,384): a hot-path flush runs in the middle.
    let name = |i: usize| format!("user.n{i:05}");
    for i in 0..N {
        store.set_xattr(ino, &name(i), &value_of(i)).unwrap();
    }
    let all: Vec<String> = (0..N).map(name).collect();
    assert_eq!(names(&store, ino), all, "every name, in byte order");
    for i in (0..N).step_by(N / 200) {
        assert_eq!(store.get_xattr(ino, &name(i)).unwrap(), value_of(i), "{}", name(i));
    }
    // Remove every 4th name from 1: the spilled values (every 1,000th) stay.
    for i in (1..N).step_by(4) {
        store.remove_xattr(ino, &name(i)).unwrap();
    }
    let left: Vec<String> = (0..N).filter(|i| i % 4 != 1).map(name).collect();
    assert_eq!(left.len(), 15_000);
    let check = |s: &DictMetadataStore| {
        assert_eq!(names(s, ino), left);
        for i in (0..N).step_by(N / 200 - 3) {
            if i % 4 != 1 {
                assert_eq!(s.get_xattr(ino, &name(i)).unwrap(), value_of(i));
            }
        }
        assert_eq!(s.get_xattr(ino, &name(3_000)).unwrap(), value_of(3_000), "a spilled value");
        assert_eq!(s.xattr_len(ino, &name(3_000)).unwrap(), 3_000);
        assert!(matches!(s.get_xattr(ino, &name(1_001)), Err(MetaError::NotFound(i)) if i == ino), "removed");
        assert!(s.xattr_len(ino, &name(5_001)).is_err(), "removed");
        assert_eq!(s.xattr_len(ino, &name(1_002)).unwrap(), 100);
        assert_eq!(names(s, before), three);
        assert_eq!(names(s, after), three);
    };
    check(&store);
    let store = commit_reload(&io, &store);
    check(&store);
    store.delete_inode(ino).unwrap();
    assert!(names(&store, ino).is_empty());
    let store = commit_reload(&io, &store);
    assert!(names(&store, ino).is_empty(), "no name after the delete");
    assert_eq!(names(&store, before), three);
    assert_eq!(names(&store, after), three);
}

#[test]
#[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
fn one_large_spilled_value() {
    let (_d, io, store) = make_store();
    let ino = file(&store);
    let big = noise(7, 3 << 20);
    let len = big.len() as u64;
    store.set_xattr(ino, "user.big", &big).unwrap();
    let check = |s: &DictMetadataStore| {
        assert_eq!(s.xattr_len(ino, "user.big").unwrap(), len);
        assert!(s.get_xattr(ino, "user.big").unwrap() == big);
        assert_eq!(s.read_xattr(ino, "user.big", 0, 4 << 10).unwrap(), &big[..4 << 10]);
        let at = (1 << 20) + 7;
        assert_eq!(s.read_xattr(ino, "user.big", at as u64, 64 << 10).unwrap(), &big[at..at + (64 << 10)]);
        assert_eq!(s.read_xattr(ino, "user.big", len - 10, 100).unwrap(), &big[big.len() - 10..], "10 bytes at the end");
        assert!(s.read_xattr(ino, "user.big", len + 5, 100).unwrap().is_empty(), "past the end");
    };
    check(&store);
    let store = commit_reload(&io, &store);
    check(&store);
    store.set_xattr(ino, "user.big", b"0123456789").unwrap();
    assert_eq!(store.xattr_len(ino, "user.big").unwrap(), 10);
    assert_eq!(store.get_xattr(ino, "user.big").unwrap(), b"0123456789");
    assert_eq!(store.read_xattr(ino, "user.big", 3, 100).unwrap(), b"3456789");
}

#[test]
#[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
fn list_order_is_name_byte_order() {
    let (_d, io, store) = make_store();
    let ino = file(&store);
    for n in ["user.b", "user.a", "user.Z", "user.é", "trusted.x"] {
        store.set_xattr(ino, n, n.as_bytes()).unwrap();
    }
    let want = ["trusted.x", "user.Z", "user.a", "user.b", "user.é"];
    assert_eq!(store.list_xattrs(ino).unwrap(), want);
    assert_eq!(names(&store, ino), want);
    let store = commit_reload(&io, &store);
    assert_eq!(store.list_xattrs(ino).unwrap(), want);
    // `Break` stops the scan.
    let mut first = Vec::new();
    store
        .for_each_xattr_name(ino, &mut |n| {
            first.push(n.to_string());
            ControlFlow::Break(())
        })
        .unwrap();
    assert_eq!(first, ["trusted.x"]);
}

#[test]
#[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
fn remove_reports_a_missing_name() {
    let (_d, io, store) = make_store();
    let ino = file(&store);
    let missing = |s: &DictMetadataStore, n: &str| matches!(s.remove_xattr(ino, n), Err(MetaError::NotFound(i)) if i == ino);
    assert!(missing(&store, "user.never"));
    store.set_xattr(ino, "user.gone", b"v").unwrap();
    store.set_xattr(ino, "user.kept", b"v").unwrap();
    store.remove_xattr(ino, "user.gone").unwrap();
    assert!(missing(&store, "user.gone"), "removed, not committed");
    assert!(missing(&store, "user.never"));
    let store = commit_reload(&io, &store);
    assert!(missing(&store, "user.gone"));
    assert!(missing(&store, "user.never"));
    store.remove_xattr(ino, "user.kept").unwrap();
    assert!(missing(&store, "user.kept"));
}

#[test]
#[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
fn a_reused_inode_number_has_no_xattrs() {
    let (_d, io, store) = make_store();
    let x = file(&store);
    for i in 0..12 {
        store.set_xattr(x, &format!("user.c{i:02}"), &[i as u8; 30]).unwrap();
    }
    store.set_xattr(x, "user.c_big", &noise(3, 10_000)).unwrap();
    let store = commit_reload(&io, &store);
    for i in 0..5 {
        store.set_xattr(x, &format!("user.p{i}"), b"pending").unwrap();
    }
    assert_eq!(names(&store, x).len(), 18);
    store.delete_inode(x).unwrap();
    store.create_inode_at(x, &InodeMeta::new_file(0, 0, 0, 0o100644)).unwrap();
    assert!(names(&store, x).is_empty());
    assert!(store.get_xattr(x, "user.c00").is_err());
    let store = commit_reload(&io, &store);
    assert!(names(&store, x).is_empty(), "after a commit and a reload");
    assert!(store.get_inode(x).is_ok());
}

#[test]
#[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
fn xattrs_from_threads_lose_nothing() {
    const PER: usize = 500;
    let (_d, io, store) = make_store();
    // Flushes run often (the integration build flushes at 16,384).
    store.test_set_tree_flush(16);
    let ino = file(&store);
    let value = |t: usize, i: usize| if i.is_multiple_of(100) { noise((t * PER + i) as u64, 3_000) } else { format!("t{t}-{i}").into_bytes() };
    let done = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|s| {
        for t in 0..4 {
            let (store, done) = (&store, &done);
            s.spawn(move || {
                for i in 0..PER {
                    store.set_xattr(ino, &format!("user.t{t}.{i:03}"), &value(t, i)).unwrap();
                }
                done.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            });
        }
        s.spawn(|| {
            while done.load(std::sync::atomic::Ordering::SeqCst) < 4 {
                store.commit().unwrap();
            }
        });
        s.spawn(|| {
            while done.load(std::sync::atomic::Ordering::SeqCst) < 4 {
                let n = names(&store, ino);
                assert!(n.windows(2).all(|w| w[0] < w[1]), "sorted, no key twice");
            }
        });
    });
    let check = |s: &DictMetadataStore| {
        let mut want: Vec<String> = (0..4).flat_map(|t| (0..PER).map(move |i| format!("user.t{t}.{i:03}"))).collect();
        want.sort();
        assert_eq!(names(s, ino), want);
        for t in 0..4 {
            for i in 0..PER {
                assert_eq!(s.get_xattr(ino, &format!("user.t{t}.{i:03}")).unwrap(), value(t, i));
            }
        }
    };
    check(&store);
    let store = commit_reload(&io, &store);
    check(&store);
}
