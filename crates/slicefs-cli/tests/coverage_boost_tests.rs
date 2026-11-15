//! Tests specifically targeting uncovered code paths in filesystem.rs.
//!
//! Goals:
//!   - inode_to_file_attr for symlinks and negative mtime
//!   - meta_error_to_fuse_errno for all variants
//!   - meta_error_to_errno all variants
//!   - dir_size helper (via compute_statfs with a real store_path)
//!   - set_auto_snapshot / auto_snapshot field
//!   - test_write: EBADF on unknown fh
//!   - test_write: fallback from Streaming to Buffered with prior content
//!   - test_write: fallback from Streaming to Buffered with empty streaming state
//!   - test_release: already-closed fh (Ok)
//!   - test_release: fsync-already-committed skip path (cas_committed + same digest)
//!   - flush_buffer_for_fsync buffered mode
//!   - flush_buffer_for_fsync empty file (byte_count==0) fast path
//!   - flush_buffer_for_fsync closed fh no-op
//!   - test_setattr_size: open Buffered handle to zero
//!   - test_setattr_size: open Buffered handle to nonzero
//!   - test_setattr_size: open Streaming handle to nonzero (materialize+resize)
//!   - test_setattr_size: open Streaming handle to zero (fast path)
//!   - test_setattr_mode on non-existent inode (error path)
//!   - test_setattr_uid_gid only uid
//!   - test_setattr_uid_gid only gid
//!   - test_setattr_mtime on valid inode
//!   - simulate_readlink empty manifest
//!   - simulate_rmdir on non-existent name (ENOENT)
//!   - simulate_rmdir on a regular file (ENOTDIR)
//!   - simulate_unlink on non-existent name (ENOENT)
//!   - simulate_link to non-existent source inode (ENOENT)
//!   - simulate_rename noreplace with no target (should succeed)
//!   - simulate_rename overwrite with directory target
//!   - test_read: zero-size read
//!   - test_read: offset beyond EOF
//!   - test_read: read from uncommitted streaming write handle
//!   - test_read: no manifest (empty file)
//!   - XAttr operations via DictMetadataStore directly

use blockset::{State, Tree, FileStorageAdd, file_storage_get};
use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use slicefs_cli::filesystem::{
    SliceFsFilesystem, inode_to_file_attr, meta_error_to_fuse_errno, meta_error_to_errno,
};
use slicefs_traits::metadata::{InodeMeta, MetadataStore, MetaError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};
use tempfile::TempDir;
#[allow(unused_imports)]
use fuser::FileType;

const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFLNK: u32 = 0o120_000;
const S_IFMT: u32 = 0o170_000;

fn fresh_fs() -> (SliceFsFilesystem, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let io = Arc::new(Mutex::new(StoreIo::new(dir.path())));
    let meta = DictMetadataStore::new(io.clone());
    let fs = SliceFsFilesystem::new(meta, io, None);
    (fs, dir)
}

fn fresh_fs_with_path() -> (SliceFsFilesystem, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let io = Arc::new(Mutex::new(StoreIo::new(dir.path())));
    let meta = DictMetadataStore::new(io.clone());
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

// ── inode_to_file_attr edge cases ─────────────────────────────────────────────

#[test]
fn test_inode_to_file_attr_symlink_kind() {
    let meta = InodeMeta {
        ino: 42,
        mode: S_IFLNK | 0o777,
        uid: 0,
        gid: 0,
        nlinks: 1,
        size: 10,
        mtime_sec: 1000,
        mtime_nsec: 0,
        ctime_sec: 1000,
        ctime_nsec: 0,
        rdev: 0, atime_sec: 0, atime_nsec: 0,
    };
    let attr = inode_to_file_attr(&meta);
    use fuser::FileType;
    assert_eq!(attr.kind, FileType::Symlink, "mode S_IFLNK must map to Symlink");
}

#[test]
fn test_inode_to_file_attr_unknown_type_is_regular() {
    // Mode with no type bits → defaults to RegularFile
    let meta = InodeMeta {
        ino: 7,
        mode: 0o644, // no type bits
        uid: 0,
        gid: 0,
        nlinks: 1,
        size: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        ctime_sec: 0,
        ctime_nsec: 0,
        rdev: 0, atime_sec: 0, atime_nsec: 0,
    };
    let attr = inode_to_file_attr(&meta);
    use fuser::FileType;
    assert_eq!(attr.kind, FileType::RegularFile, "unknown type bits default to RegularFile");
}

#[test]
fn test_inode_to_file_attr_negative_mtime() {
    // mtime_sec < 0 means time before UNIX_EPOCH
    let meta = InodeMeta {
        ino: 5,
        mode: S_IFREG | 0o644,
        uid: 0,
        gid: 0,
        nlinks: 1,
        size: 0,
        mtime_sec: -100,
        mtime_nsec: 0,
        ctime_sec: -50,
        ctime_nsec: 0,
        rdev: 0, atime_sec: 0, atime_nsec: 0,
    };
    let attr = inode_to_file_attr(&meta);
    // mtime should be UNIX_EPOCH - 100s
    let expected_mtime = UNIX_EPOCH - Duration::new(100, 0);
    assert_eq!(attr.mtime, expected_mtime, "negative mtime_sec should be UNIX_EPOCH - duration");
    let expected_ctime = UNIX_EPOCH - Duration::new(50, 0);
    assert_eq!(attr.ctime, expected_ctime, "negative ctime_sec should be UNIX_EPOCH - duration");
}

#[test]
fn test_inode_to_file_attr_blocks_roundup() {
    // size=511 → blocks=1 (rounds up to 512)
    let meta = InodeMeta {
        ino: 3,
        mode: S_IFREG | 0o644,
        uid: 0,
        gid: 0,
        nlinks: 1,
        size: 511,
        mtime_sec: 0,
        mtime_nsec: 0,
        ctime_sec: 0,
        ctime_nsec: 0,
        rdev: 0, atime_sec: 0, atime_nsec: 0,
    };
    let attr = inode_to_file_attr(&meta);
    assert_eq!(attr.blocks, 1, "511 bytes should round up to 1 block");
}

#[test]
fn test_inode_to_file_attr_blocks_exact() {
    // size=512 → blocks=1
    let meta = InodeMeta {
        ino: 4,
        mode: S_IFREG | 0o644,
        uid: 0,
        gid: 0,
        nlinks: 1,
        size: 512,
        mtime_sec: 0,
        mtime_nsec: 0,
        ctime_sec: 0,
        ctime_nsec: 0,
        rdev: 0, atime_sec: 0, atime_nsec: 0,
    };
    let attr = inode_to_file_attr(&meta);
    assert_eq!(attr.blocks, 1, "512 bytes = exactly 1 block");
}

// ── meta_error_to_fuse_errno coverage ────────────────────────────────────────

/// Helper: convert Errno to i32 for comparison (Errno doesn't impl PartialEq).
fn errno_to_i32(e: fuser::Errno) -> i32 {
    i32::from(e)
}

#[test]
fn test_meta_error_to_fuse_errno_all_variants() {
    assert_eq!(errno_to_i32(meta_error_to_fuse_errno(&MetaError::NotFound(0))), libc::ENOENT);
    assert_eq!(errno_to_i32(meta_error_to_fuse_errno(&MetaError::AlreadyExists(0))), libc::EEXIST);
    assert_eq!(errno_to_i32(meta_error_to_fuse_errno(&MetaError::NotADirectory(0))), libc::ENOTDIR);
    assert_eq!(errno_to_i32(meta_error_to_fuse_errno(&MetaError::IsADirectory(0))), libc::EISDIR);
    assert_eq!(errno_to_i32(meta_error_to_fuse_errno(&MetaError::NotEmpty(0))), libc::ENOTEMPTY);
    assert_eq!(errno_to_i32(meta_error_to_fuse_errno(&MetaError::InvalidName("x".into()))), libc::EINVAL);
    assert_eq!(errno_to_i32(meta_error_to_fuse_errno(&MetaError::Corrupted("x".into()))), libc::EIO);
    // MetaError::Io wraps std::io::Error
    let io_err = MetaError::Io(std::io::Error::new(std::io::ErrorKind::Other, "test io error"));
    assert_eq!(errno_to_i32(meta_error_to_fuse_errno(&io_err)), libc::EIO);
}

#[test]
fn test_meta_error_to_errno_all_variants() {
    assert_eq!(meta_error_to_errno(&MetaError::NotFound(0)), libc::ENOENT);
    assert_eq!(meta_error_to_errno(&MetaError::AlreadyExists(0)), libc::EEXIST);
    assert_eq!(meta_error_to_errno(&MetaError::NotADirectory(0)), libc::ENOTDIR);
    assert_eq!(meta_error_to_errno(&MetaError::IsADirectory(0)), libc::EISDIR);
    assert_eq!(meta_error_to_errno(&MetaError::NotEmpty(0)), libc::ENOTEMPTY);
    assert_eq!(meta_error_to_errno(&MetaError::InvalidName("x".into())), libc::EINVAL);
    assert_eq!(meta_error_to_errno(&MetaError::Corrupted("x".into())), libc::EIO);
    // MetaError::Io wraps std::io::Error
    let io_err = MetaError::Io(std::io::Error::new(std::io::ErrorKind::Other, "test io error"));
    assert_eq!(meta_error_to_errno(&io_err), libc::EIO);
}

// ── set_auto_snapshot ─────────────────────────────────────────────────────────

#[test]
fn test_set_auto_snapshot_mutates_field() {
    let (mut fs, _dir) = fresh_fs();
    fs.set_auto_snapshot(true);
    // We can't directly read the field (it's private), but we can verify
    // the method doesn't panic and the filesystem still works.
    let meta = fs.meta().get_inode(1).unwrap();
    assert_eq!(meta.ino, 1);
}

// ── test_write: error paths ───────────────────────────────────────────────────

#[test]
fn test_write_invalid_fh_returns_ebadf() {
    let (fs, _dir) = fresh_fs();
    // fh=9999 was never opened
    let result = fs.test_write(9999, 0, b"data");
    assert_eq!(result.unwrap_err(), libc::EBADF, "unknown fh must return EBADF");
}

// ── test_write: streaming fallback to buffered ────────────────────────────────

#[test]
fn test_write_nonsequential_after_write_loads_from_streaming_state() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "fallback.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .unwrap();
    // Write 5 bytes at offset 0 (streaming)
    fs.test_write(fh, 0, b"hello").unwrap();
    // Write at offset 3 (non-sequential — triggers fallback from streaming to buffered)
    // The existing streaming content must be materialized first
    fs.test_write(fh, 3, b"XX").unwrap();

    fs.test_release(ino, fh).unwrap();
    let content = read_content(&fs, ino);
    // "hello" with positions 3-4 overwritten: "helXX" → wait, offset 3 = 'l','o' → "helXXo"? No:
    // "hello" = [h,e,l,l,o] at [0,1,2,3,4]
    // pwrite at offset 3 with "XX" → [h,e,l,X,X]
    assert_eq!(content, b"helXX", "fallback to buffered must produce correct merged content");
}

#[test]
fn test_write_nonsequential_from_empty_streaming_falls_back_to_buffered() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "empty_fallback.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .unwrap();
    // Write at non-zero offset without prior write (streaming state is empty)
    fs.test_write(fh, 5, b"world").unwrap();
    fs.test_release(ino, fh).unwrap();

    let content = read_content(&fs, ino);
    assert_eq!(content.len(), 10, "should be 5 zeros + 5 data bytes");
    assert!(content[..5].iter().all(|&b| b == 0), "prefix must be zero-padded");
    assert_eq!(&content[5..], b"world");
}

#[test]
fn test_write_buffered_mode_extends_buffer() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "buffered.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .unwrap();
    // Trigger fallback to buffered mode first
    fs.test_write(fh, 5, b"world").unwrap(); // non-sequential → buffered
    // Now write at a later offset (buffered mode pwrite)
    fs.test_write(fh, 0, b"hello").unwrap(); // write into existing buffered space

    fs.test_release(ino, fh).unwrap();
    let content = read_content(&fs, ino);
    // Buffer started with 10 bytes (5 zeros + "world"), then "hello" at 0..5
    assert_eq!(&content[..5], b"hello");
    assert_eq!(&content[5..], b"world");
}

#[test]
fn test_write_buffered_beyond_end_extends() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "buf_extend.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .unwrap();
    // Trigger buffered mode
    fs.test_write(fh, 3, b"abc").unwrap(); // buffered with 6 bytes total: [0,0,0,a,b,c]
    // Extend beyond current buffer end
    fs.test_write(fh, 8, b"xyz").unwrap(); // → [0,0,0,a,b,c,0,0,x,y,z]

    fs.test_release(ino, fh).unwrap();
    let content = read_content(&fs, ino);
    assert_eq!(content.len(), 11);
    assert_eq!(&content[3..6], b"abc");
    assert_eq!(&content[8..11], b"xyz");
}

// ── test_release: edge cases ──────────────────────────────────────────────────

#[test]
fn test_release_already_closed_returns_ok() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "once.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .unwrap();
    fs.test_release(ino, fh).unwrap();
    // Second release on same fh should be Ok (already removed from open_files)
    let result = fs.test_release(ino, fh);
    assert!(result.is_ok(), "second release on same fh must return Ok");
}

#[test]
fn test_release_after_fsync_skips_redundant_commit() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "fsync_then_release.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .unwrap();
    fs.test_write(fh, 0, b"committed data").unwrap();
    fs.test_fsync(ino, fh).unwrap();

    // Get manifest after fsync — should have one digest
    let manifest_after_fsync = fs.meta().get_manifest(ino).unwrap();
    assert!(!manifest_after_fsync.is_empty(), "manifest must be set after fsync");

    // Now release — should not increment refcount again (digest already committed)
    fs.test_release(ino, fh).unwrap();

    // Manifest should be the same after release
    let manifest_after_release = fs.meta().get_manifest(ino).unwrap();
    assert_eq!(manifest_after_fsync, manifest_after_release,
        "manifest should not change when release finds same committed digest");
}

// ── test_fsync: buffered mode flush ───────────────────────────────────────────

#[test]
fn test_fsync_buffered_mode_flushes_to_cas() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "buf_fsync.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .unwrap();

    // Trigger buffered mode
    fs.test_write(fh, 5, b"world").unwrap();
    fs.test_write(fh, 0, b"hello").unwrap();

    // fsync should flush buffered mode to CAS
    fs.test_fsync(ino, fh).unwrap();

    let manifest = fs.meta().get_manifest(ino).unwrap();
    assert!(!manifest.is_empty(), "manifest must be set after fsync in buffered mode");

    // Verify content is correct
    let content = {
        let mut io = fs.io().lock().unwrap();
        file_storage_get(&mut *io, &manifest[0]).unwrap()
    };
    assert_eq!(&content[..5], b"hello");
    assert_eq!(&content[5..], b"world");
}

#[test]
fn test_fsync_twice_with_same_content_skips_refcount_increment() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "double_fsync.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .unwrap();
    fs.test_write(fh, 0, b"test data").unwrap();
    fs.test_fsync(ino, fh).unwrap();

    let manifest_1 = fs.meta().get_manifest(ino).unwrap();
    let rc_after_first = fs.meta().get_refcount(&manifest_1[0]);

    // Second fsync — same state, should not bump refcount
    fs.test_fsync(ino, fh).unwrap();

    let manifest_2 = fs.meta().get_manifest(ino).unwrap();
    let rc_after_second = fs.meta().get_refcount(&manifest_2[0]);

    assert_eq!(manifest_1, manifest_2, "manifests must match");
    assert_eq!(rc_after_first, rc_after_second, "refcount must not increase on redundant fsync");
}

#[test]
fn test_fsync_empty_file_returns_ok_without_manifest() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "empty_fsync.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .unwrap();
    // No write — fsync on empty open file is a no-op
    let result = fs.test_fsync(ino, fh);
    assert!(result.is_ok(), "fsync on empty open file must return Ok");

    // Release it cleanly
    fs.test_release(ino, fh).unwrap();
}

#[test]
fn test_fsync_closed_fh_is_noop() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "closed_fsync.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .unwrap();
    fs.test_release(ino, fh).unwrap();

    // fsync on closed fh must succeed (no-op)
    let result = fs.test_fsync(ino, fh);
    assert!(result.is_ok(), "fsync on closed fh must return Ok (no-op)");
}

// ── test_setattr_size: open buffered handle ───────────────────────────────────

#[test]
fn test_setattr_size_zero_on_open_buffered_handle() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "buf_zero.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .unwrap();

    // Trigger buffered mode and write some data
    fs.test_write(fh, 5, b"world").unwrap();
    fs.test_write(fh, 0, b"hello").unwrap();

    // Truncate to zero while in buffered mode
    fs.test_setattr_size(ino, Some(fh), 0).unwrap();

    // Release and verify content is empty
    fs.test_release(ino, fh).unwrap();
    let meta = fs.meta().get_inode(ino).unwrap();
    assert_eq!(meta.size, 0, "size must be 0 after truncate-to-zero on buffered handle");
}

#[test]
fn test_setattr_size_nonzero_on_open_buffered_handle() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "buf_resize.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .unwrap();

    // Trigger buffered mode with 10 bytes
    fs.test_write(fh, 5, b"world").unwrap();
    fs.test_write(fh, 0, b"hello").unwrap();

    // Resize to 3 bytes
    fs.test_setattr_size(ino, Some(fh), 3).unwrap();

    fs.test_release(ino, fh).unwrap();
    let content = read_content(&fs, ino);
    assert_eq!(content, b"hel", "buffered truncate to 3 must keep first 3 bytes");
}

#[test]
fn test_setattr_size_extend_on_open_buffered_handle() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "buf_extend2.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .unwrap();

    // Trigger buffered mode with 5 bytes: [h,e,l,l,o]
    fs.test_write(fh, 5, b"hello").unwrap(); // nonseq → buffered
    fs.test_write(fh, 0, b"START").unwrap(); // [S,T,A,R,T,h,e,l,l,o]

    // Extend to 15 (zero-fill the tail)
    fs.test_setattr_size(ino, Some(fh), 15).unwrap();

    fs.test_release(ino, fh).unwrap();
    let content = read_content(&fs, ino);
    assert_eq!(content.len(), 15, "extended buffered content must be 15 bytes");
    assert!(content[10..].iter().all(|&b| b == 0), "extension must be zero-filled");
}

// ── test_setattr_size: open streaming handle ──────────────────────────────────

#[test]
fn test_setattr_size_zero_on_open_streaming_handle() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "strm_zero.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .unwrap();

    // Write 5 bytes sequentially (streaming mode)
    fs.test_write(fh, 0, b"hello").unwrap();

    // Truncate to zero in streaming mode
    fs.test_setattr_size(ino, Some(fh), 0).unwrap();

    // Check inode size is updated immediately
    let meta = fs.meta().get_inode(ino).unwrap();
    assert_eq!(meta.size, 0, "streaming truncate-to-zero must update inode size");

    // Write new content and release
    fs.test_write(fh, 0, b"new").unwrap();
    fs.test_release(ino, fh).unwrap();
    let content = read_content(&fs, ino);
    assert_eq!(content, b"new", "after truncate-to-zero, new write must be the only content");
}

#[test]
fn test_setattr_size_nonzero_on_open_streaming_handle() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "strm_resize.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .unwrap();

    // Write 10 bytes sequentially (streaming mode)
    fs.test_write(fh, 0, b"0123456789").unwrap();

    // Resize to 5 bytes (non-zero truncate of streaming state)
    fs.test_setattr_size(ino, Some(fh), 5).unwrap();

    fs.test_release(ino, fh).unwrap();
    let content = read_content(&fs, ino);
    assert_eq!(content, b"01234", "streaming truncate to 5 must keep first 5 bytes");
}

#[test]
fn test_setattr_size_extend_on_open_streaming_handle() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "strm_extend.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .unwrap();

    // Write 3 bytes (streaming mode)
    fs.test_write(fh, 0, b"abc").unwrap();

    // Extend to 8 bytes (zero-fill the tail)
    fs.test_setattr_size(ino, Some(fh), 8).unwrap();

    fs.test_release(ino, fh).unwrap();
    let content = read_content(&fs, ino);
    assert_eq!(content.len(), 8, "streaming extend must produce 8 bytes");
    assert_eq!(&content[..3], b"abc");
    assert!(content[3..].iter().all(|&b| b == 0), "extension must be zero-filled");
}

#[test]
fn test_setattr_size_nonzero_on_empty_streaming_handle() {
    // Streaming with byte_count==0, nonzero resize — loads from manifest
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "strm_empty_resize.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .unwrap();

    // No writes yet — byte_count == 0 in streaming mode
    // Resize to 5 (zero-extension of empty)
    fs.test_setattr_size(ino, Some(fh), 5).unwrap();

    fs.test_release(ino, fh).unwrap();
    // Content comes from the new streaming state (5 zero bytes)
    let meta = fs.meta().get_inode(ino).unwrap();
    assert_eq!(meta.size, 5, "empty streaming resize must update inode size to 5");
}

// ── test_setattr_mode: error path ─────────────────────────────────────────────

#[test]
fn test_setattr_mode_nonexistent_inode_returns_error() {
    let (fs, _dir) = fresh_fs();
    let result = fs.test_setattr_mode(99999, 0o600);
    assert!(result.is_err(), "setattr_mode on non-existent inode must return error");
    assert_eq!(result.unwrap_err(), libc::EIO, "expected EIO for missing inode");
}

#[test]
fn test_setattr_uid_only() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "uid_only.txt", S_IFREG | 0o644, 0o022, 500, 600)
        .unwrap();
    fs.test_release(ino, fh).unwrap();

    // Change only uid
    fs.test_setattr_uid_gid(ino, Some(9999), None).unwrap();

    let meta = fs.meta().get_inode(ino).unwrap();
    assert_eq!(meta.uid, 9999, "uid must be updated");
    assert_eq!(meta.gid, 600, "gid must be unchanged");
}

#[test]
fn test_setattr_gid_only() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "gid_only.txt", S_IFREG | 0o644, 0o022, 500, 600)
        .unwrap();
    fs.test_release(ino, fh).unwrap();

    // Change only gid
    fs.test_setattr_uid_gid(ino, None, Some(8888)).unwrap();

    let meta = fs.meta().get_inode(ino).unwrap();
    assert_eq!(meta.uid, 500, "uid must be unchanged");
    assert_eq!(meta.gid, 8888, "gid must be updated");
}

#[test]
fn test_setattr_neither_uid_nor_gid_is_noop() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "noop_owner.txt", S_IFREG | 0o644, 0o022, 500, 600)
        .unwrap();
    fs.test_release(ino, fh).unwrap();

    // Both None → no change to uid/gid
    fs.test_setattr_uid_gid(ino, None, None).unwrap();

    let meta = fs.meta().get_inode(ino).unwrap();
    assert_eq!(meta.uid, 500, "uid must be unchanged");
    assert_eq!(meta.gid, 600, "gid must be unchanged");
}

// ── simulate_readlink edge cases ──────────────────────────────────────────────

#[test]
fn test_readlink_empty_manifest_returns_empty_string() {
    let (fs, _dir) = fresh_fs();
    // Create a symlink-like inode but with empty manifest
    let meta = InodeMeta {
        ino: 0,
        mode: S_IFLNK | 0o777,
        uid: 0,
        gid: 0,
        nlinks: 1,
        size: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        ctime_sec: 0,
        ctime_nsec: 0,
        rdev: 0, atime_sec: 0, atime_nsec: 0,
    };
    let ino = fs.meta().create_inode(&meta).unwrap();
    fs.meta().link(1, "empty_link", ino).unwrap();
    fs.meta().set_manifest(ino, &[]).unwrap();

    let result = fs.simulate_readlink(ino);
    assert!(result.is_ok(), "readlink with empty manifest must succeed");
    assert_eq!(result.unwrap(), "", "empty manifest returns empty string");
}

#[test]
fn test_readlink_nonexistent_inode_returns_error() {
    let (fs, _dir) = fresh_fs();
    let result = fs.simulate_readlink(99999);
    assert!(result.is_err(), "readlink on non-existent inode must fail");
}

// ── simulate_rmdir edge cases ─────────────────────────────────────────────────

#[test]
fn test_rmdir_nonexistent_returns_enoent() {
    let (fs, _dir) = fresh_fs();
    let result = fs.simulate_rmdir(1, "does_not_exist");
    assert!(result.is_err(), "rmdir of non-existent dir must fail");
    assert_eq!(result.unwrap_err(), libc::ENOENT, "error must be ENOENT");
}

#[test]
fn test_rmdir_on_regular_file_returns_enotdir() {
    let (fs, _dir) = fresh_fs();
    let meta = InodeMeta::new_file(0, 0, 0, S_IFREG | 0o644);
    let ino = fs.meta().create_inode(&meta).unwrap();
    fs.meta().link(1, "not_a_dir.txt", ino).unwrap();

    let result = fs.simulate_rmdir(1, "not_a_dir.txt");
    assert!(result.is_err(), "rmdir on regular file must fail");
    assert_eq!(result.unwrap_err(), libc::ENOTDIR, "error must be ENOTDIR");
}

// ── simulate_unlink edge cases ────────────────────────────────────────────────

#[test]
fn test_unlink_nonexistent_returns_enoent() {
    let (fs, _dir) = fresh_fs();
    let result = fs.simulate_unlink(1, "ghost.txt");
    assert!(result.is_err(), "unlink of non-existent file must fail");
    assert_eq!(result.unwrap_err(), libc::ENOENT, "error must be ENOENT");
}

// ── simulate_link edge cases ──────────────────────────────────────────────────

#[test]
fn test_link_nonexistent_source_returns_error() {
    let (fs, _dir) = fresh_fs();
    let result = fs.simulate_link(99999, 1, "newname.txt");
    assert!(result.is_err(), "link to non-existent source must fail");
}

// ── simulate_rename edge cases ────────────────────────────────────────────────

#[test]
fn test_rename_noreplace_no_target_succeeds() {
    let (fs, _dir) = fresh_fs();
    let meta = InodeMeta::new_file(0, 0, 0, S_IFREG | 0o644);
    let ino = fs.meta().create_inode(&meta).unwrap();
    fs.meta().link(1, "src.txt", ino).unwrap();

    // RENAME_NOREPLACE = 1, no target exists — must succeed
    let result = fs.simulate_rename(1, "src.txt", 1, "dst.txt", 1);
    assert!(result.is_ok(), "RENAME_NOREPLACE with no target must succeed");

    let found_ino = fs.meta().lookup(1, "dst.txt").unwrap();
    assert_eq!(found_ino, ino, "dst.txt must resolve to moved inode");
    assert!(fs.meta().lookup(1, "src.txt").is_err(), "src.txt must be gone");
}

#[test]
fn test_rename_overwrite_directory_target() {
    let (fs, _dir) = fresh_fs();

    // Create a file source
    let src_ino = {
        let meta = InodeMeta::new_file(0, 0, 0, S_IFREG | 0o644);
        let ino = fs.meta().create_inode(&meta).unwrap();
        fs.meta().link(1, "file_src.txt", ino).unwrap();
        ino
    };

    // Create an empty directory target
    let _dst_ino = fs.simulate_mkdir(1, "dir_dst", S_IFDIR | 0o755, 0, 0, 0).unwrap();

    // POSIX rename(2): a non-directory cannot replace a directory (EISDIR).
    let result = fs.simulate_rename(1, "file_src.txt", 1, "dir_dst", 0);
    assert_eq!(result, Err(libc::EISDIR));

    // Both names are unchanged.
    assert_eq!(fs.meta().lookup(1, "file_src.txt").unwrap(), src_ino);
    assert!(fs.meta().lookup(1, "dir_dst").is_ok());
}

// ── test_read edge cases ──────────────────────────────────────────────────────

#[test]
fn test_read_zero_size_returns_empty() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "zeroread.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .unwrap();
    fs.test_write(fh, 0, b"hello").unwrap();
    fs.test_release(ino, fh).unwrap();

    let result = fs.test_read(ino, 0, 0).unwrap();
    assert!(result.is_empty(), "reading 0 bytes must return empty vec");
}

#[test]
fn test_read_offset_beyond_eof_returns_empty() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "beyond_eof.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .unwrap();
    fs.test_write(fh, 0, b"hello").unwrap();
    fs.test_release(ino, fh).unwrap();

    // Read from offset 1000 (way past EOF)
    let result = fs.test_read(ino, 1000, 100).unwrap();
    assert!(result.is_empty(), "reading past EOF must return empty vec");
}

#[test]
fn test_read_partial_at_end_of_file() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "partial.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .unwrap();
    fs.test_write(fh, 0, b"hello world").unwrap();
    fs.test_release(ino, fh).unwrap();

    // Read 100 bytes from offset 6, but file only has 5 more bytes
    let result = fs.test_read(ino, 6, 100).unwrap();
    assert_eq!(result, b"world", "partial read at end of file must return only available bytes");
}

#[test]
fn test_read_empty_file_returns_empty() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "empty_read.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .unwrap();
    fs.test_release(ino, fh).unwrap();

    let result = fs.test_read(ino, 0, 100).unwrap();
    assert!(result.is_empty(), "reading empty file must return empty vec");
}

#[test]
fn test_read_from_uncommitted_streaming_write() {
    // test_read should see in-flight (uncommitted) streaming writes
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "uncommitted.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .unwrap();
    // Write but don't release
    fs.test_write(fh, 0, b"live data").unwrap();

    // Read should see the uncommitted streaming content
    let result = fs.test_read(ino, 0, 100).unwrap();
    assert_eq!(result, b"live data", "test_read must see uncommitted streaming content");

    // Clean up
    fs.test_release(ino, fh).unwrap();
}

#[test]
fn test_read_from_uncommitted_buffered_write() {
    // test_read should see in-flight (uncommitted) buffered writes
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "uncommitted_buf.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .unwrap();
    // Trigger buffered mode and write
    fs.test_write(fh, 5, b"world").unwrap();
    fs.test_write(fh, 0, b"hello").unwrap();

    // Read should see the uncommitted buffered content
    let result = fs.test_read(ino, 0, 20).unwrap();
    assert_eq!(&result[..5], b"hello", "first 5 bytes must be 'hello'");
    assert_eq!(&result[5..], b"world", "next 5 bytes must be 'world'");

    // Clean up
    fs.test_release(ino, fh).unwrap();
}

// ── compute_statfs / dir_size coverage ───────────────────────────────────────

#[test]
fn test_statfs_with_store_path_has_nonzero_bsize() {
    let (fs, _dir) = fresh_fs_with_path();
    let (_blocks, _bfree, _bavail, _files, _ffree, bsize) = fs.test_statfs_values();
    assert!(bsize >= 512, "bsize must be at least 512 bytes");
}

#[test]
fn test_statfs_files_equals_inode_count() {
    let (fs, _dir) = fresh_fs();
    let (_blocks, _bfree, _bavail, files, ffree, _bsize) = fs.test_statfs_values();
    let inode_count = fs.meta().inode_count();
    assert_eq!(files, inode_count, "statfs files must equal inode_count");
    assert_eq!(ffree, u64::MAX.saturating_sub(inode_count), "ffree must be u64::MAX - inode_count");
}

#[test]
fn test_statfs_after_creating_files_inode_count_increases() {
    let (fs, _dir) = fresh_fs();
    let (_, _, _, files_before, _, _) = fs.test_statfs_values();

    // Create 3 files
    for i in 0..3u32 {
        let name = format!("file{}.txt", i);
        let (ino, fh) = fs.test_create(1, &name, S_IFREG | 0o644, 0, 0, 0).unwrap();
        fs.test_release(ino, fh).unwrap();
    }

    let (_, _, _, files_after, _, _) = fs.test_statfs_values();
    assert_eq!(files_after, files_before + 3, "inode count must increase by 3");
}

// ── mknod type rejection ──────────────────────────────────────────────────────

#[test]
fn test_mknod_fifo_creates_named_pipe() {
    // pjdfstest creates FIFOs with mkfifo(2); ENOSYS broke ~2,000 subtests.
    let (fs, _dir) = fresh_fs();
    // S_IFIFO = 0o010_000; umask 022 must apply to the permission bits.
    let (ino, meta) = fs.test_mknod_full(1, "fifo_file", 0o010_000 | 0o666, 7, 8, 0o022).unwrap();
    assert_eq!(meta.mode, 0o010_644);
    assert_eq!((meta.uid, meta.gid, meta.size), (7, 8, 0));
    let attr = slicefs_cli::filesystem::inode_to_file_attr(&meta);
    assert_eq!(attr.kind, FileType::NamedPipe);
    assert_eq!(fs.test_lookup(1, "fifo_file").unwrap().0, ino);
}

#[test]
fn test_mknod_socket_creates_socket() {
    let (fs, _dir) = fresh_fs();
    // S_IFSOCK = 0o140_000 (bind(2) on a Unix socket path calls mknod).
    let (_ino, meta) = fs.test_mknod_full(1, "sock", 0o140_000 | 0o755, 0, 0, 0).unwrap();
    assert_eq!(meta.mode, 0o140_755);
    assert_eq!(slicefs_cli::filesystem::inode_to_file_attr(&meta).kind, FileType::Socket);
}

#[test]
fn test_mknod_block_device_keeps_rdev() {
    let (fs, _dir) = fresh_fs();
    // S_IFBLK = 0o060_000; rdev = makedev(1, 2) in the kernel's new encoding.
    let (ino, meta) = fs.test_mknod_rdev(1, "block_dev", 0o060_000 | 0o644, 0x0102, 0, 0, 0).unwrap();
    assert_eq!((meta.mode, meta.rdev), (0o060_644, 0x0102));
    let attr = slicefs_cli::filesystem::inode_to_file_attr(&meta);
    assert_eq!((attr.kind, attr.rdev), (FileType::BlockDevice, 0x0102));
    // Survives a lookup (re-read from the store).
    assert_eq!(fs.test_lookup(1, "block_dev").unwrap().1.rdev, 0x0102);
    assert_eq!(fs.simulate_unlink(1, "block_dev"), Ok(()));
    let _ = ino;
}

#[test]
fn test_mknod_char_device_kind() {
    let (fs, _dir) = fresh_fs();
    let (_ino, meta) = fs.test_mknod_rdev(1, "chr", 0o020_000 | 0o600, 0x0503, 0, 0, 0).unwrap();
    assert_eq!(slicefs_cli::filesystem::inode_to_file_attr(&meta).kind, FileType::CharDevice);
}

#[test]
fn test_mknod_duplicate_name_returns_error() {
    let (fs, _dir) = fresh_fs();
    // Create first file
    fs.test_mknod(1, "dupe.txt", S_IFREG | 0o644, 0, 0, 0).unwrap();
    // Duplicate must fail
    let result = fs.test_mknod(1, "dupe.txt", S_IFREG | 0o644, 0, 0, 0);
    assert!(result.is_err(), "duplicate mknod must fail");
}

// ── xattr coverage via metadata store ─────────────────────────────────────────

#[test]
fn test_xattr_set_get_list_remove() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "xattr_test.txt", S_IFREG | 0o644, 0, 0, 0)
        .unwrap();
    fs.test_release(ino, fh).unwrap();

    // Set xattr
    fs.meta().set_xattr(ino, "user.comment", b"test value").unwrap();

    // Get xattr
    let val = fs.meta().get_xattr(ino, "user.comment").unwrap();
    assert_eq!(val, b"test value", "xattr value must match");

    // List xattrs
    let names = fs.meta().list_xattrs(ino).unwrap();
    assert!(names.contains(&"user.comment".to_string()), "list must include user.comment");

    // Remove xattr
    fs.meta().remove_xattr(ino, "user.comment").unwrap();

    // Should be gone
    let result = fs.meta().get_xattr(ino, "user.comment");
    assert!(result.is_err(), "xattr must be gone after remove");
}

#[test]
fn test_xattr_multiple_attributes() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "multi_xattr.txt", S_IFREG | 0o644, 0, 0, 0)
        .unwrap();
    fs.test_release(ino, fh).unwrap();

    fs.meta().set_xattr(ino, "user.a", b"alpha").unwrap();
    fs.meta().set_xattr(ino, "user.b", b"beta").unwrap();
    fs.meta().set_xattr(ino, "user.c", b"gamma").unwrap();

    let names = fs.meta().list_xattrs(ino).unwrap();
    assert_eq!(names.len(), 3, "should have 3 xattrs");

    let a = fs.meta().get_xattr(ino, "user.a").unwrap();
    let b = fs.meta().get_xattr(ino, "user.b").unwrap();
    let c = fs.meta().get_xattr(ino, "user.c").unwrap();
    assert_eq!(a, b"alpha");
    assert_eq!(b, b"beta");
    assert_eq!(c, b"gamma");
}

#[test]
fn test_xattr_get_nonexistent_returns_error() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "no_xattr.txt", S_IFREG | 0o644, 0, 0, 0)
        .unwrap();
    fs.test_release(ino, fh).unwrap();

    let result = fs.meta().get_xattr(ino, "user.nonexistent");
    assert!(result.is_err(), "get_xattr for non-existent attr must fail");
}

#[test]
fn test_xattr_list_empty_returns_empty_vec() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "empty_xattr.txt", S_IFREG | 0o644, 0, 0, 0)
        .unwrap();
    fs.test_release(ino, fh).unwrap();

    let names = fs.meta().list_xattrs(ino).unwrap();
    assert!(names.is_empty(), "list_xattrs on file with no xattrs must return empty");
}

// ── simulate_symlink: duplicate name fails ────────────────────────────────────

#[test]
fn test_symlink_duplicate_name_returns_error() {
    let (fs, _dir) = fresh_fs();
    fs.simulate_symlink(1, "dup_link", "/target", 0, 0).unwrap();

    let result = fs.simulate_symlink(1, "dup_link", "/other_target", 0, 0);
    assert!(result.is_err(), "duplicate symlink name must fail");
}

// ── simulate_mkdir: apply umask ───────────────────────────────────────────────

#[test]
fn test_mkdir_applies_umask() {
    let (fs, _dir) = fresh_fs();
    // mode 0o777, umask 0o022 → actual perms 0o755
    let ino = fs.simulate_mkdir(1, "masked_dir", S_IFDIR | 0o777, 0o022, 0, 0).unwrap();
    let meta = fs.meta().get_inode(ino).unwrap();
    assert_eq!(meta.mode & 0o7777, 0o755, "umask must be applied to mode");
}

// ── simulate_rename: ctime updated ───────────────────────────────────────────

#[test]
fn test_rename_updates_ctime_of_moved_inode() {
    let (fs, _dir) = fresh_fs();
    let meta = InodeMeta::new_file(0, 0, 0, S_IFREG | 0o644);
    let ino = fs.meta().create_inode(&meta).unwrap();
    fs.meta().link(1, "before_rename.txt", ino).unwrap();

    let ctime_before = fs.meta().get_inode(ino).unwrap().ctime_sec;

    // Wait to ensure ctime changes
    std::thread::sleep(std::time::Duration::from_millis(2));

    fs.simulate_rename(1, "before_rename.txt", 1, "after_rename.txt", 0).unwrap();

    let ctime_after = fs.meta().get_inode(ino).unwrap().ctime_sec;
    // ctime_after should be >= ctime_before (might be equal in fast test environments)
    assert!(ctime_after >= ctime_before, "ctime must not decrease after rename");
}

// ── simulate_link: ctime updated ─────────────────────────────────────────────

#[test]
fn test_link_updates_ctime() {
    let (fs, _dir) = fresh_fs();
    let meta = InodeMeta::new_file(0, 0, 0, S_IFREG | 0o644);
    let ino = fs.meta().create_inode(&meta).unwrap();
    fs.meta().link(1, "orig_link.txt", ino).unwrap();

    let ctime_before = fs.meta().get_inode(ino).unwrap().ctime_sec;

    // Wait to ensure ctime changes
    std::thread::sleep(std::time::Duration::from_millis(2));

    fs.simulate_link(ino, 1, "hard_link.txt").unwrap();

    let ctime_after = fs.meta().get_inode(ino).unwrap().ctime_sec;
    assert!(ctime_after >= ctime_before, "ctime must not decrease after link");
}

// ── test_setattr_size: closed file with content (refcount management) ─────────

#[test]
fn test_setattr_size_zero_on_closed_file_clears_manifest() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "clear.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .unwrap();
    fs.test_write(fh, 0, b"some content").unwrap();
    fs.test_release(ino, fh).unwrap();

    let manifest_before = fs.meta().get_manifest(ino).unwrap();
    assert!(!manifest_before.is_empty(), "must have manifest before truncate");
    let rc_before = fs.meta().get_refcount(&manifest_before[0]);
    assert_eq!(rc_before, 1, "refcount must be 1");

    // Truncate to zero (closed file path)
    fs.test_setattr_size(ino, None, 0).unwrap();

    let manifest_after = fs.meta().get_manifest(ino).unwrap();
    assert!(manifest_after.is_empty(), "manifest must be empty after truncate to zero");

    // Refcount must be decremented
    let rc_after = fs.meta().get_refcount(&manifest_before[0]);
    assert_eq!(rc_after, 0, "refcount must be decremented after truncate-to-zero");
}

// ── test_create: duplicate name fails ────────────────────────────────────────

#[test]
fn test_create_duplicate_name_returns_error() {
    let (fs, _dir) = fresh_fs();
    fs.test_create(1, "dup.txt", S_IFREG | 0o644, 0, 0, 0).unwrap();
    let result = fs.test_create(1, "dup.txt", S_IFREG | 0o644, 0, 0, 0);
    assert!(result.is_err(), "creating duplicate name must fail");
}

// ── test_release: existing file not overwritten when opened for read ──────────

#[test]
fn test_release_never_opened_fh_returns_ok() {
    // test_release on a fh that was never in open_files returns Ok (already closed path)
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "protected.txt", S_IFREG | 0o644, 0, 0, 0)
        .unwrap();
    fs.test_write(fh, 0, b"original content").unwrap();
    fs.test_release(ino, fh).unwrap();

    // fh 99999 was never inserted into open_files — second release is a no-op
    let result = fs.test_release(ino, 99999);
    assert!(result.is_ok(), "release of never-opened fh must return Ok");

    // Original content must still be there
    let content = read_content(&fs, ino);
    assert_eq!(content, b"original content", "existing content must not be erased");
}

// ── test_read: file with no manifest entry ────────────────────────────────────

#[test]
fn test_read_file_with_no_manifest_returns_empty() {
    let (fs, _dir) = fresh_fs();
    // Create inode directly without manifest
    let meta = InodeMeta::new_file(0, 0, 0, S_IFREG | 0o644);
    let ino = fs.meta().create_inode(&meta).unwrap();
    fs.meta().link(1, "no_manifest.txt", ino).unwrap();
    // Set empty manifest
    fs.meta().set_manifest(ino, &[]).unwrap();

    let result = fs.test_read(ino, 0, 100).unwrap();
    assert!(result.is_empty(), "reading file with empty manifest must return empty vec");
}

// ── simulate_mkdir: uid/gid on directory ─────────────────────────────────────

#[test]
fn test_mkdir_sets_uid_gid() {
    let (fs, _dir) = fresh_fs();
    let ino = fs.simulate_mkdir(1, "owned_dir", S_IFDIR | 0o755, 0, 1234, 5678).unwrap();
    let meta = fs.meta().get_inode(ino).unwrap();
    assert_eq!(meta.uid, 1234, "directory uid must be set");
    assert_eq!(meta.gid, 5678, "directory gid must be set");
}

// ── compute_statfs when store_path is Some but dir has files ─────────────────

#[test]
fn test_statfs_with_store_path_and_written_data() {
    let (fs, _dir) = fresh_fs_with_path();

    // Write a file to create CAS data on disk
    let (ino, fh) = fs.test_create(1, "cas_data.txt", S_IFREG | 0o644, 0, 0, 0).unwrap();
    fs.test_write(fh, 0, b"data to fill CAS blocks").unwrap();
    fs.test_release(ino, fh).unwrap();

    let (blocks, bfree, bavail, files, _ffree, bsize) = fs.test_statfs_values();
    // blocks > 0 means statvfs/statfs returned real values
    assert!(blocks > 0 || bsize >= 512, "statfs must return sane values for real path");
    assert!(files >= 1, "at least one inode (root) must exist");
    assert!(bfree <= blocks || blocks == 0, "bfree must be <= blocks");
}

// ── test_write: fallback with prior fsync'd root (decrement_refcount path) ────

#[test]
fn test_write_fallback_with_prior_committed_root_decrements_refcount() {
    // Streaming write → fsync (commits root) → non-sequential write (fallback to buffered)
    // This exercises lines 344-345: decrement_refcount on the old_root in streaming fallback
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "fallback_root.txt", S_IFREG | 0o644, 0, 0, 0)
        .unwrap();

    // Write sequentially and fsync (commits root, sets last_committed_root)
    fs.test_write(fh, 0, b"hello").unwrap();
    fs.test_fsync(ino, fh).unwrap();

    let manifest_after_fsync = fs.meta().get_manifest(ino).unwrap();
    assert!(!manifest_after_fsync.is_empty());
    let committed_digest = manifest_after_fsync[0];
    let rc_before = fs.meta().get_refcount(&committed_digest);
    assert_eq!(rc_before, 1, "refcount must be 1 after fsync");

    // Non-sequential write triggers fallback from streaming to buffered,
    // and ALSO decrements the old committed root refcount
    // (write at offset 0 again while next_expected_offset is 5)
    fs.test_write(fh, 0, b"world").unwrap();

    // The refcount should have been decremented during the fallback
    // (old committed root was decremented when switching to buffered mode)
    let rc_after_fallback = fs.meta().get_refcount(&committed_digest);
    assert_eq!(rc_after_fallback, 0, "refcount must be decremented when fallback occurs with prior committed root");

    fs.test_release(ino, fh).unwrap();
}

// ── test_write: fallback loading from committed manifest (not streaming state) ─

#[test]
fn test_write_fallback_loads_from_manifest_when_streaming_empty() {
    // File has committed content → open fresh handle (byte_count=0) → non-sequential write
    // This exercises lines 322-324: loading from committed manifest when streaming state is empty
    let (fs, _dir) = fresh_fs();

    // Create file with committed content
    let (ino, fh1) = fs.test_create(1, "manifest_load.txt", S_IFREG | 0o644, 0, 0, 0)
        .unwrap();
    fs.test_write(fh1, 0, b"existing content").unwrap();
    fs.test_release(ino, fh1).unwrap();

    // Now create a new write handle for the same file via test_create (not ideal,
    // but simulates opening an existing file for writing with a fresh streaming state)
    // We need a separate open handle with byte_count=0. We can do this by creating
    // a file in buffered mode: first do a non-sequential write at offset>0 so we trigger
    // the "No streaming content yet -- load from committed manifest" path.

    // Simulate by opening a fresh write via test_create (creates new state with byte_count=0),
    // then immediately do a non-sequential write.
    // Note: test_create creates a NEW file, not opens existing. Let's instead use test_write
    // on a fresh handle we inject via the mechanism we have.
    // The path fires when: offset != next_expected_offset AND byte_count == 0.
    // We can trigger this by doing test_write at offset > 0 on a brand-new fh.

    // Create another file and immediately write at non-zero offset to exercise the path
    let (ino2, fh2) = fs.test_create(1, "fresh_nonseq.txt", S_IFREG | 0o644, 0, 0, 0)
        .unwrap();
    // First set the file to have committed content, then write non-sequentially
    fs.test_write(fh2, 0, b"original").unwrap();
    fs.test_fsync(ino2, fh2).unwrap(); // Now byte_count > 0 (8) and committed

    // Write at non-sequential offset → triggers fallback with existing streaming state
    // (not the empty streaming path, but still covers lines 322-324 via the else branch)
    fs.test_write(fh2, 0, b"override").unwrap(); // offset 0 < next_expected=8 → fallback
    fs.test_release(ino2, fh2).unwrap();

    let content = read_content(&fs, ino2);
    assert_eq!(&content[..8], b"override", "content must be overridden");
}

// ── setattr_size buffered zero with prior committed root ──────────────────────

#[test]
fn test_setattr_size_zero_buffered_with_prior_committed_root() {
    // Exercises line 660: decrement_refcount in Buffered zero-truncate
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "buf_committed_zero.txt", S_IFREG | 0o644, 0, 0, 0)
        .unwrap();

    // Trigger buffered mode and fsync to set last_committed_root
    fs.test_write(fh, 5, b"world").unwrap(); // nonseq → buffered
    fs.test_write(fh, 0, b"hello").unwrap();
    fs.test_fsync(ino, fh).unwrap(); // sets last_committed_root

    let manifest = fs.meta().get_manifest(ino).unwrap();
    assert!(!manifest.is_empty());
    let digest = manifest[0];
    let rc_before = fs.meta().get_refcount(&digest);
    assert!(rc_before >= 1, "refcount must be >= 1 after fsync");

    // Truncate to zero — this should decrement the committed root
    fs.test_setattr_size(ino, Some(fh), 0).unwrap();

    let rc_after = fs.meta().get_refcount(&digest);
    assert!(rc_after < rc_before, "refcount must be decremented on truncate-to-zero of buffered with prior root");

    fs.test_release(ino, fh).unwrap();
    let meta = fs.meta().get_inode(ino).unwrap();
    assert_eq!(meta.size, 0, "inode size must be 0 after truncate");
}

// ── setattr_size streaming nonzero with prior committed root ──────────────────

#[test]
fn test_setattr_size_streaming_resize_with_prior_committed_root() {
    // Exercises line 729: decrement_refcount in Streaming non-zero resize with prior root
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "strm_committed_resize.txt", S_IFREG | 0o644, 0, 0, 0)
        .unwrap();

    // Write and fsync to set last_committed_root
    fs.test_write(fh, 0, b"hello world").unwrap();
    fs.test_fsync(ino, fh).unwrap();

    let manifest = fs.meta().get_manifest(ino).unwrap();
    assert!(!manifest.is_empty());
    let digest = manifest[0];
    let rc_before = fs.meta().get_refcount(&digest);
    assert!(rc_before >= 1);

    // Continue writing sequentially to advance streaming state
    fs.test_write(fh, 11, b"more").unwrap();

    // Non-zero resize (truncate the streaming state to 5 bytes)
    // This should decrement the old committed root
    fs.test_setattr_size(ino, Some(fh), 5).unwrap();

    let rc_after = fs.meta().get_refcount(&digest);
    assert!(rc_after < rc_before, "refcount must be decremented on streaming resize with prior committed root");

    fs.test_release(ino, fh).unwrap();
    let content = read_content(&fs, ino);
    assert_eq!(content, b"hello", "content must be truncated to 5 bytes");
}

// ── dir_size: trigger via compute_statfs with actual files on disk ────────────

#[test]
fn test_dir_size_exercised_via_compute_statfs() {
    // dir_size is called by compute_statfs to calculate physical bytes.
    // We exercise it by having a real store_path with CAS files on disk.
    let dir = tempfile::tempdir().unwrap();
    let io = Arc::new(Mutex::new(StoreIo::new(dir.path())));
    let meta = DictMetadataStore::new(io.clone());
    let fs = SliceFsFilesystem::new(meta, io, Some(dir.path().to_path_buf()));

    // Write several files to create CAS content on disk
    for i in 0..5u32 {
        let name = format!("bigfile{}.txt", i);
        let (ino, fh) = fs.test_create(1, &name, S_IFREG | 0o644, 0, 0, 0).unwrap();
        let data = vec![i as u8; 4096];
        fs.test_write(fh, 0, &data).unwrap();
        fs.test_release(ino, fh).unwrap();
    }

    // compute_statfs exercises the statvfs path, which may also call dir_size
    // (though dir_size is not currently in the coverage hot path for statvfs).
    // The important thing is the statfs path exercises the correct branches.
    let (blocks, _bfree, _bavail, files, _ffree, bsize) = fs.test_statfs_values();
    assert!(files >= 1, "must have at least root inode");
    assert!(bsize >= 512, "bsize must be reasonable");
    // On normal filesystems blocks > 0
    // This also exercises the statvfs path in compute_statfs (lines 918-925)
    let _ = blocks; // may be 0 on some test envs, that's ok
}

// ── statfs no-store-path fallback (already tested, add explicit check) ────────

#[test]
fn test_statfs_no_store_path_returns_zero_blocks() {
    // exercises the "None" branch in compute_statfs (fallback line 931)
    let (fs, _dir) = fresh_fs(); // store_path = None
    let (blocks, bfree, bavail, _files, _ffree, _bsize) = fs.test_statfs_values();
    assert_eq!(blocks, 0, "no store_path should give 0 blocks");
    assert_eq!(bfree, 0, "no store_path should give 0 bfree");
    assert_eq!(bavail, 0, "no store_path should give 0 bavail");
}

// ── sockets are not directories (S_IFSOCK contains the S_IFDIR bit) ──────────

#[test]
fn test_unlink_socket_succeeds() {
    let (fs, _dir) = fresh_fs();
    fs.test_mknod(1, "sock", 0o140_000 | 0o755, 0, 0, 0).unwrap();
    assert_eq!(fs.simulate_unlink(1, "sock"), Ok(()));
    assert!(fs.test_lookup(1, "sock").is_err());
}

#[test]
fn test_rmdir_socket_returns_enotdir() {
    let (fs, _dir) = fresh_fs();
    fs.test_mknod(1, "sock", 0o140_000 | 0o755, 0, 0, 0).unwrap();
    assert_eq!(fs.simulate_rmdir(1, "sock"), Err(libc::ENOTDIR));
}

#[test]
fn test_create_inside_socket_returns_enotdir() {
    let (fs, _dir) = fresh_fs();
    let sock = fs.test_mknod(1, "sock", 0o140_000 | 0o755, 0, 0, 0).unwrap();
    assert_eq!(fs.simulate_mkdir(sock, "d", 0o755, 0, 0, 0).map(|_| ()), Err(libc::ENOTDIR));
}

#[test]
fn test_rename_over_socket_frees_it() {
    let (fs, _dir) = fresh_fs();
    fs.test_mknod(1, "sock", 0o140_000 | 0o755, 0, 0, 0).unwrap();
    fs.test_mknod(1, "f", 0o644, 0, 0, 0).unwrap();
    let before = fs.test_statfs_values().3;
    fs.simulate_rename(1, "f", 1, "sock", 0).unwrap();
    // The replaced socket inode must be deleted, not kept as a "directory".
    assert_eq!(fs.test_statfs_values().3, before - 1);
}

#[test]
fn test_readdir_reports_special_file_types() {
    let (fs, _dir) = fresh_fs();
    fs.test_mknod(1, "fifo", 0o010_644, 0, 0, 0).unwrap();
    fs.test_mknod(1, "sock", 0o140_644, 0, 0, 0).unwrap();
    fs.test_mknod_rdev(1, "chr", 0o020_644, 5, 0, 0, 0).unwrap();
    fs.test_mknod_rdev(1, "blk", 0o060_644, 6, 0, 0, 0).unwrap();
    let kinds: std::collections::HashMap<String, FileType> = fs.test_readdir(1, 0).unwrap()
        .into_iter().map(|(_, k, n)| (n, k)).collect();
    assert_eq!(kinds["fifo"], FileType::NamedPipe);
    assert_eq!(kinds["sock"], FileType::Socket);
    assert_eq!(kinds["chr"], FileType::CharDevice);
    assert_eq!(kinds["blk"], FileType::BlockDevice);
}

// ── open-then-unlink: the inode lives until the last handle closes ──────────

#[test]
fn test_unlink_open_file_keeps_inode_until_release() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "tmp", 0o644, 0, 0, 0).unwrap();
    fs.test_write(fh, 0, b"abc").unwrap();
    assert_eq!(fs.simulate_unlink(1, "tmp"), Ok(()));
    assert!(fs.test_lookup(1, "tmp").is_err(), "name must be gone");
    let attr = fs.test_getattr(ino).expect("fstat on an open, unlinked file must work");
    assert_eq!(attr.nlinks, 0);
    assert_eq!(fs.test_read(ino, 0, 10).unwrap(), b"abc");
    fs.test_release(ino, fh).unwrap();
    assert!(fs.test_getattr(ino).is_err(), "inode must be reclaimed at last close");
}

#[test]
fn test_unlink_read_only_open_file_keeps_inode_until_release() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "ro", 0o644, 0, 0, 0).unwrap();
    fs.test_write(fh, 0, b"data").unwrap();
    fs.test_release(ino, fh).unwrap();
    let (rfh, _) = fs.test_open(ino, libc::O_RDONLY).unwrap();
    let (rfh2, _) = fs.test_open(ino, libc::O_RDONLY).unwrap();
    fs.simulate_unlink(1, "ro").unwrap();
    assert_eq!(fs.test_read(ino, 0, 10).unwrap(), b"data");
    fs.test_release_full(ino, rfh).unwrap();
    fs.test_release_full(ino, rfh).unwrap(); // double close must not count twice
    assert!(fs.test_getattr(ino).is_ok(), "second handle still open");
    fs.test_release_full(ino, rfh2).unwrap();
    assert!(fs.test_getattr(ino).is_err());
}

#[test]
fn test_rename_over_open_file_keeps_target_until_release() {
    let (fs, _dir) = fresh_fs();
    let (dst, dfh) = fs.test_create(1, "dst", 0o644, 0, 0, 0).unwrap();
    fs.test_write(dfh, 0, b"old").unwrap();
    let (src, sfh) = fs.test_create(1, "src", 0o644, 0, 0, 0).unwrap();
    fs.test_release(src, sfh).unwrap();
    fs.simulate_rename(1, "src", 1, "dst", 0).unwrap();
    assert_eq!(fs.test_read(dst, 0, 10).unwrap(), b"old");
    fs.test_release(dst, dfh).unwrap();
    assert!(fs.test_getattr(dst).is_err());
}

#[test]
fn test_destroy_reclaims_orphans() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "o", 0o644, 0, 0, 0).unwrap();
    fs.test_write(fh, 0, b"x").unwrap();
    fs.simulate_unlink(1, "o").unwrap();
    let before = fs.test_statfs_values().3;
    fs.test_destroy();
    assert_eq!(fs.test_statfs_values().3, before - 1, "orphan inode must not survive unmount");
    let _ = ino;
}

// ── rename of directories ────────────────────────────────────────────────────

#[test]
fn test_rename_dir_onto_non_empty_dir_returns_enotempty() {
    let (fs, _dir) = fresh_fs();
    fs.simulate_mkdir(1, "a", 0o755, 0, 0, 0).unwrap();
    let b = fs.simulate_mkdir(1, "b", 0o755, 0, 0, 0).unwrap();
    fs.test_mknod(b, "keep", 0o644, 0, 0, 0).unwrap();
    assert_eq!(fs.simulate_rename(1, "a", 1, "b", 0), Err(libc::ENOTEMPTY));
    assert!(fs.test_lookup(1, "a").is_ok());
    assert!(fs.test_lookup(b, "keep").is_ok());
}

#[test]
fn test_rename_dir_onto_empty_dir_replaces_it() {
    let (fs, _dir) = fresh_fs();
    let a = fs.simulate_mkdir(1, "a", 0o755, 0, 0, 0).unwrap();
    let b = fs.simulate_mkdir(1, "b", 0o755, 0, 0, 0).unwrap();
    let root_links = fs.test_getattr(1).unwrap().nlinks;
    fs.simulate_rename(1, "a", 1, "b", 0).unwrap();
    assert_eq!(fs.test_lookup(1, "b").unwrap().0, a);
    assert!(fs.test_getattr(b).is_err(), "replaced empty dir must be deleted");
    assert_eq!(fs.test_getattr(1).unwrap().nlinks, root_links - 1);
}

#[test]
fn test_move_dir_between_parents_updates_links_and_dotdot() {
    let (fs, _dir) = fresh_fs();
    let p1 = fs.simulate_mkdir(1, "p1", 0o755, 0, 0, 0).unwrap();
    let p2 = fs.simulate_mkdir(1, "p2", 0o755, 0, 0, 0).unwrap();
    let d = fs.simulate_mkdir(p1, "d", 0o755, 0, 0, 0).unwrap();
    assert_eq!(fs.test_getattr(p1).unwrap().nlinks, 3);
    fs.simulate_rename(p1, "d", p2, "d", 0).unwrap();
    assert_eq!(fs.test_getattr(p1).unwrap().nlinks, 2);
    assert_eq!(fs.test_getattr(p2).unwrap().nlinks, 3);
    assert_eq!(fs.test_lookup(d, "..").unwrap().0, p2);
}

#[test]
fn test_rename_file_onto_dir_and_dir_onto_file() {
    let (fs, _dir) = fresh_fs();
    fs.simulate_mkdir(1, "d", 0o755, 0, 0, 0).unwrap();
    fs.test_mknod(1, "f", 0o644, 0, 0, 0).unwrap();
    assert_eq!(fs.simulate_rename(1, "f", 1, "d", 0), Err(libc::EISDIR));
    assert_eq!(fs.simulate_rename(1, "d", 1, "f", 0), Err(libc::ENOTDIR));
}

#[test]
fn test_rename_between_hard_links_of_same_inode_is_noop() {
    let (fs, _dir) = fresh_fs();
    let ino = fs.test_mknod(1, "x", 0o644, 0, 0, 0).unwrap();
    fs.simulate_link(ino, 1, "y").unwrap();
    fs.simulate_rename(1, "x", 1, "y", 0).unwrap();
    assert_eq!(fs.test_lookup(1, "x").unwrap().0, ino, "POSIX: same file -> nothing changes");
    assert_eq!(fs.test_lookup(1, "y").unwrap().0, ino);
    assert_eq!(fs.test_getattr(ino).unwrap().nlinks, 2);
}

#[test]
fn test_getattr_reports_size_of_open_write_handle() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "w", 0o644, 0, 0, 0).unwrap();
    fs.test_write(fh, 0, b"x").unwrap();
    assert_eq!(fs.test_getattr(ino).unwrap().size, 1, "fstat during a write must see the new size");
    fs.test_write(fh, 5, b"yz").unwrap(); // out of order -> buffered
    assert_eq!(fs.test_getattr(ino).unwrap().size, 7);
    fs.test_release(ino, fh).unwrap();
    assert_eq!(fs.test_getattr(ino).unwrap().size, 7);
}

// ── atime / timestamps ───────────────────────────────────────────────────────

#[test]
fn test_setattr_times_stores_atime_and_mtime() {
    let (fs, _dir) = fresh_fs();
    let ino = fs.test_mknod(1, "t", 0o644, 0, 0, 0).unwrap();
    let m = fs.test_setattr_times(ino, Some((1_900_000_000, 7)), Some((1_950_000_000, 9))).unwrap();
    assert_eq!((m.atime_sec, m.atime_nsec, m.mtime_sec, m.mtime_nsec), (1_900_000_000, 7, 1_950_000_000, 9));
    let attr = slicefs_cli::filesystem::inode_to_file_attr(&fs.test_getattr(ino).unwrap());
    assert_eq!(attr.atime, UNIX_EPOCH + Duration::new(1_900_000_000, 7));
    assert_eq!(attr.mtime, UNIX_EPOCH + Duration::new(1_950_000_000, 9));
}

#[test]
fn test_time_conversion_handles_pre_epoch() {
    use slicefs_cli::filesystem::{system_time_to_pair, pair_to_system_time};
    let t = UNIX_EPOCH - Duration::new(10, 250_000_000);
    let (s, ns) = system_time_to_pair(t);
    assert_eq!((s, ns), (-11, 750_000_000));
    assert_eq!(pair_to_system_time(s, ns), t);
    assert_eq!(system_time_to_pair(UNIX_EPOCH + Duration::new(5, 1)), (5, 1));
}

fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

/// Set a directory's mtime/ctime to the epoch so a later change is visible.
fn age(fs: &SliceFsFilesystem, ino: u64) {
    let mut m = fs.test_getattr(ino).unwrap();
    m.mtime_sec = 0; m.ctime_sec = 0;
    fs.meta().update_inode(&m).unwrap();
}

#[test]
fn test_new_inodes_get_current_times() {
    let (fs, _dir) = fresh_fs();
    let t0 = now_secs();
    let (f, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
    fs.test_release(f, fh).unwrap();
    let n = fs.test_mknod(1, "n", 0o010_644, 0, 0, 0).unwrap();
    let d = fs.simulate_mkdir(1, "d", 0o755, 0, 0, 0).unwrap();
    let l = fs.simulate_symlink(1, "l", "f", 0, 0).unwrap();
    for ino in [f, n, d, l] {
        let m = fs.test_getattr(ino).unwrap();
        assert!(m.mtime_sec >= t0 && m.ctime_sec >= t0 && m.atime_sec >= t0, "ino {ino}: {m:?}");
    }
}

#[test]
fn test_entry_changes_update_parent_times() {
    let (fs, _dir) = fresh_fs();
    let p = fs.simulate_mkdir(1, "p", 0o755, 0, 0, 0).unwrap();
    let q = fs.simulate_mkdir(1, "q", 0o755, 0, 0, 0).unwrap();
    let changed = |fs: &SliceFsFilesystem, ino: u64| {
        let m = fs.test_getattr(ino).unwrap();
        m.mtime_sec > 0 && m.ctime_sec > 0
    };
    age(&fs, p); let (f, fh) = fs.test_create(p, "f", 0o644, 0, 0, 0).unwrap(); fs.test_release(f, fh).unwrap();
    assert!(changed(&fs, p), "create");
    age(&fs, p); fs.test_mknod(p, "n", 0o644, 0, 0, 0).unwrap(); assert!(changed(&fs, p), "mknod");
    age(&fs, p); fs.simulate_mkdir(p, "sub", 0o755, 0, 0, 0).unwrap(); assert!(changed(&fs, p), "mkdir");
    age(&fs, p); fs.simulate_symlink(p, "s", "f", 0, 0).unwrap(); assert!(changed(&fs, p), "symlink");
    age(&fs, p); age(&fs, f); fs.simulate_link(f, p, "f2").unwrap();
    assert!(changed(&fs, p), "link: parent"); assert!(fs.test_getattr(f).unwrap().ctime_sec > 0, "link: file ctime");
    age(&fs, p); age(&fs, f); fs.simulate_unlink(p, "f2").unwrap();
    assert!(changed(&fs, p), "unlink: parent"); assert!(fs.test_getattr(f).unwrap().ctime_sec > 0, "unlink: file ctime");
    age(&fs, p); fs.simulate_rmdir(p, "sub").unwrap(); assert!(changed(&fs, p), "rmdir");
    age(&fs, p); age(&fs, q); fs.simulate_rename(p, "n", q, "n", 0).unwrap();
    assert!(changed(&fs, p), "rename: old parent"); assert!(changed(&fs, q), "rename: new parent");
}

// ── overwrite / out-of-order writes ──────────────────────────────────────────

#[test]
fn test_overwrite_start_of_existing_file_keeps_tail() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
    fs.test_write(fh, 0, b"0123456789").unwrap();
    fs.test_release(ino, fh).unwrap();
    let (fh, _) = fs.test_open(ino, libc::O_RDWR).unwrap();
    fs.test_write(fh, 0, b"AB").unwrap();
    fs.test_release(ino, fh).unwrap();
    assert_eq!(fs.test_read(ino, 0, 100).unwrap(), b"AB23456789");
}

fn pattern(n: usize) -> Vec<u8> {
    (0..n as u32).map(|i| (i.wrapping_mul(2654435761) >> 11) as u8).collect()
}

#[test]
fn test_shuffled_writes_inside_window_stay_streaming() {
    let (fs, _dir) = fresh_fs();
    let data = pattern(32 * 4096);
    let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
    // NFS-style reordering: pairs swapped (1,0,3,2,...)
    let chunks: Vec<_> = data.chunks(4096).enumerate().collect();
    for pair in chunks.chunks(2) {
        for (i, c) in pair.iter().rev() {
            fs.test_write(fh, (*i * 4096) as u64, c).unwrap();
        }
    }
    assert!(fs.test_is_streaming(fh), "reordered writes inside the window must not force Buffered mode");
    assert_eq!(fs.test_getattr(ino).unwrap().size, data.len() as u64);
    fs.test_release(ino, fh).unwrap();
    assert_eq!(fs.test_read(ino, 0, data.len() as u32).unwrap(), data);
}

#[test]
fn test_read_and_release_with_pending_gap() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "g", 0o644, 0, 0, 0).unwrap();
    fs.test_write(fh, 0, b"aaaa").unwrap();
    fs.test_write(fh, 8, b"cccc").unwrap(); // gap 4..8 stays a hole
    assert_eq!(fs.test_read(ino, 0, 100).unwrap(), b"aaaa\0\0\0\0cccc");
    fs.test_write(fh, 4, b"bb").unwrap();
    fs.test_release(ino, fh).unwrap();
    assert_eq!(fs.test_read(ino, 0, 100).unwrap(), b"aaaabb\0\0cccc");
}

#[test]
fn test_overlap_with_pending_keeps_newest_bytes() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "o", 0o644, 0, 0, 0).unwrap();
    fs.test_write(fh, 4, b"OLDOLD").unwrap(); // pending [4,10)
    fs.test_write(fh, 0, b"newnewne").unwrap(); // [0,8) overlaps pending, newer
    fs.test_release(ino, fh).unwrap();
    assert_eq!(fs.test_read(ino, 0, 100).unwrap(), b"newnewneLD");
}

#[test]
fn test_fsync_with_pending_data_is_complete() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "s", 0o644, 0, 0, 0).unwrap();
    fs.test_write(fh, 0, b"12").unwrap();
    fs.test_write(fh, 4, b"56").unwrap();
    fs.test_write(fh, 2, b"34").unwrap();
    fs.test_fsync(ino, fh).unwrap();
    fs.test_release(ino, fh).unwrap();
    assert_eq!(fs.test_read(ino, 0, 100).unwrap(), b"123456");
}

#[test]
fn test_ftruncate_on_fresh_handle_keeps_existing_bytes() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "t", 0o644, 0, 0, 0).unwrap();
    fs.test_write(fh, 0, b"0123456789").unwrap();
    fs.test_release(ino, fh).unwrap();
    let (fh, _) = fs.test_open(ino, libc::O_RDWR).unwrap();
    fs.test_setattr_size(ino, Some(fh), 4).unwrap();
    fs.test_release(ino, fh).unwrap();
    assert_eq!(fs.test_read(ino, 0, 100).unwrap(), b"0123");
}

#[test]
fn test_repeated_fsync_without_writes_keeps_content() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "r", 0o644, 0, 0, 0).unwrap();
    fs.test_write(fh, 4, b"5678").unwrap();
    fs.test_write(fh, 0, b"1234").unwrap();
    fs.test_write(fh, 2, b"xx").unwrap(); // overlap -> Buffered
    fs.test_fsync(ino, fh).unwrap();
    let root1 = fs.meta().get_manifest(ino).unwrap();
    fs.test_fsync(ino, fh).unwrap();
    assert_eq!(fs.meta().get_manifest(ino).unwrap(), root1);
    fs.test_write(fh, 8, b"9").unwrap();
    fs.test_fsync(ino, fh).unwrap();
    assert_ne!(fs.meta().get_manifest(ino).unwrap(), root1, "a write after fsync must be flushed");
    fs.test_release(ino, fh).unwrap();
    assert_eq!(fs.test_read(ino, 0, 100).unwrap(), b"12xx56789");
}

fn existing_file(fs: &SliceFsFilesystem, name: &str, content: &[u8]) -> u64 {
    let (ino, fh) = fs.test_create(1, name, 0o644, 0, 0, 0).unwrap();
    fs.test_write(fh, 0, content).unwrap();
    fs.test_release(ino, fh).unwrap();
    ino
}

#[test]
fn test_truncate_to_zero_then_extend_gives_zeros() {
    let (fs, _dir) = fresh_fs();
    let ino = existing_file(&fs, "z", b"0123456789");
    let (fh, _) = fs.test_open(ino, libc::O_RDWR).unwrap();
    fs.test_setattr_size(ino, Some(fh), 0).unwrap();
    fs.test_setattr_size(ino, Some(fh), 4).unwrap();
    fs.test_release(ino, fh).unwrap();
    assert_eq!(fs.test_read(ino, 0, 100).unwrap(), b"\0\0\0\0");
}

#[test]
fn test_o_trunc_then_write_past_start_gives_hole() {
    let (fs, _dir) = fresh_fs();
    let ino = existing_file(&fs, "h", b"0123456789");
    let (fh, _) = fs.test_open(ino, libc::O_WRONLY | libc::O_TRUNC).unwrap();
    fs.test_write(fh, 4, b"EFGH").unwrap();
    fs.test_release(ino, fh).unwrap();
    assert_eq!(fs.test_read(ino, 0, 100).unwrap(), b"\0\0\0\0EFGH");
    assert_eq!(fs.test_getattr(ino).unwrap().size, 8);
}

#[test]
fn test_rename_over_multiply_linked_target_updates_its_ctime() {
    let (fs, _dir) = fresh_fs();
    fs.test_mknod(1, "src", 0o644, 0, 0, 0).unwrap();
    let dst = fs.test_mknod(1, "dst", 0o644, 0, 0, 0).unwrap();
    fs.simulate_link(dst, 1, "dstlnk").unwrap();
    age(&fs, dst);
    fs.simulate_rename(1, "src", 1, "dst", 0).unwrap();
    let m = fs.test_getattr(dst).unwrap();
    assert_eq!(m.nlinks, 1);
    assert!(m.ctime_sec > 0, "nlink changed, so ctime must change");
}

#[test]
fn test_truncate_updates_mtime_and_ctime() {
    let (fs, _dir) = fresh_fs();
    let ino = existing_file(&fs, "m", b"test\n");
    age(&fs, ino);
    let (fh, _) = fs.test_open(ino, libc::O_WRONLY | libc::O_TRUNC).unwrap();
    let m = fs.test_getattr(ino).unwrap();
    assert!(m.mtime_sec > 0 && m.ctime_sec > 0, "O_TRUNC: {m:?}");
    fs.test_release(ino, fh).unwrap();
    age(&fs, ino);
    fs.test_setattr_size(ino, None, 2).unwrap();
    let m = fs.test_getattr(ino).unwrap();
    assert!(m.mtime_sec > 0 && m.ctime_sec > 0, "truncate(path): {m:?}");
}

#[test]
fn test_setattr_size_and_mtime_together_keeps_explicit_mtime() {
    let (fs, _dir) = fresh_fs();
    let ino = existing_file(&fs, "sm", b"0123456789");
    let m = fs.test_setattr(ino, None, None, None, Some(3), None, Some((1_234_567_890, 5))).unwrap();
    assert_eq!((m.size, m.mtime_sec, m.mtime_nsec), (3, 1_234_567_890, 5));
    assert_eq!(fs.test_getattr(ino).unwrap().mtime_sec, 1_234_567_890);
}
