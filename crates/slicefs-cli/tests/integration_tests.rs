//! Comprehensive integration and E2E tests for SliceFS.
//!
//! Tests complete workflows end-to-end:
//!   - Seed -> Load -> Read round-trip
//!   - Write -> Read -> Overwrite -> Read
//!   - Concurrent/interleaved file operations
//!   - Metadata operations (chmod, chown, mtime, xattr)
//!   - Directory operations (mkdir, readdir, rmdir, rename)
//!   - Symlink and hard link E2E
//!   - Snapshot E2E
//!   - WAL/Crash recovery E2E
//!   - Dedup verification
//!   - Edge cases (empty files, special names, deep nesting, many files)
//!
//! All tests use `DictMetadataStore` and `SliceFsFilesystem` test_* helpers --
//! no FUSE mount required.

use blockset::file_storage_get;
use metadata::segment::load_store_from_segments;
use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use metadata::wal::{WalConfig, create_wal};
use slicefs_cli::filesystem::SliceFsFilesystem;
use slicefs_cli::seed::run_seed;
use slicefs_traits::metadata::MetadataStore;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;

// ── Helpers ──────────────────────────────────────────────────────────────────

fn fresh_fs() -> (SliceFsFilesystem, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let io = Arc::new(Mutex::new(StoreIo::new(dir.path())));
    let meta = DictMetadataStore::new(io.clone());
    let fs = SliceFsFilesystem::new(meta, io, None);
    (fs, dir)
}

fn fresh_fs_with_wal() -> (SliceFsFilesystem, TempDir) {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join("segments")).unwrap();
    let wal = create_wal(WalConfig::PerOp, dir.path(), 1).unwrap();
    let io = Arc::new(Mutex::new(StoreIo::new(dir.path())));
    let mut meta = DictMetadataStore::new(io.clone());
    meta.set_wal(wal);
    let fs = SliceFsFilesystem::new(meta, io, Some(dir.path().to_path_buf()));
    (fs, dir)
}

/// Read file content via manifest + file_storage_get.
fn read_content(fs: &SliceFsFilesystem, ino: u64) -> Vec<u8> {
    let manifest = fs.meta().get_manifest(ino).unwrap_or_default();
    if manifest.is_empty() {
        return vec![];
    }
    let mut io = fs.io().lock().unwrap();
    file_storage_get(&mut *io, &manifest[0]).unwrap_or_default()
}

/// Create a file with content via test_create + test_write + test_release.
fn create_file_with_content(
    fs: &SliceFsFilesystem,
    parent: u64,
    name: &str,
    content: &[u8],
) -> u64 {
    let (ino, fh) = fs
        .test_create(parent, name, S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create should succeed");
    if !content.is_empty() {
        fs.test_write(fh, 0, content).expect("write should succeed");
    }
    fs.test_release(ino, fh).expect("release should succeed");
    ino
}

// ════════════════════════════════════════════════════════════════════════════
// 1. Seed -> Load -> Read round-trip
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn test_seed_roundtrip_various_file_types() {
    // Create a source directory with various files
    let source_dir = TempDir::new().unwrap();
    let store_dir = TempDir::new().unwrap();

    // Empty file
    std::fs::write(source_dir.path().join("empty.txt"), b"").unwrap();

    // Small text file
    std::fs::write(source_dir.path().join("small.txt"), b"hello world").unwrap();

    // Larger file (64 KB of deterministic data)
    let large_content: Vec<u8> = (0..65536).map(|i| (i % 256) as u8).collect();
    std::fs::write(source_dir.path().join("large.bin"), &large_content).unwrap();

    // Binary file with all byte values
    let binary_content: Vec<u8> = (0..=255).collect();
    std::fs::write(source_dir.path().join("binary.bin"), &binary_content).unwrap();

    // Nested directory with file
    std::fs::create_dir_all(source_dir.path().join("subdir/nested")).unwrap();
    std::fs::write(
        source_dir.path().join("subdir/nested/deep.txt"),
        b"deep content",
    )
    .unwrap();
    std::fs::write(source_dir.path().join("subdir/inner.txt"), b"inner content").unwrap();

    // Seed
    run_seed(store_dir.path(), source_dir.path()).expect("seed should succeed");

    // Load the store from segments
    let segs_dir = store_dir.path().join("segments");
    let (root_opt, _snapshots) = load_store_from_segments(&segs_dir).expect("should load segments");
    let root = root_opt.expect("root should exist after seed");

    let io = Arc::new(Mutex::new(StoreIo::new(store_dir.path())));
    let store = DictMetadataStore::load_from_root(io.clone(), &root)
        .expect("should reconstruct store from root");

    // Verify root directory has expected entries
    let root_entries = store.list_directory(1).unwrap();
    let root_names: Vec<&str> = root_entries.iter().map(|e| e.name.as_str()).collect();
    assert!(root_names.contains(&"empty.txt"));
    assert!(root_names.contains(&"small.txt"));
    assert!(root_names.contains(&"large.bin"));
    assert!(root_names.contains(&"binary.bin"));
    assert!(root_names.contains(&"subdir"));

    // Verify empty file
    let empty_ino = store.lookup(1, "empty.txt").unwrap();
    let empty_meta = store.get_inode(empty_ino).unwrap();
    assert_eq!(empty_meta.size, 0);

    // Verify small file content
    let small_ino = store.lookup(1, "small.txt").unwrap();
    let small_manifest = store.get_manifest(small_ino).unwrap();
    assert!(!small_manifest.is_empty());
    let small_content = {
        let mut io_g = io.lock().unwrap();
        file_storage_get(&mut *io_g, &small_manifest[0]).unwrap()
    };
    assert_eq!(small_content, b"hello world");

    // Verify large file content byte-for-byte
    let large_ino = store.lookup(1, "large.bin").unwrap();
    let large_manifest = store.get_manifest(large_ino).unwrap();
    let large_readback = {
        let mut io_g = io.lock().unwrap();
        file_storage_get(&mut *io_g, &large_manifest[0]).unwrap()
    };
    assert_eq!(
        large_readback, large_content,
        "large file must round-trip byte-for-byte"
    );

    // Verify binary file
    let bin_ino = store.lookup(1, "binary.bin").unwrap();
    let bin_manifest = store.get_manifest(bin_ino).unwrap();
    let bin_readback = {
        let mut io_g = io.lock().unwrap();
        file_storage_get(&mut *io_g, &bin_manifest[0]).unwrap()
    };
    assert_eq!(bin_readback, binary_content, "binary file must round-trip");

    // Verify nested directory structure
    let subdir_ino = store.lookup(1, "subdir").unwrap();
    let subdir_meta = store.get_inode(subdir_ino).unwrap();
    assert_eq!(subdir_meta.mode & S_IFDIR, S_IFDIR);

    let inner_ino = store.lookup(subdir_ino, "inner.txt").unwrap();
    let inner_manifest = store.get_manifest(inner_ino).unwrap();
    let inner_readback = {
        let mut io_g = io.lock().unwrap();
        file_storage_get(&mut *io_g, &inner_manifest[0]).unwrap()
    };
    assert_eq!(inner_readback, b"inner content");

    // Verify deeply nested file
    let nested_ino = store.lookup(subdir_ino, "nested").unwrap();
    let deep_ino = store.lookup(nested_ino, "deep.txt").unwrap();
    let deep_manifest = store.get_manifest(deep_ino).unwrap();
    let deep_readback = {
        let mut io_g = io.lock().unwrap();
        file_storage_get(&mut *io_g, &deep_manifest[0]).unwrap()
    };
    assert_eq!(deep_readback, b"deep content");
}

#[test]
fn test_seed_preserves_directory_structure() {
    let source_dir = TempDir::new().unwrap();
    let store_dir = TempDir::new().unwrap();

    // Create multi-level directory structure
    std::fs::create_dir_all(source_dir.path().join("a/b/c")).unwrap();
    std::fs::write(source_dir.path().join("a/file_a.txt"), b"in a").unwrap();
    std::fs::write(source_dir.path().join("a/b/file_b.txt"), b"in b").unwrap();
    std::fs::write(source_dir.path().join("a/b/c/file_c.txt"), b"in c").unwrap();

    run_seed(store_dir.path(), source_dir.path()).expect("seed should succeed");

    let segs_dir = store_dir.path().join("segments");
    let (root_opt, _) = load_store_from_segments(&segs_dir).unwrap();
    let root = root_opt.unwrap();
    let io = Arc::new(Mutex::new(StoreIo::new(store_dir.path())));
    let store = DictMetadataStore::load_from_root(io, &root).unwrap();

    let a_ino = store.lookup(1, "a").unwrap();
    let b_ino = store.lookup(a_ino, "b").unwrap();
    let c_ino = store.lookup(b_ino, "c").unwrap();

    assert!(store.lookup(a_ino, "file_a.txt").is_ok());
    assert!(store.lookup(b_ino, "file_b.txt").is_ok());
    assert!(store.lookup(c_ino, "file_c.txt").is_ok());
}

// ════════════════════════════════════════════════════════════════════════════
// 2. Write -> Read -> Overwrite -> Read
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn test_write_read_overwrite_read_roundtrip() {
    let (fs, _dir) = fresh_fs();

    // Create a file and write initial content
    let (ino, fh) = fs
        .test_create(1, "doc.txt", S_IFREG | 0o644, 0o022, 1000, 1000)
        .unwrap();
    fs.test_write(fh, 0, b"original content").unwrap();
    fs.test_release(ino, fh).unwrap();

    // Read back initial content
    let content = fs.test_read(ino, 0, 1024).unwrap();
    assert_eq!(content, b"original content");

    // Overwrite with O_TRUNC via test_open
    let (fh2, is_write) = fs.test_open(ino, libc::O_WRONLY | libc::O_TRUNC).unwrap();
    assert!(is_write, "O_WRONLY should be a write handle");
    fs.test_write(fh2, 0, b"new content").unwrap();
    fs.test_release(ino, fh2).unwrap();

    // Read back new content
    let new_content = fs.test_read(ino, 0, 1024).unwrap();
    assert_eq!(new_content, b"new content");

    // Verify old content is completely gone (size changed)
    let meta = fs.meta().get_inode(ino).unwrap();
    assert_eq!(meta.size, 11, "size must reflect new content length");
}

#[test]
fn test_overwrite_with_shorter_content() {
    let (fs, _dir) = fresh_fs();
    let ino = create_file_with_content(&fs, 1, "shrink.txt", b"long original content here");

    // Overwrite with shorter content
    let (fh, _) = fs.test_open(ino, libc::O_WRONLY | libc::O_TRUNC).unwrap();
    fs.test_write(fh, 0, b"short").unwrap();
    fs.test_release(ino, fh).unwrap();

    let content = fs.test_read(ino, 0, 1024).unwrap();
    assert_eq!(content, b"short");
    assert_eq!(fs.meta().get_inode(ino).unwrap().size, 5);
}

#[test]
fn test_overwrite_with_longer_content() {
    let (fs, _dir) = fresh_fs();
    let ino = create_file_with_content(&fs, 1, "grow.txt", b"short");

    let (fh, _) = fs.test_open(ino, libc::O_WRONLY | libc::O_TRUNC).unwrap();
    fs.test_write(fh, 0, b"much longer replacement content")
        .unwrap();
    fs.test_release(ino, fh).unwrap();

    let content = fs.test_read(ino, 0, 1024).unwrap();
    assert_eq!(content, b"much longer replacement content");
}

#[test]
fn test_multiple_overwrites_in_sequence() {
    let (fs, _dir) = fresh_fs();
    let ino = create_file_with_content(&fs, 1, "multi.txt", b"version 1");

    for version in 2..=5 {
        let new_content = format!("version {}", version);
        let (fh, _) = fs.test_open(ino, libc::O_WRONLY | libc::O_TRUNC).unwrap();
        fs.test_write(fh, 0, new_content.as_bytes()).unwrap();
        fs.test_release(ino, fh).unwrap();

        let readback = fs.test_read(ino, 0, 1024).unwrap();
        assert_eq!(readback, new_content.as_bytes());
    }
}

// ════════════════════════════════════════════════════════════════════════════
// 3. Concurrent / interleaved operations
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn test_interleaved_writes_to_multiple_files() {
    let (fs, _dir) = fresh_fs();

    let (ino_a, fh_a) = fs
        .test_create(1, "a.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .unwrap();
    let (ino_b, fh_b) = fs
        .test_create(1, "b.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .unwrap();
    let (ino_c, fh_c) = fs
        .test_create(1, "c.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .unwrap();

    // Interleave writes across handles
    fs.test_write(fh_a, 0, b"AAA").unwrap();
    fs.test_write(fh_b, 0, b"BBB").unwrap();
    fs.test_write(fh_c, 0, b"CCC").unwrap();
    fs.test_write(fh_a, 3, b"aaa").unwrap();
    fs.test_write(fh_b, 3, b"bbb").unwrap();
    fs.test_write(fh_c, 3, b"ccc").unwrap();

    fs.test_release(ino_a, fh_a).unwrap();
    fs.test_release(ino_b, fh_b).unwrap();
    fs.test_release(ino_c, fh_c).unwrap();

    assert_eq!(fs.test_read(ino_a, 0, 1024).unwrap(), b"AAAaaa");
    assert_eq!(fs.test_read(ino_b, 0, 1024).unwrap(), b"BBBbbb");
    assert_eq!(fs.test_read(ino_c, 0, 1024).unwrap(), b"CCCccc");
}

#[test]
fn test_read_while_another_file_is_being_written() {
    let (fs, _dir) = fresh_fs();

    // Create and close file A
    let ino_a = create_file_with_content(&fs, 1, "readable.txt", b"stable content");

    // Open file B for writing
    let (ino_b, fh_b) = fs
        .test_create(1, "writing.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .unwrap();
    fs.test_write(fh_b, 0, b"being written").unwrap();

    // Read A while B is still open for writing
    let a_content = fs.test_read(ino_a, 0, 1024).unwrap();
    assert_eq!(
        a_content, b"stable content",
        "reading A while writing B must work"
    );

    fs.test_release(ino_b, fh_b).unwrap();

    // Verify B as well
    assert_eq!(fs.test_read(ino_b, 0, 1024).unwrap(), b"being written");
}

#[test]
fn test_cross_handle_read_on_open_write() {
    let (fs, _dir) = fresh_fs();

    let (ino, fh) = fs
        .test_create(1, "crossread.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .unwrap();
    fs.test_write(fh, 0, b"uncommitted data").unwrap();

    // Read the same file while it's open for writing (cross-handle read)
    let content = fs.test_read(ino, 0, 1024).unwrap();
    assert_eq!(
        content, b"uncommitted data",
        "cross-handle read should see uncommitted data"
    );

    fs.test_release(ino, fh).unwrap();
}

// ════════════════════════════════════════════════════════════════════════════
// 4. Metadata operations E2E
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn test_chmod_roundtrip() {
    let (fs, _dir) = fresh_fs();
    let ino = create_file_with_content(&fs, 1, "perms.txt", b"data");

    // Change to 0o755
    fs.test_setattr_mode(ino, 0o755).unwrap();
    let meta = fs.meta().get_inode(ino).unwrap();
    assert_eq!(meta.mode & 0o7777, 0o755);
    assert_eq!(meta.mode & S_IFREG, S_IFREG, "type bits must be preserved");

    // Change to read-only 0o444
    fs.test_setattr_mode(ino, 0o444).unwrap();
    let meta = fs.meta().get_inode(ino).unwrap();
    assert_eq!(meta.mode & 0o7777, 0o444);
}

#[test]
fn test_chown_roundtrip() {
    let (fs, _dir) = fresh_fs();
    let ino = create_file_with_content(&fs, 1, "owned.txt", b"data");

    fs.test_setattr_uid_gid(ino, Some(2000), Some(3000))
        .unwrap();
    let meta = fs.meta().get_inode(ino).unwrap();
    assert_eq!(meta.uid, 2000);
    assert_eq!(meta.gid, 3000);

    // Change only uid
    fs.test_setattr_uid_gid(ino, Some(4000), None).unwrap();
    let meta = fs.meta().get_inode(ino).unwrap();
    assert_eq!(meta.uid, 4000);
    assert_eq!(meta.gid, 3000, "gid must be unchanged when None passed");

    // Change only gid
    fs.test_setattr_uid_gid(ino, None, Some(5000)).unwrap();
    let meta = fs.meta().get_inode(ino).unwrap();
    assert_eq!(meta.uid, 4000, "uid must be unchanged when None passed");
    assert_eq!(meta.gid, 5000);
}

#[test]
fn test_mtime_roundtrip() {
    let (fs, _dir) = fresh_fs();
    let ino = create_file_with_content(&fs, 1, "timed.txt", b"data");

    fs.test_setattr_mtime(ino, 1_700_000_000, 123_456_789)
        .unwrap();
    let meta = fs.meta().get_inode(ino).unwrap();
    assert_eq!(meta.mtime_sec, 1_700_000_000);
    assert_eq!(meta.mtime_nsec, 123_456_789);
}

#[test]
fn test_xattr_full_lifecycle() {
    let (fs, _dir) = fresh_fs();
    let ino = create_file_with_content(&fs, 1, "xattr.txt", b"data");

    // Set xattr
    fs.test_setxattr(ino, "user.author", b"alice").unwrap();
    fs.test_setxattr(ino, "user.version", b"42").unwrap();

    // Get xattr
    let val = fs.test_getxattr(ino, "user.author").unwrap();
    assert_eq!(val, b"alice");
    let val2 = fs.test_getxattr(ino, "user.version").unwrap();
    assert_eq!(val2, b"42");

    // List xattrs -- returns null-separated names
    let list = fs.test_listxattr(ino).unwrap();
    let names: Vec<&str> = list
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| std::str::from_utf8(s).unwrap())
        .collect();
    assert!(names.contains(&"user.author"));
    assert!(names.contains(&"user.version"));

    // Overwrite xattr
    fs.test_setxattr(ino, "user.author", b"bob").unwrap();
    let updated = fs.test_getxattr(ino, "user.author").unwrap();
    assert_eq!(updated, b"bob");

    // Remove xattr
    fs.test_removexattr(ino, "user.author").unwrap();
    let err = fs.test_getxattr(ino, "user.author").unwrap_err();
    assert_eq!(err, libc::ENODATA, "removed xattr must return ENODATA");

    // Other xattr still present
    assert_eq!(fs.test_getxattr(ino, "user.version").unwrap(), b"42");

    // Remove remaining
    fs.test_removexattr(ino, "user.version").unwrap();
    let list = fs.test_listxattr(ino).unwrap();
    let remaining: Vec<&[u8]> = list.split(|&b| b == 0).filter(|s| !s.is_empty()).collect();
    assert!(remaining.is_empty(), "all xattrs must be removed");
}

#[test]
fn test_xattr_binary_values() {
    let (fs, _dir) = fresh_fs();
    let ino = create_file_with_content(&fs, 1, "binxattr.txt", b"data");

    let binary_val: Vec<u8> = (0..=255).collect();
    fs.test_setxattr(ino, "user.binary", &binary_val).unwrap();
    let readback = fs.test_getxattr(ino, "user.binary").unwrap();
    assert_eq!(readback, binary_val, "binary xattr value must round-trip");
}

// ════════════════════════════════════════════════════════════════════════════
// 5. Directory operations E2E
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn test_mkdir_create_files_readdir_verify() {
    let (fs, _dir) = fresh_fs();

    let dir_ino = fs
        .simulate_mkdir(1, "mydir", S_IFDIR | 0o755, 0o022, 0, 0)
        .unwrap();

    // Create files inside
    create_file_with_content(&fs, dir_ino, "file1.txt", b"content 1");
    create_file_with_content(&fs, dir_ino, "file2.txt", b"content 2");
    create_file_with_content(&fs, dir_ino, "file3.txt", b"content 3");

    // Readdir
    let entries = fs.test_readdir(dir_ino, 0).unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.2.as_str()).collect();
    assert!(names.contains(&"."));
    assert!(names.contains(&".."));
    assert!(names.contains(&"file1.txt"));
    assert!(names.contains(&"file2.txt"));
    assert!(names.contains(&"file3.txt"));
}

#[test]
fn test_nested_mkdir_and_deep_file_access() {
    let (fs, _dir) = fresh_fs();

    let d1 = fs
        .simulate_mkdir(1, "level1", S_IFDIR | 0o755, 0o022, 0, 0)
        .unwrap();
    let d2 = fs
        .simulate_mkdir(d1, "level2", S_IFDIR | 0o755, 0o022, 0, 0)
        .unwrap();
    let d3 = fs
        .simulate_mkdir(d2, "level3", S_IFDIR | 0o755, 0o022, 0, 0)
        .unwrap();

    let ino = create_file_with_content(&fs, d3, "deep.txt", b"deep nested content");

    // Navigate back to the file and read
    let found_d1 = fs.meta().lookup(1, "level1").unwrap();
    let found_d2 = fs.meta().lookup(found_d1, "level2").unwrap();
    let found_d3 = fs.meta().lookup(found_d2, "level3").unwrap();
    let found_ino = fs.meta().lookup(found_d3, "deep.txt").unwrap();
    assert_eq!(found_ino, ino);

    let content = fs.test_read(ino, 0, 1024).unwrap();
    assert_eq!(content, b"deep nested content");
}

#[test]
fn test_rmdir_empty_then_verify_gone() {
    let (fs, _dir) = fresh_fs();

    fs.simulate_mkdir(1, "tempdir", S_IFDIR | 0o755, 0o022, 0, 0)
        .unwrap();
    assert!(fs.meta().lookup(1, "tempdir").is_ok());

    fs.simulate_rmdir(1, "tempdir").unwrap();
    assert!(
        fs.meta().lookup(1, "tempdir").is_err(),
        "directory must be gone after rmdir"
    );
}

#[test]
fn test_rmdir_nonempty_returns_enotempty() {
    let (fs, _dir) = fresh_fs();

    let dir_ino = fs
        .simulate_mkdir(1, "notempty", S_IFDIR | 0o755, 0o022, 0, 0)
        .unwrap();
    create_file_with_content(&fs, dir_ino, "child.txt", b"blocking removal");

    let result = fs.simulate_rmdir(1, "notempty");
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), libc::ENOTEMPTY);
}

#[test]
fn test_rename_file_within_directory() {
    let (fs, _dir) = fresh_fs();
    let ino = create_file_with_content(&fs, 1, "old.txt", b"rename me");

    fs.simulate_rename(1, "old.txt", 1, "new.txt", 0).unwrap();

    assert!(fs.meta().lookup(1, "old.txt").is_err());
    let found = fs.meta().lookup(1, "new.txt").unwrap();
    assert_eq!(found, ino);

    let content = fs.test_read(ino, 0, 1024).unwrap();
    assert_eq!(content, b"rename me", "content must survive rename");
}

#[test]
fn test_rename_file_across_directories() {
    let (fs, _dir) = fresh_fs();
    let dir_ino = fs
        .simulate_mkdir(1, "dest", S_IFDIR | 0o755, 0o022, 0, 0)
        .unwrap();
    let ino = create_file_with_content(&fs, 1, "moveme.txt", b"moving data");

    fs.simulate_rename(1, "moveme.txt", dir_ino, "moved.txt", 0)
        .unwrap();

    assert!(fs.meta().lookup(1, "moveme.txt").is_err());
    let found = fs.meta().lookup(dir_ino, "moved.txt").unwrap();
    assert_eq!(found, ino);

    let content = fs.test_read(ino, 0, 1024).unwrap();
    assert_eq!(content, b"moving data");
}

#[test]
fn test_rename_directory_with_contents() {
    let (fs, _dir) = fresh_fs();
    let dir_ino = fs
        .simulate_mkdir(1, "oldname", S_IFDIR | 0o755, 0o022, 0, 0)
        .unwrap();
    create_file_with_content(&fs, dir_ino, "inside.txt", b"inner data");

    fs.simulate_rename(1, "oldname", 1, "newname", 0).unwrap();

    assert!(fs.meta().lookup(1, "oldname").is_err());
    let new_dir_ino = fs.meta().lookup(1, "newname").unwrap();
    assert_eq!(new_dir_ino, dir_ino);

    // File inside should still be accessible
    let file_ino = fs.meta().lookup(dir_ino, "inside.txt").unwrap();
    let content = fs.test_read(file_ino, 0, 1024).unwrap();
    assert_eq!(content, b"inner data");
}

// ════════════════════════════════════════════════════════════════════════════
// 6. Symlink E2E
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn test_symlink_create_readlink_verify() {
    let (fs, _dir) = fresh_fs();

    let ino = fs
        .simulate_symlink(1, "mylink", "/etc/config", 1000, 1000)
        .unwrap();

    // Readlink must return exact target
    let target = fs.simulate_readlink(ino).unwrap();
    assert_eq!(target, "/etc/config");

    // Entry must be in parent directory
    let found = fs.meta().lookup(1, "mylink").unwrap();
    assert_eq!(found, ino);
}

#[test]
fn test_symlink_relative_target() {
    let (fs, _dir) = fresh_fs();
    let ino = fs
        .simulate_symlink(1, "rellink", "../sibling/file.txt", 0, 0)
        .unwrap();
    let target = fs.simulate_readlink(ino).unwrap();
    assert_eq!(target, "../sibling/file.txt");
}

#[test]
fn test_symlink_long_target() {
    let (fs, _dir) = fresh_fs();
    let long_target: String = "/a/".repeat(100) + "file.txt";
    let ino = fs
        .simulate_symlink(1, "longlink", &long_target, 0, 0)
        .unwrap();
    let target = fs.simulate_readlink(ino).unwrap();
    assert_eq!(target, long_target);
}

// ════════════════════════════════════════════════════════════════════════════
// 7. Hard link E2E
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn test_hardlink_both_names_exist() {
    let (fs, _dir) = fresh_fs();
    let ino = create_file_with_content(&fs, 1, "original.txt", b"shared data");

    fs.simulate_link(ino, 1, "link.txt").unwrap();

    let ino_orig = fs.meta().lookup(1, "original.txt").unwrap();
    let ino_link = fs.meta().lookup(1, "link.txt").unwrap();
    assert_eq!(ino_orig, ino_link, "both names must resolve to same inode");
}

#[test]
fn test_hardlink_write_through_one_read_through_other() {
    let (fs, _dir) = fresh_fs();

    // Create a file with initial content
    let (ino, fh) = fs
        .test_create(1, "source.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .unwrap();
    fs.test_write(fh, 0, b"initial").unwrap();
    fs.test_release(ino, fh).unwrap();

    // Create hard link
    fs.simulate_link(ino, 1, "alias.txt").unwrap();

    // Overwrite through original name
    let (fh2, _) = fs.test_open(ino, libc::O_WRONLY | libc::O_TRUNC).unwrap();
    fs.test_write(fh2, 0, b"updated via source").unwrap();
    fs.test_release(ino, fh2).unwrap();

    // Read through alias -- should see updated content
    let alias_ino = fs.meta().lookup(1, "alias.txt").unwrap();
    let content = fs.test_read(alias_ino, 0, 1024).unwrap();
    assert_eq!(
        content, b"updated via source",
        "reading through hard link should see latest content"
    );
}

#[test]
fn test_hardlink_unlink_one_other_still_works() {
    let (fs, _dir) = fresh_fs();
    let ino = create_file_with_content(&fs, 1, "keep.txt", b"persistent data");
    fs.simulate_link(ino, 1, "remove.txt").unwrap();

    // Verify nlinks is 2
    let meta = fs.meta().get_inode(ino).unwrap();
    assert_eq!(meta.nlinks, 2);

    // Unlink one name
    fs.simulate_unlink(1, "remove.txt").unwrap();

    // Other name still works
    let found = fs.meta().lookup(1, "keep.txt").unwrap();
    assert_eq!(found, ino);
    let content = fs.test_read(ino, 0, 1024).unwrap();
    assert_eq!(content, b"persistent data");

    // Nlinks must be 1 now
    let meta = fs.meta().get_inode(ino).unwrap();
    assert_eq!(meta.nlinks, 1);
}

#[test]
fn test_hardlink_nlink_count() {
    let (fs, _dir) = fresh_fs();
    let ino = create_file_with_content(&fs, 1, "base.txt", b"data");

    assert_eq!(fs.meta().get_inode(ino).unwrap().nlinks, 1);

    fs.simulate_link(ino, 1, "link1.txt").unwrap();
    assert_eq!(fs.meta().get_inode(ino).unwrap().nlinks, 2);

    fs.simulate_link(ino, 1, "link2.txt").unwrap();
    assert_eq!(fs.meta().get_inode(ino).unwrap().nlinks, 3);

    fs.simulate_unlink(1, "link1.txt").unwrap();
    assert_eq!(fs.meta().get_inode(ino).unwrap().nlinks, 2);
}

// ════════════════════════════════════════════════════════════════════════════
// 8. Snapshot E2E
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn test_snapshot_preserves_state_after_modifications() {
    let (fs, dir) = fresh_fs_with_wal();

    // Create initial file
    let ino = create_file_with_content(&fs, 1, "data.txt", b"snapshot version 1");

    // Create a named snapshot
    let snap = fs.meta().create_snapshot(Some("v1".to_string())).unwrap();
    assert_eq!(snap.version, 1);
    assert_eq!(snap.name, Some("v1".to_string()));

    // Modify the file after snapshot
    let (fh, _) = fs.test_open(ino, libc::O_WRONLY | libc::O_TRUNC).unwrap();
    fs.test_write(fh, 0, b"modified after snapshot").unwrap();
    fs.test_release(ino, fh).unwrap();

    // Current state should have modified content
    let current_content = fs.test_read(ino, 0, 1024).unwrap();
    assert_eq!(current_content, b"modified after snapshot");

    // Load the snapshot root and verify it has the original content
    let io = Arc::new(Mutex::new(StoreIo::new(dir.path())));
    let snap_store = DictMetadataStore::load_from_root(io, &snap.root).unwrap();
    let snap_ino = snap_store.lookup(1, "data.txt").unwrap();
    let snap_manifest = snap_store.get_manifest(snap_ino).unwrap();
    let snap_content = {
        let mut io_g = fs.io().lock().unwrap();
        file_storage_get(&mut *io_g, &snap_manifest[0]).unwrap()
    };
    assert_eq!(
        snap_content, b"snapshot version 1",
        "snapshot must preserve original content"
    );
}

#[test]
fn test_snapshot_list_and_find() {
    let (fs, _dir) = fresh_fs_with_wal();

    create_file_with_content(&fs, 1, "f1.txt", b"v1");
    let snap1 = fs
        .meta()
        .create_snapshot(Some("release-1".to_string()))
        .unwrap();

    create_file_with_content(&fs, 1, "f2.txt", b"v2");
    let snap2 = fs
        .meta()
        .create_snapshot(Some("release-2".to_string()))
        .unwrap();

    // List all snapshots
    let snaps = fs.meta().list_snapshots();
    assert_eq!(snaps.len(), 2);
    assert_eq!(snaps[0].version, snap1.version);
    assert_eq!(snaps[1].version, snap2.version);

    // Find by name
    let found = fs.meta().find_snapshot("release-1").unwrap();
    assert_eq!(found.version, snap1.version);

    // Find by version number
    let found2 = fs.meta().find_snapshot(&snap2.version.to_string()).unwrap();
    assert_eq!(found2.name, Some("release-2".to_string()));
}

// ════════════════════════════════════════════════════════════════════════════
// 9. WAL / Crash recovery E2E
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn test_crash_recovery_committed_data_survives() {
    let store_dir = TempDir::new().unwrap();

    {
        let (fs, _) = {
            std::fs::create_dir_all(store_dir.path().join("segments")).unwrap();
            let wal = create_wal(WalConfig::PerOp, store_dir.path(), 1).unwrap();
            let io = Arc::new(Mutex::new(StoreIo::new(store_dir.path())));
            let mut meta = DictMetadataStore::new(io.clone());
            meta.set_wal(wal);
            let fs = SliceFsFilesystem::new(meta, io, Some(store_dir.path().to_path_buf()));
            (fs, ())
        };

        // Write and commit
        let (ino, fh) = fs.test_create(1, "committed.txt", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"durable data").unwrap();
        fs.test_release(ino, fh).unwrap();
        fs.meta().commit().unwrap();

        // Write an uncommitted file
        let (_ino2, fh2) = fs
            .test_create(1, "uncommitted.txt", 0o644, 0, 0, 0)
            .unwrap();
        fs.test_write(fh2, 0, b"lost data").unwrap();
        // DO NOT release or commit -- simulate crash
        drop(fs);
    }

    // Reload
    let segs_dir = store_dir.path().join("segments");
    let (root_opt, _) = load_store_from_segments(&segs_dir).unwrap();
    let root = root_opt.expect("committed root must exist");
    let io = Arc::new(Mutex::new(StoreIo::new(store_dir.path())));
    let rebuilt = DictMetadataStore::load_from_root(io, &root).unwrap();

    // Committed file must be present
    assert!(rebuilt.lookup(1, "committed.txt").is_ok());

    // Root must be consistent (no panic)
    let root_inode = rebuilt.get_inode(1).unwrap();
    assert_eq!(root_inode.mode & S_IFDIR, S_IFDIR);
}

#[test]
fn test_crash_recovery_fsynced_data_survives() {
    let store_dir = TempDir::new().unwrap();

    {
        std::fs::create_dir_all(store_dir.path().join("segments")).unwrap();
        let wal = create_wal(WalConfig::PerOp, store_dir.path(), 1).unwrap();
        let io = Arc::new(Mutex::new(StoreIo::new(store_dir.path())));
        let mut meta = DictMetadataStore::new(io.clone());
        meta.set_wal(wal);
        let fs = SliceFsFilesystem::new(meta, io, Some(store_dir.path().to_path_buf()));

        let (ino, fh) = fs.test_create(1, "fsynced.txt", 0o644, 0, 0, 0).unwrap();
        fs.test_write(fh, 0, b"fsynced content").unwrap();
        fs.test_fsync(ino, fh).unwrap();
        fs.meta().commit().unwrap();

        // Crash
        drop(fs);
    }

    let segs_dir = store_dir.path().join("segments");
    let (root_opt, _) = load_store_from_segments(&segs_dir).unwrap();
    let root = root_opt.unwrap();
    let io = Arc::new(Mutex::new(StoreIo::new(store_dir.path())));
    let rebuilt = DictMetadataStore::load_from_root(io, &root).unwrap();

    let ino = rebuilt.lookup(1, "fsynced.txt").unwrap();
    let meta = rebuilt.get_inode(ino).unwrap();
    assert_eq!(meta.size, 15, "fsynced file size must be preserved");
}

// ════════════════════════════════════════════════════════════════════════════
// 10. Dedup verification
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn test_dedup_identical_content_shares_digest() {
    let (fs, _dir) = fresh_fs();
    let content = b"identical content for dedup test";

    let ino1 = create_file_with_content(&fs, 1, "dup1.txt", content);
    let ino2 = create_file_with_content(&fs, 1, "dup2.txt", content);

    let m1 = fs.meta().get_manifest(ino1).unwrap();
    let m2 = fs.meta().get_manifest(ino2).unwrap();
    assert_eq!(m1, m2, "identical content must produce the same digest");
}

#[test]
fn test_dedup_refcount_is_correct() {
    let (fs, _dir) = fresh_fs();
    let content = b"shared content for refcount check";

    let ino1 = create_file_with_content(&fs, 1, "ref1.txt", content);
    let ino2 = create_file_with_content(&fs, 1, "ref2.txt", content);

    let m1 = fs.meta().get_manifest(ino1).unwrap();
    let rc = fs.meta().get_refcount(&m1[0]);
    assert_eq!(rc, 2, "refcount must be 2 for two files with same content");

    // Create a third copy
    let _ino3 = create_file_with_content(&fs, 1, "ref3.txt", content);
    let rc = fs.meta().get_refcount(&m1[0]);
    assert_eq!(
        rc, 3,
        "refcount must be 3 for three files with same content"
    );

    // Delete one file
    fs.simulate_unlink(1, "ref1.txt").unwrap();

    // Remaining files should still be readable
    let content2 = fs.test_read(ino2, 0, 1024).unwrap();
    assert_eq!(content2, content);
}

#[test]
fn test_dedup_different_content_different_digest() {
    let (fs, _dir) = fresh_fs();

    let ino1 = create_file_with_content(&fs, 1, "unique1.txt", b"content A");
    let ino2 = create_file_with_content(&fs, 1, "unique2.txt", b"content B");

    let m1 = fs.meta().get_manifest(ino1).unwrap();
    let m2 = fs.meta().get_manifest(ino2).unwrap();
    assert_ne!(m1, m2, "different content must produce different digests");
}

// ════════════════════════════════════════════════════════════════════════════
// 11. Edge cases
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn test_edge_empty_file() {
    let (fs, _dir) = fresh_fs();
    let ino = create_file_with_content(&fs, 1, "empty.txt", b"");

    let content = fs.test_read(ino, 0, 1024).unwrap();
    assert!(content.is_empty());
    assert_eq!(fs.meta().get_inode(ino).unwrap().size, 0);
}

#[test]
fn test_edge_single_byte_file() {
    let (fs, _dir) = fresh_fs();
    let ino = create_file_with_content(&fs, 1, "one.txt", b"X");

    let content = fs.test_read(ino, 0, 1024).unwrap();
    assert_eq!(content, b"X");
    assert_eq!(fs.meta().get_inode(ino).unwrap().size, 1);
}

#[test]
fn test_edge_all_zeros_file() {
    let (fs, _dir) = fresh_fs();
    let zeros = vec![0u8; 4096];
    let ino = create_file_with_content(&fs, 1, "zeros.bin", &zeros);

    let content = fs.test_read(ino, 0, 8192).unwrap();
    assert_eq!(content.len(), 4096);
    assert!(content.iter().all(|&b| b == 0));
}

#[test]
fn test_edge_file_names_with_spaces() {
    let (fs, _dir) = fresh_fs();
    let ino = create_file_with_content(&fs, 1, "file with spaces.txt", b"spaced");

    let found = fs.meta().lookup(1, "file with spaces.txt").unwrap();
    assert_eq!(found, ino);
    let content = fs.test_read(ino, 0, 1024).unwrap();
    assert_eq!(content, b"spaced");
}

#[test]
fn test_edge_file_names_with_special_chars() {
    let (fs, _dir) = fresh_fs();

    // Various special characters in filenames
    let names = [
        "file-with-dashes.txt",
        "file_with_underscores.txt",
        "file.multiple.dots.txt",
        "UPPERCASE.TXT",
        "MiXeD.CaSe.txt",
        "file@symbol.txt",
        "file#hash.txt",
        "file+plus.txt",
        "file=equals.txt",
    ];

    for name in &names {
        let ino = create_file_with_content(&fs, 1, name, name.as_bytes());
        let found = fs.meta().lookup(1, name).unwrap();
        assert_eq!(found, ino, "should find file with name: {}", name);
        let content = fs.test_read(ino, 0, 1024).unwrap();
        assert_eq!(content, name.as_bytes());
    }
}

#[test]
fn test_edge_unicode_file_names() {
    let (fs, _dir) = fresh_fs();

    let unicode_names = [
        "\u{00e9}t\u{00e9}.txt", // ete.txt with accents
        "\u{00fc}ber.txt",       // uber with umlaut
        "\u{4f60}\u{597d}.txt",  // Chinese "hello"
        "\u{1f600}.txt",         // emoji
    ];

    for name in &unicode_names {
        let ino = create_file_with_content(&fs, 1, name, b"unicode");
        let found = fs.meta().lookup(1, name).unwrap();
        assert_eq!(found, ino, "should find file with unicode name: {}", name);
    }
}

#[test]
fn test_edge_deep_nesting_10_levels() {
    let (fs, _dir) = fresh_fs();

    let mut parent = 1u64;
    for level in 0..10 {
        let name = format!("level_{}", level);
        parent = fs
            .simulate_mkdir(parent, &name, S_IFDIR | 0o755, 0o022, 0, 0)
            .unwrap();
    }

    // Create file at deepest level
    let ino = create_file_with_content(&fs, parent, "deep.txt", b"10 levels deep");

    // Navigate back down and verify
    let mut cur = 1u64;
    for level in 0..10 {
        cur = fs.meta().lookup(cur, &format!("level_{}", level)).unwrap();
    }
    let found = fs.meta().lookup(cur, "deep.txt").unwrap();
    assert_eq!(found, ino);

    let content = fs.test_read(ino, 0, 1024).unwrap();
    assert_eq!(content, b"10 levels deep");
}

#[test]
fn test_edge_many_files_in_one_directory() {
    let (fs, _dir) = fresh_fs();

    let dir_ino = fs
        .simulate_mkdir(1, "bigdir", S_IFDIR | 0o755, 0o022, 0, 0)
        .unwrap();

    // Create 100 files
    let mut inodes = Vec::new();
    for i in 0..100 {
        let name = format!("file_{:03}.txt", i);
        let content = format!("content of file {}", i);
        let ino = create_file_with_content(&fs, dir_ino, &name, content.as_bytes());
        inodes.push(ino);
    }

    // Readdir should have 100 files + . and ..
    let entries = fs.test_readdir(dir_ino, 0).unwrap();
    assert_eq!(entries.len(), 102, "100 files + . + ..");

    // Spot check a few files
    for i in [0, 25, 50, 75, 99] {
        let name = format!("file_{:03}.txt", i);
        let expected_content = format!("content of file {}", i);
        let found_ino = fs.meta().lookup(dir_ino, &name).unwrap();
        assert_eq!(found_ino, inodes[i]);
        let content = fs.test_read(found_ino, 0, 1024).unwrap();
        assert_eq!(content, expected_content.as_bytes());
    }
}

#[test]
fn test_edge_read_at_various_offsets() {
    let (fs, _dir) = fresh_fs();
    let content = b"0123456789ABCDEF";
    let ino = create_file_with_content(&fs, 1, "offsets.txt", content);

    // Read from offset 0
    let r = fs.test_read(ino, 0, 16).unwrap();
    assert_eq!(r, content);

    // Read from middle
    let r = fs.test_read(ino, 5, 5).unwrap();
    assert_eq!(r, b"56789");

    // Read past end -- should return less data
    let r = fs.test_read(ino, 14, 10).unwrap();
    assert_eq!(r, b"EF");

    // Read at exact end -- should return empty
    let r = fs.test_read(ino, 16, 10).unwrap();
    assert!(r.is_empty());

    // Read beyond end -- should return empty
    let r = fs.test_read(ino, 100, 10).unwrap();
    assert!(r.is_empty());
}

#[test]
fn test_edge_zero_size_read() {
    let (fs, _dir) = fresh_fs();
    let ino = create_file_with_content(&fs, 1, "noread.txt", b"some content");

    let r = fs.test_read(ino, 0, 0).unwrap();
    assert!(r.is_empty(), "zero-size read must return empty");
}

#[test]
fn test_edge_write_read_large_file_128kb() {
    let (fs, _dir) = fresh_fs();

    // Generate 128 KB of patterned data
    let size = 128 * 1024;
    let content: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();

    let (ino, fh) = fs
        .test_create(1, "big.bin", S_IFREG | 0o644, 0o022, 0, 0)
        .unwrap();

    // Write in 4 KB chunks
    for chunk_start in (0..size).step_by(4096) {
        let chunk_end = std::cmp::min(chunk_start + 4096, size);
        fs.test_write(fh, chunk_start as u64, &content[chunk_start..chunk_end])
            .unwrap();
    }
    fs.test_release(ino, fh).unwrap();

    let readback = fs.test_read(ino, 0, size as u32).unwrap();
    assert_eq!(readback.len(), size);
    assert_eq!(
        readback, content,
        "128 KB file must round-trip byte-for-byte"
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 12. Lookup and getattr E2E
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn test_lookup_returns_correct_inode_metadata() {
    let (fs, _dir) = fresh_fs();
    let ino = create_file_with_content(&fs, 1, "lookup_me.txt", b"data");

    let (found_ino, meta) = fs.test_lookup(1, "lookup_me.txt").unwrap();
    assert_eq!(found_ino, ino);
    assert_eq!(meta.uid, 1000);
    assert_eq!(meta.gid, 1000);
    assert_eq!(meta.mode & 0o7777, 0o644);
}

#[test]
fn test_lookup_nonexistent_returns_error() {
    let (fs, _dir) = fresh_fs();
    let result = fs.test_lookup(1, "ghost.txt");
    assert!(result.is_err());
}

#[test]
fn test_getattr_returns_correct_size_after_write() {
    let (fs, _dir) = fresh_fs();
    let ino = create_file_with_content(&fs, 1, "sized.txt", b"twelve chars");

    let meta = fs.test_getattr(ino).unwrap();
    assert_eq!(meta.size, 12);
    assert_eq!(meta.mode & S_IFREG, S_IFREG);
}

// ════════════════════════════════════════════════════════════════════════════
// 13. Full workflow: create directory tree, populate, navigate, modify, verify
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn test_full_workflow_project_directory() {
    let (fs, _dir) = fresh_fs();

    // Create a project-like directory structure
    let src = fs
        .simulate_mkdir(1, "src", S_IFDIR | 0o755, 0o022, 1000, 1000)
        .unwrap();
    let tests = fs
        .simulate_mkdir(1, "tests", S_IFDIR | 0o755, 0o022, 1000, 1000)
        .unwrap();

    // Create source files
    let main_ino = create_file_with_content(&fs, src, "main.rs", b"fn main() {}");
    let lib_ino = create_file_with_content(&fs, src, "lib.rs", b"pub mod utils;");
    let _test_ino =
        create_file_with_content(&fs, tests, "test_main.rs", b"#[test] fn it_works() {}");

    // Create root files
    let _cargo_ino =
        create_file_with_content(&fs, 1, "Cargo.toml", b"[package]\nname = \"myproj\"");
    let _readme_ino = create_file_with_content(&fs, 1, "README.md", b"# My Project");

    // Verify root directory listing
    let root_entries = fs.test_readdir(1, 0).unwrap();
    let root_names: Vec<&str> = root_entries.iter().map(|e| e.2.as_str()).collect();
    assert!(root_names.contains(&"src"));
    assert!(root_names.contains(&"tests"));
    assert!(root_names.contains(&"Cargo.toml"));
    assert!(root_names.contains(&"README.md"));

    // Verify src directory listing
    let src_entries = fs.test_readdir(src, 0).unwrap();
    let src_names: Vec<&str> = src_entries.iter().map(|e| e.2.as_str()).collect();
    assert!(src_names.contains(&"main.rs"));
    assert!(src_names.contains(&"lib.rs"));

    // Modify main.rs
    let (fh, _) = fs
        .test_open(main_ino, libc::O_WRONLY | libc::O_TRUNC)
        .unwrap();
    fs.test_write(fh, 0, b"fn main() { println!(\"hello\"); }")
        .unwrap();
    fs.test_release(main_ino, fh).unwrap();

    let content = fs.test_read(main_ino, 0, 1024).unwrap();
    assert_eq!(content, b"fn main() { println!(\"hello\"); }");

    // Rename lib.rs to utils.rs
    fs.simulate_rename(src, "lib.rs", src, "utils.rs", 0)
        .unwrap();
    assert!(fs.meta().lookup(src, "lib.rs").is_err());
    let utils_ino = fs.meta().lookup(src, "utils.rs").unwrap();
    assert_eq!(utils_ino, lib_ino);

    // Delete test file
    let test_ino = fs.meta().lookup(tests, "test_main.rs").unwrap();
    fs.simulate_unlink(tests, "test_main.rs").unwrap();
    assert!(fs.meta().lookup(tests, "test_main.rs").is_err());
    // With nlinks=1, the inode is deleted
    assert!(fs.meta().get_inode(test_ino).is_err());
}

// ════════════════════════════════════════════════════════════════════════════
// 14. Truncate E2E workflows
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn test_truncate_to_zero_and_rewrite() {
    let (fs, _dir) = fresh_fs();
    let ino = create_file_with_content(&fs, 1, "trunc.txt", b"old content that is long");

    // Truncate to 0 via setattr on closed file
    fs.test_setattr_size(ino, None, 0).unwrap();
    assert_eq!(fs.meta().get_inode(ino).unwrap().size, 0);

    let empty = fs.test_read(ino, 0, 1024).unwrap();
    assert!(empty.is_empty());

    // Rewrite
    let (fh, _) = fs.test_open(ino, libc::O_WRONLY).unwrap();
    fs.test_write(fh, 0, b"fresh start").unwrap();
    fs.test_release(ino, fh).unwrap();

    let content = fs.test_read(ino, 0, 1024).unwrap();
    assert_eq!(content, b"fresh start");
}

#[test]
fn test_truncate_to_shorter_then_longer() {
    let (fs, _dir) = fresh_fs();
    let ino = create_file_with_content(&fs, 1, "resize.txt", b"hello world!!");

    // Truncate to 5
    fs.test_setattr_size(ino, None, 5).unwrap();
    let content = read_content(&fs, ino);
    assert_eq!(content, b"hello");

    // Extend to 10 (zero-padded)
    fs.test_setattr_size(ino, None, 10).unwrap();
    let content = read_content(&fs, ino);
    assert_eq!(content.len(), 10);
    assert_eq!(&content[..5], b"hello");
    assert!(content[5..].iter().all(|&b| b == 0));
}

// ════════════════════════════════════════════════════════════════════════════
// 15. Open / flush / release lifecycle
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn test_open_read_only_does_not_create_write_handle() {
    let (fs, _dir) = fresh_fs();
    let ino = create_file_with_content(&fs, 1, "readonly.txt", b"read me");

    let (fh, is_write) = fs.test_open(ino, libc::O_RDONLY).unwrap();
    assert!(!is_write, "O_RDONLY must not create a write handle");

    // Read should work
    let content = fs.test_read(ino, 0, 1024).unwrap();
    assert_eq!(content, b"read me");

    // Release should work (no-op for read-only handles)
    fs.test_release_full(ino, fh).unwrap();
}

#[test]
fn test_flush_then_release_preserves_content() {
    let (fs, _dir) = fresh_fs();

    let (ino, fh) = fs
        .test_create(1, "flush_test.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .unwrap();
    fs.test_write(fh, 0, b"flush me").unwrap();

    // Flush (like NFS CLOSE -> FUSE flush)
    fs.test_flush(ino, fh).unwrap();

    // Release
    fs.test_release(ino, fh).unwrap();

    let content = fs.test_read(ino, 0, 1024).unwrap();
    assert_eq!(content, b"flush me");
}

// ════════════════════════════════════════════════════════════════════════════
// 16. Seed + mount round-trip (seed -> load_store -> filesystem operations)
// ════════════════════════════════════════════════════════════════════════════

#[test]
fn test_seed_then_mount_and_modify() {
    let source_dir = TempDir::new().unwrap();
    let store_dir = TempDir::new().unwrap();

    // Create source content
    std::fs::write(source_dir.path().join("readme.txt"), b"original readme").unwrap();
    std::fs::create_dir(source_dir.path().join("docs")).unwrap();
    std::fs::write(source_dir.path().join("docs/guide.txt"), b"original guide").unwrap();

    // Seed
    run_seed(store_dir.path(), source_dir.path()).unwrap();

    // Load store (simulating mount)
    let segs_dir = store_dir.path().join("segments");
    let (root_opt, _) = load_store_from_segments(&segs_dir).unwrap();
    let root = root_opt.unwrap();
    let io = Arc::new(Mutex::new(StoreIo::new(store_dir.path())));
    let store = DictMetadataStore::load_from_root(io.clone(), &root).unwrap();

    // Wrap in filesystem for test operations
    let fs = SliceFsFilesystem::new(store, io, None);

    // Read seeded content
    let readme_ino = fs.meta().lookup(1, "readme.txt").unwrap();
    let readme_content = fs.test_read(readme_ino, 0, 1024).unwrap();
    assert_eq!(readme_content, b"original readme");

    // Navigate into seeded directory
    let docs_ino = fs.meta().lookup(1, "docs").unwrap();
    let guide_ino = fs.meta().lookup(docs_ino, "guide.txt").unwrap();
    let guide_content = fs.test_read(guide_ino, 0, 1024).unwrap();
    assert_eq!(guide_content, b"original guide");

    // Create new file after "mount"
    let new_ino = create_file_with_content(&fs, 1, "new_file.txt", b"post-mount content");
    let new_content = fs.test_read(new_ino, 0, 1024).unwrap();
    assert_eq!(new_content, b"post-mount content");

    // Modify seeded file
    let (fh, _) = fs
        .test_open(readme_ino, libc::O_WRONLY | libc::O_TRUNC)
        .unwrap();
    fs.test_write(fh, 0, b"updated readme").unwrap();
    fs.test_release(readme_ino, fh).unwrap();

    let updated = fs.test_read(readme_ino, 0, 1024).unwrap();
    assert_eq!(updated, b"updated readme");
}
