//! `StoreIo`: the metadata nodes (inodes, directory lists, maps, manifests,
//! root records) in the chunk store, next to the file content.
//!
//! A node is stored like a file: FastCDC chunks under a manifest tree
//! (`chunk_store::file`). So a big map that changes in a few places between
//! commits stores only the changed chunks again. The node key is the
//! `ObjectId` of the manifest root, as a `ObjectKey`.
//!
//! The metadata and the file content share one `ChunkStore`: a commit syncs
//! its active pack once, then logs the root.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chunk_store::file::{FileReader, FileWriter};
use chunk_store::{ChunkStore, ObjectId};
use slicefs_traits::key::ObjectKey;
use slicefs_traits::metadata::MetaError;

/// Read nodes by key.
pub trait NodeRead {
    /// The bytes of the node `key`, or `None` if it is missing or damaged.
    fn get_node(&mut self, key: &ObjectKey) -> chunk_store::Result<Vec<u8>>;

    /// The chunk store of the nodes (for reads of a node in windows).
    fn chunk_store(&self) -> &ChunkStore;
}

/// Store nodes.
pub trait NodeWrite: NodeRead {
    /// Store `bytes` as one node and return its key. An error is kept and
    /// returned by the next sync (so a commit never logs a root over it).
    fn put_node(&mut self, bytes: &[u8]) -> ObjectKey;
}

impl<T: NodeRead + ?Sized> NodeRead for &mut T {
    fn get_node(&mut self, key: &ObjectKey) -> chunk_store::Result<Vec<u8>> {
        (**self).get_node(key)
    }

    fn chunk_store(&self) -> &ChunkStore {
        (**self).chunk_store()
    }
}

impl<T: NodeWrite + ?Sized> NodeWrite for &mut T {
    fn put_node(&mut self, bytes: &[u8]) -> ObjectKey {
        (**self).put_node(bytes)
    }
}

// An `ObjectKey` holds the bytes of an `ObjectId` (tag + digest).
const _: () = assert!(chunk_store::ID_LEN == slicefs_traits::key::KEY_BYTES);
const _: () = assert!(chunk_store::DIGEST_LEN == slicefs_traits::key::DIGEST_BYTES);

/// `ObjectId` as the `ObjectKey` that the metadata stores (the same bytes).
pub fn to_key(id: ObjectId) -> ObjectKey {
    ObjectKey(id.0)
}

/// The `ObjectId` of a key from the metadata.
pub fn to_id(k: &ObjectKey) -> ObjectId {
    ObjectId(k.0)
}

/// Store `bytes` as one value (a file or a node) and return its key.
pub fn put_value(cs: &ChunkStore, bytes: &[u8]) -> chunk_store::Result<ObjectKey> {
    let mut w = FileWriter::default();
    w.write(cs, bytes)?;
    w.finish(cs).map(to_key)
}

/// All bytes of the value `key`. The error says why not (#65): `Missing`,
/// `Corrupt`, `UnknownHash` or `Io`.
pub fn get_value(cs: &ChunkStore, key: &ObjectKey) -> chunk_store::Result<Vec<u8>> {
    let mut r = FileReader::open(cs, to_id(key))?;
    let n = r.len() as usize;
    r.read(cs, 0, n)
}

/// The metadata error for a node `what` that could not be read: an I/O error
/// stays an I/O error; a missing, damaged or unknown-hash node is corrupt
/// metadata (every node that a root refers to must be there).
pub fn node_err(what: &str, key: &ObjectKey, e: chunk_store::Error) -> MetaError {
    match e {
        chunk_store::Error::Io(io) => MetaError::Io(io),
        e => MetaError::Corrupted(format!("{what} {key:?}: {e}")),
    }
}

/// Read-only node access on a shared chunk store, without the `StoreIo`
/// lock (for the GC mark and for readers in other threads).
#[derive(Clone)]
pub struct NodeReader(pub Arc<ChunkStore>);

impl NodeRead for NodeReader {
    fn get_node(&mut self, key: &ObjectKey) -> chunk_store::Result<Vec<u8>> {
        get_value(&self.0, key)
    }

    fn chunk_store(&self) -> &ChunkStore {
        &self.0
    }
}

/// Node writes on a shared chunk store, without the `StoreIo` lock (for a
/// commit that runs beside requests, #108). Keeps the first put error.
pub struct NodeWriter {
    cs: Arc<ChunkStore>,
    failed: Option<io::Error>,
    puts: u64,
}

impl NodeWriter {
    pub fn new(cs: Arc<ChunkStore>) -> Self {
        Self { cs, failed: None, puts: 0 }
    }

    /// Number of nodes put so far.
    pub fn puts(&self) -> u64 {
        self.puts
    }

    /// The first put error, if any.
    pub fn result(&mut self) -> io::Result<()> {
        self.failed.take().map_or(Ok(()), Err)
    }
}

impl NodeRead for NodeWriter {
    fn get_node(&mut self, key: &ObjectKey) -> chunk_store::Result<Vec<u8>> {
        get_value(&self.cs, key)
    }

    fn chunk_store(&self) -> &ChunkStore {
        &self.cs
    }
}

impl NodeWrite for NodeWriter {
    fn put_node(&mut self, bytes: &[u8]) -> ObjectKey {
        self.puts += 1;
        match put_value(&self.cs, bytes) {
            Ok(k) => k,
            Err(e) => {
                if self.failed.is_none() {
                    self.failed = Some(io::Error::other(e));
                }
                ObjectKey::ZERO
            }
        }
    }
}

/// The node store of a mounted (or opened) store directory.
pub struct StoreIo {
    root: PathBuf,
    cs: Arc<ChunkStore>,
    /// The first put error since the last sync.
    failed: Option<io::Error>,
    /// Nodes put since the last `sync_written`.
    unsynced: usize,
    /// Number of `sync_written` calls that had something to sync (for tests).
    syncs: u64,
}

impl StoreIo {
    /// Nodes of the store directory `root`, in `cs` (the chunk store of that
    /// directory, shared with the file content).
    pub fn new(root: impl Into<PathBuf>, cs: Arc<ChunkStore>) -> Self {
        Self { root: root.into(), cs, failed: None, unsynced: 0, syncs: 0 }
    }

    /// Open the chunk store of `root` (in `root/packs/`) for a `StoreIo`
    /// that is the only user of that directory (tests, tools).
    pub fn open(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        let cs = ChunkStore::open(&root, chunk_store::Options::default()).map_err(io::Error::other)?;
        Ok(Self::new(root, Arc::new(cs)))
    }

    /// The store directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The chunk store.
    pub fn store(&self) -> &Arc<ChunkStore> {
        &self.cs
    }

    /// Nodes put since the last `sync_written`.
    pub fn unsynced_count(&self) -> usize {
        self.unsynced
    }

    /// Number of syncs that had nodes to sync.
    pub fn sync_count(&self) -> u64 {
        self.syncs
    }

    /// Make every node put since the last call durable (with all other
    /// objects of the chunk store). A commit calls this before it logs the
    /// root. Returns a put error kept since the last call.
    pub fn sync_written(&mut self) -> io::Result<()> {
        if let Some(e) = self.failed.take() {
            return Err(e);
        }
        if self.unsynced == 0 {
            return Ok(());
        }
        self.cs.sync().map_err(io::Error::other)?;
        self.unsynced = 0;
        self.syncs += 1;
        Ok(())
    }
}

impl StoreIo {
    /// For a commit that syncs the chunk store itself, with this lock
    /// released (#108): returns the put error kept since the last sync, else
    /// counts the sync the caller makes next (the unsynced count goes to 0).
    pub fn begin_commit_sync(&mut self) -> io::Result<()> {
        if let Some(e) = self.failed.take() {
            return Err(e);
        }
        self.unsynced = 0;
        self.syncs += 1;
        Ok(())
    }
}

impl NodeRead for StoreIo {
    fn get_node(&mut self, key: &ObjectKey) -> chunk_store::Result<Vec<u8>> {
        get_value(&self.cs, key)
    }

    fn chunk_store(&self) -> &ChunkStore {
        &self.cs
    }
}

impl NodeWrite for StoreIo {
    fn put_node(&mut self, bytes: &[u8]) -> ObjectKey {
        self.unsynced += 1;
        match put_value(&self.cs, bytes) {
            Ok(k) => k,
            Err(e) => {
                if self.failed.is_none() {
                    self.failed = Some(io::Error::other(e));
                }
                ObjectKey::ZERO
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
    fn a_node_reads_back_and_equal_bytes_share_a_key() {
        let dir = TempDir::new().unwrap();
        let mut io = StoreIo::open(dir.path()).unwrap();
        let a = io.put_node(b"hello, SliceFS!");
        assert_eq!(io.get_node(&a).unwrap(), b"hello, SliceFS!");
        assert_eq!(io.put_node(b"hello, SliceFS!"), a);
        let empty = io.put_node(b"");
        assert_eq!(io.get_node(&empty).unwrap(), b"");
        assert_ne!(empty, a);
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
    fn a_missing_node_says_missing() {
        use chunk_store::ObjectHasher;
        let dir = TempDir::new().unwrap();
        let mut io = StoreIo::open(dir.path()).unwrap();
        let absent = to_key(chunk_store::Blake3_256.id(b"never stored"));
        assert!(matches!(io.get_node(&absent), Err(chunk_store::Error::Missing(_))));
        // A key with a tag this build does not know says so (#65).
        assert!(matches!(io.get_node(&ObjectKey::example(1)), Err(chunk_store::Error::UnknownHash(_))));
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support the punch-hole probe of FileIo (fallocate, fcntl F_PUNCHHOLE)")]
    fn sync_written_syncs_only_after_puts() {
        let dir = TempDir::new().unwrap();
        let mut io = StoreIo::open(dir.path()).unwrap();
        io.sync_written().unwrap();
        assert_eq!(io.sync_count(), 0);
        io.put_node(b"x");
        assert_eq!(io.unsynced_count(), 1);
        io.sync_written().unwrap();
        assert_eq!((io.unsynced_count(), io.sync_count()), (0, 1));
        io.sync_written().unwrap();
        assert_eq!(io.sync_count(), 1);
    }
}
