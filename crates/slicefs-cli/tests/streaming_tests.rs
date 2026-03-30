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
