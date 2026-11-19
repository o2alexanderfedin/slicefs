//! File content on the chunk store (docs/design/chunk-store.md, step 2).
//!
//! File data goes into `<store>/packs/` through `chunk_store`: FastCDC chunks
//! with tagged object IDs (a hash tag + a 32-byte digest), a manifest tree,
//! zstd. The metadata keeps a file's content key as an `ObjectKey` (the same
//! 33 bytes as an `ObjectId`).
//!
//! The format marker (`FORMAT`) also records the write hash of the store:
//! the hash of the IDs of new objects, as a line `hash <name>`. A marker
//! without that line means blake3-256.
//! The metadata nodes are in the same chunk store (`metadata::store_io`,
//! step 3).

use std::path::Path;
use std::sync::Arc;

use slicefs_traits::key::ObjectKey;
use chunk_store::file::{FileReader, FileWriter};
use chunk_store::{hasher_by_name, hasher_names, ChunkStore, ObjectHasher, Options, DEFAULT_HASHER};

/// Shared handle of the content store.
pub type Content = Arc<ChunkStore>;

/// Marker file of the store format with file content in packs.
pub const FORMAT_FILE: &str = "FORMAT";
/// The first line of the marker.
pub const FORMAT: &str = "slicefs-store 6 (metadata and file content in packs, tagged object IDs)\n";
/// Prefix of the marker line that records the write hash.
const HASH_LINE: &str = "hash ";

/// The marker of a store whose new objects use `hasher`.
pub fn marker(hasher: &dyn ObjectHasher) -> String {
    format!("{FORMAT}{HASH_LINE}{}\n", hasher.name())
}

fn read_marker(store: &Path) -> std::io::Result<Option<String>> {
    match std::fs::read_to_string(store.join(FORMAT_FILE)) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// The write hash that the marker of `store` records: blake3-256 if there is
/// no marker or no hash line. A name this build does not know is an error.
pub fn write_hasher(store: &Path) -> std::io::Result<&'static dyn ObjectHasher> {
    let Some(m) = read_marker(store)? else { return Ok(DEFAULT_HASHER) };
    let Some(name) = m.lines().skip(1).find_map(|l| l.strip_prefix(HASH_LINE)).map(str::trim) else {
        return Ok(DEFAULT_HASHER);
    };
    hasher_by_name(name).ok_or_else(|| {
        std::io::Error::other(format!(
            "{} records the write hash {name:?}, which this SliceFS does not know (known: {})",
            store.display(),
            hasher_names()
        ))
    })
}

/// Parse `--hash`: the name of a known hasher.
pub fn parse_hash(s: &str) -> Result<String, String> {
    hasher_by_name(s)
        .map(|h| h.name().to_string())
        .ok_or_else(|| format!("unknown hash {s:?}; known: {}", hasher_names()))
}

/// Make `name` (if given) the write hash of `store`: new objects use it,
/// objects of the old hash stay readable (each is verified by its own tag).
/// Records it in the marker (made for a new store) and, if it changes the
/// hash of an existing store, prints one line. Call before the store opens.
pub fn select_hash(store: &Path, name: Option<&str>) -> std::io::Result<()> {
    let Some(name) = name else { return Ok(()) };
    let new = hasher_by_name(name).ok_or_else(|| std::io::Error::other(parse_hash(name).unwrap_err()))?;
    std::fs::create_dir_all(store)?;
    let old = read_marker(store)?;
    if let Some(m) = &old {
        if !m.starts_with(FORMAT) {
            check_format(store)?; // the message for an older format
            return Err(std::io::Error::other(format!("{}: unknown store format {:?}", store.display(), m.trim())));
        }
        let current = write_hasher(store)?;
        if current.name() == new.name() {
            return Ok(());
        }
        println!("New objects use hash {} (was {}); stored objects stay readable.", new.name(), current.name());
    }
    write_marker(store, new)
}

/// Write the format marker durably: a synced temp file renamed over the old
/// marker, then a sync of the directory. A power loss leaves the old marker
/// or the new one, never an empty or torn file.
fn write_marker(store: &Path, hasher: &dyn ObjectHasher) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = store.join(format!("{FORMAT_FILE}.tmp"));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(marker(hasher).as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, store.join(FORMAT_FILE))?;
    std::fs::File::open(store)?.sync_all()
}

/// Refuse a store of an older format, whose format changed without a
/// migration. A store passes if it has the format marker, or if it has no
/// marker and its current root node is in the chunk store (a new store, or
/// one made by code that writes no marker, as in tests).
/// Call this before the store is opened (mount, gc, stats, scrub).
pub fn check_format(store: &Path) -> std::io::Result<()> {
    let older_as = |what: &str| {
        std::io::Error::other(format!(
            "{} was made by an older SliceFS ({what}). The store \
             format changed without a migration: copy the files out with the \
             older version, then into a new store",
            store.display()
        ))
    };
    let older = || older_as("metadata in vt0/");
    match std::fs::read_to_string(store.join(FORMAT_FILE)) {
        Ok(s) if s.starts_with(FORMAT) => return Ok(()),
        Ok(s) if s.starts_with("slicefs-store 5 ") => return Err(older_as("224-bit object IDs")),
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

/// The temp files of a full GC (`<store>/gc-tmp`; a writable open removes
/// what a crash left).
pub const GC_TMP_DIR: &str = "gc-tmp";

/// Open the content store of `store` (creating it and the format marker if
/// needed).
pub fn open(store: &Path) -> std::io::Result<Content> {
    std::fs::create_dir_all(store)?;
    let path = store.join(FORMAT_FILE);
    if !path.exists() {
        write_marker(store, DEFAULT_HASHER)?;
    }
    let opts = Options { hasher: write_hasher(store)?, gc_tmp_dir: Some(store.join(GC_TMP_DIR)), ..Options::default() };
    let cs = ChunkStore::open_with(storage(store), opts).map_err(std::io::Error::other)?;
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
    let opts = Options { read_only: true, hasher: write_hasher(store)?, ..Options::default() };
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

pub use metadata::store_io::{to_key, to_id};

/// A content stream of a file being written. `clone()` + `finish()` gives the content so far.
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

    pub fn finish(self, cs: &ChunkStore) -> std::io::Result<ObjectKey> {
        self.0.finish(cs).map(to_key).map_err(std::io::Error::other)
    }
}

/// A new content from the content `base`: the bytes of `src` in the
/// `changed` ranges, the base bytes below `keep`, zeros up to `new_len`
/// (`chunk_store::file::splice`). Only the chunks near the changed bytes are
/// cut and hashed again; the key is the key of a full write of the same bytes.
pub fn splice(
    cs: &ChunkStore,
    base: Option<&ObjectKey>,
    keep: u64,
    changed: &[std::ops::Range<u64>],
    new_len: u64,
    src: &mut dyn chunk_store::file::SpliceSource,
) -> std::io::Result<(ObjectKey, chunk_store::file::SpliceStats)> {
    chunk_store::file::splice(cs, base.map(to_id), keep, changed, new_len, src)
        .map(|(id, stats)| (to_key(id), stats))
        .map_err(std::io::Error::other)
}

/// Store `data` as one file content and return its key.
pub fn put_all(cs: &ChunkStore, data: &[u8]) -> std::io::Result<ObjectKey> {
    let mut s = Stream::default();
    s.push(cs, data)?;
    s.finish(cs)
}

/// The read size of [`put_reader`].
pub const READ_BUF: usize = 1 << 20;

/// Store all bytes of `r` as one file content, in reads of [`READ_BUF`]
/// bytes; return its key and length. The memory does not depend on the
/// length of the content. The key is the same as of [`put_all`] for the
/// same bytes.
pub fn put_reader(cs: &ChunkStore, r: &mut impl std::io::Read) -> std::io::Result<(ObjectKey, u64)> {
    let mut s = Stream::default();
    let mut buf = vec![0u8; READ_BUF];
    loop {
        match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => s.push(cs, &buf[..n])?,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let len = s.len();
    Ok((s.finish(cs)?, len))
}

/// All bytes of the content `key`, or `None` if an object is missing or
/// damaged.
pub fn get_all(cs: &ChunkStore, key: &ObjectKey) -> Option<Vec<u8>> {
    let mut r = FileReader::open(cs, to_id(key)).ok()?;
    let n = r.len() as usize;
    r.read(cs, 0, n).ok()
}

/// A reader of one content; reads keep their node and chunk caches.
pub struct Reader(FileReader);

impl Reader {
    pub fn open(cs: &ChunkStore, key: &ObjectKey) -> std::io::Result<Self> {
        FileReader::open(cs, to_id(key)).map(Reader).map_err(std::io::Error::other)
    }

    pub fn key(&self) -> ObjectKey {
        to_key(self.0.key())
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
pub(crate) mod tests {
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
        meta.set_manifest(ino, &[ObjectKey::example(1)]).unwrap();
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
        assert!(err.contains("older SliceFS (metadata in vt0/)"), "{err}");
        std::fs::write(d.path().join(FORMAT_FILE), "slicefs-store 5 (metadata and file content in packs)\n").unwrap();
        let err = check_format(d.path()).err().unwrap().to_string();
        assert!(err.contains("older SliceFS (224-bit object IDs)"), "{err}");
    }

    /// Pseudo-random bytes (xorshift64): no FastCDC cut is at a fixed place.
    pub(crate) fn random_bytes(n: usize, mut seed: u64) -> Vec<u8> {
        (0..n)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed as u8
            })
            .collect()
    }

    /// A reader gives the same key as `put_all` for the same bytes, also
    /// across chunk and read-buffer boundaries.
    #[test]
    fn put_reader_key_equals_put_all_key() {
        let d = tempfile::TempDir::new().unwrap();
        let cs = open(d.path()).unwrap();
        let max = chunk_store::CHUNK_MAX as usize;
        let cases: Vec<Vec<u8>> = vec![
            Vec::new(),
            vec![7],
            random_bytes(max, 1),
            random_bytes(2 * max + 1, 2),
            random_bytes(3 * READ_BUF + 12_345, 3),
        ];
        for data in cases {
            let (key, len) = put_reader(&cs, &mut data.as_slice()).unwrap();
            assert_eq!(len, data.len() as u64);
            assert_eq!(key, put_all(&cs, &data).unwrap(), "key differs for {} bytes", data.len());
            assert_eq!(get_all(&cs, &key).unwrap(), data);
        }
    }

    #[test]
    fn digest_and_id_convert_both_ways() {
        let id = chunk_store::ObjectHasher::id(&chunk_store::Blake3_256, b"abc");
        assert_eq!(to_id(&to_key(id)), id);
    }

    #[test]
    fn a_new_store_gets_the_marker_and_an_old_store_is_refused() {
        let d = tempfile::TempDir::new().unwrap();
        let cs = open(d.path()).unwrap();
        let k = put_all(&cs, b"hello").unwrap();
        assert_eq!(get_all(&cs, &k).unwrap(), b"hello");
        assert_eq!(std::fs::read_to_string(d.path().join(FORMAT_FILE)).unwrap(), marker(DEFAULT_HASHER));
        drop(cs);
        assert!(check_format(d.path()).is_ok(), "a store with the marker passes");
        assert!(check_format(tempfile::TempDir::new().unwrap().path()).is_ok(), "an empty store passes");

        let old = tempfile::TempDir::new().unwrap();
        make_old_store(old.path());
        let err = check_format(old.path()).err().unwrap().to_string();
        assert!(err.contains("older SliceFS"), "{err}");
    }
}
