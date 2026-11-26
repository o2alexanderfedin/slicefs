//! The rehash engine: write every live value of a store again under another
//! hash, and give the roots that refer to the new keys (`slicefs rehash`).
//!
//! Every file content and every metadata node is a value: bytes in FastCDC
//! chunks under a manifest tree (`chunk_store::file`). The engine has two
//! primitives:
//!
//! - **Byte copy of a value.** File content and the inode, directory and
//!   xattr nodes hold no content address, so their bytes stay the same.
//!   They are streamed from a [`FileReader`] (every object read is
//!   verified, a read window of [`WINDOW`] bytes) into a [`FileWriter`] of
//!   the store that is open with the target hasher and the store profile.
//!   The new key is the key of a fresh write of the same bytes. The tree is
//!   not translated node by node: the manifest node cuts depend on the IDs
//!   (`is_cut`), so only the real writer gives the fresh-write key. A
//!   directory entry holds the BLAKE3-256 of its name, not a content
//!   address; it is inside the copied bytes and does not change.
//! - **Translation of a key-carrying node.** The manifest lists, the four
//!   `u64 -> key` maps, the inode map, the refcount table and the root
//!   record are read in windows of whole records; each key is replaced by
//!   its new key, and the node is written again through a `FileWriter`.
//!   These nodes are always translated, also when their own key has the
//!   target tag: they can list keys of another tag. The refcount rows are
//!   sorted by their new keys with the external sorter of the chunk store,
//!   and rows that get one new key are merged.
//!
//! RAM does not grow with the object count: per value one read window, one
//! reader and one writer (one path of nested values at a time: root record,
//! map, manifest list, content); a fixed map cache; the fixed buffers of
//! the sorter. No value and no map is read into RAM whole (only the root
//! record, which has a fixed size).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chunk_store::extsort::{self, SortCfg, Sorter, TmpDir};
use chunk_store::file::{FileReader, FileWriter};
use chunk_store::{ChunkStore, ObjectHasher};
use slicefs_traits::key::{ObjectKey, KEY_BYTES};

use crate::inode_map::RECORD;
use crate::snapshot::SnapshotEntry;
use crate::store::{root_rec, MAP_ENTRY};
use crate::store_io::{to_id, to_key};

/// Bytes read from a value at a time.
pub const WINDOW: usize = 1 << 20;

/// The roots of a store: the current root and the snapshot roots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootSet {
    pub current: ObjectKey,
    pub snapshots: Vec<SnapshotEntry>,
}

/// What a run does.
pub enum Mode {
    /// Write the new objects. `map_dir` holds the temp files of the run (a
    /// new directory each run; the caller removes it).
    Rewrite { map_dir: PathBuf },
    /// Write nothing: read and verify what a real run reads, and count.
    DryRun,
}

/// The options of a run.
pub struct RehashOptions<'a> {
    /// The hash of the new objects. In [`Mode::Rewrite`] the store must be
    /// open with this hasher.
    pub target: &'static dyn ObjectHasher,
    pub mode: Mode,
    /// Least time between two calls of `progress`.
    pub progress_every: Duration,
    /// Called with the running counts.
    pub progress: Option<&'a mut dyn FnMut(&Stats)>,
    /// For crash tests: stop with [`RehashError::Stopped`] right after this
    /// many rewritten values, with no sync and no cleanup.
    #[doc(hidden)]
    pub stop_after_values: Option<u64>,
}

/// Counts of a run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Roots to do (the current root and the snapshots).
    pub roots_total: u64,
    pub roots_done: u64,
    /// Values (contents and nodes) read, and their bytes.
    pub values_read: u64,
    pub bytes_read: u64,
    /// Values written again under the target hash.
    pub values_rewritten: u64,
    /// Values kept as they are (every object already has the target tag).
    pub values_kept: u64,
    /// Values found in the old-to-new map (done before in this run).
    pub map_hits: u64,
    /// New objects in the store, and their stored bytes.
    pub objects_written: u64,
    pub stored_bytes_written: u64,
    /// Objects of another tag in the values that a real run rewrites, and
    /// the bytes of those values (an estimate of the work; dry-run only).
    pub other_tag_objects: u64,
    pub other_tag_bytes: u64,
    /// Refcount rows dropped: their object is missing and no manifest of
    /// the root uses it.
    pub stale_refcounts: u64,
}

/// Why a run stopped.
#[derive(Debug, thiserror::Error)]
pub enum RehashError {
    #[error("cannot read the {what} {}: {source}", .key.to_hex())]
    Read { what: &'static str, key: ObjectKey, source: chunk_store::Error },
    #[error("cannot write a new object: {0}")]
    Write(chunk_store::Error),
    #[error("damaged metadata: {0}")]
    Corrupt(String),
    #[error("rehash temp files: {0}")]
    Io(#[from] std::io::Error),
    #[error("the store writes with {store}, not with the target {target}")]
    WrongHasher { store: &'static str, target: &'static str },
    #[error("stopped after {0} rewritten values")]
    Stopped(u64),
}

/// The node kind of a map entry: one old key can be both a file content
/// and a node, and each has its own translation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Value = 1,
    ManifestList = 2,
    ValueMap = 3,
    ManifestMap = 4,
    InodeMap = 5,
    Refcounts = 6,
    Root = 7,
}

/// A fixed-size map from (kind, old key) to new key in RAM (a stand-in).
struct StandIn {
    slots: Vec<Option<(Kind, ObjectKey, ObjectKey)>>,
}

const STAND_IN_SLOTS: usize = 1 << 12;

impl StandIn {
    fn new() -> Self {
        StandIn { slots: vec![None; STAND_IN_SLOTS] }
    }

    fn at(kind: Kind, old: &ObjectKey) -> usize {
        let h = u64::from_le_bytes(old.digest()[..8].try_into().unwrap()) ^ (kind as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        h as usize & (STAND_IN_SLOTS - 1)
    }

    fn get(&mut self, kind: Kind, old: &ObjectKey) -> std::io::Result<Option<ObjectKey>> {
        Ok(match self.slots[Self::at(kind, old)] {
            Some((k, o, n)) if k == kind && o == *old => Some(n),
            _ => None,
        })
    }

    fn insert(&mut self, kind: Kind, old: &ObjectKey, new: &ObjectKey) -> std::io::Result<()> {
        self.slots[Self::at(kind, old)] = Some((kind, *old, *new));
        Ok(())
    }
}

/// A refcount sort row: new key, count (big-endian), row index
/// (big-endian). The index keeps two equal rows apart: the sorter removes
/// identical rows.
const REF_ROW: usize = KEY_BYTES + 16;

/// A key-carrying node of records.
#[derive(Clone, Copy)]
enum Blob {
    /// A manifest list: content keys.
    ManifestList,
    /// A `u64 -> key` map of inode, directory or xattr nodes.
    ValueMap(&'static str),
    /// The `u64 -> key` map of manifest lists.
    ManifestMap,
    /// The inode map: `ino + key` records with no count.
    InodeMap,
}

impl Blob {
    fn kind(self) -> Kind {
        match self {
            Blob::ManifestList => Kind::ManifestList,
            Blob::ValueMap(_) => Kind::ValueMap,
            Blob::ManifestMap => Kind::ManifestMap,
            Blob::InodeMap => Kind::InodeMap,
        }
    }

    fn what(self) -> &'static str {
        match self {
            Blob::ManifestList => "manifest list",
            Blob::ValueMap(_) | Blob::ManifestMap => "u64-key map",
            Blob::InodeMap => "inode map",
        }
    }

    /// Bytes of the count before the records (0: no count).
    fn head(self) -> usize {
        match self {
            Blob::ManifestList | Blob::InodeMap => 0,
            Blob::ValueMap(_) | Blob::ManifestMap => 8,
        }
    }

    fn record(self) -> usize {
        match self {
            Blob::ManifestList => KEY_BYTES,
            Blob::ValueMap(_) | Blob::ManifestMap => MAP_ENTRY,
            Blob::InodeMap => RECORD,
        }
    }
}

struct Walker<'c, 'a> {
    cs: &'c ChunkStore,
    /// `None` in a dry-run.
    map_dir: Option<PathBuf>,
    map: StandIn,
    stats: Stats,
    progress: Option<&'a mut dyn FnMut(&Stats)>,
    every: Duration,
    last: Instant,
    stop_after: Option<u64>,
}

impl Walker<'_, '_> {
    fn dry(&self) -> bool {
        self.map_dir.is_none()
    }

    fn tick(&mut self, force: bool) {
        if let Some(p) = self.progress.as_mut()
            && (force || self.last.elapsed() >= self.every)
        {
            p(&self.stats);
            self.last = Instant::now();
        }
    }

    fn hit(&mut self, kind: Kind, old: &ObjectKey) -> Result<Option<ObjectKey>, RehashError> {
        let got = self.map.get(kind, old)?;
        if got.is_some() {
            self.stats.map_hits += 1;
        }
        Ok(got)
    }

    /// End a value: the new key from the writer (counted as rewritten), or
    /// in a dry-run the old key.
    fn done(&mut self, kind: Kind, old: ObjectKey, w: Option<FileWriter>) -> Result<ObjectKey, RehashError> {
        let new = match w {
            Some(w) => {
                let new = to_key(w.finish(self.cs).map_err(RehashError::Write)?);
                self.stats.values_rewritten += 1;
                new
            }
            None => old,
        };
        self.map.insert(kind, &old, &new)?;
        if let Some(n) = self.stop_after
            && self.stats.values_rewritten == n
            && !self.dry()
        {
            return Err(RehashError::Stopped(n));
        }
        self.tick(false);
        Ok(new)
    }

    /// The new key of the value `old` (file content or an inode, directory
    /// or xattr node): its bytes streamed into a new writer.
    fn value(&mut self, what: &'static str, old: ObjectKey) -> Result<ObjectKey, RehashError> {
        if let Some(new) = self.hit(Kind::Value, &old)? {
            return Ok(new);
        }
        let cs = self.cs;
        let err = |source| RehashError::Read { what, key: old, source };
        let mut r = FileReader::open(cs, to_id(&old)).map_err(err)?;
        let len = r.len();
        let mut w = (!self.dry()).then(FileWriter::new);
        let mut off = 0u64;
        while off < len {
            let b = r.read(cs, off, WINDOW).map_err(err)?;
            if b.is_empty() {
                return Err(RehashError::Corrupt(format!("{what} {}: short read at {off}", old.to_hex())));
            }
            if let Some(w) = w.as_mut() {
                w.write(cs, &b).map_err(RehashError::Write)?;
            }
            off += b.len() as u64;
        }
        self.stats.values_read += 1;
        self.stats.bytes_read += len;
        self.done(Kind::Value, old, w)
    }

    /// The new key of a key-carrying node of records: each key translated,
    /// the records in the same order.
    fn blob(&mut self, b: Blob, old: ObjectKey) -> Result<ObjectKey, RehashError> {
        let kind = b.kind();
        if let Some(new) = self.hit(kind, &old)? {
            return Ok(new);
        }
        let cs = self.cs;
        let what = b.what();
        let corrupt = |m: String| RehashError::Corrupt(format!("{what} {}: {m}", old.to_hex()));
        let err = |source| RehashError::Read { what, key: old, source };
        let mut r = FileReader::open(cs, to_id(&old)).map_err(err)?;
        let len = r.len();
        let (head, rec) = (b.head(), b.record());
        let mut w = (!self.dry()).then(FileWriter::new);
        let count = if head == 0 {
            if !len.is_multiple_of(rec as u64) {
                return Err(corrupt(format!("{len} bytes is not a multiple of {rec}")));
            }
            len / rec as u64
        } else {
            let h = if len >= 8 { r.read(cs, 0, 8).map_err(err)? } else { Vec::new() };
            if h.len() != 8 {
                return Err(corrupt(format!("{len} bytes, no count")));
            }
            let count = u64::from_le_bytes(h[..8].try_into().unwrap());
            let want = count.checked_mul(rec as u64).and_then(|n| n.checked_add(8));
            if want != Some(len) {
                return Err(corrupt(format!("{len} bytes cannot hold {count} records")));
            }
            if let Some(w) = w.as_mut() {
                w.write(cs, &h).map_err(RehashError::Write)?;
            }
            count
        };
        let per = (WINDOW / rec) as u64;
        let mut off = head as u64;
        let mut done = 0u64;
        let mut last_ino: Option<u64> = None;
        let mut out = Vec::new();
        while done < count {
            let n = per.min(count - done);
            let buf = r.read(cs, off, n as usize * rec).map_err(err)?;
            if buf.len() != n as usize * rec {
                return Err(corrupt(format!("short read at {off}")));
            }
            out.clear();
            for one in buf.chunks_exact(rec) {
                let new = match b {
                    Blob::ManifestList => self.value("file content", ObjectKey::read(one, 0))?,
                    _ => {
                        let ino = u64::from_le_bytes(one[..8].try_into().unwrap());
                        if last_ino.is_some_and(|p| ino <= p) {
                            return Err(corrupt(format!("inode {ino} out of order")));
                        }
                        last_ino = Some(ino);
                        out.extend_from_slice(&one[..8]);
                        let key = ObjectKey::read(one, 8);
                        match b {
                            Blob::ValueMap(node) => self.value(node, key)?,
                            Blob::ManifestMap => self.blob(Blob::ManifestList, key)?,
                            _ => self.value("inode node", key)?,
                        }
                    }
                };
                out.extend_from_slice(&new.0);
            }
            if let Some(w) = w.as_mut() {
                w.write(cs, &out).map_err(RehashError::Write)?;
            }
            off += buf.len() as u64;
            done += n;
        }
        self.stats.values_read += 1;
        self.stats.bytes_read += len;
        self.done(kind, old, w)
    }

    /// The new key of a refcount table: each key translated, the rows
    /// sorted by new key, the counts of one new key summed.
    fn refcounts(&mut self, old: ObjectKey) -> Result<ObjectKey, RehashError> {
        if let Some(new) = self.hit(Kind::Refcounts, &old)? {
            return Ok(new);
        }
        let cs = self.cs;
        let what = "refcount table";
        let corrupt = |m: String| RehashError::Corrupt(format!("{what} {}: {m}", old.to_hex()));
        let err = |source| RehashError::Read { what, key: old, source };
        let mut r = FileReader::open(cs, to_id(&old)).map_err(err)?;
        let len = r.len();
        let h = if len >= 8 { r.read(cs, 0, 8).map_err(err)? } else { Vec::new() };
        if h.len() != 8 {
            return Err(corrupt(format!("{len} bytes, no count")));
        }
        let count = u64::from_le_bytes(h[..8].try_into().unwrap());
        if count.checked_mul(MAP_ENTRY as u64).and_then(|n| n.checked_add(8)) != Some(len) {
            return Err(corrupt(format!("{len} bytes cannot hold {count} rows")));
        }
        let mut sorter = match &self.map_dir {
            Some(dir) => {
                let tmp = Arc::new(TmpDir::new(dir)?);
                let cfg = SortCfg { chunk_records: extsort::SORT_CHUNK_BYTES / REF_ROW, fan_in: extsort::MERGE_FAN_IN };
                Some(Sorter::<REF_ROW>::new(tmp, "refcounts", cfg))
            }
            None => None,
        };
        let per = (WINDOW / MAP_ENTRY) as u64;
        let (mut off, mut done, mut row) = (8u64, 0u64, 0u64);
        while done < count {
            let n = per.min(count - done);
            let buf = r.read(cs, off, n as usize * MAP_ENTRY).map_err(err)?;
            if buf.len() != n as usize * MAP_ENTRY {
                return Err(corrupt(format!("short read at {off}")));
            }
            for one in buf.as_chunks::<MAP_ENTRY>().0 {
                let key = ObjectKey::read(one, 0);
                let new = match self.value("file content", key) {
                    Ok(new) => new,
                    // No manifest of this root uses it (the manifests
                    // were translated first): a stale count.
                    Err(RehashError::Read { source: chunk_store::Error::Missing(_), .. }) => {
                        self.stats.stale_refcounts += 1;
                        continue;
                    }
                    Err(e) => return Err(e),
                };
                if let Some(s) = sorter.as_mut() {
                    let mut rec = [0u8; REF_ROW];
                    rec[..KEY_BYTES].copy_from_slice(&new.0);
                    rec[KEY_BYTES..KEY_BYTES + 8].copy_from_slice(&u64::from_le_bytes(one[KEY_BYTES..].try_into().unwrap()).to_be_bytes());
                    rec[KEY_BYTES + 8..].copy_from_slice(&row.to_be_bytes());
                    s.push(rec)?;
                }
                row += 1;
            }
            off += buf.len() as u64;
            done += n;
        }
        self.stats.values_read += 1;
        self.stats.bytes_read += len;
        let Some(sorter) = sorter else {
            return self.done(Kind::Refcounts, old, None);
        };
        let sorted = sorter.finish()?;
        // Pass 1: the count of distinct new keys.
        let mut distinct = 0u64;
        let mut prev: Option<[u8; KEY_BYTES]> = None;
        let mut rd = sorted.reader()?;
        while let Some(rec) = rd.next_rec()? {
            let k: [u8; KEY_BYTES] = rec[..KEY_BYTES].try_into().unwrap();
            if prev != Some(k) {
                distinct += 1;
                prev = Some(k);
            }
        }
        // Pass 2: one row per new key with the sum of its counts.
        let mut w = FileWriter::new();
        w.write(cs, &distinct.to_le_bytes()).map_err(RehashError::Write)?;
        let mut out = Vec::new();
        let mut cur: Option<([u8; KEY_BYTES], u64)> = None;
        let mut rd = sorted.reader()?;
        loop {
            let rec = rd.next_rec()?;
            let next = rec.map(|rec| {
                let k: [u8; KEY_BYTES] = rec[..KEY_BYTES].try_into().unwrap();
                (k, u64::from_be_bytes(rec[KEY_BYTES..KEY_BYTES + 8].try_into().unwrap()))
            });
            match (cur, next) {
                (Some((k, sum)), Some((nk, c))) if k == nk => cur = Some((k, sum.saturating_add(c))),
                (c, n) => {
                    if let Some((k, sum)) = c {
                        out.extend_from_slice(&k);
                        out.extend_from_slice(&sum.to_le_bytes());
                        if out.len() >= WINDOW {
                            w.write(cs, &out).map_err(RehashError::Write)?;
                            out.clear();
                        }
                    }
                    if n.is_none() {
                        break;
                    }
                    cur = n;
                }
            }
        }
        w.write(cs, &out).map_err(RehashError::Write)?;
        self.done(Kind::Refcounts, old, Some(w))
    }

    /// The new key of a root record: its six keys translated, the other
    /// fields copied.
    fn root(&mut self, old: ObjectKey) -> Result<ObjectKey, RehashError> {
        if let Some(new) = self.hit(Kind::Root, &old)? {
            return Ok(new);
        }
        let cs = self.cs;
        let err = |source| RehashError::Read { what: "root record", key: old, source };
        let mut r = FileReader::open(cs, to_id(&old)).map_err(err)?;
        if r.len() != root_rec::SIZE as u64 {
            return Err(RehashError::Corrupt(format!("root record {}: {} bytes, not {}", old.to_hex(), r.len(), root_rec::SIZE)));
        }
        let mut rec = r.read(cs, 0, root_rec::SIZE).map_err(err)?;
        if rec.len() != root_rec::SIZE {
            return Err(RehashError::Corrupt(format!("root record {}: short read", old.to_hex())));
        }
        drop(r);
        let at = |rec: &[u8], o: usize| ObjectKey::read(rec, o);
        let fields = [
            (root_rec::INODE_MAP, Some(Blob::InodeMap)),
            (root_rec::INODE_DATA, Some(Blob::ValueMap("inode node"))),
            (root_rec::DIR_DATA, Some(Blob::ValueMap("directory node"))),
            (root_rec::XATTR_DATA, Some(Blob::ValueMap("xattr node"))),
            (root_rec::MANIFEST_DATA, Some(Blob::ManifestMap)),
            (root_rec::REFCOUNTS, None),
        ];
        for (o, b) in fields {
            let key = at(&rec, o);
            let new = match b {
                Some(b) => self.blob(b, key)?,
                None => self.refcounts(key)?,
            };
            rec[o..o + KEY_BYTES].copy_from_slice(&new.0);
        }
        self.stats.values_read += 1;
        self.stats.bytes_read += root_rec::SIZE as u64;
        let w = if self.dry() {
            None
        } else {
            let mut w = FileWriter::new();
            w.write(cs, &rec).map_err(RehashError::Write)?;
            Some(w)
        };
        self.done(Kind::Root, old, w)
    }
}

/// Write every value of the current root and of each snapshot root of
/// `roots` under `opts.target`, and return the new roots (each snapshot
/// keeps its version, name and creation time) with the counts. In
/// [`Mode::Rewrite`], `cs` must be open with the target hasher and the
/// store profile; the caller syncs `cs` before it uses the new roots.
pub fn rehash_roots(cs: &ChunkStore, roots: &RootSet, opts: RehashOptions<'_>) -> Result<(RootSet, Stats), RehashError> {
    let map_dir = match opts.mode {
        Mode::Rewrite { map_dir } => {
            if cs.hasher().tag() != opts.target.tag() {
                return Err(RehashError::WrongHasher { store: cs.hasher().name(), target: opts.target.name() });
            }
            Some(map_dir)
        }
        Mode::DryRun => None,
    };
    let before = cs.stats();
    let mut w = Walker {
        cs,
        map_dir,
        map: StandIn::new(),
        stats: Stats { roots_total: 1 + roots.snapshots.len() as u64, ..Stats::default() },
        progress: opts.progress,
        every: opts.progress_every,
        last: Instant::now(),
        stop_after: opts.stop_after_values,
    };
    let mut new = Vec::with_capacity(1 + roots.snapshots.len());
    for root in std::iter::once(roots.current).chain(roots.snapshots.iter().map(|s| s.root)) {
        w.tick(true);
        new.push(w.root(root)?);
        w.stats.roots_done += 1;
        w.tick(true);
    }
    let after = cs.stats();
    w.stats.objects_written = after.objects.saturating_sub(before.objects);
    w.stats.stored_bytes_written = after.stored_bytes.saturating_sub(before.stored_bytes);
    w.tick(true);
    let snapshots = roots
        .snapshots
        .iter()
        .zip(&new[1..])
        .map(|(s, root)| SnapshotEntry { root: *root, ..s.clone() })
        .collect();
    Ok((RootSet { current: new[0], snapshots }, w.stats))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use chunk_store::file::file_objects;
    use chunk_store::hash::TestKeyedBlake3;
    use chunk_store::mem::MemIo;
    use chunk_store::{Blake3_256, Options};
    use slicefs_traits::metadata::{InodeMeta, MetadataStore};

    use crate::store::{content_keys, referenced_roots, DictMetadataStore};
    use crate::store_io::{get_value, put_value, to_id, NodeReader, StoreIo};

    const KEYED: &dyn ObjectHasher = &TestKeyedBlake3;
    const BLAKE3: &dyn ObjectHasher = &Blake3_256;

    fn open(mem: &MemIo, hasher: &'static dyn ObjectHasher) -> Arc<ChunkStore> {
        let opts = Options { hasher, ..Options::default() };
        Arc::new(ChunkStore::open_with(Arc::new(mem.clone()), opts).unwrap())
    }

    fn io(cs: &Arc<ChunkStore>) -> Arc<Mutex<StoreIo>> {
        Arc::new(Mutex::new(StoreIo::new("/unused", Arc::clone(cs))))
    }

    fn load(cs: &Arc<ChunkStore>, root: &ObjectKey) -> DictMetadataStore {
        DictMetadataStore::load_from_root(io(cs), root).unwrap()
    }

    fn noise(seed: u64, n: usize) -> Vec<u8> {
        let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    fn rewrite(target: &'static dyn ObjectHasher, dir: &Path) -> RehashOptions<'static> {
        RehashOptions {
            target,
            mode: Mode::Rewrite { map_dir: dir.join("rehash-tmp") },
            progress_every: Duration::from_secs(3600),
            progress: None,
            stop_after_values: None,
        }
    }

    fn one(root: ObjectKey) -> RootSet {
        RootSet { current: root, snapshots: vec![] }
    }

    /// A file `name` in `parent` with `bytes` as its content and a refcount.
    fn add_file(m: &DictMetadataStore, cs: &ChunkStore, parent: u64, name: &str, bytes: &[u8]) -> (u64, ObjectKey) {
        let ino = m.create_inode(&InodeMeta::new_file(0, 0, 0, 0o100644)).unwrap();
        m.link(parent, name, ino).unwrap();
        let key = put_value(cs, bytes).unwrap();
        m.set_manifest(ino, &[key]).unwrap();
        m.increment_refcount(&key);
        let mut meta = m.get_inode(ino).unwrap();
        meta.size = bytes.len() as u64;
        m.update_inode(&meta).unwrap();
        (ino, key)
    }

    /// A BLAKE3-256 store with every kind of node; returns the root.
    fn build(mem: &MemIo) -> ObjectKey {
        let cs = open(mem, BLAKE3);
        let m = DictMetadataStore::new(io(&cs));
        let r = m.root_ino();
        add_file(&m, &cs, r, "empty", &[]);
        let (ten, _) = add_file(&m, &cs, r, "ten", b"0123456789");
        add_file(&m, &cs, r, "random", &noise(1, 300 << 10));
        let mut sparse = noise(2, 2 << 20);
        sparse[512 << 10..(512 << 10) + (1 << 20)].fill(0);
        add_file(&m, &cs, r, "sparse", &sparse);
        let sub = m.create_directory(r, "sub", &InodeMeta::new_directory(0, 0, 0, 0o40755)).unwrap();
        let shared = noise(3, 20_000);
        add_file(&m, &cs, sub, "a", &shared);
        add_file(&m, &cs, sub, "b", &shared);
        m.set_xattr(ten, "user.note", b"hello").unwrap();
        m.commit().unwrap()
    }

    type Node = (InodeMeta, Vec<(String, Vec<u8>)>, String, u64);

    /// Each path with its inode, xattrs, a digest of its bytes and the
    /// refcount of its content.
    fn tree(m: &DictMetadataStore, cs: &ChunkStore) -> BTreeMap<String, Node> {
        let mut out = BTreeMap::new();
        let mut todo = vec![(String::new(), m.root_ino())];
        while let Some((path, ino)) = todo.pop() {
            let meta = m.get_inode(ino).unwrap();
            let mut xattrs: Vec<_> = m
                .list_xattrs(ino)
                .unwrap()
                .into_iter()
                .map(|n| {
                    let v = m.get_xattr(ino, &n).unwrap();
                    (n, v)
                })
                .collect();
            xattrs.sort();
            let (digest, count) = if meta.mode & 0o170_000 == 0o040_000 {
                for e in m.list_directory(ino).unwrap() {
                    if e.name != "." && e.name != ".." {
                        todo.push((format!("{path}/{}", e.name), e.ino));
                    }
                }
                (String::new(), 0)
            } else {
                let keys = m.get_manifest(ino).unwrap();
                let mut all = Vec::new();
                for k in &keys {
                    all.extend(get_value(cs, k).unwrap());
                }
                let d: String = Blake3_256.digest(&all).iter().map(|b| format!("{b:02x}")).collect();
                (d, keys.first().map_or(0, |k| m.get_refcount(k)))
            };
            out.insert(path, (meta, xattrs, digest, count));
        }
        out
    }

    /// Every object of every key of `root` has `tag`.
    fn assert_one_tag(cs: &Arc<ChunkStore>, root: &ObjectKey, tag: u8) {
        let mut nr = NodeReader(Arc::clone(cs));
        let mut keys = referenced_roots(&mut nr, root).unwrap();
        keys.extend(content_keys(&mut nr, root).unwrap());
        for k in &keys {
            for id in file_objects(cs, to_id(k)).unwrap() {
                assert_eq!(id.tag(), tag, "object {id:?} of {k:?}");
            }
        }
    }

    #[test]
    fn a_root_rehashes_to_the_same_tree() {
        let mem = MemIo::new();
        let root = build(&mem);
        let (old, lb, ic) = {
            let cs = open(&mem, BLAKE3);
            let m = load(&cs, &root);
            (tree(&m, &cs), m.logical_bytes(), m.inode_count())
        };
        let cs = open(&mem, KEYED);
        let dir = tempfile::tempdir().unwrap();
        let (new, stats) = rehash_roots(&cs, &one(root), rewrite(KEYED, dir.path())).unwrap();
        assert_ne!(new.current, root);
        assert_eq!(stats.roots_done, 1);
        let m = load(&cs, &new.current);
        assert_eq!(tree(&m, &cs), old);
        assert_eq!((m.logical_bytes(), m.inode_count()), (lb, ic));
        assert_one_tag(&cs, &new.current, KEYED.tag());
        let mut nr = NodeReader(Arc::clone(&cs));
        for k in content_keys(&mut nr, &new.current).unwrap() {
            let bytes = get_value(&cs, &k).unwrap();
            assert_eq!(put_value(&cs, &bytes).unwrap(), k, "the key of a fresh write");
        }
        // Canonical form: a commit of the loaded root writes every map again
        // and gives the same root, with no new object.
        let objects = cs.stats().objects;
        assert_eq!(load(&cs, &new.current).commit().unwrap(), new.current);
        assert_eq!(cs.stats().objects, objects);
    }

    #[test]
    fn two_old_keys_of_one_content_merge_their_counts() {
        let mem = MemIo::new();
        let (a, root1) = {
            let cs = open(&mem, BLAKE3);
            let m = DictMetadataStore::new(io(&cs));
            let (a, _) = add_file(&m, &cs, m.root_ino(), "a", b"same bytes");
            (a, m.commit().unwrap())
        };
        let (b, root2, k2) = {
            let cs = open(&mem, KEYED);
            let m = load(&cs, &root1);
            let (b, k2) = add_file(&m, &cs, m.root_ino(), "b", b"same bytes");
            (b, m.commit().unwrap(), k2)
        };
        assert_eq!(k2.tag(), KEYED.tag());
        let cs = open(&mem, BLAKE3);
        let dir = tempfile::tempdir().unwrap();
        let (new, _) = rehash_roots(&cs, &one(root2), rewrite(BLAKE3, dir.path())).unwrap();
        let m = load(&cs, &new.current);
        let ka = m.get_manifest(a).unwrap()[0];
        assert_eq!(m.get_manifest(b).unwrap()[0], ka);
        assert_eq!(ka.tag(), BLAKE3.tag());
        assert_eq!(m.get_refcount(&ka), 2, "the counts of both old keys");
        assert_eq!(m.get_refcount(&k2), 0);
    }

    #[test]
    fn a_stale_refcount_is_dropped_and_counted() {
        let mem = MemIo::new();
        let stale = ObjectKey::new(0x1e, [9; 32]);
        let root = {
            let cs = open(&mem, BLAKE3);
            let m = DictMetadataStore::new(io(&cs));
            add_file(&m, &cs, m.root_ino(), "f", b"data");
            m.increment_refcount(&stale);
            m.commit().unwrap()
        };
        let cs = open(&mem, KEYED);
        let dir = tempfile::tempdir().unwrap();
        let (new, stats) = rehash_roots(&cs, &one(root), rewrite(KEYED, dir.path())).unwrap();
        assert_eq!(stats.stale_refcounts, 1);
        let m = load(&cs, &new.current);
        assert_eq!(m.get_refcount(&stale), 0);
        let k = m.get_manifest(m.lookup(m.root_ino(), "f").unwrap()).unwrap()[0];
        assert_eq!(m.get_refcount(&k), 1);
    }
}
