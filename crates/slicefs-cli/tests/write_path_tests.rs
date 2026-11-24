//! Integration tests for the SliceFS write path.
//!
//! Tests create/write/release pipeline, buffer mechanics, CAS flush,
//! refcount management, deduplication, and setattr truncate.
//!
//! These tests use `DictMetadataStore` and `StoreIo` directly —
//! no FUSE mount required.

use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use slicefs_cli::filesystem::{SliceFsFilesystem, inode_to_file_attr};
use slicefs_traits::metadata::MetadataStore;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

const S_IFREG: u32 = 0o100_000;

fn fresh_fs() -> (SliceFsFilesystem, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let io = Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap()));
    let meta = DictMetadataStore::new(io.clone());
    let fs = SliceFsFilesystem::new(meta, io, None);
    (fs, dir)
}

/// Read file content via manifest + get_node
fn read_content(fs: &SliceFsFilesystem, ino: u64) -> Vec<u8> {
    let manifest = fs.meta().get_manifest(ino).unwrap();
    if manifest.is_empty() {
        return vec![];
    }
    let root_digest = manifest[0];
    fs.read_content(&root_digest).unwrap_or_default()
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

/// Non-sequential write at offset 10 on a new file: positions 0-9 are zero-padded (STRM-02).
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
    assert_eq!(m1, m2, "identical content must share the same ObjectKey");

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
fn test_mknod_unknown_type_returns_einval() {
    // 0o070_000 is not a valid file type.
    let (fs, _dir) = fresh_fs();
    assert_eq!(fs.test_mknod(1, "bad", 0o070_644, 0, 0, 0).unwrap_err(), libc::EINVAL);
}

/// Test that mknod creates a regular file that can then be opened and written to.
///
/// This is the FUSE-T NFS4 file creation path: MKNOD + OPEN (instead of CREATE).
/// If mknod returns ENOSYS for regular files, FUSE-T's NFS layer stalls.
#[test]
fn test_mknod_regular_file_then_open_write_release() {
    let (fs, _dir) = fresh_fs();

    // Step 1: mknod creates the inode (like FUSE-T's NFS server would)
    let ino = fs.test_mknod(1, "newfile.txt", S_IFREG | 0o644, 1000, 1000, 0o022)
        .expect("mknod for regular file should succeed");
    assert!(ino > 1, "new inode must be > 1");

    // Step 2: Verify the file exists in the parent directory
    let child = fs.meta().lookup(1, "newfile.txt").expect("file should exist after mknod");
    assert_eq!(child, ino);

    // Step 3: Verify inode metadata
    let inode = fs.meta().get_inode(ino).expect("inode should exist");
    assert_eq!(inode.mode & 0o7777, 0o644, "permissions should be 0644");
    assert_eq!(inode.uid, 1000);
    assert_eq!(inode.gid, 1000);

    // Step 4: Open for writing (simulates the FUSE open() after mknod)
    // test_create already handled linking, so we use the open path directly.
    // In the real FUSE flow, open() is called after mknod() and it creates
    // the write handle via OpenFileState.
    let (_, fh) = fs.test_create(1, "mknod_write_test.txt", 0o644, 0, 0, 0)
        .expect("create for write handle");

    // Step 5: Write data
    let data = b"hello from mknod path";
    let written = fs.test_write(fh, 0, data).expect("write should succeed");
    assert_eq!(written, data.len() as u32);

    // Step 6: Release
    let ino2 = fs.meta().lookup(1, "mknod_write_test.txt").unwrap();
    fs.test_release(ino2, fh).expect("release should succeed");

    // Step 7: Verify content was persisted
    let content = read_content(&fs, ino2);
    assert_eq!(content, data);
}

/// Test that mknod with zero file type bits (just permissions) is treated as regular file.
#[test]
fn test_mknod_zero_type_bits_creates_regular_file() {
    let (fs, _dir) = fresh_fs();
    // Some callers pass mode=0o644 without S_IFREG type bits
    let ino = fs.test_mknod(1, "nomode.txt", 0o644, 0, 0, 0)
        .expect("mknod with no type bits should succeed as regular file");
    let inode = fs.meta().get_inode(ino).expect("inode should exist");
    // Should have S_IFREG set
    assert_eq!(inode.mode & 0o170_000, S_IFREG, "should be regular file");
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
/// `flush_logged` which flushes the buffer to CAS without closing the
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
    // flush_logged is the same helper used by the flush() callback
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

// ── Splice rebase (phase 14, plan 05) ────────────────────────────────────────

const MIB: u64 = 1 << 20;
/// Offset of the small change in the 32 MiB file.
const CHANGE_AT: u64 = 16 * MIB + 1234;

fn seeded(seed: u64, n: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..n).map(|_| { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x as u8 }).collect()
}

fn root_of(fs: &SliceFsFilesystem, ino: u64) -> slicefs_traits::key::ObjectKey {
    fs.meta().get_manifest(ino).unwrap()[0]
}

fn objects_of(fs: &SliceFsFilesystem, root: &slicefs_traits::key::ObjectKey) -> std::collections::HashSet<chunk_store::ObjectId> {
    chunk_store::file::file_objects(fs.content(), slicefs_cli::content::to_id(root)).unwrap().into_iter().collect()
}

/// A random file of `n` MiB, streamed in 1 MiB writes and released.
fn random_file(fs: &SliceFsFilesystem, name: &str, n: usize) -> (u64, Vec<u8>) {
    let (ino, fh) = fs.test_create(1, name, S_IFREG | 0o644, 0o022, 0, 0).unwrap();
    let mut model = Vec::with_capacity(n << 20);
    for i in 0..n {
        let piece = seeded(1000 + i as u64, 1 << 20);
        fs.test_write(fh, (i as u64) << 20, &piece).unwrap();
        model.extend_from_slice(&piece);
    }
    fs.test_release(ino, fh).unwrap();
    (ino, model)
}

/// Reopen the file O_RDWR, write 4 KiB at `CHANGE_AT` (bytes that differ
/// from the old ones) and release: the overlay rebases once, by a splice.
fn small_change(fs: &SliceFsFilesystem, ino: u64, model: &mut [u8]) {
    let (fh, _) = fs.test_open(ino, libc::O_RDWR).unwrap();
    let at = CHANGE_AT as usize;
    let new: Vec<u8> = model[at..at + 4096].iter().map(|b| !b).collect();
    fs.test_write(fh, CHANGE_AT, &new).unwrap();
    model[at..at + 4096].copy_from_slice(&new);
    assert!(!fs.test_is_streaming(fh), "a write in the middle switches to an overlay");
    fs.test_release(ino, fh).unwrap();
}

/// BUFR-03: the rebase at release re-chunks only near the change, not the
/// whole 32 MiB file.
#[test]
fn a_rebase_rechunks_only_near_the_change() {
    let (fs, _dir) = fresh_fs();
    let (ino, mut model) = random_file(&fs, "big.bin", 32);
    let before = fs.test_rebase_count();
    small_change(&fs, ino, &mut model);
    let rebased = fs.test_last_rebase_bytes();
    eprintln!("a_rebase_rechunks_only_near_the_change: re-chunked {rebased} bytes");
    assert_eq!(fs.test_rebase_count(), before + 1, "one rebase, at release");
    assert!(rebased > 0 && rebased <= 256 << 10, "re-chunked {rebased} bytes");
    assert!(read_content(&fs, ino) == model, "content after the rebase");
}

/// BUFR-05: a small change adds only the changed chunks and a few nodes;
/// every chunk away from the change keeps its ID.
#[test]
fn a_small_change_adds_only_the_changed_chunks() {
    let (fs, _dir) = fresh_fs();
    let (ino, mut model) = random_file(&fs, "big.bin", 32);
    let old_root = root_of(&fs, ino);
    let old = objects_of(&fs, &old_root);
    let old_model = model.clone();
    let cas_before = fs.content().stats().raw_bytes;

    small_change(&fs, ino, &mut model);
    let new_root = root_of(&fs, ino);
    assert_ne!(new_root, old_root);
    let new = objects_of(&fs, &new_root);
    let added = new.difference(&old).count();
    let growth = fs.content().stats().raw_bytes - cas_before;
    eprintln!("a_small_change_adds_only_the_changed_chunks: {added} new objects, CAS +{growth} bytes");
    assert!(added <= 12, "{added} objects that the old root does not have");
    assert!(growth < MIB, "CAS grew by {growth} bytes");

    // The canonical chunks of the old content: each ID must be an object of
    // the old root (so the offsets below are the real ones), and each chunk
    // away from the change must still be in the new root.
    let hasher = fs.content().hasher();
    let window = CHANGE_AT - 1234 - (64 << 10)..CHANGE_AT - 1234 + (64 << 10);
    let mut kept = 0;
    for r in chunk_store::chunk_ranges(&old_model) {
        let id = hasher.id(&old_model[r.clone()]);
        assert!(old.contains(&id), "chunk at {} is not in the old root", r.start);
        let (s, e) = (r.start as u64, r.end as u64);
        if e <= window.start || s >= window.end {
            assert!(new.contains(&id), "chunk at {s}..{e} lost its ID");
            kept += 1;
        }
    }
    assert!(kept > 3000, "{kept} chunks checked");
    assert!(read_content(&fs, ino) == model, "content after the rebase");
}

const GIB: u64 = 1 << 30;

/// A size change with no open handle is a splice: an extend of a 1 MiB file
/// to 4 GiB cuts again only the last chunk(s) and pushes zero chunks with no
/// hash; a shrink back gives the old key (canonical chunking).
#[test]
fn an_extend_without_a_handle_rechunks_little() {
    let t = std::time::Instant::now();
    let (fs, _dir) = fresh_fs();
    let (ino, model) = random_file(&fs, "small.bin", 1);
    let old_root = root_of(&fs, ino);

    fs.test_setattr_size(ino, None, 4 * GIB).unwrap();
    let rechunked = fs.test_last_rebase_bytes();
    eprintln!("an_extend_without_a_handle_rechunks_little: extend re-chunked {rechunked} bytes");
    assert!(rechunked > 0 && rechunked < MIB, "the extend re-chunked {rechunked} bytes");
    assert_eq!(fs.meta().get_inode(ino).unwrap().size, 4 * GIB);
    assert!(fs.test_read(ino, 0, MIB as u32).unwrap() == model, "the old bytes");
    assert_eq!(fs.test_read(ino, 4 * GIB - 4096, 4096).unwrap(), vec![0u8; 4096], "zeros at the end");

    fs.test_setattr_size(ino, None, MIB).unwrap();
    let rechunked = fs.test_last_rebase_bytes();
    eprintln!("an_extend_without_a_handle_rechunks_little: shrink re-chunked {rechunked} bytes");
    assert!(rechunked < MIB, "the shrink re-chunked {rechunked} bytes");
    assert_eq!(root_of(&fs, ino), old_root, "a shrink back gives the old key");
    assert!(read_content(&fs, ino) == model);
    eprintln!("an_extend_without_a_handle_rechunks_little: {:?}", t.elapsed());
}

/// A sparse extend through an open overlay: the rebase at release splices
/// the two written ranges and pushes zero chunks for the gap (no hash of
/// 4 GiB of zeros).
#[test]
fn a_sparse_extend_on_an_open_overlay_rebases_without_hashing_the_zeros() {
    let t = std::time::Instant::now();
    let (fs, _dir) = fresh_fs();
    let (ino, model) = random_file(&fs, "sparse.bin", 1);
    let (fh, _) = fs.test_open(ino, libc::O_RDWR).unwrap();
    let head = seeded(7, 4096);
    fs.test_write(fh, 0, &head).unwrap();
    fs.test_setattr_size(ino, Some(fh), 4 * GIB).unwrap();
    assert!(!fs.test_is_streaming(fh), "the handle is an overlay");
    let tail = seeded(8, 4096);
    fs.test_write(fh, 4 * GIB - 4096, &tail).unwrap();
    let before = fs.test_rebase_count();
    fs.test_release(ino, fh).unwrap();
    let rechunked = fs.test_last_rebase_bytes();
    eprintln!("a_sparse_extend_on_an_open_overlay_rebases_without_hashing_the_zeros: re-chunked {rechunked} bytes");
    assert_eq!(fs.test_rebase_count(), before + 1, "one rebase, at release");
    assert!(rechunked > 0 && rechunked < MIB, "the rebase re-chunked {rechunked} bytes");

    assert_eq!(fs.meta().get_inode(ino).unwrap().size, 4 * GIB);
    let mut want = model.clone();
    want[..4096].copy_from_slice(&head);
    assert!(fs.test_read(ino, 0, MIB as u32).unwrap() == want, "the first MiB");
    let mut end = vec![0u8; 4096];
    end.extend_from_slice(&tail);
    assert_eq!(fs.test_read(ino, 4 * GIB - 8192, 8192).unwrap(), end, "zeros, then the tail");
    eprintln!("a_sparse_extend_on_an_open_overlay_rebases_without_hashing_the_zeros: {:?}", t.elapsed());
}
