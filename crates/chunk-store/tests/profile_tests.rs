//! Chunk profiles (Phase 22): the limits of a profile, the zero chunk of
//! each store, and stores with different profiles in one process.

use std::sync::Arc;

use chunk_store::file::{FileReader, FileWriter};
use chunk_store::mem::MemIo;
use chunk_store::{chunk_ranges, ChunkProfile, ChunkStore, Options};

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
    assert!(ChunkProfile::new(MIB, 4 * MIB, 16 * MIB).is_ok());
    assert_eq!(ChunkProfile::DEFAULT.to_string(), "4/8/32 KiB");
    assert_eq!(kib(16, 64, 256).to_string(), "16/64/256 KiB");
    assert_eq!(ChunkProfile::new(64, 256, 1024).unwrap().to_string(), "64/256/1024 B");
    let x = xorshift(300 << 10, 0x2267_c0de_5eed_0002);
    assert_eq!(chunk_ranges(&x), ChunkProfile::DEFAULT.ranges(&x));
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
