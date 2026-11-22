//! The retry decorator: a short sync error recovers, a lasting one stops the
//! store, and a plain retry (without a new write) is shown to lose data.

use chunk_store::mem::MemIo;
use chunk_store::pack_io::{Kind, PackFile, PackIo};
use chunk_store::retry::{RetryingIo, KEEP_CAP, KEEP_LIMIT};
use chunk_store::{ChunkStore, ObjectId, Options};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

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

#[test]
fn the_copy_of_unsynced_writes_stays_below_the_keep_limit() {
    // #98: the RAM copy must not grow with the data. Past KEEP_LIMIT the
    // pack is synced at once, so those bytes survive a crash with no sync.
    let mem = MemIo::new();
    let io = retrying(&mem);
    let mut f = io.create(1).unwrap();
    mem.sync_dir().unwrap();
    let piece = random(1 << 20, 7);
    let n = KEEP_LIMIT / piece.len() + 1;
    for i in 0..n {
        f.write_at(&piece, (i * piece.len()) as u64).unwrap();
    }
    drop(f);
    mem.crash();
    let saved = mem.read(1, Kind::Pack).unwrap();
    assert!(saved.len() > KEEP_LIMIT, "only {} bytes were synced", saved.len());
    for i in 0..n {
        assert_eq!(&saved[i * piece.len()..(i + 1) * piece.len()], &piece[..], "piece {i}");
    }
}

// ── #108: the sync that bounds the RAM copy runs in the background ──────────

const MIB: usize = 1 << 20;
const LONG: Duration = Duration::from_secs(20);

/// Write `n` pieces of 1 MiB from offset 0 in a new thread; each finished
/// write sends its index, and the thread gives the file back.
fn writer(mut f: Box<dyn PackFile>, piece: Vec<u8>, n: usize) -> (mpsc::Receiver<usize>, thread::JoinHandle<Box<dyn PackFile>>) {
    let (tx, rx) = mpsc::channel();
    let h = thread::spawn(move || {
        for i in 0..n {
            f.write_at(&piece, (i * MIB) as u64).unwrap();
            let _ = tx.send(i);
        }
        f
    });
    (rx, h)
}

/// After a crash, pack 1 holds the `n` pieces.
fn assert_durable(mem: &MemIo, piece: &[u8], n: usize) {
    mem.sync_dir().unwrap();
    mem.crash();
    let saved = mem.read(1, Kind::Pack).unwrap();
    assert_eq!(saved.len(), n * MIB);
    for i in 0..n {
        assert_eq!(&saved[i * MIB..(i + 1) * MIB], piece, "piece {i}");
    }
}

#[test]
fn a_write_does_not_wait_for_a_slow_background_sync() {
    let mem = MemIo::new();
    let io = retrying(&mem);
    let f = io.create(1).unwrap();
    mem.hold_syncs(true); // the disk finishes no sync for now
    let piece = random(MIB, 11);
    // Past KEEP_LIMIT (a sync starts and hangs), but below the cap.
    let n = KEEP_LIMIT / MIB + 4;
    assert!(n * MIB < KEEP_CAP);
    let (rx, h) = writer(f, piece.clone(), n);
    for i in 0..n {
        assert_eq!(rx.recv_timeout(LONG), Ok(i), "write {i} waited for the held sync");
    }
    let mut f = h.join().unwrap();
    mem.hold_syncs(false);
    f.sync().unwrap();
    drop(f);
    assert_durable(&mem, &piece, n);
}

#[test]
fn a_write_waits_for_the_sync_at_the_cap() {
    let mem = MemIo::new();
    let io = retrying(&mem);
    let f = io.create(1).unwrap();
    mem.hold_syncs(true);
    let piece = random(MIB, 12);
    let n = KEEP_CAP / MIB + 4;
    let (rx, h) = writer(f, piece.clone(), n);
    let mut done = 0;
    while rx.recv_timeout(Duration::from_millis(300)).is_ok() {
        done += 1;
        assert!(done <= n);
    }
    assert!(done < n, "no write waited at the cap");
    assert!(done * MIB > KEEP_LIMIT && done * MIB <= KEEP_CAP + MIB, "stopped after {done} MiB");
    mem.hold_syncs(false);
    for i in done..n {
        assert_eq!(rx.recv_timeout(LONG), Ok(i));
    }
    let mut f = h.join().unwrap();
    f.sync().unwrap();
    drop(f);
    assert_durable(&mem, &piece, n);
}

#[test]
fn a_failed_background_sync_is_written_again_and_nothing_is_lost() {
    let mem = MemIo::new();
    let io = retrying(&mem);
    let mut f = io.create(1).unwrap();
    // The background sync fails and (as on Linux) marks the pages clean.
    mem.fail_next_syncs(1);
    let piece = random(MIB, 13);
    let n = 2 * KEEP_LIMIT / MIB + 3;
    for i in 0..n {
        f.write_at(&piece, (i * MIB) as u64).unwrap();
    }
    f.sync().unwrap();
    drop(f);
    assert_durable(&mem, &piece, n);
}

#[test]
fn sync_waits_for_the_background_sync_and_the_rest() {
    let mem = MemIo::new();
    let io = retrying(&mem);
    let f = io.create(1).unwrap();
    mem.hold_syncs(true);
    let piece = random(MIB, 14);
    let n = KEEP_LIMIT / MIB + 3;
    let (rx, h) = writer(f, piece.clone(), n);
    for i in 0..n {
        assert_eq!(rx.recv_timeout(LONG), Ok(i));
    }
    let mut f = h.join().unwrap();
    let (tx, done) = mpsc::channel();
    let s = thread::spawn(move || {
        let r = f.sync();
        let _ = tx.send(());
        (f, r)
    });
    assert!(done.recv_timeout(Duration::from_millis(300)).is_err(), "sync returned before the disk synced");
    mem.hold_syncs(false);
    let (f, r) = s.join().unwrap();
    r.unwrap();
    drop(f);
    assert_durable(&mem, &piece, n);
}

#[test]
fn writes_after_the_start_of_a_background_sync_are_kept_until_synced() {
    // The background sync covers only the writes before it started; the
    // later ones must stay in the copy, so a failed sync after it repairs them.
    let mem = MemIo::new();
    let io = retrying(&mem);
    let f = io.create(1).unwrap();
    mem.hold_syncs(true);
    let piece = random(MIB, 15);
    let n = KEEP_LIMIT / MIB + 4;
    let (rx, h) = writer(f, piece.clone(), n);
    for i in 0..n {
        assert_eq!(rx.recv_timeout(LONG), Ok(i));
    }
    let mut f = h.join().unwrap();
    // The held background sync works; the sync of the rest fails once.
    mem.fail_sync_after(1);
    mem.hold_syncs(false);
    f.sync().unwrap();
    drop(f);
    assert_durable(&mem, &piece, n);
}

#[test]
fn a_put_that_fills_a_pack_does_not_wait_for_its_seal() {
    let mem = MemIo::new();
    let io = retrying(&mem);
    let o = Options { pack_limit: 64 << 10, index_background_merge: false, ..Options::default() };
    let s = Arc::new(ChunkStore::open_with(Arc::clone(&io), o).unwrap());
    s.sync().unwrap();
    mem.hold_syncs(true);
    // Objects of 8000 bytes: the ninth fills the first pack and seals it.
    let (tx, rx) = mpsc::channel();
    let s2 = Arc::clone(&s);
    let h = thread::spawn(move || {
        let objs = put_many(&s2, 12, 400);
        let _ = tx.send(());
        objs
    });
    assert!(rx.recv_timeout(LONG).is_ok(), "a put waited for the seal");
    let objs = h.join().unwrap();
    assert_eq!(s.stats().packs, 2);
    let (tx, done) = mpsc::channel();
    let s3 = Arc::clone(&s);
    let t = thread::spawn(move || {
        let r = s3.sync();
        let _ = tx.send(());
        r
    });
    assert!(done.recv_timeout(Duration::from_millis(300)).is_err(), "sync did not wait for the seal");
    mem.hold_syncs(false);
    t.join().unwrap().unwrap();
    drop(s);
    mem.crash();
    let s = open(&io);
    for (id, d) in &objs {
        assert_eq!(&s.get(id).unwrap(), d);
    }
}

#[test]
fn puts_go_on_while_a_store_sync_waits_for_the_disk() {
    let mem = MemIo::new();
    let io = retrying(&mem);
    let s = Arc::new(open(&io));
    let objs = put_many(&s, 5, 500);
    mem.hold_syncs(true);
    let (tx, done) = mpsc::channel();
    let s2 = Arc::clone(&s);
    let t = thread::spawn(move || {
        let r = s2.sync();
        let _ = tx.send(());
        r
    });
    // The sync waits for the held disk; puts do not wait for it.
    let (ptx, put) = mpsc::channel();
    let s3 = Arc::clone(&s);
    let p = thread::spawn(move || {
        let more = put_many(&s3, 5, 600);
        let _ = ptx.send(());
        more
    });
    assert!(put.recv_timeout(LONG).is_ok(), "a put waited for the store sync");
    assert!(done.recv_timeout(Duration::from_millis(300)).is_err(), "sync returned before the disk synced");
    mem.hold_syncs(false);
    t.join().unwrap().unwrap();
    let more = p.join().unwrap();
    s.sync().unwrap();
    drop(s);
    mem.crash();
    let s = open(&io);
    for (id, d) in objs.iter().chain(&more) {
        assert_eq!(&s.get(id).unwrap(), d);
    }
}

#[test]
fn puts_go_on_while_a_store_sync_waits_for_a_seal() {
    let mem = MemIo::new();
    let io = retrying(&mem);
    let o = Options { pack_limit: 64 << 10, index_background_merge: false, ..Options::default() };
    let s = Arc::new(ChunkStore::open_with(Arc::clone(&io), o).unwrap());
    s.sync().unwrap();
    mem.hold_syncs(true);
    // Nine objects of 8000 bytes fill the first pack: its seal is held.
    let objs = put_many(&s, 9, 700);
    let (tx, done) = mpsc::channel();
    let s2 = Arc::clone(&s);
    let t = thread::spawn(move || {
        let r = s2.sync();
        let _ = tx.send(());
        r
    });
    let (ptx, put) = mpsc::channel();
    let s3 = Arc::clone(&s);
    let p = thread::spawn(move || {
        let more = put_many(&s3, 3, 800);
        let _ = ptx.send(());
        more
    });
    assert!(put.recv_timeout(LONG).is_ok(), "a put waited for the store sync");
    assert!(done.recv_timeout(Duration::from_millis(300)).is_err(), "sync returned before the seal");
    mem.hold_syncs(false);
    t.join().unwrap().unwrap();
    let more = p.join().unwrap();
    s.sync().unwrap();
    drop(s);
    mem.crash();
    let s = open(&io);
    for (id, d) in objs.iter().chain(&more) {
        assert_eq!(&s.get(id).unwrap(), d);
    }
}

// One failed sync (of the background sync of phase 1) must not let the sync
// with no store lock after it drop the copies before they are written again.
#[test]
fn a_single_sync_error_is_repaired_and_the_data_is_durable() {
    let mem = MemIo::new();
    let io = retrying(&mem);
    let s = open(&io);
    let objs = put_many(&s, 20, 900);
    mem.fail_next_syncs(1);
    s.sync().unwrap();
    drop(s);
    mem.crash();
    let s = open(&io);
    for (id, d) in &objs {
        assert_eq!(&s.get(id).unwrap(), d, "data lost after a repaired sync");
    }
}

// The sync handle reports an error only after its retries end; the store
// then fails, and refuses later puts and syncs.
#[test]
fn a_failed_handle_sync_fails_the_store() {
    let mem = MemIo::new();
    let io = retrying(&mem);
    let s = open(&io);
    put_many(&s, 5, 1000);
    mem.set_fail_syncs(true);
    assert!(s.sync().is_err());
    mem.set_fail_syncs(false);
    assert!(s.put(&random(8000, 1100)).is_err(), "a put after the failure is refused");
    assert!(s.sync().is_err(), "a sync after the failure is refused");
}
