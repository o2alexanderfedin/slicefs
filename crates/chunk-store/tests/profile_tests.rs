//! Chunk profiles (Phase 22): the limits of a profile, the zero chunk of
//! each store, the golden cut points, stores with different profiles in one
//! process, and an old reader of a store with another profile.

use std::sync::Arc;

use chunk_store::file::{splice, FileReader, FileWriter, SpliceSource};
use chunk_store::mem::MemIo;
use chunk_store::{chunk_ranges, ChunkProfile, ChunkStore, ObjectId, Options};
use tempfile::TempDir;

const KIB: u32 = 1024;
const MIB: u32 = 1024 * 1024;

/// `n` xorshift64 bytes from the start state `seed` (used as is).
fn xorshift(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

fn kib(min: u32, avg: u32, max: u32) -> ChunkProfile {
    ChunkProfile::new(min * KIB, avg * KIB, max * KIB).unwrap()
}

#[test]
fn the_five_profiles_and_the_limits_are_accepted() {
    assert_eq!(ChunkProfile::new(4096, 8192, 32768), Ok(ChunkProfile::DEFAULT));
    for (a, b, c) in [(8, 16, 64), (8, 32, 128), (16, 64, 256), (32, 128, 512)] {
        assert!(ChunkProfile::new(a * KIB, b * KIB, c * KIB).is_ok(), "{a}/{b}/{c} KiB");
    }
    assert!(ChunkProfile::new(64, 256, 1024).is_ok());
    assert!(ChunkProfile::new(256 * KIB, 512 * KIB, MIB).is_ok());
    assert_eq!(ChunkProfile::DEFAULT.to_string(), "4/8/32 KiB");
    assert_eq!(kib(16, 64, 256).to_string(), "16/64/256 KiB");
    assert_eq!(ChunkProfile::new(64, 256, 1024).unwrap().to_string(), "64/256/1024 B");
    let x = xorshift(300 << 10, 0x2267_c0de_5eed_0002);
    assert_eq!(chunk_ranges(&x), ChunkProfile::DEFAULT.ranges(&x));
}

/// The max is at most 1 MiB (`ChunkProfile::MAX_LIMIT`): a stream holds up
/// to 2 x max bytes outside `--dirty-budget`, so the limit bounds that at
/// about 2 MiB per streaming file. 64/256/1024 KiB is accepted; the same
/// shape at 128/512/2048 KiB is refused only for the limit, and the
/// message names it.
#[test]
fn the_max_is_at_most_1_mib() {
    assert_eq!(ChunkProfile::MAX_LIMIT, MIB);
    assert!(ChunkProfile::new(64 * KIB, 256 * KIB, 1024 * KIB).is_ok());
    for (a, b, c) in [(128 * KIB, 512 * KIB, 2048 * KIB), (MIB, 4 * MIB, 16 * MIB)] {
        let e = ChunkProfile::new(a, b, c).unwrap_err();
        assert!(e.contains("1024 KiB") && e.contains("1 MiB"), "{a}/{b}/{c}: {e}");
    }
}

#[test]
fn bad_profiles_are_refused() {
    for (a, b, c) in [
        (12288, 24576, 49152),
        (8192, 4096, 32768),
        (8192, 8192, 32768),
        (0, 8192, 32768),
        (32, 256, 1024),
        (2 * MIB, 4 * MIB, 8 * MIB),
        (4096, 8 * MIB, 16 * MIB),
        (4096, 8192, 32 * MIB),
    ] {
        assert!(ChunkProfile::new(a, b, c).is_err(), "{a}/{b}/{c} bytes");
    }
}

#[test]
fn zero_chunks_of_each_store_need_no_store_read() {
    let stores: Vec<(MemIo, ChunkStore)> = [ChunkProfile::DEFAULT, kib(16, 64, 256)]
        .into_iter()
        .map(|profile| {
            let mem = MemIo::new();
            let s = ChunkStore::open_with(Arc::new(mem.clone()), Options { profile, ..Options::default() }).unwrap();
            (mem, s)
        })
        .collect();
    let mut readers = Vec::new();
    for (_, s) in &stores {
        let mut w = FileWriter::new();
        w.write_zeros(s, 2 << 20).unwrap();
        let key = w.finish(s).unwrap();
        readers.push(FileReader::open(s, key).unwrap()); // reads the root node
    }
    for ((mem, s), r) in stores.iter().zip(&mut readers) {
        mem.set_fail_reads(true);
        let got = r.read(s, 0, 2 << 20).unwrap_or_else(|e| panic!("{}: {e}", s.profile()));
        assert_eq!(got.len(), 2 << 20);
        assert!(got.iter().all(|&b| b == 0));
    }
}

/// A splice source over the expected new bytes.
struct Bytes<'a>(&'a [u8]);

impl SpliceSource for Bytes<'_> {
    fn read(&mut self, offset: u64, len: usize) -> chunk_store::Result<Vec<u8>> {
        Ok(self.0[offset as usize..offset as usize + len].to_vec())
    }
}

fn write(s: &ChunkStore, data: &[u8]) -> ObjectId {
    let mut w = FileWriter::new();
    for piece in data.chunks(1 << 20) {
        w.write(s, piece).unwrap();
    }
    w.finish(s).unwrap()
}

fn read_all(s: &ChunkStore, key: ObjectId) -> Vec<u8> {
    let mut r = FileReader::open(s, key).unwrap();
    let n = r.len() as usize;
    r.read(s, 0, n).unwrap()
}

/// 5000 random bytes, 3 MiB of zeros, 5000 random bytes: the bytes and the
/// key of a write with `write_zeros`.
fn sparse(s: &ChunkStore) -> (Vec<u8>, ObjectId) {
    let (head, tail) = (xorshift(5000, 0x2267_0000_0000_0b01), xorshift(5000, 0x2267_0000_0000_0b02));
    let mut w = FileWriter::new();
    w.write(s, &head).unwrap();
    w.write_zeros(s, 3 << 20).unwrap();
    w.write(s, &tail).unwrap();
    let mut data = head;
    data.resize(data.len() + (3 << 20), 0);
    data.extend_from_slice(&tail);
    (data, w.finish(s).unwrap())
}

/// The FastCDC cut points of one fixed 1 MiB input for the five profiles.
/// The values come from fastcdc 3.2.1 itself (computed when the phase was
/// planned), not from this code: never change them to make the test pass.
#[test]
fn golden_cut_points() {
    let data = xorshift(1 << 20, 0x2267_c0de_5eed_0001);
    let ends = |p: ChunkProfile| -> Vec<u64> { p.ranges(&data).iter().map(|r| r.end as u64).collect() };
    let digest = |e: &[u64]| -> String {
        let bytes: Vec<u8> = e.iter().flat_map(|v| v.to_le_bytes()).collect();
        blake3::hash(&bytes).to_hex().to_string()
    };
    let default: [u64; 99] = [
        13800, 23883, 29116, 38014, 47396, 56701, 67070, 74807, 86207, 92433, 101949, 110204, 114636, 120873, 130051,
        140511, 151590, 160604, 176262, 195033, 215917, 222887, 228027, 237049, 247237, 254833, 273501, 289189, 301062,
        307926, 318715, 324536, 333843, 338598, 348455, 368962, 380405, 391496, 400350, 412329, 421976, 431697, 439221,
        467689, 476326, 481837, 491432, 501382, 513856, 521607, 529891, 535681, 545523, 556662, 567658, 577017, 590204,
        606690, 614372, 623926, 633324, 644098, 655991, 664638, 676652, 696267, 704832, 719563, 735572, 749757, 757099,
        772964, 787258, 796585, 802824, 811145, 824041, 832496, 843638, 857207, 864805, 873682, 887114, 893046, 903353,
        910078, 928338, 946975, 956469, 961380, 971124, 982144, 989293, 994887, 1009540, 1017963, 1027243, 1047702,
        1048576,
    ];
    let e = ends(ChunkProfile::DEFAULT);
    assert_eq!(e, default);
    assert_eq!(digest(&e), "6cbf2ccd701adb3ee0839d677ad893bb3d1f9eabd00cc78c07eabc2741176a3e");
    for ((a, b, c), count, want) in [
        ((8, 16, 64), 48, "cba855c8876e3e4aabbd21d76b076b2537a53042aab632c64651da53ea9032a6"),
        ((8, 32, 128), 26, "6752f03d541e92430162c88efe52282d5d1872b52b75781025606ce21fcde3cc"),
        ((16, 64, 256), 13, "115c523c58b22a54cb332e3aa417976a4c503106180a46eddf4d5216a0fa1cbd"),
        ((32, 128, 512), 6, "9bd1d30d11a1863bc1de0966933d0d9b2c1a7d674bc427e96edf2b7e2672187c"),
    ] {
        let e = ends(kib(a, b, c));
        assert_eq!(e.len(), count, "{a}/{b}/{c} KiB");
        assert_eq!(digest(&e), want, "{a}/{b}/{c} KiB");
    }
}

#[test]
fn two_profiles_in_one_process_read_back_byte_for_byte() {
    let dirs = [TempDir::new().unwrap(), TempDir::new().unwrap()];
    let stores: Vec<ChunkStore> = [ChunkProfile::DEFAULT, kib(16, 64, 256)]
        .into_iter()
        .zip(&dirs)
        .map(|(profile, d)| ChunkStore::open(d.path(), Options { profile, ..Options::default() }).unwrap())
        .collect();
    let a = xorshift(3 << 20, 0x2267_0000_0000_0a01);
    let mut keys = Vec::new();
    for s in &stores {
        let p = s.profile();
        // (a) random bytes.
        let key = write(s, &a);
        assert!(read_all(s, key) == a, "{p}: random read back");
        for r in p.ranges(&a) {
            assert!(s.contains(&s.hasher().id(&a[r.clone()])), "{p}: chunk {r:?}");
        }
        keys.push(key);
        // (b) a zero run.
        let (b, bkey) = sparse(s);
        assert!(read_all(s, bkey) == b, "{p}: zero run read back");
        // (c) a spliced edit: 4 KiB of new bytes, then zeros to 4 MiB.
        let at = (1u64 << 20) + 77;
        let mut c = a.clone();
        c[at as usize..at as usize + 4096].copy_from_slice(&xorshift(4096, 0x2267_0000_0000_0c01));
        c.resize(4 << 20, 0);
        let changed = at..at + 4096;
        let (ckey, _) = splice(s, Some(key), 3 << 20, std::slice::from_ref(&changed), 4 << 20, &mut Bytes(&c)).unwrap();
        assert_eq!(ckey, write(s, &c), "{p}: splice key");
        assert!(read_all(s, ckey) == c, "{p}: splice read back");
    }
    assert_ne!(keys[0], keys[1]);
}

#[test]
fn an_old_reader_reads_a_store_of_another_profile() {
    let d = TempDir::new().unwrap();
    let a = xorshift(3 << 20, 0x2267_0000_0000_0a01);
    let (key, b, bkey) = {
        let s = ChunkStore::open(d.path(), Options { profile: kib(16, 64, 256), ..Options::default() }).unwrap();
        let key = write(&s, &a);
        let (b, bkey) = sparse(&s);
        s.close().unwrap();
        (key, b, bkey)
    };
    // What a build with no profiles does: the default options.
    let s = ChunkStore::open(d.path(), Options::default()).unwrap();
    assert!(read_all(&s, key) == a);
    assert!(read_all(&s, bkey) == b);
    // Its write of the same bytes gives another key: it must not write.
    assert_ne!(write(&s, &a), key);
}
