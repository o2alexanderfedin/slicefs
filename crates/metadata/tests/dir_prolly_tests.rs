//! Directory entries by name in the `dir_data` prolly tree (plan 26-09),
//! through the product API: a large directory (past the flush limit),
//! hard links, and a rename across directories.

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

fn mkdir(store: &DictMetadataStore, parent: u64, name: &str) -> u64 {
    store.create_directory(parent, name, &InodeMeta::new_directory(0, 0, 0, 0o40755)).unwrap()
}

fn commit_reload(io: &Arc<Mutex<StoreIo>>, store: &DictMetadataStore) -> DictMetadataStore {
    let root = store.commit().unwrap();
    DictMetadataStore::load_from_root(Arc::clone(io), &root).unwrap()
}

/// The names of `dir` in listing order.
fn names(store: &DictMetadataStore, dir: u64) -> Vec<String> {
    store.list_directory(dir).unwrap().into_iter().map(|e| e.name).collect()
}

/// 20,000 names in one directory (past `TREE_FLUSH`, 16,384 in this build,
/// so the overlay is flushed while they are added): the listing is in
/// name byte order, lookups hit, and the directory empties and goes, before
/// and after a reload.
#[test]
#[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
fn a_large_directory() {
    const N: u64 = 20_000;
    let (_dir, io, store) = make_store();
    let d = mkdir(&store, 1, "big");
    // Names that do not sort as numbers: "f10" < "f9" in byte order.
    let mut files = Vec::with_capacity(N as usize);
    for i in 0..N {
        let ino = file(&store);
        store.link(d, &format!("f{i}"), ino).unwrap();
        files.push(ino);
    }
    let mut want: Vec<String> = (0..N).map(|i| format!("f{i}")).collect();
    want.push(".".into());
    want.push("..".into());
    want.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    let check = |s: &DictMetadataStore| {
        let entries = s.list_directory(d).unwrap();
        assert_eq!(entries.len() as u64, N + 2);
        let got: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(got, want, "name byte order");
        // 100 lookups of seeded random names (xorshift).
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..100 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let i = x % N;
            assert_eq!(s.lookup(d, &format!("f{i}")).unwrap(), files[i as usize], "f{i}");
        }
        assert_eq!(s.lookup(d, "..").unwrap(), 1);
        assert!(!s.dir_is_empty(d).unwrap());
    };
    check(&store);
    let store = commit_reload(&io, &store);
    check(&store);
    for i in 0..N {
        store.unlink(d, &format!("f{i}")).unwrap();
        store.delete_inode(files[i as usize]).unwrap();
    }
    assert!(store.dir_is_empty(d).unwrap());
    assert_eq!(names(&store, d), [".", ".."]);
    // The rmdir path: the name in the parent, then the inode.
    store.unlink(1, "big").unwrap();
    store.delete_inode(d).unwrap();
    assert!(matches!(store.list_directory(d), Err(MetaError::NotADirectory(_))));
    let store = commit_reload(&io, &store);
    assert!(matches!(store.lookup(1, "big"), Err(MetaError::NotFound(_))));
    assert!(matches!(store.dir_is_empty(d), Err(MetaError::NotADirectory(_))));
    assert_eq!(names(&store, 1), [".", ".."]);
}

/// One inode under two names in two directories: each name is its own
/// entry; an unlink of one keeps the other, and both survive a reload.
#[test]
#[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
fn hard_links_are_separate_entries() {
    let (_dir, io, store) = make_store();
    let (a, b) = (mkdir(&store, 1, "a"), mkdir(&store, 1, "b"));
    let f = file(&store);
    store.link(a, "one", f).unwrap();
    store.link(b, "two", f).unwrap();
    store.link(b, "three", f).unwrap();
    let store = commit_reload(&io, &store);
    assert_eq!(store.lookup(a, "one").unwrap(), f);
    assert_eq!(store.lookup(b, "two").unwrap(), f);
    assert_eq!(names(&store, b), [".", "..", "three", "two"]);
    store.unlink(b, "two").unwrap();
    assert_eq!(store.lookup(b, "three").unwrap(), f, "the other name stays");
    assert!(matches!(store.lookup(b, "two"), Err(MetaError::NotFound(_))));
    let store = commit_reload(&io, &store);
    assert_eq!(store.lookup(a, "one").unwrap(), f);
    assert_eq!(store.lookup(b, "three").unwrap(), f);
    assert!(matches!(store.lookup(b, "two"), Err(MetaError::NotFound(_))));
}

/// A directory moved to another parent (link new, unlink old,
/// `set_parent`): `..` is the new parent after a commit and a reload.
#[test]
#[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
fn rename_across_directories_moves_dotdot() {
    let (_dir, io, store) = make_store();
    let (src, dst) = (mkdir(&store, 1, "src"), mkdir(&store, 1, "dst"));
    let moved = mkdir(&store, src, "moved");
    let inner = file(&store);
    store.link(moved, "inner", inner).unwrap();
    let store = commit_reload(&io, &store);
    assert_eq!(store.lookup(moved, "..").unwrap(), src);
    store.link(dst, "renamed", moved).unwrap();
    store.unlink(src, "moved").unwrap();
    store.set_parent(moved, dst).unwrap();
    assert_eq!(store.lookup(moved, "..").unwrap(), dst, "before a commit");
    let store = commit_reload(&io, &store);
    assert_eq!(store.lookup(moved, "..").unwrap(), dst);
    assert_eq!(store.lookup(dst, "renamed").unwrap(), moved);
    assert!(matches!(store.lookup(src, "moved"), Err(MetaError::NotFound(_))));
    assert!(store.dir_is_empty(src).unwrap());
    assert_eq!(names(&store, moved), [".", "..", "inner"]);
    assert_eq!(store.lookup(moved, "inner").unwrap(), inner);
}
