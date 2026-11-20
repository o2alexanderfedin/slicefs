//! `seed` subcommand — imports a directory tree into a SliceFS CAS store.
//!
//! Walks the source directory recursively, stores file content in the chunk
//! store (FastCDC + BLAKE3 in `packs/`),
//! builds inode and directory records in `DictMetadataStore`, then writes a
//! `RootUpdate` to `segments/segment-000001.seg` for crash-safe recovery.
//!
//! Metadata nodes and file content go to `packs/`; no `dictionary.bin` or `root.bin` files are
//! written.

use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::content::{self, Content};
use metadata::segment::{SegmentWriter, SegmentEntry};
use metadata::store::DictMetadataStore;
use slicefs_traits::key::ObjectKey;
use slicefs_traits::metadata::{InodeMeta, InodeId, MetadataStore};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

// POSIX type bits
const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;

/// Import a directory tree rooted at `source_dir` into a CAS store at `store_path`.
///
/// Creates:
/// - `<store_path>/packs/` — metadata nodes and file content (chunk store)
/// - `<store_path>/index/` — the on-disk chunk index
/// - `<store_path>/segments/segment-000001.seg` — root update segment
///
/// `hash` is `--hash`: the write hash of the new objects (`None`: the
/// store's recorded hash, blake3-256 for a new store).
pub fn run_seed(store_path: &Path, source_dir: &Path, hash: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(store_path)?;
    content::select_hash(store_path, hash)?;

    let io = Arc::new(Mutex::new(content::store_io(store_path)?));
    let meta_store = DictMetadataStore::new(io.clone());
    let cs = Arc::clone(io.lock().unwrap().store());
    let sync = Arc::clone(&cs);
    meta_store.set_pre_root_sync(Arc::new(move || sync.sync().map_err(std::io::Error::other)));

    // Update root inode (ino=1) permissions from source directory.
    // DictMetadataStore::new() creates it with uid=0/gid=0/0o755 by default,
    // which blocks non-root writes when DefaultPermissions is used.
    let source_meta = std::fs::metadata(source_dir)?;
    let root_inode_meta = dir_inode_meta(&source_meta);
    let mut root_inode = meta_store.get_inode(1)?;
    root_inode.mode = root_inode_meta.mode;
    root_inode.uid = root_inode_meta.uid;
    root_inode.gid = root_inode_meta.gid;
    meta_store.update_inode(&root_inode)?;

    // Walk source_dir: root maps to inode 1 (the pre-created root dir).
    walk_dir(source_dir, 1, &meta_store, &cs)?;

    // Commit the metadata nodes into the chunk store; get the root key.
    let root = meta_store.commit()?;

    // Write root digest to segments/segment-000001.seg as a RootUpdate entry.
    let segs_dir = store_path.join("segments");
    std::fs::create_dir_all(&segs_dir)?;
    let seg_path = segs_dir.join("segment-000001.seg");
    let mut writer = SegmentWriter::new(&seg_path, 1)?;
    writer.write_entry(&SegmentEntry::RootUpdate { root })?;
    writer.close()?;
    cs.close()?;

    Ok(())
}

/// Recursively walk `dir_path`, creating inodes/dirs/manifests under `parent_ino`.
fn walk_dir(
    dir_path: &Path,
    parent_ino: InodeId,
    meta_store: &DictMetadataStore,
    cs: &Content,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir_path)? {
        let entry = entry?;
        entries.push(entry);
    }
    // Sort for deterministic output.
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let path = entry.path();
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        let fs_meta = std::fs::metadata(&path)?;

        if fs_meta.is_dir() {
            let dir_meta = dir_inode_meta(&fs_meta);
            let ino = meta_store.create_directory(parent_ino, &name, &dir_meta)?;
            walk_dir(&path, ino, meta_store, cs)?;
        } else if fs_meta.is_file() {
            let (ino, content_digest) = seed_file(&path, &fs_meta, meta_store, cs)?;
            meta_store.link(parent_ino, &name, ino)?;
            meta_store.set_manifest(ino, &[content_digest])?;
        }
        // Symlinks and other special files are skipped.
    }
    Ok(())
}

/// Create an inode for a regular file and write its content into the chunk store.
/// Returns (ino, content_digest).
fn seed_file(
    path: &Path,
    fs_meta: &std::fs::Metadata,
    meta_store: &DictMetadataStore,
    cs: &Content,
) -> Result<(InodeId, ObjectKey), Box<dyn std::error::Error>> {
    // Stream the content into the chunk store (FastCDC chunks): the memory
    // does not depend on the file size.
    let mut file = std::fs::File::open(path)?;
    let (content_digest, size) = content::put_reader(cs, &mut file)?;

    let inode_meta = file_inode_meta(fs_meta, size);
    let ino = meta_store.create_inode(&inode_meta)?;

    Ok((ino, content_digest))
}

/// Build an `InodeMeta` for a regular file.
fn file_inode_meta(fs_meta: &std::fs::Metadata, size: u64) -> InodeMeta {
    #[cfg(unix)]
    let (mode, uid, gid, mtime_sec, mtime_nsec) = {
        use std::os::unix::fs::MetadataExt;
        let m = fs_meta.permissions().mode();
        let mode = S_IFREG | (m & 0o7777);
        let uid = fs_meta.uid();
        let gid = fs_meta.gid();
        let mtime_sec = fs_meta.mtime();
        let mtime_nsec = fs_meta.mtime_nsec() as u32;
        (mode, uid, gid, mtime_sec, mtime_nsec)
    };
    #[cfg(not(unix))]
    let (mode, uid, gid, mtime_sec, mtime_nsec) = (S_IFREG | 0o644, 0u32, 0u32, 0i64, 0u32);

    let mut meta = InodeMeta::new_file(0, uid, gid, mode);
    meta.size = size;
    meta.mtime_sec = mtime_sec;
    meta.mtime_nsec = mtime_nsec;
    meta
}

/// Build an `InodeMeta` for a directory.
fn dir_inode_meta(fs_meta: &std::fs::Metadata) -> InodeMeta {
    #[cfg(unix)]
    let (mode, uid, gid) = {
        use std::os::unix::fs::MetadataExt;
        let m = fs_meta.permissions().mode();
        let mode = S_IFDIR | (m & 0o7777);
        (mode, fs_meta.uid(), fs_meta.gid())
    };
    #[cfg(not(unix))]
    let (mode, uid, gid) = (S_IFDIR | 0o755, 0u32, 0u32);

    InodeMeta::new_directory(0, uid, gid, mode)
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use metadata::segment::load_store_from_segments;
    use metadata::store::DictMetadataStore;
    use metadata::store_io::StoreIo;
    use slicefs_traits::metadata::MetadataStore;
    use tempfile::TempDir;

    fn make_tempdir() -> TempDir {
        tempfile::tempdir().expect("failed to create tempdir")
    }

    /// Seed an empty directory — store/segments/ and store/packs/ are created (no dictionary.bin).
    #[test]
    fn test_seed_empty_directory() {
        let store_dir = make_tempdir();
        let source_dir = make_tempdir();

        run_seed(store_dir.path(), source_dir.path(), None).expect("seed failed");

        let segs_dir = store_dir.path().join("segments");
        assert!(segs_dir.is_dir(), "segments/ must exist after seed");
        assert!(store_dir.path().join("packs").is_dir(), "packs/ must exist after seed (metadata nodes)");
        assert!(!store_dir.path().join("vt0").exists(), "nothing is stored in vt0/ any more");

        // dictionary.bin must NOT exist
        assert!(
            !store_dir.path().join("dictionary.bin").exists(),
            "dictionary.bin must NOT be written by new seed"
        );
    }

    /// Seed creates a segment-000001.seg with a RootUpdate entry.
    #[test]
    fn test_seed_creates_segment_with_root() {
        let store_dir = make_tempdir();
        let source_dir = make_tempdir();

        run_seed(store_dir.path(), source_dir.path(), None).expect("seed failed");

        let segs_dir = store_dir.path().join("segments");
        let seg_file = segs_dir.join("segment-000001.seg");
        assert!(seg_file.exists(), "segment-000001.seg must exist after seed");

        // load_store_from_segments should find a root
        let (root_opt, _snapshots) = load_store_from_segments(&segs_dir).expect("load failed");
        assert!(root_opt.is_some(), "segment must contain a RootUpdate");
    }

    /// Seed a directory with one file — content is retrievable via get_node.
    #[test]
    fn test_seed_single_file_content_round_trip() {
        let store_dir = make_tempdir();
        let source_dir = make_tempdir();

        let content = b"hello from SliceFS seed test";
        std::fs::write(source_dir.path().join("hello.txt"), content).unwrap();

        run_seed(store_dir.path(), source_dir.path(), None).expect("seed failed");

        // Load from segments to get root
        let segs_dir = store_dir.path().join("segments");
        let (root_opt, _snapshots) = load_store_from_segments(&segs_dir).expect("load failed");
        let root = root_opt.expect("no root found");

        // Reconstruct metadata store
        let io = Arc::new(Mutex::new(StoreIo::open(store_dir.path()).unwrap()));
        let reloaded = DictMetadataStore::load_from_root(io.clone(), &root).expect("load_from_root failed");

        // Find the file inode via lookup.
        let file_ino = reloaded.lookup(1, "hello.txt").expect("lookup failed");

        // Get the manifest (content digest).
        let manifest = reloaded.get_manifest(file_ino).expect("get_manifest failed");
        assert_eq!(manifest.len(), 1, "expected exactly one block in manifest");

        // Read the content back from the chunk store.
        let cs = content::open(store_dir.path()).unwrap();
        let read_back = content::get_all(&cs, &manifest[0]).expect("content read failed");
        assert_eq!(read_back, content.as_slice(), "content mismatch after round-trip");
    }

    /// Seed a directory with nested subdirectories — all dirs appear after load_from_root.
    #[test]
    fn test_seed_nested_directories() {
        let store_dir = make_tempdir();
        let source_dir = make_tempdir();

        // Build: source/
        //   a/
        //     b/
        //       deep.txt
        //   top.txt
        std::fs::create_dir_all(source_dir.path().join("a").join("b")).unwrap();
        std::fs::write(source_dir.path().join("top.txt"), b"top level").unwrap();
        std::fs::write(source_dir.path().join("a").join("b").join("deep.txt"), b"deep file").unwrap();

        run_seed(store_dir.path(), source_dir.path(), None).expect("seed failed");

        // Reload from segments.
        let segs_dir = store_dir.path().join("segments");
        let (root_opt, _) = load_store_from_segments(&segs_dir).expect("load failed");
        let root = root_opt.expect("no root found");

        let io = Arc::new(Mutex::new(StoreIo::open(store_dir.path()).unwrap()));
        let reloaded = DictMetadataStore::load_from_root(io, &root).unwrap();

        // top.txt in root
        assert!(reloaded.lookup(1, "top.txt").is_ok(), "top.txt not found in root");

        // dir "a" in root
        let a_ino = reloaded.lookup(1, "a").expect("dir a not found");

        // dir "b" in a
        let b_ino = reloaded.lookup(a_ino, "b").expect("dir b not found");

        // deep.txt in b
        assert!(reloaded.lookup(b_ino, "deep.txt").is_ok(), "deep.txt not found in b");
    }

    /// A file larger than the read buffer is stored in parts and reads back
    /// byte-exact, with the correct size.
    #[test]
    fn test_seed_file_larger_than_read_buffer() {
        let store_dir = make_tempdir();
        let source_dir = make_tempdir();

        let data = content::tests::random_bytes(2 * content::READ_BUF + 4321, 9);
        std::fs::write(source_dir.path().join("big.bin"), &data).unwrap();

        run_seed(store_dir.path(), source_dir.path(), None).expect("seed failed");

        let segs_dir = store_dir.path().join("segments");
        let (root_opt, _) = load_store_from_segments(&segs_dir).expect("load failed");
        let root = root_opt.expect("no root");
        let io = Arc::new(Mutex::new(StoreIo::open(store_dir.path()).unwrap()));
        let reloaded = DictMetadataStore::load_from_root(io, &root).unwrap();

        let ino = reloaded.lookup(1, "big.bin").unwrap();
        assert_eq!(reloaded.get_inode(ino).unwrap().size, data.len() as u64);
        let manifest = reloaded.get_manifest(ino).unwrap();
        let cs = content::open(store_dir.path()).unwrap();
        assert_eq!(manifest[0], content::put_all(&cs, &data).unwrap(), "key differs from put_all");
        assert!(content::get_all(&cs, &manifest[0]).unwrap() == data, "content mismatch");
    }

    /// Seed preserves file metadata — size matches original file size in InodeMeta.
    #[test]
    fn test_seed_preserves_file_size() {
        let store_dir = make_tempdir();
        let source_dir = make_tempdir();

        let content: Vec<u8> = (0u8..200u8).collect();
        std::fs::write(source_dir.path().join("sized.bin"), &content).unwrap();

        run_seed(store_dir.path(), source_dir.path(), None).expect("seed failed");

        let segs_dir = store_dir.path().join("segments");
        let (root_opt, _) = load_store_from_segments(&segs_dir).expect("load failed");
        let root = root_opt.expect("no root");

        let io = Arc::new(Mutex::new(StoreIo::open(store_dir.path()).unwrap()));
        let reloaded = DictMetadataStore::load_from_root(io, &root).unwrap();

        let ino = reloaded.lookup(1, "sized.bin").unwrap();
        let meta = reloaded.get_inode(ino).unwrap();
        assert_eq!(meta.size, 200, "file size mismatch: expected 200, got {}", meta.size);
    }
}
