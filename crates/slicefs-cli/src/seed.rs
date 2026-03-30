//! `seed` subcommand — imports a directory tree into a SliceFS CAS store.
//!
//! Walks the source directory recursively, chunks file content via `State` CDC,
//! builds inode and directory records in `DictMetadataStore`, then writes a
//! `RootUpdate` to `segments/segment-000001.seg` for crash-safe recovery.
//!
//! File content is stored in `vt0/` batch files via `FileStorageAdd`; no
//! `dictionary.bin` or `root.bin` files are written.

use std::path::Path;
use std::sync::{Arc, Mutex};

use blockset::{State, Tree, FileStorageAdd};
use metadata::segment::{SegmentWriter, SegmentEntry};
use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use slicefs_traits::digest::Digest224;
use slicefs_traits::metadata::{InodeMeta, InodeId, MetadataStore};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

// POSIX type bits
const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;

/// Import a directory tree rooted at `source_dir` into a CAS store at `store_path`.
///
/// Creates:
/// - `<store_path>/vt0/` — FileStorage batch files (content blocks)
/// - `<store_path>/segments/segment-000001.seg` — root update segment
pub fn run_seed(store_path: &Path, source_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(store_path)?;

    let io = Arc::new(Mutex::new(StoreIo::new(store_path)));
    let meta_store = DictMetadataStore::new(io.clone());

    // Walk source_dir: root maps to inode 1 (the pre-created root dir).
    walk_dir(source_dir, 1, &meta_store, &io)?;

    // Commit metadata state into FileStorage, get root digest.
    let root = meta_store.commit()?;

    // Write root digest to segments/segment-000001.seg as a RootUpdate entry.
    let segs_dir = store_path.join("segments");
    std::fs::create_dir_all(&segs_dir)?;
    let seg_path = segs_dir.join("segment-000001.seg");
    let mut writer = SegmentWriter::new(&seg_path, 1)?;
    writer.write_entry(&SegmentEntry::RootUpdate { root })?;
    writer.close()?;

    Ok(())
}

/// Recursively walk `dir_path`, creating inodes/dirs/manifests under `parent_ino`.
fn walk_dir(
    dir_path: &Path,
    parent_ino: InodeId,
    meta_store: &DictMetadataStore,
    io: &Arc<Mutex<StoreIo>>,
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
            walk_dir(&path, ino, meta_store, io)?;
        } else if fs_meta.is_file() {
            let (ino, content_digest) = seed_file(&path, &fs_meta, meta_store, io)?;
            meta_store.link(parent_ino, &name, ino)?;
            meta_store.set_manifest(ino, &[content_digest])?;
        }
        // Symlinks and other special files are skipped.
    }
    Ok(())
}

/// Create an inode for a regular file and push its content into FileStorage.
/// Returns (ino, content_digest).
fn seed_file(
    path: &Path,
    fs_meta: &std::fs::Metadata,
    meta_store: &DictMetadataStore,
    io: &Arc<Mutex<StoreIo>>,
) -> Result<(InodeId, Digest224), Box<dyn std::error::Error>> {
    let bytes = std::fs::read(path)?;
    let size = bytes.len() as u64;

    // Push content bytes into FileStorage via CDC.
    let content_digest: Digest224 = {
        let mut io_guard = io.lock().unwrap();
        let mut fsa = FileStorageAdd::new(&mut *io_guard);
        let digest = State::push_all(&mut fsa, &bytes);
        drop(fsa);
        digest
    };

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
    use blockset::file_storage_get;
    use metadata::segment::load_store_from_segments;
    use metadata::store::DictMetadataStore;
    use metadata::store_io::StoreIo;
    use slicefs_traits::metadata::MetadataStore;
    use tempfile::TempDir;

    fn make_tempdir() -> TempDir {
        tempfile::tempdir().expect("failed to create tempdir")
    }

    /// Seed an empty directory — store/segments/ and store/vt0/ are created (no dictionary.bin).
    #[test]
    fn test_seed_empty_directory() {
        let store_dir = make_tempdir();
        let source_dir = make_tempdir();

        run_seed(store_dir.path(), source_dir.path()).expect("seed failed");

        let segs_dir = store_dir.path().join("segments");
        let vt0_dir = store_dir.path().join("vt0");

        assert!(segs_dir.is_dir(), "segments/ must exist after seed");
        assert!(vt0_dir.is_dir(), "vt0/ must exist after seed (FileStorage batch files)");

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

        run_seed(store_dir.path(), source_dir.path()).expect("seed failed");

        let segs_dir = store_dir.path().join("segments");
        let seg_file = segs_dir.join("segment-000001.seg");
        assert!(seg_file.exists(), "segment-000001.seg must exist after seed");

        // load_store_from_segments should find a root
        let (root_opt, _snapshots) = load_store_from_segments(&segs_dir).expect("load failed");
        assert!(root_opt.is_some(), "segment must contain a RootUpdate");
    }

    /// Seed a directory with one file — content is retrievable via file_storage_get.
    #[test]
    fn test_seed_single_file_content_round_trip() {
        let store_dir = make_tempdir();
        let source_dir = make_tempdir();

        let content = b"hello from SliceFS seed test";
        std::fs::write(source_dir.path().join("hello.txt"), content).unwrap();

        run_seed(store_dir.path(), source_dir.path()).expect("seed failed");

        // Load from segments to get root
        let segs_dir = store_dir.path().join("segments");
        let (root_opt, _snapshots) = load_store_from_segments(&segs_dir).expect("load failed");
        let root = root_opt.expect("no root found");

        // Reconstruct metadata store
        let io = Arc::new(Mutex::new(StoreIo::new(store_dir.path())));
        let reloaded = DictMetadataStore::load_from_root(io.clone(), &root).expect("load_from_root failed");

        // Find the file inode via lookup.
        let file_ino = reloaded.lookup(1, "hello.txt").expect("lookup failed");

        // Get the manifest (content digest).
        let manifest = reloaded.get_manifest(file_ino).expect("get_manifest failed");
        assert_eq!(manifest.len(), 1, "expected exactly one block in manifest");

        // Retrieve content bytes from FileStorage.
        let read_back = {
            let mut io_guard = io.lock().unwrap();
            file_storage_get(&mut *io_guard, &manifest[0]).expect("file_storage_get failed")
        };
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

        run_seed(store_dir.path(), source_dir.path()).expect("seed failed");

        // Reload from segments.
        let segs_dir = store_dir.path().join("segments");
        let (root_opt, _) = load_store_from_segments(&segs_dir).expect("load failed");
        let root = root_opt.expect("no root found");

        let io = Arc::new(Mutex::new(StoreIo::new(store_dir.path())));
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

    /// Seed preserves file metadata — size matches original file size in InodeMeta.
    #[test]
    fn test_seed_preserves_file_size() {
        let store_dir = make_tempdir();
        let source_dir = make_tempdir();

        let content: Vec<u8> = (0u8..200u8).collect();
        std::fs::write(source_dir.path().join("sized.bin"), &content).unwrap();

        run_seed(store_dir.path(), source_dir.path()).expect("seed failed");

        let segs_dir = store_dir.path().join("segments");
        let (root_opt, _) = load_store_from_segments(&segs_dir).expect("load failed");
        let root = root_opt.expect("no root");

        let io = Arc::new(Mutex::new(StoreIo::new(store_dir.path())));
        let reloaded = DictMetadataStore::load_from_root(io, &root).unwrap();

        let ino = reloaded.lookup(1, "sized.bin").unwrap();
        let meta = reloaded.get_inode(ino).unwrap();
        assert_eq!(meta.size, 200, "file size mismatch: expected 200, got {}", meta.size);
    }
}
