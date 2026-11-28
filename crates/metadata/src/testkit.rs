//! Test-only helpers (hidden from the docs): a bulk loader that makes a
//! committed store of many files without one product call per file.
//!
//! [`bulk_store`] writes the five map trees with `prolly::Builder` (each
//! tree from one stream of entries in key order) and then the root record.
//! Every key and value goes through the encoders of the store (`TreeKey`,
//! `TreeValue`, `intern_manifest`, the `root_rec` offsets) and the tree
//! config of the `StoreIo` (with the store salt), so it writes the store
//! format and has no copy of it. A change of the format changes this file
//! in the same commit; the tests load each bulk store with
//! `DictMetadataStore::load_from_root` and read it back.
//!
//! The layout of a store of `files` files and `dirs` directories:
//! - inode 1 is the root directory; directory `j` (0-based) is inode
//!   `2 + j`, named [`dir_name`] in the root; file `i` is inode
//!   `dirs + 2 + i`, named [`file_name`] in directory `2 + i % dirs` (in
//!   the root if `dirs` is 0). Every directory has `.` and `..`.
//! - file `i` has the manifest of group `i / FILES_PER_MANIFEST`: one
//!   manifest object per group that lists one real content object
//!   ([`content`]); the file size is the length of that content.
//! - every `xattr_every`-th file (`i % xattr_every == 0`) has the inline
//!   xattr [`XATTR_NAME`] with the value [`xattr_value`].
//! - `refcounts_per_file` refcount entries per file, count 1, under the
//!   keys [`refcount_key`] (not real objects: the GC does not read them).

use std::sync::{Arc, Mutex};

use chunk_store::lock_stats::{self, Site};
use chunk_store::ChunkStore;
use slicefs_traits::key::{ObjectKey, DIGEST_BYTES, KEY_BYTES};
use slicefs_traits::metadata::{InodeMeta, MetaError};

use crate::dirent::{entry_key, kind_of, DirValue, KIND_DIR};
use crate::manifest::intern_manifest;
use crate::pmap::{tree_err, TreeKey, TreeValue};
use crate::store::root_rec;
use crate::store_io::{put_value, to_key, NodeWriter, StoreIo};
use crate::xattr::{XattrKey, XattrValue};

/// Files that share one manifest object (and one content object).
pub const FILES_PER_MANIFEST: u64 = 1_000;

/// The name of the xattr of a bulk file.
pub const XATTR_NAME: &str = "user.bulk";

/// Mode of a bulk directory and of the root.
const DIR_MODE: u32 = 0o040_755;
/// Mode of a bulk file.
const FILE_MODE: u32 = 0o100_644;

/// What [`bulk_store`] makes.
#[derive(Clone, Copy, Debug)]
pub struct BulkSpec {
    /// Regular files.
    pub files: u64,
    /// Directories below the root (the files are spread over them).
    pub dirs: u64,
    /// One file in this many has an xattr (0: none).
    pub xattr_every: u64,
    /// Refcount entries per file.
    pub refcounts_per_file: u64,
}

impl BulkSpec {
    /// The inode of directory `j`.
    pub fn dir_ino(&self, j: u64) -> u64 {
        2 + j
    }

    /// The inode of file `i`.
    pub fn file_ino(&self, i: u64) -> u64 {
        self.dirs + 2 + i
    }

    /// The directory of file `i`.
    pub fn parent_of(&self, i: u64) -> u64 {
        if self.dirs == 0 { 1 } else { self.dir_ino(i % self.dirs) }
    }

    /// True if file `i` has the xattr [`XATTR_NAME`].
    pub fn has_xattr(&self, i: u64) -> bool {
        self.xattr_every != 0 && i.is_multiple_of(self.xattr_every)
    }

    /// Inodes of the store: the root, the directories and the files.
    pub fn inode_count(&self) -> u64 {
        1 + self.dirs + self.files
    }

    /// The `next_ino` of the root record (the first free inode number).
    pub fn next_ino(&self) -> u64 {
        self.dirs + self.files + 2
    }

    /// The inode record of `ino` (uid and gid 0, times 0).
    pub fn inode_meta(&self, ino: u64) -> InodeMeta {
        if ino == 1 {
            let mut m = InodeMeta::new_directory(1, 0, 0, DIR_MODE);
            m.nlinks = 2 + u32::try_from(self.dirs).expect("dirs fit u32");
            m
        } else if ino < self.file_ino(0) {
            InodeMeta::new_directory(ino, 0, 0, DIR_MODE)
        } else {
            let mut m = InodeMeta::new_file(ino, 0, 0, FILE_MODE);
            m.size = content(group_of(ino - self.file_ino(0))).len() as u64;
            m
        }
    }
}

/// The name of directory `j` (fixed width: name order is number order).
pub fn dir_name(j: u64) -> String {
    format!("d{j:010}")
}

/// The name of file `i` (fixed width: name order is number order).
pub fn file_name(i: u64) -> String {
    format!("f{i:010}")
}

/// The manifest group of file `i`.
pub fn group_of(i: u64) -> u64 {
    i / FILES_PER_MANIFEST
}

/// The bytes of the content object of manifest group `g`.
pub fn content(g: u64) -> Vec<u8> {
    format!("bulk content of group {g}").into_bytes()
}

/// The xattr value of file `i` (short: stored inline).
pub fn xattr_value(i: u64) -> Vec<u8> {
    format!("bulk xattr {i}").into_bytes()
}

/// The refcount key `n` (0-based over the store): tag `tag`, then `n` big
/// endian, so the keys come in tree order.
pub fn refcount_key(tag: u8, n: u64) -> ObjectKey {
    let mut d = [0u8; DIGEST_BYTES];
    d[..8].copy_from_slice(&n.to_be_bytes());
    ObjectKey::new(tag, d)
}

/// The I/O error of a chunk-store error.
fn io_err(e: impl std::fmt::Display) -> MetaError {
    MetaError::Io(std::io::Error::other(e.to_string()))
}

/// One tree of the store, built from `entries` (in key order).
fn build(
    cs: &ChunkStore,
    cfg: prolly::Config,
    name: &str,
    entries: impl Iterator<Item = Result<(Vec<u8>, Vec<u8>), MetaError>>,
) -> Result<ObjectKey, MetaError> {
    let mut b = prolly::Builder::new(cs, cfg);
    for e in entries {
        let (k, v) = e?;
        b.push(&k, &v).map_err(|e| tree_err(name, e))?;
    }
    b.finish().map(|(root, _)| to_key(root)).map_err(|e| tree_err(name, e))
}

/// Make the store of `spec` in the chunk store of `io` and give the key
/// of its root record (for `DictMetadataStore::load_from_root`). The chunk
/// store is synced at the end. RAM: one node per tree level and one
/// directory listing at a time, not the store.
pub fn bulk_store(io: &Arc<Mutex<StoreIo>>, spec: &BulkSpec) -> Result<ObjectKey, MetaError> {
    let (cs, cfg) = {
        let g = lock_stats::lock(io, Site::Io);
        (Arc::clone(g.store()), g.tree_config())
    };
    let ok = |k: Vec<u8>, v: Vec<u8>| Ok::<_, MetaError>((k, v));

    // inode_data: the root, the directories, the files (inode order).
    let inode_data = build(&cs, cfg, "inode_data", (1..spec.next_ino()).map(|ino| {
        ok(TreeKey::encode(&ino), spec.inode_meta(ino).encode())
    }))?;

    // dir_data: per directory in inode order, `.`, `..`, then the names.
    let dot = |dir: u64, parent: u64| {
        [(".", dir), ("..", parent)].map(|(n, ino)| (entry_key(dir, n).encode(), DirValue { ino, kind: KIND_DIR }.encode()))
    };
    let file_kind = kind_of(FILE_MODE);
    let file_entry = move |i: u64| {
        (entry_key(spec.parent_of(i), &file_name(i)).encode(), DirValue { ino: spec.file_ino(i), kind: file_kind }.encode())
    };
    let root_entries = dot(1, 1).into_iter()
        .chain((0..spec.dirs).map(|j| (entry_key(1, &dir_name(j)).encode(), DirValue { ino: spec.dir_ino(j), kind: KIND_DIR }.encode())))
        .chain((0..if spec.dirs == 0 { spec.files } else { 0 }).map(file_entry));
    let dir_entries = (0..spec.dirs).flat_map(move |j| {
        dot(spec.dir_ino(j), 1).into_iter().chain((j..spec.files).step_by(spec.dirs as usize).map(file_entry))
    });
    let dir_data = build(&cs, cfg, "dir_data", root_entries.chain(dir_entries).map(|(k, v)| ok(k, v)))?;

    // manifest_data: one manifest and one content object per group.
    let mut w = NodeWriter::new(Arc::clone(&cs));
    let mut group: Option<(u64, ObjectKey)> = None;
    let mut content_tag = None;
    let manifest_data = build(&cs, cfg, "manifest_data", (0..spec.files).map(|i| {
        let g = group_of(i);
        let manifest = match group {
            Some((at, m)) if at == g => m,
            _ => {
                let c = put_value(&cs, &content(g)).map_err(io_err)?;
                content_tag = Some(c.tag());
                let m = intern_manifest(&mut w, &[c]);
                group = Some((g, m));
                m
            }
        };
        ok(TreeKey::encode(&spec.file_ino(i)), TreeValue::encode(&manifest))
    }))?;
    w.result().map_err(MetaError::Io)?;

    // xattr_data: one inline entry per xattr file, in inode order.
    let xattr_data = build(&cs, cfg, "xattr_data", (0..spec.files).filter(|i| spec.has_xattr(*i)).map(|i| {
        ok(XattrKey::new(spec.file_ino(i), XATTR_NAME).encode(), XattrValue::Inline(xattr_value(i)).encode())
    }))?;

    // refcounts: the keys of the real content tag (any tag if no file).
    let tag = content_tag.unwrap_or(1);
    let refcounts = build(&cs, cfg, "refcounts", (0..spec.files * spec.refcounts_per_file).map(|n| {
        ok(TreeKey::encode(&refcount_key(tag, n)), TreeValue::encode(&1u64))
    }))?;

    // The root record at the `root_rec` offsets.
    let logical: u64 = (0..spec.files).map(|i| content(group_of(i)).len() as u64).sum();
    let mut rec = vec![0u8; root_rec::SIZE];
    let mut put = |at: usize, b: &[u8]| rec[at..at + b.len()].copy_from_slice(b);
    put(root_rec::ROOT_DIR_INO, &1u64.to_le_bytes());
    put(root_rec::NEXT_INO, &spec.next_ino().to_le_bytes());
    for (at, k) in [
        (root_rec::INODE_DATA, &inode_data),
        (root_rec::DIR_DATA, &dir_data),
        (root_rec::MANIFEST_DATA, &manifest_data),
        (root_rec::XATTR_DATA, &xattr_data),
        (root_rec::REFCOUNTS, &refcounts),
    ] {
        debug_assert_eq!(k.0.len(), KEY_BYTES);
        put(at, &k.0);
    }
    put(root_rec::LOGICAL_BYTES, &logical.to_le_bytes());
    put(root_rec::INODE_COUNT, &spec.inode_count().to_le_bytes());
    let root = put_value(&cs, &rec).map_err(io_err)?;
    cs.sync().map_err(io_err)?;
    Ok(root)
}
