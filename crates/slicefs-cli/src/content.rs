//! File content on the chunk store (docs/design/chunk-store.md, step 2).
//!
//! File data goes into `<store>/packs/` through `chunk_store`: FastCDC chunks
//! with BLAKE3-224 IDs, a manifest tree, zstd. The metadata keeps a file's
//! content key as a `Digest224` (28 bytes, the same size as an `ObjectId`).
//! The metadata nodes themselves stay in `vt0/` (blockset) until step 3.

use std::path::Path;
use std::sync::Arc;

use blockset::Digest224;
use chunk_store::file::{FileReader, FileWriter};
use chunk_store::{ChunkStore, ObjectId, Options};

/// Shared handle of the content store.
pub type Content = Arc<ChunkStore>;

/// Marker file of the store format with file content in packs.
pub const FORMAT_FILE: &str = "FORMAT";
pub const FORMAT: &str = "slicefs-store 4 (file content in packs)\n";

/// Refuse a store of an older format. A store with metadata but without the
/// format marker was made by an older SliceFS (file content in `vt0/`): the
/// format changed without a migration. A new (empty) store passes. Call this
/// before the store is opened (mount, gc, stats, scrub).
pub fn check_format(store: &Path) -> std::io::Result<()> {
    match std::fs::read_to_string(store.join(FORMAT_FILE)) {
        Ok(s) if s == FORMAT => Ok(()),
        Ok(s) => Err(std::io::Error::other(format!(
            "{}: unknown store format {:?}",
            store.display(),
            s.trim()
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && has_old_content(store) => {
            Err(std::io::Error::other(format!(
                "{} was made by an older SliceFS (file content in vt0/). The store \
                 format changed without a migration: copy the files out with the \
                 older version, then into a new store",
                store.display()
            )))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Open the content store of `store` (creating it and the format marker if
/// needed).
pub fn open(store: &Path) -> std::io::Result<Content> {
    std::fs::create_dir_all(store)?;
    let marker = store.join(FORMAT_FILE);
    if !marker.exists() {
        std::fs::write(&marker, FORMAT)?;
    }
    let cs = ChunkStore::open(store, Options::default()).map_err(std::io::Error::other)?;
    Ok(Arc::new(cs))
}

/// True if `store` has committed metadata but no format marker.
fn has_old_content(store: &Path) -> bool {
    std::fs::read_dir(store.join("segments")).is_ok_and(|mut d| d.next().is_some())
}

/// `ObjectId` as the `Digest224` that the metadata stores (7 words, LE).
pub fn to_digest(id: ObjectId) -> Digest224 {
    let mut d = [0u32; 7];
    for (w, c) in d.iter_mut().zip(id.0.chunks_exact(4)) {
        *w = u32::from_le_bytes(c.try_into().unwrap());
    }
    d
}

/// The `ObjectId` of a content key from the metadata.
pub fn to_id(d: &Digest224) -> ObjectId {
    let mut id = [0u8; 28];
    for (c, w) in id.chunks_exact_mut(4).zip(d.iter()) {
        c.copy_from_slice(&w.to_le_bytes());
    }
    ObjectId(id)
}

/// A content stream: the replacement of the blockset `State` for a file
/// being written. `clone()` + `finish()` gives the content so far.
#[derive(Clone, Default)]
pub struct Stream(FileWriter);

impl Stream {
    pub fn push(&mut self, cs: &ChunkStore, data: &[u8]) -> std::io::Result<()> {
        self.0.write(cs, data).map_err(std::io::Error::other)
    }

    pub fn push_zeros(&mut self, cs: &ChunkStore, n: u64) -> std::io::Result<()> {
        self.0.write_zeros(cs, n).map_err(std::io::Error::other)
    }

    pub fn len(&self) -> u64 {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn finish(self, cs: &ChunkStore) -> std::io::Result<Digest224> {
        self.0.finish(cs).map(to_digest).map_err(std::io::Error::other)
    }
}

/// Store `data` as one file content and return its key.
pub fn put_all(cs: &ChunkStore, data: &[u8]) -> std::io::Result<Digest224> {
    let mut s = Stream::default();
    s.push(cs, data)?;
    s.finish(cs)
}

/// All bytes of the content `key`, or `None` if an object is missing or
/// damaged.
pub fn get_all(cs: &ChunkStore, key: &Digest224) -> Option<Vec<u8>> {
    let mut r = FileReader::open(cs, to_id(key)).ok()?;
    let n = r.len() as usize;
    r.read(cs, 0, n).ok()
}

/// A reader of one content; reads keep their node and chunk caches.
pub struct Reader(FileReader);

impl Reader {
    pub fn open(cs: &ChunkStore, key: &Digest224) -> std::io::Result<Self> {
        FileReader::open(cs, to_id(key)).map(Reader).map_err(std::io::Error::other)
    }

    pub fn key(&self) -> Digest224 {
        to_digest(self.0.key())
    }

    pub fn len(&self) -> u64 {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn read(&mut self, cs: &ChunkStore, offset: u64, size: usize) -> std::io::Result<Vec<u8>> {
        self.0.read(cs, offset, size).map_err(std::io::Error::other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_and_id_convert_both_ways() {
        let id = ObjectId::of(b"abc");
        assert_eq!(to_id(&to_digest(id)), id);
    }

    #[test]
    fn a_new_store_gets_the_marker_and_an_old_store_is_refused() {
        let d = tempfile::TempDir::new().unwrap();
        let cs = open(d.path()).unwrap();
        let k = put_all(&cs, b"hello").unwrap();
        assert_eq!(get_all(&cs, &k).unwrap(), b"hello");
        assert_eq!(std::fs::read_to_string(d.path().join(FORMAT_FILE)).unwrap(), FORMAT);
        drop(cs);
        assert!(check_format(d.path()).is_ok(), "a store with the marker passes");
        assert!(check_format(tempfile::TempDir::new().unwrap().path()).is_ok(), "an empty store passes");

        let old = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(old.path().join("segments")).unwrap();
        std::fs::write(old.path().join("segments/0000000001.seg"), b"x").unwrap();
        let err = check_format(old.path()).err().unwrap().to_string();
        assert!(err.contains("older SliceFS"), "{err}");
    }
}
