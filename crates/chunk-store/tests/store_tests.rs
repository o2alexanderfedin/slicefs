//! Behaviour of the chunk store (docs/design/chunk-store.md).

use chunk_store::{chunk_ranges, ChunkStore, ObjectId, Options};
use std::fs;
use tempfile::TempDir;

fn small_packs() -> Options {
    Options { pack_limit: 64 * 1024, ..Options::default() }
}

fn random(n: usize, seed: u64) -> Vec<u8> {
    // xorshift: deterministic, incompressible enough for zstd.
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

fn pack_files(dir: &std::path::Path, ext: &str) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir.join("packs"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(ext))
        .collect();
    v.sort();
    v
}

#[test]
fn put_then_get_returns_the_bytes() {
    let dir = TempDir::new().unwrap();
    let store = ChunkStore::open(dir.path(), Options::default()).unwrap();
    let data = b"hello chunk store".to_vec();
    let id = store.put(&data).unwrap();
    assert_eq!(id, ObjectId::of(&data));
    assert_eq!(store.get(&id).unwrap(), data);
    assert!(store.contains(&id));
    assert!(store.get(&ObjectId::of(b"other")).is_none());
}

#[test]
fn the_same_bytes_are_stored_once() {
    let dir = TempDir::new().unwrap();
    let store = ChunkStore::open(dir.path(), Options::default()).unwrap();
    let data = random(8192, 1);
    store.put(&data).unwrap();
    let before = store.stats();
    store.put(&data).unwrap();
    assert_eq!(store.stats(), before, "a second put of the same bytes writes nothing");
    assert_eq!(before.objects, 1);
}

#[test]
fn compressible_data_is_compressed_and_random_data_is_not() {
    let dir = TempDir::new().unwrap();
    let store = ChunkStore::open(dir.path(), Options::default()).unwrap();
    let text = b"#include <boost/config.hpp>\n".repeat(300);
    let noise = random(8192, 2);
    let t = store.put(&text).unwrap();
    let n = store.put(&noise).unwrap();
    let st = store.stats();
    assert!(st.stored_bytes < (text.len() + noise.len()) as u64);
    assert!(store.stored_len(&t).unwrap() < text.len() as u32 / 8);
    assert_eq!(store.stored_len(&n).unwrap(), noise.len() as u32, "no gain: kept raw");
    assert_eq!(store.get(&t).unwrap(), text);
    assert_eq!(store.get(&n).unwrap(), noise);
}

#[test]
fn objects_survive_a_reopen_after_sync() {
    let dir = TempDir::new().unwrap();
    let ids: Vec<(ObjectId, Vec<u8>)>;
    {
        let store = ChunkStore::open(dir.path(), small_packs()).unwrap();
        ids = (0..40).map(|i| { let d = random(4000, i + 10); (store.put(&d).unwrap(), d) }).collect();
        store.sync().unwrap();
    }
    assert!(pack_files(dir.path(), ".idx").len() >= 2, "64 KiB packs: several sealed");
    let store = ChunkStore::open(dir.path(), small_packs()).unwrap();
    for (id, d) in &ids {
        assert_eq!(&store.get(id).unwrap(), d);
    }
    assert_eq!(store.stats().objects, 40);
}

#[test]
fn a_sealed_pack_without_its_index_is_scanned_and_indexed() {
    let dir = TempDir::new().unwrap();
    let ids: Vec<ObjectId>;
    {
        let store = ChunkStore::open(dir.path(), small_packs()).unwrap();
        ids = (0..40).map(|i| store.put(&random(4000, i + 100)).unwrap()).collect();
        store.close().unwrap();
    }
    for idx in pack_files(dir.path(), ".idx") {
        fs::remove_file(dir.path().join("packs").join(idx)).unwrap();
    }
    let store = ChunkStore::open(dir.path(), small_packs()).unwrap();
    assert!(ids.iter().all(|id| store.contains(id)));
    drop(store);
    assert!(!pack_files(dir.path(), ".idx").is_empty(), "the index files are written again");
}

#[test]
fn a_corrupt_index_is_ignored_and_rebuilt() {
    let dir = TempDir::new().unwrap();
    let ids: Vec<ObjectId>;
    {
        let store = ChunkStore::open(dir.path(), small_packs()).unwrap();
        ids = (0..40).map(|i| store.put(&random(4000, i + 200)).unwrap()).collect();
        store.close().unwrap();
    }
    let idx = dir.path().join("packs").join(&pack_files(dir.path(), ".idx")[0]);
    let mut b = fs::read(&idx).unwrap();
    let mid = b.len() / 2;
    b[mid] ^= 0xff;
    fs::write(&idx, b).unwrap();
    let store = ChunkStore::open(dir.path(), small_packs()).unwrap();
    assert!(ids.iter().all(|id| store.contains(id)));
}

#[test]
fn a_torn_tail_of_the_active_pack_is_cut_and_the_rest_kept() {
    let dir = TempDir::new().unwrap();
    let (a, b) = (random(3000, 300), random(3000, 301));
    let (ia, ib);
    {
        let store = ChunkStore::open(dir.path(), Options::default()).unwrap();
        ia = store.put(&a).unwrap();
        store.sync().unwrap();
        ib = store.put(&b).unwrap();
        store.sync().unwrap();
    }
    // A crash in the middle of the last record: drop its last 10 bytes.
    let pack = dir.path().join("packs").join(&pack_files(dir.path(), ".pack")[0]);
    let len = fs::metadata(&pack).unwrap().len();
    fs::OpenOptions::new().write(true).open(&pack).unwrap().set_len(len - 10).unwrap();

    let store = ChunkStore::open(dir.path(), Options::default()).unwrap();
    assert_eq!(store.get(&ia).unwrap(), a);
    assert!(!store.contains(&ib), "the torn record is gone");
    // New writes go after the cut and read back.
    let ib2 = store.put(&b).unwrap();
    store.sync().unwrap();
    drop(store);
    let store = ChunkStore::open(dir.path(), Options::default()).unwrap();
    assert_eq!(store.get(&ib2).unwrap(), b);
    assert_eq!(store.get(&ia).unwrap(), a);
}

#[test]
fn a_flipped_byte_in_a_record_is_detected() {
    let dir = TempDir::new().unwrap();
    let d = random(3000, 400);
    let id;
    {
        let store = ChunkStore::open(dir.path(), Options::default()).unwrap();
        id = store.put(&d).unwrap();
        store.sync().unwrap();
    }
    let pack = dir.path().join("packs").join(&pack_files(dir.path(), ".pack")[0]);
    let mut b = fs::read(&pack).unwrap();
    let n = b.len();
    b[n - 100] ^= 1;
    fs::write(&pack, b).unwrap();
    let store = ChunkStore::open(dir.path(), Options::default()).unwrap();
    assert!(store.get(&id).is_none(), "a damaged record is not returned as data");
}

#[test]
fn chunks_cover_the_input_and_have_the_configured_sizes() {
    let data = random(1 << 20, 500);
    let r = chunk_ranges(&data);
    assert_eq!(r.first().unwrap().start, 0);
    assert_eq!(r.last().unwrap().end, data.len());
    assert!(r.windows(2).all(|w| w[0].end == w[1].start));
    assert!(r[..r.len() - 1].iter().all(|c| (4096..=32768).contains(&c.len())));
    let avg = data.len() / r.len();
    assert!((4096..=16384).contains(&avg), "average {avg} near 8 KiB");
}

#[test]
fn one_inserted_byte_changes_only_the_chunks_near_it() {
    let data = random(1 << 20, 600);
    let mut shifted = data.clone();
    shifted.insert(100, 0x42);
    let ids = |d: &[u8]| -> std::collections::HashSet<ObjectId> {
        chunk_ranges(d).into_iter().map(|r| ObjectId::of(&d[r])).collect()
    };
    let (a, b) = (ids(&data), ids(&shifted));
    let common = a.intersection(&b).count();
    assert!(common + 3 >= a.len(), "{common} of {} chunks kept", a.len());
}
