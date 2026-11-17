//! Spike E: the mmap write path reads back with `pread` and through a fresh
//! open (recovery walk), for each fixed preallocation; it writes the same
//! bytes as the `pwrite` paths; a torn tail after the last record is cut.

use arena_spike::arena::arena_path;
use arena_spike::codec::{self, HEADER_PAGE};
use arena_spike::mmap_write::{Mode, Writer};
use arena_spike::workload;
use arena_spike::{ArenaCfg, ObjectId, Prealloc};
use std::fs;
use std::os::unix::fs::FileExt;
use tempfile::TempDir;

const MIB: u64 = 1 << 20;

fn cfg(p: Prealloc, size: u64) -> ArenaCfg {
    ArenaCfg { size, prealloc: p, granule: 1, trim_on_seal: false }
}

fn objs(n: usize, seed: u64) -> Vec<Vec<u8>> {
    workload::mixed(n, seed)
}

fn put_all(w: &mut Writer, o: &[Vec<u8>]) -> Vec<ObjectId> {
    o.iter()
        .map(|d| {
            w.put(d).unwrap();
            ObjectId::of(d)
        })
        .collect()
}

fn round_trip(mode: Mode, p: Prealloc) {
    let d = TempDir::new().unwrap();
    let o = objs(200, 11);
    let mut w = Writer::create(d.path(), 1, cfg(p, 4 * MIB), mode, 3).unwrap();
    let ids = put_all(&mut w, &o);
    // Before sync: pread sees the bytes written through the map (one cache).
    for (id, data) in ids.iter().zip(&o) {
        assert_eq!(w.get(id).as_deref(), Some(data.as_slice()), "pread before sync");
    }
    w.sync().unwrap();
    // A repeated put is a dedup hit.
    assert!(w.put(&o[0]).unwrap().is_none());
    let arenas = w.arenas;
    drop(w);
    // Fresh open of the last arena: recovery walk.
    let mut found = 0;
    for num in 1..=arenas {
        let r = Writer::open(d.path(), num, cfg(p, 4 * MIB), Mode::PwBuf, 3).unwrap();
        found += r.index.len();
        for (id, data) in ids.iter().zip(&o) {
            if r.index.contains_key(id) {
                assert_eq!(r.get(id).as_deref(), Some(data.as_slice()), "after reopen");
            }
        }
    }
    assert_eq!(found, ids.len());
}

macro_rules! rt {
    ($($n:ident: $m:expr, $p:expr;)*) => { $( #[test] fn $n() { round_trip($m, $p) } )* };
}

rt! {
    mmap_sparse_round_trip: Mode::Mmap, Prealloc::Sparse;
    mmap_reserve_round_trip: Mode::Mmap, Prealloc::Reserve;
    mmap_zeroed_round_trip: Mode::Mmap, Prealloc::Zeroed;
    pwbuf_reserve_round_trip: Mode::PwBuf, Prealloc::Reserve;
    alloc_reserve_round_trip: Mode::Alloc, Prealloc::Reserve;
}

/// Small arenas: the writer rotates; every record still reads back.
#[test]
fn mmap_rotates_arenas() {
    let d = TempDir::new().unwrap();
    let o = objs(400, 5);
    let mut w = Writer::create(d.path(), 1, cfg(Prealloc::Reserve, MIB), Mode::Mmap, 3).unwrap();
    let ids = put_all(&mut w, &o);
    assert!(w.arenas > 1, "arenas {}", w.arenas);
    for (id, data) in ids.iter().zip(&o) {
        assert_eq!(w.get(id).as_deref(), Some(data.as_slice()));
    }
}

/// The three paths write the same arena bytes (same record v2, same zstd
/// output) when no arena rotates.
#[test]
fn the_three_paths_write_the_same_bytes() {
    let o = objs(300, 21);
    let mut files = Vec::new();
    for m in [Mode::Alloc, Mode::PwBuf, Mode::Mmap] {
        let d = TempDir::new().unwrap();
        let mut w = Writer::create(d.path(), 1, cfg(Prealloc::Reserve, 16 * MIB), m, 3).unwrap();
        put_all(&mut w, &o);
        assert_eq!(w.arenas, 1);
        w.sync().unwrap();
        drop(w);
        files.push(fs::read(arena_path(d.path(), 1)).unwrap());
    }
    assert!(files[0] == files[1], "alloc != pwbuf");
    assert!(files[0] == files[2], "alloc != mmap");
}

/// A torn record after the last good one (bytes written through the map,
/// no CRC) is cut at open; the bytes after the cut read as zeros.
#[test]
fn mmap_torn_tail_is_cut() {
    for p in [Prealloc::Sparse, Prealloc::Reserve, Prealloc::Zeroed] {
        let d = TempDir::new().unwrap();
        let o = objs(50, 3);
        let mut w = Writer::create(d.path(), 1, cfg(p, 4 * MIB), Mode::Mmap, 3).unwrap();
        let ids = put_all(&mut w, &o);
        w.sync().unwrap();
        let hwm = w.arena().hwm;
        // Half a record: marker and id, then garbage, no valid CRC.
        let mut torn = codec::encode_record(&ObjectId::of(b"t"), 0, 9000, &workload::random(9000, 1), 1);
        torn.truncate(5000);
        w.arena().file().write_all_at(&torn, hwm).unwrap();
        drop(w);
        let r = Writer::open(d.path(), 1, cfg(p, 4 * MIB), Mode::Mmap, 3).unwrap();
        assert_eq!(r.index.len(), ids.len(), "{p:?}");
        assert_eq!(r.arena().hwm, hwm, "{p:?}");
        let mut b = vec![1u8; 8192];
        r.arena().file().read_exact_at(&mut b, hwm).unwrap();
        assert!(b.iter().all(|&x| x == 0), "{p:?}: tail not zero after cut");
        assert_eq!(r.arena().len(), 4 * MIB);
        assert!(hwm > HEADER_PAGE);
    }
}

/// Resume after reopen: new records go after the old ones, through the map.
#[test]
fn mmap_resumes_after_reopen() {
    let d = TempDir::new().unwrap();
    let c = cfg(Prealloc::Reserve, 8 * MIB);
    let a = objs(100, 1);
    let b = objs(100, 2);
    let mut w = Writer::create(d.path(), 1, c, Mode::Mmap, 3).unwrap();
    put_all(&mut w, &a);
    w.sync().unwrap();
    drop(w);
    let mut w = Writer::open(d.path(), 1, c, Mode::Mmap, 3).unwrap();
    put_all(&mut w, &b);
    w.sync().unwrap();
    drop(w);
    let r = Writer::open(d.path(), 1, c, Mode::PwBuf, 3).unwrap();
    for x in a.iter().chain(&b) {
        assert_eq!(r.get(&ObjectId::of(x)).as_deref(), Some(x.as_slice()));
    }
}
