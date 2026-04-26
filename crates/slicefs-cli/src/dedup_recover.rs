//! `dedup-recover` subcommand — non-destructive dedup index recovery.
//!
//! ## Usage
//!
//! ```text
//! slicefs dedup-recover --store <PATH>
//! ```
//!
//! Renames an existing `<store>/cas/.dedup-index/index.redb` to
//! `index.redb.bak.<unix-secs>`, then rebuilds from the CAS shards.
//! On rebuild failure, the backup is restored. If no existing index is
//! present, this is equivalent to `reindex`.
//!
//! See ARCHITECTURE §9.2 / OQ-5.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Run the `dedup-recover` subcommand.
///
/// # Errors
///
/// Returns an error if the CAS root does not exist or if the rebuild fails.
/// On rebuild failure the prior index is restored from its `.bak` copy.
pub fn run_dedup_recover(store: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let cas_root = store.join("cas");
    if !cas_root.exists() {
        return Err(format!("{:?} does not exist", cas_root).into());
    }
    let dedup_root = cas_root.join(".dedup-index");
    let idx = dedup_root.join("index.redb");
    let bak_name = format!(
        "index.redb.bak.{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    );
    let bak = dedup_root.join(&bak_name);

    let had_existing_index = idx.exists();
    if had_existing_index {
        // Ensure dedup_root exists before rename (it does if idx exists, but be safe).
        std::fs::create_dir_all(&dedup_root)?;
        std::fs::rename(&idx, &bak)
            .map_err(|e| format!("backup existing index to {bak:?} failed: {e}"))?;
        println!("backed up old index to {bak:?}");
    }

    let cfg = slicefs_dedup::DedupIndexConfig::builder(&cas_root).build();
    if let Err(e) = slicefs_dedup::RedbDedupIndex::rebuild_from_cas(cfg) {
        // Restore the backup if we made one.
        if had_existing_index && bak.exists() {
            let _ = std::fs::rename(&bak, &idx);
        }
        return Err(format!("recover failed: {e}").into());
    }

    if had_existing_index {
        println!("recover ok; backup retained at {bak:?}");
    } else {
        println!("recover ok: {:?} (no prior index to back up)", dedup_root);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_cas_returns_error() {
        let td = tempfile::tempdir().unwrap();
        let result = run_dedup_recover(td.path());
        assert!(result.is_err(), "missing cas must error");
    }

    #[test]
    fn first_recover_creates_index_no_backup() {
        let td = tempfile::tempdir().unwrap();
        let cas = td.path().join("cas");
        std::fs::create_dir_all(&cas).unwrap();

        run_dedup_recover(td.path()).expect("first recover should succeed");

        let dedup = cas.join(".dedup-index");
        assert!(dedup.join("index.redb").exists());
        let baks: Vec<_> = std::fs::read_dir(&dedup)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("index.redb.bak.")
            })
            .collect();
        assert!(baks.is_empty(), "no backup expected on first recover");
    }
}
