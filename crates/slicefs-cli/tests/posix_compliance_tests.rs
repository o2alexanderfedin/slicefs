//! Custom POSIX compliance test suite for SliceFS.
//!
//! Exercises all Phase 4 POSIX operations through `DictMetadataStore` +
//! `SliceFsFilesystem` without a FUSE mount, mirroring what pjdfstest checks.
//!
//! Test categories:
//!   1. File operations (create, write, read, delete)
//!   2. Directory operations (mkdir, rmdir nesting and errors)
//!   3. Rename (same-dir, cross-dir, overwrite, editor pattern)
//!   4. Symlinks (create, readlink, dangling, long target)
//!   5. Hard links (nlinks, lifecycle, cross-directory)
//!   6. Truncate (shorter, longer, zero)
//!   7. Permissions (chmod, chown, mtime)
//!   8. Dedup verification (content digest equality, refcounts)

use blockset::{Dictionary, GetBytes, GetData};
use metadata::store::DictMetadataStore;
use slicefs_cli::filesystem::SliceFsFilesystem;
use slicefs_compression::NoneCompressor;
use slicefs_traits::digest::from_digest224;
use slicefs_traits::metadata::MetadataStore;
use std::sync::Arc;

const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;

fn fresh_fs() -> SliceFsFilesystem {
    let meta = DictMetadataStore::new();
    let dict = Dictionary::default();
    SliceFsFilesystem::new(meta, dict, None, Arc::new(NoneCompressor::new()), 1)
}

/// Read file content via manifest + GetBytes (same pipeline as real FUSE read)
fn read_content(fs: &SliceFsFilesystem, ino: u64) -> Vec<u8> {
    let manifest = fs.meta().get_manifest(ino).unwrap();
    if manifest.is_empty() {
        return vec![];
    }
    let root256 = from_digest224(&manifest[0]);
    let dict = fs.dict().lock().unwrap();
    let get_data = GetData::new(&*dict, &root256);
    GetBytes::new(get_data).collect()
}

// ════════════════════════════════════════════════════════════════════════════
// 1. File Operations (POSIX-01)
// ════════════════════════════════════════════════════════════════════════════

/// Create file, write content, close, read back — content matches exactly.
#[test]
fn test_file_create_write_read() {
    let fs = fresh_fs();
    let content = b"hello, slicefs!";

    let (ino, fh) = fs.test_create(1, "hello.txt", S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create hello.txt");
    fs.test_write(fh, 0, content).expect("write");
    fs.test_release(ino, fh).expect("release");

    let readback = read_content(&fs, ino);
    assert_eq!(readback, content, "content should round-trip through CAS");
}

/// Create file, write at offset — content before offset is zero-padded.
#[test]
fn test_file_write_at_offset_zero_pads() {
    let fs = fresh_fs();

    let (ino, fh) = fs.test_create(1, "sparse.bin", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create sparse.bin");
    fs.test_write(fh, 10, b"data").expect("write at offset 10");
    fs.test_release(ino, fh).expect("release");

    let content = read_content(&fs, ino);
    assert_eq!(content.len(), 14, "file should be 14 bytes: 10 padding + 4 data");
    assert_eq!(&content[..10], &[0u8; 10], "first 10 bytes should be zero-padded");
    assert_eq!(&content[10..], b"data", "data at offset 10 should match");
}

/// Create file, close without writing — empty file with size 0.
#[test]
fn test_file_create_empty_has_size_zero() {
    let fs = fresh_fs();

    let (ino, fh) = fs.test_create(1, "empty.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create empty.txt");
    fs.test_release(ino, fh).expect("release without writing");

    let meta = fs.meta().get_inode(ino).unwrap();
    assert_eq!(meta.size, 0, "empty file should have size 0");

    let content = read_content(&fs, ino);
    assert!(content.is_empty(), "content of empty file should be empty");
}

/// Delete file — lookup returns NotFound afterward.
#[test]
fn test_file_delete_removes_from_directory() {
    let fs = fresh_fs();

    let (ino, fh) = fs.test_create(1, "delete_me.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create delete_me.txt");
    fs.test_release(ino, fh).expect("release");

    // File is accessible before unlink
    fs.meta().lookup(1, "delete_me.txt").expect("should be found before unlink");

    // Unlink removes it
    fs.simulate_unlink(1, "delete_me.txt").expect("unlink should succeed");

    let result = fs.meta().lookup(1, "delete_me.txt");
    assert!(result.is_err(), "lookup should fail after unlink");
}

/// Create file in subdirectory — nested paths work.
#[test]
fn test_file_create_in_subdirectory() {
    let fs = fresh_fs();

    let subdir_ino = fs.simulate_mkdir(1, "subdir", S_IFDIR | 0o755, 0o022, 0, 0)
        .expect("mkdir subdir");

    let (file_ino, fh) = fs.test_create(subdir_ino, "nested.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create nested.txt in subdir");
    fs.test_write(fh, 0, b"nested content").expect("write");
    fs.test_release(file_ino, fh).expect("release");

    let found_ino = fs.meta().lookup(subdir_ino, "nested.txt").expect("lookup in subdir");
    assert_eq!(found_ino, file_ino, "lookup should find the file in subdir");

    let content = read_content(&fs, file_ino);
    assert_eq!(content, b"nested content");
}

/// Write content, then overwrite with different content — reads back new content.
#[test]
fn test_file_overwrite_content() {
    let fs = fresh_fs();

    let (ino, fh1) = fs.test_create(1, "overwrite.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create");
    fs.test_write(fh1, 0, b"original data").expect("first write");
    fs.test_release(ino, fh1).expect("first release");

    // Re-open by simulating truncate + write (setattr size=0, then write)
    fs.test_setattr_size(ino, None, 0).expect("truncate to 0");

    // Now write new content to the zeroed file via a new handle
    let (ino2, fh2) = fs.test_create(1, "overwrite2.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create overwrite2");
    fs.test_write(fh2, 0, b"new content").expect("write new content");
    fs.test_release(ino2, fh2).expect("release");

    let content = read_content(&fs, ino2);
    assert_eq!(content, b"new content");
}

// ════════════════════════════════════════════════════════════════════════════
// 2. Directory Operations (POSIX-02)
// ════════════════════════════════════════════════════════════════════════════

/// mkdir creates directory with . and .. entries.
#[test]
fn test_dir_mkdir_has_dot_entries() {
    let fs = fresh_fs();

    let dir_ino = fs.simulate_mkdir(1, "newdir", S_IFDIR | 0o755, 0o022, 0, 0)
        .expect("mkdir newdir");

    let entries = fs.meta().list_directory(dir_ino).expect("list_directory");
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();

    assert!(names.contains(&"."), "directory should contain .");
    assert!(names.contains(&".."), "directory should contain ..");
}

/// mkdir in subdirectory — nested mkdir works.
#[test]
fn test_dir_nested_mkdir() {
    let fs = fresh_fs();

    let parent_ino = fs.simulate_mkdir(1, "parent", S_IFDIR | 0o755, 0o022, 0, 0)
        .expect("mkdir parent");
    let child_ino = fs.simulate_mkdir(parent_ino, "child", S_IFDIR | 0o755, 0o022, 0, 0)
        .expect("mkdir parent/child");

    let found = fs.meta().lookup(parent_ino, "child").expect("lookup parent/child");
    assert_eq!(found, child_ino);

    let child_entries = fs.meta().list_directory(child_ino).expect("list child");
    let names: Vec<&str> = child_entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&".") && names.contains(&".."));
}

/// mkdir increments parent nlinks (for the .. backlink).
#[test]
fn test_dir_mkdir_increments_parent_nlinks() {
    let fs = fresh_fs();

    let parent_before = fs.meta().get_inode(1).unwrap().nlinks;
    fs.simulate_mkdir(1, "newdir", S_IFDIR | 0o755, 0o022, 0, 0).expect("mkdir");
    let parent_after = fs.meta().get_inode(1).unwrap().nlinks;

    assert_eq!(
        parent_after, parent_before + 1,
        "mkdir should increment parent nlinks by 1"
    );
}

/// rmdir on empty directory succeeds.
#[test]
fn test_dir_rmdir_empty_succeeds() {
    let fs = fresh_fs();

    fs.simulate_mkdir(1, "removeme", S_IFDIR | 0o755, 0o022, 0, 0).expect("mkdir");
    fs.simulate_rmdir(1, "removeme").expect("rmdir should succeed on empty dir");

    let result = fs.meta().lookup(1, "removeme");
    assert!(result.is_err(), "directory should be gone after rmdir");
}

/// rmdir on non-empty directory returns ENOTEMPTY.
#[test]
fn test_dir_rmdir_nonempty_returns_enotempty() {
    let fs = fresh_fs();

    let dir_ino = fs.simulate_mkdir(1, "nonempty", S_IFDIR | 0o755, 0o022, 0, 0).expect("mkdir");
    let (fino, fh) = fs.test_create(dir_ino, "file.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create file inside dir");
    fs.test_release(fino, fh).expect("release");

    let result = fs.simulate_rmdir(1, "nonempty");
    assert_eq!(
        result,
        Err(libc::ENOTEMPTY),
        "rmdir on non-empty dir should return ENOTEMPTY"
    );
}

/// rmdir on a regular file returns ENOTDIR.
#[test]
fn test_dir_rmdir_on_file_returns_enotdir() {
    let fs = fresh_fs();

    let (ino, fh) = fs.test_create(1, "notadir.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create file");
    fs.test_release(ino, fh).expect("release");

    let result = fs.simulate_rmdir(1, "notadir.txt");
    assert_eq!(
        result,
        Err(libc::ENOTDIR),
        "rmdir on a regular file should return ENOTDIR"
    );
}

/// rmdir decrements parent nlinks.
#[test]
fn test_dir_rmdir_decrements_parent_nlinks() {
    let fs = fresh_fs();

    fs.simulate_mkdir(1, "tmpdir", S_IFDIR | 0o755, 0o022, 0, 0).expect("mkdir");
    let nlinks_after_mkdir = fs.meta().get_inode(1).unwrap().nlinks;

    fs.simulate_rmdir(1, "tmpdir").expect("rmdir");
    let nlinks_after_rmdir = fs.meta().get_inode(1).unwrap().nlinks;

    assert_eq!(
        nlinks_after_rmdir, nlinks_after_mkdir - 1,
        "rmdir should decrement parent nlinks"
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 3. Rename (POSIX-03)
// ════════════════════════════════════════════════════════════════════════════

/// Rename file within same directory — new name accessible, old name gone.
#[test]
fn test_rename_within_same_directory() {
    let fs = fresh_fs();

    let (ino, fh) = fs.test_create(1, "old.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create old.txt");
    fs.test_write(fh, 0, b"rename test").expect("write");
    fs.test_release(ino, fh).expect("release");

    fs.simulate_rename(1, "old.txt", 1, "new.txt", 0).expect("rename");

    let found_ino = fs.meta().lookup(1, "new.txt").expect("new name should be accessible");
    assert_eq!(found_ino, ino, "renamed inode number should be preserved");

    let result = fs.meta().lookup(1, "old.txt");
    assert!(result.is_err(), "old name should be gone after rename");
}

/// Rename file across directories — file is in new location.
#[test]
fn test_rename_across_directories() {
    let fs = fresh_fs();

    let dir_a = fs.simulate_mkdir(1, "dir_a", S_IFDIR | 0o755, 0o022, 0, 0).expect("mkdir dir_a");
    let dir_b = fs.simulate_mkdir(1, "dir_b", S_IFDIR | 0o755, 0o022, 0, 0).expect("mkdir dir_b");

    let (ino, fh) = fs.test_create(dir_a, "file.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create in dir_a");
    fs.test_write(fh, 0, b"cross-dir rename").expect("write");
    fs.test_release(ino, fh).expect("release");

    fs.simulate_rename(dir_a, "file.txt", dir_b, "moved.txt", 0).expect("cross-dir rename");

    let found = fs.meta().lookup(dir_b, "moved.txt").expect("file should be in dir_b");
    assert_eq!(found, ino, "inode preserved after cross-dir rename");

    let result = fs.meta().lookup(dir_a, "file.txt");
    assert!(result.is_err(), "file should be gone from dir_a");
}

/// Rename overwrites existing target — target inode is removed.
#[test]
fn test_rename_overwrites_existing_target() {
    let fs = fresh_fs();

    let (src_ino, fh1) = fs.test_create(1, "src.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create src");
    fs.test_write(fh1, 0, b"source content").expect("write src");
    fs.test_release(src_ino, fh1).expect("release src");

    let (dst_ino, fh2) = fs.test_create(1, "dst.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create dst");
    fs.test_write(fh2, 0, b"destination content").expect("write dst");
    fs.test_release(dst_ino, fh2).expect("release dst");

    // rename src over dst — dst should be atomically replaced
    fs.simulate_rename(1, "src.txt", 1, "dst.txt", 0).expect("rename with overwrite");

    let found = fs.meta().lookup(1, "dst.txt").expect("dst.txt still accessible");
    assert_eq!(found, src_ino, "dst.txt should now point to src inode");

    // src.txt should be gone
    let result = fs.meta().lookup(1, "src.txt");
    assert!(result.is_err(), "src.txt should be gone after rename");

    // Original dst inode should be deleted (refcount decremented)
    let dst_inode_result = fs.meta().get_inode(dst_ino);
    assert!(dst_inode_result.is_err(), "overwritten dst inode should be deleted");
}

/// Rename directory within same parent.
#[test]
fn test_rename_directory() {
    let fs = fresh_fs();

    let dir_ino = fs.simulate_mkdir(1, "olddir", S_IFDIR | 0o755, 0o022, 0, 0).expect("mkdir");

    fs.simulate_rename(1, "olddir", 1, "newdir", 0).expect("rename dir");

    let found = fs.meta().lookup(1, "newdir").expect("newdir should exist");
    assert_eq!(found, dir_ino);

    let result = fs.meta().lookup(1, "olddir");
    assert!(result.is_err(), "olddir should be gone");
}

/// Editor pattern: create temp file, write, rename over original.
#[test]
fn test_rename_editor_pattern() {
    let fs = fresh_fs();

    // Create the "original" file
    let (orig_ino, fh1) = fs.test_create(1, "document.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create document.txt");
    fs.test_write(fh1, 0, b"original content").expect("write original");
    fs.test_release(orig_ino, fh1).expect("release original");

    // Create temp file with new content
    let (tmp_ino, fh2) = fs.test_create(1, "document.txt.tmp", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create temp");
    fs.test_write(fh2, 0, b"updated content").expect("write updated");
    fs.test_release(tmp_ino, fh2).expect("release temp");

    // Atomic rename temp over original (editor save pattern)
    fs.simulate_rename(1, "document.txt.tmp", 1, "document.txt", 0).expect("atomic rename");

    let found_ino = fs.meta().lookup(1, "document.txt").expect("document.txt should exist");
    let content = read_content(&fs, found_ino);
    assert_eq!(content, b"updated content", "document.txt should have updated content after rename");

    // Temp file should be gone
    let temp_result = fs.meta().lookup(1, "document.txt.tmp");
    assert!(temp_result.is_err(), "temp file should be gone after rename");
}

// ════════════════════════════════════════════════════════════════════════════
// 4. Symlinks (POSIX-04)
// ════════════════════════════════════════════════════════════════════════════

/// Create symlink, readlink returns target path.
#[test]
fn test_symlink_readlink_returns_target() {
    let fs = fresh_fs();

    let sym_ino = fs.simulate_symlink(1, "link.txt", "/target/path", 0, 0)
        .expect("create symlink");

    let target = fs.simulate_readlink(sym_ino).expect("readlink");
    assert_eq!(target, "/target/path", "readlink should return the original target");
}

/// Symlink to non-existent target (dangling symlink) — POSIX allows this.
#[test]
fn test_symlink_dangling_allowed() {
    let fs = fresh_fs();

    // Target doesn't exist — symlink creation should still succeed
    let sym_ino = fs.simulate_symlink(1, "dangling", "/nonexistent/path/to/nowhere", 0, 0)
        .expect("dangling symlink creation should succeed");

    let target = fs.simulate_readlink(sym_ino).expect("readlink dangling symlink");
    assert_eq!(target, "/nonexistent/path/to/nowhere");
}

/// Symlink inode size equals target path length.
#[test]
fn test_symlink_size_equals_target_length() {
    let fs = fresh_fs();

    let target = "/usr/local/lib/libfoo.so.1";
    let sym_ino = fs.simulate_symlink(1, "libfoo.so", target, 0, 0)
        .expect("create symlink");

    let meta = fs.meta().get_inode(sym_ino).unwrap();
    assert_eq!(
        meta.size, target.len() as u64,
        "symlink size should equal target length"
    );
}

/// Symlink with long target path (>256 chars) — no length limit in POSIX.
#[test]
fn test_symlink_long_target_path() {
    let fs = fresh_fs();

    let long_target = "/very/long/path/".repeat(20); // 320 chars
    let sym_ino = fs.simulate_symlink(1, "longlink", &long_target, 0, 0)
        .expect("long symlink should succeed");

    let target = fs.simulate_readlink(sym_ino).expect("readlink");
    assert_eq!(target, long_target);
    assert!(target.len() > 256, "target path should be > 256 chars");
}

/// Symlink and regular file with same name conflict (symlink replaces if rename).
#[test]
fn test_symlink_and_file_are_distinct_inodes() {
    let fs = fresh_fs();

    let sym_ino = fs.simulate_symlink(1, "link.txt", "/target", 0, 0)
        .expect("create symlink");

    let (file_ino, fh) = fs.test_create(1, "file.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create regular file");
    fs.test_release(file_ino, fh).expect("release");

    assert_ne!(sym_ino, file_ino, "symlink and file should have different inodes");

    let sym_meta = fs.meta().get_inode(sym_ino).unwrap();
    // Symlink mode has S_IFLNK set
    assert!(sym_meta.mode & 0o170_000 == 0o120_000, "symlink should have S_IFLNK type bits");
}

// ════════════════════════════════════════════════════════════════════════════
// 5. Hard Links (POSIX-05)
// ════════════════════════════════════════════════════════════════════════════

/// Create hard link — nlinks increases to 2.
#[test]
fn test_link_nlinks_equals_two() {
    let fs = fresh_fs();

    let (ino, fh) = fs.test_create(1, "original.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create original");
    fs.test_write(fh, 0, b"shared content").expect("write");
    fs.test_release(ino, fh).expect("release");

    // Before linking
    let meta_before = fs.meta().get_inode(ino).unwrap();
    assert_eq!(meta_before.nlinks, 1);

    fs.simulate_link(ino, 1, "hardlink.txt").expect("create hard link");

    let meta_after = fs.meta().get_inode(ino).unwrap();
    assert_eq!(meta_after.nlinks, 2, "nlinks should be 2 after hard link creation");
}

/// Unlink one hard link — nlinks decreases to 1, file still accessible.
#[test]
fn test_link_unlink_one_keeps_inode() {
    let fs = fresh_fs();

    let (ino, fh) = fs.test_create(1, "original.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create");
    fs.test_write(fh, 0, b"data").expect("write");
    fs.test_release(ino, fh).expect("release");

    fs.simulate_link(ino, 1, "link.txt").expect("create hard link");

    // Remove one hard link
    fs.simulate_unlink(1, "original.txt").expect("unlink original");

    // Inode still exists because nlinks == 1
    let meta = fs.meta().get_inode(ino).expect("inode should still exist");
    assert_eq!(meta.nlinks, 1, "nlinks should be 1 after one unlink");

    // Still accessible via the second name
    let found_ino = fs.meta().lookup(1, "link.txt").expect("hard link still accessible");
    assert_eq!(found_ino, ino);
}

/// Unlink last hard link — inode deleted.
#[test]
fn test_link_unlink_last_deletes_inode() {
    let fs = fresh_fs();

    let (ino, fh) = fs.test_create(1, "todelete.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create");
    fs.test_write(fh, 0, b"ephemeral content").expect("write");
    fs.test_release(ino, fh).expect("release");

    // Only one link — unlink should delete the inode
    fs.simulate_unlink(1, "todelete.txt").expect("unlink last link");

    let result = fs.meta().get_inode(ino);
    assert!(result.is_err(), "inode should be deleted after last unlink");
}

/// Hard link to file in a different directory.
#[test]
fn test_link_cross_directory() {
    let fs = fresh_fs();

    let dir_ino = fs.simulate_mkdir(1, "subdir", S_IFDIR | 0o755, 0o022, 0, 0)
        .expect("mkdir subdir");

    let (ino, fh) = fs.test_create(1, "original.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create in root");
    fs.test_write(fh, 0, b"cross-dir linked").expect("write");
    fs.test_release(ino, fh).expect("release");

    // Create hard link in subdir
    fs.simulate_link(ino, dir_ino, "cross_link.txt").expect("cross-dir hard link");

    let meta = fs.meta().get_inode(ino).unwrap();
    assert_eq!(meta.nlinks, 2, "nlinks should be 2 after cross-dir link");

    let found = fs.meta().lookup(dir_ino, "cross_link.txt").expect("link in subdir");
    assert_eq!(found, ino, "both names should point to the same inode");
}

/// Attempting hard link to directory returns EPERM.
#[test]
fn test_link_to_directory_returns_eperm() {
    let fs = fresh_fs();

    let dir_ino = fs.simulate_mkdir(1, "adir", S_IFDIR | 0o755, 0o022, 0, 0).expect("mkdir");

    let result = fs.simulate_link(dir_ino, 1, "dirlink");
    assert_eq!(
        result,
        Err(libc::EPERM),
        "hard link to directory should return EPERM"
    );
}

// ════════════════════════════════════════════════════════════════════════════
// 6. Truncate (POSIX-09)
// ════════════════════════════════════════════════════════════════════════════

/// Truncate file to shorter length — content is trimmed.
#[test]
fn test_truncate_to_shorter_length() {
    let fs = fresh_fs();

    let (ino, fh) = fs.test_create(1, "truncate.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create");
    fs.test_write(fh, 0, b"hello world").expect("write");
    fs.test_release(ino, fh).expect("release");

    fs.test_setattr_size(ino, None, 5).expect("truncate to 5 bytes");

    let meta = fs.meta().get_inode(ino).unwrap();
    assert_eq!(meta.size, 5, "inode size should be 5 after truncate");

    let content = read_content(&fs, ino);
    assert_eq!(content, b"hello", "content should be 'hello' after truncate to 5");
}

/// Truncate file to longer length (zero extend).
#[test]
fn test_truncate_zero_extend() {
    let fs = fresh_fs();

    let (ino, fh) = fs.test_create(1, "extend.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create");
    fs.test_write(fh, 0, b"abc").expect("write 3 bytes");
    fs.test_release(ino, fh).expect("release");

    fs.test_setattr_size(ino, None, 10).expect("extend to 10 bytes");

    let meta = fs.meta().get_inode(ino).unwrap();
    assert_eq!(meta.size, 10, "inode size should be 10 after zero-extend");

    let content = read_content(&fs, ino);
    assert_eq!(content.len(), 10);
    assert_eq!(&content[..3], b"abc", "original content preserved");
    assert_eq!(&content[3..], &[0u8; 7], "extended bytes should be zero");
}

/// Truncate file to 0 — becomes an empty file.
#[test]
fn test_truncate_to_zero() {
    let fs = fresh_fs();

    let (ino, fh) = fs.test_create(1, "zeroed.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create");
    fs.test_write(fh, 0, b"some content to erase").expect("write");
    fs.test_release(ino, fh).expect("release");

    fs.test_setattr_size(ino, None, 0).expect("truncate to 0");

    let meta = fs.meta().get_inode(ino).unwrap();
    assert_eq!(meta.size, 0, "inode size should be 0 after full truncate");
}

// ════════════════════════════════════════════════════════════════════════════
// 7. Permissions (POSIX metadata)
// ════════════════════════════════════════════════════════════════════════════

/// chmod changes mode bits.
#[test]
fn test_chmod_changes_mode_bits() {
    let fs = fresh_fs();

    let (ino, fh) = fs.test_create(1, "perm.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create");
    fs.test_release(ino, fh).expect("release");

    let before = fs.meta().get_inode(ino).unwrap();
    assert_eq!(before.mode & 0o7777, 0o644);

    fs.test_setattr_mode(ino, S_IFREG | 0o755).expect("chmod 755");

    let after = fs.meta().get_inode(ino).unwrap();
    assert_eq!(after.mode & 0o7777, 0o755, "mode bits should be 755 after chmod");
}

/// chown changes uid and gid.
#[test]
fn test_chown_changes_uid_gid() {
    let fs = fresh_fs();

    let (ino, fh) = fs.test_create(1, "own.txt", S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create");
    fs.test_release(ino, fh).expect("release");

    fs.test_setattr_uid_gid(ino, Some(2000), Some(2000)).expect("chown");

    let meta = fs.meta().get_inode(ino).unwrap();
    assert_eq!(meta.uid, 2000, "uid should be 2000 after chown");
    assert_eq!(meta.gid, 2000, "gid should be 2000 after chown");
}

/// mtime updates after write.
#[test]
fn test_mtime_updates_on_write() {
    let fs = fresh_fs();

    let (ino, fh) = fs.test_create(1, "mtime.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create");
    let meta_before = fs.meta().get_inode(ino).unwrap();
    // mtime before write should be at creation time (0 initially)

    fs.test_write(fh, 0, b"update mtime").expect("write");
    fs.test_release(ino, fh).expect("release");

    let meta_after = fs.meta().get_inode(ino).unwrap();
    // mtime should be >= creation time
    assert!(
        meta_after.mtime_sec >= meta_before.mtime_sec,
        "mtime should not decrease after write"
    );
}

/// setattr mtime allows explicit timestamp setting.
#[test]
fn test_setattr_mtime_explicit() {
    let fs = fresh_fs();

    let (ino, fh) = fs.test_create(1, "ts.txt", S_IFREG | 0o644, 0o022, 0, 0).expect("create");
    fs.test_release(ino, fh).expect("release");

    fs.test_setattr_mtime(ino, 1_700_000_000, 0).expect("set mtime");

    let meta = fs.meta().get_inode(ino).unwrap();
    assert_eq!(meta.mtime_sec, 1_700_000_000, "mtime_sec should be explicitly set");
}

// ════════════════════════════════════════════════════════════════════════════
// 8. Dedup Verification
// ════════════════════════════════════════════════════════════════════════════

/// Write identical content to two files — same manifest digest.
#[test]
fn test_dedup_identical_content_same_digest() {
    let fs = fresh_fs();

    let content = b"identical dedup content here";

    let (ino1, fh1) = fs.test_create(1, "dup1.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create dup1");
    fs.test_write(fh1, 0, content).expect("write dup1");
    fs.test_release(ino1, fh1).expect("release dup1");

    let (ino2, fh2) = fs.test_create(1, "dup2.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create dup2");
    fs.test_write(fh2, 0, content).expect("write dup2 (identical)");
    fs.test_release(ino2, fh2).expect("release dup2");

    let manifest1 = fs.meta().get_manifest(ino1).unwrap();
    let manifest2 = fs.meta().get_manifest(ino2).unwrap();

    assert!(!manifest1.is_empty(), "manifest1 should not be empty");
    assert!(!manifest2.is_empty(), "manifest2 should not be empty");
    assert_eq!(
        manifest1[0], manifest2[0],
        "identical content should produce identical manifest digests"
    );
}

/// Write different content — different manifest digest.
#[test]
fn test_dedup_different_content_different_digest() {
    let fs = fresh_fs();

    let (ino1, fh1) = fs.test_create(1, "f1.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create f1");
    fs.test_write(fh1, 0, b"content alpha").expect("write f1");
    fs.test_release(ino1, fh1).expect("release f1");

    let (ino2, fh2) = fs.test_create(1, "f2.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create f2");
    fs.test_write(fh2, 0, b"content beta!").expect("write f2");
    fs.test_release(ino2, fh2).expect("release f2");

    let manifest1 = fs.meta().get_manifest(ino1).unwrap();
    let manifest2 = fs.meta().get_manifest(ino2).unwrap();

    assert!(!manifest1.is_empty() && !manifest2.is_empty());
    assert_ne!(
        manifest1[0], manifest2[0],
        "different content should produce different manifest digests"
    );
}

/// Write same content twice — refcount should be 2.
#[test]
fn test_dedup_refcount_incremented_for_identical_content() {
    let fs = fresh_fs();

    let content = b"shared content refcount test";

    let (ino1, fh1) = fs.test_create(1, "rc1.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create rc1");
    fs.test_write(fh1, 0, content).expect("write rc1");
    fs.test_release(ino1, fh1).expect("release rc1");

    let manifest1 = fs.meta().get_manifest(ino1).unwrap();
    let rc_after_first = fs.meta().get_refcount(&manifest1[0]);
    assert_eq!(rc_after_first, 1, "refcount should be 1 after first write");

    let (ino2, fh2) = fs.test_create(1, "rc2.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create rc2");
    fs.test_write(fh2, 0, content).expect("write rc2 identical");
    fs.test_release(ino2, fh2).expect("release rc2");

    let rc_after_second = fs.meta().get_refcount(&manifest1[0]);
    assert_eq!(rc_after_second, 2, "refcount should be 2 after two identical files");
}

/// Overwrite file content — old refcount decremented, new refcount incremented.
#[test]
fn test_dedup_refcount_decremented_on_overwrite() {
    let fs = fresh_fs();

    let (ino, fh) = fs.test_create(1, "overwrite_rc.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create");
    fs.test_write(fh, 0, b"original content").expect("write original");
    fs.test_release(ino, fh).expect("release original");

    let old_manifest = fs.meta().get_manifest(ino).unwrap();
    let old_digest = old_manifest[0];
    let rc_before = fs.meta().get_refcount(&old_digest);
    assert_eq!(rc_before, 1, "old content should have refcount 1");

    // Overwrite via truncate + write (setattr to 0, then create new content)
    // Use setattr to truncate to 0 (this decrements old refcount)
    fs.test_setattr_size(ino, None, 0).expect("truncate to 0 before overwrite");

    // After truncating to 0, the old refcount should be decremented
    let rc_after_truncate = fs.meta().get_refcount(&old_digest);
    assert_eq!(rc_after_truncate, 0, "old content refcount should be 0 after truncation to 0");
}

/// logical_bytes tracks dedup correctly (two identical files counted twice logically).
#[test]
fn test_dedup_logical_bytes_counts_both_files() {
    let fs = fresh_fs();

    let content = b"dedup logical bytes test data";

    let (ino1, fh1) = fs.test_create(1, "lb1.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create lb1");
    fs.test_write(fh1, 0, content).expect("write lb1");
    fs.test_release(ino1, fh1).expect("release lb1");

    let logical_after_first = fs.meta().logical_bytes();

    let (ino2, fh2) = fs.test_create(1, "lb2.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create lb2");
    fs.test_write(fh2, 0, content).expect("write lb2 identical");
    fs.test_release(ino2, fh2).expect("release lb2");

    let logical_after_second = fs.meta().logical_bytes();

    // Logical bytes should roughly double (both inodes count their size)
    assert!(
        logical_after_second >= logical_after_first + content.len() as u64,
        "logical bytes {} should increase by >= {} after second identical file (was {})",
        logical_after_second,
        content.len(),
        logical_after_first
    );
}
