//! `seed` subcommand — imports a directory tree into a SliceFS CAS store.
//!
//! Walks the source directory recursively, chunks file content via `State` CDC,
//! builds inode and directory records in `DictMetadataStore`, then serializes
//! the Dictionary and root digest to disk as `dictionary.bin` and `root.bin`.

use std::path::Path;

use blockset::{State, Tree};
use metadata::store::{DictMetadataStore, serialize_dictionary};
use slicefs_traits::digest::Digest224;
use slicefs_traits::metadata::{InodeMeta, InodeId, MetadataStore};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

// POSIX type bits
const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;

/// Import a directory tree rooted at `source_dir` into a CAS store at `store_path`.
///
/// Creates `<store_path>/dictionary.bin` and `<store_path>/root.bin`.
pub fn run_seed(store_path: &Path, source_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(store_path)?;

    let meta_store = DictMetadataStore::new();

    // Walk source_dir: root maps to inode 1 (the pre-created root dir).
    walk_dir(source_dir, 1, &meta_store)?;

    // Commit metadata state into Dictionary, get root digest.
    let root = meta_store.commit()?;

    // Serialize Dictionary.
    let dict_bytes = {
        let dict = meta_store.dict().lock().unwrap();
        serialize_dictionary(&*dict)
    };
    std::fs::write(store_path.join("dictionary.bin"), &dict_bytes)?;

    // Write root.bin: 7 × u32 LE = 28 bytes.
    let mut root_bytes = Vec::with_capacity(28);
    for word in &root {
        root_bytes.extend_from_slice(&word.to_le_bytes());
    }
    std::fs::write(store_path.join("root.bin"), &root_bytes)?;

    Ok(())
}

/// Recursively walk `dir_path`, creating inodes/dirs/manifests under `parent_ino`.
fn walk_dir(
    dir_path: &Path,
    parent_ino: InodeId,
    meta_store: &DictMetadataStore,
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
            walk_dir(&path, ino, meta_store)?;
        } else if fs_meta.is_file() {
            let (ino, content_digest) = seed_file(&path, &fs_meta, meta_store)?;
            meta_store.link(parent_ino, &name, ino)?;
            meta_store.set_manifest(ino, &[content_digest])?;
        }
        // Symlinks and other special files are skipped.
    }
    Ok(())
}

/// Create an inode for a regular file and push its content into the Dictionary.
/// Returns (ino, content_digest).
fn seed_file(
    path: &Path,
    fs_meta: &std::fs::Metadata,
    meta_store: &DictMetadataStore,
) -> Result<(InodeId, Digest224), Box<dyn std::error::Error>> {
    let bytes = std::fs::read(path)?;
    let size = bytes.len() as u64;

    // Push content bytes into the store's Dictionary via CDC.
    let content_digest: Digest224 = {
        let mut dict = meta_store.dict().lock().unwrap();
        State::push_all(&mut *dict, &bytes)
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
    use blockset::{GetBytes, GetData};
    use metadata::store::deserialize_dictionary;
    use slicefs_traits::digest::from_digest224;
    use slicefs_traits::metadata::MetadataStore;
    use tempfile::TempDir;

    fn make_tempdir() -> TempDir {
        tempfile::tempdir().expect("failed to create tempdir")
    }

    /// Seed an empty directory — store/root.bin (28 bytes) and store/dictionary.bin are created.
    #[test]
    fn test_seed_empty_directory() {
        let store_dir = make_tempdir();
        let source_dir = make_tempdir();

        run_seed(store_dir.path(), source_dir.path()).expect("seed failed");

        let root_bin = store_dir.path().join("root.bin");
        let dict_bin = store_dir.path().join("dictionary.bin");

        assert!(root_bin.exists(), "root.bin missing");
        assert!(dict_bin.exists(), "dictionary.bin missing");

        let root_bytes = std::fs::read(&root_bin).unwrap();
        assert_eq!(root_bytes.len(), 28, "root.bin should be exactly 28 bytes (Digest224)");
    }

    /// Seed a directory with one file — content is retrievable from the persisted Dictionary.
    #[test]
    fn test_seed_single_file_content_round_trip() {
        let store_dir = make_tempdir();
        let source_dir = make_tempdir();

        let content = b"hello from SliceFS seed test";
        std::fs::write(source_dir.path().join("hello.txt"), content).unwrap();

        run_seed(store_dir.path(), source_dir.path()).expect("seed failed");

        // Load the persisted dictionary and root.
        let dict_bytes = std::fs::read(store_dir.path().join("dictionary.bin")).unwrap();
        let root_bytes = std::fs::read(store_dir.path().join("root.bin")).unwrap();

        assert_eq!(root_bytes.len(), 28);

        let dict = deserialize_dictionary(&dict_bytes).expect("deserialize failed");
        let mut root: Digest224 = [0u32; 7];
        for (i, word) in root.iter_mut().enumerate() {
            *word = u32::from_le_bytes(root_bytes[i * 4..i * 4 + 4].try_into().unwrap());
        }

        // Reload metadata store.
        let reloaded = DictMetadataStore::load_from_root(dict.clone(), &root)
            .expect("load_from_root failed");

        // Find the file inode via lookup.
        let file_ino = reloaded.lookup(1, "hello.txt").expect("lookup failed");

        // Get the manifest (content digest).
        let manifest = reloaded.get_manifest(file_ino).expect("get_manifest failed");
        assert_eq!(manifest.len(), 1, "expected exactly one block in manifest");

        // Retrieve content bytes from the Dictionary.
        let content_digest256 = from_digest224(&manifest[0]);
        let read_back: Vec<u8> = GetBytes::new(GetData::new(&dict, &content_digest256)).collect();
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

        // Reload.
        let dict_bytes = std::fs::read(store_dir.path().join("dictionary.bin")).unwrap();
        let root_bytes = std::fs::read(store_dir.path().join("root.bin")).unwrap();
        let dict = deserialize_dictionary(&dict_bytes).unwrap();
        let mut root: Digest224 = [0u32; 7];
        for (i, w) in root.iter_mut().enumerate() {
            *w = u32::from_le_bytes(root_bytes[i * 4..i * 4 + 4].try_into().unwrap());
        }
        let reloaded = DictMetadataStore::load_from_root(dict, &root).unwrap();

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

        let dict_bytes = std::fs::read(store_dir.path().join("dictionary.bin")).unwrap();
        let root_bytes = std::fs::read(store_dir.path().join("root.bin")).unwrap();
        let dict = deserialize_dictionary(&dict_bytes).unwrap();
        let mut root: Digest224 = [0u32; 7];
        for (i, w) in root.iter_mut().enumerate() {
            *w = u32::from_le_bytes(root_bytes[i * 4..i * 4 + 4].try_into().unwrap());
        }
        let reloaded = DictMetadataStore::load_from_root(dict, &root).unwrap();

        let ino = reloaded.lookup(1, "sized.bin").unwrap();
        let meta = reloaded.get_inode(ino).unwrap();
        assert_eq!(meta.size, 200, "file size mismatch: expected 200, got {}", meta.size);
    }
}
