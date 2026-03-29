//! Integration tests for compression in the FUSE data path.
//!
//! Tests wire-up of compress_block/decompress_block in write and read paths,
//! pre-Phase-6 migration fallback, inode.size reflects uncompressed bytes,
//! and dedup within same compressor.

use metadata::store::DictMetadataStore;
use slicefs_cli::filesystem::SliceFsFilesystem;
use slicefs_compression::{NoneCompressor, ZstdCompressor, Lz4Compressor};
use slicefs_traits::metadata::MetadataStore;
use std::sync::Arc;

const S_IFREG: u32 = 0o100_000;

/// Build a fresh filesystem with the given compressor and store_version.
fn fs_with_compressor(compressor: Arc<dyn slicefs_traits::Compressor>, store_version: u32) -> SliceFsFilesystem {
    let meta = DictMetadataStore::new();
    let dict = blockset::Dictionary::default();
    SliceFsFilesystem::new(meta, dict, None, compressor, store_version)
}

fn none_fs_v1() -> SliceFsFilesystem {
    fs_with_compressor(Arc::new(NoneCompressor::new()), 1)
}

fn none_fs_v2() -> SliceFsFilesystem {
    fs_with_compressor(Arc::new(NoneCompressor::new()), 2)
}

fn zstd_fs() -> SliceFsFilesystem {
    fs_with_compressor(Arc::new(ZstdCompressor::new(3)), 2)
}

fn lz4_fs() -> SliceFsFilesystem {
    fs_with_compressor(Arc::new(Lz4Compressor::new()), 2)
}

// ── Round-trip tests ──────────────────────────────────────────────────────────

#[test]
fn test_zstd_compress_write_read_roundtrip() {
    let fs = zstd_fs();
    let data = b"hello zstd!".repeat(100);
    let (ino, fh) = fs.test_create(1, "zstd.txt", S_IFREG | 0o644, 0, 0, 0).unwrap();
    fs.test_write(fh, 0, &data).unwrap();
    fs.test_release(ino, fh).unwrap();

    // Read back and verify
    let read_back = fs.test_read(ino, 0, data.len() as u32).unwrap();
    assert_eq!(read_back, data, "zstd: read-back should match original");
}

#[test]
fn test_lz4_compress_write_read_roundtrip() {
    let fs = lz4_fs();
    let data = b"hello lz4!".repeat(100);
    let (ino, fh) = fs.test_create(1, "lz4.txt", S_IFREG | 0o644, 0, 0, 0).unwrap();
    fs.test_write(fh, 0, &data).unwrap();
    fs.test_release(ino, fh).unwrap();

    let read_back = fs.test_read(ino, 0, data.len() as u32).unwrap();
    assert_eq!(read_back, data, "lz4: read-back should match original");
}

#[test]
fn test_none_compressor_v2_roundtrip() {
    let fs = none_fs_v2();
    let data = b"none compressor v2 test data";
    let (ino, fh) = fs.test_create(1, "none.txt", S_IFREG | 0o644, 0, 0, 0).unwrap();
    fs.test_write(fh, 0, data).unwrap();
    fs.test_release(ino, fh).unwrap();

    let read_back = fs.test_read(ino, 0, data.len() as u32).unwrap();
    assert_eq!(read_back, data, "none v2: read-back should match original");
}

// ── inode.size reflects uncompressed size ────────────────────────────────────

#[test]
fn test_inode_size_is_raw_uncompressed_size() {
    let fs = zstd_fs();
    let data = b"compressible data ".repeat(200); // 3600 bytes compressible
    let (ino, fh) = fs.test_create(1, "bigfile.txt", S_IFREG | 0o644, 0, 0, 0).unwrap();
    fs.test_write(fh, 0, &data).unwrap();
    fs.test_release(ino, fh).unwrap();

    let inode = fs.meta().get_inode(ino).unwrap();
    assert_eq!(
        inode.size,
        data.len() as u64,
        "inode.size should be raw (uncompressed) size"
    );
}

// ── Pre-Phase-6 migration fallback ───────────────────────────────────────────

#[test]
fn test_pre_phase6_blocks_readable_with_store_version_1() {
    // store_version=1 means no compression headers — read path returns raw bytes
    let fs = none_fs_v1();
    let data = b"old pre-phase-6 content";
    let (ino, fh) = fs.test_create(1, "old.txt", S_IFREG | 0o644, 0, 0, 0).unwrap();
    fs.test_write(fh, 0, data).unwrap();
    fs.test_release(ino, fh).unwrap();

    let read_back = fs.test_read(ino, 0, data.len() as u32).unwrap();
    assert_eq!(read_back, data, "v1 store: raw bytes should be returned unchanged");
}

#[test]
fn test_decompress_fallback_for_pre_phase6_blocks() {
    // Write raw (no compression header) via store_version=1,
    // then read via store_version=2 filesystem (simulates first Phase-6 mount).
    // The fallback in the read path should handle it gracefully.
    use blockset::{Dictionary, State, Tree};
    use slicefs_traits::digest::Digest224;
    use slicefs_traits::metadata::MetadataStore;
    use std::sync::{Arc, Mutex};

    // Write raw bytes directly into a dict (no compression header)
    let meta = DictMetadataStore::new();
    let dict_arc = Arc::new(Mutex::new(Dictionary::default()));
    let raw_data = b"pre-phase-6 raw content no header";
    let digest = {
        let mut dict = dict_arc.lock().unwrap();
        State::push_all(&mut *dict, raw_data)
    };

    // Create a fresh inode and set manifest with the raw digest
    let inode = slicefs_traits::metadata::InodeMeta::new_file(raw_data.len() as u64, 0, 0, S_IFREG | 0o644);
    let ino = meta.create_inode(&inode).unwrap();
    meta.link(1, "legacy.txt", ino).unwrap();
    meta.set_manifest(ino, &[digest]).unwrap();
    meta.increment_refcount(&digest);

    // Now mount it with store_version=2 and zstd compressor
    let dict_clone = {
        let dict = dict_arc.lock().unwrap();
        dict.clone()
    };
    let fs = SliceFsFilesystem::new(
        meta,
        dict_clone,
        None,
        Arc::new(ZstdCompressor::new(3)),
        2, // store_version=2: read path will try decompress, should fall back
    );

    // Should not panic or return EIO; should return the original raw bytes
    let read_back = fs.test_read(ino, 0, raw_data.len() as u32).unwrap();
    assert_eq!(
        read_back, raw_data,
        "pre-Phase-6 block: fallback should return raw bytes unchanged"
    );
}

// ── Dedup within same compressor ─────────────────────────────────────────────

#[test]
fn test_same_content_same_compressor_same_digest() {
    let fs = zstd_fs();
    let data = b"dedup test content ".repeat(50);

    let (ino1, fh1) = fs.test_create(1, "dedup1.txt", S_IFREG | 0o644, 0, 0, 0).unwrap();
    fs.test_write(fh1, 0, &data).unwrap();
    fs.test_release(ino1, fh1).unwrap();

    let (ino2, fh2) = fs.test_create(1, "dedup2.txt", S_IFREG | 0o644, 0, 0, 0).unwrap();
    fs.test_write(fh2, 0, &data).unwrap();
    fs.test_release(ino2, fh2).unwrap();

    let manifest1 = fs.meta().get_manifest(ino1).unwrap();
    let manifest2 = fs.meta().get_manifest(ino2).unwrap();
    assert_eq!(manifest1, manifest2, "identical content + compressor should produce identical Digest224 (dedup)");
}

// ── Offset slicing after decompression ───────────────────────────────────────

#[test]
fn test_read_with_offset_returns_correct_slice() {
    let fs = zstd_fs();
    let data = b"0123456789abcdefghij".repeat(10); // 200 bytes
    let (ino, fh) = fs.test_create(1, "slice.txt", S_IFREG | 0o644, 0, 0, 0).unwrap();
    fs.test_write(fh, 0, &data).unwrap();
    fs.test_release(ino, fh).unwrap();

    let slice = fs.test_read(ino, 10, 10).unwrap();
    assert_eq!(&slice, b"abcdefghij", "read with offset should return correct slice from decompressed data");
}

// ── Symlink compression round-trip ───────────────────────────────────────────

#[test]
fn test_symlink_compress_readlink_roundtrip() {
    let fs = zstd_fs();
    let target = "/some/very/long/path/that/should/compress/well/because/it/is/repetitive";

    let ino = fs.simulate_symlink(1, "mylink", target, 0, 0).unwrap();
    let read_target = fs.simulate_readlink(ino).unwrap();
    assert_eq!(read_target, target, "symlink target should round-trip through compression");
}

// ── Setattr truncate preserves compression ───────────────────────────────────

#[test]
fn test_setattr_truncate_preserves_content_and_compresses() {
    let fs = zstd_fs();
    let data = b"hello world this is test content for truncation";
    let (ino, fh) = fs.test_create(1, "trunc.txt", S_IFREG | 0o644, 0, 0, 0).unwrap();
    fs.test_write(fh, 0, data).unwrap();
    fs.test_release(ino, fh).unwrap();

    // Truncate to first 11 bytes
    fs.test_setattr_size(ino, None, 11).unwrap();

    let inode = fs.meta().get_inode(ino).unwrap();
    assert_eq!(inode.size, 11);

    let read_back = fs.test_read(ino, 0, 100).unwrap();
    assert_eq!(&read_back, b"hello world", "truncated content should match");
}

// ── Existing tests still pass with NoneCompressor + v1 ───────────────────────

#[test]
fn test_backward_compat_none_compressor_v1() {
    // Proves existing behavior unchanged when store_version=1 and NoneCompressor
    let fs = none_fs_v1();
    let data = b"backward compat test";
    let (ino, fh) = fs.test_create(1, "compat.txt", S_IFREG | 0o644, 0, 0, 0).unwrap();
    fs.test_write(fh, 0, data).unwrap();
    fs.test_release(ino, fh).unwrap();

    let inode = fs.meta().get_inode(ino).unwrap();
    assert_eq!(inode.size, data.len() as u64);

    let read_back = fs.test_read(ino, 0, data.len() as u32).unwrap();
    assert_eq!(read_back, data);
}
