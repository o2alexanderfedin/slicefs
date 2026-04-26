//! `reindex` subcommand — offline rebuild of the dedup index from CAS contents.
//!
//! ## Usage
//!
//! ```text
//! slicefs reindex --store <PATH> [--offline]
//! ```
//!
//! Walks `<store>/cas/<shard>/` and bulk-loads every 28-byte content address
//! into a fresh `<store>/cas/.dedup-index/index.redb`, atomically replacing
//! any existing index. The `--offline` flag is required in MVP (default true);
//! online reindex is a v2 feature.
//!
//! See ARCHITECTURE §9.2 (operator-driven recovery, K1).

use std::path::Path;

/// Run the `reindex` subcommand.
///
/// # Errors
///
/// Returns an error if `--offline` is false (online reindex is unsupported in
/// MVP), if the CAS root does not exist, or if `RedbDedupIndex::rebuild_from_cas`
/// fails.
pub fn run_reindex(store: &Path, offline: bool) -> Result<(), Box<dyn std::error::Error>> {
    if !offline {
        return Err("online reindex is a v2 feature; pass --offline (default)".into());
    }
    let cas_root = store.join("cas");
    if !cas_root.exists() {
        return Err(format!("{:?} does not exist", cas_root).into());
    }
    let cfg = slicefs_dedup::DedupIndexConfig::builder(&cas_root).build();
    slicefs_dedup::RedbDedupIndex::rebuild_from_cas(cfg)
        .map_err(|e| format!("reindex failed: {e}"))?;
    println!("reindex ok: {:?}", cas_root.join(".dedup-index"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn online_mode_returns_error() {
        let td = tempfile::tempdir().unwrap();
        let result = run_reindex(td.path(), false);
        assert!(result.is_err(), "online reindex must error");
        let msg = result.err().unwrap().to_string();
        assert!(msg.contains("v2 feature"), "got: {msg}");
    }

    #[test]
    fn missing_cas_returns_error() {
        let td = tempfile::tempdir().unwrap();
        // No `cas/` subdir.
        let result = run_reindex(td.path(), true);
        assert!(result.is_err(), "missing cas must error");
        let msg = result.err().unwrap().to_string();
        assert!(msg.contains("does not exist"), "got: {msg}");
    }

    #[test]
    fn empty_cas_succeeds_and_creates_index() {
        let td = tempfile::tempdir().unwrap();
        let cas = td.path().join("cas");
        std::fs::create_dir_all(&cas).unwrap();

        run_reindex(td.path(), true).expect("reindex on empty cas should succeed");

        let idx = cas.join(".dedup-index").join("index.redb");
        assert!(idx.exists(), "index.redb must be created at {idx:?}");
    }
}
