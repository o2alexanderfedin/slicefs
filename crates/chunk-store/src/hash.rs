//! Hash strategies for object IDs.
//!
//! An `ObjectId` is self-describing: a 1-byte tag names the hash, then the
//! 32-byte digest. The store writes new objects with one hasher (its write
//! hasher, injected in `Options`), and verifies each object with the hasher
//! of the object's own tag. So a store can change its write hash and still
//! read and verify the objects of the older hash.

use crate::{ObjectId, DIGEST_LEN};
use sha2::{Digest, Sha256 as Sha2};

/// A hash algorithm for object IDs. Every algorithm gives `DIGEST_LEN`
/// (32) bytes; there are no variable-length digests.
pub trait ObjectHasher: Send + Sync {
    /// Stable name, as in the store's format marker and `--hash`.
    fn name(&self) -> &'static str;
    /// The tag byte of the IDs that this hasher makes (first byte of an ID).
    fn tag(&self) -> u8;
    /// The digest of `data`.
    fn digest(&self, data: &[u8]) -> [u8; DIGEST_LEN];

    /// The tagged ID of `data`.
    fn id(&self, data: &[u8]) -> ObjectId {
        ObjectId::new(self.tag(), self.digest(data))
    }
}

impl std::fmt::Debug for dyn ObjectHasher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

/// BLAKE3, the full 256-bit output. Tag 0x1e: the multihash code of BLAKE3.
#[derive(Clone, Copy, Debug, Default)]
pub struct Blake3_256;

impl ObjectHasher for Blake3_256 {
    fn name(&self) -> &'static str {
        "blake3-256"
    }
    fn tag(&self) -> u8 {
        0x1e
    }
    fn digest(&self, data: &[u8]) -> [u8; DIGEST_LEN] {
        *blake3::hash(data).as_bytes()
    }
}

/// SHA-256. Tag 0x12: the multihash code of sha2-256. The `sha2` crate
/// picks SHA-NI (x86_64) or the ARMv8 SHA2 instructions at run time when
/// the CPU has them.
#[derive(Clone, Copy, Debug, Default)]
pub struct Sha256;

impl ObjectHasher for Sha256 {
    fn name(&self) -> &'static str {
        "sha256"
    }
    fn tag(&self) -> u8 {
        0x12
    }
    fn digest(&self, data: &[u8]) -> [u8; DIGEST_LEN] {
        Sha2::digest(data).into()
    }
}

/// A second hash for tests of hash changes: keyed BLAKE3, tag 0xf0 (a tag
/// in no real store). Only in test builds (`cfg(test)` or the `test-hasher`
/// feature, which the dev-dependencies of the other crates turn on).
#[cfg(any(test, feature = "test-hasher"))]
#[derive(Clone, Copy, Debug, Default)]
pub struct TestKeyedBlake3;

#[cfg(any(test, feature = "test-hasher"))]
impl ObjectHasher for TestKeyedBlake3 {
    fn name(&self) -> &'static str {
        "test-keyed-blake3"
    }
    fn tag(&self) -> u8 {
        0xf0
    }
    fn digest(&self, data: &[u8]) -> [u8; DIGEST_LEN] {
        *blake3::keyed_hash(b"slicefs test hasher, not for use", data).as_bytes()
    }
}

/// The default write hasher.
pub const DEFAULT_HASHER: &dyn ObjectHasher = &Blake3_256;

static HASHERS: &[&dyn ObjectHasher] = &[
    &Blake3_256,
    &Sha256,
    #[cfg(any(test, feature = "test-hasher"))]
    &TestKeyedBlake3,
];

/// Every hasher this build knows.
pub fn hashers() -> &'static [&'static dyn ObjectHasher] {
    HASHERS
}

/// The known hasher with this name.
pub fn hasher_by_name(name: &str) -> Option<&'static dyn ObjectHasher> {
    hashers().iter().copied().find(|h| h.name() == name)
}

/// The known hasher with this tag.
pub fn hasher_by_tag(tag: u8) -> Option<&'static dyn ObjectHasher> {
    hashers().iter().copied().find(|h| h.tag() == tag)
}

/// The names of the known hashers, comma-separated (for error messages).
pub fn hasher_names() -> String {
    hashers().iter().map(|h| h.name()).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::{file_objects, FileReader, FileWriter};
    use crate::mem::MemIo;
    use crate::pack_io::{Kind, PackIo};
    use crate::{ChunkStore, Error, ObjectId, Options};
    use std::collections::HashSet;
    use std::sync::Arc;

    #[test]
    fn lookup_by_name_and_tag() {
        assert_eq!(hasher_by_name("blake3-256").unwrap().tag(), 0x1e);
        assert_eq!(hasher_by_tag(0x1e).unwrap().name(), "blake3-256");
        assert!(hasher_by_name("sha1").is_none());
        assert!(hasher_by_tag(0x00).is_none());
        let id = Blake3_256.id(b"abc");
        assert_eq!(id.tag(), 0x1e);
        assert_eq!(id.digest(), blake3::hash(b"abc").as_bytes());
        assert_ne!(TestKeyedBlake3.id(b"abc").digest(), id.digest());
        assert_eq!(hasher_by_name("sha256").unwrap().tag(), 0x12);
        assert_eq!(hasher_by_tag(0x12).unwrap().name(), "sha256");
        assert!(hasher_names().split(", ").any(|n| n == "sha256"), "{}", hasher_names());
    }

    fn hex(d: &[u8]) -> String {
        d.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// D-10: the SHA-256 digest equals the known vectors (FIPS 180 examples).
    #[test]
    fn sha256_golden_vectors() {
        let million_a = vec![b'a'; 1_000_000];
        let cases: [(&[u8], &str); 4] = [
            (b"", "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),
            (b"abc", "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
            (
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
                "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
            ),
            (&million_a, "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"),
        ];
        for (input, want) in cases {
            assert_eq!(hex(&Sha256.digest(input)), want, "input of {} bytes", input.len());
        }
        assert_eq!(Sha256.id(b"abc").tag(), 0x12);
        assert_eq!(Sha256.name(), "sha256");
    }

    /// The speed of SHA-256 on 8 KiB blocks, by hand only (VALIDATION manual
    /// check): over 1,000 MB/s means a hardware path, about 250 MB/s the
    /// soft path. Run it in release mode.
    #[test]
    #[ignore = "by hand: prints the SHA-256 speed of 8 KiB blocks"]
    fn sha256_speed() {
        let block = data(9, 8192);
        let start = std::time::Instant::now();
        let (mut n, mut acc) = (0u64, 0u8);
        while start.elapsed() < std::time::Duration::from_secs(1) {
            for _ in 0..1000 {
                acc ^= Sha256.digest(&block)[0];
            }
            n += 1000;
        }
        let mb = (n * 8192) as f64 / 1e6 / start.elapsed().as_secs_f64();
        println!("sha256: {mb:.0} MB/s on 8 KiB blocks (check byte {acc})");
    }

    /// The invariant of the tagged IDs: a store with any known write hash
    /// puts a file and reads it back, and the key has the tag of that hash.
    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot run the zstd C code (ZSTD_createCCtx)")]
    fn every_known_hasher_round_trips() {
        let bytes = data(7, 100_000);
        for &h in hashers() {
            let mem = MemIo::new();
            let s = open(&mem, h);
            let key = write_file(&s, &bytes);
            assert_eq!(key.tag(), h.tag(), "{}", h.name());
            assert!(file_objects(&s, key).unwrap().iter().all(|id| id.tag() == h.tag()), "{}", h.name());
            assert_eq!(read_file(&s, key), bytes, "{}", h.name());
        }
    }

    /// A hash that no build knows (as if a newer SliceFS wrote the store).
    struct Unknown;
    impl ObjectHasher for Unknown {
        fn name(&self) -> &'static str {
            "unknown-test"
        }
        fn tag(&self) -> u8 {
            0x77
        }
        fn digest(&self, data: &[u8]) -> [u8; DIGEST_LEN] {
            *blake3::hash(data).as_bytes()
        }
    }

    fn open(mem: &MemIo, hasher: &'static dyn ObjectHasher) -> ChunkStore {
        ChunkStore::open_with(Arc::new(mem.clone()), Options { hasher, ..Options::default() }).unwrap()
    }

    fn data(seed: u64, n: usize) -> Vec<u8> {
        let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        (0..n).map(|_| { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x as u8 }).collect()
    }

    fn write_file(s: &ChunkStore, bytes: &[u8]) -> ObjectId {
        let mut w = FileWriter::default();
        w.write(s, bytes).unwrap();
        w.finish(s).unwrap()
    }

    fn read_file(s: &ChunkStore, key: ObjectId) -> Vec<u8> {
        let mut r = FileReader::open(s, key).unwrap();
        let n = r.len() as usize;
        r.read(s, 0, n).unwrap()
    }

    /// Reopened with another write hash, the store reads and verifies the
    /// old objects, and new objects carry the new tag.
    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot run the zstd C code (ZSTD_createCCtx)")]
    fn a_new_write_hash_keeps_old_objects_readable() {
        let mem = MemIo::new();
        let a = data(1, 100_000);
        let ka = {
            let s = open(&mem, &Blake3_256);
            let k = write_file(&s, &a);
            s.close().unwrap();
            k
        };
        assert_eq!(ka.tag(), 0x1e);

        let s = open(&mem, &TestKeyedBlake3);
        assert_eq!(s.hasher().name(), "test-keyed-blake3");
        assert_eq!(read_file(&s, ka), a, "old file under the new write hash");
        let b = data(2, 100_000);
        let kb = write_file(&s, &b);
        assert_eq!(kb.tag(), 0xf0, "new objects carry the new tag");
        assert!(file_objects(&s, kb).unwrap().iter().all(|id| id.tag() == 0xf0));
        assert_eq!(read_file(&s, kb), b);
        s.close().unwrap();
        drop(s);

        // Without the indexes, the open scan of the sealed packs verifies
        // every record with the hasher of its tag: both tags pass.
        let mut nums = mem.list().unwrap();
        nums.sort_unstable();
        nums.pop(); // the newest pack is scanned as the active pack (no hash check)
        assert!(!nums.is_empty());
        for num in nums {
            mem.remove(num, Kind::Idx).unwrap();
        }
        let s = open(&mem, &Blake3_256);
        for k in [ka, kb] {
            for id in file_objects(&s, k).unwrap() {
                s.read(&id).unwrap();
            }
        }
        assert_eq!(read_file(&s, ka), a);
        assert_eq!(read_file(&s, kb), b);
    }

    /// GC keeps the live objects of both hashes and frees the dead ones.
    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot run the zstd C code (ZSTD_createCCtx)")]
    fn gc_keeps_live_objects_of_both_hashes() {
        let mem = MemIo::new();
        let s = open(&mem, &Blake3_256);
        let (live_old, dead_old) = (write_file(&s, &data(1, 60_000)), write_file(&s, &data(2, 60_000)));
        s.close().unwrap();
        drop(s);
        let s = open(&mem, &TestKeyedBlake3);
        let (live_new, dead_new) = (write_file(&s, &data(3, 60_000)), write_file(&s, &data(4, 60_000)));

        let objects = |keys: [ObjectId; 2]| -> HashSet<ObjectId> {
            keys.iter().flat_map(|k| file_objects(&s, *k).unwrap()).collect()
        };
        let live = objects([live_old, live_new]);
        let dead: Vec<ObjectId> = objects([dead_old, dead_new]).into_iter().filter(|id| !live.contains(id)).collect();
        assert!(dead.iter().any(|id| id.tag() == 0x1e) && dead.iter().any(|id| id.tag() == 0xf0));

        let boundary = s.gc_begin().unwrap();
        let st = s.gc_finish(&live, boundary).unwrap();
        assert_eq!(st.removed_objects as usize, dead.len(), "{st:?}");
        assert!(dead.iter().all(|id| !s.contains(id)), "dead objects of both tags are freed");
        assert_eq!(read_file(&s, live_old), data(1, 60_000));
        assert_eq!(read_file(&s, live_new), data(3, 60_000));
    }

    /// An object whose tag this build does not know is an error: on read,
    /// and in the open scan of a pack without its index. Never a panic.
    #[test]
    fn an_unknown_hash_tag_is_an_error() {
        let mem = MemIo::new();
        let s = open(&mem, &Unknown);
        let id = s.put(b"written by a newer SliceFS").unwrap();
        assert!(matches!(s.read(&id), Err(Error::UnknownHash(x)) if x == id));
        assert!(s.get(&id).is_none());
        assert!(s.read(&id).unwrap_err().to_string().contains("does not know"));
        let mut w = FileWriter::default();
        w.write(&s, b"a file").unwrap();
        let key = w.finish(&s).unwrap();
        assert!(matches!(FileReader::open(&s, key), Err(Error::UnknownHash(_))));
        s.close().unwrap();
        drop(s);

        // A second pack, so the first is a sealed pack; then lose its index.
        let s = open(&mem, &Blake3_256);
        s.put(b"known").unwrap();
        s.close().unwrap();
        drop(s);
        let first = *mem.list().unwrap().iter().min().unwrap();
        mem.remove(first, Kind::Idx).unwrap();
        let r = ChunkStore::open_with(Arc::new(mem.clone()), Options::default());
        assert!(matches!(r, Err(Error::UnknownHash(_))), "the scan meets an unknown tag");
    }

    /// A pack of the older format (version 1: 32-byte IDs) is refused at
    /// open, not misread.
    #[test]
    fn an_older_pack_format_is_refused() {
        let mem = MemIo::new();
        let mut f = mem.create(1).unwrap();
        let mut h = b"SFPK".to_vec();
        h.extend_from_slice(&1u16.to_le_bytes());
        h.extend_from_slice(&1u64.to_le_bytes());
        f.write_at(&h, 0).unwrap();
        f.sync().unwrap();
        drop(f);
        let err = ChunkStore::open_with(Arc::new(mem.clone()), Options::default()).err().expect("refused");
        assert!(err.to_string().contains("format version 1"), "{err}");
    }
}
