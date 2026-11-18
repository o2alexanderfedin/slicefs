//! The store under test.
//!
//! Only the chunk store exists now. Phase 17 (#85) adds other index backends
//! here (for example a spike on fjall). The steps in `main.rs` call only the
//! traits in this file, so a new backend needs no change to the steps.

use std::path::Path;

use chunk_store::{ChunkStore, ObjectId, Options};

/// An open store.
pub trait Store {
    /// Store `data` (a new object, or a dedup hit).
    fn put(&self, data: &[u8]) -> Result<(), String>;
    /// True if the store has `id`.
    #[allow(dead_code)] // TEMP: the lookup step uses it
    fn contains(&self, id: &ObjectId) -> bool;
    /// Distinct objects in the store.
    fn objects(&self) -> u64;
    /// Pack files of the store.
    fn packs(&self) -> u64;
    /// Seal the store: every pack gets its index.
    fn close(&self) -> Result<(), String>;
}

/// A kind of store: how to make one and how to open one.
pub trait Backend {
    /// The name given with `--backend`.
    fn name(&self) -> &'static str;
    /// Make a new store in `dir`. `pack_limit` replaces the default pack size
    /// only when it is given.
    fn create(&self, dir: &Path, pack_limit: Option<u64>) -> Result<Box<dyn Store>, String>;
    /// Open the store in `dir`.
    fn open(&self, dir: &Path, read_only: bool) -> Result<Box<dyn Store>, String>;
}

/// The chunk store of `crates/chunk-store`, opened with `ChunkStore::open`.
pub struct ChunkStoreBackend;

struct ChunkStoreHandle(ChunkStore);

fn err(e: chunk_store::Error) -> String {
    e.to_string()
}

impl Store for ChunkStoreHandle {
    fn put(&self, data: &[u8]) -> Result<(), String> {
        self.0.put(data).map(|_| ()).map_err(err)
    }
    fn contains(&self, id: &ObjectId) -> bool {
        self.0.contains(id)
    }
    fn objects(&self) -> u64 {
        self.0.stats().objects
    }
    fn packs(&self) -> u64 {
        self.0.stats().packs
    }
    fn close(&self) -> Result<(), String> {
        self.0.close().map_err(err)
    }
}

impl Backend for ChunkStoreBackend {
    fn name(&self) -> &'static str {
        "chunk-store"
    }

    fn create(&self, dir: &Path, pack_limit: Option<u64>) -> Result<Box<dyn Store>, String> {
        let mut opts = Options::default();
        if let Some(limit) = pack_limit {
            opts.pack_limit = limit;
        }
        Ok(Box::new(ChunkStoreHandle(ChunkStore::open(dir, opts).map_err(err)?)))
    }

    fn open(&self, dir: &Path, read_only: bool) -> Result<Box<dyn Store>, String> {
        let opts = Options { read_only, ..Options::default() };
        Ok(Box::new(ChunkStoreHandle(ChunkStore::open(dir, opts).map_err(err)?)))
    }
}

/// The backend with this `--backend` name.
pub fn backend_by_name(name: &str) -> Option<Box<dyn Backend>> {
    match name {
        "chunk-store" => Some(Box::new(ChunkStoreBackend)),
        _ => None,
    }
}
