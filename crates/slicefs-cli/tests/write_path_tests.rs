//! Integration tests for the SliceFS write path.
//!
//! Tests create/write/release pipeline, buffer mechanics, CAS flush,
//! refcount management, deduplication, and setattr truncate.
//!
//! These tests use `DictMetadataStore` and `StoreIo` directly —
//! no FUSE mount required.

use blockset::file_storage_get;
use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use slicefs_cli::filesystem::{SliceFsFilesystem, inode_to_file_attr};
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

/// Read file content via manifest + file_storage_get
fn read_content(fs: &SliceFsFilesystem, ino: u64) -> Vec<u8> {
    let manifest = fs.meta().get_manifest(ino).unwrap();
    if manifest.is_empty() {
        return vec![];
    }
    let root_digest = manifest[0];
    let mut io = fs.io().lock().unwrap();
    file_storage_get(&mut *io, &root_digest).unwrap_or_default()
}

// ── Task 1 tests ──────────────────────────────────────────────────────────────

#[test]
fn test_create_produces_inode_in_parent_dir() {
    let (fs, _dir) = fresh_fs();
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
    let (fs, _dir) = fresh_fs();
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
    let (fs, _dir) = fresh_fs();
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
    let (fs, _dir) = fresh_fs();
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
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "flush.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create should succeed");
    fs.test_write(fh, 0, b"flush content").expect("write should succeed");
    fs.test_release(ino, fh).expect("release should succeed");

    let content = read_content(&fs, ino);
    assert_eq!(content, b"flush content");
}

#[test]
fn test_release_updates_inode_size_and_mtime() {
    let (fs, _dir) = fresh_fs();
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
    let (fs, _dir) = fresh_fs();
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
    let (fs, _dir) = fresh_fs();

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
    let (fs, _dir) = fresh_fs();
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
    let (fs, _dir) = fresh_fs();
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
    let (fs, _dir) = fresh_fs();
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
    let (fs, _dir) = fresh_fs();
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
    let (fs, _dir) = fresh_fs();
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
    let (fs, _dir) = fresh_fs();
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
    let (fs, _dir) = fresh_fs();
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
    let (fs, _dir) = fresh_fs();
    let result = fs.test_mknod(1, "fifo", 0o10644, 0);
    assert!(result.is_err(), "mknod must return error");
    // error code should be ENOSYS
    let err = result.unwrap_err();
    assert_eq!(err, libc::ENOSYS, "mknod must return ENOSYS");
}

/// Regression test for O_CREAT|O_TRUNC hang (second bug).
///
/// When FUSE_ATOMIC_O_TRUNC is NOT advertised, FUSE-T sends `setattr(size=0)`
/// after `create()` for every O_CREAT|O_TRUNC open. Before this fix, a newly
/// created file had no manifest entry yet — `get_manifest()` returned NotFound
/// which was mapped to EIO. FUSE-T's NFS layer would then stall waiting for a
/// successful setattr response, causing the open() call to hang indefinitely.
///
/// The fix: (1) treat NotFound from get_manifest as an empty manifest in
/// test_setattr_size Case B; (2) advertise FUSE_ATOMIC_O_TRUNC in init() so
/// FUSE-T never sends the separate setattr in the first place.
#[test]
fn test_setattr_size_zero_on_new_file_without_manifest() {
    let (fs, _dir) = fresh_fs();

    // Simulate: echo "test" > /mount/newfile.txt with FUSE_ATOMIC_O_TRUNC NOT set.
    // FUSE-T calls create() then setattr(size=0) for new files with O_TRUNC.
    let (ino, fh) = fs.test_create(1, "otrunc.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create should succeed");

    // At this point: inode exists, directory entry exists, open handle exists,
    // but manifest_data has NO entry for this inode yet (no write has happened).
    // Before the fix: this returned EIO, causing FUSE-T to hang.
    fs.test_setattr_size(ino, None, 0)
        .expect("setattr(size=0) on new file without manifest must succeed");

    // Inode size must be 0
    let meta = fs.meta().get_inode(ino).unwrap();
    assert_eq!(meta.size, 0);

    // Normal write + release after the setattr must still work
    fs.test_write(fh, 0, b"hello").expect("write should succeed");
    fs.test_release(ino, fh).expect("release should succeed");

    let content = read_content(&fs, ino);
    assert_eq!(content, b"hello");
}

/// Regression test for FUSE-T write hang.
///
/// Under FUSE-T, macOS maps NFS4 CLOSE → FUSE flush.  If flush returns ENOSYS
/// the NFS client stalls indefinitely.  The flush callback now calls
/// `flush_buffer_for_fsync` which flushes the buffer to CAS without closing the
/// handle, then returns ok().  This test exercises that exact path via
/// `test_fsync` (which shares the same helper).
#[test]
fn test_flush_write_read_roundtrip() {
    let (fs, _dir) = fresh_fs();

    // Simulate: echo "test" > /mount/newfile.txt
    // Step 1: create (NFS4 OPEN CREATE → FUSE create)
    let (ino, fh) = fs.test_create(1, "newfile.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create should succeed");

    // Step 2: write (NFS4 WRITE → FUSE write)
    fs.test_write(fh, 0, b"test\n").expect("write should succeed");

    // Step 3: flush (NFS4 CLOSE → FUSE flush)
    // flush_buffer_for_fsync is the same helper used by the flush() callback
    fs.test_fsync(ino, fh).expect("flush (via test_fsync) must not return ENOSYS");

    // Step 4: release (NFS4 final close → FUSE release)
    fs.test_release(ino, fh).expect("release should succeed");

    // Step 5: read back (cat /mount/newfile.txt → FUSE read)
    let content = read_content(&fs, ino);
    assert_eq!(content, b"test\n", "written content must be readable after flush+release");

    // Inode size must reflect the written data
    let meta = fs.meta().get_inode(ino).unwrap();
    assert_eq!(meta.size, 5, "inode size must equal written byte count");
}
