//! Crash tests of spike A (plan section 4, cases 1-6), per variant where the
//! case applies. A crash is simulated by dropping the store without `close`
//! and then changing the files as a torn write would, or by the fault hooks
//! in `sys` (thread-local, so tests in parallel do not see each other's).

use arena_spike::codec::{self, rec_len, Decoded, MAX_STORED, REC_CRC, REC_HEADER};
use arena_spike::sys::{self, Fault};
use arena_spike::{ObjectId, Options, Prealloc, SpikeStore};
use std::fs;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const MIB: u64 = 1 << 20;


/// `ARENA_TEST_READ=mmap` runs the same tests with the mmap read path
/// (spike B); default pread.
fn read_mode() -> arena_spike::ReadMode {
    match std::env::var("ARENA_TEST_READ").as_deref() {
        Ok("mmap") => arena_spike::ReadMode::Mmap { advice: arena_spike::Advice::Default },
        _ => arena_spike::ReadMode::Pread,
    }
}

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

fn opts(p: Prealloc) -> Options {
    let mut o = Options::with_prealloc(p);
    o.read = read_mode();
    o.arena.size = MIB;
    o
}

fn arena_file(dir: &Path, num: u64) -> PathBuf {
    dir.join("packs").join(format!("{num:016x}.arena"))
}

fn idx_file(dir: &Path, num: u64) -> PathBuf {
    dir.join("packs").join(format!("{num:016x}.idx"))
}

macro_rules! per_variant {
    ($($name:ident: [$($v:ident),*]),* $(,)?) => {
        $( mod $name { $( #[test] fn $v() { super::$name(super::variant(stringify!($v))) } )* } )*
    };
}

fn variant(v: &str) -> Prealloc {
    Prealloc::parse(&v.to_uppercase()).unwrap()
}

per_variant!(
    case2_a_torn_record_cuts_the_later_ones_and_the_tail_reads_as_zeros: [v0, v1, v2, v3],
    case3_a_garbage_header_without_an_idx_deletes_the_file: [v0, v1, v2, v3],
    case4_records_after_the_hwm_of_a_resumable_idx_are_found: [v0, v1, v2, v3],
    case5_a_crash_in_trim_on_seal_before_the_idx_is_accepted: [v1, v2, v3],
    case6_an_interrupted_recovery_cut_is_finished_by_the_next_open: [v1, v2],
    an_unsynced_tail_of_zeros_is_not_a_record: [v0, v1, v2, v3],
    a_read_only_open_changes_nothing: [v0, v1, v2, v3],
);

/// Case 1: a 45-byte all-zero tail (what a fixed arena holds after the last
/// record) fails decode. Record v2 rejects it at the marker, before any
/// length is read. Record v1 (no marker) reads stored = 0 from the zeros and
/// rejects it only because the CRC32C of 37 zero bytes is not 0.
#[test]
fn case1_an_all_zero_tail_fails_decode_in_v1_and_v2() {
    let z = [0u8; REC_HEADER + REC_CRC];
    assert_eq!(z.len(), 45);
    assert_eq!(codec::decode_record(&z, MAX_STORED), Decoded::Bad, "v2: bad marker");
    assert_eq!(codec::decode_record(&[0u8; 4096], MAX_STORED), Decoded::Bad, "v2: a zero page");
    assert_eq!(codec::decode_record_v1(&z), None, "v1: CRC of the zero header");
    assert_ne!(crc32c::crc32c(&[0u8; 37]), 0, "why v1 rejects it");
    // And a v2 record that is cut short reports Short, not Ok.
    let r = codec::encode_record(&ObjectId::of(b"x"), 0, 100, &[7u8; 100], 1);
    assert_eq!(codec::decode_record(&r[..r.len() - 1], MAX_STORED), Decoded::Short);
}

fn case2_a_torn_record_cuts_the_later_ones_and_the_tail_reads_as_zeros(p: Prealloc) {
    let dir = TempDir::new().unwrap();
    let (a, b, c) = (random(5000, 1), random(5000, 2), random(5000, 3));
    let (ia, ib, ic, end_a);
    {
        let s = SpikeStore::open(dir.path(), opts(p)).unwrap();
        ia = s.put(&a).unwrap();
        ib = s.put(&b).unwrap();
        ic = s.put(&c).unwrap();
        s.sync().unwrap();
        let la = s.loc(&ia).unwrap();
        end_a = la.off + rec_len(la.stored) as u64;
        // A torn write of b: its middle sector never reached the disk; c did.
        let lb = s.loc(&ib).unwrap();
        let f = fs::OpenOptions::new().write(true).open(arena_file(dir.path(), 1)).unwrap();
        f.write_all_at(&[0u8; 512], lb.off + 1024).unwrap();
        assert_eq!(s.loc(&ic).unwrap().off, lb.off + rec_len(lb.stored) as u64, "c follows b");
    }
    let s = SpikeStore::open(dir.path(), opts(p)).unwrap();
    assert_eq!(s.get(&ia).unwrap(), a);
    assert!(!s.contains(&ib), "torn record cut");
    assert!(!s.contains(&ic), "a valid record after a torn one is gone too");
    let bytes = fs::read(arena_file(dir.path(), 1)).unwrap();
    if p.fixed() {
        assert_eq!(bytes.len() as u64, MIB, "fixed length again");
    } else {
        assert_eq!(bytes.len() as u64, end_a, "V0: cut at the end of a");
    }
    assert!(bytes[end_a as usize..].iter().all(|&x| x == 0), "the tail reads as zeros after open");
    // Appends go after a, and a put of c stores it again.
    let ic2 = s.put(&c).unwrap();
    assert_eq!(s.loc(&ic2).unwrap().off, end_a);
    s.sync().unwrap();
    drop(s);
    let s = SpikeStore::open(dir.path(), opts(p)).unwrap();
    assert_eq!(s.get(&ic).unwrap(), c);
}

fn case3_a_garbage_header_without_an_idx_deletes_the_file(p: Prealloc) {
    let dir = TempDir::new().unwrap();
    let keep;
    {
        let mut o = opts(p);
        o.gc_seal = arena_spike::SealRule::Always;
        let s = SpikeStore::open(dir.path(), o).unwrap();
        keep = s.put(&random(5000, 10)).unwrap();
        s.seal_active().unwrap(); // arena 1: sealed, with idx
        s.put(&random(5000, 11)).unwrap(); // arena 2: active, no idx
        s.sync().unwrap();
    }
    assert!(arena_file(dir.path(), 2).exists() && !idx_file(dir.path(), 2).exists());
    let f = fs::OpenOptions::new().write(true).open(arena_file(dir.path(), 2)).unwrap();
    f.write_all_at(&random(4096, 12), 0).unwrap();
    drop(f);
    let s = SpikeStore::open(dir.path(), opts(p)).unwrap();
    assert!(!arena_file(dir.path(), 2).exists(), "deleted, not cut");
    assert_eq!(s.get(&keep).unwrap(), random(5000, 10));
    assert_eq!(s.stats().objects, 1);
    // A new arena gets a new number (not 2 again would also be fine; it
    // must simply work).
    let id = s.put(&random(5000, 13)).unwrap();
    s.sync().unwrap();
    drop(s);
    let s = SpikeStore::open(dir.path(), opts(p)).unwrap();
    assert!(s.get(&id).is_some());
}

fn case4_records_after_the_hwm_of_a_resumable_idx_are_found(p: Prealloc) {
    let dir = TempDir::new().unwrap();
    let first: Vec<_> = (0..5).map(|i| random(6000, 100 + i)).collect();
    let later: Vec<_> = (0..5).map(|i| random(6000, 200 + i)).collect();
    {
        let s = SpikeStore::open(dir.path(), opts(p)).unwrap();
        for d in &first {
            s.put(d).unwrap();
        }
        s.close().unwrap();
    }
    let idx = arena_spike::arena::load_idx(&dir.path().join("packs"), 1).unwrap();
    assert_eq!(idx.state, codec::IdxState::Open);
    {
        let s = SpikeStore::open(dir.path(), opts(p)).unwrap();
        for d in &later {
            s.put(d).unwrap();
        }
        s.sync().unwrap(); // crash: no close, the idx still has the old hwm
    }
    let idx2 = arena_spike::arena::load_idx(&dir.path().join("packs"), 1).unwrap();
    assert_eq!(idx2.hwm, idx.hwm, "the idx was not rewritten");
    let s = SpikeStore::open(dir.path(), opts(p)).unwrap();
    assert_eq!(s.stats().objects, 10);
    for d in first.iter().chain(&later) {
        assert_eq!(&s.get(&ObjectId::of(d)).unwrap(), d);
    }
}

fn case5_a_crash_in_trim_on_seal_before_the_idx_is_accepted(p: Prealloc) {
    let dir = TempDir::new().unwrap();
    let data: Vec<_> = (0..10).map(|i| random(6000, 300 + i)).collect();
    let mut o = opts(p);
    o.arena.trim_on_seal = true;
    o.read = arena_spike::ReadMode::Pread; // trim_on_seal is refused with mmap
    {
        let s = SpikeStore::open(dir.path(), o).unwrap();
        for d in &data {
            s.put(d).unwrap();
        }
        s.sync().unwrap();
        sys::arm(Fault::TrimBeforeIdx);
        assert!(s.seal_active().is_err(), "the injected fault stops the seal");
    }
    let len = fs::metadata(arena_file(dir.path(), 1)).unwrap().len();
    assert!(len < MIB, "the file was trimmed: {len}");
    assert!(!idx_file(dir.path(), 1).exists(), "no idx yet");
    let s = SpikeStore::open(dir.path(), o).unwrap();
    for d in &data {
        assert_eq!(&s.get(&ObjectId::of(d)).unwrap(), d);
    }
    let extra = random(6000, 399);
    let id = s.put(&extra).unwrap();
    s.sync().unwrap();
    drop(s);
    let s = SpikeStore::open(dir.path(), o).unwrap();
    assert_eq!(s.get(&id).unwrap(), extra);
    assert_eq!(s.stats().objects, 11);
}

fn case6_an_interrupted_recovery_cut_is_finished_by_the_next_open(p: Prealloc) {
    let dir = TempDir::new().unwrap();
    let (a, b) = (random(6000, 500), random(6000, 501));
    let (ia, ib);
    {
        let s = SpikeStore::open(dir.path(), opts(p)).unwrap();
        ia = s.put(&a).unwrap();
        ib = s.put(&b).unwrap();
        s.sync().unwrap();
        let lb = s.loc(&ib).unwrap();
        let f = fs::OpenOptions::new().write(true).open(arena_file(dir.path(), 1)).unwrap();
        f.write_all_at(&[0u8; 16], lb.off + rec_len(lb.stored) as u64 - 16).unwrap();
    }
    sys::arm(Fault::CutAfterTruncDown);
    assert!(SpikeStore::open(dir.path(), opts(p)).is_err(), "the first open stops after the ftruncate down");
    let len = fs::metadata(arena_file(dir.path(), 1)).unwrap().len();
    assert!(len < MIB, "half-done cut: the file is short ({len})");
    let s = SpikeStore::open(dir.path(), opts(p)).unwrap();
    assert_eq!(fs::metadata(arena_file(dir.path(), 1)).unwrap().len(), MIB, "fixed length again");
    assert_eq!(s.get(&ia).unwrap(), a);
    assert!(!s.contains(&ib));
    let ib2 = s.put(&b).unwrap();
    s.sync().unwrap();
    drop(s);
    let s = SpikeStore::open(dir.path(), opts(p)).unwrap();
    assert_eq!(s.get(&ib2).unwrap(), b);
}

/// Puts that were never synced: the file (for a fixed arena) has zeros where
/// they would be. They must not become records, and nothing before them is
/// lost.
fn an_unsynced_tail_of_zeros_is_not_a_record(p: Prealloc) {
    let dir = TempDir::new().unwrap();
    let a = random(7000, 600);
    let (ia, end);
    {
        let s = SpikeStore::open(dir.path(), opts(p)).unwrap();
        ia = s.put(&a).unwrap();
        s.sync().unwrap();
        let l = s.loc(&ia).unwrap();
        end = l.off + rec_len(l.stored) as u64;
        let b = s.put(&random(7000, 601)).unwrap();
        // "Lost" write: zero the unsynced record (V0: cut the file there).
        let f = fs::OpenOptions::new().write(true).open(arena_file(dir.path(), 1)).unwrap();
        let lb = s.loc(&b).unwrap();
        if p.fixed() {
            f.write_all_at(&vec![0u8; rec_len(lb.stored)], lb.off).unwrap();
        } else {
            f.set_len(end).unwrap();
        }
    }
    let s = SpikeStore::open(dir.path(), opts(p)).unwrap();
    assert_eq!(s.stats().objects, 1);
    assert_eq!(s.get(&ia).unwrap(), a);
    assert_eq!(s.active().unwrap().1, end);
}

fn a_read_only_open_changes_nothing(p: Prealloc) {
    let dir = TempDir::new().unwrap();
    let a = random(7000, 700);
    {
        let s = SpikeStore::open(dir.path(), opts(p)).unwrap();
        s.put(&a).unwrap();
        let b = s.put(&random(7000, 701)).unwrap();
        s.sync().unwrap();
        let lb = s.loc(&b).unwrap();
        let f = fs::OpenOptions::new().write(true).open(arena_file(dir.path(), 1)).unwrap();
        f.write_all_at(&[0u8; 8], lb.off + 100).unwrap();
    }
    let before = fs::read(arena_file(dir.path(), 1)).unwrap();
    let mut ro = opts(p);
    ro.read_only = true;
    let s = SpikeStore::open(dir.path(), ro).unwrap();
    assert_eq!(s.get(&ObjectId::of(&a)).unwrap(), a);
    assert_eq!(s.stats().objects, 1);
    drop(s);
    assert_eq!(fs::read(arena_file(dir.path(), 1)).unwrap(), before, "no cut, zero or trim");
    assert!(!idx_file(dir.path(), 1).exists());
}
