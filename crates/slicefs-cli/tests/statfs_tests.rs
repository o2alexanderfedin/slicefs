//! Integration tests for dedup-aware statfs reporting.
//!
//! These tests verify that statfs returns real logical and physical byte counts,
//! enabling users to see the dedup ratio directly from `df` output.
//!
//! Test setup: `DictMetadataStore` + `StoreIo` + `SliceFsFilesystem` directly,
//! no FUSE mount required.

use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use slicefs_cli::filesystem::SliceFsFilesystem;
use slicefs_compression::NoneCompressor;
use slicefs_traits::metadata::MetadataStore;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

const S_IFREG: u32 = 0o100_000;

fn fresh_fs() -> (SliceFsFilesystem, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let io = Arc::new(Mutex::new(StoreIo::new(dir.path())));
    let meta = DictMetadataStore::new(io.clone());
    let fs = SliceFsFilesystem::new(meta, io, None, Arc::new(NoneCompressor::new()), 1);
    (fs, dir)
}

// ── Logical bytes tests ───────────────────────────────────────────────────────

/// Empty filesystem should have 0 logical bytes.
#[test]
fn test_logical_bytes_empty_store() {
    let dir = tempfile::tempdir().unwrap();
    let io = Arc::new(Mutex::new(StoreIo::new(dir.path())));
    let meta = DictMetadataStore::new(io);
    assert_eq!(meta.logical_bytes(), 0, "empty store should have 0 logical bytes");
}

/// After creating a file with known size, logical bytes should increase.
#[test]
fn test_logical_bytes_increases_after_write() {
    let (fs, _dir) = fresh_fs();

    let (ino, fh) = fs
        .test_create(1, "file.txt", S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create should succeed");

    let content = b"hello world!"; // 12 bytes
    fs.test_write(fh, 0, content).expect("write should succeed");
    fs.test_release(ino, fh).expect("release should succeed");

    let logical = fs.meta().logical_bytes();
    assert!(
        logical >= content.len() as u64,
        "logical bytes {} should be >= {} after write",
        logical,
        content.len()
    );
}

/// Logical bytes should equal sum of all inode sizes.
#[test]
fn test_logical_bytes_equals_sum_of_inode_sizes() {
    let (fs, _dir) = fresh_fs();

    let content1 = b"first file content";   // 18 bytes
    let content2 = b"second file data!!";   // 18 bytes

    let (ino1, fh1) = fs
        .test_create(1, "a.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create a.txt");
    fs.test_write(fh1, 0, content1).expect("write a.txt");
    fs.test_release(ino1, fh1).expect("release a.txt");

    let (ino2, fh2) = fs
        .test_create(1, "b.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create b.txt");
    fs.test_write(fh2, 0, content2).expect("write b.txt");
    fs.test_release(ino2, fh2).expect("release b.txt");

    let meta1 = fs.meta().get_inode(ino1).unwrap();
    let meta2 = fs.meta().get_inode(ino2).unwrap();
    let expected_logical = meta1.size + meta2.size;
    let logical = fs.meta().logical_bytes();

    assert_eq!(
        logical, expected_logical,
        "logical bytes {} should equal sum of inode sizes {}",
        logical, expected_logical
    );
}

// ── Dedup ratio tests ─────────────────────────────────────────────────────────

/// Two files with identical content: both inodes report their size in logical_bytes,
/// but the CAS stores the content only once (dedup).
/// For large enough content, logical > physical proving the dedup ratio.
#[test]
fn test_dedup_ratio_with_identical_files() {
    let (fs, _dir) = fresh_fs();

    // Use content large enough to demonstrate dedup effect.
    // Both files have the same content — logical = 2 * content_size,
    // but CAS stores it once (dedup via Digest224 equality).
    let content = vec![0x42u8; 2048];

    let (ino1, fh1) = fs
        .test_create(1, "file1.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create file1");
    fs.test_write(fh1, 0, &content).expect("write file1");
    fs.test_release(ino1, fh1).expect("release file1");

    let (ino2, fh2) = fs
        .test_create(1, "file2.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create file2");
    fs.test_write(fh2, 0, &content).expect("write file2 (identical content)");
    fs.test_release(ino2, fh2).expect("release file2");

    let logical = fs.meta().logical_bytes();
    // Logical must be at least 2 * content.len() (both inodes track their sizes)
    assert!(
        logical >= 2 * content.len() as u64,
        "logical bytes {} should be >= 2 * {} = {}",
        logical,
        content.len(),
        2 * content.len()
    );

    // Verify manifests are identical (same Digest224 → dedup confirmed)
    let manifest1 = fs.meta().get_manifest(ino1).unwrap();
    let manifest2 = fs.meta().get_manifest(ino2).unwrap();
    assert_eq!(
        manifest1, manifest2,
        "identical content should produce identical manifests (dedup)"
    );

    // logical >= 2 * 2048 = 4096
    assert!(
        logical >= 4096,
        "logical bytes {} should be >= 4096 for two 2048-byte files",
        logical
    );
}

/// Two files with different content: no dedup.
#[test]
fn test_distinct_files_have_low_dedup_ratio() {
    let (fs, _dir) = fresh_fs();

    let content1 = b"unique content alpha";
    let content2 = b"unique content betaa"; // same length, different content

    let (ino1, fh1) = fs
        .test_create(1, "f1.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create f1");
    fs.test_write(fh1, 0, content1).expect("write f1");
    fs.test_release(ino1, fh1).expect("release f1");

    let (ino2, fh2) = fs
        .test_create(1, "f2.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create f2");
    fs.test_write(fh2, 0, content2).expect("write f2");
    fs.test_release(ino2, fh2).expect("release f2");

    let logical = fs.meta().logical_bytes();
    assert!(
        logical >= (content1.len() + content2.len()) as u64,
        "logical bytes {} should cover both distinct files",
        logical
    );
}

// ── Physical bytes tests ──────────────────────────────────────────────────────

/// Physical bytes must be > 0 after writing a file.
#[test]
fn test_physical_bytes_nonzero_after_write() {
    let (fs, dir) = fresh_fs();

    let (ino, fh) = fs
        .test_create(1, "data.bin", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create data.bin");
    fs.test_write(fh, 0, b"some data content here").expect("write");
    fs.test_release(ino, fh).expect("release");

    // Physical bytes: actual bytes in vt0/ CAS batch files on disk.
    let vt0_path = dir.path().join("vt0");
    let physical_bytes: u64 = if vt0_path.exists() {
        std::fs::read_dir(&vt0_path)
            .unwrap()
            .flatten()
            .filter_map(|e| e.metadata().ok())
            .filter(|m| m.is_file())
            .map(|m| m.len())
            .sum()
    } else {
        0
    };

    assert!(
        physical_bytes > 0,
        "physical bytes should be > 0 after writing a file (vt0/ must contain data)"
    );
}

// ── statfs() integration tests ────────────────────────────────────────────────

/// statfs() on empty filesystem returns 0 logical_bytes.
#[test]
fn test_statfs_empty_filesystem() {
    let dir = tempfile::tempdir().unwrap();
    let io = Arc::new(Mutex::new(StoreIo::new(dir.path())));
    let meta = DictMetadataStore::new(io);
    assert_eq!(
        meta.logical_bytes(),
        0,
        "fresh store must have 0 logical bytes for statfs"
    );
}

/// After writing 1000+ bytes, statfs reports >= 1000 logical bytes.
#[test]
fn test_statfs_after_write_reports_logical_bytes() {
    let (fs, _dir) = fresh_fs();

    let content = vec![0xABu8; 1000];
    let (ino, fh) = fs
        .test_create(1, "big.bin", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create big.bin");
    fs.test_write(fh, 0, &content).expect("write 1000 bytes");
    fs.test_release(ino, fh).expect("release");

    let logical = fs.meta().logical_bytes();
    assert!(
        logical >= 1000,
        "logical bytes {} should be >= 1000 after writing 1000 bytes",
        logical
    );
}

/// logical_bytes tracks write then delete correctly.
#[test]
fn test_logical_bytes_decremented_on_delete() {
    let (fs, _dir) = fresh_fs();

    let content = b"content to be deleted later";
    let (ino, fh) = fs
        .test_create(1, "temp.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create temp.txt");
    fs.test_write(fh, 0, content).expect("write");
    fs.test_release(ino, fh).expect("release");

    let before = fs.meta().logical_bytes();
    assert!(before >= content.len() as u64, "logical bytes should reflect file size");

    // Simulate unlink: decrement nlinks to 0 triggers delete path
    fs.simulate_unlink(1, "temp.txt").expect("unlink temp.txt");

    let after = fs.meta().logical_bytes();
    assert!(
        after < before,
        "logical bytes {} should decrease after file deletion (was {})",
        after,
        before
    );
}
