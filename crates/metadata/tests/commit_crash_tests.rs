//! A commit cut at each write and sync step, then a power loss
//! (`MemIo::crash()` keeps only what is durable): the last logged root loads
//! to five consistent trees that hold exactly the state of that root
//! (PMAP-08, plans 26-05 and 26-12). No sync runs between the node puts of
//! one commit, so a crash at `AfterApply` or `BeforeSync` drops all of them;
//! a small `pack_limit` makes packs seal in the middle of the commit.
//!
//! `check_tree` walks the five trees of a root record and checks the rules
//! between them, then compares them with a model that the test keeps (it is
//! never read back from the store). `random_cuts_replay_to_consistent_trees`
//! runs seeded random schedules: `PMAP08_SEED` and `PMAP08_CASES` set the
//! seed and the case count (defaults `DEFAULT_SEED` and 32).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use chunk_store::mem::MemIo;
use chunk_store::{ChunkStore, ObjectId, Options};
use metadata::inode::deserialize_inode;
use metadata::store::{CommitFault, DictMetadataStore, TestTree};
use metadata::store_io::{get_value, StoreIo};
use metadata::xattr::fits_inline;
use slicefs_traits::key::{ObjectKey, KEY_BYTES};
use slicefs_traits::metadata::{InodeMeta, MetadataStore};

/// Offsets of the tree roots in the root record (pinned by the
/// `root_record_layout` unit test): root dir ino and next ino, then the
/// five trees.
const INODE_DATA: usize = 8 + 8;
const DIR_DATA: usize = INODE_DATA + KEY_BYTES;
const MANIFEST_DATA: usize = DIR_DATA + KEY_BYTES;
const XATTR_DATA: usize = MANIFEST_DATA + KEY_BYTES;
const REFCOUNTS: usize = XATTR_DATA + KEY_BYTES;

const S_IFMT: u32 = 0o170_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFREG: u32 = 0o100_000;

const FAULTS: [CommitFault; 5] = [
    CommitFault::AfterApply,
    CommitFault::BeforeSync,
    CommitFault::AfterInstall,
    CommitFault::BeforePreRootSync,
    CommitFault::BeforeRoot,
];

const TREES: [TestTree; 5] = [TestTree::Inode, TestTree::Dir, TestTree::Manifest, TestTree::Xattr, TestTree::Refcount];

/// Seed of `random_cuts_replay_to_consistent_trees` when `PMAP08_SEED` is not set.
const DEFAULT_SEED: u64 = 0x2612_0808_5EED;

fn open(mem: &MemIo, opts: &Options) -> (Arc<Mutex<StoreIo>>, Arc<ChunkStore>) {
    let cs = Arc::new(ChunkStore::open_with(Arc::new(mem.clone()), opts.clone()).unwrap());
    (Arc::new(Mutex::new(StoreIo::new("/unused", Arc::clone(&cs)))), cs)
}

/// The xattrs that a root must hold: (ino, name) -> value.
type Xattrs = BTreeMap<(u64, String), Vec<u8>>;

/// The fields of an inode that the operations of the tests change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Node {
    mode: u32,
    size: u64,
    nlinks: u32,
}

impl Node {
    fn of(m: &InodeMeta) -> Self {
        Node { mode: m.mode, size: m.size, nlinks: m.nlinks }
    }

    fn is_dir(&self) -> bool {
        self.mode & S_IFMT == S_IFDIR
    }
}

/// The state that a root must hold, kept by the test.
#[derive(Clone, Debug, Default)]
struct Model {
    inodes: BTreeMap<u64, Node>,
    /// The names other than `.` and `..`: (directory, name) -> child.
    names: BTreeMap<(u64, String), u64>,
    /// The parent of each directory (the root's is 1).
    parent: BTreeMap<u64, u64>,
    manifests: BTreeMap<u64, Vec<ObjectKey>>,
    xattrs: Xattrs,
    refcounts: BTreeMap<ObjectKey, u64>,
}

/// The (key, value) entries of a tree, in key order.
type Entries = Vec<(Vec<u8>, Vec<u8>)>;

macro_rules! ensure {
    ($c:expr, $($fmt:tt)+) => {
        if !$c {
            return Err(format!($($fmt)+));
        }
    };
}

/// Every entry of the tree whose root is at offset `at` of `rec`: each node
/// reads and passes the prolly checks (`prolly::visit` checks each node
/// against its parent entry), the keys ascend, and the count in the root
/// (`prolly::count`) equals the walk.
fn walk(cs: &ChunkStore, rec: &[u8], at: usize, tree: &str) -> Result<Entries, String> {
    let root = ObjectId(rec[at..at + KEY_BYTES].try_into().unwrap());
    let ctx = prolly::Ctx::uncached(cs);
    let mut out = Entries::new();
    let all = prolly::visit(&ctx, &root, &mut |v| {
        out.extend(v.entries.iter().cloned());
        true
    })
    .map_err(|e| format!("{tree}: a node fails its checks: {e}"))?;
    ensure!(all, "{tree}: the walk ends early");
    ensure!(out.windows(2).all(|w| w[0].0 < w[1].0), "{tree}: the keys do not ascend");
    let n = prolly::count(&ctx, &root).map_err(|e| format!("{tree}: count: {e}"))?;
    ensure!(n == out.len() as u64, "{tree}: the root counts {n} entries, the walk {}", out.len());
    Ok(out)
}

fn ino_of(k: &[u8]) -> u64 {
    u64::from_be_bytes(k[..8].try_into().unwrap())
}

fn name_of(k: &[u8], tree: &str) -> Result<String, String> {
    String::from_utf8(k[8..].to_vec()).map_err(|_| format!("{tree}: a name is not UTF-8: {k:?}"))
}

/// Validate the five trees of `root`, as loaded by `meta`.
///
/// The rules between the trees first: every inode record is 56 or 72 bytes
/// and holds its own ino; every directory has `.` (itself) and `..` (a
/// directory; 1 for the root); every entry is in a directory, its child is
/// an inode and its type byte is the child's type; every manifest ino and
/// the ino of every xattr entry (the first 8 key bytes) is an inode; an
/// xattr is inline exactly when it fits, and every value reads
/// (`xattr_len`, `read_xattr`); every refcount is at least 1;
/// `inode_count` is the entry count of the inode tree and `logical_bytes`
/// the sum of the sizes; `list_directory` gives the entries of the tree.
/// Then the state equals `want`.
fn check_tree(meta: &DictMetadataStore, cs: &ChunkStore, root: &ObjectKey, want: &Model) -> Result<(), String> {
    let rec = get_value(cs, root).map_err(|e| format!("root record: {e}"))?;
    let inode_entries = walk(cs, &rec, INODE_DATA, "inode_data")?;
    let dir_entries = walk(cs, &rec, DIR_DATA, "dir_data")?;
    let manifest_entries = walk(cs, &rec, MANIFEST_DATA, "manifest_data")?;
    let xattr_entries = walk(cs, &rec, XATTR_DATA, "xattr_data")?;
    let refcount_entries = walk(cs, &rec, REFCOUNTS, "refcounts")?;

    // inode_data.
    let mut inodes: BTreeMap<u64, Node> = BTreeMap::new();
    let mut bytes = 0u64;
    for (k, v) in &inode_entries {
        ensure!(k.len() == 8, "inode_data: a key of {} bytes", k.len());
        let ino = ino_of(k);
        ensure!(v.len() == 56 || v.len() == 72, "inode_data: the record of {ino} has {} bytes", v.len());
        let m = deserialize_inode(v).map_err(|e| format!("inode_data: the record of {ino}: {e}"))?;
        ensure!(m.ino == ino, "inode_data: the record under {ino} holds ino {}", m.ino);
        bytes += m.size;
        inodes.insert(ino, Node::of(&m));
    }
    ensure!(meta.inode_count() == inodes.len() as u64, "inode_count {} but the inode tree has {}", meta.inode_count(), inodes.len());
    ensure!(meta.logical_bytes() == bytes, "logical_bytes {} but the sizes add to {bytes}", meta.logical_bytes());

    // dir_data.
    let mut dirs: BTreeMap<u64, BTreeMap<String, u64>> = BTreeMap::new();
    for (k, v) in &dir_entries {
        ensure!(k.len() > 8, "dir_data: a key of {} bytes", k.len());
        let (dir, name) = (ino_of(k), name_of(k, "dir_data")?);
        ensure!(v.len() == 9, "dir_data: the value of {dir}/{name} has {} bytes", v.len());
        let child = u64::from_le_bytes(v[..8].try_into().unwrap());
        let d = inodes.get(&dir).ok_or_else(|| format!("dir_data: entry {name} is in {dir}, which is not an inode"))?;
        ensure!(d.is_dir(), "dir_data: entry {name} is in {dir}, which is not a directory");
        let c = inodes.get(&child).ok_or_else(|| format!("dir_data: child {child} of {dir}/{name} is not an inode"))?;
        ensure!(v[8] as u32 == (c.mode & S_IFMT) >> 12, "dir_data: type byte {} of {dir}/{name}, child mode {:o}", v[8], c.mode);
        dirs.entry(dir).or_default().insert(name, child);
    }
    for (ino, n) in &inodes {
        if !n.is_dir() {
            continue;
        }
        let e = dirs.get(ino).ok_or_else(|| format!("directory {ino} has no entries"))?;
        ensure!(e.get(".") == Some(ino), "directory {ino}: `.` is {:?}", e.get("."));
        let up = e.get("..").ok_or_else(|| format!("directory {ino} has no `..`"))?;
        ensure!(inodes.get(up).is_some_and(Node::is_dir), "directory {ino}: `..` is {up}, not a directory");
        ensure!(*ino != 1 || *up == 1, "the root's `..` is {up}");
        let mut listed: Vec<(String, u64)> =
            meta.list_directory(*ino).map_err(|e| format!("list_directory({ino}): {e}"))?.into_iter().map(|e| (e.name, e.ino)).collect();
        listed.sort();
        let tree: Vec<(String, u64)> = e.iter().map(|(n, c)| (n.clone(), *c)).collect();
        ensure!(listed == tree, "list_directory({ino}) gives {listed:?}, the tree {tree:?}");
    }

    // manifest_data.
    let mut manifests: BTreeMap<u64, ObjectKey> = BTreeMap::new();
    for (k, v) in &manifest_entries {
        ensure!(k.len() == 8 && v.len() == KEY_BYTES, "manifest_data: an entry of {} and {} bytes", k.len(), v.len());
        let ino = ino_of(k);
        ensure!(inodes.contains_key(&ino), "manifest_data: ino {ino} is not an inode");
        let digest = ObjectKey(v.as_slice().try_into().unwrap());
        ensure!(meta.manifest_digest(ino) == Some(digest), "manifest_data: the digest of {ino} is not what the store reads");
        meta.get_manifest(ino).map_err(|e| format!("manifest of {ino}: {e}"))?;
        manifests.insert(ino, digest);
    }

    // xattr_data.
    let mut xattrs = Xattrs::new();
    for (k, v) in &xattr_entries {
        ensure!(k.len() > 8 && !v.is_empty(), "xattr_data: an entry of {} and {} bytes", k.len(), v.len());
        let (ino, name) = (ino_of(k), name_of(k, "xattr_data")?);
        ensure!(inodes.contains_key(&ino), "xattr_data: {name} of ino {ino}, which is not an inode");
        let len = meta.xattr_len(ino, &name).map_err(|e| format!("xattr_len({ino}, {name}): {e}"))?;
        let value = meta.read_xattr(ino, &name, 0, len as usize).map_err(|e| format!("read_xattr({ino}, {name}): {e}"))?;
        ensure!(value.len() as u64 == len, "xattr {ino} {name}: {} bytes read of {len}", value.len());
        match v[0] {
            0 => {
                ensure!(v[1..] == value[..], "xattr {ino} {name}: the inline value is not what the store reads");
                ensure!(fits_inline(name.len(), value.len()), "xattr {ino} {name}: inline, but {len} bytes do not fit");
            }
            1 => {
                ensure!(v.len() == 1 + KEY_BYTES, "xattr {ino} {name}: a spilled value of {} bytes", v.len());
                ensure!(!fits_inline(name.len(), value.len()), "xattr {ino} {name}: spilled, but {len} bytes fit inline");
            }
            t => return Err(format!("xattr {ino} {name}: tag {t}")),
        }
        xattrs.insert((ino, name), value);
    }

    // refcounts.
    let mut refcounts: BTreeMap<ObjectKey, u64> = BTreeMap::new();
    for (k, v) in &refcount_entries {
        ensure!(k.len() == KEY_BYTES && v.len() == 8, "refcounts: an entry of {} and {} bytes", k.len(), v.len());
        let key = ObjectKey(k.as_slice().try_into().unwrap());
        let n = u64::from_le_bytes(v.as_slice().try_into().unwrap());
        ensure!(n >= 1, "refcounts: {key:?} counts {n}");
        ensure!(meta.get_refcount(&key) == n, "refcounts: the store reads {} for {key:?}, the tree {n}", meta.get_refcount(&key));
        refcounts.insert(key, n);
    }

    // The state equals the model.
    ensure!(inodes == want.inodes, "the inodes: tree {inodes:?}\nmodel {:?}", want.inodes);
    let names: BTreeMap<(u64, String), u64> = dirs
        .iter()
        .flat_map(|(d, e)| e.iter().filter(|(n, _)| *n != "." && *n != "..").map(|(n, c)| ((*d, n.clone()), *c)))
        .collect();
    ensure!(names == want.names, "the names: tree {names:?}\nmodel {:?}", want.names);
    let parent: BTreeMap<u64, u64> = dirs.iter().map(|(d, e)| (*d, e[".."])).collect();
    ensure!(parent == want.parent, "the `..` entries: tree {parent:?}\nmodel {:?}", want.parent);
    ensure!(
        manifests.keys().eq(want.manifests.keys()),
        "the manifest inos: tree {:?}\nmodel {:?}",
        manifests.keys().collect::<Vec<_>>(),
        want.manifests.keys().collect::<Vec<_>>()
    );
    for (ino, blocks) in &want.manifests {
        ensure!(meta.get_manifest(*ino).ok().as_ref() == Some(blocks), "the manifest of {ino}");
    }
    ensure!(xattrs == want.xattrs, "the xattrs: tree {:?}\nmodel {:?}", xattrs.keys().collect::<Vec<_>>(), want.xattrs.keys().collect::<Vec<_>>());
    for ino in inodes.keys() {
        let mut got = Vec::new();
        meta.for_each_xattr_name(*ino, &mut |n| {
            got.push(n.to_string());
            std::ops::ControlFlow::Continue(())
        })
        .map_err(|e| format!("xattr names of {ino}: {e}"))?;
        let expect: Vec<String> = want.xattrs.keys().filter(|(i, _)| i == ino).map(|(_, n)| n.clone()).collect();
        ensure!(got == expect, "the xattr names of {ino}: {got:?}, model {expect:?}");
    }
    ensure!(refcounts == want.refcounts, "the refcounts: tree {refcounts:?}\nmodel {:?}", want.refcounts);
    Ok(())
}

/// `check_tree` that panics with `what` and the error.
fn check(meta: &DictMetadataStore, cs: &ChunkStore, root: &ObjectKey, want: &Model, what: &str) {
    if let Err(e) = check_tree(meta, cs, root, want) {
        panic!("{what}: {e}");
    }
}

/// Seeded numbers (xorshift).
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below(hi - lo + 1)
    }

    fn pick<T: Clone>(&mut self, v: &[T]) -> Option<T> {
        (!v.is_empty()).then(|| v[self.below(v.len())].clone())
    }
}

/// A store, the model of its state, and the changes that keep both in step
/// (each one mirrors what the store does: `link` and `unlink` do not change
/// `nlinks`, so the changes do that as the file system does).
struct World {
    store: Arc<DictMetadataStore>,
    m: Model,
    /// Next manifest value (each is distinct) and next name number.
    next: u32,
    names: u64,
}

impl World {
    fn new(store: DictMetadataStore, flush: Option<usize>) -> Self {
        if let Some(n) = flush {
            store.test_set_tree_flush(n);
        }
        let mut m = Model::default();
        m.inodes.insert(1, Node::of(&store.get_inode(1).unwrap()));
        m.parent.insert(1, 1);
        World { store: Arc::new(store), m, next: 0, names: 0 }
    }

    /// Continue from a store reloaded to the state `m`, with the counters
    /// of `counters` (so no name is made twice).
    fn resume(store: DictMetadataStore, m: Model, flush: Option<usize>, counters: (u32, u64)) -> Self {
        if let Some(n) = flush {
            store.test_set_tree_flush(n);
        }
        World { store: Arc::new(store), m, next: counters.0, names: counters.1 }
    }

    fn counters(&self) -> (u32, u64) {
        (self.next, self.names)
    }

    fn name(&mut self, prefix: &str) -> String {
        self.names += 1;
        format!("{prefix}{}", self.names)
    }

    fn value(&mut self) -> Vec<ObjectKey> {
        self.next += 1;
        vec![ObjectKey::example(self.next)]
    }

    fn files(&self) -> Vec<u64> {
        self.m.inodes.iter().filter(|(_, n)| n.mode & S_IFMT == S_IFREG).map(|(i, _)| *i).collect()
    }

    fn dirs(&self) -> Vec<u64> {
        self.m.inodes.iter().filter(|(_, n)| n.is_dir()).map(|(i, _)| *i).collect()
    }

    /// The directories other than the root with no names in them.
    fn empty_dirs(&self) -> Vec<u64> {
        self.dirs().into_iter().filter(|d| *d != 1 && !self.m.names.keys().any(|(p, _)| p == d)).collect()
    }

    fn name_of(&self, child: u64) -> Option<(u64, String)> {
        self.m.names.iter().find(|(_, c)| **c == child).map(|(k, _)| k.clone())
    }

    fn nlinks(&mut self, ino: u64, up: bool) {
        self.store.modify_inode(ino, |m| if up { m.nlinks += 1 } else { m.nlinks -= 1 }).unwrap();
        let n = self.m.inodes.get_mut(&ino).unwrap();
        if up { n.nlinks += 1 } else { n.nlinks -= 1 }
    }

    /// A new empty file named in `dir`.
    fn file(&mut self, dir: u64) -> u64 {
        let ino = self.store.create_inode(&InodeMeta::new_file(0, 0, 0, S_IFREG | 0o644)).unwrap();
        let name = self.name("f");
        self.store.link(dir, &name, ino).unwrap();
        self.m.inodes.insert(ino, Node { mode: S_IFREG | 0o644, size: 0, nlinks: 1 });
        self.m.names.insert((dir, name), ino);
        ino
    }

    /// A new file in the root with a size and a manifest.
    fn add(&mut self) -> u64 {
        let ino = self.file(1);
        self.write(ino, 1000 + ino % 7_000);
        ino
    }

    fn mkdir(&mut self, dir: u64) -> u64 {
        let name = self.name("d");
        let ino = self.store.create_directory(dir, &name, &InodeMeta::new_directory(0, 0, 0, S_IFDIR | 0o755)).unwrap();
        self.m.inodes.insert(ino, Node { mode: S_IFDIR | 0o755, size: 0, nlinks: 2 });
        self.m.names.insert((dir, name), ino);
        self.m.parent.insert(ino, dir);
        // The store adds the link of the new `..` to the parent.
        self.m.inodes.get_mut(&dir).unwrap().nlinks += 1;
        ino
    }

    fn resize(&mut self, ino: u64, size: u64) {
        self.store.modify_inode(ino, |m| m.size = size).unwrap();
        self.m.inodes.get_mut(&ino).unwrap().size = size;
    }

    fn remanifest(&mut self, ino: u64) {
        let v = self.value();
        self.store.set_manifest(ino, &v).unwrap();
        self.m.manifests.insert(ino, v);
    }

    /// A new size and manifest (a file write).
    fn write(&mut self, ino: u64, size: u64) {
        self.resize(ino, size);
        self.remanifest(ino);
    }

    fn set_x(&mut self, ino: u64, name: &str, len: usize) {
        self.next += 1;
        let v = noise(u64::from(self.next), len);
        self.store.set_xattr(ino, name, &v).unwrap();
        self.m.xattrs.insert((ino, name.to_string()), v);
    }

    fn rm_x(&mut self, ino: u64, name: &str) {
        self.store.remove_xattr(ino, name).unwrap();
        self.m.xattrs.remove(&(ino, name.to_string()));
    }

    /// One more name of file `ino`, in `dir`.
    fn hard_link(&mut self, ino: u64, dir: u64) -> String {
        let name = self.name("h");
        self.store.link(dir, &name, ino).unwrap();
        self.m.names.insert((dir, name.clone()), ino);
        self.nlinks(ino, true);
        name
    }

    /// Move `dir/name` (a file, or an empty directory) to `to` with a new name.
    fn rename(&mut self, dir: u64, name: &str, to: u64) {
        let child = self.m.names[&(dir, name.to_string())];
        let new = self.name("r");
        self.store.link(to, &new, child).unwrap();
        self.store.unlink(dir, name).unwrap();
        self.m.names.remove(&(dir, name.to_string()));
        self.m.names.insert((to, new), child);
        if self.m.inodes[&child].is_dir() {
            self.store.set_parent(child, to).unwrap();
            self.m.parent.insert(child, to);
            self.nlinks(dir, false);
            self.nlinks(to, true);
        }
    }

    /// Remove the name `dir/name` of a file; the inode goes with its last name.
    fn unlink(&mut self, dir: u64, name: &str) {
        let child = self.m.names.remove(&(dir, name.to_string())).unwrap();
        self.store.unlink(dir, name).unwrap();
        if self.m.names.values().any(|c| *c == child) {
            self.nlinks(child, false);
        } else {
            self.store.delete_inode(child).unwrap();
            self.drop_inode(child);
        }
    }

    /// Remove the empty directory `dir/name`.
    fn rmdir(&mut self, dir: u64, name: &str) {
        let child = self.m.names.remove(&(dir, name.to_string())).unwrap();
        self.store.unlink(dir, name).unwrap();
        self.store.delete_inode(child).unwrap();
        self.drop_inode(child);
        self.m.parent.remove(&child);
        self.nlinks(dir, false);
    }

    /// `delete_inode` removes the manifest and the xattrs of the inode too.
    fn drop_inode(&mut self, ino: u64) {
        self.m.inodes.remove(&ino);
        self.m.manifests.remove(&ino);
        self.m.xattrs.retain(|(i, _), _| *i != ino);
    }

    fn inc(&mut self, k: u32) {
        let key = ObjectKey::example(k);
        self.store.increment_refcount(&key);
        *self.m.refcounts.entry(key).or_insert(0) += 1;
    }

    fn dec(&mut self, k: u32) {
        let key = ObjectKey::example(k);
        self.store.decrement_refcount(&key);
        if let Some(n) = self.m.refcounts.get_mut(&key) {
            *n -= 1;
            if *n == 0 {
                self.m.refcounts.remove(&key);
            }
        }
    }

    /// Changes in each of the five trees: a directory with files, xattrs
    /// (inline, spilled, a replace that spills and a remove), a hard link
    /// across directories, renames of a file and of an empty directory
    /// across directories, an rmdir, refcounts up and down.
    fn every_tree(&mut self) {
        let d = self.mkdir(1);
        let e = self.mkdir(d);
        let f = self.file(d);
        self.write(f, 5_000);
        let g = self.file(e);
        self.write(g, 300);
        self.set_x(f, "user.small", 40);
        self.set_x(f, "user.big", 30_000);
        self.set_x(g, "user.grow", 10);
        self.set_x(g, "user.grow", 4_000);
        self.set_x(d, "user.dir", 20);
        self.rm_x(f, "user.small");
        self.hard_link(f, 1);
        let (p, n) = self.name_of(g).unwrap();
        self.rename(p, &n, 1);
        let empty = self.mkdir(1);
        let (p, n) = self.name_of(empty).unwrap();
        self.rename(p, &n, d);
        let gone = self.mkdir(e);
        let (p, n) = self.name_of(gone).unwrap();
        self.rmdir(p, &n);
        for k in [1, 2, 2, 3] {
            self.inc(k);
        }
        self.dec(2);
        self.dec(3);
        self.dec(99); // absent: nothing changes
    }

    /// `changed` writes, `deleted` unlinks and `created` new files in the
    /// root, on files spread over the state, then `every_tree`.
    fn churn(&mut self, changed: usize, deleted: usize, created: usize) {
        let files = self.files();
        for ino in files.iter().step_by((files.len() / changed.max(1)).max(1)).take(changed) {
            let size = self.m.inodes[ino].size + 1;
            self.write(*ino, size);
        }
        for ino in files.iter().rev().step_by(3).take(deleted) {
            if let Some((p, n)) = self.name_of(*ino) {
                self.unlink(p, &n);
            }
        }
        for _ in 0..created {
            self.add();
        }
        self.every_tree();
    }

    /// One random change (a create when the change has no target).
    fn random_op(&mut self, r: &mut Rng) {
        let files = self.files();
        let dirs = self.dirs();
        let file_names: Vec<(u64, String)> =
            self.m.names.iter().filter(|(_, c)| files.contains(c)).map(|(k, _)| k.clone()).collect();
        let roll = r.below(100);
        let done = match roll {
            0..=14 => {
                let d = r.pick(&dirs).unwrap();
                let f = self.file(d);
                if r.below(2) == 0 {
                    self.write(f, r.range(1, 100_000) as u64);
                }
                true
            }
            15..=24 => r.pick(&files).map(|f| self.write(f, r.range(0, 100_000) as u64)).is_some(),
            25..=29 => r.pick(&files).map(|f| self.resize(f, r.range(0, 50_000) as u64)).is_some(),
            30..=39 => {
                let ino = r.pick(&files).or_else(|| r.pick(&dirs)).unwrap();
                let old: Vec<String> = self.m.xattrs.keys().filter(|(i, _)| *i == ino).map(|(_, n)| n.clone()).collect();
                let name = match r.pick(&old) {
                    Some(n) if r.below(2) == 0 => n,
                    _ => self.name("user.x"),
                };
                let len = if r.below(4) == 0 { r.range(2_100, 20_000) } else { r.range(0, 200) };
                self.set_x(ino, &name, len);
                true
            }
            40..=46 => {
                let all: Vec<(u64, String)> = self.m.xattrs.keys().cloned().collect();
                r.pick(&all).map(|(i, n)| self.rm_x(i, &n)).is_some()
            }
            47..=53 => match (r.pick(&files), r.pick(&dirs)) {
                (Some(f), Some(d)) => {
                    self.hard_link(f, d);
                    true
                }
                _ => false,
            },
            54..=61 => {
                // A file name, or the name of an empty directory.
                let mut movable = file_names.clone();
                for e in self.empty_dirs() {
                    movable.extend(self.name_of(e));
                }
                match r.pick(&movable) {
                    Some((p, n)) => {
                        let child = self.m.names[&(p, n.clone())];
                        let to: Vec<u64> = dirs.iter().copied().filter(|d| *d != child).collect();
                        let to = r.pick(&to).unwrap();
                        self.rename(p, &n, to);
                        true
                    }
                    None => false,
                }
            }
            62..=68 => {
                let d = r.pick(&dirs).unwrap();
                self.mkdir(d);
                true
            }
            69..=73 => {
                let empty: Vec<(u64, String)> = self.empty_dirs().into_iter().filter_map(|e| self.name_of(e)).collect();
                r.pick(&empty).map(|(p, n)| self.rmdir(p, &n)).is_some()
            }
            74..=81 => r.pick(&file_names).map(|(p, n)| self.unlink(p, &n)).is_some(),
            82..=91 => {
                self.inc(r.range(1, 16) as u32);
                true
            }
            _ => {
                self.dec(r.range(1, 16) as u32);
                true
            }
        };
        if !done {
            self.file(1);
        }
    }

    fn random_ops(&mut self, r: &mut Rng, n: usize) {
        for _ in 0..n {
            self.random_op(r);
        }
    }

    /// Edits of `tree` only (and of the inode tree, for `nlinks`), on `n`
    /// distinct keys: the first key is `files[0]` (with the name `t0` for
    /// the directory tree, `user.t0` for the xattr tree, refcount key 1000).
    fn touch(&mut self, tree: TestTree, files: &[u64], n: usize) {
        for (i, f) in files.iter().take(n).enumerate() {
            match tree {
                TestTree::Inode => self.resize(*f, 777 + i as u64),
                TestTree::Manifest => self.remanifest(*f),
                TestTree::Xattr => self.set_x(files[0], &format!("user.t{i}"), 16),
                TestTree::Dir => {
                    self.store.link(1, &format!("t{i}"), *f).unwrap();
                    self.m.names.insert((1, format!("t{i}")), *f);
                    self.nlinks(*f, true);
                }
                TestTree::Refcount => self.inc(1000 + i as u32),
            }
        }
    }
}

/// The scenario of the matrix at one fault: a committed root A; changes in
/// all five trees; the commit fails at `fault`; a power loss; A reloads to
/// its state; then, on that reloaded store, new changes and a good commit
/// B; a power loss; B reloads to its state. Returns how many packs the
/// failed commit sealed.
fn cut_then_crash(fault: CommitFault, opts: &Options, files: usize, churn: (usize, usize, usize), flush: Option<usize>) -> u64 {
    let what = format!("{fault:?}");
    let mem = MemIo::new();
    let (io, cs) = open(&mem, opts);
    let mut w = World::new(DictMetadataStore::new(io), flush);
    for _ in 0..files {
        w.add();
    }
    w.every_tree();
    let a = w.store.commit().unwrap();
    let state_a = w.m.clone();

    w.churn(churn.0, churn.1, churn.2);
    let packs = cs.stats().packs;
    w.store.test_fail_next_commit_at(fault);
    assert!(w.store.commit().is_err(), "{what}: the commit fails");
    let sealed = cs.stats().packs - packs;
    let counters = w.counters();
    drop(w);
    drop(cs);
    mem.crash();

    // The last logged root is A: it loads to its own state.
    let (io, cs) = open(&mem, opts);
    let loaded = DictMetadataStore::load_from_root(Arc::clone(&io), &a).unwrap();
    check(&loaded, &cs, &a, &state_a, &format!("{what}: root A after the crash"));

    // The same store goes on: new changes and a good commit B.
    let mut w = World::resume(loaded, state_a, flush, counters);
    w.churn(10, 5, 8);
    let b = w.store.commit().unwrap();
    let state_b = w.m.clone();
    drop(w);
    drop(cs);
    mem.crash();

    let (io, cs) = open(&mem, opts);
    let loaded = DictMetadataStore::load_from_root(io, &b).unwrap();
    check(&loaded, &cs, &b, &state_b, &format!("{what}: root B after the second crash"));
    sealed
}

#[test]
#[cfg_attr(miri, ignore = "Miri does not support getgid (DictMetadataStore::new)")]
fn a_cut_commit_reloads_to_a_consistent_tree() {
    // 300 files; 60 writes, 20 unlinks, 30 creates, and the edits of
    // `every_tree`.
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
// edits of the churn reach the working roots in many flushes, and the commit
// that is cut at each fault point applies only what is left. The nodes of the
// flushes are in the working roots, not in the root that was logged last.
#[test]
#[cfg_attr(miri, ignore = "Miri does not support getgid (DictMetadataStore::new)")]
fn a_flush_before_the_cut_does_not_break_the_matrix() {
    for fault in FAULTS {
        cut_then_crash(fault, &Options::default(), 300, (60, 20, 30), Some(8));
    }
}

// The validator finds a hand-made inconsistent state: an inode deleted
// before its name is removed (the public calls in the wrong order), then
// committed. The same store passes before the bad delete.
#[test]
#[cfg_attr(miri, ignore = "Miri does not support getgid (DictMetadataStore::new)")]
fn check_tree_finds_a_name_whose_inode_is_gone() {
    let opts = Options::default();
    let mem = MemIo::new();
    let (io, cs) = open(&mem, &opts);
    let mut w = World::new(DictMetadataStore::new(io), None);
    for _ in 0..20 {
        w.add();
    }
    w.every_tree();
    let a = w.store.commit().unwrap();
    let loaded = DictMetadataStore::load_from_root(Arc::clone(w.store.io()), &a).unwrap();
    check(&loaded, &cs, &a, &w.m, "the consistent store");

    let f = w.files()[3];
    w.store.delete_inode(f).unwrap();
    let b = w.store.commit().unwrap();
    let loaded = DictMetadataStore::load_from_root(Arc::clone(w.store.io()), &b).unwrap();
    let e = check_tree(&loaded, &cs, &b, &w.m).expect_err("a name without an inode is found");
    assert!(e.contains(&format!("child {f} of")) && e.contains("is not an inode"), "{e}");
}

// A failed hot-path flush of each tree requeues its entries and keeps the
// error: the next commit fails with it, the commit after that writes every
// change, and a power loss then reloads it to the model.
#[test]
#[cfg_attr(miri, ignore = "Miri does not support getgid (DictMetadataStore::new)")]
fn a_failed_flush_of_each_tree_then_a_good_commit() {
    let opts = Options::default();
    for tree in TREES {
        let what = format!("{tree:?}");
        let mem = MemIo::new();
        let (io, cs) = open(&mem, &opts);
        let mut w = World::new(DictMetadataStore::new(io), Some(4));
        for _ in 0..20 {
            w.add();
        }
        w.every_tree();
        w.store.commit().unwrap();
        w.store.test_fail_next_flush_of(tree);
        let files = w.files();
        w.touch(tree, &files, 8);
        let e = w.store.commit().expect_err("the commit after a failed flush fails");
        assert!(e.to_string().contains("injected flush fault"), "{what}: {e}");
        w.every_tree();
        let b = w.store.commit().unwrap();
        let want = w.m.clone();
        drop(w);
        drop(cs);
        mem.crash();
        let (io, cs) = open(&mem, &opts);
        let loaded = DictMetadataStore::load_from_root(io, &b).unwrap();
        check(&loaded, &cs, &b, &want, &format!("{what}: the good commit after the failed flush"));
    }
}

// A change of a key of a flush, made between the install of the flush's
// root and the settle of the overlay (the flush hook of each tree), is
// kept: the settle does not drop the newer edit, the next commit writes it,
// and a power loss then reloads it.
#[test]
#[cfg_attr(miri, ignore = "Miri does not support getgid (DictMetadataStore::new)")]
fn a_change_in_the_flush_window_of_each_tree_is_kept() {
    let opts = Options::default();
    for tree in TREES {
        let what = format!("{tree:?}");
        let mem = MemIo::new();
        let (io, cs) = open(&mem, &opts);
        let mut w = World::new(DictMetadataStore::new(io), Some(4));
        for _ in 0..20 {
            w.add();
        }
        w.store.commit().unwrap();
        let files = w.files();
        let f0 = files[0];
        let ran = Arc::new(AtomicBool::new(false));
        {
            let weak = Arc::downgrade(&w.store);
            let ran = Arc::clone(&ran);
            w.store.test_set_flush_hook_of(
                tree,
                Some(Arc::new(move || {
                    if ran.swap(true, Ordering::SeqCst) {
                        return;
                    }
                    let st = weak.upgrade().unwrap();
                    match tree {
                        TestTree::Inode => st.modify_inode(f0, |m| m.size = 12_345).unwrap(),
                        TestTree::Manifest => st.set_manifest(f0, &[ObjectKey::example(999_999)]).unwrap(),
                        TestTree::Xattr => st.set_xattr(f0, "user.t0", b"set in the window").unwrap(),
                        TestTree::Dir => {
                            st.unlink(1, "t0").unwrap();
                            st.modify_inode(f0, |m| m.nlinks -= 1).unwrap();
                        }
                        TestTree::Refcount => st.increment_refcount(&ObjectKey::example(1000)),
                    }
                })),
            );
        }
        w.touch(tree, &files, 4);
        assert!(ran.load(Ordering::SeqCst), "{what}: a flush of the tree ran the hook");
        w.store.test_set_flush_hook_of(tree, None);
        // What the hook did, in the model.
        match tree {
            TestTree::Inode => w.m.inodes.get_mut(&f0).unwrap().size = 12_345,
            TestTree::Manifest => {
                w.m.manifests.insert(f0, vec![ObjectKey::example(999_999)]);
            }
            TestTree::Xattr => {
                w.m.xattrs.insert((f0, "user.t0".into()), b"set in the window".to_vec());
            }
            TestTree::Dir => {
                w.m.names.remove(&(1, "t0".into()));
                w.m.inodes.get_mut(&f0).unwrap().nlinks -= 1;
            }
            TestTree::Refcount => *w.m.refcounts.get_mut(&ObjectKey::example(1000)).unwrap() += 1,
        }
        let b = w.store.commit().unwrap();
        let want = w.m.clone();
        drop(w);
        drop(cs);
        mem.crash();
        let (io, cs) = open(&mem, &opts);
        let loaded = DictMetadataStore::load_from_root(io, &b).unwrap();
        check(&loaded, &cs, &b, &want, &format!("{what}: the change in the flush window"));
    }
}

/// One case of the random schedule: random changes and a good commit A;
/// more changes and a commit at a random fault point (or none); a power
/// loss; the last logged root reloads to its model; then changes on the
/// reloaded store, a good commit, a power loss, and that root reloads too.
fn random_case(seed: u64) {
    let mut r = Rng::new(seed);
    let flush = [Some(1), Some(4), Some(8), None][r.below(4)];
    let opts = Options::default();
    let mem = MemIo::new();
    let (io, cs) = open(&mem, &opts);
    let mut w = World::new(DictMetadataStore::new(io), flush);
    let n = r.range(20, 60);
    w.random_ops(&mut r, n);
    let a = w.store.commit().unwrap();
    let state_a = w.m.clone();

    let n = r.range(20, 60);
    w.random_ops(&mut r, n);
    let fault = [None, Some(FAULTS[0]), Some(FAULTS[1]), Some(FAULTS[2]), Some(FAULTS[3]), Some(FAULTS[4])][r.below(6)];
    let what = format!("seed {seed:#x} flush {flush:?} fault {fault:?}");
    eprintln!("  flush {flush:?} fault {fault:?}");
    let (root, want) = match fault {
        Some(f) => {
            w.store.test_fail_next_commit_at(f);
            assert!(w.store.commit().is_err(), "{what}: the commit fails");
            (a, state_a)
        }
        None => (w.store.commit().unwrap(), w.m.clone()),
    };
    let counters = w.counters();
    drop(w);
    drop(cs);
    mem.crash();
    let (io, cs) = open(&mem, &opts);
    let loaded = DictMetadataStore::load_from_root(Arc::clone(&io), &root).unwrap();
    check(&loaded, &cs, &root, &want, &format!("{what}: the last logged root after the crash"));

    let mut w = World::resume(loaded, want, flush, counters);
    let n = r.range(10, 30);
    w.random_ops(&mut r, n);
    let c = w.store.commit().unwrap();
    let want = w.m.clone();
    drop(w);
    drop(cs);
    mem.crash();
    let (io, cs) = open(&mem, &opts);
    let loaded = DictMetadataStore::load_from_root(io, &c).unwrap();
    check(&loaded, &cs, &c, &want, &format!("{what}: the commit after the reload"));
}

fn env_u64(name: &str, default: u64) -> u64 {
    match std::env::var(name) {
        Ok(v) => {
            let v = v.trim();
            let parsed = match v.strip_prefix("0x") {
                Some(hex) => u64::from_str_radix(hex, 16),
                None => v.parse(),
            };
            parsed.unwrap_or_else(|_| panic!("{name}={v} is not a number"))
        }
        Err(_) => default,
    }
}

// Seeded random schedules (create, write, xattr set and remove, hard link,
// rename across directories, mkdir, rmdir, unlink, refcount changes) with a
// random fault point per case. A failure names the case and its seed; run
// one case again with PMAP08_SEED=<case seed> PMAP08_CASES=1.
#[test]
#[cfg_attr(miri, ignore = "Miri does not support getgid (DictMetadataStore::new)")]
fn random_cuts_replay_to_consistent_trees() {
    let seed = env_u64("PMAP08_SEED", DEFAULT_SEED);
    let cases = env_u64("PMAP08_CASES", 32);
    eprintln!("PMAP08_SEED={seed:#x} PMAP08_CASES={cases}");
    let mut r = Rng::new(seed);
    for case in 0..cases {
        // Case 0 uses the seed itself, so PMAP08_SEED=<case seed> repeats a case.
        let s = if case == 0 { seed } else { r.next() };
        eprintln!("case {case} seed {s:#x}");
        random_case(s);
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
    let mut w = World::new(DictMetadataStore::new(io), None);
    for _ in 0..300 {
        w.add();
    }
    w.store.commit().unwrap();
    w.churn(60, 20, 30);
    w.store.set_pre_root_sync(Arc::new(|| Err(std::io::Error::other("content sync fails"))));
    assert!(w.store.commit().is_err(), "the commit fails at the content sync");
    assert!(w.store.has_uncommitted_changes());
    // Reads still see every change.
    for (ino, v) in &w.m.manifests {
        assert_eq!(w.store.get_manifest(*ino).unwrap(), *v);
    }
    w.store.set_pre_root_sync(Arc::new(|| Ok(())));
    w.churn(5, 2, 4);
    let b = w.store.commit().unwrap();
    let state = w.m.clone();
    drop(w);
    drop(cs);
    mem.crash();
    let (io, cs) = open(&mem, &opts);
    let loaded = DictMetadataStore::load_from_root(io, &b).unwrap();
    check(&loaded, &cs, &b, &state, "root after the failed sync and a good commit");
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
    let mut w = World::new(DictMetadataStore::new(io), None);
    for _ in 0..300 {
        w.add();
    }
    let a = w.store.commit().unwrap();
    let state_a = w.m.clone();
    w.churn(60, 20, 30);
    mem.fail_sync_after(0);
    assert!(w.store.commit().is_err(), "the commit fails at the sync");
    assert!(w.store.commit().is_err(), "the store stopped its writes");
    let counters = w.counters();
    drop(w);
    drop(cs);
    mem.crash();
    let (io, cs) = open(&mem, &opts);
    let loaded = DictMetadataStore::load_from_root(Arc::clone(&io), &a).unwrap();
    check(&loaded, &cs, &a, &state_a, "root A after the failed sync");
    let mut w = World::resume(loaded, state_a, None, counters);
    w.churn(10, 5, 8);
    let b = w.store.commit().unwrap();
    let state_b = w.m.clone();
    drop(w);
    drop(cs);
    mem.crash();
    let (io, cs) = open(&mem, &opts);
    let loaded = DictMetadataStore::load_from_root(io, &b).unwrap();
    check(&loaded, &cs, &b, &state_b, "root B after the remount");
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
