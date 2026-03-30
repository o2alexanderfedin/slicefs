//! Streaming write integration tests for SliceFS.
//!
//! Tests cover fsync mid-stream (STRM-04), read-during-write (STRM-03),
//! refcount lifecycle, empty file release, and large sequential writes.
//!
//! All tests use `DictMetadataStore` and `StoreIo` directly --
//! no FUSE mount required.

use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use slicefs_cli::filesystem::SliceFsFilesystem;
use slicefs_traits::metadata::MetadataStore;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

const S_IFREG: u32 = 0o100_000;

fn fresh_fs() -> (SliceFsFilesystem, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let io = Arc::new(Mutex::new(StoreIo::new(dir.path())));
    let meta = DictMetadataStore::new(io.clone());
    let fs = SliceFsFilesystem::new(meta, io, None);
    (fs, dir)
}

// ── Test 1: write, fsync, write more, release -- final file contains all bytes ──

#[test]
fn test_streaming_write_fsync_then_more_writes() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "file.txt", S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create should succeed");

    // Write "hello"
    fs.test_write(fh, 0, b"hello").expect("write 1 should succeed");

    // fsync mid-stream
    fs.test_fsync(ino, fh).expect("fsync should succeed");

    // Write " world" after fsync
    fs.test_write(fh, 5, b" world").expect("write 2 should succeed");

    // Release
    fs.test_release(ino, fh).expect("release should succeed");

    // Read content -- should be "hello world"
    let content = fs.test_read(ino, 0, 1024).expect("read should succeed");
    assert_eq!(content, b"hello world");
}

// ── Test 2: read-during-write without fsync returns uncommitted bytes (STRM-03) ──

#[test]
fn test_read_during_write_uncommitted() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "stream.txt", S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create should succeed");

    // Write data without fsync
    fs.test_write(fh, 0, b"streaming data").expect("write should succeed");

    // Read via test_read WITHOUT fsync -- should see uncommitted bytes
    let content = fs.test_read(ino, 0, 1024).expect("read should succeed");
    assert_eq!(content, b"streaming data");

    // Clean up
    fs.test_release(ino, fh).expect("release should succeed");
}

// ── Test 3: fsync midstream then continue writing (STRM-04) ──

#[test]
fn test_fsync_midstream_then_continue() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "chunks.txt", S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create should succeed");

    // Write chunk A
    fs.test_write(fh, 0, b"AAAA").expect("write A should succeed");

    // fsync -- commits chunk A
    fs.test_fsync(ino, fh).expect("fsync 1 should succeed");

    // Read after first fsync
    let content1 = fs.test_read(ino, 0, 1024).expect("read 1 should succeed");
    assert_eq!(content1, b"AAAA");

    // Write chunk B
    fs.test_write(fh, 4, b"BBBB").expect("write B should succeed");

    // fsync again -- commits chunks A+B
    fs.test_fsync(ino, fh).expect("fsync 2 should succeed");

    // Read after second fsync
    let content2 = fs.test_read(ino, 0, 1024).expect("read 2 should succeed");
    assert_eq!(content2, b"AAAABBBB");

    // Release
    fs.test_release(ino, fh).expect("release should succeed");

    // Final read
    let content3 = fs.test_read(ino, 0, 1024).expect("final read should succeed");
    assert_eq!(content3, b"AAAABBBB");
}

// ── Test 4: fsync refcount lifecycle -- no leak on multiple fsyncs ──

#[test]
fn test_fsync_refcount_no_leak() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "refcount.txt", S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create should succeed");

    // Write "v1" and fsync
    fs.test_write(fh, 0, b"v1").expect("write v1 should succeed");
    fs.test_fsync(ino, fh).expect("fsync 1 should succeed");

    // Write " v2" (appended) and fsync again
    fs.test_write(fh, 2, b" v2").expect("write v2 should succeed");
    fs.test_fsync(ino, fh).expect("fsync 2 should succeed");

    // Release
    fs.test_release(ino, fh).expect("release should succeed");

    // File reads correctly -- no error means store is consistent
    let content = fs.test_read(ino, 0, 1024).expect("read should succeed");
    assert_eq!(content, b"v1 v2");

    // Verify inode size is correct
    let inode = fs.meta().get_inode(ino).expect("get inode should succeed");
    assert_eq!(inode.size, 5);
}

// ── Test 5: empty file release -- empty manifest, no CAS push ──

#[test]
fn test_empty_file_release() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "empty.txt", S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create should succeed");

    // Release immediately (no writes)
    fs.test_release(ino, fh).expect("release should succeed");

    // Manifest should be empty
    let manifest = fs.meta().get_manifest(ino).expect("get manifest should succeed");
    assert!(manifest.is_empty(), "empty file should have empty manifest");

    // Read returns empty bytes
    let content = fs.test_read(ino, 0, 1024).expect("read should succeed");
    assert!(content.is_empty(), "empty file should read as empty");
}

// ── Test 6: large sequential write (1 MB in 4 KB chunks) ──

#[test]
fn test_sequential_large_write() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "large.bin", S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create should succeed");

    // Generate 1 MB of deterministic data
    let chunk_size = 4096;
    let num_chunks = 256;
    let total_size = chunk_size * num_chunks;
    let mut expected = Vec::with_capacity(total_size);

    for i in 0..num_chunks {
        let chunk: Vec<u8> = (0..chunk_size)
            .map(|j| ((i * chunk_size + j) % 256) as u8)
            .collect();
        let offset = (i * chunk_size) as u64;
        fs.test_write(fh, offset, &chunk).expect("write chunk should succeed");
        expected.extend_from_slice(&chunk);
    }

    // Release
    fs.test_release(ino, fh).expect("release should succeed");

    // Read back entire file
    let content = fs.test_read(ino, 0, total_size as u32).expect("read should succeed");
    assert_eq!(content.len(), total_size, "content length should match");
    assert_eq!(content, expected, "content should match byte-for-byte");

    // Verify inode size
    let inode = fs.meta().get_inode(ino).expect("get inode should succeed");
    assert_eq!(inode.size, total_size as u64);
}

// ── Test 7: truncate to 0 on open handle resets to empty, further writes work (STRM-05) ──

#[test]
fn test_truncate_to_zero_on_open_handle() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "trunc0.txt", S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create should succeed");

    // Write "hello world"
    fs.test_write(fh, 0, b"hello world").expect("write should succeed");

    // Truncate to 0
    fs.test_setattr_size(ino, Some(fh), 0).expect("truncate to 0 should succeed");

    // Write new content
    fs.test_write(fh, 0, b"new content").expect("write after truncate should succeed");

    // Release
    fs.test_release(ino, fh).expect("release should succeed");

    // Read -- should be "new content"
    let content = fs.test_read(ino, 0, 1024).expect("read should succeed");
    assert_eq!(content, b"new content");
}

// ── Test 8: truncate to N>0 materializes and resizes correctly (STRM-05) ──

#[test]
fn test_truncate_midstream_nonzero() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "trunc5.txt", S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create should succeed");

    // Write "hello world" (11 bytes)
    fs.test_write(fh, 0, b"hello world").expect("write should succeed");

    // Truncate to 5
    fs.test_setattr_size(ino, Some(fh), 5).expect("truncate to 5 should succeed");

    // Release
    fs.test_release(ino, fh).expect("release should succeed");

    // Read -- should be "hello" (5 bytes)
    let content = fs.test_read(ino, 0, 1024).expect("read should succeed");
    assert_eq!(content, b"hello");
}

// ── Test 9: truncate extends beyond current size with zero padding (STRM-05) ──

#[test]
fn test_truncate_extend_beyond() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "extend.txt", S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create should succeed");

    // Write "hi" (2 bytes)
    fs.test_write(fh, 0, b"hi").expect("write should succeed");

    // Truncate to 10 (extend with zeros)
    fs.test_setattr_size(ino, Some(fh), 10).expect("truncate to 10 should succeed");

    // Release
    fs.test_release(ino, fh).expect("release should succeed");

    // Read -- should be "hi" followed by 8 zero bytes (10 bytes total)
    let content = fs.test_read(ino, 0, 1024).expect("read should succeed");
    assert_eq!(content.len(), 10);
    assert_eq!(&content[..2], b"hi");
    assert_eq!(&content[2..], &[0u8; 8]);
}

// ── Test 10: truncate after fsync decrements old committed root (STRM-05) ──

#[test]
fn test_truncate_after_fsync_decrements_refcount() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "fsync_trunc.txt", S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create should succeed");

    // Write "data" and fsync (commits root)
    fs.test_write(fh, 0, b"data").expect("write should succeed");
    fs.test_fsync(ino, fh).expect("fsync should succeed");

    // Truncate to 0 (should decrement old committed root)
    fs.test_setattr_size(ino, Some(fh), 0).expect("truncate to 0 should succeed");

    // Write new data
    fs.test_write(fh, 0, b"new data").expect("write after truncate should succeed");

    // Release
    fs.test_release(ino, fh).expect("release should succeed");

    // Read -- should be "new data" (no errors means refcount lifecycle is correct)
    let content = fs.test_read(ino, 0, 1024).expect("read should succeed");
    assert_eq!(content, b"new data");
}

// ── Test 11: cas_committed guard -- fsync then release without further writes (STRM-04) ──

#[test]
fn test_cas_committed_guard_fsync_then_release() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "guard.txt", S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create should succeed");

    // Write "committed data"
    fs.test_write(fh, 0, b"committed data").expect("write should succeed");

    // fsync commits the data
    fs.test_fsync(ino, fh).expect("fsync should succeed");

    // Release WITHOUT further writes (cas_committed guard prevents empty overwrite)
    fs.test_release(ino, fh).expect("release should succeed");

    // Read -- should still be "committed data"
    let content = fs.test_read(ino, 0, 1024).expect("read should succeed");
    assert_eq!(content, b"committed data");
}

// ── Test 12: write after fsync produces correct final content (STRM-04) ──

#[test]
fn test_write_after_fsync_produces_correct_final() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "append.txt", S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create should succeed");

    // Write "part1"
    fs.test_write(fh, 0, b"part1").expect("write 1 should succeed");

    // fsync
    fs.test_fsync(ino, fh).expect("fsync should succeed");

    // Write "part2" (appended)
    fs.test_write(fh, 5, b"part2").expect("write 2 should succeed");

    // Release
    fs.test_release(ino, fh).expect("release should succeed");

    // Read -- should be "part1part2"
    let content = fs.test_read(ino, 0, 1024).expect("read should succeed");
    assert_eq!(content, b"part1part2");
}
