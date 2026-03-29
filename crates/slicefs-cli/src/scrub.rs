//! `scrub` subcommand — verify integrity of all stored blocks in a SliceFS store.
//!
//! ## Usage
//!
//! ```text
//! slicefs scrub <store>
//! slicefs scrub <store> --json
//! ```
//!
//! ## How integrity verification works
//!
//! The blockset `Dictionary` is a Merkle tree: each entry maps a 224-bit key to
//! two 256-bit children (Branches). The key is derived as:
//!
//!   `key = to_digest224(compress(left, right))`
//!
//! So for every `(key, [left, right])` entry, we can re-derive the expected key
//! and compare it against the stored key. A mismatch means the dictionary entry
//! has been corrupted (either the key or the branches).
//!
//! ## Online stores (mounted)
//!
//! If `mount.lock` is present, we print a warning and continue. The active
//! write-ahead segment is not included in closed segments, so in-flight data
//! is not covered by this scan.

use std::path::Path;

use serde::Serialize;

use blockset::Dictionary;
use metadata::segment::{load_store_from_segments, migrate_legacy_store};
use sha2_compress::{Sha2, SHA224};

/// A single corrupted dictionary entry.
#[derive(Debug, Serialize)]
pub struct CorruptedBlock {
    /// The stored key (hex representation of Digest224).
    pub stored_key: String,
    /// The recomputed key, which should match stored_key.
    pub expected_key: String,
    /// Description of the corruption type.
    pub corruption_type: String,
}

/// Scrub report for the entire store.
#[derive(Debug, Serialize)]
pub struct ScrubReport {
    pub blocks_verified: usize,
    pub corrupted_blocks: usize,
    pub status: String,
    pub mounted: bool,
    pub corrupted: Vec<CorruptedBlock>,
}

/// Run the `scrub` subcommand.
///
/// Returns `Ok(())` if no corruption is found. Returns `Err(...)` if any
/// corrupted blocks are detected (causes a non-zero exit code via main.rs).
pub fn run_scrub(store_path: &Path, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    // Check if store is currently mounted.
    let lock_path = store_path.join("mount.lock");
    let mounted = lock_path.exists();
    if mounted {
        let msg = "Store is mounted; scrubbing closed segments only. Active writes not covered.";
        if json {
            eprintln!("{{\"warning\": \"{}\"}}", msg);
        } else {
            eprintln!("Warning: {}", msg);
        }
    }

    // Migrate legacy format (dictionary.bin + root.bin) to segments/ if needed.
    if store_path.join("dictionary.bin").exists() {
        migrate_legacy_store(store_path)
            .map_err(|e| format!("migration failed: {}", e))?;
    } else if !store_path.join("segments").is_dir() {
        return Err(format!(
            "store not found at {}: no dictionary.bin or segments/ directory",
            store_path.display()
        ).into());
    }

    let segs_dir = store_path.join("segments");

    // Load store from segment replay.
    let (dict, _root_opt, _snapshots) = load_store_from_segments(&segs_dir)
        .map_err(|e| format!("failed to load segments: {}", e))?;

    // Verify all dictionary entries.
    let (blocks_verified, corrupted) = verify_dictionary(&dict);

    let corrupted_count = corrupted.len();
    let status = if corrupted_count == 0 {
        "clean".to_string()
    } else {
        format!("corrupted ({} blocks)", corrupted_count)
    };

    let report = ScrubReport {
        blocks_verified,
        corrupted_blocks: corrupted_count,
        status,
        mounted,
        corrupted,
    };

    if json {
        let output = serde_json::to_string_pretty(&report)
            .map_err(|e| format!("JSON serialization error: {}", e))?;
        println!("{}", output);
    } else {
        print_human_report(&report);
    }

    if report.corrupted_blocks > 0 {
        Err(format!(
            "scrub found {} corrupted block(s)",
            report.corrupted_blocks
        )
        .into())
    } else {
        Ok(())
    }
}

/// Verify all entries in the dictionary by re-deriving keys from branch pairs.
///
/// Returns `(verified_count, corrupted_entries)`.
///
/// ## Verification algorithm
///
/// All dictionary keys are SHA-224 hashes. The blockset Dictionary inserts a key only
/// when `to_digest224(result)` succeeds — which requires the hash suffix `0xFFFF_FFFF`
/// to be present. This suffix is set by SHA224 compression. Therefore, each entry
/// `(key, [left, right])` satisfies:
///
///   `key = SHA224.compress(left, right)[..7]`
///
/// We re-derive the expected key using `SHA224.compress` and compare with the stored key.
fn verify_dictionary(dict: &Dictionary) -> (usize, Vec<CorruptedBlock>) {
    let mut blocks_verified = 0usize;
    let mut corrupted = Vec::new();

    for (stored_key, branches) in dict.iter() {
        blocks_verified += 1;

        let [left, right] = branches;
        // Re-derive the SHA-224 hash of this branch pair.
        let recomputed = SHA224.compress(left, right);
        // Extract the Digest224 (first 7 u32s) from the hash result.
        let mut expected_key: [u32; 7] = [0; 7];
        expected_key.copy_from_slice(&recomputed[..7]);

        if expected_key != *stored_key {
            corrupted.push(CorruptedBlock {
                stored_key: digest224_to_hex(stored_key),
                expected_key: digest224_to_hex(&expected_key),
                corruption_type: "key_mismatch".to_string(),
            });
        }
    }

    (blocks_verified, corrupted)
}


/// Convert a `Digest224` (`[u32; 7]`) to a hex string for display.
fn digest224_to_hex(key: &[u32; 7]) -> String {
    key.iter()
        .map(|w| format!("{:08x}", w))
        .collect::<Vec<_>>()
        .join("")
}

/// Print scrub report in human-readable format.
fn print_human_report(report: &ScrubReport) {
    println!("SliceFS Scrub Report");
    println!("====================");
    println!("Blocks verified  : {}", report.blocks_verified);
    println!("Corrupted blocks : {}", report.corrupted_blocks);
    println!("Status           : {}", report.status);
    println!("Mounted          : {}", if report.mounted { "yes" } else { "no" });

    if !report.corrupted.is_empty() {
        println!();
        println!("Corrupted Entries");
        println!("-----------------");
        for block in &report.corrupted {
            println!("  stored_key  : {}", block.stored_key);
            println!("  expected_key: {}", block.expected_key);
            println!("  type        : {}", block.corruption_type);
            println!();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use metadata::store::{serialize_dictionary, DictMetadataStore};
    use metadata::wal::WalConfig;
    use slicefs_traits::metadata::{InodeMeta, MetadataStore};
    use tempfile::TempDir;

    const S_IFREG: u32 = 0o100_000;

    /// Create an empty segments directory so load_store_from_segments succeeds.
    fn make_empty_store() -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("segments")).unwrap();
        dir
    }

    /// Write a valid seeded store in legacy format (dictionary.bin + root.bin).
    fn write_legacy_store(dir: &TempDir) {
        let meta = DictMetadataStore::new();
        let file_meta = InodeMeta::new_file(0, 0, 0, S_IFREG | 0o644);
        let ino = meta.create_inode(&file_meta).unwrap();
        meta.link(1, "hello.txt", ino).unwrap();
        let root = meta.commit().unwrap();

        let dict_bytes = {
            let dict = meta.dict().lock().unwrap();
            serialize_dictionary(&*dict)
        };
        std::fs::write(dir.path().join("dictionary.bin"), &dict_bytes).unwrap();

        let mut root_bytes = Vec::with_capacity(28);
        for word in &root {
            root_bytes.extend_from_slice(&word.to_le_bytes());
        }
        std::fs::write(dir.path().join("root.bin"), &root_bytes).unwrap();
    }

    /// Write a valid seeded store in segment format.
    fn write_segment_store(dir: &TempDir) {
        use metadata::wal::create_wal;
        let segs_dir = dir.path().join("segments");
        std::fs::create_dir_all(&segs_dir).unwrap();

        let wal = create_wal(WalConfig::PerOp, dir.path(), 1).unwrap();
        let mut meta = DictMetadataStore::new();
        meta.set_wal(wal);

        let file_meta = InodeMeta::new_file(0, 0, 0, S_IFREG | 0o644);
        let ino = meta.create_inode(&file_meta).unwrap();
        meta.link(1, "hello.txt", ino).unwrap();
        meta.commit().unwrap();
        meta.shutdown_wal().unwrap();
    }

    #[test]
    fn test_scrub_empty_store_is_clean() {
        let store = make_empty_store();
        let result = run_scrub(store.path(), false);
        assert!(result.is_ok(), "empty store should scrub clean: {:?}", result);
    }

    #[test]
    fn test_scrub_empty_store_json_is_clean() {
        let store = make_empty_store();
        let result = run_scrub(store.path(), true);
        assert!(result.is_ok(), "empty store --json scrub should be clean: {:?}", result);
    }

    #[test]
    fn test_scrub_legacy_store_is_clean() {
        let dir = tempfile::tempdir().unwrap();
        write_legacy_store(&dir);
        let result = run_scrub(dir.path(), false);
        assert!(result.is_ok(), "legacy store should scrub clean: {:?}", result);
    }

    #[test]
    fn test_scrub_legacy_store_migrates_to_segments() {
        let dir = tempfile::tempdir().unwrap();
        write_legacy_store(&dir);
        run_scrub(dir.path(), false).unwrap();
        // After scrub, dictionary.bin should be gone and segments/ should exist.
        assert!(
            !dir.path().join("dictionary.bin").exists(),
            "dictionary.bin should be removed after migration"
        );
        assert!(
            dir.path().join("segments").is_dir(),
            "segments/ should exist after migration"
        );
    }

    #[test]
    fn test_scrub_legacy_store_json_is_clean() {
        let dir = tempfile::tempdir().unwrap();
        write_legacy_store(&dir);
        let result = run_scrub(dir.path(), true);
        assert!(result.is_ok(), "legacy store --json scrub should be clean: {:?}", result);
    }

    #[test]
    fn test_scrub_segment_store_is_clean() {
        let dir = tempfile::tempdir().unwrap();
        write_segment_store(&dir);
        let result = run_scrub(dir.path(), false);
        assert!(result.is_ok(), "segment store should scrub clean: {:?}", result);
    }

    #[test]
    fn test_scrub_missing_store_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        // No dictionary.bin, no segments/ — should fail.
        let result = run_scrub(dir.path(), false);
        assert!(result.is_err(), "missing store should return error");
        let msg = result.err().unwrap().to_string();
        assert!(
            msg.contains("store not found"),
            "error should mention 'store not found', got: {}",
            msg
        );
    }

    #[test]
    fn test_verify_fresh_dictionary_is_clean() {
        // A freshly constructed DictMetadataStore has valid dictionary entries.
        let meta = DictMetadataStore::new();
        let dict = meta.dict().lock().unwrap().clone();

        let (verified, corrupted) = verify_dictionary(&dict);
        assert!(corrupted.is_empty(),
            "fresh dictionary should have no corrupted entries; found: {:?}", corrupted);
        assert_eq!(verified, dict.len(), "should verify all {} entries", dict.len());
    }

    #[test]
    fn test_digest224_to_hex_length_and_format() {
        let key: [u32; 7] = [1u32, 2, 3, 4, 5, 6, 7];
        let hex = digest224_to_hex(&key);
        assert_eq!(hex.len(), 56, "Digest224 hex must be 56 chars (7 * 8)");
        assert!(hex.starts_with("00000001"), "first word should be 00000001");
    }
}
