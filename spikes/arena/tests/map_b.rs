//! Spike B correctness tests (plan section 5): the mmap read path on V1
//! (sparse, full fixed length) arenas.

use arena_spike::codec::{decode_record, rec_len, Decoded};
use arena_spike::map::{ArenaMap, MmapPath};
use arena_spike::{sys, Advice, Loc, ObjectId, Options, Prealloc, ReadMode, SpikeStore};
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::FileExt;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const MIB: u64 = 1 << 20;

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

fn mmap_opts(p: Prealloc, size: u64) -> Options {
    let mut o = Options::with_prealloc(p);
    o.arena.size = size;
    o.read = ReadMode::Mmap { advice: Advice::Default };
    o
}

fn mmap_path(s: &SpikeStore) -> &MmapPath {
    s.read_path().as_any().downcast_ref::<MmapPath>().expect("mmap read path")
}

/// Object `i` of the coherence test: 256..4351 bytes, random.
fn obj(i: usize) -> Vec<u8> {
    random(256 + (i * 7919) % 4096, i as u64 + 1)
}

/// A writer puts while readers `get()` each new id at once, through the
/// map of the active arena (made at its full length before the bytes were
/// written), across seals. 200k objects or 60 s.
#[test]
fn active_arena_reads_through_the_map_see_every_put_across_seals() {
    let dir = TempDir::new().unwrap();
    let s = Arc::new(SpikeStore::open(dir.path(), mmap_opts(Prealloc::Sparse, 4 * MIB)).unwrap());
    let max = std::env::var("MAP_B_OBJECTS").ok().and_then(|v| v.parse().ok()).unwrap_or(200_000usize);
    let ids: Arc<RwLock<Vec<ObjectId>>> = Arc::new(RwLock::new(Vec::with_capacity(max)));
    let done = Arc::new(AtomicBool::new(false));
    let checked = Arc::new(AtomicUsize::new(0));
    let bad = Arc::new(AtomicUsize::new(0));
    let mut readers = Vec::new();
    for r in 0..2 {
        let (s, ids, done, checked, bad) = (s.clone(), ids.clone(), done.clone(), checked.clone(), bad.clone());
        readers.push(std::thread::spawn(move || {
            let mut next = 0usize; // reader r checks every id, in order, as soon as it exists
            let mut x = r as u64 + 7;
            loop {
                let (n, id, old) = {
                    let v = ids.read().unwrap();
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    let old = (!v.is_empty()).then(|| (x as usize) % v.len()).map(|k| (k, v[k]));
                    (v.len(), v.get(next).copied(), old)
                };
                match id {
                    Some(id) => {
                        let ok = s.get(&id).is_some_and(|d| d == obj(next));
                        if !ok {
                            bad.fetch_add(1, Ordering::Relaxed);
                        }
                        checked.fetch_add(1, Ordering::Relaxed);
                        next += 1;
                    }
                    None if done.load(Ordering::Acquire) && next >= n => break,
                    None => std::hint::spin_loop(),
                }
                // And one older id (it may be in a sealed arena by now).
                if let Some((k, id)) = old {
                    if s.get(&id).is_none_or(|d| d != obj(k)) {
                        bad.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }));
    }
    let t0 = Instant::now();
    let mut n = 0;
    while n < max && t0.elapsed() < Duration::from_secs(60) {
        let id = s.put(&obj(n)).unwrap();
        ids.write().unwrap().push(id);
        n += 1;
    }
    done.store(true, Ordering::Release);
    for r in readers {
        r.join().unwrap();
    }
    let arenas = s.stats().packs;
    eprintln!(
        "coherence: {n} objects in {:.1} s, {arenas} arenas, {} reads checked, {} mapped, {} bad",
        t0.elapsed().as_secs_f64(),
        checked.load(Ordering::Relaxed),
        mmap_path(&s).mapped(),
        bad.load(Ordering::Relaxed)
    );
    assert_eq!(bad.load(Ordering::Relaxed), 0);
    assert_eq!(checked.load(Ordering::Relaxed), 2 * n);
    assert!(arenas >= 10, "the test must cross seals ({arenas} arenas)");
    assert!(mmap_path(&s).mapped() >= 10);
}

/// Hold the map of an arena, let GC delete the arena, read every record
/// through the held map, drop it: then the space comes back.
#[test]
fn an_arena_unlinked_while_mapped_stays_readable_and_its_space_comes_back_after_unmap() {
    let dir = TempDir::new().unwrap();
    let size = 32 * MIB;
    let s = SpikeStore::open(dir.path(), mmap_opts(Prealloc::Sparse, size)).unwrap();
    let mut objs = Vec::new();
    let mut i = 0u64;
    // Fill arena 1 (random data is stored as is) until arena 2 exists.
    while s.active().is_none_or(|(n, _)| n == 1) {
        let d = random(8000, 9000 + i);
        objs.push((s.put(&d).unwrap(), d));
        i += 1;
    }
    s.sync().unwrap();
    let in1: Vec<(ObjectId, Loc, Vec<u8>)> = objs
        .iter()
        .filter_map(|(id, d)| s.loc(id).filter(|l| l.arena == 1).map(|l| (*id, l, d.clone())))
        .collect();
    assert!(in1.len() > 1000);
    let path = s.packs_dir().join(format!("{:016x}.arena", 1));
    let alloc = sys::allocated(&path).unwrap();
    assert!(alloc > size / 2, "allocated {alloc}");
    let held = mmap_path(&s).map(1).expect("map of arena 1");
    // Nothing live: GC deletes arena 1 (arena 2 is active and kept).
    let t = s.gc_begin().unwrap();
    let st = s.gc_finish(&HashSet::new(), t).unwrap();
    assert!(st.packs_deleted >= 1);
    assert!(!path.exists(), "arena 1 is unlinked");
    for (id, l, d) in &in1 {
        let b = held.bytes(l.off, rec_len(l.stored)).unwrap();
        let Decoded::Ok(h) = decode_record(b, u32::MAX) else { panic!("record of {id:?} not readable") };
        assert_eq!(h.id, *id);
        assert_eq!(arena_spike::codec::payload(b, h.stored), &d[..]);
    }
    let before = sys::free_bytes(dir.path()).unwrap();
    drop(held);
    // APFS may give the blocks back a little later; other load moves f_bavail.
    let mut gain = 0i64;
    for _ in 0..40 {
        gain = sys::free_bytes(dir.path()).unwrap() as i64 - before as i64;
        if gain >= alloc as i64 / 2 {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    eprintln!("unlink while mapped: allocated {alloc}, free space gain after munmap {gain}");
    assert!(gain >= alloc as i64 / 2, "free space grew by {gain}, arena had {alloc} allocated");
}

/// A child process maps a file, the file is cut to half, and the child
/// touches a page past the new end: SIGBUS kills the child, not the parent.
#[test]
fn touching_a_mapped_page_past_a_cut_kills_the_child_with_sigbus() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("f");
    std::fs::write(&path, random(MIB as usize, 5)).unwrap();
    let mut c = Command::new(env!("CARGO_BIN_EXE_bench"))
        .arg("sigbus-child")
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(c.stdout.take().unwrap());
    let mut line = String::new();
    out.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "mapped");
    std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(MIB / 2).unwrap();
    writeln!(c.stdin.take().unwrap()).unwrap();
    let st = c.wait().unwrap();
    let mut rest = String::new();
    let _ = std::io::Read::read_to_string(&mut out, &mut rest);
    assert_eq!(st.signal(), Some(libc::SIGBUS), "child status {st}, output {rest:?}");
}

/// A Loc past the end of the arena (or with an overflowing offset, or of an
/// arena that does not exist) makes `get` return `None`, not a crash.
#[test]
fn a_corrupt_loc_past_the_end_of_the_arena_makes_get_return_none() {
    let dir = TempDir::new().unwrap();
    let size = MIB;
    let s = SpikeStore::open(dir.path(), mmap_opts(Prealloc::Sparse, size)).unwrap();
    let ids: Vec<_> = (0..20).map(|i| s.put(&random(5000, 70 + i)).unwrap()).collect();
    let good = s.loc(&ids[0]).unwrap();
    assert!(s.get(&ids[0]).is_some());
    for (k, bad) in [
        Loc { off: size - 10, ..good },
        Loc { off: size, ..good },
        Loc { off: u64::MAX - 5, ..good },
        Loc { stored: u32::MAX - 100, ..good },
        Loc { arena: 999, ..good },
        Loc { off: good.off + 1, ..good },
    ]
    .into_iter()
    .enumerate()
    {
        s.debug_set_loc(ids[k], bad);
        assert_eq!(s.get(&ids[k]), None, "case {k}: {bad:?}");
    }
    // The others still read.
    for id in &ids[6..] {
        assert!(s.get(id).is_some());
    }
}

/// A torn tail of the active arena: the cut runs in `open`, before the map
/// exists, so the reopened store (mmap) reads every synced object and
/// zeros after the cut.
#[test]
fn a_torn_tail_is_cut_before_the_map_and_synced_objects_read_through_the_map() {
    let dir = TempDir::new().unwrap();
    let o = mmap_opts(Prealloc::Sparse, 4 * MIB);
    let (objs, num, hwm) = {
        let s = SpikeStore::open(dir.path(), o).unwrap();
        let objs: Vec<_> = (0..200).map(|i| {
            let d = random(3000, 400 + i);
            (s.put(&d).unwrap(), d)
        }).collect();
        s.sync().unwrap();
        let (num, hwm) = s.active().unwrap();
        (objs, num, hwm)
    }; // dropped without close: no idx
    let path = dir.path().join("packs").join(format!("{num:016x}.arena"));
    // A torn record: a valid marker and header, then garbage.
    let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    let mut torn = arena_spike::codec::encode_record(&ObjectId::of(b"x"), 0, 4000, &random(4000, 1), 1);
    torn.truncate(2000);
    f.write_all_at(&torn, hwm).unwrap();
    drop(f);
    let s = SpikeStore::open(dir.path(), o).unwrap();
    for (id, d) in &objs {
        assert_eq!(s.get(id).as_ref(), Some(d));
    }
    let m = mmap_path(&s).map(num).expect("active arena is mapped");
    assert_eq!(m.len() as u64, 4 * MIB);
    assert!(m.bytes(hwm, 2000).unwrap().iter().all(|&b| b == 0), "tail after the cut reads as zeros");
    // Appends continue in the same arena and read through the same map.
    let d = random(3000, 999);
    let id = s.put(&d).unwrap();
    assert_eq!(s.loc(&id).unwrap().arena, num);
    assert_eq!(s.get(&id), Some(d));
}

/// V0 (Grow) arenas have no fixed length: with `ReadMode::Mmap` they are
/// read with pread, and nothing is mapped.
#[test]
fn grow_arenas_under_mmap_mode_use_pread() {
    let dir = TempDir::new().unwrap();
    let s = SpikeStore::open(dir.path(), mmap_opts(Prealloc::Grow, MIB)).unwrap();
    let objs: Vec<_> = (0..400).map(|i| {
        let d = random(5000, 800 + i);
        (s.put(&d).unwrap(), d)
    }).collect();
    for (id, d) in &objs {
        assert_eq!(s.get(id).as_ref(), Some(d));
    }
    assert_eq!(mmap_path(&s).mapped(), 0);
}

/// A map is refused when the file length is not the expected one.
#[test]
fn a_map_of_the_wrong_length_is_refused() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("f");
    std::fs::write(&p, vec![1u8; 8192]).unwrap();
    assert!(ArenaMap::open(&p, 4096, Advice::Default).is_err());
    let m = ArenaMap::open(&p, 8192, Advice::Random).unwrap();
    assert!(m.bytes(8190, 2).is_some());
    assert!(m.bytes(8190, 3).is_none());
    assert!(m.bytes(u64::MAX, 2).is_none());
}
