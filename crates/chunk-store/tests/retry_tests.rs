//! The retry decorator: a short sync error recovers, a lasting one stops the
//! store, and a plain retry (without a new write) is shown to lose data.

use chunk_store::mem::MemIo;
use chunk_store::pack_io::{Kind, PackIo};
use chunk_store::retry::RetryingIo;
use chunk_store::{ChunkStore, ObjectId, Options};
use std::sync::Arc;

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

fn put_many(s: &ChunkStore, n: u64, seed: u64) -> Vec<(ObjectId, Vec<u8>)> {
    (0..n).map(|i| { let d = random(8000, seed + i); (s.put(&d).unwrap(), d) }).collect()
}

fn retrying(mem: &MemIo) -> Arc<dyn PackIo> {
    Arc::new(RetryingIo::with_backoff(Arc::new(mem.clone()), vec![std::time::Duration::ZERO; 3]))
}

fn open(io: &Arc<dyn PackIo>) -> ChunkStore {
    ChunkStore::open_with(Arc::clone(io), Options::default()).unwrap()
}

#[test]
fn a_plain_retry_of_a_failed_sync_loses_the_data() {
    // The mock acts as Linux: the failed sync marks the pages clean.
    let mem = MemIo::new();
    let mut f = mem.create(1).unwrap();
    f.write_at(b"0123456789", 0).unwrap();
    mem.fail_next_syncs(1);
    assert!(f.sync().is_err());
    f.sync().unwrap(); // "success"
    drop(f);
    mem.sync_dir().unwrap();
    mem.crash();
    assert_eq!(mem.read(1, Kind::Pack).unwrap(), vec![0u8; 10], "the bytes were never saved");
}

#[test]
fn a_short_sync_error_is_repaired_and_the_data_is_durable() {
    let mem = MemIo::new();
    let io = retrying(&mem);
    let s = open(&io);
    let objs = put_many(&s, 20, 1);
    mem.fail_next_syncs(2);
    s.sync().unwrap();
    // The store goes on, with no remount.
    let more = put_many(&s, 20, 100);
    s.sync().unwrap();
    drop(s);
    mem.crash();
    let s = open(&io);
    for (id, d) in objs.iter().chain(&more) {
        assert_eq!(&s.get(id).unwrap(), d, "data lost after a repaired sync");
    }
}

#[test]
fn a_lasting_sync_error_still_stops_the_store() {
    let mem = MemIo::new();
    let io = retrying(&mem);
    let s = open(&io);
    put_many(&s, 5, 200);
    mem.set_fail_syncs(true);
    assert!(s.sync().is_err());
    mem.set_fail_syncs(false);
    assert!(s.sync().is_err(), "after the retries end, the store refuses syncs");
}

#[test]
fn a_short_error_of_an_index_write_is_retried() {
    let mem = MemIo::new();
    let io = retrying(&mem);
    let s = open(&io);
    let objs = put_many(&s, 20, 300);
    // close() syncs the pack, then writes its index durably: fail both once.
    mem.fail_next_syncs(2);
    s.close().unwrap();
    drop(s);
    mem.crash();
    let s = open(&io);
    for (id, d) in &objs {
        assert_eq!(&s.get(id).unwrap(), d);
    }
}

#[test]
fn a_cut_before_the_retry_is_kept() {
    // set_len then a failed sync: the retry must not bring back cut bytes.
    let mem = MemIo::new();
    let io = retrying(&mem);
    let mut f = io.create(1).unwrap();
    f.write_at(&[7u8; 100], 0).unwrap();
    f.set_len(40).unwrap();
    mem.fail_next_syncs(1);
    f.sync().unwrap();
    drop(f);
    io.sync_dir().unwrap();
    mem.crash();
    assert_eq!(mem.read(1, Kind::Pack).unwrap(), vec![7u8; 40]);
}
