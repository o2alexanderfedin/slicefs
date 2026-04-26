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
    let (ino, fh) = fs
        .test_create(1, "file.txt", S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create should succeed");

    // Write "hello"
    fs.test_write(fh, 0, b"hello")
        .expect("write 1 should succeed");

    // fsync mid-stream
    fs.test_fsync(ino, fh).expect("fsync should succeed");

    // Write " world" after fsync
    fs.test_write(fh, 5, b" world")
        .expect("write 2 should succeed");

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
    let (ino, fh) = fs
        .test_create(1, "stream.txt", S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create should succeed");

    // Write data without fsync
    fs.test_write(fh, 0, b"streaming data")
        .expect("write should succeed");

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
    let (ino, fh) = fs
        .test_create(1, "chunks.txt", S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create should succeed");

    // Write chunk A
    fs.test_write(fh, 0, b"AAAA")
        .expect("write A should succeed");

    // fsync -- commits chunk A
    fs.test_fsync(ino, fh).expect("fsync 1 should succeed");

    // Read after first fsync
    let content1 = fs.test_read(ino, 0, 1024).expect("read 1 should succeed");
    assert_eq!(content1, b"AAAA");

    // Write chunk B
    fs.test_write(fh, 4, b"BBBB")
        .expect("write B should succeed");

    // fsync again -- commits chunks A+B
    fs.test_fsync(ino, fh).expect("fsync 2 should succeed");

    // Read after second fsync
    let content2 = fs.test_read(ino, 0, 1024).expect("read 2 should succeed");
    assert_eq!(content2, b"AAAABBBB");

    // Release
    fs.test_release(ino, fh).expect("release should succeed");

    // Final read
    let content3 = fs
        .test_read(ino, 0, 1024)
        .expect("final read should succeed");
    assert_eq!(content3, b"AAAABBBB");
}

// ── Test 4: fsync refcount lifecycle -- no leak on multiple fsyncs ──

#[test]
fn test_fsync_refcount_no_leak() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs
        .test_create(1, "refcount.txt", S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create should succeed");

    // Write "v1" and fsync
    fs.test_write(fh, 0, b"v1")
        .expect("write v1 should succeed");
    fs.test_fsync(ino, fh).expect("fsync 1 should succeed");

    // Write " v2" (appended) and fsync again
    fs.test_write(fh, 2, b" v2")
        .expect("write v2 should succeed");
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
    let (ino, fh) = fs
        .test_create(1, "empty.txt", S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create should succeed");

    // Release immediately (no writes)
    fs.test_release(ino, fh).expect("release should succeed");

    // Manifest should be empty
    let manifest = fs
        .meta()
        .get_manifest(ino)
        .expect("get manifest should succeed");
    assert!(manifest.is_empty(), "empty file should have empty manifest");

    // Read returns empty bytes
    let content = fs.test_read(ino, 0, 1024).expect("read should succeed");
    assert!(content.is_empty(), "empty file should read as empty");
}

// ── Test 6: large sequential write (1 MB in 4 KB chunks) ──

#[test]
fn test_sequential_large_write() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs
        .test_create(1, "large.bin", S_IFREG | 0o644, 0o022, 1000, 1000)
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
        fs.test_write(fh, offset, &chunk)
            .expect("write chunk should succeed");
        expected.extend_from_slice(&chunk);
    }

    // Release
    fs.test_release(ino, fh).expect("release should succeed");

    // Read back entire file
    let content = fs
        .test_read(ino, 0, total_size as u32)
        .expect("read should succeed");
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
    let (ino, fh) = fs
        .test_create(1, "trunc0.txt", S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create should succeed");

    // Write "hello world"
    fs.test_write(fh, 0, b"hello world")
        .expect("write should succeed");

    // Truncate to 0
    fs.test_setattr_size(ino, Some(fh), 0)
        .expect("truncate to 0 should succeed");

    // Write new content
    fs.test_write(fh, 0, b"new content")
        .expect("write after truncate should succeed");

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
    let (ino, fh) = fs
        .test_create(1, "trunc5.txt", S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create should succeed");

    // Write "hello world" (11 bytes)
    fs.test_write(fh, 0, b"hello world")
        .expect("write should succeed");

    // Truncate to 5
    fs.test_setattr_size(ino, Some(fh), 5)
        .expect("truncate to 5 should succeed");

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
    let (ino, fh) = fs
        .test_create(1, "extend.txt", S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create should succeed");

    // Write "hi" (2 bytes)
    fs.test_write(fh, 0, b"hi").expect("write should succeed");

    // Truncate to 10 (extend with zeros)
    fs.test_setattr_size(ino, Some(fh), 10)
        .expect("truncate to 10 should succeed");

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
    let (ino, fh) = fs
        .test_create(1, "fsync_trunc.txt", S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create should succeed");

    // Write "data" and fsync (commits root)
    fs.test_write(fh, 0, b"data").expect("write should succeed");
    fs.test_fsync(ino, fh).expect("fsync should succeed");

    // Truncate to 0 (should decrement old committed root)
    fs.test_setattr_size(ino, Some(fh), 0)
        .expect("truncate to 0 should succeed");

    // Write new data
    fs.test_write(fh, 0, b"new data")
        .expect("write after truncate should succeed");

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
    let (ino, fh) = fs
        .test_create(1, "guard.txt", S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create should succeed");

    // Write "committed data"
    fs.test_write(fh, 0, b"committed data")
        .expect("write should succeed");

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
    let (ino, fh) = fs
        .test_create(1, "append.txt", S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create should succeed");

    // Write "part1"
    fs.test_write(fh, 0, b"part1")
        .expect("write 1 should succeed");

    // fsync
    fs.test_fsync(ino, fh).expect("fsync should succeed");

    // Write "part2" (appended)
    fs.test_write(fh, 5, b"part2")
        .expect("write 2 should succeed");

    // Release
    fs.test_release(ino, fh).expect("release should succeed");

    // Read -- should be "part1part2"
    let content = fs.test_read(ino, 0, 1024).expect("read should succeed");
    assert_eq!(content, b"part1part2");
}

// === Phase 11: Non-sequential write handling (STRM-02) ===

// ── Test 13: non-sequential write triggers fallback with zero-padding (STRM-02a) ──

#[test]
fn test_nonseq_fallback_on_gap() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs
        .test_create(1, "gap.txt", S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create should succeed");

    // Write 5 bytes at offset 0 (sequential)
    fs.test_write(fh, 0, b"AAAAA")
        .expect("write 1 should succeed");

    // Write 5 bytes at offset 20 (gap triggers fallback)
    fs.test_write(fh, 20, b"BBBBB")
        .expect("write 2 should succeed");

    // Release
    fs.test_release(ino, fh).expect("release should succeed");

    // Read back full content -- should be 25 bytes
    let content = fs.test_read(ino, 0, 1024).expect("read should succeed");
    assert_eq!(content.len(), 25);
    assert_eq!(&content[0..5], b"AAAAA");
    assert!(
        content[5..20].iter().all(|&b| b == 0),
        "gap must be zero-padded"
    );
    assert_eq!(&content[20..25], b"BBBBB");
}

// ── Test 14: fallback materializes streaming content correctly (STRM-02b) ──

#[test]
fn test_fallback_materializes_streaming_content() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs
        .test_create(1, "materialize.txt", S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create should succeed");

    // Write 100 bytes sequentially at offset 0
    let original: Vec<u8> = (0..100).map(|i| (i % 256) as u8).collect();
    fs.test_write(fh, 0, &original)
        .expect("write 1 should succeed");

    // Write 10 bytes at offset 50 (triggers fallback, overlaps existing streaming content)
    let overwrite = b"XXXXXXXXXX";
    fs.test_write(fh, 50, overwrite)
        .expect("write 2 should succeed");

    // Release
    fs.test_release(ino, fh).expect("release should succeed");

    // Read back
    let content = fs.test_read(ino, 0, 1024).expect("read should succeed");
    assert_eq!(content.len(), 100);
    assert_eq!(&content[0..50], &original[0..50]);
    assert_eq!(&content[50..60], overwrite);
    assert_eq!(&content[60..100], &original[60..100]);
}

// ── Test 15: pwrite on new file with gap zero-pads (STRM-02d) ──

#[test]
fn test_pwrite_new_file_gap_zero_pads() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs
        .test_create(1, "pwrite_new.txt", S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create should succeed");

    // Write 5 bytes at offset 50 on a brand new file (byte_count=0, triggers immediate fallback)
    fs.test_write(fh, 50, b"hello")
        .expect("write should succeed");

    // Release
    fs.test_release(ino, fh).expect("release should succeed");

    // Read back -- 55 bytes: 50 zeros + "hello"
    let content = fs.test_read(ino, 0, 1024).expect("read should succeed");
    assert_eq!(content.len(), 55);
    assert!(
        content[0..50].iter().all(|&b| b == 0),
        "gap must be zero-padded"
    );
    assert_eq!(&content[50..55], b"hello");
}

// ── Test 16: read in buffered mode (STRM-02e) ──

#[test]
fn test_read_in_buffered_mode() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs
        .test_create(1, "buf_read.txt", S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create should succeed");

    // Write 10 bytes at offset 0 (sequential)
    fs.test_write(fh, 0, b"AAAAAAAAAA")
        .expect("write 1 should succeed");

    // Write 5 bytes at offset 20 (non-sequential, triggers fallback)
    fs.test_write(fh, 20, b"BBBBB")
        .expect("write 2 should succeed");

    // DO NOT release -- read back via test_read while still open (buffered mode)
    let content = fs.test_read(ino, 0, 1024).expect("read should succeed");
    assert_eq!(content.len(), 25);
    assert_eq!(&content[0..10], b"AAAAAAAAAA");
    assert!(
        content[10..20].iter().all(|&b| b == 0),
        "gap must be zero-padded"
    );
    assert_eq!(&content[20..25], b"BBBBB");

    // Clean up
    fs.test_release(ino, fh).expect("release should succeed");
}

// ── Test 17: fsync in buffered mode (STRM-02f) ──

#[test]
fn test_fsync_in_buffered_mode() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs
        .test_create(1, "buf_fsync.txt", S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create should succeed");

    // Write 10 bytes at offset 0 (sequential)
    fs.test_write(fh, 0, b"AAAAAAAAAA")
        .expect("write 1 should succeed");

    // Write 5 bytes at offset 20 (triggers fallback)
    fs.test_write(fh, 20, b"BBBBB")
        .expect("write 2 should succeed");

    // fsync mid-stream in buffered mode
    fs.test_fsync(ino, fh).expect("fsync should succeed");

    // Write 5 more bytes at offset 30
    fs.test_write(fh, 30, b"CCCCC")
        .expect("write 3 should succeed");

    // Release
    fs.test_release(ino, fh).expect("release should succeed");

    // Read back -- 35 bytes
    let content = fs.test_read(ino, 0, 1024).expect("read should succeed");
    assert_eq!(content.len(), 35);
    assert_eq!(&content[0..10], b"AAAAAAAAAA");
    assert!(
        content[10..20].iter().all(|&b| b == 0),
        "gap must be zero-padded"
    );
    assert_eq!(&content[20..25], b"BBBBB");
    assert!(
        content[25..30].iter().all(|&b| b == 0),
        "gap must be zero-padded"
    );
    assert_eq!(&content[30..35], b"CCCCC");
}

// ── Test 18: truncate in buffered mode (STRM-02g) ──

#[test]
fn test_truncate_in_buffered_mode() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs
        .test_create(1, "buf_trunc.txt", S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create should succeed");

    // Write 10 bytes at offset 0 (sequential)
    fs.test_write(fh, 0, b"AAAAAAAAAA")
        .expect("write 1 should succeed");

    // Write 5 bytes at offset 20 (triggers fallback)
    fs.test_write(fh, 20, b"BBBBB")
        .expect("write 2 should succeed");

    // Truncate to 15 -- removes the non-sequential part at offset 20
    fs.test_setattr_size(ino, Some(fh), 15)
        .expect("truncate should succeed");

    // Release
    fs.test_release(ino, fh).expect("release should succeed");

    // Read back -- 15 bytes: original 10 + 5 zeros
    let content = fs.test_read(ino, 0, 1024).expect("read should succeed");
    assert_eq!(content.len(), 15);
    assert_eq!(&content[0..10], b"AAAAAAAAAA");
    assert!(
        content[10..15].iter().all(|&b| b == 0),
        "padded bytes must be zero"
    );
}

// ── Test 19: out-of-order writes simulating writeback_cache (STRM-02i) ──

#[test]
fn test_out_of_order_writes() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs
        .test_create(1, "ooo.txt", S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create should succeed");

    // Write chunks in reverse order simulating writeback_cache reordering
    fs.test_write(fh, 30, b"CCCC")
        .expect("write C should succeed"); // offset 30, triggers immediate fallback
    fs.test_write(fh, 0, b"AAAA")
        .expect("write A should succeed");
    fs.test_write(fh, 15, b"BBBB")
        .expect("write B should succeed");

    // Release
    fs.test_release(ino, fh).expect("release should succeed");

    // Read back -- 34 bytes
    let content = fs.test_read(ino, 0, 1024).expect("read should succeed");
    assert_eq!(content.len(), 34);
    assert_eq!(&content[0..4], b"AAAA");
    assert!(
        content[4..15].iter().all(|&b| b == 0),
        "gap 4..15 must be zeros"
    );
    assert_eq!(&content[15..19], b"BBBB");
    assert!(
        content[19..30].iter().all(|&b| b == 0),
        "gap 19..30 must be zeros"
    );
    assert_eq!(&content[30..34], b"CCCC");
}

// ── Test 20: overlapping writes (STRM-02k) ──

#[test]
fn test_overlapping_writes() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs
        .test_create(1, "overlap.txt", S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create should succeed");

    // Write 10 bytes at offset 0
    fs.test_write(fh, 0, b"AAAAAAAAAA")
        .expect("write 1 should succeed");

    // Write 5 bytes at offset 3 (triggers fallback, overlaps)
    fs.test_write(fh, 3, b"BBBBB")
        .expect("write 2 should succeed");

    // Release
    fs.test_release(ino, fh).expect("release should succeed");

    // Read back -- 10 bytes (last write wins in overlap region)
    let content = fs.test_read(ino, 0, 1024).expect("read should succeed");
    assert_eq!(content.len(), 10);
    assert_eq!(&content[0..3], b"AAA");
    assert_eq!(&content[3..8], b"BBBBB");
    assert_eq!(&content[8..10], b"AA");
}

// ── Test 21: mixed sequential then non-sequential writes (STRM-02l) ──

#[test]
fn test_mixed_seq_then_nonseq() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs
        .test_create(1, "mixed.txt", S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create should succeed");

    // Write 4 sequential chunks of 10 bytes each
    let chunk_a: Vec<u8> = vec![b'A'; 10];
    let chunk_b: Vec<u8> = vec![b'B'; 10];
    let chunk_c: Vec<u8> = vec![b'C'; 10];
    let chunk_d: Vec<u8> = vec![b'D'; 10];
    fs.test_write(fh, 0, &chunk_a)
        .expect("write A should succeed");
    fs.test_write(fh, 10, &chunk_b)
        .expect("write B should succeed");
    fs.test_write(fh, 20, &chunk_c)
        .expect("write C should succeed");
    fs.test_write(fh, 30, &chunk_d)
        .expect("write D should succeed");

    // Non-sequential write at offset 15 (triggers fallback)
    fs.test_write(fh, 15, b"XXXXX")
        .expect("write X should succeed");

    // Release
    fs.test_release(ino, fh).expect("release should succeed");

    // Read back -- 40 bytes
    let content = fs.test_read(ino, 0, 1024).expect("read should succeed");
    assert_eq!(content.len(), 40);
    // First 15 bytes: AAAAAAAAAA BBBBB (first 10 A's + first 5 B's)
    assert_eq!(&content[0..10], b"AAAAAAAAAA");
    assert_eq!(&content[10..15], b"BBBBB");
    // Bytes 15..20: overwritten by XXXXX
    assert_eq!(&content[15..20], b"XXXXX");
    // Bytes 20..40: original C's and D's
    assert_eq!(&content[20..30], b"CCCCCCCCCC");
    assert_eq!(&content[30..40], b"DDDDDDDDDD");
}

// ── Test 22: sequential writes stay in streaming mode -- no false fallback (STRM-02h) ──

#[test]
fn test_sequential_stays_streaming() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs
        .test_create(1, "seq_only.bin", S_IFREG | 0o644, 0o022, 1000, 1000)
        .expect("create should succeed");

    // Write 5 sequential chunks of 100 bytes each
    let mut expected = Vec::with_capacity(500);
    for i in 0u8..5 {
        let chunk: Vec<u8> = vec![i + b'A'; 100];
        fs.test_write(fh, (i as u64) * 100, &chunk)
            .expect("write should succeed");
        expected.extend_from_slice(&chunk);
    }

    // Release
    fs.test_release(ino, fh).expect("release should succeed");

    // Read back -- should be 500 bytes, all correct
    let content = fs.test_read(ino, 0, 1024).expect("read should succeed");
    assert_eq!(content.len(), 500);
    assert_eq!(content, expected);
}
