//! Integration tests for SliceFS directory operations and hard links.
//!
//! Tests mkdir, rmdir, unlink (nlinks lifecycle), link (hard links),
//! rename (same-dir, cross-dir, overwrite, NOREPLACE), and symlink/readlink.
//!
//! Uses `DictMetadataStore` and `SliceFsFilesystem` simulate_* methods directly —
//! no FUSE mount required.

use blockset::{State, Tree, FileStorageAdd, file_storage_get};
use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use slicefs_cli::filesystem::SliceFsFilesystem;
use slicefs_traits::metadata::{InodeMeta, MetadataStore};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFLNK: u32 = 0o120_000;

fn fresh_fs() -> (SliceFsFilesystem, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let io = Arc::new(Mutex::new(StoreIo::new(dir.path())));
    let meta = DictMetadataStore::new(io.clone());
    let fs = SliceFsFilesystem::new(meta, io, None);
    (fs, dir)
}

/// Create a regular file directly via metadata store (no write handle needed).
/// Returns the inode number.
fn make_file(fs: &SliceFsFilesystem, parent_ino: u64, name: &str) -> u64 {
    let meta = InodeMeta::new_file(0, 0, 0, S_IFREG | 0o644);
    let ino = fs.meta().create_inode(&meta).unwrap();
    fs.meta().link(parent_ino, name, ino).unwrap();
    ino
}

/// Read the content of a file via manifest + file_storage_get.
fn read_content(fs: &SliceFsFilesystem, ino: u64) -> Vec<u8> {
    let manifest = fs.meta().get_manifest(ino).unwrap_or_default();
    if manifest.is_empty() {
        return vec![];
    }
    let mut io = fs.io().lock().unwrap();
    file_storage_get(&mut *io, &manifest[0]).unwrap_or_default()
}

// ── Task 1: mkdir ─────────────────────────────────────────────────────────────

#[test]
fn test_mkdir_creates_directory_with_dot_entries() {
    let (fs, _dir) = fresh_fs();
    let ino = fs
        .simulate_mkdir(1, "subdir", S_IFDIR | 0o755, 0o022, 0, 0)
        .expect("mkdir should succeed");

    // The new inode should be a directory
    let meta = fs.meta().get_inode(ino).unwrap();
    assert_ne!(meta.mode & S_IFDIR, 0, "new inode must be a directory");

    // . and .. must exist
    let entries = fs.meta().list_directory(ino).unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"."), "directory must have '.' entry");
    assert!(names.contains(&".."), "directory must have '..' entry");

    // . points to self
    let dot_ino = fs.meta().lookup(ino, ".").unwrap();
    assert_eq!(dot_ino, ino);

    // .. points to parent
    let dotdot_ino = fs.meta().lookup(ino, "..").unwrap();
    assert_eq!(dotdot_ino, 1, "'..' must point to parent inode 1");
}

#[test]
fn test_mkdir_creates_entry_in_parent() {
    let (fs, _dir) = fresh_fs();
    let ino = fs
        .simulate_mkdir(1, "mydir", S_IFDIR | 0o755, 0o022, 0, 0)
        .expect("mkdir should succeed");

    // Parent must have the new dir entry
    let child_ino = fs.meta().lookup(1, "mydir").unwrap();
    assert_eq!(child_ino, ino);
}

#[test]
fn test_mkdir_increments_parent_nlinks() {
    let (fs, _dir) = fresh_fs();
    let parent_nlinks_before = fs.meta().get_inode(1).unwrap().nlinks;

    fs.simulate_mkdir(1, "newdir", S_IFDIR | 0o755, 0o022, 0, 0)
        .expect("mkdir should succeed");

    let parent_nlinks_after = fs.meta().get_inode(1).unwrap().nlinks;
    assert_eq!(
        parent_nlinks_after,
        parent_nlinks_before + 1,
        "parent nlinks must increment by 1 for each subdirectory created"
    );
}

// ── Task 1: rmdir ─────────────────────────────────────────────────────────────

#[test]
fn test_rmdir_empty_directory_succeeds() {
    let (fs, _dir) = fresh_fs();
    let ino = fs
        .simulate_mkdir(1, "emptydir", S_IFDIR | 0o755, 0o022, 0, 0)
        .unwrap();

    // Verify it exists
    assert!(fs.meta().lookup(1, "emptydir").is_ok());

    fs.simulate_rmdir(1, "emptydir")
        .expect("rmdir empty dir should succeed");

    // Entry must be gone from parent
    assert!(
        fs.meta().lookup(1, "emptydir").is_err(),
        "parent must not have the entry after rmdir"
    );

    // Inode must be deleted
    assert!(
        fs.meta().get_inode(ino).is_err(),
        "inode must be deleted after rmdir"
    );
}

#[test]
fn test_rmdir_decrements_parent_nlinks() {
    let (fs, _dir) = fresh_fs();
    fs.simulate_mkdir(1, "toremove", S_IFDIR | 0o755, 0o022, 0, 0)
        .unwrap();

    let nlinks_before = fs.meta().get_inode(1).unwrap().nlinks;

    fs.simulate_rmdir(1, "toremove").unwrap();

    let nlinks_after = fs.meta().get_inode(1).unwrap().nlinks;
    assert_eq!(
        nlinks_after,
        nlinks_before - 1,
        "parent nlinks must decrement by 1 after rmdir"
    );
}

#[test]
fn test_rmdir_nonempty_returns_enotempty() {
    let (fs, _dir) = fresh_fs();
    fs.simulate_mkdir(1, "parent_dir", S_IFDIR | 0o755, 0o022, 0, 0)
        .unwrap();
    let parent_ino = fs.meta().lookup(1, "parent_dir").unwrap();

    // Create a file inside the directory
    make_file(&fs, parent_ino, "child.txt");

    // rmdir must fail with ENOTEMPTY
    let result = fs.simulate_rmdir(1, "parent_dir");
    assert!(result.is_err(), "rmdir non-empty dir must fail");
    assert_eq!(
        result.unwrap_err(),
        libc::ENOTEMPTY,
        "error must be ENOTEMPTY"
    );
}

// ── Task 1: unlink ────────────────────────────────────────────────────────────

#[test]
fn test_unlink_removes_entry_and_decrements_nlinks() {
    let (fs, _dir) = fresh_fs();
    let ino = make_file(&fs, 1, "tounlink.txt");

    let nlinks_before = fs.meta().get_inode(ino).unwrap().nlinks;

    fs.simulate_unlink(1, "tounlink.txt")
        .expect("unlink should succeed");

    // Entry must be gone
    assert!(
        fs.meta().lookup(1, "tounlink.txt").is_err(),
        "entry must be removed after unlink"
    );

    // With nlinks==1, inode is deleted
    if nlinks_before == 1 {
        assert!(
            fs.meta().get_inode(ino).is_err(),
            "inode must be deleted when nlinks reaches 0"
        );
    }
}

#[test]
fn test_unlink_with_nlinks_1_deletes_inode_and_decrements_refcount() {
    let (fs, _dir) = fresh_fs();
    // Create file with content
    let meta = InodeMeta::new_file(0, 0, 0, S_IFREG | 0o644);
    let ino = fs.meta().create_inode(&meta).unwrap();
    fs.meta().link(1, "reftest.txt", ino).unwrap();

    // Push content and set manifest with refcount
    let content = b"hello refcount";
    let digest = {
        let mut io = fs.io().lock().unwrap();
        let mut fsa = FileStorageAdd::new(&mut *io);
        let d = State::push_all(&mut fsa, content);
        drop(fsa);
        d
    };
    fs.meta().set_manifest(ino, &[digest]).unwrap();
    fs.meta().increment_refcount(&digest);

    let rc_before = fs.meta().get_refcount(&digest);
    assert_eq!(rc_before, 1);

    fs.simulate_unlink(1, "reftest.txt")
        .expect("unlink should succeed");

    // Inode deleted (nlinks was 1)
    assert!(
        fs.meta().get_inode(ino).is_err(),
        "inode must be deleted at nlinks==0"
    );

    // Refcount decremented to 0
    let rc_after = fs.meta().get_refcount(&digest);
    assert_eq!(rc_after, 0, "refcount must reach 0 after unlink with nlinks==1");
}

#[test]
fn test_unlink_with_nlinks_2_keeps_inode() {
    let (fs, _dir) = fresh_fs();
    let ino = make_file(&fs, 1, "shared.txt");

    // Create a hard link (second name, nlinks becomes 2)
    {
        let mut inode = fs.meta().get_inode(ino).unwrap();
        inode.nlinks = 2;
        fs.meta().update_inode(&inode).unwrap();
        fs.meta().link(1, "shared2.txt", ino).unwrap();
    }

    fs.simulate_unlink(1, "shared.txt")
        .expect("unlink should succeed");

    // Entry gone, but inode still exists (nlinks==1 now)
    assert!(fs.meta().lookup(1, "shared.txt").is_err());
    let meta = fs.meta().get_inode(ino).unwrap();
    assert_eq!(meta.nlinks, 1, "nlinks must be 1 after unlink of one of two links");
}

#[test]
fn test_unlink_directory_returns_eisdir() {
    let (fs, _dir) = fresh_fs();
    fs.simulate_mkdir(1, "adir", S_IFDIR | 0o755, 0o022, 0, 0)
        .unwrap();

    let result = fs.simulate_unlink(1, "adir");
    assert!(result.is_err(), "unlink on directory must fail");
    assert_eq!(
        result.unwrap_err(),
        libc::EISDIR,
        "error must be EISDIR"
    );
}

// ── Task 1: link (hard link) ──────────────────────────────────────────────────

#[test]
fn test_link_creates_second_directory_entry() {
    let (fs, _dir) = fresh_fs();
    let ino = make_file(&fs, 1, "original.txt");

    fs.simulate_link(ino, 1, "hardlink.txt")
        .expect("link should succeed");

    // Both names must resolve to the same inode
    let ino2 = fs.meta().lookup(1, "hardlink.txt").unwrap();
    assert_eq!(ino, ino2, "hard link must resolve to same inode");
}

#[test]
fn test_link_increments_nlinks() {
    let (fs, _dir) = fresh_fs();
    let ino = make_file(&fs, 1, "orig.txt");
    let nlinks_before = fs.meta().get_inode(ino).unwrap().nlinks;

    fs.simulate_link(ino, 1, "link2.txt").expect("link should succeed");

    let nlinks_after = fs.meta().get_inode(ino).unwrap().nlinks;
    assert_eq!(
        nlinks_after,
        nlinks_before + 1,
        "nlinks must increment by 1 after hard link"
    );
}

#[test]
fn test_link_to_directory_returns_eperm() {
    let (fs, _dir) = fresh_fs();
    fs.simulate_mkdir(1, "dirlink", S_IFDIR | 0o755, 0o022, 0, 0)
        .unwrap();
    let dir_ino = fs.meta().lookup(1, "dirlink").unwrap();

    let result = fs.simulate_link(dir_ino, 1, "hardlink_to_dir");
    assert!(result.is_err(), "hard link to directory must fail");
    assert_eq!(
        result.unwrap_err(),
        libc::EPERM,
        "error must be EPERM for hard link to directory"
    );
}

#[test]
fn test_link_accessible_from_both_paths() {
    let (fs, _dir) = fresh_fs();
    // Create file with content via manifest
    let meta = InodeMeta::new_file(0, 0, 0, S_IFREG | 0o644);
    let ino = fs.meta().create_inode(&meta).unwrap();
    fs.meta().link(1, "file_a.txt", ino).unwrap();

    let content = b"shared content";
    let digest = {
        let mut io = fs.io().lock().unwrap();
        let mut fsa = FileStorageAdd::new(&mut *io);
        let d = State::push_all(&mut fsa, content);
        drop(fsa);
        d
    };
    fs.meta().set_manifest(ino, &[digest]).unwrap();

    fs.simulate_link(ino, 1, "file_b.txt")
        .expect("link should succeed");

    // Both names resolve to same inode with same manifest
    let ino_a = fs.meta().lookup(1, "file_a.txt").unwrap();
    let ino_b = fs.meta().lookup(1, "file_b.txt").unwrap();
    assert_eq!(ino_a, ino_b);
    assert_eq!(ino_a, ino);

    let manifest = fs.meta().get_manifest(ino).unwrap();
    assert_eq!(manifest, vec![digest]);
}

// ── Task 2: rename ────────────────────────────────────────────────────────────

#[test]
fn test_rename_within_same_directory() {
    let (fs, _dir) = fresh_fs();
    let ino = make_file(&fs, 1, "old_name.txt");

    fs.simulate_rename(1, "old_name.txt", 1, "new_name.txt", 0)
        .expect("rename should succeed");

    // Old name gone, new name present
    assert!(fs.meta().lookup(1, "old_name.txt").is_err(), "old name must be gone");
    let new_ino = fs.meta().lookup(1, "new_name.txt").unwrap();
    assert_eq!(new_ino, ino, "new name must resolve to same inode");
}

#[test]
fn test_rename_across_directories() {
    let (fs, _dir) = fresh_fs();
    let dir_ino = fs
        .simulate_mkdir(1, "subdir", S_IFDIR | 0o755, 0o022, 0, 0)
        .unwrap();
    let ino = make_file(&fs, 1, "move_me.txt");

    fs.simulate_rename(1, "move_me.txt", dir_ino, "moved.txt", 0)
        .expect("cross-dir rename should succeed");

    // Old entry gone from root
    assert!(fs.meta().lookup(1, "move_me.txt").is_err(), "old entry must be gone from source dir");
    // New entry present in subdir
    let new_ino = fs.meta().lookup(dir_ino, "moved.txt").unwrap();
    assert_eq!(new_ino, ino, "new entry must resolve to same inode");
}

#[test]
fn test_rename_overwrites_existing_target() {
    let (fs, _dir) = fresh_fs();
    let src_ino = make_file(&fs, 1, "src.txt");
    let _dst_ino = make_file(&fs, 1, "dst.txt");

    // Rename src over dst (overwrite)
    fs.simulate_rename(1, "src.txt", 1, "dst.txt", 0)
        .expect("rename with overwrite should succeed");

    // src is gone
    assert!(fs.meta().lookup(1, "src.txt").is_err(), "old source must be gone");
    // dst now points to src's inode
    let result_ino = fs.meta().lookup(1, "dst.txt").unwrap();
    assert_eq!(result_ino, src_ino, "target must now be source inode");
}

#[test]
fn test_rename_noreplace_returns_eexist_if_target_exists() {
    let (fs, _dir) = fresh_fs();
    make_file(&fs, 1, "alpha.txt");
    make_file(&fs, 1, "beta.txt");

    // RENAME_NOREPLACE = 1 (from Linux kernel)
    let result = fs.simulate_rename(1, "alpha.txt", 1, "beta.txt", 1);
    assert!(result.is_err(), "RENAME_NOREPLACE must fail if target exists");
    assert_eq!(
        result.unwrap_err(),
        libc::EEXIST,
        "error must be EEXIST for RENAME_NOREPLACE"
    );
}

#[test]
fn test_rename_exchange_returns_enosys() {
    let (fs, _dir) = fresh_fs();
    make_file(&fs, 1, "x.txt");
    make_file(&fs, 1, "y.txt");

    // RENAME_EXCHANGE = 2
    let result = fs.simulate_rename(1, "x.txt", 1, "y.txt", 2);
    assert!(result.is_err(), "RENAME_EXCHANGE must return error");
    assert_eq!(
        result.unwrap_err(),
        libc::ENOSYS,
        "error must be ENOSYS for RENAME_EXCHANGE"
    );
}

#[test]
fn test_rename_nonexistent_source_returns_enoent() {
    let (fs, _dir) = fresh_fs();

    let result = fs.simulate_rename(1, "ghost.txt", 1, "new.txt", 0);
    assert!(result.is_err(), "rename of non-existent source must fail");
    assert_eq!(
        result.unwrap_err(),
        libc::ENOENT,
        "error must be ENOENT for missing source"
    );
}

// ── Task 2: symlink / readlink ────────────────────────────────────────────────

#[test]
fn test_symlink_creates_slnk_inode() {
    let (fs, _dir) = fresh_fs();
    let ino = fs
        .simulate_symlink(1, "mylink", "/some/target/path", 0, 0)
        .expect("symlink should succeed");

    let meta = fs.meta().get_inode(ino).unwrap();
    assert_ne!(
        meta.mode & S_IFLNK,
        0,
        "symlink inode must have S_IFLNK type bits"
    );
}

#[test]
fn test_symlink_readlink_returns_target() {
    let (fs, _dir) = fresh_fs();
    let target = "/usr/local/share/myfile";
    let ino = fs
        .simulate_symlink(1, "link_to_myfile", target, 0, 0)
        .expect("symlink should succeed");

    let read_back = fs
        .simulate_readlink(ino)
        .expect("readlink should succeed");

    assert_eq!(read_back, target, "readlink must return the exact target path");
}

#[test]
fn test_symlink_target_in_parent_directory() {
    let (fs, _dir) = fresh_fs();
    let ino = fs
        .simulate_symlink(1, "relative_link", "../sibling/file", 0, 0)
        .expect("symlink should succeed");

    // Verify entry exists in parent
    let found_ino = fs.meta().lookup(1, "relative_link").unwrap();
    assert_eq!(found_ino, ino, "symlink entry must be in parent directory");
}

#[test]
fn test_symlink_stores_target_as_cas_content() {
    let (fs, _dir) = fresh_fs();
    let target = "/absolute/path/to/target";
    let ino = fs
        .simulate_symlink(1, "cas_link", target, 0, 0)
        .expect("symlink should succeed");

    // Target bytes must be in the manifest
    let manifest = fs.meta().get_manifest(ino).unwrap();
    assert!(!manifest.is_empty(), "symlink must have non-empty manifest");

    // Content bytes must match target string
    let content = read_content(&fs, ino);
    assert_eq!(content, target.as_bytes(), "symlink content must be target bytes");
}

#[test]
fn test_symlink_inode_size_equals_target_length() {
    let (fs, _dir) = fresh_fs();
    let target = "/some/path";
    let ino = fs
        .simulate_symlink(1, "sized_link", target, 0, 0)
        .expect("symlink should succeed");

    let meta = fs.meta().get_inode(ino).unwrap();
    assert_eq!(
        meta.size,
        target.len() as u64,
        "inode size must equal target path length"
    );
}
