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
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
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
    /// Attempts to load the bloom filter from a snapshot on disk. If the
    /// snapshot is missing or corrupt, rebuilds the bloom by iterating
    /// through the redb table. Spawns the [`BatchWriter`] thread.
    pub fn open(config: DedupIndexConfig) -> Result<Self, DedupIndexError> {
        let root = DedupRoot::new(&config.dedup_root);
        let db = Arc::new(
            Database::builder()
                .set_cache_size(config.redb_cache_bytes)
                .open(root.redb())?,
        );

        let bloom = match crate::bloom_snapshot::load(&root) {
            Ok((_meta, payload)) => {
                // Cross-checking meta.redb_hwm_at_snapshot vs current redb HWM is a v2 lever.
                // For MVP we trust the snapshot if it loads.
                AtomicBloomFilter::from_serialized(&payload, &config.bloom)
            }
            Err(_) => {
                // No snapshot or corrupt; rebuild from redb.
                let bf = AtomicBloomFilter::new(&config.bloom);
                let txn = db.begin_read().map_err(DedupIndexError::from)?;
                let t = txn.open_table(DEDUP_TABLE).map_err(DedupIndexError::from)?;
                let iter = t.iter().map_err(DedupIndexError::from)?;
                for entry_res in iter {
                    let (k, _) = entry_res.map_err(DedupIndexError::from)?;
                    bf.set(&k.value()[..]);
                }
                bf
            }
        };

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

    /// Rebuild the redb file by walking the CAS shard tree.
    ///
    /// Operator-facing recovery (ARCHITECTURE §9.2; I7 idempotent).
    /// Bulk-loads every 28-byte content address discovered under
    /// `cas_root/{00..ff}/` into a fresh `index.redb.tmp`, then
    /// atomically renames over `index.redb` and fsyncs the parent
    /// directory. Safe to re-run: any leftover `.tmp` from a crashed
    /// previous attempt is removed first.
    ///
    /// Delegates to [`crate::recovery::rebuild_from_cas`] so the
    /// procedure can be invoked without first constructing a
    /// `RedbDedupIndex` (which would itself open the redb file we
    /// are about to replace).
    pub fn rebuild_from_cas(config: DedupIndexConfig) -> Result<(), DedupIndexError> {
        crate::recovery::rebuild_from_cas(config)
    }

    /// Snapshot the live counters for operator surfaces (CLI `stats` block).
    ///
    /// Reads each [`StatsCounters`] atomic with `Relaxed` ordering — these
    /// counters are advisory metrics, not synchronization. `redb_free_bytes`
    /// is not exposed in MVP because redb 4.1's `DatabaseStats` lives on
    /// `WriteTransaction` and we do not want a stats read to contend with
    /// the [`BatchWriter`]; the field is reserved for v2 when redb gains a
    /// read-side stats API. `bloom_load_factor` is similarly reserved —
    /// fastbloom does not expose load-factor introspection.
    pub fn stats_snapshot(&self) -> crate::stats::StatsSnapshot {
        let s = &self.stats;
        crate::stats::StatsSnapshot {
            inserts_total: s.inserts_total.load(Ordering::Relaxed),
            lookups_total: s.lookups_total.load(Ordering::Relaxed),
            bloom_hits_total: s.bloom_hits_total.load(Ordering::Relaxed),
            bloom_false_positives_total: s.bloom_false_positives_total.load(Ordering::Relaxed),
            commits_total: s.commits_total.load(Ordering::Relaxed),
            commit_failures_total: s.commit_failures_total.load(Ordering::Relaxed),
            removes_total: s.removes_total.load(Ordering::Relaxed),
            backpressure_rejects_total: s.backpressure_rejects_total.load(Ordering::Relaxed),
            verify_on_present_hits_total: s.verify_on_present_hits_total.load(Ordering::Relaxed),
            bloom_snapshot_failures_total: s.bloom_snapshot_failures_total.load(Ordering::Relaxed),
            bloom_load_factor: 0.0,
            redb_free_bytes: 0,
            queue_depth: 0,
            hwm: self.high_water.load(Ordering::Acquire),
        }
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

    fn flush(&self) -> Result<(), CasError> {
        // BatchWriter has no explicit drain primitive; submit() blocks
        // for reply already, so by the time the most-recent insert
        // returned, all earlier inserts are committed. To force device
        // durability, open the redb file and F_FULLFSYNC it.
        let f = std::fs::File::open(self.root.redb()).map_err(CasError::Io)?;
        crate::platform::durable_sync(&f).map_err(CasError::Io)?;
        Ok(())
    }
}

impl Drop for RedbDedupIndex {
    fn drop(&mut self) {
        // Best-effort: shut batcher with timeout, then flush, then update manifest.
        // See ARCHITECTURE §5.2 (Drop contract) and §9.1 (Healthy transition).
        if let Some(bw) = self.batch_writer.take() {
            bw.shutdown(std::time::Duration::from_secs(5));
        }

        // J1: final snapshot, regardless of N. Best-effort. Done BEFORE
        // the redb durable_sync so a snapshot failure does not block
        // device-level durability of the index file.
        let payload = self.bloom.to_bytes();
        let meta = crate::bloom_snapshot::BloomSnapshotMeta {
            bloom_capacity: self.config.bloom.capacity as u64,
            bloom_fpr_bits: self.config.bloom.fpr,
            entries_at_snapshot: self
                .stats
                .inserts_total
                .load(std::sync::atomic::Ordering::Relaxed),
            redb_hwm_at_snapshot: self
                .high_water
                .load(std::sync::atomic::Ordering::Acquire),
        };
        let _ = crate::bloom_snapshot::write_atomic(&self.root, &meta, &payload);

        let _ = std::fs::File::open(self.root.redb())
            .and_then(|f| crate::platform::durable_sync(&f));

        let mut m = match crate::manifest::Manifest::read(&self.root) {
            Ok(m) => m,
            Err(_) => crate::manifest::Manifest::new(
                self.config.bloom.capacity as u64,
                self.config.bloom.fpr,
                self.config.page_size as u32,
            ),
        };
        m.last_shutdown_was_clean = true;
        m.last_clean_shutdown_unix_micros = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros() as u64;
        m.entries_high_water_mark = self.high_water.load(std::sync::atomic::Ordering::Acquire);
        let _ = m.write_atomic(&self.root);
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

#[cfg(test)]
mod insert_tests {
    use super::*;
    use slicefs_traits::{ChunkHash, DedupIndex, DedupResult};

    fn make_hash(seed: u8) -> ChunkHash {
        let mut v = vec![0u8; 28];
        v[0] = seed;
        ChunkHash::from_bytes(v)
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
    }

    #[test]
    fn flush_drains_pending_inserts() {
        let td = tempfile::tempdir().unwrap();
        let cas = td.path().join("cas");
        std::fs::create_dir_all(&cas).unwrap();
        let cfg = DedupIndexConfig::builder(&cas).mode(crate::config::DurabilityMode::Default).build();
        let idx = RedbDedupIndex::create(cfg).unwrap();
        for i in 0..50u8 { idx.insert(&make_hash(i)).unwrap(); }
        idx.flush().unwrap();

        // After flush, every insert must be Present even on a fresh read txn.
        for i in 0..50u8 {
            assert!(matches!(idx.lookup(&make_hash(i)).unwrap(), DedupResult::Present));
        }
    }

    #[test]
    fn drop_writes_clean_shutdown_manifest() {
        let td = tempfile::tempdir().unwrap();
        let cas = td.path().join("cas");
        std::fs::create_dir_all(&cas).unwrap();
        let cfg = DedupIndexConfig::builder(&cas).build();
        let dedup_root = cfg.dedup_root.clone();
        {
            let idx = RedbDedupIndex::create(cfg.clone()).unwrap();
            idx.insert(&make_hash(1)).unwrap();
            // Drop fires here when scope exits.
        }
        let m = crate::manifest::Manifest::read(&DedupRoot::new(&dedup_root)).unwrap();
        assert!(m.last_shutdown_was_clean);
        assert_eq!(m.entries_high_water_mark, 1);
    }

    #[test]
    fn snapshot_after_threshold_inserts() {
        let td = tempfile::tempdir().unwrap();
        let cas = td.path().join("cas");
        std::fs::create_dir_all(&cas).unwrap();
        let mut cfg = DedupIndexConfig::builder(&cas).build();
        cfg.bloom.snapshot_every = 10;
        let dedup_root = cfg.dedup_root.clone();
        {
            let idx = RedbDedupIndex::create(cfg).unwrap();
            for i in 0..15u8 {
                idx.insert(&make_hash(i)).unwrap();
            }
            idx.flush().unwrap();
        } // Drop triggers final snapshot.
        assert!(
            DedupRoot::new(&dedup_root).bloom().exists(),
            "bloom.snap must exist after threshold inserts + drop"
        );
    }

    #[test]
    fn open_loads_bloom_from_snapshot() {
        let td = tempfile::tempdir().unwrap();
        let cas = td.path().join("cas");
        std::fs::create_dir_all(&cas).unwrap();
        let cfg = DedupIndexConfig::builder(&cas).build();
        {
            let idx = RedbDedupIndex::create(cfg.clone()).unwrap();
            idx.insert(&make_hash(33)).unwrap();
            idx.flush().unwrap();
        } // Drop writes snapshot.
        let idx2 = RedbDedupIndex::open(cfg).unwrap();
        assert!(idx2.bloom_check(&make_hash(33)),
            "bloom must be loaded from snapshot or rebuilt from redb");
    }

    #[test]
    fn open_rebuilds_bloom_from_redb_when_snapshot_corrupt() {
        let td = tempfile::tempdir().unwrap();
        let cas = td.path().join("cas");
        std::fs::create_dir_all(&cas).unwrap();
        let cfg = DedupIndexConfig::builder(&cas).build();
        {
            let idx = RedbDedupIndex::create(cfg.clone()).unwrap();
            idx.insert(&make_hash(44)).unwrap();
            idx.flush().unwrap();
        }
        // Corrupt the snapshot.
        let bloom_path = DedupRoot::new(&cfg.dedup_root).bloom();
        let mut bytes = std::fs::read(&bloom_path).unwrap();
        bytes[0] ^= 0xFF; // breaks magic
        std::fs::write(&bloom_path, &bytes).unwrap();

        let idx2 = RedbDedupIndex::open(cfg).unwrap();
        // Bloom rebuild from redb should set this hash.
        assert!(idx2.bloom_check(&make_hash(44)),
            "bloom rebuild from redb must include the inserted hash");
    }
}
