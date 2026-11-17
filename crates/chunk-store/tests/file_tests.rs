//! File content on the chunk store: streaming writer, manifest tree, reader.

use chunk_store::file::{file_objects, FileReader, FileWriter, LEAF_ENTRIES};
use chunk_store::{ChunkStore, Options};
use tempfile::TempDir;

fn random(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

fn store() -> (TempDir, ChunkStore) {
    let d = TempDir::new().unwrap();
    let s = ChunkStore::open(d.path(), Options::default()).unwrap();
    (d, s)
}

fn write_in_pieces(s: &ChunkStore, data: &[u8], piece: usize) -> chunk_store::ObjectId {
    let mut w = FileWriter::new();
    for p in data.chunks(piece) {
        w.write(s, p).unwrap();
    }
    assert_eq!(w.len(), data.len() as u64);
    w.finish(s).unwrap()
}

#[test]
fn the_key_does_not_depend_on_the_write_sizes() {
    let (_d, s) = store();
    let data = random(3 << 20, 1);
    let a = write_in_pieces(&s, &data, 4096);
    let b = write_in_pieces(&s, &data, 1_000_003);
    let c = write_in_pieces(&s, &data, 7);
    assert_eq!(a, b);
    assert_eq!(a, c);
}

#[test]
fn reads_return_the_written_bytes_at_any_offset() {
    let (_d, s) = store();
    let data = random(1 << 20, 2);
    let key = write_in_pieces(&s, &data, 65536);
    let mut r = FileReader::open(&s, key).unwrap();
    assert_eq!(r.len(), data.len() as u64);
    for &(off, n) in &[(0usize, 10usize), (8000, 20000), (1 << 19, 131072), ((1 << 20) - 5, 5), (12345, 1)] {
        assert_eq!(r.read(&s, off as u64, n).unwrap(), &data[off..off + n], "at {off}+{n}");
    }
    assert_eq!(r.read(&s, (1 << 20) - 3, 100).unwrap(), &data[(1 << 20) - 3..], "short at EOF");
    assert!(r.read(&s, 1 << 20, 10).unwrap().is_empty(), "nothing after EOF");
}

#[test]
fn a_large_file_uses_a_manifest_tree() {
    let (_d, s) = store();
    // About 1,500 chunks of 8 KiB: more than one leaf.
    let data = random(12 << 20, 3);
    let key = write_in_pieces(&s, &data, 1 << 20);
    let objs = file_objects(&s, key).unwrap();
    let nodes = objs.len() - chunk_store::chunk_ranges(&data).len();
    assert!(nodes >= 3, "a root and at least two leaves, got {nodes} nodes");
    assert!(LEAF_ENTRIES <= 1024);
    let mut r = FileReader::open(&s, key).unwrap();
    let mid = 6 << 20;
    assert_eq!(r.read(&s, mid as u64, 300_000).unwrap(), &data[mid..mid + 300_000]);
    assert_eq!(r.read(&s, 0, data.len()).unwrap(), data);
}

#[test]
fn zeros_are_stored_once() {
    let (_d, s) = store();
    let mut w = FileWriter::new();
    w.write(&s, b"head").unwrap();
    w.write_zeros(&s, 10 << 20).unwrap();
    w.write(&s, b"tail").unwrap();
    let key = w.finish(&s).unwrap();
    let mut r = FileReader::open(&s, key).unwrap();
    assert_eq!(r.len(), 4 + (10 << 20) + 4);
    assert_eq!(r.read(&s, 4, 8).unwrap(), vec![0u8; 8]);
    assert_eq!(r.read(&s, 4 + (10 << 20), 4).unwrap(), b"tail");
    assert!(s.stats().stored_bytes < 200_000, "10 MiB of zeros: {} stored bytes", s.stats().stored_bytes);
}

#[test]
fn the_same_content_twice_adds_nothing() {
    let (_d, s) = store();
    let data = random(500_000, 4);
    let a = write_in_pieces(&s, &data, 10_000);
    let before = s.stats();
    let b = write_in_pieces(&s, &data, 33_333);
    assert_eq!(a, b);
    assert_eq!(s.stats(), before);
}

#[test]
fn an_empty_file_has_a_key_and_reads_nothing() {
    let (_d, s) = store();
    let key = FileWriter::new().finish(&s).unwrap();
    let mut r = FileReader::open(&s, key).unwrap();
    assert_eq!(r.len(), 0);
    assert!(r.read(&s, 0, 10).unwrap().is_empty());
}

#[test]
fn a_clone_of_a_writer_finishes_the_content_so_far() {
    let (_d, s) = store();
    let data = random(300_000, 5);
    let mut w = FileWriter::new();
    w.write(&s, &data[..100_000]).unwrap();
    let snap = w.clone().finish(&s).unwrap();
    w.write(&s, &data[100_000..]).unwrap();
    let full = w.finish(&s).unwrap();
    let mut r = FileReader::open(&s, snap).unwrap();
    assert_eq!(r.read(&s, 0, 200_000).unwrap(), &data[..100_000]);
    let mut r = FileReader::open(&s, full).unwrap();
    assert_eq!(r.read(&s, 0, 300_000).unwrap(), data);
}

#[test]
fn a_missing_object_is_an_error_not_wrong_data() {
    let (_d, s) = store();
    let other = TempDir::new().unwrap();
    let s2 = ChunkStore::open(other.path(), Options::default()).unwrap();
    let key = write_in_pieces(&s2, &random(100_000, 6), 100_000);
    assert!(FileReader::open(&s, key).is_err());
}
