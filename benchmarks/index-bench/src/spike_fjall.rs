//! Spike backend `spike-fjall` (Phase 17 plan 01): the same index-only Store
//! semantics as `spike-runs`, on fjall. Throwaway code on the spike branch.
//!
//! Settings, chosen to be as near the locked on-disk rules as fjall allows:
//! Bloom 10 bits per key on every level, 4 KiB data blocks, no compression
//! (data, index and journal), filter and index blocks pinned in RAM on every
//! level and not partitioned (so open loads them, like the run filters and
//! fences), point-read misses expected (filters on the last level too), a
//! 16 MiB block cache, and a journal that is only written to the OS on
//! persist (the packs are the log of the real store).
//!
//! close: flush the memtable to a table, major compaction (blocking), wait
//! until no flush or compaction runs, persist with SyncAll, then write the
//! exact object count.

use std::cell::Cell;
use std::path::Path;

use chunk_store::{ObjectId, DEFAULT_HASHER};
use fjall::config::{
    BlockSizePolicy, BloomConstructionPolicy, CompressionPolicy, FilterPolicy, FilterPolicyEntry,
    PartitioningPolicy, PinningPolicy,
};
use fjall::{CompressionType, Database, Keyspace, KeyspaceCreateOptions, PersistMode};

use crate::backend::{Backend, Store};

const COUNT_FILE: &str = "COUNT";
pub const CACHE_BYTES: u64 = 16 << 20;

fn e(e: fjall::Error) -> String {
    e.to_string()
}

pub fn keyspace_options() -> KeyspaceCreateOptions {
    KeyspaceCreateOptions::default()
        .filter_policy(FilterPolicy::all(FilterPolicyEntry::Bloom(BloomConstructionPolicy::BitsPerKey(10.0))))
        .data_block_size_policy(BlockSizePolicy::all(4096))
        .data_block_compression_policy(CompressionPolicy::disabled())
        .index_block_compression_policy(CompressionPolicy::disabled())
        .filter_block_pinning_policy(PinningPolicy::all(true))
        .index_block_pinning_policy(PinningPolicy::all(true))
        .filter_block_partitioning_policy(PartitioningPolicy::all(false))
        .index_block_partitioning_policy(PartitioningPolicy::all(false))
        .expect_point_read_hits(false)
}

pub fn open_db(dir: &Path) -> Result<(Database, Keyspace), String> {
    let db = Database::builder(dir.join("fjall"))
        .cache_size(CACHE_BYTES)
        .journal_compression(CompressionType::None)
        .manual_journal_persist(true)
        .open()
        .map_err(e)?;
    let ks = db.keyspace("idx", keyspace_options).map_err(e)?;
    Ok((db, ks))
}

pub struct SpikeFjallBackend;

struct Handle {
    dir: std::path::PathBuf,
    db: Database,
    ks: Keyspace,
    count: Cell<u64>,
}

impl Store for Handle {
    fn put(&self, data: &[u8]) -> Result<(), String> {
        let id: ObjectId = DEFAULT_HASHER.id(data);
        if self.ks.get(id.0).map_err(e)?.is_some() {
            return Ok(());
        }
        let i = self.count.get();
        let mut l = [0u8; 25];
        l[0..8].copy_from_slice(&(i / 1_240_000 + 1).to_le_bytes());
        l[8..16].copy_from_slice(&(14 + (i % 1_240_000) * 54).to_le_bytes());
        l[16..20].copy_from_slice(&8u32.to_le_bytes());
        l[20..24].copy_from_slice(&8u32.to_le_bytes());
        self.ks.insert(id.0, l).map_err(e)?;
        self.count.set(i + 1);
        Ok(())
    }
    fn contains(&self, id: &ObjectId) -> bool {
        self.ks.contains_key(id.0).unwrap_or(false)
    }
    fn objects(&self) -> u64 {
        self.count.get()
    }
    /// The number of tables (the "packs" output key of the bench).
    fn packs(&self) -> u64 {
        self.ks.table_count() as u64
    }
    fn close(&self) -> Result<(), String> {
        self.ks.rotate_memtable_and_wait().map_err(e)?;
        self.ks.major_compact().map_err(e)?;
        while self.db.outstanding_flushes() > 0 || self.db.active_compactions() > 0 {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        self.db.persist(PersistMode::SyncAll).map_err(e)?;
        std::fs::write(self.dir.join(COUNT_FILE), self.count.get().to_string()).map_err(|x| x.to_string())?;
        Ok(())
    }
}

impl Backend for SpikeFjallBackend {
    fn name(&self) -> &'static str {
        "spike-fjall"
    }
    fn create(&self, dir: &Path, _pack_limit: Option<u64>) -> Result<Box<dyn Store>, String> {
        // The create step counts the bytes in packs/; there are none.
        std::fs::create_dir_all(dir.join("packs")).map_err(|x| x.to_string())?;
        let (db, ks) = open_db(dir)?;
        Ok(Box::new(Handle { dir: dir.to_path_buf(), db, ks, count: Cell::new(0) }))
    }
    fn open(&self, dir: &Path, _read_only: bool) -> Result<Box<dyn Store>, String> {
        let count: u64 = std::fs::read_to_string(dir.join(COUNT_FILE))
            .map_err(|x| x.to_string())?
            .trim()
            .parse()
            .map_err(|_| "bad COUNT file".to_string())?;
        let (db, ks) = open_db(dir)?;
        Ok(Box::new(Handle { dir: dir.to_path_buf(), db, ks, count: Cell::new(count) }))
    }
}
