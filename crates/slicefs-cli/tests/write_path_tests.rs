//! Integration tests for the SliceFS write path.
//!
//! Tests create/write/release pipeline, buffer mechanics, CAS flush,
//! refcount management, deduplication, and setattr truncate.
//!
//! These tests use `DictMetadataStore` and `Dictionary` directly —
//! no FUSE mount required.

use blockset::{Dictionary, GetBytes, GetData};
use metadata::store::DictMetadataStore;
use slicefs_cli::filesystem::{SliceFsFilesystem, inode_to_file_attr};
use slicefs_compression::NoneCompressor;
use slicefs_traits::digest::from_digest224;
use slicefs_traits::metadata::MetadataStore;
use std::sync::Arc;

const S_IFREG: u32 = 0o100_000;

fn fresh_fs() -> SliceFsFilesystem {
    let meta = DictMetadataStore::new();
    let dict = Dictionary::default();
    SliceFsFilesystem::new(meta, dict, None, Arc::new(NoneCompressor::new()), 1)
}

/// Read file content via manifest + GetBytes
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

// ── Task 1 tests ──────────────────────────────────────────────────────────────

#[test]
fn test_create_produces_inode_in_parent_dir() {
    let fs = fresh_fs();
    let (ino, fh) = fs.test_create(1, "hello.txt", S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create should succeed");

    // Inode must exist
    let meta = fs.meta().get_inode(ino).unwrap();
    assert_eq!(meta.uid, 1000);
    assert_eq!(meta.gid, 1000);

    // Directory entry must exist
    let child_ino = fs.meta().lookup(1, "hello.txt").unwrap();
    assert_eq!(child_ino, ino);

    // Write handle must be > 0
    assert!(fh > 0);
}

#[test]
fn test_create_returns_valid_attr() {
    let fs = fresh_fs();
    let (ino, _fh) = fs.test_create(1, "file.txt", S_IFREG | 0o644, 0o022, 500, 500)
        .expect("create should succeed");
    let meta = fs.meta().get_inode(ino).unwrap();
    let attr = inode_to_file_attr(&meta);
    assert_eq!(attr.ino.0, ino);
    assert_eq!(attr.uid, 500);
    assert_eq!(attr.gid, 500);
}

#[test]
fn test_write_sequential_produces_correct_buffer() {
    let fs = fresh_fs();
    let (ino, fh) = fs.test_create(1, "seq.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create should succeed");

    // write "hello" at offset 0, then " world" at offset 5
    let n1 = fs.test_write(fh, 0, b"hello").expect("write should succeed");
    assert_eq!(n1, 5);
    let n2 = fs.test_write(fh, 5, b" world").expect("write should succeed");
    assert_eq!(n2, 6);

    fs.test_release(ino, fh).expect("release should succeed");

    let content = read_content(&fs, ino);
    assert_eq!(content, b"hello world");
}

#[test]
fn test_write_with_gap_zero_pads() {
    let fs = fresh_fs();
    let (ino, fh) = fs.test_create(1, "gap.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create should succeed");

    // write 5 bytes at offset 10 — positions 0-9 must be zero-padded
    fs.test_write(fh, 10, b"hello").expect("write should succeed");
    fs.test_release(ino, fh).expect("release should succeed");

    let content = read_content(&fs, ino);
    assert_eq!(content.len(), 15);
    assert!(content[0..10].iter().all(|&b| b == 0));
    assert_eq!(&content[10..15], b"hello");
}

#[test]
fn test_release_flushes_to_cas_content_readable() {
    let fs = fresh_fs();
    let (ino, fh) = fs.test_create(1, "flush.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create should succeed");
    fs.test_write(fh, 0, b"flush content").expect("write should succeed");
    fs.test_release(ino, fh).expect("release should succeed");

    let content = read_content(&fs, ino);
    assert_eq!(content, b"flush content");
}

#[test]
fn test_release_updates_inode_size_and_mtime() {
    let fs = fresh_fs();
    let (ino, fh) = fs.test_create(1, "size.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create should succeed");
    fs.test_write(fh, 0, b"hello").expect("write should succeed");
    fs.test_release(ino, fh).expect("release should succeed");

    let meta = fs.meta().get_inode(ino).unwrap();
    assert_eq!(meta.size, 5);
    // mtime must have been set (nonzero after release)
    assert!(meta.mtime_sec > 0 || meta.mtime_nsec > 0);
}

#[test]
fn test_release_increments_refcount() {
    let fs = fresh_fs();
    let (ino, fh) = fs.test_create(1, "refcount.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create should succeed");
    fs.test_write(fh, 0, b"refcount data").expect("write should succeed");
    fs.test_release(ino, fh).expect("release should succeed");

    let manifest = fs.meta().get_manifest(ino).unwrap();
    assert!(!manifest.is_empty());
    let rc = fs.meta().get_refcount(&manifest[0]);
    assert_eq!(rc, 1, "refcount must be 1 after one release");
}

#[test]
fn test_two_identical_files_share_content_digest() {
    let fs = fresh_fs();

    let (ino1, fh1) = fs.test_create(1, "a.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create should succeed");
    fs.test_write(fh1, 0, b"same content").expect("write should succeed");
    fs.test_release(ino1, fh1).expect("release should succeed");

    let (ino2, fh2) = fs.test_create(1, "b.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create should succeed");
    fs.test_write(fh2, 0, b"same content").expect("write should succeed");
    fs.test_release(ino2, fh2).expect("release should succeed");

    let m1 = fs.meta().get_manifest(ino1).unwrap();
    let m2 = fs.meta().get_manifest(ino2).unwrap();
    assert_eq!(m1, m2, "identical content must share the same Digest224");

    let rc = fs.meta().get_refcount(&m1[0]);
    assert_eq!(rc, 2, "refcount must be 2 after two files with same content");
}

#[test]
fn test_empty_file_create_release_has_empty_manifest() {
    let fs = fresh_fs();
    let (ino, fh) = fs.test_create(1, "empty.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create should succeed");
    // No writes — just release
    fs.test_release(ino, fh).expect("release should succeed");

    let manifest = fs.meta().get_manifest(ino).unwrap();
    assert!(manifest.is_empty(), "empty file must have empty manifest");

    let meta = fs.meta().get_inode(ino).unwrap();
    assert_eq!(meta.size, 0);
}

// ── Task 2 tests ──────────────────────────────────────────────────────────────

#[test]
fn test_setattr_truncate_smaller_on_closed_file() {
    let fs = fresh_fs();
    let (ino, fh) = fs.test_create(1, "trunc.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create should succeed");
    fs.test_write(fh, 0, b"hello world").expect("write should succeed");
    fs.test_release(ino, fh).expect("release should succeed");

    // Truncate to 5 bytes
    fs.test_setattr_size(ino, None, 5).expect("setattr size should succeed");

    let content = read_content(&fs, ino);
    assert_eq!(content, b"hello");

    let meta = fs.meta().get_inode(ino).unwrap();
    assert_eq!(meta.size, 5);
}

#[test]
fn test_setattr_truncate_larger_zero_extends_closed_file() {
    let fs = fresh_fs();
    let (ino, fh) = fs.test_create(1, "extend.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create should succeed");
    fs.test_write(fh, 0, b"hi").expect("write should succeed");
    fs.test_release(ino, fh).expect("release should succeed");

    // Extend to 10 bytes
    fs.test_setattr_size(ino, None, 10).expect("setattr size should succeed");

    let content = read_content(&fs, ino);
    assert_eq!(content.len(), 10);
    assert_eq!(&content[0..2], b"hi");
    assert!(content[2..].iter().all(|&b| b == 0));
}

#[test]
fn test_setattr_truncate_open_file_handle_truncates_buffer() {
    let fs = fresh_fs();
    let (ino, fh) = fs.test_create(1, "open_trunc.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create should succeed");
    fs.test_write(fh, 0, b"hello world").expect("write should succeed");

    // Truncate the in-flight buffer while file is open
    fs.test_setattr_size(ino, Some(fh), 5).expect("setattr size on open file should succeed");

    // Now release — content should be 5 bytes
    fs.test_release(ino, fh).expect("release should succeed");

    let content = read_content(&fs, ino);
    assert_eq!(content, b"hello");
}

#[test]
fn test_setattr_mode_changes_permission_bits() {
    let fs = fresh_fs();
    let (ino, fh) = fs.test_create(1, "perm.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create should succeed");
    fs.test_release(ino, fh).expect("release should succeed");

    fs.test_setattr_mode(ino, 0o600).expect("setattr mode should succeed");

    let meta = fs.meta().get_inode(ino).unwrap();
    assert_eq!(meta.mode & 0o7777, 0o600);
    // Type bits must be preserved
    assert_eq!(meta.mode & S_IFREG, S_IFREG);
}

#[test]
fn test_setattr_uid_gid_changes_ownership() {
    let fs = fresh_fs();
    let (ino, fh) = fs.test_create(1, "owner.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create should succeed");
    fs.test_release(ino, fh).expect("release should succeed");

    fs.test_setattr_uid_gid(ino, Some(1001), Some(2002))
        .expect("setattr uid/gid should succeed");

    let meta = fs.meta().get_inode(ino).unwrap();
    assert_eq!(meta.uid, 1001);
    assert_eq!(meta.gid, 2002);
}

#[test]
fn test_setattr_mtime_updates_timestamp() {
    let fs = fresh_fs();
    let (ino, fh) = fs.test_create(1, "mtime.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create should succeed");
    fs.test_release(ino, fh).expect("release should succeed");

    // Set mtime to a specific value (1_000_000 seconds since epoch)
    fs.test_setattr_mtime(ino, 1_000_000, 0)
        .expect("setattr mtime should succeed");

    let meta = fs.meta().get_inode(ino).unwrap();
    assert_eq!(meta.mtime_sec, 1_000_000);
}

#[test]
fn test_mknod_returns_enosys() {
    let fs = fresh_fs();
    let result = fs.test_mknod(1, "fifo", 0o10644, 0);
    assert!(result.is_err(), "mknod must return error");
    // error code should be ENOSYS
    let err = result.unwrap_err();
    assert_eq!(err, libc::ENOSYS, "mknod must return ENOSYS");
}
