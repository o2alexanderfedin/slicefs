//! redb-backed authoritative store for the DedupIndex.
//!
//! See ARCHITECTURE §7.2: single table `dedup_index_v1` with key
//! `&[u8; 28]` (the 28-byte content address) and unit value.
//!
//! This module provides the skeleton — `create()` / `open()` and the
//! [`DEDUP_TABLE`] definition. Insert / lookup / remove / flush land in
//! later tasks (G1, G2, H1, H2, I1).

use crate::config::DedupIndexConfig;
use crate::error::DedupIndexError;
use crate::paths::DedupRoot;
use redb::{Database, TableDefinition};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountState {
    /// Manifest present and clean. Bloom-xxh3 cross-check is added by Task J2.
    Healthy,
    /// Manifest absent / unclean / bloom xxh3 fail.
    Suspect,
    /// Redb file truncated, schema mismatch, or operator --force-rebuild.
    Rebuilding,
}

/// The single authoritative table.
///
/// - Name: `dedup_index_v1` (the `_v1` suffix gives us a forward path
///   for incompatible schema changes; see ARCHITECTURE §7.2).
/// - Key:  `&[u8; 28]` — the full content address (no truncation).
/// - Val:  `()` — presence-only; the CAS is the source of truth for
///   payload bytes (S1).
pub const DEDUP_TABLE: TableDefinition<&[u8; 28], ()> =
    TableDefinition::new("dedup_index_v1");

/// Authoritative on-disk index, redb 4.1.
///
/// Constructed via [`RedbDedupIndex::create`] (first-time bootstrap) or
/// [`RedbDedupIndex::open`] (subsequent mounts). The bloom filter,
/// batcher, manifest, and stats counters are bolted on by later tasks.
// Fields are wired up by later tasks: BatchWriter (G1), lookup (H1),
// flush() (I1), Drop (I2), bloom snapshot (J1), recovery (K1).
#[allow(dead_code)]
pub struct RedbDedupIndex {
    pub(crate) config: DedupIndexConfig,
    pub(crate) root: DedupRoot,
    pub(crate) db: Arc<Database>,
}

impl RedbDedupIndex {
    /// Create a fresh index at `config.dedup_root`.
    ///
    /// Creates the directory if missing, opens the redb database with the
    /// configured cache, and touches [`DEDUP_TABLE`] inside a write
    /// transaction so the table's metadata exists on disk before the
    /// first real insert.
    ///
    /// Note: `config.page_size` is recorded in [`DedupIndexConfig`] for
    /// the manifest, but redb 4.1 only exposes `set_page_size` under
    /// `cfg(test)` / `cfg(fuzzing)`. We rely on redb's 4 KiB default,
    /// which matches our configured value.
    pub fn create(config: DedupIndexConfig) -> Result<Self, DedupIndexError> {
        let root = DedupRoot::new(&config.dedup_root);
        std::fs::create_dir_all(root.base())?;
        let db = Database::builder()
            .set_cache_size(config.redb_cache_bytes)
            .create(root.redb())?;
        // Touch the table so its metadata exists.
        let txn = db.begin_write()?;
        {
            let _t = txn.open_table(DEDUP_TABLE)?;
        }
        txn.commit()?;
        Ok(Self { config, root, db: Arc::new(db) })
    }

    /// Open an existing index at `config.dedup_root`.
    ///
    /// Does not touch the table — readers should open their own
    /// read transactions.
    pub fn open(config: DedupIndexConfig) -> Result<Self, DedupIndexError> {
        let root = DedupRoot::new(&config.dedup_root);
        let db = Database::builder()
            .set_cache_size(config.redb_cache_bytes)
            .open(root.redb())?;
        Ok(Self { config, root, db: Arc::new(db) })
    }

    /// Probe the manifest on open to determine mount state.
    ///
    /// Returns the state of the index before recovery/rebuild decisions:
    /// - `Healthy`: manifest present and clean shutdown recorded
    /// - `Suspect`: manifest absent, unclean shutdown, or corrupt
    /// - `Rebuilding`: reachable from operator-driven recovery (K1) and
    ///   corruption detection (later tasks)
    pub fn probe(config: &DedupIndexConfig) -> Result<MountState, DedupIndexError> {
        let root = DedupRoot::new(&config.dedup_root);
        if !root.manifest().exists() {
            return Ok(MountState::Suspect);
        }
        match crate::manifest::Manifest::read(&root) {
            Ok(m) if m.last_shutdown_was_clean => Ok(MountState::Healthy),
            Ok(_) => Ok(MountState::Suspect),
            Err(DedupIndexError::ManifestCorrupt(_)) => Ok(MountState::Suspect),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DedupIndexConfig;

    #[test]
    fn create_then_open_roundtrip() {
        let td = tempfile::tempdir().unwrap();
        let cas = td.path().join("cas");
        std::fs::create_dir_all(&cas).unwrap();
        let cfg = DedupIndexConfig::builder(&cas).build();
        let idx = RedbDedupIndex::create(cfg.clone()).unwrap();
        drop(idx);
        let _idx2 = RedbDedupIndex::open(cfg).unwrap();
    }

    #[test]
    fn empty_table_lookup_returns_none() {
        use redb::ReadableDatabase;
        let td = tempfile::tempdir().unwrap();
        let cas = td.path().join("cas");
        std::fs::create_dir_all(&cas).unwrap();
        let cfg = DedupIndexConfig::builder(&cas).build();
        let idx = RedbDedupIndex::create(cfg).unwrap();
        let txn = idx.db.begin_read().unwrap();
        let t = txn.open_table(DEDUP_TABLE).unwrap();
        assert!(t.get(&[0u8; 28]).unwrap().is_none());
    }
}

#[cfg(test)]
mod probe_tests {
    use super::*;
    use crate::manifest::Manifest;

    fn cfg() -> (tempfile::TempDir, DedupIndexConfig) {
        let td = tempfile::tempdir().unwrap();
        let cas = td.path().join("cas");
        std::fs::create_dir_all(&cas).unwrap();
        let cfg = DedupIndexConfig::builder(&cas).build();
        std::fs::create_dir_all(&cfg.dedup_root).unwrap();
        (td, cfg)
    }

    #[test]
    fn missing_manifest_is_suspect() {
        let (_g, c) = cfg();
        assert_eq!(RedbDedupIndex::probe(&c).unwrap(), MountState::Suspect);
    }

    #[test]
    fn unclean_shutdown_is_suspect() {
        let (_g, c) = cfg();
        let mut m = Manifest::new(1, 0.01, 4096);
        m.last_shutdown_was_clean = false;
        m.write_atomic(&DedupRoot::new(&c.dedup_root)).unwrap();
        assert_eq!(RedbDedupIndex::probe(&c).unwrap(), MountState::Suspect);
    }

    #[test]
    fn clean_shutdown_is_healthy() {
        let (_g, c) = cfg();
        let mut m = Manifest::new(1, 0.01, 4096);
        m.last_shutdown_was_clean = true;
        m.write_atomic(&DedupRoot::new(&c.dedup_root)).unwrap();
        assert_eq!(RedbDedupIndex::probe(&c).unwrap(), MountState::Healthy);
    }
}
