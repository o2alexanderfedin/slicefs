//! File content on the chunk store (docs/design/chunk-store.md, step 2).
//!
//! File data goes into `<store>/packs/` through `chunk_store`: FastCDC chunks
//! with tagged object IDs (a hash tag + a 32-byte digest), a manifest tree,
//! zstd. The metadata keeps a file's content key as an `ObjectKey` (the same
//! 33 bytes as an `ObjectId`).
//!
//! The format marker (`FORMAT`) also records the write hash of the store:
//! the hash of the IDs of new objects, as a line `hash <name>`. A marker
//! without that line means blake3-256. A line `chunks <min> <avg> <max>`
//! records the chunk profile of the store; no such line means 4/8/32 KiB.
//! The metadata nodes are in the same chunk store (`metadata::store_io`,
//! step 3).

use std::path::Path;
use std::sync::Arc;

use slicefs_traits::key::ObjectKey;
use chunk_store::file::{FileReader, FileWriter};
use chunk_store::{hasher_by_name, hasher_names, ChunkProfile, ChunkStore, ObjectHasher, Options, DEFAULT_HASHER};

/// Shared handle of the content store.
pub type Content = Arc<ChunkStore>;

/// The store header (`FORMAT`, its first line and the header of a new
/// store) is owned by `crate::header`.
pub use crate::header::{marker, FORMAT, FORMAT_FILE};
use crate::header::{self, Header, Line, Want};

/// Refuse a path that is not an existing store (no `segments/` directory).
/// The commands that write an existing store (gc, snapshot create and
/// switch) call it before the header check, so that they make nothing on
/// such a path. `seed` and `mount` make new stores and do not call it.
pub fn require_store(store: &Path) -> Result<(), String> {
    if store.join("segments").is_dir() {
        return Ok(());
    }
    Err(format!("failed to load segments: store not found at {}: no segments/ directory", store.display()))
}

/// The write hash that the header of `store` records: blake3-256 if there
/// is no header or no hash line. A name this build does not know is an
/// error.
pub fn write_hasher(store: &Path) -> std::io::Result<&'static dyn ObjectHasher> {
    let h = header::read(store)?.map(|text| header::parse(store, &text)).transpose()?;
    hasher_of(store, h.as_ref())
}

/// The write hash of the parsed header `h` of `store` (see [`write_hasher`]).
fn hasher_of(store: &Path, h: Option<&Header>) -> std::io::Result<&'static dyn ObjectHasher> {
    let Some(name) = h.and_then(Header::hash) else { return Ok(DEFAULT_HASHER) };
    hasher_by_name(name).ok_or_else(|| {
        std::io::Error::other(format!(
            "{} records the write hash {name:?}, which this SliceFS does not know (known: {})",
            store.display(),
            hasher_names()
        ))
    })
}

/// The chunk profile of the parsed header `h`: the default profile
/// (4/8/32 KiB) if there is no header or no `chunks` line.
pub fn profile_of(h: Option<&Header>) -> ChunkProfile {
    h.and_then(Header::chunk_profile).unwrap_or(ChunkProfile::DEFAULT)
}

/// Check `want` (`--chunk-profile`, `None`: none given) against the profile
/// of the parsed header `h` of `store`. The profile of a store is fixed
/// when the store is made: another profile is an error, never a rewrite.
pub fn check_profile(store: &Path, h: Option<&Header>, want: Option<ChunkProfile>) -> std::io::Result<()> {
    let have = profile_of(h);
    match want {
        Some(want) if want != have => Err(std::io::Error::other(header::msg_profile_differs(store, have, want))),
        _ => Ok(()),
    }
}

/// Parse `--chunk-profile`: `MIN/AVG/MAX`, three whole numbers of KiB (for
/// example `16/64/256`) that [`ChunkProfile::new`] accepts. Refuses a sign,
/// a unit, a space, an empty part and an overflow of the byte size.
pub fn parse_chunk_profile(s: &str) -> Result<ChunkProfile, String> {
    let bad = || format!("invalid chunk profile {s:?}; use MIN/AVG/MAX in KiB, for example 16/64/256");
    let kib = |w: &str| {
        if w.is_empty() || !w.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        w.parse::<u32>().ok()?.checked_mul(1024)
    };
    let parts: Vec<&str> = s.split('/').collect();
    let [min, avg, max] = parts[..] else { return Err(bad()) };
    let (min, avg, max) = (kib(min).ok_or_else(bad)?, kib(avg).ok_or_else(bad)?, kib(max).ok_or_else(bad)?);
    ChunkProfile::new(min, avg, max).map_err(|reason| format!("chunk profile {s} KiB: {reason}"))
}

/// Parse `--hash`: the name of a known hasher.
pub fn parse_hash(s: &str) -> Result<String, String> {
    hasher_by_name(s)
        .map(|h| h.name().to_string())
        .ok_or_else(|| format!("unknown hash {s:?}; known: {}", hasher_names()))
}

/// Make `name` (if given) the write hash of `store`: new objects use it,
/// objects of the old hash stay readable (each is verified by its own tag).
/// Prints the line of [`check_write`] when the hash of an existing store
/// changes. Call before the store opens.
pub fn select_hash(store: &Path, name: Option<&str>) -> std::io::Result<()> {
    if name.is_none() {
        return Ok(());
    }
    if let Some(line) = check_write(store, name)? {
        println!("{line}");
    }
    Ok(())
}

/// The header check of a command that writes (`Want::Write`), with the
/// write hash `name` of `--hash` (`None`: keep the recorded one) and the
/// default chunk profile for a new store: [`check_write_with`] with no
/// `--chunk-profile`.
pub fn check_write(store: &Path, name: Option<&str>) -> std::io::Result<Option<String>> {
    check_write_with(store, name, None)
}

/// The header check of a command that writes (`Want::Write`), with the
/// write hash `name` of `--hash` (`None`: keep the recorded one) and the
/// chunk profile `profile` of `--chunk-profile` (`None`: the recorded one,
/// the default for a new store). A store that this build may not write is
/// refused, and is never rewritten. A new store gets the hash and the
/// profile in its first header write. On an existing store, another
/// profile is refused before any rewrite ([`check_profile`]). The hash is
/// recorded in the header, and the line to print is returned when it
/// changes the hash of an existing store. The rewrite changes only the hash
/// line (it is line 2 when there was none); every other line stays in its
/// order.
pub fn check_write_with(store: &Path, name: Option<&str>, profile: Option<ChunkProfile>) -> std::io::Result<Option<String>> {
    // The name first, so that an unknown name makes nothing; a new store
    // gets the new hash and profile in its first header write.
    let new = match name {
        Some(n) => Some(hasher_by_name(n).ok_or_else(|| std::io::Error::other(parse_hash(n).unwrap_err()))?),
        None => None,
    };
    let opened = header::check_with(store, Want::Write, new.unwrap_or(DEFAULT_HASHER), profile.unwrap_or(ChunkProfile::DEFAULT))?;
    check_profile(store, opened.header.as_ref(), profile)?;
    let Some(new) = new else { return Ok(None) };
    let mut h = opened.header.ok_or_else(|| std::io::Error::other(format!("{}/FORMAT is missing after its write", store.display())))?;
    let current = hasher_of(store, Some(&h))?;
    if current.name() == new.name() {
        return Ok(None);
    }
    let line = format!("New objects use hash {} (was {}); stored objects stay readable.", new.name(), current.name());
    match h.lines.iter_mut().find(|l| matches!(l, Line::Hash(_))) {
        Some(l) => *l = Line::Hash(new.name().to_string()),
        None => h.lines.insert(0, Line::Hash(new.name().to_string())),
    }
    header::write_header(store, &h)?;
    Ok(Some(line))
}

/// The temp files of a full GC (`<store>/gc-tmp`; a writable open removes
/// what a crash left).
pub const GC_TMP_DIR: &str = "gc-tmp";

/// Open the content store of `store` to write. The header check comes
/// first (`Want::Write`): a new store gets its header durably before the
/// chunk store makes `packs/`; a store that this build may not write is
/// refused.
pub fn open(store: &Path) -> std::io::Result<Content> {
    let opened = header::check(store, Want::Write)?;
    let hasher = hasher_of(store, opened.header.as_ref())?;
    let opts = Options { hasher, profile: profile_of(opened.header.as_ref()), gc_tmp_dir: Some(store.join(GC_TMP_DIR)), ..Options::default() };
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
/// changes, also while a mount writes to it (for `scrub`). The header
/// check comes first (`Want::Read`).
pub fn open_read_only(store: &Path) -> std::io::Result<Content> {
    let opened = header::check(store, Want::Read)?;
    let hasher = hasher_of(store, opened.header.as_ref())?;
    let opts = Options { read_only: true, hasher, profile: profile_of(opened.header.as_ref()), ..Options::default() };
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

/// Parse `--dirty-budget` and `--dirty-limit`: a whole number of bytes,
/// with an optional binary suffix `K`, `M` or `G` (also `KiB`, `MiB`,
/// `GiB`; any case; 1024-based). Refuses a sign, a fraction, another
/// suffix, an overflow, and a value below `MIN_DIRTY_BYTES` (this also
/// refuses 0).
pub fn parse_dirty_bytes(s: &str) -> Result<usize, String> {
    use crate::filesystem::MIN_DIRTY_BYTES;
    let bad = || format!("invalid size {s:?}; use bytes, or a whole number with K, M or G (1024-based)");
    let digits = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, suffix) = s.split_at(digits);
    if num.is_empty() {
        return Err(bad());
    }
    let unit: usize = match suffix.to_ascii_lowercase().as_str() {
        "" => 1,
        "k" | "kib" => 1 << 10,
        "m" | "mib" => 1 << 20,
        "g" | "gib" => 1 << 30,
        _ => return Err(bad()),
    };
    let bytes = num
        .parse::<usize>()
        .ok()
        .and_then(|n| n.checked_mul(unit))
        .ok_or_else(|| format!("size {s:?} is too large"))?;
    if bytes < MIN_DIRTY_BYTES {
        return Err(format!("size {s:?} is below the minimum of {}M ({MIN_DIRTY_BYTES} bytes)", MIN_DIRTY_BYTES >> 20));
    }
    Ok(bytes)
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

    #[test]
    fn dirty_bytes_parse_with_binary_suffixes() {
        assert_eq!(parse_dirty_bytes("64M"), Ok(67108864));
        assert_eq!(parse_dirty_bytes("4M"), Ok(4194304));
        assert_eq!(parse_dirty_bytes("1G"), Ok(1073741824));
        assert_eq!(parse_dirty_bytes("67108864"), Ok(67108864));
        assert_eq!(parse_dirty_bytes("4MiB"), parse_dirty_bytes("4M"));
        assert_eq!(parse_dirty_bytes("64m"), parse_dirty_bytes("64M"));
        assert_eq!(parse_dirty_bytes("4096K"), parse_dirty_bytes("4M"));
        assert_eq!(parse_dirty_bytes("4096kib"), Ok(4194304));
        assert_eq!(parse_dirty_bytes("1gib"), Ok(1073741824));
    }

    #[test]
    fn dirty_bytes_refuse_bad_values() {
        for v in ["3M", "4194303"] {
            let err = parse_dirty_bytes(v).expect_err(v);
            assert!(err.contains("4M") && err.contains("4194304"), "{v}: the message names the minimum: {err}");
        }
        for v in ["0", "1.5M", "64X", "64MB", "", "M", "+4M", "-4M", " 4M", "99999999999G", "99999999999999999999999"] {
            assert!(parse_dirty_bytes(v).is_err(), "{v:?} is refused");
        }
    }

    #[test]
    fn parse_chunk_profile_takes_whole_kib() {
        assert_eq!(parse_chunk_profile("16/64/256"), Ok(ChunkProfile::new(16384, 65536, 262144).unwrap()));
        assert_eq!(parse_chunk_profile("4/8/32"), Ok(ChunkProfile::DEFAULT));
        for v in ["8/32/128", "32/128/512", "64/256/1024"] {
            assert!(parse_chunk_profile(v).is_ok(), "{v}");
        }
    }

    #[test]
    fn parse_chunk_profile_refuses_bad_values() {
        for v in [
            "16/64",
            "16/64/256/1024",
            "16k/64/256",
            "+16/64/256",
            "-16/64/256",
            " 16/64/256",
            "16//256",
            "12/24/48",
            "64/16/256",
            "0/8/32",
            "1/2/32768",
            "4194304/8388608/16777216",
            "128/512/2048",
        ] {
            assert!(parse_chunk_profile(v).is_err(), "{v:?} is refused");
        }
        let err = parse_chunk_profile("16/64").unwrap_err();
        assert!(err.contains("MIN/AVG/MAX"), "{err}");
    }

    /// A refused profile is named once, in KiB as typed, with the reason.
    #[test]
    fn a_refused_profile_is_named_once_in_kib() {
        for (v, reason) in [
            ("12/24/48", "each size must be a power of two"),
            ("64/16/256", "min < avg < max"),
            ("128/512/2048", "1024 KiB (1 MiB)"),
        ] {
            let err = parse_chunk_profile(v).unwrap_err();
            assert_eq!(err.matches("chunk profile").count(), 1, "{err}");
            assert!(err.starts_with(&format!("chunk profile {v} KiB: ")), "{err}");
            assert!(err.contains(reason) && !err.contains("bytes"), "{err}");
        }
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

    /// [New store]: a new store gets the header at its first open; a store
    /// with data and no header is refused by both opens, and no header is
    /// written.
    #[test]
    fn a_new_store_gets_the_header_and_data_with_no_header_is_refused() {
        let d = tempfile::TempDir::new().unwrap();
        let cs = open(d.path()).unwrap();
        let k = put_all(&cs, b"hello").unwrap();
        assert_eq!(get_all(&cs, &k).unwrap(), b"hello");
        drop(cs);
        assert_eq!(std::fs::read_to_string(d.path().join(FORMAT_FILE)).unwrap(), marker(DEFAULT_HASHER));
        std::fs::remove_file(d.path().join(FORMAT_FILE)).unwrap();
        for err in [open(d.path()).err(), open_read_only(d.path()).err()] {
            let err = err.expect("refused").to_string();
            assert!(err.contains("FORMAT is missing"), "{err}");
        }
        assert!(!d.path().join(FORMAT_FILE).exists(), "no header is written");
    }
}
