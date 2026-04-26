//! redb-backed authoritative store for the DedupIndex.
//!
//! See ARCHITECTURE §7.2: single table `dedup_index_v1` with key
//! `&[u8; 28]` (the 28-byte content address) and unit value.
//!
//! G2 wires the [`BatchWriter`] into the struct and ships
//! [`DedupIndex::insert`] (commit-then-bloom). [`DedupIndex::lookup`]
//! and [`DedupIndex::remove`] are stubbed for tasks H1 and H2.

use crate::atomic_bloom::AtomicBloomFilter;
use crate::batch_writer::BatchWriter;
use crate::config::DedupIndexConfig;
use crate::error::DedupIndexError;
use crate::paths::DedupRoot;
use crate::stats::StatsCounters;
use redb::{Database, ReadableDatabase, TableDefinition};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

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
/// [`RedbDedupIndex::open`] (subsequent mounts). Both paths spawn a
/// [`BatchWriter`] that owns the single redb writer thread (G1/G2).
///
/// The manifest, bloom snapshot persistence, and Drop-time clean-shutdown
/// hooks are bolted on by later tasks (I2, J1).
//
// Some fields (`config`, `root`, `high_water`, `stats`) are populated
// here but not yet *read* until later tasks (I1 flush, I2 Drop, J1
// snapshot). Keep `#[allow(dead_code)]` until those tasks land — the
// alternative is per-field allows that we'd just have to remove anyway.
#[allow(dead_code)]
pub struct RedbDedupIndex {
    pub(crate) config: DedupIndexConfig,
    pub(crate) root: DedupRoot,
    pub(crate) db: Arc<Database>,
    pub(crate) bloom: AtomicBloomFilter,
    pub(crate) high_water: Arc<AtomicU64>,
    pub(crate) stats: Arc<StatsCounters>,
    pub(crate) batch_writer: Option<BatchWriter>,
}

impl RedbDedupIndex {
    /// Create a fresh index at `config.dedup_root`.
    ///
    /// Creates the directory if missing, opens the redb database with the
    /// configured cache, and touches [`DEDUP_TABLE`] inside a write
    /// transaction so the table's metadata exists on disk before the
    /// first real insert. Then spawns the [`BatchWriter`] thread.
    ///
    /// Note: `config.page_size` is recorded in [`DedupIndexConfig`] for
    /// the manifest, but redb 4.1 only exposes `set_page_size` under
    /// `cfg(test)` / `cfg(fuzzing)`. We rely on redb's 4 KiB default,
    /// which matches our configured value.
    pub fn create(config: DedupIndexConfig) -> Result<Self, DedupIndexError> {
        let root = DedupRoot::new(&config.dedup_root);
        std::fs::create_dir_all(root.base())?;
        let db = Arc::new(
            Database::builder()
                .set_cache_size(config.redb_cache_bytes)
                .create(root.redb())?,
        );
        // Touch the table so its metadata exists.
        let txn = db.begin_write()?;
        {
            let _t = txn.open_table(DEDUP_TABLE)?;
        }
        txn.commit()?;

        let bloom = AtomicBloomFilter::new(&config.bloom);
        let stats = Arc::new(StatsCounters::default());
        let high_water = Arc::new(AtomicU64::new(0));
        let bw = BatchWriter::spawn(
            config.clone(),
            Arc::clone(&db),
            bloom.clone_handle(),
            Arc::clone(&stats),
            Arc::clone(&high_water),
        );

        Ok(Self {
            config,
            root,
            db,
            bloom,
            high_water,
            stats,
            batch_writer: Some(bw),
        })
    }

    /// Open an existing index at `config.dedup_root`.
    ///
    /// Does not touch the table — readers should open their own
    /// read transactions. Spawns the [`BatchWriter`] thread.
    pub fn open(config: DedupIndexConfig) -> Result<Self, DedupIndexError> {
        let root = DedupRoot::new(&config.dedup_root);
        let db = Arc::new(
            Database::builder()
                .set_cache_size(config.redb_cache_bytes)
                .open(root.redb())?,
        );

        let bloom = AtomicBloomFilter::new(&config.bloom);
        let stats = Arc::new(StatsCounters::default());
        let high_water = Arc::new(AtomicU64::new(0));
        let bw = BatchWriter::spawn(
            config.clone(),
            Arc::clone(&db),
            bloom.clone_handle(),
            Arc::clone(&stats),
            Arc::clone(&high_water),
        );

        Ok(Self {
            config,
            root,
            db,
            bloom,
            high_water,
            stats,
            batch_writer: Some(bw),
        })
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

use slicefs_traits::{CasError, ChunkHash, DedupIndex, DedupResult};

impl RedbDedupIndex {
    /// Convert a [`ChunkHash`] into the fixed 28-byte redb key. Returns
    /// [`CasError::Index`] if the hash is not exactly 28 bytes wide.
    fn hash28(h: &ChunkHash) -> Result<[u8; 28], CasError> {
        let bytes = h.as_bytes();
        if bytes.len() != 28 {
            return Err(CasError::Index(format!(
                "ChunkHash must be 28 bytes, got {}",
                bytes.len()
            )));
        }
        let mut out = [0u8; 28];
        out.copy_from_slice(bytes);
        Ok(out)
    }

    /// Convert a hash to its CAS path using 2-char shard / rest layout.
    ///
    /// Returns `None` if the hex string is shorter than 4 characters
    /// (which should never happen for a 28-byte hash, but defensive).
    fn cas_path(&self, hash: &ChunkHash) -> Option<std::path::PathBuf> {
        let hex: String = hash.as_bytes().iter().map(|b| format!("{:02x}", b)).collect();
        if hex.len() < 4 {
            return None;
        }
        Some(self.config.cas_root.join(&hex[..2]).join(&hex[2..]))
    }
}

impl DedupIndex for RedbDedupIndex {
    fn bloom_check(&self, hash: &ChunkHash) -> bool {
        let bytes = hash.as_bytes();
        self.bloom.contains(bytes)
    }

    fn insert(&self, hash: &ChunkHash) -> Result<(), CasError> {
        let h = Self::hash28(hash)?;
        let bw = self.batch_writer.as_ref().ok_or_else(|| {
            CasError::Index("RedbDedupIndex was opened without a batch writer".into())
        })?;
        bw.submit(h).map_err(CasError::from)
    }

    fn lookup(&self, hash: &ChunkHash) -> Result<DedupResult, CasError> {
        self.stats.lookups_total.fetch_add(1, Ordering::Relaxed);

        let bytes = hash.as_bytes();
        if !self.bloom.contains(bytes) {
            return Ok(DedupResult::DefinitelyAbsent);
        }
        self.stats.bloom_hits_total.fetch_add(1, Ordering::Relaxed);

        let h = Self::hash28(hash)?;
        let txn = self.db.begin_read().map_err(|e| {
            CasError::Index(format!("redb begin_read: {e}"))
        })?;
        let t = txn.open_table(DEDUP_TABLE).map_err(|e| {
            CasError::Index(format!("redb open_table: {e}"))
        })?;
        let hit = t.get(&h).map_err(|e| {
            CasError::Index(format!("redb get: {e}"))
        })?.is_some();

        if !hit {
            self.stats.bloom_false_positives_total.fetch_add(1, Ordering::Relaxed);
            return Ok(DedupResult::Absent);
        }

        if self.config.verify_on_present && let Some(p) = self.cas_path(hash) {
            self.stats.verify_on_present_hits_total.fetch_add(1, Ordering::Relaxed);
            if !p.exists() {
                return Ok(DedupResult::Absent);
            }
        }
        Ok(DedupResult::Present)
    }

    fn remove(&self, hash: &ChunkHash) -> Result<(), CasError> {
        let h = Self::hash28(hash)?;
        let mut txn = self.db.begin_write().map_err(|e| {
            CasError::Index(format!("redb begin_write: {e}"))
        })?;
        let _ = txn.set_durability(match self.config.durability {
            crate::config::DurabilityMode::Seed     => redb::Durability::None,
            crate::config::DurabilityMode::Default  => redb::Durability::Immediate,
            crate::config::DurabilityMode::Paranoid => redb::Durability::Immediate,
        });
        {
            let mut t = txn.open_table(DEDUP_TABLE).map_err(|e| CasError::Index(format!("open: {e}")))?;
            t.remove(&h).map_err(|e| CasError::Index(format!("remove: {e}")))?;
        }
        txn.commit().map_err(|e| CasError::Index(format!("commit: {e}")))?;
        self.stats.removes_total.fetch_add(1, Ordering::Relaxed);
        Ok(())
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
        let mut idx = RedbDedupIndex::create(cfg.clone()).unwrap();
        // Until Task I2 lands a Drop impl, the batcher thread holds an
        // Arc<Database> clone that keeps redb's in-process registry busy
        // until the thread exits. Explicitly shut it down here so the
        // subsequent open() does not race against thread teardown.
        if let Some(bw) = idx.batch_writer.take() {
            bw.shutdown(std::time::Duration::from_secs(2));
        }
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

#[cfg(test)]
mod insert_tests {
    use super::*;
    use slicefs_traits::{ChunkHash, DedupIndex, DedupResult};

    fn make_hash(seed: u8) -> ChunkHash {
        let mut v = vec![0u8; 28];
        v[0] = seed;
        ChunkHash::from_bytes(v)
    }

    fn shutdown_then_drop(mut idx: RedbDedupIndex) {
        if let Some(bw) = idx.batch_writer.take() {
            bw.shutdown(std::time::Duration::from_secs(2));
        }
        drop(idx);
    }

    #[test]
    fn insert_then_bloom_check_true() {
        let td = tempfile::tempdir().unwrap();
        let cas = td.path().join("cas");
        std::fs::create_dir_all(&cas).unwrap();
        let cfg = DedupIndexConfig::builder(&cas).build();
        let idx = RedbDedupIndex::create(cfg).unwrap();

        let h = make_hash(1);
        idx.insert(&h).unwrap();
        assert!(idx.bloom_check(&h));

        shutdown_then_drop(idx);
    }

    #[test]
    fn definitely_absent_bypasses_redb() {
        let td = tempfile::tempdir().unwrap();
        let cas = td.path().join("cas");
        std::fs::create_dir_all(&cas).unwrap();
        let cfg = DedupIndexConfig::builder(&cas).build();
        let idx = RedbDedupIndex::create(cfg).unwrap();
        let h = make_hash(0xFE);
        let r = idx.lookup(&h).unwrap();
        assert!(matches!(r, DedupResult::DefinitelyAbsent));

        shutdown_then_drop(idx);
    }

    #[test]
    fn present_after_insert() {
        let td = tempfile::tempdir().unwrap();
        let cas = td.path().join("cas");
        std::fs::create_dir_all(&cas).unwrap();
        let cfg = DedupIndexConfig::builder(&cas).build();
        let idx = RedbDedupIndex::create(cfg).unwrap();
        let h = make_hash(7);
        idx.insert(&h).unwrap();
        let r = idx.lookup(&h).unwrap();
        assert!(matches!(r, DedupResult::Present));

        shutdown_then_drop(idx);
    }

    #[test]
    fn remove_makes_lookup_absent_but_bloom_still_hits() {
        let td = tempfile::tempdir().unwrap();
        let cas = td.path().join("cas");
        std::fs::create_dir_all(&cas).unwrap();
        let cfg = DedupIndexConfig::builder(&cas).build();
        let idx = RedbDedupIndex::create(cfg).unwrap();
        let h = make_hash(9);
        idx.insert(&h).unwrap();
        idx.remove(&h).unwrap();
        assert!(matches!(idx.lookup(&h).unwrap(), DedupResult::Absent),
            "lookup must be Absent after remove (bloom hit + redb miss = false positive)");
        assert!(idx.bloom_check(&h),
            "bloom must NOT be updated on remove (I3 — drift is benign)");
        shutdown_then_drop(idx);
    }
}
