//! A write past the dirty limit pays for a bounded rebase step, not for the
//! rebase of the whole overlay (#108). The work of one write is measured as
//! the bytes it cut again with FastCDC and the base bytes it read, summed
//! over the whole write call; the bound does not depend on the dirty limit.

use std::sync::{Arc, Mutex};

use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use slicefs_cli::filesystem::{SliceFsFilesystem, REBASE_STEP};
use tempfile::TempDir;

const KIB: u64 = 1 << 10;
const MIB: u64 = 1 << 20;
/// Dirty limit of these tests. A whole rebase at this limit cuts again about
/// three times the dirty bytes when they are scattered 4 KiB writes.
const LIMIT: usize = 8 << 20;
/// Most work of one write: a step frees what the write added or
/// `REBASE_STEP`; scattered 4 KiB extents cut about 3x their bytes again.
const WORK_BOUND: u64 = 6 * REBASE_STEP as u64;

fn fresh_fs() -> (SliceFsFilesystem, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let io = Arc::new(Mutex::new(StoreIo::open(dir.path()).unwrap()));
    let meta = DictMetadataStore::new(io.clone());
    let fs = SliceFsFilesystem::new(meta, io, None);
    (fs, dir)
}

fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

fn seeded(seed: u64, n: usize) -> Vec<u8> {
    let mut x = seed;
    (0..n).map(|_| { x = splitmix(x); x as u8 }).collect()
}

/// A random file of `mib` MiB, streamed and closed.
fn random_file(fs: &SliceFsFilesystem, name: &str, mib: u64) -> (u64, Vec<u8>) {
    let (ino, fh) = fs.test_create(1, name, 0o644, 0, 0, 0).unwrap();
    let mut model = Vec::new();
    for i in 0..mib {
        let piece = seeded(1000 + i, MIB as usize);
        fs.test_write(fh, i * MIB, &piece).unwrap();
        model.extend_from_slice(&piece);
    }
    fs.test_release(ino, fh).unwrap();
    (ino, model)
}

/// Work of the writes so far: the largest per-write re-chunk and base read,
/// and the totals.
#[derive(Default, Debug)]
struct Work {
    max_rechunk: u64,
    max_base: u64,
    total_rechunk: u64,
    writes_with_steps: u64,
}

impl Work {
    fn add(&mut self, fs: &SliceFsFilesystem) {
        let (r, b) = (fs.test_last_write_rebase_bytes(), fs.test_last_write_base_bytes());
        self.max_rechunk = self.max_rechunk.max(r);
        self.max_base = self.max_base.max(b);
        self.total_rechunk += r;
        if r > 0 {
            self.writes_with_steps += 1;
        }
    }
}

fn check_content(fs: &SliceFsFilesystem, ino: u64, model: &[u8], what: &str) {
    let mut off = 0;
    while off < model.len() {
        let n = (4 * MIB as usize).min(model.len() - off);
        let got = fs.test_read(ino, off as u64, n as u32).unwrap();
        assert!(got == model[off..off + n], "{what}: content differs in [{off}, {})", off + n);
        off += n;
    }
}

/// Scattered 4 KiB writes into a 48 MiB file, twice the dirty limit: each
/// write past the limit cuts again at most `WORK_BOUND` bytes. A whole
/// rebase at this limit cuts about 24 MiB.
#[test]
fn scattered_writes_past_the_limit_each_pay_a_bounded_step() {
    let (fs, _dir) = fresh_fs();
    let (ino, mut model) = random_file(&fs, "f", 48);
    fs.test_set_dirty_limit(LIMIT);
    let (fh, _) = fs.test_open(ino, libc::O_RDWR).unwrap();
    let mut work = Work::default();
    let size = model.len() as u64;
    for i in 0..4096u64 {
        let off = splitmix(77 + i) % (size - 4 * KIB);
        let data = seeded(5000 + i, 4 * KIB as usize);
        fs.test_write(fh, off, &data).unwrap();
        model[off as usize..(off + 4 * KIB) as usize].copy_from_slice(&data);
        work.add(&fs);
        if i == 3000 {
            check_content(&fs, ino, &model, "mid-run");
        }
    }
    eprintln!("scattered: {work:?}");
    assert!(work.writes_with_steps >= 2, "the limit was passed: {work:?}");
    assert!(work.max_rechunk <= WORK_BOUND, "one write cut {} bytes again", work.max_rechunk);
    assert!(work.max_base <= WORK_BOUND, "one write read {} base bytes", work.max_base);
    fs.test_release(ino, fh).unwrap();
    check_content(&fs, ino, &model, "after release");
}

/// One extent that grows past the limit (in-order writes through an
/// overlay handle): a step takes a part of it, not all of it. Also at the
/// production limit: the spare capacity of a growing extent (1/8 of its
/// length) must not make the step grow with the limit.
#[test]
fn a_growing_extent_past_the_limit_is_rebased_in_parts() {
    growing_extent(LIMIT, 32, 4, 28);
    // At 32 MiB the extent runs past the end of a small file (no 48 MiB base
    // to stream in a debug build).
    growing_extent(slicefs_cli::filesystem::DIRTY_LIMIT, 2, 2, 38);
}

/// A `base_mib` file; a write at its middle switches to an overlay; then
/// in-order 128 KiB writes from `from_mib` to `to_mib` make one extent.
fn growing_extent(limit: usize, base_mib: u64, from_mib: u64, to_mib: u64) {
    let (fs, _dir) = fresh_fs();
    let (ino, mut model) = random_file(&fs, "f", base_mib);
    fs.test_set_dirty_limit(limit);
    let (fh, _) = fs.test_open(ino, libc::O_RDWR).unwrap();
    let mut work = Work::default();
    let at = base_mib * MIB / 2 + 7;
    let first = seeded(1, 4096);
    fs.test_write(fh, at, &first).unwrap();
    model[at as usize..(at + 4096) as usize].copy_from_slice(&first);
    assert!(!fs.test_is_streaming(fh));
    model.resize(model.len().max((to_mib * MIB) as usize), 0);
    let mut off = from_mib * MIB;
    let mut i = 0;
    while off < to_mib * MIB {
        let data = seeded(900 + i, 128 * KIB as usize);
        fs.test_write(fh, off, &data).unwrap();
        model[off as usize..(off + 128 * KIB) as usize].copy_from_slice(&data);
        work.add(&fs);
        off += 128 * KIB;
        i += 1;
    }
    eprintln!("growing extent, limit {} MiB: {work:?}", limit >> 20);
    assert!(work.writes_with_steps >= 2, "the limit was passed: {work:?}");
    // In-order bytes are cut again about once, so a step of the larger of one write and
    // `REBASE_STEP` is well below 2 * REBASE_STEP. If the step freed the
    // spare capacity of the extent too, it would be about limit / 8 more.
    assert!(work.max_rechunk <= 2 * REBASE_STEP as u64, "one write cut {} bytes again", work.max_rechunk);
    check_content(&fs, ino, &model, "with the handle open");
    fs.test_release(ino, fh).unwrap();
    check_content(&fs, ino, &model, "after release");
}

/// Out-of-order writes wait in the reorder window above the dirty limit; a
/// write that overlaps them switches to an overlay that is over the limit at
/// once. Each later write frees a bounded step, and the overlay comes down
/// below the limit.
#[test]
fn a_switch_above_the_limit_comes_down_in_bounded_steps() {
    let (fs, _dir) = fresh_fs();
    let (ino, fh) = fs.test_create(1, "f", 0o644, 0, 0, 0).unwrap();
    fs.test_set_dirty_limit(LIMIT);
    let mut model = vec![0u8; (13 * MIB) as usize];
    for i in 1..13u64 {
        let data = seeded(300 + i, MIB as usize);
        fs.test_write(fh, i * MIB, &data).unwrap();
        model[(i * MIB) as usize..((i + 1) * MIB) as usize].copy_from_slice(&data);
    }
    assert!(fs.test_is_streaming(fh), "the writes wait in the reorder window");
    let mut work = Work::default();
    let mut writes = 0;
    let mut i = 0u64;
    loop {
        let off = MIB + 10 + splitmix(i) % (11 * MIB);
        let data = seeded(7000 + i, 4 * KIB as usize);
        fs.test_write(fh, off, &data).unwrap();
        model[off as usize..(off + 4 * KIB) as usize].copy_from_slice(&data);
        work.add(&fs);
        writes += 1;
        i += 1;
        if fs.test_dirty_total() <= LIMIT {
            break;
        }
        assert!(writes < 64, "the overlay does not come down: {} bytes", fs.test_dirty_total());
    }
    eprintln!("switch: {writes} writes to come down; {work:?}");
    assert!(!fs.test_is_streaming(fh));
    assert!(writes >= 2, "more than one step: the first write must not pay for all");
    assert!(work.max_rechunk <= WORK_BOUND, "one write cut {} bytes again", work.max_rechunk);
    check_content(&fs, ino, &model, "with the handle open");
    fs.test_release(ino, fh).unwrap();
    check_content(&fs, ino, &model, "after release");
}

/// The budget of all open files (#105) with a new file A whose
/// out-of-order writes wait in the reorder window. Phase 19 (D-13): a
/// writer over the budget steps only its own file, so writes to another
/// file B never touch A (moving work to other files is Phase 20). Then A's
/// own in-order writes, still over the budget, turn its stream into an
/// overlay (its pending writes the extents) and store steps. Both files read
/// back right before and after release.
#[test]
fn the_budget_turns_a_stream_with_pending_writes_into_steps() {
    let (fs, _dir) = fresh_fs();
    // A: a new file, 3 MiB of writes ahead of the stream (pending).
    let (a, fa) = fs.test_create(1, "a", 0o644, 0, 0, 0).unwrap();
    let mut model_a = vec![0u8; (4 * MIB) as usize];
    for i in 1..4u64 {
        let data = seeded(40 + i, MIB as usize);
        fs.test_write(fa, i * MIB, &data).unwrap();
        model_a[(i * MIB) as usize..((i + 1) * MIB) as usize].copy_from_slice(&data);
    }
    assert!(fs.test_is_streaming(fa), "A waits in the reorder window");
    // B: an existing file, random 4 KiB writes. The budget is set now, so
    // a write to B is the first to pass it.
    let (b, mut model_b) = random_file(&fs, "b", 8);
    fs.test_set_dirty_budget(2 << 20);
    let (fb, _) = fs.test_open(b, libc::O_RDWR).unwrap();
    let mut work = Work::default();
    for i in 0..64u64 {
        let off = splitmix(300 + i) % (8 * MIB - 4 * KIB);
        let data = seeded(600 + i, 4 * KIB as usize);
        fs.test_write(fb, off, &data).unwrap();
        model_b[off as usize..(off + 4 * KIB) as usize].copy_from_slice(&data);
        work.add(&fs);
    }
    eprintln!("budget, writes to B: dirty total {} KiB; {work:?}", fs.test_dirty_total() >> 10);
    assert!(fs.test_is_streaming(fa), "a write to B does not step A");
    // A's 3 MiB pending, plus at most one write of B (B steps itself).
    assert!(fs.test_dirty_total() <= (3 * MIB + 8 * KIB) as usize, "dirty total {}", fs.test_dirty_total());
    assert!(work.max_rechunk <= WORK_BOUND, "one write cut {} bytes again", work.max_rechunk);
    check_content(&fs, a, &model_a, "A pending");
    // Fill A's gap [0, 1 MiB) in order: A's own writes pass the budget.
    let mut work = Work::default();
    for i in 0..8u64 {
        let data = seeded(80 + i, 128 * KIB as usize);
        fs.test_write(fa, i * 128 * KIB, &data).unwrap();
        model_a[(i * 128 * KIB) as usize..((i + 1) * 128 * KIB) as usize].copy_from_slice(&data);
        work.add(&fs);
    }
    eprintln!("budget, writes to A: dirty total {} KiB; {work:?}", fs.test_dirty_total() >> 10);
    assert!(!fs.test_is_streaming(fa), "the budget turned A into an overlay");
    assert!(fs.test_dirty_total() <= 2 << 20, "dirty total {}", fs.test_dirty_total());
    assert!(work.max_rechunk <= WORK_BOUND, "one write cut {} bytes again", work.max_rechunk);
    check_content(&fs, a, &model_a, "A with the gap filled");
    check_content(&fs, b, &model_b, "B");
    fs.test_release(a, fa).unwrap();
    fs.test_release(b, fb).unwrap();
    check_content(&fs, a, &model_a, "A after release");
    check_content(&fs, b, &model_b, "B after release");
}
