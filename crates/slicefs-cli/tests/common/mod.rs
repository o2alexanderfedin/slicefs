//! Helpers shared by the test binaries that crash and replay a store: a new
//! file system with a write log, what a mount after a crash does (the last
//! root, then the replay of the log), and a dump of the tree to compare the
//! two. Each binary uses a subset.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use metadata::segment::load_store_from_segments;
use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use metadata::wal::{create_wal, WalConfig};
use slicefs_cli::filesystem::SliceFsFilesystem;
use slicefs_cli::oplog;
use slicefs_traits::metadata::{InodeMeta, MetadataStore};

const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;

/// Bytes `dump` reads at a time: a file is never read whole.
const DUMP_PIECE: u64 = 64 << 10;

/// A new store at `store` with a write log in mode `config`, committed once.
pub fn new_fs_with(store: &Path, config: WalConfig) -> SliceFsFilesystem {
    new_fs_with_options(store, config, chunk_store::Options::default())
}

/// `new_fs_with` with chunk store options (for example a short
/// `store_wait`, which makes a slow background sync a busy store).
pub fn new_fs_with_options(store: &Path, config: WalConfig, opts: chunk_store::Options) -> SliceFsFilesystem {
    std::fs::create_dir_all(store.join("segments")).unwrap();
    let wal = create_wal(config, store, 1).unwrap();
    let cs = chunk_store::ChunkStore::open(store, opts).unwrap();
    let io = Arc::new(Mutex::new(StoreIo::new(store, Arc::new(cs))));
    let mut meta = DictMetadataStore::new(io.clone());
    meta.set_wal(wal);
    let fs = SliceFsFilesystem::new(meta, io, Some(store.to_path_buf()));
    fs.meta().commit().unwrap();
    fs
}

/// What a mount after a crash does: the last root, then replay the log.
pub fn reload_and_replay(store: &Path) -> SliceFsFilesystem {
    reload_and_replay_with(store, |_| {})
}

/// `reload_and_replay` with `setup` run on the new file system before replay.
pub fn reload_and_replay_with(store: &Path, setup: impl FnOnce(&SliceFsFilesystem)) -> SliceFsFilesystem {
    let (root, snaps) = load_store_from_segments(&store.join("segments")).unwrap();
    let io = Arc::new(Mutex::new(StoreIo::open(store).unwrap()));
    let mut meta = DictMetadataStore::load_from_root(io.clone(), &root.unwrap()).unwrap();
    meta.set_snapshots(snaps);
    // A new segment after the last one, as a mount makes.
    let next = segment_paths(store).last()
        .and_then(|p| p.file_name()?.to_str()?.strip_prefix("segment-")?.strip_suffix(".seg")?.parse::<u64>().ok())
        .map_or(1, |n| n + 1);
    let wal = create_wal(WalConfig::PerOp, store, next).unwrap();
    meta.set_wal(wal);
    let fs = SliceFsFilesystem::new(meta, io, Some(store.to_path_buf()));
    setup(&fs);
    // The mount's replay: it streams the log (#106).
    oplog::replay_log(&fs, &store.join("segments")).unwrap();
    fs
}

/// The segment files of `store`, by id.
pub fn segment_paths(store: &Path) -> Vec<PathBuf> {
    let mut v: Vec<(u64, PathBuf)> = std::fs::read_dir(store.join("segments")).unwrap()
        .filter_map(|e| {
            let p = e.unwrap().path();
            let n: u64 = p.file_name()?.to_str()?.strip_prefix("segment-")?.strip_suffix(".seg")?.parse().ok()?;
            Some((n, p))
        })
        .collect();
    v.sort();
    v.into_iter().map(|(_, p)| p).collect()
}

/// The records of the write log of `store` after the last root (the log is
/// synced first).
pub fn log_len(fs: &SliceFsFilesystem, store: &Path) -> usize {
    fs.meta().flush_wal().unwrap();
    let (_, _, log) = metadata::segment::scan_store_with_log(&store.join("segments")).unwrap();
    log.records().count()
}

/// Path -> description of every entry under the root.
pub fn dump(fs: &SliceFsFilesystem) -> BTreeMap<String, String> {
    dump_by(fs, |ino| fs.meta().get_inode(ino).unwrap())
}

/// `dump` as stat sees it: a file with an open write handle shows the size
/// and times of that handle (its overlay is not in the inode until release).
pub fn dump_open(fs: &SliceFsFilesystem) -> BTreeMap<String, String> {
    dump_by(fs, |ino| fs.test_getattr(ino).unwrap())
}

pub fn dump_by(fs: &SliceFsFilesystem, inode: impl Fn(u64) -> InodeMeta) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut todo = vec![(1u64, String::new())];
    while let Some((dir, path)) = todo.pop() {
        for e in fs.meta().list_directory(dir).unwrap() {
            if e.name == "." || e.name == ".." {
                continue;
            }
            let p = format!("{path}/{}", e.name);
            let m = inode(e.ino);
            let mut xattrs: Vec<(String, Vec<u8>)> = fs.meta().list_xattrs(e.ino).unwrap_or_default()
                .into_iter().map(|n| { let v = fs.meta().get_xattr(e.ino, &n).unwrap(); (n, v) }).collect();
            xattrs.sort();
            let body = match m.mode & 0o170_000 {
                S_IFDIR => { todo.push((e.ino, p.clone())); String::new() }
                S_IFREG => format!("content={:x?}", content_hash(fs, e.ino, m.size)),
                0o120_000 => format!("target={}", fs.simulate_readlink(e.ino).unwrap()),
                _ => String::new(),
            };
            out.insert(p, format!(
                "ino={} mode={:o} uid={} gid={} size={} nlinks={} mtime={}.{} ctime={}.{} atime={}.{} xattrs={:?} {body}",
                e.ino, m.mode, m.uid, m.gid, m.size, m.nlinks, m.mtime_sec, m.mtime_nsec, m.ctime_sec, m.ctime_nsec,
                m.atime_sec, m.atime_nsec, xattrs
            ));
        }
    }
    out
}

/// A short content fingerprint (FNV-1a) of the file `ino`, read in pieces;
/// a read past `size` that still gives bytes changes it too.
fn content_hash(fs: &SliceFsFilesystem, ino: u64, size: u64) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let mut off = 0u64;
    loop {
        let piece = fs.test_read(ino, off, DUMP_PIECE as u32).unwrap();
        if piece.is_empty() {
            return h;
        }
        h = blake_from(h, &piece);
        off += piece.len() as u64;
        if off > size {
            return h;
        }
    }
}

fn blake_from(h: u64, data: &[u8]) -> u64 {
    data.iter().fold(h, |h, &b| (h ^ b as u64).wrapping_mul(0x1_0000_0001_b3))
}

/// The keys where two dumps differ, with both values (at most `n`).
pub fn first_differences(a: &BTreeMap<String, String>, b: &BTreeMap<String, String>, n: usize) -> Vec<String> {
    let keys: std::collections::BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    keys.into_iter()
        .filter(|k| a.get(*k) != b.get(*k))
        .take(n)
        .map(|k| format!("{k}: live={:?} replayed={:?}", a.get(k), b.get(k)))
        .collect()
}
