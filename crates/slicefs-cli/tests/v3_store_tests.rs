//! V3 store invariant tests.
//!
//! Proves the v3 raw-bytes store format:
//!   DECOMP-01: Raw bytes are written to CAS without a compression header
//!   DECOMP-02: Bytes retrieved from CAS are identical to bytes written
//!   DECOMP-03: test_read returns the exact raw bytes that were written
//!   DECOMP-04: Identical raw content produces the same Digest224 (dedup)

use blockset::file_storage_get;
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

/// DECOMP-01, DECOMP-02: Raw bytes are written to CAS with no AlgorithmId header.
///
/// In v1/v2 format, the CAS stored `[AlgorithmId byte] + [compressed_or_raw_bytes]`.
/// In v3, the content bytes are stored directly: `[raw_bytes]`.
///
/// This test writes known bytes, reads them back via `file_storage_get` at
/// the manifest digest, and asserts:
///   - Stored length == raw input length (no extra header byte)
///   - Stored bytes == raw input exactly (no header, no encoding)
#[test]
fn test_write_raw_no_compression() {
    let (fs, _dir) = fresh_fs();
    let content = b"hello raw v3 world";

    let (ino, fh) = fs
        .test_create(1, "raw.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create should succeed");
    fs.test_write(fh, 0, content).expect("write should succeed");
    fs.test_release(ino, fh).expect("release should succeed");

    // Get the manifest digest — the root Digest224 of the stored content
    let manifest = fs
        .meta()
        .get_manifest(ino)
        .expect("manifest must exist after release");
    assert!(
        !manifest.is_empty(),
        "manifest must be non-empty after writing content"
    );

    // Read bytes back via file_storage_get — bypasses the filesystem read path
    let stored = {
        let mut io = fs.io().lock().unwrap();
        file_storage_get(&mut *io, &manifest[0])
            .expect("file_storage_get must succeed for stored content")
    };

    // DECOMP-01: no compression header — length must equal raw input length exactly
    assert_eq!(
        stored.len(),
        content.len(),
        "stored byte count must equal raw input (no 1-byte AlgorithmId header)"
    );

    // DECOMP-02: bytes must be exactly the raw input (no transformation)
    assert_eq!(
        stored, content,
        "stored bytes must be identical to raw input bytes"
    );
}

/// DECOMP-03: test_read returns the exact raw bytes that were written.
///
/// Exercises the filesystem read path (manifest → file_storage_get → slice).
/// Asserts the round-trip: write raw bytes, read back via test_read, exact match.
#[test]
fn test_read_returns_raw_bytes() {
    let (fs, _dir) = fresh_fs();
    let content = b"v3 read path round-trip test content";

    let (ino, fh) = fs
        .test_create(1, "roundtrip.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create should succeed");
    fs.test_write(fh, 0, content).expect("write should succeed");
    fs.test_release(ino, fh).expect("release should succeed");

    // Read back via the filesystem read path
    let read_back = fs
        .test_read(ino, 0, content.len() as u32)
        .expect("test_read must succeed");

    // DECOMP-03: returned bytes must exactly equal the original raw content
    assert_eq!(
        read_back, content,
        "test_read must return the exact raw bytes that were written"
    );
}

/// DECOMP-04: Identical raw content produces the same Digest224 (content-addressed dedup).
///
/// Two files with the same content must share the same manifest digest.
/// This proves that dedup works on raw bytes (not on compressed bytes,
/// which could differ by compressor state/level).
#[test]
fn test_raw_content_dedup() {
    let (fs, _dir) = fresh_fs();
    let content = b"dedup-me: identical content written twice";

    // File 1
    let (ino1, fh1) = fs
        .test_create(1, "dedup_a.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create file 1 should succeed");
    fs.test_write(fh1, 0, content)
        .expect("write to file 1 should succeed");
    fs.test_release(ino1, fh1)
        .expect("release file 1 should succeed");

    // File 2
    let (ino2, fh2) = fs
        .test_create(1, "dedup_b.txt", S_IFREG | 0o644, 0o022, 0, 0)
        .expect("create file 2 should succeed");
    fs.test_write(fh2, 0, content)
        .expect("write to file 2 should succeed");
    fs.test_release(ino2, fh2)
        .expect("release file 2 should succeed");

    // Get manifests for both files
    let manifest1 = fs
        .meta()
        .get_manifest(ino1)
        .expect("manifest for file 1 must exist");
    let manifest2 = fs
        .meta()
        .get_manifest(ino2)
        .expect("manifest for file 2 must exist");

    assert!(!manifest1.is_empty(), "file 1 manifest must be non-empty");
    assert!(!manifest2.is_empty(), "file 2 manifest must be non-empty");

    // DECOMP-04: same raw content → same Digest224 → dedup
    assert_eq!(
        manifest1[0], manifest2[0],
        "identical raw content must produce the same Digest224 (content-addressed dedup)"
    );
}
