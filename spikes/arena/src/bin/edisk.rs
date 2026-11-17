//! Spike E: full-disk and error behaviour of mmap writes on APFS.
//!
//! `edisk run --scratch DIR [--cases a,b,..] [--out results/e/disk.jsonl]`
//!
//! For each case the parent makes a fresh 64 MiB APFS disk image (no root:
//! `hdiutil create` / `attach -nobrowse`), runs the case in a child process
//! (`edisk child --case X --dir MNT`), so a SIGBUS kills only the child, and
//! reads the exit status. Then it detaches the image, attaches it again and
//! compares the arena bytes with the pattern the child wrote ("ok" means the
//! bytes are equal after the remount, not only in the page cache). It always
//! detaches and deletes the image (`detach -force` as fallback).
//!
//! The child prints one line per step and flushes it, so the parent knows
//! the last step before a signal: `S <step> <detail>`.

use arena_spike::arena::Arena;
use arena_spike::bench::Args;
use arena_spike::measure::Rec;
use arena_spike::mmap_write::{page_size, Map};
use arena_spike::{sys, ArenaCfg, Prealloc};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MIB: u64 = 1 << 20;
/// Arena size on the 64 MiB volume.
const ARENA: u64 = 16 * MIB;
/// The hole of the b cases: [HOLE_LO, HOLE_HI), 16 KiB aligned.
const HOLE_LO: u64 = 4 * MIB;
const HOLE_HI: u64 = 8 * MIB;

const CASES: &[&str] = &[
    "a_reserve_mmap",
    "a_sparse_mmap",
    "a_zeroed_mmap",
    "a_reserve_pwrite",
    "a_sparse_pwrite",
    "b_hole_mmap",
    "b_hole_pwrite",
    "b_prealloc_peof_mmap",
    "b_prealloc_vol",
    "b_rezero_mmap",
    "c_clone_written_mmap",
    "c_clone_written_pwrite",
    "c_clone_fresh_mmap",
    "c_clone_fresh_pwrite",
    "d_detach_mmap",
    "d_detach_handler",
    "h_handler_eof",
];

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let a = Args::parse(&argv);
    match argv.first().map(String::as_str) {
        Some("run") => run(&a),
        Some("child") => {
            let case = a.str("case", "");
            let dir = PathBuf::from(a.str("dir", ""));
            if let Err(e) = child(&case, &dir) {
                say("error", &err(&e));
                std::process::exit(3);
            }
        }
        _ => {
            eprintln!("usage: edisk run --scratch DIR [--cases a,b] [--out F]");
            std::process::exit(2);
        }
    }
}

// ── Child ─────────────────────────────────────────────────────────────────────

fn say(step: &str, detail: &str) {
    let mut o = io::stdout().lock();
    let _ = writeln!(o, "S {step} {detail}");
    let _ = o.flush();
}

fn err(e: &io::Error) -> String {
    match e.raw_os_error() {
        Some(libc::ENOSPC) => "ENOSPC".into(),
        Some(libc::EIO) => "EIO".into(),
        Some(libc::EINVAL) => "EINVAL".into(),
        Some(libc::ENOTSUP) => "ENOTSUP".into(),
        Some(libc::ENXIO) => "ENXIO".into(),
        Some(libc::EBADF) => "EBADF".into(),
        Some(libc::ENOENT) => "ENOENT".into(),
        Some(n) => format!("errno{n}"),
        None => e.to_string().replace(' ', "_"),
    }
}

fn res<T>(r: &io::Result<T>) -> String {
    match r {
        Ok(_) => "ok".into(),
        Err(e) => err(e),
    }
}

/// The pattern byte word at offset `off` (8-byte aligned), per `gen`.
fn word(off: u64, gen: u64) -> u64 {
    let mut z = off.wrapping_add(gen.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn pattern(lo: u64, hi: u64, gen: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity((hi - lo) as usize);
    let mut o = lo;
    while o < hi {
        v.extend_from_slice(&word(o, gen).to_le_bytes());
        o += 8;
    }
    v
}

fn blocks(f: &File) -> u64 {
    f.metadata().map(|m| m.blocks() * 512).unwrap_or(0)
}

fn state(tag: &str, f: &File, dir: &Path) {
    say(tag, &format!("st_blocks_bytes={} f_bavail={}", blocks(f), sys::free_bytes(dir).unwrap_or(0)));
}

fn arena_cfg(p: Prealloc) -> ArenaCfg {
    ArenaCfg { size: ARENA, prealloc: p, granule: 1, trim_on_seal: false }
}

/// Write into `f` in 1 MiB, then 64 KiB, then 4 KiB steps until each step
/// fails; `F_FULLFSYNC`. Returns the bytes written and the last error.
fn fill_one(f: &mut File) -> (u64, String) {
    let block = vec![0x5Au8; MIB as usize];
    let mut wrote = 0u64;
    let mut last = String::from("none");
    for step in [MIB as usize, 64 << 10, 4096] {
        loop {
            match f.write(&block[..step]) {
                Ok(n) if n > 0 => wrote += n as u64,
                Ok(_) => break,
                Err(e) => {
                    last = err(&e);
                    break;
                }
            }
        }
    }
    if let Err(e) = sys::full_sync(f) {
        last = format!("{last}+sync_{}", err(&e));
    }
    (wrote, last)
}

/// Fill the volume: one large filler file until its writes fail (repeated
/// after `F_FULLFSYNC`, as APFS frees space at commit), then new 4 KiB
/// files until a new file cannot take 4 KiB three times in a row. The
/// fill is proven when that probe fails.
fn fill(dir: &Path) -> io::Result<()> {
    let mut f = OpenOptions::new().create(true).append(true).open(dir.join("fill"))?;
    let mut total = 0u64;
    let mut last = String::new();
    for _ in 0..20 {
        let (w, e) = fill_one(&mut f);
        total += w;
        last = e;
        if w == 0 {
            break;
        }
    }
    let (mut files, mut misses, mut small) = (0u64, 0u32, 0u64);
    while misses < 3 && files < 100_000 {
        let p = dir.join(format!("fill_{files}"));
        files += 1;
        let r = OpenOptions::new().create(true).write(true).open(&p).and_then(|mut g| {
            let (w, _) = fill_one(&mut g);
            Ok(w)
        });
        match r {
            Ok(w) if w > 0 => {
                small += w;
                misses = 0;
            }
            _ => misses += 1,
        }
    }
    say(
        "fill",
        &format!(
            "big_bytes={total} big_err={last} small_files={files} small_bytes={small} f_bavail={}",
            sys::free_bytes(dir).unwrap_or(0)
        ),
    );
    let probe = OpenOptions::new().create(true).write(true).open(dir.join("probe")).and_then(|mut p| {
        p.write_all(&[1u8; 4096])?;
        sys::full_sync(&p)
    });
    say("fill_probe", &res(&probe));
    Ok(())
}

/// Write `[lo, hi)` of the map with pattern `gen`, one page at a time, and
/// print the offset every MiB (so a SIGBUS shows where it hit).
fn mmap_write(m: &Map, lo: u64, hi: u64, gen: u64) {
    say("mmap_write_start", &format!("lo={lo} hi={hi}"));
    let pg = page_size();
    let p = pattern(lo, hi, gen);
    let mut o = lo;
    while o < hi {
        let n = (hi - o).min(pg - o % pg) as usize;
        let s = (o - lo) as usize;
        m.slice(o, n).copy_from_slice(&p[s..s + n]);
        o += n as u64;
        if o % MIB == 0 {
            say("mmap_progress", &format!("off={o}"));
        }
    }
    say("mmap_write", "returned");
}

fn pwrite(f: &File, lo: u64, hi: u64, gen: u64) -> io::Result<()> {
    let p = pattern(lo, hi, gen);
    let mut o = lo;
    for c in p.chunks(MIB as usize) {
        f.write_all_at(c, o)?;
        o += c.len() as u64;
    }
    Ok(())
}

/// msync + F_FULLFSYNC, each result printed.
fn mmap_sync(m: &Map, f: &File, lo: u64, hi: u64) {
    say("msync", &res(&m.msync(lo, hi)));
    fsync_probe(f, lo, hi);
}

/// F_FULLFSYNC; after an error: does a second one also fail (is the error
/// kept), and what does `pread` of the range show now (the page cache)?
fn fsync_probe(f: &File, lo: u64, hi: u64) {
    let r = sys::full_sync(f);
    say("fullsync", &res(&r));
    if r.is_err() {
        say("fullsync_again", &res(&sys::full_sync(f)));
        let mut b = vec![0u8; (hi - lo) as usize];
        let v = match f.read_exact_at(&mut b, lo) {
            Err(e) => err(&e),
            Ok(()) if b == pattern(lo, hi, 2) => "new_bytes".into(),
            Ok(()) if b.iter().all(|&x| x == 0) => "zeros".into(),
            Ok(()) => "other".into(),
        };
        say("pread_after_err", &v);
    }
}

fn punch(f: &File, lo: u64, hi: u64) -> io::Result<()> {
    let mut p = libc::fpunchhole_t { fp_flags: 0, reserved: 0, fp_offset: lo as libc::off_t, fp_length: (hi - lo) as libc::off_t };
    // SAFETY: valid fd and struct.
    if unsafe { libc::fcntl(f.as_raw_fd(), libc::F_PUNCHHOLE, &mut p) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn prealloc(f: &File, mode: libc::c_int, off: u64, len: u64) -> (io::Result<()>, i64) {
    let mut st = libc::fstore_t {
        fst_flags: libc::F_ALLOCATEALL,
        fst_posmode: mode,
        fst_offset: off as libc::off_t,
        fst_length: len as libc::off_t,
        fst_bytesalloc: 0,
    };
    // SAFETY: valid fd and struct.
    let r = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_PREALLOCATE, &mut st) };
    (if r == -1 { Err(io::Error::last_os_error()) } else { Ok(()) }, st.fst_bytesalloc)
}

fn clone(from: &Path, to: &Path) -> io::Result<()> {
    let a = CString::new(from.as_os_str().as_bytes())?;
    let b = CString::new(to.as_os_str().as_bytes())?;
    // SAFETY: NUL-terminated paths.
    if unsafe { libc::clonefile(a.as_ptr(), b.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A written arena (pattern gen 1 over the data area), synced.
fn written_arena(dir: &Path) -> io::Result<Arena> {
    let a = Arena::create(dir, 1, &arena_cfg(Prealloc::Reserve))?;
    pwrite(a.file(), 4096, ARENA, 1)?;
    sys::full_sync(a.file())?;
    Ok(a)
}

/// A written arena with a hole at [HOLE_LO, HOLE_HI), synced.
fn holed_arena(dir: &Path) -> io::Result<Arena> {
    let a = written_arena(dir)?;
    state("written", a.file(), dir);
    say("punch", &res(&punch(a.file(), HOLE_LO, HOLE_HI)));
    sys::full_sync(a.file())?;
    say("seek_hole", &format!("{:?}", sys::seek_hole(a.file(), 4096).ok()));
    state("punched", a.file(), dir);
    Ok(a)
}

fn child(case: &str, dir: &Path) -> io::Result<()> {
    say("start", &format!("case={case} page={}", page_size()));
    match case {
        "a_reserve_mmap" | "a_sparse_mmap" | "a_zeroed_mmap" => {
            let p = match case {
                "a_reserve_mmap" => Prealloc::Reserve,
                "a_sparse_mmap" => Prealloc::Sparse,
                _ => Prealloc::Zeroed,
            };
            let a = Arena::create(dir, 1, &arena_cfg(p))?;
            state("created", a.file(), dir);
            fill(dir)?;
            state("filled", a.file(), dir);
            let m = Map::new(a.file(), ARENA)?;
            mmap_write(&m, 4096, ARENA, 2);
            state("after_write", a.file(), dir);
            mmap_sync(&m, a.file(), 4096, ARENA);
            state("after_sync", a.file(), dir);
        }
        "a_reserve_pwrite" | "a_sparse_pwrite" => {
            let p = if case == "a_reserve_pwrite" { Prealloc::Reserve } else { Prealloc::Sparse };
            let a = Arena::create(dir, 1, &arena_cfg(p))?;
            state("created", a.file(), dir);
            fill(dir)?;
            say("pwrite", &res(&pwrite(a.file(), 4096, ARENA, 2)));
            fsync_probe(a.file(), 4096, ARENA);
            state("after_sync", a.file(), dir);
        }
        "b_hole_mmap" | "b_hole_pwrite" => {
            let a = holed_arena(dir)?;
            fill(dir)?;
            state("filled", a.file(), dir);
            if case == "b_hole_mmap" {
                let m = Map::new(a.file(), ARENA)?;
                mmap_write(&m, HOLE_LO, HOLE_HI, 2);
                mmap_sync(&m, a.file(), HOLE_LO, HOLE_HI);
            } else {
                say("pwrite", &res(&pwrite(a.file(), HOLE_LO, HOLE_HI, 2)));
                fsync_probe(a.file(), HOLE_LO, HOLE_HI);
            }
            state("after_sync", a.file(), dir);
        }
        "b_prealloc_peof_mmap" => {
            let a = holed_arena(dir)?;
            let (r, n) = prealloc(a.file(), libc::F_PEOFPOSMODE, 0, HOLE_HI - HOLE_LO);
            say("prealloc_peof", &format!("{} bytesalloc={n}", res(&r)));
            sys::full_sync(a.file())?;
            state("after_prealloc", a.file(), dir);
            say("seek_hole", &format!("{:?}", sys::seek_hole(a.file(), 4096).ok()));
            fill(dir)?;
            let m = Map::new(a.file(), ARENA)?;
            mmap_write(&m, HOLE_LO, HOLE_HI, 2);
            mmap_sync(&m, a.file(), HOLE_LO, HOLE_HI);
            state("after_sync", a.file(), dir);
        }
        "b_prealloc_vol" => {
            let a = holed_arena(dir)?;
            for (off, tag) in [(HOLE_LO, "vol_at_hole_off"), (0, "vol_at_0")] {
                let (r, n) = prealloc(a.file(), libc::F_VOLPOSMODE, off, HOLE_HI - HOLE_LO);
                say(tag, &format!("{} bytesalloc={n}", res(&r)));
            }
            sys::full_sync(a.file())?;
            state("after_prealloc", a.file(), dir);
            say("seek_hole", &format!("{:?}", sys::seek_hole(a.file(), 4096).ok()));
        }
        "b_rezero_mmap" => {
            let a = holed_arena(dir)?;
            arena_spike::arena::write_zeros(a.file(), HOLE_LO, HOLE_HI)?;
            sys::full_sync(a.file())?;
            state("rezeroed", a.file(), dir);
            say("seek_hole", &format!("{:?}", sys::seek_hole(a.file(), 4096).ok()));
            fill(dir)?;
            let m = Map::new(a.file(), ARENA)?;
            mmap_write(&m, HOLE_LO, HOLE_HI, 2);
            mmap_sync(&m, a.file(), HOLE_LO, HOLE_HI);
            state("after_sync", a.file(), dir);
        }
        "c_clone_written_mmap" | "c_clone_written_pwrite" => {
            let a = written_arena(dir)?;
            say("clone", &res(&clone(a.path(), &dir.join("clone.arena"))));
            state("cloned", a.file(), dir);
            fill(dir)?;
            if case == "c_clone_written_mmap" {
                let m = Map::new(a.file(), ARENA)?;
                mmap_write(&m, HOLE_LO, HOLE_HI, 2);
                mmap_sync(&m, a.file(), HOLE_LO, HOLE_HI);
            } else {
                say("pwrite", &res(&pwrite(a.file(), HOLE_LO, HOLE_HI, 2)));
                fsync_probe(a.file(), HOLE_LO, HOLE_HI);
            }
            state("after_sync", a.file(), dir);
        }
        "c_clone_fresh_mmap" | "c_clone_fresh_pwrite" => {
            let a = Arena::create(dir, 1, &arena_cfg(Prealloc::Reserve))?;
            say("clone", &res(&clone(a.path(), &dir.join("clone.arena"))));
            state("cloned", a.file(), dir);
            fill(dir)?;
            if case == "c_clone_fresh_mmap" {
                let m = Map::new(a.file(), ARENA)?;
                mmap_write(&m, 4096, ARENA, 2);
                mmap_sync(&m, a.file(), 4096, ARENA);
            } else {
                say("pwrite", &res(&pwrite(a.file(), 4096, ARENA, 2)));
                fsync_probe(a.file(), 4096, ARENA);
            }
            state("after_sync", a.file(), dir);
        }
        "d_detach_mmap" | "d_detach_handler" => {
            // The parent force-detaches the volume after "ready".
            let a = Arena::create(dir, 1, &arena_cfg(Prealloc::Reserve))?;
            let m = Map::new(a.file(), ARENA)?;
            mmap_write(&m, 4096, ARENA / 2, 2);
            mmap_sync(&m, a.file(), 4096, ARENA / 2);
            if case == "d_detach_handler" {
                install_handler();
            }
            say("ready", "");
            let mut b = [0u8; 1];
            let _ = io::stdin().read(&mut b);
            say("resumed", "");
            let mut sum = 0u64;
            for o in (4096..ARENA / 2).step_by(page_size() as usize) {
                sum = sum.wrapping_add(m.slice(o, 1)[0] as u64);
            }
            say("read_old_pages", &format!("returned sum={sum} faults_seen={}", FAULTED.load(std::sync::atomic::Ordering::SeqCst)));
            mmap_write(&m, ARENA / 2, ARENA, 2);
            say("write_new_pages", &format!("faults_seen={}", FAULTED.load(std::sync::atomic::Ordering::SeqCst)));
            mmap_sync(&m, a.file(), ARENA / 2, ARENA);
            say("pwrite", &res(&a.file().write_all_at(&[1u8; 4096], 4096)));
            let mut b = [0u8; 4096];
            say("pread", &res(&a.file().read_exact_at(&mut b, 8 * MIB)));
        }
        "h_handler_eof" => handler_eof(dir)?,
        _ => return Err(io::Error::other(format!("unknown case {case}"))),
    }
    say("done", "");
    Ok(())
}

// ── SIGBUS handler feasibility (case h) ───────────────────────────────────────

static FAULTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// On SIGBUS: map an anonymous page over the fault address, count it, and
/// return. The faulted store then lands in the anonymous page; the writer
/// checks the count after the copy. (Feasibility only: it replaces the std
/// handler, does not chain, and is not async-signal-reviewed.)
extern "C" fn on_bus(_sig: libc::c_int, info: *mut libc::siginfo_t, _ctx: *mut libc::c_void) {
    // SAFETY: the kernel passes a valid siginfo_t for SA_SIGINFO.
    let addr = unsafe { (*info).si_addr } as usize;
    let pg = page_size() as usize;
    let page = addr / pg * pg;
    // SAFETY: replaces one page of our own file mapping with anonymous memory.
    let r = unsafe {
        libc::mmap(page as *mut _, pg, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_ANON | libc::MAP_PRIVATE | libc::MAP_FIXED, -1, 0)
    };
    if r == libc::MAP_FAILED {
        // SAFETY: restore the default action; the retry then kills us.
        unsafe { libc::signal(libc::SIGBUS, libc::SIG_DFL) };
    }
    FAULTED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

fn install_handler() {
    // SAFETY: zeroed sigaction is valid; then a valid handler and flags.
    let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
    sa.sa_sigaction = on_bus as *const () as usize;
    sa.sa_flags = libc::SA_SIGINFO;
    let r = unsafe { libc::sigaction(libc::SIGBUS, &sa, std::ptr::null_mut()) };
    say("sigaction", if r == 0 { "ok" } else { "failed" });
}

/// A sure SIGBUS: a 2-page map of a 1-page file, store into page 2. Without
/// a handler: does it SIGBUS? With the handler: does the store return, and
/// does the writer see the fault?
fn handler_eof(dir: &Path) -> io::Result<()> {
    let pg = page_size();
    let f = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(dir.join("eof"))?;
    f.set_len(pg)?;
    let m = Map::new(&f, 2 * pg)?;
    m.slice(0, 8).copy_from_slice(&[7u8; 8]);
    say("page1_store", "returned");
    install_handler();
    m.slice(pg, 8).copy_from_slice(&[9u8; 8]);
    let n = FAULTED.load(std::sync::atomic::Ordering::SeqCst);
    say("page2_store", &format!("returned faults_seen={n}"));
    // The file is unchanged past EOF.
    say("file_len", &format!("{}", f.metadata()?.len()));
    // Without the handler: the same store kills the process (the parent sees
    // the signal).
    let m2 = Map::new(&f, 2 * pg)?;
    // SAFETY: restore the default action.
    unsafe { libc::signal(libc::SIGBUS, libc::SIG_DFL) };
    say("no_handler_store", "start");
    m2.slice(pg, 8).copy_from_slice(&[9u8; 8]);
    say("no_handler_store", "returned");
    Ok(())
}

// ── Parent ────────────────────────────────────────────────────────────────────

fn hdiutil(args: &[&str], secs: u64) -> (bool, String) {
    let mut c = match Command::new("hdiutil").args(args).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => return (false, e.to_string()),
    };
    let t = Instant::now();
    loop {
        match c.try_wait() {
            Ok(Some(st)) => {
                let mut s = String::new();
                if let Some(mut e) = c.stderr.take() {
                    let _ = e.read_to_string(&mut s);
                }
                return (st.success(), s.trim().replace('\n', " | "));
            }
            Ok(None) if t.elapsed() > Duration::from_secs(secs) => {
                let _ = c.kill();
                let _ = c.wait();
                return (false, "timeout".into());
            }
            _ => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn detach(mnt: &Path) -> String {
    let m = mnt.to_str().unwrap();
    let (ok, e) = hdiutil(&["detach", m], 60);
    if ok {
        return "ok".into();
    }
    let (ok2, e2) = hdiutil(&["detach", "-force", m], 60);
    format!("detach={e} force={}", if ok2 { "ok".into() } else { e2 })
}

fn attach(dmg: &Path, mnt: &Path) -> bool {
    fs::create_dir_all(mnt).ok();
    hdiutil(&["attach", "-nobrowse", "-mountpoint", mnt.to_str().unwrap(), dmg.to_str().unwrap()], 60).0
}

/// Compare the arena after a remount with the expected pattern; returns the
/// first bad offset (or "equal"), and the number of bad 16 KiB pages.
fn verify(path: &Path, lo: u64, hi: u64, gen: u64) -> String {
    let Ok(f) = File::open(path) else { return "no_file".into() };
    let want = pattern(lo, hi, gen);
    let mut got = vec![0u8; want.len()];
    if let Err(e) = f.read_exact_at(&mut got, lo) {
        return format!("read_{}", err(&e));
    }
    let pg = 16384usize;
    let mut bad = 0;
    let mut first = None;
    let mut zero_pages = 0;
    for (i, (w, g)) in want.chunks(pg).zip(got.chunks(pg)).enumerate() {
        if w != g {
            bad += 1;
            first.get_or_insert(lo + (i * pg) as u64);
            if g.iter().all(|&b| b == 0) {
                zero_pages += 1;
            }
        }
    }
    match first {
        None => "equal".into(),
        Some(o) => format!("differs first_off={o} bad_pages={bad} zero_pages={zero_pages} of {}", want.len().div_ceil(pg)),
    }
}

/// The range and pattern a case writes last, for the check after remount.
fn expect(case: &str) -> Option<(u64, u64, u64)> {
    Some(match case {
        "a_reserve_mmap" | "a_sparse_mmap" | "a_zeroed_mmap" | "a_reserve_pwrite" | "a_sparse_pwrite" | "c_clone_fresh_mmap" | "c_clone_fresh_pwrite" => {
            (4096, ARENA, 2)
        }
        "b_hole_mmap" | "b_hole_pwrite" | "b_prealloc_peof_mmap" | "b_rezero_mmap" | "c_clone_written_mmap"
        | "c_clone_written_pwrite" => (HOLE_LO, HOLE_HI, 2),
        _ => return None,
    })
}

fn run(a: &Args) {
    let scratch = PathBuf::from(a.str("scratch", "/tmp"));
    let out = PathBuf::from(a.str("out", "results/e/disk.jsonl"));
    let cases: Vec<String> = match a.0.get("cases") {
        Some(c) => c.split(',').map(String::from).collect(),
        None => CASES.iter().map(|s| s.to_string()).collect(),
    };
    fs::create_dir_all(&scratch).unwrap();
    if let Some(p) = out.parent() {
        fs::create_dir_all(p).unwrap();
    }
    let mut outf = OpenOptions::new().create(true).append(true).open(&out).unwrap();
    let exe = std::env::current_exe().unwrap();
    for case in &cases {
        let dmg = scratch.join("e.dmg");
        let mnt = scratch.join("mnt_e");
        let _ = fs::remove_file(&dmg);
        let (ok, e) = hdiutil(&["create", "-size", "64m", "-fs", "APFS", "-volname", "spikeE", dmg.to_str().unwrap()], 120);
        if !ok || !attach(&dmg, &mnt) {
            eprintln!("{case}: image setup failed: {e}");
            continue;
        }
        let mut c = Command::new(&exe)
            .args(["child", "--case", case, "--dir", mnt.to_str().unwrap()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = Vec::new();
        let mut force_detach = String::new();
        let stdout = c.stdout.take().unwrap();
        let mut rd = BufReader::new(stdout);
        let mut l = String::new();
        let t0 = Instant::now();
        while rd.read_line(&mut l).map(|n| n > 0).unwrap_or(false) {
            let line = l.trim().to_string();
            eprintln!("  {case}: {line}");
            if line.starts_with("S ready") {
                force_detach = detach(&mnt);
                eprintln!("  {case}: parent detach: {force_detach}");
                let _ = c.stdin.as_mut().unwrap().write_all(b"g");
            }
            lines.push(line);
            l.clear();
        }
        let st = c.wait().unwrap();
        let mut stderr = String::new();
        let _ = c.stderr.take().unwrap().read_to_string(&mut stderr);
        let last = lines.iter().rev().find(|x| !x.starts_with("S mmap_progress")).cloned().unwrap_or_default();
        let last_progress = lines.iter().rev().find(|x| x.starts_with("S mmap_progress")).cloned().unwrap_or_default();
        let sig = st.signal();
        let outcome = match sig {
            Some(libc::SIGBUS) => "SIGBUS".to_string(),
            Some(s) => format!("signal{s}"),
            None => format!("exit{}", st.code().unwrap_or(-1)),
        };
        // Detach (flushes what the kernel still holds), attach again, compare.
        let d1 = if force_detach.is_empty() { detach(&mnt) } else { "already".into() };
        let check = match expect(case) {
            Some((lo, hi, gen)) if attach(&dmg, &mnt) => {
                let v = verify(&mnt.join(format!("{:016x}.arena", 1)), lo, hi, gen);
                let d2 = detach(&mnt);
                format!("{v} (detach2={d2})")
            }
            Some(_) => "reattach_failed".into(),
            None => "n/a".into(),
        };
        let _ = fs::remove_file(&dmg);
        let get = |step: &str| {
            lines
                .iter()
                .find(|x| x.starts_with(&format!("S {step} ")) || **x == format!("S {step}"))
                .map(|x| x[step.len() + 2..].trim().to_string())
                .unwrap_or_default()
        };
        let mut r = Rec::new("edisk", case);
        r.s("outcome", &outcome)
            .s("last_step", &last)
            .s("last_progress", &last_progress)
            .s("msync", &get("msync"))
            .s("fullsync", &get("fullsync"))
            .s("pwrite", &get("pwrite"))
            .s("fill_probe", &get("fill_probe"))
            .s("detach", &d1)
            .s("after_remount", &check)
            .s("steps", &lines.join(" ; "))
            .s("stderr", stderr.trim())
            .n("secs", t0.elapsed().as_secs_f64());
        if !force_detach.is_empty() {
            r.s("force_detach", &force_detach);
        }
        writeln!(outf, "{}", r.to_json()).unwrap();
        println!("{case}: {outcome} | last: {last} | msync={} fullsync={} pwrite={} | remount: {check}", get("msync"), get("fullsync"), get("pwrite"));
    }
}
