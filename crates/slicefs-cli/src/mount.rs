//! `mount` subcommand — loads a seeded SliceFS store and starts a FUSE session.
//!
//! ## Store layout
//!
//! ```text
//! <store>/
//!   dictionary.bin   # Full Dictionary serialized via serialize_dictionary
//!   root.bin         # 28 bytes: root Digest224 as 7 × u32 LE
//! ```
//!
//! ## Usage
//!
//! ```text
//! slicefs mount <mountpoint> --store <store> [--noatime] [--cache-size <bytes>]
//! ```
//!
//! The command blocks until the FUSE session ends (SIGTERM, Ctrl+C, or `slicefs unmount`).

use std::path::Path;

use blockset::Dictionary;
use fuser::{mount2, Config, MountOption, SessionACL};
use metadata::store::{deserialize_dictionary, DictMetadataStore};
use slicefs_traits::digest::Digest224;

use crate::filesystem::SliceFsFilesystem;

/// Load a seeded store from disk.
///
/// Reads `<store_path>/root.bin` (must be exactly 28 bytes) and
/// `<store_path>/dictionary.bin`, then reconstructs the metadata store.
///
/// Returns `(DictMetadataStore, Dictionary)` where the Dictionary is a clone
/// used for content reads (GetBytes) in the SliceFsFilesystem. The clone is
/// made BEFORE passing the original to `load_from_root`, which consumes it.
pub fn load_store(
    store_path: &Path,
) -> Result<(DictMetadataStore, Dictionary), Box<dyn std::error::Error>> {
    // Read and validate root.bin
    let root_bytes = std::fs::read(store_path.join("root.bin")).map_err(|e| {
        format!(
            "failed to read root.bin in {}: {}",
            store_path.display(),
            e
        )
    })?;

    if root_bytes.len() != 28 {
        return Err(format!(
            "root.bin must be exactly 28 bytes (Digest224), got {} bytes",
            root_bytes.len()
        )
        .into());
    }

    let mut root: Digest224 = [0u32; 7];
    for (i, word) in root.iter_mut().enumerate() {
        *word = u32::from_le_bytes(root_bytes[i * 4..i * 4 + 4].try_into().unwrap());
    }

    // Read and deserialize dictionary.bin
    let dict_bytes = std::fs::read(store_path.join("dictionary.bin")).map_err(|e| {
        format!(
            "failed to read dictionary.bin in {}: {}",
            store_path.display(),
            e
        )
    })?;

    let dict = deserialize_dictionary(&dict_bytes)
        .map_err(|e| format!("failed to deserialize dictionary.bin: {}", e))?;

    // Clone the dict BEFORE passing to load_from_root (which consumes it).
    // The clone is used for file content reads in SliceFsFilesystem.
    let content_dict = dict.clone();

    let meta = DictMetadataStore::load_from_root(dict, &root)
        .map_err(|e| format!("failed to reconstruct metadata store: {}", e))?;

    Ok((meta, content_dict))
}

/// Build the FUSE mount configuration.
///
/// Always includes: `RO`, `FSName("slicefs")`, `DefaultPermissions`.
/// Adds `NoAtime` when `noatime` is true.
/// ACL defaults to `Owner` (only the mounting user can access the filesystem).
pub fn build_mount_options(noatime: bool) -> Config {
    let mut mount_options = vec![
        MountOption::RO,
        MountOption::FSName("slicefs".to_string()),
        MountOption::DefaultPermissions,
    ];
    if noatime {
        mount_options.push(MountOption::NoAtime);
    }
    let mut cfg = Config::default();
    cfg.mount_options = mount_options;
    cfg.acl = SessionACL::Owner;
    cfg
}

/// Run the `mount` subcommand.
///
/// Loads the store, constructs the FUSE filesystem, and starts a blocking
/// `fuser::mount2` session. Blocks until the session ends (SIGTERM, Ctrl+C,
/// or `slicefs unmount`).
///
/// `_cache_size` is accepted but unused in Phase 3. Placeholder for Phase 4.
pub fn run_mount(
    store_path: &Path,
    mountpoint: &Path,
    noatime: bool,
    _cache_size: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let (meta, content_dict) = load_store(store_path)?;
    let fs = SliceFsFilesystem::new(meta, content_dict);
    let config = build_mount_options(noatime);

    println!("SliceFS mounted read-only at {}", mountpoint.display());

    mount2(fs, mountpoint, &config)?;

    // mount2 has returned — session ended, destroy() already called.
    // The SliceFsFilesystem was moved into mount2 and consumed.
    // destroy() already committed the root via meta.commit().
    // For Phase 3 read-only, no further persistence needed after mount2 returns.
    // Phase 4 will need post-session serialization when writes are introduced.
    println!("SliceFS unmounted.");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use metadata::store::{serialize_dictionary, DictMetadataStore};
    use slicefs_traits::metadata::{InodeMeta, MetadataStore};
    use tempfile::TempDir;

    const S_IFREG: u32 = 0o100_000;
    const S_IFDIR: u32 = 0o040_000;

    /// Write a valid seeded store to a temp directory for tests.
    fn write_seeded_store(store_dir: &TempDir) {
        let meta = DictMetadataStore::new();
        // Add a test file
        let file_meta = InodeMeta::new_file(0, 0, 0, S_IFREG | 0o644);
        let ino = meta.create_inode(&file_meta).unwrap();
        meta.link(1, "hello.txt", ino).unwrap();
        let root = meta.commit().unwrap();

        let dict_bytes = {
            let dict = meta.dict().lock().unwrap();
            serialize_dictionary(&*dict)
        };
        std::fs::write(store_dir.path().join("dictionary.bin"), &dict_bytes).unwrap();

        let mut root_bytes = Vec::with_capacity(28);
        for word in &root {
            root_bytes.extend_from_slice(&word.to_le_bytes());
        }
        std::fs::write(store_dir.path().join("root.bin"), &root_bytes).unwrap();
    }

    #[test]
    fn test_load_store_returns_correct_inode_1() {
        let store_dir = tempfile::tempdir().unwrap();
        write_seeded_store(&store_dir);

        let (meta, _dict) = load_store(store_dir.path()).expect("load_store failed");

        // Inode 1 must exist and be a directory (root)
        let root_meta = meta.get_inode(1).expect("root inode missing");
        let kind = root_meta.mode & 0o170_000;
        assert_eq!(kind, S_IFDIR, "inode 1 should be a directory");
    }

    #[test]
    fn test_load_store_finds_seeded_file() {
        let store_dir = tempfile::tempdir().unwrap();
        write_seeded_store(&store_dir);

        let (meta, _dict) = load_store(store_dir.path()).expect("load_store failed");

        // hello.txt was seeded in write_seeded_store
        let ino = meta.lookup(1, "hello.txt").expect("hello.txt not found");
        assert!(ino > 1, "file inode should be > 1");
    }

    #[test]
    fn test_load_store_rejects_missing_root_bin() {
        let store_dir = tempfile::tempdir().unwrap();
        // Write only dictionary.bin, no root.bin
        std::fs::write(store_dir.path().join("dictionary.bin"), b"").unwrap();

        let result = load_store(store_dir.path());
        assert!(result.is_err(), "should fail with missing root.bin");
        let msg = result.err().unwrap().to_string();
        assert!(
            msg.contains("root.bin"),
            "error should mention root.bin, got: {}",
            msg
        );
    }

    #[test]
    fn test_load_store_rejects_missing_dictionary_bin() {
        let store_dir = tempfile::tempdir().unwrap();
        // Write only root.bin (valid 28 bytes), no dictionary.bin
        let root_bytes = [0u8; 28];
        std::fs::write(store_dir.path().join("root.bin"), &root_bytes).unwrap();

        let result = load_store(store_dir.path());
        assert!(result.is_err(), "should fail with missing dictionary.bin");
        let msg = result.err().unwrap().to_string();
        assert!(
            msg.contains("dictionary.bin"),
            "error should mention dictionary.bin, got: {}",
            msg
        );
    }

    #[test]
    fn test_load_store_rejects_root_bin_wrong_size() {
        let store_dir = tempfile::tempdir().unwrap();
        // Write root.bin with wrong size (e.g., 16 bytes instead of 28)
        std::fs::write(store_dir.path().join("root.bin"), &[0u8; 16]).unwrap();
        std::fs::write(store_dir.path().join("dictionary.bin"), b"").unwrap();

        let result = load_store(store_dir.path());
        assert!(result.is_err(), "should fail with wrong root.bin size");
        let msg = result.err().unwrap().to_string();
        assert!(
            msg.contains("28 bytes") || msg.contains("16 bytes"),
            "error should mention size, got: {}",
            msg
        );
    }

    #[test]
    fn test_build_mount_options_with_noatime() {
        let config = build_mount_options(true);
        assert!(
            config.mount_options.contains(&MountOption::RO),
            "RO must always be present"
        );
        assert!(
            config.mount_options.contains(&MountOption::NoAtime),
            "NoAtime must be present when noatime=true"
        );
        assert!(
            config.mount_options.contains(&MountOption::DefaultPermissions),
            "DefaultPermissions must always be present"
        );
    }

    #[test]
    fn test_build_mount_options_without_noatime() {
        let config = build_mount_options(false);
        assert!(
            config.mount_options.contains(&MountOption::RO),
            "RO must always be present"
        );
        assert!(
            !config.mount_options.contains(&MountOption::NoAtime),
            "NoAtime must NOT be present when noatime=false"
        );
        assert!(
            config.mount_options.contains(&MountOption::DefaultPermissions),
            "DefaultPermissions must always be present"
        );
    }
}
