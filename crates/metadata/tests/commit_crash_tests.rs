//! A commit cut at each write and sync step, then a power loss
//! (`MemIo::crash()` keeps only what is durable): the last logged root loads
//! to a consistent manifest tree that holds exactly the state of that root
//! (PMAP-08, plan 26-05). No sync runs between the node puts of one commit,
//! so a crash at `AfterApply` or `BeforeSync` drops all of them; a small
//! `pack_limit` makes packs seal in the middle of the commit.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use chunk_store::mem::MemIo;
use chunk_store::{ChunkStore, ObjectId, Options};
use metadata::store::{CommitFault, DictMetadataStore};
use metadata::store_io::{get_value, StoreIo};
use slicefs_traits::key::{ObjectKey, KEY_BYTES};
use slicefs_traits::metadata::{InodeMeta, MetadataStore};

/// Offset of the manifest tree root in the root record (pinned by the
/// `root_record_layout` unit test): root dir ino, next ino, inode and dir maps.
const MANIFEST_DATA: usize = 8 + 8 + 2 * KEY_BYTES;

const FAULTS: [CommitFault; 5] = [
    CommitFault::AfterApply,
    CommitFault::BeforeSync,
    CommitFault::AfterInstall,
    CommitFault::BeforePreRootSync,
    CommitFault::BeforeRoot,
];

fn open(mem: &MemIo, opts: &Options) -> (Arc<Mutex<StoreIo>>, Arc<ChunkStore>) {
    let cs = Arc::new(ChunkStore::open_with(Arc::new(mem.clone()), opts.clone()).unwrap());
    (Arc::new(Mutex::new(StoreIo::new("/unused", Arc::clone(&cs)))), cs)
}

/// The state that a root must hold: the manifest of each file (a file is
/// linked in the root directory as `f<ino>`).
type State = BTreeMap<u64, Vec<ObjectKey>>;

/// Validate the manifest tree of `root`, as loaded by `meta`, against `want`:
/// every node reads and passes the prolly checks (`prolly::visit` checks each
/// node against its parent entry and the key order), the tree holds exactly
/// the inos of `want`, each ino is an inode and has the manifest of `want`,
/// and the root directory lists exactly the files of `want`.
fn check_tree(meta: &DictMetadataStore, cs: &ChunkStore, root: &ObjectKey, want: &State, what: &str) {
    let rec = get_value(cs, root).unwrap_or_else(|e| panic!("{what}: root record: {e}"));
    let tree = ObjectId(rec[MANIFEST_DATA..MANIFEST_DATA + KEY_BYTES].try_into().unwrap());
    let mut seen: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    let all = prolly::visit(&prolly::Ctx::uncached(cs), &tree, &mut |v| {
        for (k, val) in v.entries {
            let ino = u64::from_be_bytes(k.as_slice().try_into().expect("an 8-byte key"));
            assert!(seen.insert(ino, val.clone()).is_none(), "{what}: ino {ino} twice in the tree");
        }
        true
    })
    .unwrap_or_else(|e| panic!("{what}: a node of the tree fails its checks: {e}"));
    assert!(all, "{what}: the walk ends early");
    assert_eq!(prolly::count(&prolly::Ctx::uncached(cs), &tree).unwrap(), want.len() as u64, "{what}: entry count");
    assert_eq!(seen.keys().collect::<Vec<_>>(), want.keys().collect::<Vec<_>>(), "{what}: the inos of the tree");
    for (ino, blocks) in want {
        meta.get_inode(*ino).unwrap_or_else(|e| panic!("{what}: manifest ino {ino} is not an inode: {e}"));
        assert_eq!(meta.get_manifest(*ino).unwrap(), *blocks, "{what}: manifest of ino {ino}");
        assert_eq!(meta.manifest_digest(*ino).map(|d| d.0.to_vec()).as_ref(), seen.get(ino), "{what}: tree value of ino {ino}");
    }
    let mut names: Vec<String> =
        meta.list_directory(1).unwrap().into_iter().map(|e| e.name).filter(|n| n != "." && n != "..").collect();
    names.sort();
    let mut expect: Vec<String> = want.keys().map(|i| format!("f{i}")).collect();
    expect.sort();
    assert_eq!(names, expect, "{what}: the root directory");
}

/// The model of a store, and the changes that keep both in step.
struct World {
    store: DictMetadataStore,
    state: State,
    /// Next manifest value (each is distinct).
    next: u32,
}

impl World {
    fn new(store: DictMetadataStore) -> Self {
        World { store, state: State::new(), next: 0 }
    }

    /// Continue from a reloaded store whose state is `state`.
    fn resume(store: DictMetadataStore, state: State, flush: Option<usize>) -> Self {
        if let Some(n) = flush {
            store.test_set_tree_flush(n);
        }
        World { store, state, next: 1_000_000 }
    }

    fn value(&mut self) -> Vec<ObjectKey> {
        self.next += 1;
        vec![ObjectKey::example(self.next)]
    }

    fn add(&mut self) -> u64 {
        let ino = self.store.create_inode(&InodeMeta::new_file(0, 0, 0, 0o100644)).unwrap();
        self.store.link(1, &format!("f{ino}"), ino).unwrap();
        let v = self.value();
        self.store.set_manifest(ino, &v).unwrap();
        self.state.insert(ino, v);
        ino
    }

    fn change(&mut self, ino: u64) {
        let v = self.value();
        self.store.set_manifest(ino, &v).unwrap();
        self.state.insert(ino, v);
    }

    fn remove(&mut self, ino: u64) {
        self.store.unlink(1, &format!("f{ino}")).unwrap();
        self.store.delete_inode(ino).unwrap();
        self.state.remove(&ino);
    }

    /// `changed` manifest changes, `deleted` deletes and `created` creates,
    /// on files spread over the state.
    fn churn(&mut self, changed: usize, deleted: usize, created: usize) {
        let inos: Vec<u64> = self.state.keys().copied().collect();
        for ino in inos.iter().step_by((inos.len() / changed.max(1)).max(1)).take(changed) {
            self.change(*ino);
        }
        let victims: Vec<u64> = inos.iter().rev().step_by(3).take(deleted).copied().collect();
        for ino in victims {
            self.remove(ino);
        }
        for _ in 0..created {
            self.add();
        }
    }
}

/// The scenario of the matrix at one fault: a committed root A; changes; the
/// commit fails at `fault`; a power loss; A reloads to its state; then, on
/// that reloaded store, a new change and a good commit B; a power loss; B
/// reloads to its state. Returns how many packs the failed commit sealed.
fn cut_then_crash(fault: CommitFault, opts: &Options, files: usize, churn: (usize, usize, usize), flush: Option<usize>) -> u64 {
    let what = format!("{fault:?}");
    let mem = MemIo::new();
    let (io, cs) = open(&mem, opts);
    let mut w = World::new(DictMetadataStore::new(io));
    if let Some(n) = flush {
        w.store.test_set_tree_flush(n);
    }
    for _ in 0..files {
        w.add();
    }
    let a = w.store.commit().unwrap();
    let state_a = w.state.clone();

    w.churn(churn.0, churn.1, churn.2);
    let packs = cs.stats().packs;
    w.store.test_fail_next_commit_at(fault);
    assert!(w.store.commit().is_err(), "{what}: the commit fails");
    let sealed = cs.stats().packs - packs;
    drop(w);
    drop(cs);
    mem.crash();

    // The last logged root is A: it loads to its own state.
    let (io, cs) = open(&mem, opts);
    let loaded = DictMetadataStore::load_from_root(Arc::clone(&io), &a).unwrap();
    check_tree(&loaded, &cs, &a, &state_a, &format!("{what}: root A after the crash"));

    // The same store goes on: a new change and a good commit B.
    let mut w = World::resume(loaded, state_a, flush);
    w.churn(10, 5, 8);
    let b = w.store.commit().unwrap();
    let state_b = w.state.clone();
    drop(w);
    drop(cs);
    mem.crash();

    let (io, cs) = open(&mem, opts);
    let loaded = DictMetadataStore::load_from_root(io, &b).unwrap();
    check_tree(&loaded, &cs, &b, &state_b, &format!("{what}: root B after the second crash"));
    sealed
}

#[test]
#[cfg_attr(miri, ignore = "Miri does not support getgid (DictMetadataStore::new)")]
fn a_cut_commit_reloads_to_a_consistent_tree() {
    // 300 files; 60 manifest changes, 20 deletes, 30 creates.
    for fault in FAULTS {
        cut_then_crash(fault, &Options::default(), 300, (60, 20, 30), None);
    }
}

#[test]
#[cfg_attr(miri, ignore = "Miri does not support getgid (DictMetadataStore::new)")]
fn packs_that_seal_inside_a_commit() {
    let opts = Options { pack_limit: 64 << 10, ..Options::default() };
    for fault in FAULTS {
        let sealed = cut_then_crash(fault, &opts, 3_000, (600, 200, 300), None);
        assert!(sealed >= 1, "{fault:?}: a pack sealed during the failed commit");
    }
}

// The hot-path flush also runs before the cut: with a flush limit of 8 the
// edits of the churn reach the working root in many flushes, and the commit
// that is cut at each fault point applies only what is left. The nodes of the
// flushes are in the working root, not in the root that was logged last.
#[test]
#[cfg_attr(miri, ignore = "Miri does not support getgid (DictMetadataStore::new)")]
fn a_flush_before_the_cut_does_not_break_the_matrix() {
    for fault in FAULTS {
        cut_then_crash(fault, &Options::default(), 300, (60, 20, 30), Some(8));
    }
}

// A content sync that fails (the sync before the root is logged) fails the
// commit and its root is not returned; the next commit writes the root
// again and every change is durable.
#[test]
#[cfg_attr(miri, ignore = "Miri does not support getgid (DictMetadataStore::new)")]
fn a_failed_sync_then_a_good_commit() {
    let opts = Options::default();
    let mem = MemIo::new();
    let (io, cs) = open(&mem, &opts);
    let mut w = World::new(DictMetadataStore::new(io));
    for _ in 0..300 {
        w.add();
    }
    w.store.commit().unwrap();
    w.churn(60, 20, 30);
    w.store.set_pre_root_sync(Arc::new(|| Err(std::io::Error::other("content sync fails"))));
    assert!(w.store.commit().is_err(), "the commit fails at the content sync");
    assert!(w.store.has_uncommitted_changes());
    // Reads still see every change.
    for (ino, v) in &w.state {
        assert_eq!(w.store.get_manifest(*ino).unwrap(), *v);
    }
    w.store.set_pre_root_sync(Arc::new(|| Ok(())));
    w.churn(5, 2, 4);
    let b = w.store.commit().unwrap();
    let state = w.state.clone();
    drop(w);
    drop(cs);
    mem.crash();
    let (io, cs) = open(&mem, &opts);
    let loaded = DictMetadataStore::load_from_root(io, &b).unwrap();
    check_tree(&loaded, &cs, &b, &state, "root after the failed sync and a good commit");
}

// A failed sync of the chunk store (a directory sync, which is not retried)
// stops its writes until the store is opened again, so later commits fail
// too. A power loss then leaves the last logged root, which loads to its
// state, and the reloaded store commits again.
#[test]
#[cfg_attr(miri, ignore = "Miri does not support getgid (DictMetadataStore::new)")]
fn a_failed_chunk_store_sync_stops_writes_until_a_remount() {
    let opts = Options::default();
    let mem = MemIo::new();
    let (io, cs) = open(&mem, &opts);
    let mut w = World::new(DictMetadataStore::new(io));
    for _ in 0..300 {
        w.add();
    }
    let a = w.store.commit().unwrap();
    let state_a = w.state.clone();
    w.churn(60, 20, 30);
    mem.fail_sync_after(0);
    assert!(w.store.commit().is_err(), "the commit fails at the sync");
    assert!(w.store.commit().is_err(), "the store stopped its writes");
    drop(w);
    drop(cs);
    mem.crash();
    let (io, cs) = open(&mem, &opts);
    let loaded = DictMetadataStore::load_from_root(Arc::clone(&io), &a).unwrap();
    check_tree(&loaded, &cs, &a, &state_a, "root A after the failed sync");
    let mut w = World::resume(loaded, state_a, None);
    w.churn(10, 5, 8);
    let b = w.store.commit().unwrap();
    let state_b = w.state.clone();
    drop(w);
    drop(cs);
    mem.crash();
    let (io, cs) = open(&mem, &opts);
    let loaded = DictMetadataStore::load_from_root(io, &b).unwrap();
    check_tree(&loaded, &cs, &b, &state_b, "root B after the remount");
}

/// Offset of the xattr tree root in the root record: after the manifest field.
const XATTR_DATA: usize = MANIFEST_DATA + KEY_BYTES;

/// The xattrs that a root must hold: (ino, name) -> value.
type Xattrs = BTreeMap<(u64, String), Vec<u8>>;

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

/// Validate the xattrs of `root`, as loaded by `meta`, against `want` with
/// the bounded calls: the names of each of `inos` in byte order, each
/// length, each value read whole and a large one also at two offsets; every
/// node of the xattr tree reads and passes the prolly checks, it holds
/// exactly `want.len()` entries, and the first 8 key bytes of each entry are
/// an inode.
fn check_xattrs(meta: &DictMetadataStore, cs: &ChunkStore, root: &ObjectKey, inos: &[u64], want: &Xattrs, what: &str) {
    for ino in inos {
        let mut names = Vec::new();
        meta.for_each_xattr_name(*ino, &mut |n| {
            names.push(n.to_string());
            std::ops::ControlFlow::Continue(())
        })
        .unwrap_or_else(|e| panic!("{what}: names of {ino}: {e}"));
        let expect: Vec<String> = want.keys().filter(|(i, _)| i == ino).map(|(_, n)| n.clone()).collect();
        assert_eq!(names, expect, "{what}: the names of ino {ino}");
    }
    for ((ino, name), v) in want {
        let len = v.len() as u64;
        assert_eq!(meta.xattr_len(*ino, name).unwrap(), len, "{what}: length of {ino} {name}");
        assert!(meta.read_xattr(*ino, name, 0, v.len()).unwrap() == *v, "{what}: value of {ino} {name}");
        if v.len() > 10_000 {
            let at = v.len() / 3;
            assert_eq!(meta.read_xattr(*ino, name, at as u64, 4096).unwrap(), &v[at..at + 4096], "{what}: {name} at {at}");
            assert_eq!(meta.read_xattr(*ino, name, len - 100, 200).unwrap(), &v[v.len() - 100..], "{what}: the tail of {name}");
        }
    }
    let rec = get_value(cs, root).unwrap_or_else(|e| panic!("{what}: root record: {e}"));
    let tree = ObjectId(rec[XATTR_DATA..XATTR_DATA + KEY_BYTES].try_into().unwrap());
    let mut entries = 0u64;
    let all = prolly::visit(&prolly::Ctx::uncached(cs), &tree, &mut |v| {
        for (k, _) in v.entries {
            let ino = u64::from_be_bytes(k[..8].try_into().expect("an 8-byte prefix"));
            meta.get_inode(ino).unwrap_or_else(|e| panic!("{what}: xattr of ino {ino}, not an inode: {e}"));
            entries += 1;
        }
        true
    })
    .unwrap_or_else(|e| panic!("{what}: a node of the xattr tree fails its checks: {e}"));
    assert!(all, "{what}: the walk ends early");
    assert_eq!(entries, want.len() as u64, "{what}: entry count");
}

/// The files and the xattr model of `xattrs_survive_a_cut_commit`.
struct XWorld {
    a: u64,
    b: u64,
    d: u64,
    want: Xattrs,
}

impl XWorld {
    fn set(&mut self, store: &DictMetadataStore, ino: u64, name: &str, v: Vec<u8>) {
        store.set_xattr(ino, name, &v).unwrap();
        self.want.insert((ino, name.to_string()), v);
    }

    fn file(store: &DictMetadataStore) -> u64 {
        store.create_inode(&InodeMeta::new_file(0, 0, 0, 0o100644)).unwrap()
    }

    /// File A with 300 inline names, B with a 200 KiB spilled value, D with 20 names.
    fn build(store: &DictMetadataStore) -> Self {
        let (a, b, d) = (Self::file(store), Self::file(store), Self::file(store));
        let mut w = XWorld { a, b, d, want: Xattrs::new() };
        for i in 0..300u64 {
            w.set(store, a, &format!("user.a{i:03}"), noise(i + 1, 40));
        }
        w.set(store, b, "user.big", noise(500, 200 << 10));
        for i in 0..20u64 {
            w.set(store, d, &format!("user.d{i:02}"), noise(600 + i, 30));
        }
        w
    }

    /// 100 new names and 50 removes on A, B's value replaced by another
    /// spilled value, a spilled value on a new file C, D deleted. Gives C.
    fn edit(&mut self, store: &DictMetadataStore) -> u64 {
        for i in 300..400u64 {
            self.set(store, self.a, &format!("user.a{i:03}"), noise(i + 1, 40));
        }
        for i in (0..300u64).step_by(6) {
            let name = format!("user.a{i:03}");
            store.remove_xattr(self.a, &name).unwrap();
            self.want.remove(&(self.a, name));
        }
        self.set(store, self.b, "user.big", noise(501, 150 << 10));
        let c = Self::file(store);
        self.set(store, c, "user.big", noise(502, 120 << 10));
        store.delete_inode(self.d).unwrap();
        let d = self.d;
        self.want.retain(|(ino, _), _| *ino != d);
        c
    }
}

// PMAP-08 for xattrs: a commit cut at each fault point, then a power loss,
// reloads the last logged root to its xattrs (inline and spilled); a good
// commit of the same edits then reloads to the new state.
#[test]
#[cfg_attr(miri, ignore = "Miri does not support getgid (DictMetadataStore::new)")]
fn xattrs_survive_a_cut_commit() {
    let opts = Options::default();
    for fault in FAULTS {
        let what = format!("{fault:?}");
        let mem = MemIo::new();
        let (io, cs) = open(&mem, &opts);
        let store = DictMetadataStore::new(io);
        // Flushes run before the cut too.
        store.test_set_tree_flush(64);
        let mut w = XWorld::build(&store);
        let r1 = store.commit().unwrap();
        let want_r1 = w.want.clone();
        let c = w.edit(&store);
        let inos = [w.a, w.b, c, w.d];
        store.test_fail_next_commit_at(fault);
        assert!(store.commit().is_err(), "{what}: the commit fails");
        drop(store);
        drop(cs);
        mem.crash();

        let (io, cs) = open(&mem, &opts);
        let loaded = DictMetadataStore::load_from_root(Arc::clone(&io), &r1).unwrap();
        check_xattrs(&loaded, &cs, &r1, &inos, &want_r1, &format!("{what}: R1 after the crash"));

        // The same edits, a good commit, a power loss.
        loaded.test_set_tree_flush(64);
        let mut w = XWorld { want: want_r1, ..w };
        let c = w.edit(&loaded);
        let inos = [w.a, w.b, c, w.d];
        let r2 = loaded.commit().unwrap();
        drop(loaded);
        drop(cs);
        mem.crash();
        let (io, cs) = open(&mem, &opts);
        let loaded = DictMetadataStore::load_from_root(io, &r2).unwrap();
        check_xattrs(&loaded, &cs, &r2, &inos, &w.want, &format!("{what}: R2 after the second crash"));
    }
}
