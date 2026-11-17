//! File content on the chunk store (docs/design/chunk-store.md, step 2).
//!
//! File data goes into `<store>/packs/` through `chunk_store`: FastCDC chunks
//! with BLAKE3-224 IDs, a manifest tree, zstd. The metadata keeps a file's
//! content key as a `Digest224` (28 bytes, the same size as an `ObjectId`).
//! The metadata nodes are in the same chunk store (`metadata::store_io`,
//! step 3).

use std::path::Path;
use std::sync::Arc;

use blockset::Digest224;
use chunk_store::file::{FileReader, FileWriter};
use chunk_store::{ChunkStore, ObjectId, Options};

/// Shared handle of the content store.
pub type Content = Arc<ChunkStore>;

/// Marker file of the store format with file content in packs.
pub const FORMAT_FILE: &str = "FORMAT";
pub const FORMAT: &str = "slicefs-store 5 (metadata and file content in packs)\n";

/// Refuse a store of an older format, whose format changed without a
/// migration. A store passes if it has the format marker, or if it has no
/// marker and its current root node is in the chunk store (a new store, or
/// one made by code that writes no marker, as in tests).
/// Call this before the store is opened (mount, gc, stats, scrub).
pub fn check_format(store: &Path) -> std::io::Result<()> {
    let older = || {
        std::io::Error::other(format!(
            "{} was made by an older SliceFS (metadata in vt0/). The store \
             format changed without a migration: copy the files out with the \
             older version, then into a new store",
            store.display()
        ))
    };
    match std::fs::read_to_string(store.join(FORMAT_FILE)) {
        Ok(s) if s == FORMAT => return Ok(()),
        Ok(s) if s.starts_with("slicefs-store ") => return Err(older()),
        Ok(s) => {
            return Err(std::io::Error::other(format!(
                "{}: unknown store format {:?}",
                store.display(),
                s.trim()
            )))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let segs = store.join("segments");
    if !segs.is_dir() {
        return Ok(());
    }
    let Ok((Some(root), _)) = metadata::segment::load_store_from_segments(&segs) else {
        return Ok(()); // nothing committed (or a damaged log: later steps report it)
    };
    if open_read_only(store)?.contains(&to_id(&root)) {
        Ok(())
    } else {
        Err(older())
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
    let cs = ChunkStore::open_with(storage(store), Options::default()).map_err(std::io::Error::other)?;
    Ok(Arc::new(cs))
}

/// The storage strategy of a store: chosen here, once, and injected into the
/// chunk store (the store itself makes no checks of its environment). Files,
/// with a retry of failed syncs that rewrites the unsynced data first.
fn storage(store: &Path) -> Arc<dyn chunk_store::pack_io::PackIo> {
    let files = Arc::new(chunk_store::pack_io::FileIo::new(store.join("packs")));
    Arc::new(chunk_store::retry::RetryingIo::new(files))
}

/// The node store (metadata) of `store`, on the same chunk store as the
/// file content.
pub fn store_io(store: &Path) -> std::io::Result<metadata::store_io::StoreIo> {
    Ok(metadata::store_io::StoreIo::new(store, open(store)?))
}

/// The node store of `store` for reading only (see [`open_read_only`]).
/// Load the root from the segments first, then open this: the open sees
/// only the objects that are in the packs at that time.
pub fn store_io_read_only(store: &Path) -> std::io::Result<metadata::store_io::StoreIo> {
    Ok(metadata::store_io::StoreIo::new(store, open_read_only(store)?))
}

/// Open the content store of `store` for reading only: nothing on disk
/// changes, also while a mount writes to it (for `scrub`).
pub fn open_read_only(store: &Path) -> std::io::Result<Content> {
    let opts = Options { read_only: true, ..Options::default() };
    let cs = ChunkStore::open_with(storage(store), opts).map_err(std::io::Error::other)?;
    Ok(Arc::new(cs))
}

/// Parse `--compression`: `none`, `zstd` (level 3) or `zstd:LEVEL` (1-22).
/// Returns the zstd level, 0 for none.
pub fn parse_compression(s: &str) -> Result<i32, String> {
    match s {
        "none" | "off" => Ok(0),
        "zstd" => Ok(3),
        _ => {
            let level = s
                .strip_prefix("zstd:")
                .and_then(|l| l.parse::<i32>().ok())
                .ok_or_else(|| format!("unknown compression {s:?}; use zstd, zstd:LEVEL or none"))?;
            if (1..=22).contains(&level) {
                Ok(level)
            } else {
                Err(format!("zstd level {level} is not in 1-22"))
            }
        }
    }
}

pub use metadata::store_io::{to_digest, to_id};

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
    fn compression_specs_parse_to_zstd_levels() {
        assert_eq!(parse_compression("none"), Ok(0));
        assert_eq!(parse_compression("zstd"), Ok(3));
        assert_eq!(parse_compression("zstd:19"), Ok(19));
        assert!(parse_compression("zstd:0").is_err());
        assert!(parse_compression("zstd:23").is_err());
        assert!(parse_compression("lz4").is_err());
    }

    /// A committed store whose root node is not in the chunk store (as with
    /// metadata in vt0/).
    fn make_old_store(dir: &Path) {
        use metadata::store::DictMetadataStore;
        use slicefs_traits::metadata::{InodeMeta, MetadataStore};
        std::fs::create_dir_all(dir.join("segments")).unwrap();
        let wal = metadata::wal::create_wal(metadata::wal::WalConfig::PerOp, dir, 1).unwrap();
        let io = Arc::new(std::sync::Mutex::new(metadata::store_io::StoreIo::open(dir).unwrap()));
        let mut meta = DictMetadataStore::new(io);
        meta.set_wal(wal);
        let ino = meta.create_inode(&InodeMeta::new_file(0, 0, 0, 0o100644)).unwrap();
        meta.link(1, "f", ino).unwrap();
        meta.set_manifest(ino, &[[1, 2, 3, 4, 5, 6, 7]]).unwrap();
        meta.commit().unwrap();
        meta.shutdown_wal().unwrap();
        drop(meta);
        std::fs::remove_dir_all(dir.join("packs")).unwrap();
    }

    /// A marker of an older format is refused at once.
    #[test]
    fn an_older_format_marker_is_refused() {
        let d = tempfile::TempDir::new().unwrap();
        std::fs::write(d.path().join(FORMAT_FILE), "slicefs-store 4 (file content in packs)\n").unwrap();
        let err = check_format(d.path()).err().unwrap().to_string();
        assert!(err.contains("older SliceFS"), "{err}");
    }

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
        make_old_store(old.path());
        let err = check_format(old.path()).err().unwrap().to_string();
        assert!(err.contains("older SliceFS"), "{err}");
    }
}
