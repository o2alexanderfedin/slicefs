//! In-process stress harness (Phase 18). N threads run mixed operations
//! against the `SliceFsFilesystem` core through the request handlers (no FUSE
//! mount). The data are checked against a model, and the report shows latency
//! histograms and the commit gate times.
//!
//! Three modes: `OneFile` (all threads on one shared file, each thread owns a
//! set of stripes), `ManyFiles` (each thread owns its names in its own
//! directory and in `shared`, and creates, renames, unlinks, writes, fsyncs
//! them) and `MultiHandle` (one shared file; each thread has a read-only, a
//! read-write and an `O_APPEND` handle on it). A commit thread runs
//! `timed_commit` beside the workers.
//!
//! With `serialize` on, a harness mutex lets one operation run at a time (N
//! threads). Lock order: the harness mutex first, then the commit gate (never
//! the harness mutex while the gate is held). The commit thread does not take
//! the harness mutex (as in the daemon). `serialize_off_target` runs every
//! mode with `serialize` off (Phase 19).
//!
//! File bytes are never held in RAM as a model: a block is a function of
//! (seed, file, block, version), and the model keeps only versions.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Barrier, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hdrhistogram::Histogram;
use metadata::wal::WalConfig;
use slicefs_cli::commit_timer::timed_commit;
use slicefs_cli::filesystem::{Admit, SliceFsFilesystem, DIRTY_BUDGET};
use slicefs_cli::flusher;
use slicefs_cli::handlers::{
    self as h, AttrResult, CreateResult, DataResult, EmptyResult, EntryResult, OpenResult,
    ReaddirResult, WriteResult,
};
use tempfile::TempDir;

const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;
const BLOCK: usize = 4096;
/// Blocks in a stripe: the unit of ownership. Only the owner thread writes it.
const STRIPE: usize = 16;
const ROOT: u64 = 1;
const FILE_ID: u64 = 1;
/// Many-files mode: a file has at most 16 blocks (64 KiB), a thread at most
/// 32 live files.
const MAX_BLOCKS: usize = 16;
const MAX_LIVE: usize = 32;
/// The commit thread commits this often.
const COMMIT_EVERY: Duration = Duration::from_millis(250);
/// Multi-handle mode: the size of one append record, and the most records
/// one thread appends (so the file and the dirty total stay small).
const RECORD: usize = 512;
const MAX_APPENDS: u64 = 256;

// ---- content: no file bytes in RAM -------------------------------------

fn splitmix(x: u64) -> u64 {
    let mut x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// The 4096 bytes of one block at one version. Version 0 is a never-written
/// block (zeros); every other version gives different bytes.
fn block_bytes(seed: u64, file_id: u64, block: u64, version: u32) -> Vec<u8> {
    if version == 0 {
        return vec![0; BLOCK];
    }
    let mut k = splitmix(seed);
    k = splitmix(k ^ file_id);
    k = splitmix(k ^ block);
    k = splitmix(k ^ u64::from(version));
    let mut out = Vec::with_capacity(BLOCK);
    for i in 0..(BLOCK / 8) as u64 {
        out.extend_from_slice(&splitmix(k.wrapping_add(i.wrapping_mul(0x9E37_79B9))).to_le_bytes());
    }
    out
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let o = splitmix(self.0);
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        o
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

// ---- configuration and model --------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    OneFile,
    ManyFiles,
    MultiHandle,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::OneFile => "one-file",
            Mode::ManyFiles => "many-files",
            Mode::MultiHandle => "multi-handle",
        }
    }
}

#[derive(Clone)]
struct Config {
    mode: Mode,
    threads: usize,
    /// Ops per thread in a counted run (use `usize::MAX` with `secs`).
    ops_per_thread: usize,
    /// A timed run: every worker stops at this deadline.
    secs: Option<u64>,
    file_bytes: u64,
    seed: u64,
    serialize: bool,
    wal: WalConfig,
    /// The watchdog aborts the process if the run takes longer than this.
    watchdog: Duration,
    /// The run ends in a crash: the commit thread makes no commit in the
    /// last `COMMIT_EVERY` before the deadline and none at the end, so the
    /// log holds the end of the run.
    crash: bool,
    /// Drop every k-th unlocked step (`test_drop_every`; 0: off).
    drop_every: usize,
    /// Dirty limit of each open file in KiB (`test_set_dirty_limit`; 0: unchanged).
    dirty_limit_kib: usize,
}

/// A number from the environment, with a range. Out of range panics with the
/// variable name; the value is only ever used as a number (never a path or a
/// command).
fn env_num(name: &str, default: u64, lo: u64, hi: u64) -> u64 {
    let Ok(s) = std::env::var(name) else { return default };
    let v: u64 = s.trim().parse().unwrap_or_else(|_| panic!("{name} must be a number in {lo}..={hi}"));
    assert!((lo..=hi).contains(&v), "{name} must be in {lo}..={hi}, got {v}");
    v
}

impl Config {
    /// The long run (by hand): `STRESS_THREADS` (1..=64, default 8),
    /// `STRESS_SECS` (1..=3600, default 60), `STRESS_FILE_MIB` (1..=4096,
    /// default 64), `STRESS_SEED`, `STRESS_SERIALIZE` (0 or 1, default 1),
    /// `STRESS_DROP_EVERY` (0..=1000, default 0: drop every k-th unlocked
    /// step) and `STRESS_DIRTY_LIMIT_KIB` (0..=1048576, default 0: the limit
    /// is unchanged). `STRESS_MODE` is read by `modes_from_env`.
    fn from_env(defaults: Config) -> Config {
        let secs = env_num("STRESS_SECS", 60, 1, 3600);
        let file_bytes = env_num("STRESS_FILE_MIB", 64, 1, 4096) << 20;
        // `run` also verifies every block of the file, one 4 KiB read at a time;
        // allow about 5000 verify reads per second on top of the timed run.
        let verify_secs = file_bytes / BLOCK as u64 / 5_000;
        Config {
            threads: env_num("STRESS_THREADS", 8, 1, 64) as usize,
            ops_per_thread: usize::MAX,
            secs: Some(secs),
            file_bytes,
            serialize: env_num("STRESS_SERIALIZE", 1, 0, 1) == 1,
            watchdog: Duration::from_secs(secs + 60 + verify_secs),
            drop_every: env_num("STRESS_DROP_EVERY", 0, 0, 1000) as usize,
            dirty_limit_kib: env_num("STRESS_DIRTY_LIMIT_KIB", 0, 0, 1 << 20) as usize,
            ..defaults
        }
    }
}

/// `STRESS_MODE`: `one-file`, `many-files`, `multi-handle` or `both`
/// (default: one-file and many-files).
fn modes_from_env() -> Vec<Mode> {
    match std::env::var("STRESS_MODE").as_deref() {
        Err(_) | Ok("both") => vec![Mode::OneFile, Mode::ManyFiles],
        Ok("one-file") => vec![Mode::OneFile],
        Ok("many-files") => vec![Mode::ManyFiles],
        Ok("multi-handle") => vec![Mode::MultiHandle],
        Ok(_) => panic!("STRESS_MODE must be one-file, many-files, multi-handle or both"),
    }
}

/// No unbounded wait: if the run does not end in `limit`, print the seed and
/// abort the process. Dropping the sender (the end of a run) stops the thread,
/// so it never fires in a later test of the same binary.
fn watchdog(limit: Duration, seed: u64) -> mpsc::Sender<()> {
    let (tx, rx) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        if let Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(limit) {
            eprintln!("STRESS watchdog: no finish after {} s, seed={seed}", limit.as_secs());
            std::process::abort();
        }
    });
    tx
}

/// Stress builds (`--features deadlock-detect`): one process-wide thread asks
/// parking_lot for a lock cycle every second. On a cycle it prints each thread
/// with its backtrace and aborts. It is a detached thread: it ends with the
/// process, and nothing waits for it.
#[cfg(feature = "deadlock-detect")]
fn start_deadlock_watchdog() {
    static START: std::sync::Once = std::sync::Once::new();
    START.call_once(|| {
        std::thread::spawn(|| loop {
            std::thread::sleep(Duration::from_secs(1));
            let cycles = parking_lot::deadlock::check_deadlock();
            if cycles.is_empty() {
                continue;
            }
            eprintln!("STRESS deadlock detected: {} cycle(s)", cycles.len());
            for (i, threads) in cycles.iter().enumerate() {
                for t in threads {
                    eprintln!("cycle {i} thread {:?}\n{:?}", t.thread_id(), t.backtrace());
                }
            }
            std::process::abort();
        });
    });
}

#[cfg(not(feature = "deadlock-detect"))]
fn start_deadlock_watchdog() {}

fn seed_from_env() -> u64 {
    match std::env::var("STRESS_SEED") {
        Ok(s) => s.trim().parse().expect("STRESS_SEED must be a number"),
        Err(_) => SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64,
    }
}

/// One-file mode. Per 4 KiB block, two versions: `committed` (the last write
/// that returned Ok) and `pending` (the last write that was started).
struct FileModel {
    committed: Vec<AtomicU32>,
    pending: Vec<AtomicU32>,
}

impl FileModel {
    fn new(blocks: usize) -> Self {
        Self {
            committed: (0..blocks).map(|_| AtomicU32::new(0)).collect(),
            pending: (0..blocks).map(|_| AtomicU32::new(0)).collect(),
        }
    }
}

/// One-file and multi-handle modes. `appends[t]`: the records thread `t`
/// has appended (multi-handle mode; each append that returned Ok).
struct OneFile {
    model: FileModel,
    ino: u64,
    appends: Vec<AtomicU64>,
}

/// Multi-handle mode: record `k` of thread `t`, 512 bytes. The first 16
/// bytes name it; the rest is a function of (seed, t, k).
fn append_record(seed: u64, t: u64, k: u64) -> Vec<u8> {
    let mut r = Vec::with_capacity(RECORD);
    r.extend_from_slice(&t.to_le_bytes());
    r.extend_from_slice(&k.to_le_bytes());
    let key = splitmix(splitmix(seed ^ 0x5eed) ^ (t << 32) ^ k);
    for i in 0..((RECORD - 16) / 8) as u64 {
        r.extend_from_slice(&splitmix(key.wrapping_add(i)).to_le_bytes());
    }
    r
}

/// Many-files mode: the directories of the run.
struct Dirs {
    shared: u64,
    per_thread: Vec<u64>,
}

/// Many-files mode: one live file of one thread. Versions only.
struct Ent {
    ino: u64,
    ver: [u32; MAX_BLOCKS],
    /// File length in blocks (the highest written block + 1).
    len: usize,
    fh: Option<u64>,
}

/// Many-files mode: the model of one thread, keyed by (dir ino, name).
struct ThreadModel {
    files: BTreeMap<(u64, String), Ent>,
}

#[derive(Debug)]
struct Mismatch {
    op: &'static str,
    block: Option<u64>,
    detail: String,
}

fn errno_mismatch(op: &'static str, errno: i32) -> Mismatch {
    Mismatch { op, block: None, detail: format!("errno {errno}") }
}

fn mm(op: &'static str, detail: String) -> Mismatch {
    Mismatch { op, block: None, detail }
}

// ---- histograms ----------------------------------------------------------

fn hist() -> Histogram<u64> {
    Histogram::<u64>::new_with_bounds(1, 60_000_000_000, 3).unwrap()
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Read,
    Write,
    Fsync,
    Create,
    Rename,
    Unlink,
    Open,
    Release,
    Lookup,
    Append,
}

const KIND_NAMES: [&str; 10] =
    ["read", "write", "fsync", "create", "rename", "unlink", "open", "release", "lookup", "append"];

struct Hists {
    ops: Vec<Histogram<u64>>,
    gate_wait: Histogram<u64>,
    gate_hold: Histogram<u64>,
}

impl Hists {
    fn new() -> Self {
        Self { ops: KIND_NAMES.iter().map(|_| hist()).collect(), gate_wait: hist(), gate_hold: hist() }
    }
    fn add(&mut self, o: &Hists) {
        for (a, b) in self.ops.iter_mut().zip(&o.ops) {
            a.add(b).unwrap();
        }
        self.gate_wait.add(&o.gate_wait).unwrap();
        self.gate_hold.add(&o.gate_hold).unwrap();
    }
}

/// The commit thread: times of `timed_commit`.
struct CommitHists {
    gate_wait: Histogram<u64>,
    hold: Histogram<u64>,
    total: Histogram<u64>,
}

impl CommitHists {
    fn new() -> Self {
        Self { gate_wait: hist(), hold: hist(), total: hist() }
    }
}

fn ns(d: Duration) -> u64 {
    d.as_nanos() as u64
}

// ---- file system setup -----------------------------------------------------

fn new_fs(store: &Path, config: WalConfig) -> SliceFsFilesystem {
    common::new_fs_with(store, config)
}

// ---- the run ---------------------------------------------------------------

/// Everything a run needs and leaves behind: the file system (kept alive so a
/// test can look at it after the run), the temp dir, and the models.
struct Ctx {
    fs: SliceFsFilesystem,
    _dir: TempDir,
    gate: Arc<RwLock<()>>,
    serial: Mutex<()>,
    cfg: Config,
    one: Option<OneFile>,
    dirs: Option<Dirs>,
    /// Handler calls running now, and the most seen at once (inside the gate).
    inflight: AtomicUsize,
    max_inflight: AtomicUsize,
    /// A timed run ends at this instant; `halt` ends it early (too many mismatches).
    deadline: Option<Instant>,
    halt: AtomicBool,
    ops_done: AtomicUsize,
}

/// What `run` returns: the context, the many-files models, the mismatches and
/// the report. The tests assert on it (and a self-test plants a bug in it).
struct Outcome {
    ctx: Ctx,
    models: Vec<ThreadModel>,
    mismatches: Vec<Mismatch>,
    report: Report,
}

/// What a worker returns.
struct WorkerOut {
    mismatches: Vec<Mismatch>,
    model: Option<ThreadModel>,
}

fn mkdir(fs: &SliceFsFilesystem, parent: u64, name: &str, seed: u64) -> u64 {
    match h::handle_mkdir(fs, parent, name, S_IFDIR | 0o755, 0o022, 501, 20) {
        EntryResult::Ok(ino, _) => ino,
        EntryResult::Error(e) => panic!("seed={seed} mkdir {name} errno {e}"),
    }
}

/// The op kinds of the many-files mix, with weights in percent.
#[derive(Clone, Copy, PartialEq)]
enum Op {
    Create,
    Write,
    Read,
    Fsync,
    Rename,
    Unlink,
    Release,
    Open,
}

const MIX: [(Op, usize); 8] = [
    (Op::Create, 15),
    (Op::Write, 25),
    (Op::Read, 20),
    (Op::Fsync, 5),
    (Op::Rename, 10),
    (Op::Unlink, 10),
    (Op::Release, 10),
    (Op::Open, 5),
];

fn pick_op(rng: &mut Rng) -> Op {
    let mut r = rng.below(100);
    for (op, w) in MIX {
        if r < w {
            return op;
        }
        r -= w;
    }
    Op::Create
}

impl Ctx {
    /// Checked by a worker before each op: false when the run is over.
    fn keep_going(&self, found: usize) -> bool {
        if found >= 50 {
            self.halt.store(true, Ordering::SeqCst);
        }
        if self.halt.load(Ordering::SeqCst) || self.deadline.is_some_and(|d| Instant::now() >= d) {
            return false;
        }
        self.ops_done.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// One operation: the harness mutex (serialize mode), then the commit gate
    /// through `gate_shared` (as `fuse_callbacks.rs` does), then the handler. The latency is the
    /// handler call only; the gate wait and hold are timed apart.
    fn gated<R>(&self, hs: &mut Hists, kind: Kind, f: impl FnOnce() -> R) -> R {
        let _s = self.cfg.serialize.then(|| self.serial.lock().unwrap());
        let w = Instant::now();
        let g = self.fs.gate_shared();
        let acquired = Instant::now();
        hs.gate_wait.saturating_record(ns(acquired - w));
        let n = self.inflight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_inflight.fetch_max(n, Ordering::SeqCst);
        let r = f();
        self.inflight.fetch_sub(1, Ordering::SeqCst);
        let op_ns = ns(acquired.elapsed());
        drop(g);
        let hold = ns(acquired.elapsed());
        hs.ops[kind as usize].saturating_record(op_ns);
        hs.gate_hold.saturating_record(hold);
        r
    }

    // ---- one-file mode ----

    fn read_blocks(&self, hs: &mut Hists, fh_ino: u64, first: usize, n: usize, out: &mut Vec<Mismatch>) {
        let one = self.one.as_ref().expect("one-file context");
        let c0: Vec<u32> = (first..first + n).map(|b| one.model.committed[b].load(Ordering::SeqCst)).collect();
        let r = self.gated(hs, Kind::Read, || {
            h::handle_read(&self.fs, fh_ino, (first * BLOCK) as u64, (n * BLOCK) as u32)
        });
        let p1: Vec<u32> = (first..first + n).map(|b| one.model.pending[b].load(Ordering::SeqCst)).collect();
        let data = match r {
            DataResult::Ok(d) => d,
            DataResult::Error(e) => return out.push(errno_mismatch("read", e)),
        };
        if data.len() != n * BLOCK {
            return out.push(Mismatch {
                op: "read",
                block: Some(first as u64),
                detail: format!("short read: {} of {}", data.len(), n * BLOCK),
            });
        }
        for i in 0..n {
            let got = &data[i * BLOCK..(i + 1) * BLOCK];
            let block = (first + i) as u64;
            let ok = (c0[i]..=p1[i].max(c0[i]))
                .any(|v| block_bytes(self.cfg.seed, FILE_ID, block, v) == got);
            if !ok {
                out.push(Mismatch {
                    op: "read",
                    block: Some(block),
                    detail: format!("bytes match no version in {}..={}", c0[i], p1[i]),
                });
            }
        }
    }

    fn write_blocks(&self, hs: &mut Hists, fh: u64, first: usize, n: usize, out: &mut Vec<Mismatch>) {
        let one = self.one.as_ref().expect("one-file context");
        let mut buf = Vec::with_capacity(n * BLOCK);
        for b in first..first + n {
            // Only the owner writes this block: pending is the last version.
            let v = one.model.pending[b].load(Ordering::SeqCst) + 1;
            one.model.pending[b].store(v, Ordering::SeqCst);
            buf.extend_from_slice(&block_bytes(self.cfg.seed, FILE_ID, b as u64, v));
        }
        let r = self.gated(hs, Kind::Write, || h::handle_write(&self.fs, fh, (first * BLOCK) as u64, &buf));
        match r {
            WriteResult::Ok(k) if k as usize == buf.len() => {
                for b in first..first + n {
                    let v = one.model.pending[b].load(Ordering::SeqCst);
                    one.model.committed[b].store(v, Ordering::SeqCst);
                }
            }
            WriteResult::Ok(k) => out.push(Mismatch {
                op: "write",
                block: Some(first as u64),
                detail: format!("short write: {k} of {}", buf.len()),
            }),
            WriteResult::Error(e) => out.push(errno_mismatch("write", e)),
        }
    }

    fn worker_one_file(&self, t: usize, hs: &mut Hists) -> WorkerOut {
        let one = self.one.as_ref().expect("one-file context");
        let mut out = Vec::new();
        let blocks = one.model.committed.len();
        let stripes = blocks / STRIPE;
        let owned: Vec<usize> = (0..stripes).filter(|s| s % self.cfg.threads == t).collect();
        let mut rng = Rng(self.cfg.seed ^ ((t as u64 + 1) << 40));
        let fh = match self.open_fh(hs, one.ino) {
            Ok(fh) => fh,
            Err(m) => return WorkerOut { mismatches: vec![m], model: None },
        };
        for _ in 0..self.cfg.ops_per_thread {
            if !self.keep_going(out.len()) {
                break;
            }
            if rng.below(25) == 0 {
                self.fsync_fh(hs, one.ino, fh, &mut out);
            } else if !owned.is_empty() && rng.below(100) < 60 {
                let first = owned[rng.below(owned.len())] * STRIPE;
                let n = 1 + rng.below(STRIPE);
                let start = first + rng.below(STRIPE - n + 1);
                self.write_blocks(hs, fh, start, n, &mut out);
                // A check after each step: read the written range back.
                self.read_blocks(hs, one.ino, start, n, &mut out);
            } else {
                let n = 1 + rng.below(STRIPE);
                let start = rng.below(blocks - n + 1);
                self.read_blocks(hs, one.ino, start, n, &mut out);
            }
        }
        self.release_fh(hs, one.ino, fh, &mut out);
        WorkerOut { mismatches: out, model: None }
    }

    // ---- multi-handle mode ----

    /// One shared file, three handles per thread: read-only, read-write and
    /// read-write with `O_APPEND` (POSIX.1 2.9.7, ROADMAP Phase 19 criterion
    /// 6). Writes go through the read-write handle, one owned block at a
    /// time; the read after each write (visibility) and the other reads use
    /// the inode of the read-only handle (`handle_read` takes the inode). Each
    /// block read must be exactly one allowed version (all or none). Appends
    /// go through the append handle with a wrong offset on purpose; they land
    /// after the block region (checked by `verify_appends`).
    fn worker_multi_handle(&self, t: usize, hs: &mut Hists) -> WorkerOut {
        let one = self.one.as_ref().expect("multi-handle context");
        let mut out = Vec::new();
        let blocks = one.model.committed.len();
        let owned: Vec<usize> = (0..blocks / STRIPE).filter(|s| s % self.cfg.threads == t).collect();
        let mut rng = Rng(self.cfg.seed ^ ((t as u64 + 1) << 40) ^ 0xa11);
        let mut fhs = Vec::new();
        for flags in [libc::O_RDONLY, libc::O_RDWR, libc::O_RDWR | libc::O_APPEND] {
            match self.open_flags(hs, one.ino, flags) {
                Ok(fh) => fhs.push(fh),
                Err(m) => {
                    out.push(m);
                    break;
                }
            }
        }
        if let [_ro, rw, ap] = fhs[..] {
            let mut k = 0u64;
            for _ in 0..self.cfg.ops_per_thread {
                if !self.keep_going(out.len()) {
                    break;
                }
                let r = rng.below(100);
                if r < 4 {
                    self.fsync_fh(hs, one.ino, rw, &mut out);
                } else if r < 44 && !owned.is_empty() {
                    let b = owned[rng.below(owned.len())] * STRIPE + rng.below(STRIPE);
                    self.write_blocks(hs, rw, b, 1, &mut out);
                    // A completed write is visible through the read-only handle.
                    self.read_blocks(hs, one.ino, b, 1, &mut out);
                } else if r < 64 && k < MAX_APPENDS {
                    let rec = append_record(self.cfg.seed, t as u64, k);
                    let wrong = if k % 2 == 0 { 0 } else { 999_999 + k };
                    match self.gated(hs, Kind::Append, || h::handle_write(&self.fs, ap, wrong, &rec)) {
                        WriteResult::Ok(n) if n as usize == RECORD => {
                            k += 1;
                            one.appends[t].store(k, Ordering::SeqCst);
                        }
                        WriteResult::Ok(n) => out.push(mm("append", format!("short append: {n} of {RECORD}"))),
                        WriteResult::Error(e) => out.push(errno_mismatch("append", e)),
                    }
                } else {
                    let n = 1 + rng.below(STRIPE);
                    let start = rng.below(blocks - n + 1);
                    self.read_blocks(hs, one.ino, start, n, &mut out);
                }
            }
        }
        for fh in fhs {
            self.release_fh(hs, one.ino, fh, &mut out);
        }
        WorkerOut { mismatches: out, model: None }
    }

    // ---- handler wrappers used by all modes ----

    fn open_fh(&self, hs: &mut Hists, ino: u64) -> Result<u64, Mismatch> {
        self.open_flags(hs, ino, libc::O_RDWR)
    }

    fn open_flags(&self, hs: &mut Hists, ino: u64, flags: i32) -> Result<u64, Mismatch> {
        match self.gated(hs, Kind::Open, || h::handle_open(&self.fs, ino, flags)) {
            OpenResult::Ok(fh, _) => Ok(fh),
            OpenResult::Error(e) => Err(errno_mismatch("open", e)),
        }
    }

    fn fsync_fh(&self, hs: &mut Hists, ino: u64, fh: u64, out: &mut Vec<Mismatch>) {
        if let EmptyResult::Error(e) = self.gated(hs, Kind::Fsync, || h::handle_fsync(&self.fs, ino, fh)) {
            out.push(errno_mismatch("fsync", e));
        }
    }

    fn release_fh(&self, hs: &mut Hists, ino: u64, fh: u64, out: &mut Vec<Mismatch>) {
        if let EmptyResult::Error(e) = self.gated(hs, Kind::Release, || h::handle_release(&self.fs, ino, fh)) {
            out.push(errno_mismatch("release", e));
        }
    }

    /// `Some(ino)` if the name exists, `None` on ENOENT; any other error is a mismatch.
    fn lookup(&self, hs: &mut Hists, dir: u64, name: &str, out: &mut Vec<Mismatch>) -> Option<(u64, u64)> {
        match self.gated(hs, Kind::Lookup, || h::handle_lookup(&self.fs, dir, name)) {
            EntryResult::Ok(ino, attr) => Some((ino, attr.size)),
            EntryResult::Error(e) if e == libc::ENOENT => None,
            EntryResult::Error(e) => {
                out.push(errno_mismatch("lookup", e));
                None
            }
        }
    }

    // ---- many-files mode ----

    /// Read `n` blocks from `first` of a live file and compare with the model.
    fn read_ent(&self, hs: &mut Hists, ent: &Ent, name: &str, first: usize, n: usize, out: &mut Vec<Mismatch>) {
        let r = self.gated(hs, Kind::Read, || {
            h::handle_read(&self.fs, ent.ino, (first * BLOCK) as u64, (n * BLOCK) as u32)
        });
        let data = match r {
            DataResult::Ok(d) => d,
            DataResult::Error(e) => return out.push(mm("read", format!("{name}: errno {e}"))),
        };
        if data.len() != n * BLOCK {
            return out.push(mm("read", format!("{name}: short read {} of {}", data.len(), n * BLOCK)));
        }
        for i in 0..n {
            let b = first + i;
            if data[i * BLOCK..(i + 1) * BLOCK] != block_bytes(self.cfg.seed, ent.ino, b as u64, ent.ver[b])[..] {
                out.push(Mismatch {
                    op: "read",
                    block: Some(b as u64),
                    detail: format!("{name}: block differs from version {}", ent.ver[b]),
                });
            }
        }
    }

    /// Write 1..=4 blocks at a random place, then read them back.
    fn write_ent(&self, hs: &mut Hists, ent: &mut Ent, name: &str, rng: &mut Rng, out: &mut Vec<Mismatch>) {
        let fh = match ent.fh {
            Some(fh) => fh,
            None => match self.open_fh(hs, ent.ino) {
                Ok(fh) => {
                    ent.fh = Some(fh);
                    fh
                }
                Err(m) => return out.push(m),
            },
        };
        let n = 1 + rng.below(4);
        let start = rng.below(MAX_BLOCKS - n + 1);
        let mut ver = ent.ver;
        let mut buf = Vec::with_capacity(n * BLOCK);
        for b in start..start + n {
            ver[b] += 1;
            buf.extend_from_slice(&block_bytes(self.cfg.seed, ent.ino, b as u64, ver[b]));
        }
        match self.gated(hs, Kind::Write, || h::handle_write(&self.fs, fh, (start * BLOCK) as u64, &buf)) {
            WriteResult::Ok(k) if k as usize == buf.len() => {
                ent.ver = ver;
                ent.len = ent.len.max(start + n);
                self.read_ent(hs, ent, name, start, n, out);
            }
            WriteResult::Ok(k) => out.push(mm("write", format!("{name}: short write {k} of {}", buf.len()))),
            WriteResult::Error(e) => out.push(mm("write", format!("{name}: errno {e}"))),
        }
    }

    fn worker_many_files(&self, t: usize, hs: &mut Hists) -> WorkerOut {
        let dirs = self.dirs.as_ref().expect("many-files context");
        let own = dirs.per_thread[t];
        let mut out: Vec<Mismatch> = Vec::new();
        let mut files: BTreeMap<(u64, String), Ent> = BTreeMap::new();
        let mut rng = Rng(self.cfg.seed ^ ((t as u64 + 1) << 40));
        let mut counter = 0usize;
        let fresh = |counter: &mut usize| {
            *counter += 1;
            format!("t{t}-{counter}")
        };

        for _ in 0..self.cfg.ops_per_thread {
            if !self.keep_going(out.len()) {
                break;
            }
            let mut op = pick_op(&mut rng);
            // Targets: pick a key that fits the op; no target means create.
            let keys_where = |p: &dyn Fn(&Ent) -> bool, files: &BTreeMap<(u64, String), Ent>| -> Vec<(u64, String)> {
                files.iter().filter(|(_, e)| p(e)).map(|(k, _)| k.clone()).collect()
            };
            let mut cands = match op {
                Op::Create => Vec::new(),
                Op::Write | Op::Rename | Op::Unlink => keys_where(&|_| true, &files),
                Op::Read => keys_where(&|e| e.len > 0, &files),
                Op::Fsync | Op::Release => keys_where(&|e| e.fh.is_some(), &files),
                Op::Open => keys_where(&|e| e.fh.is_none(), &files),
            };
            if op == Op::Create && files.len() >= MAX_LIVE {
                op = Op::Unlink;
                cands = keys_where(&|_| true, &files);
            }
            if op != Op::Create && cands.is_empty() {
                op = Op::Create;
            }
            match op {
                Op::Create => {
                    let dir = if rng.below(2) == 0 { own } else { dirs.shared };
                    let name = fresh(&mut counter);
                    let r = self.gated(hs, Kind::Create, || {
                        h::handle_create(&self.fs, dir, &name, S_IFREG | 0o644, 0o022, 501, 20, libc::O_RDWR)
                    });
                    let (ino, fh) = match r {
                        CreateResult::Ok(ino, fh, _) => (ino, fh),
                        CreateResult::Error(e) => {
                            out.push(mm("create", format!("{name}: errno {e}")));
                            continue;
                        }
                    };
                    let mut ent = Ent { ino, ver: [0; MAX_BLOCKS], len: 0, fh: Some(fh) };
                    match self.lookup(hs, dir, &name, &mut out) {
                        Some((i, 0)) if i == ino => {}
                        other => out.push(mm("create", format!("{name}: lookup after create gave {other:?}, want ino {ino} size 0"))),
                    }
                    self.write_ent(hs, &mut ent, &name, &mut rng, &mut out);
                    if rng.below(2) == 0 {
                        self.release_fh(hs, ino, fh, &mut out);
                        ent.fh = None;
                    }
                    files.insert((dir, name), ent);
                }
                Op::Write => {
                    let key = cands[rng.below(cands.len())].clone();
                    let ent = files.get_mut(&key).unwrap();
                    self.write_ent(hs, ent, &key.1, &mut rng, &mut out);
                }
                Op::Read => {
                    let key = cands[rng.below(cands.len())].clone();
                    let ent = &files[&key];
                    let n = 1 + rng.below(ent.len);
                    let first = rng.below(ent.len - n + 1);
                    self.read_ent(hs, ent, &key.1, first, n, &mut out);
                }
                Op::Fsync => {
                    let key = cands[rng.below(cands.len())].clone();
                    let ent = &files[&key];
                    self.fsync_fh(hs, ent.ino, ent.fh.unwrap(), &mut out);
                }
                Op::Release => {
                    let key = cands[rng.below(cands.len())].clone();
                    let ent = files.get_mut(&key).unwrap();
                    self.release_fh(hs, ent.ino, ent.fh.take().unwrap(), &mut out);
                }
                Op::Open => {
                    let key = cands[rng.below(cands.len())].clone();
                    let ent = files.get_mut(&key).unwrap();
                    match self.open_fh(hs, ent.ino) {
                        Ok(fh) => ent.fh = Some(fh),
                        Err(m) => out.push(m),
                    }
                }
                Op::Rename => {
                    let key = cands[rng.below(cands.len())].clone();
                    let ndir = if rng.below(2) == 0 { own } else { dirs.shared };
                    let nname = fresh(&mut counter);
                    let r = self.gated(hs, Kind::Rename, || {
                        h::handle_rename(&self.fs, key.0, &key.1, ndir, &nname, 0)
                    });
                    match r {
                        EmptyResult::Ok => {
                            let ent = files.remove(&key).unwrap();
                            match self.lookup(hs, ndir, &nname, &mut out) {
                                Some((i, _)) if i == ent.ino => {}
                                other => out.push(mm("rename", format!("{nname}: lookup after rename gave {other:?}, want ino {}", ent.ino))),
                            }
                            if let Some((i, _)) = self.lookup(hs, key.0, &key.1, &mut out) {
                                out.push(mm("rename", format!("{}: old name still resolves to ino {i}", key.1)));
                            }
                            files.insert((ndir, nname), ent);
                        }
                        EmptyResult::Error(e) => out.push(mm("rename", format!("{} -> {nname}: errno {e}", key.1))),
                    }
                }
                Op::Unlink => {
                    let key = cands[rng.below(cands.len())].clone();
                    let mut ent = files.remove(&key).unwrap();
                    if let Some(fh) = ent.fh.take() {
                        self.release_fh(hs, ent.ino, fh, &mut out);
                    }
                    match self.gated(hs, Kind::Unlink, || h::handle_unlink(&self.fs, key.0, &key.1)) {
                        EmptyResult::Ok => {
                            if let Some((i, _)) = self.lookup(hs, key.0, &key.1, &mut out) {
                                out.push(mm("unlink", format!("{}: still resolves to ino {i}", key.1)));
                            }
                        }
                        EmptyResult::Error(e) => {
                            out.push(mm("unlink", format!("{}: errno {e}", key.1)));
                            files.insert(key, ent);
                        }
                    }
                }
            }
        }
        // Close what is still open, so the final check opens files fresh.
        for ((_, name), ent) in files.iter_mut() {
            if let Some(fh) = ent.fh.take() {
                let mut tmp = Vec::new();
                self.release_fh(hs, ent.ino, fh, &mut tmp);
                out.extend(tmp.into_iter().map(|m| mm(m.op, format!("{name}: {}", m.detail))));
            }
        }
        WorkerOut { mismatches: out, model: Some(ThreadModel { files }) }
    }

    /// The commit thread: `timed_commit` every `COMMIT_EVERY`, and once more
    /// after `stop` is set (the workers are done by then).
    fn commit_loop(&self, stop: &AtomicBool) -> (CommitHists, Vec<Mismatch>) {
        self.commit_loop_every(stop, COMMIT_EVERY)
    }

    /// `commit_loop` with another interval (the S7 baseline uses the daemon's).
    fn commit_loop_every(&self, stop: &AtomicBool, every: Duration) -> (CommitHists, Vec<Mismatch>) {
        let mut ch = CommitHists::new();
        let mut out = Vec::new();
        loop {
            let t0 = Instant::now();
            let mut stopped = false;
            while t0.elapsed() < every {
                if stop.load(Ordering::SeqCst) {
                    stopped = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let tail = self.cfg.crash && (stopped || self.deadline.is_some_and(|d| Instant::now() + every > d));
            if !tail {
                match timed_commit(&self.fs, &self.gate) {
                    Ok(t) => {
                        ch.gate_wait.saturating_record(ns(t.gate_wait));
                        ch.hold.saturating_record(ns(t.hold));
                        ch.total.saturating_record(ns(t.total));
                    }
                    Err(e) => out.push(mm("commit", format!("{e:?}"))),
                }
            }
            if stopped {
                return (ch, out);
            }
        }
    }
}

/// One-file mode, after the join: every block of the file, read through the
/// handler, must equal the model's `committed` version.
fn verify_files(ctx: &Ctx) -> Vec<Mismatch> {
    let one = ctx.one.as_ref().expect("one-file context");
    let mut out = Vec::new();
    let mut hs = Hists::new();
    for b in 0..one.model.committed.len() {
        // No concurrent writer now: pending == committed.
        ctx.read_blocks(&mut hs, one.ino, b, 1, &mut out);
        let c = one.model.committed[b].load(Ordering::SeqCst);
        let p = one.model.pending[b].load(Ordering::SeqCst);
        if c != p {
            out.push(Mismatch { op: "verify", block: Some(b as u64), detail: format!("committed {c} != pending {p}") });
        }
    }
    out
}

/// Multi-handle mode, after the join: the file is the block region and then
/// every appended record exactly once and whole, in some order. Read in 64
/// KiB pieces with a bit set of the records seen (no whole-file buffer).
fn verify_appends(ctx: &Ctx) -> Vec<Mismatch> {
    const PIECE: u64 = 64 << 10;
    let one = ctx.one.as_ref().expect("multi-handle context");
    let counts: Vec<u64> = one.appends.iter().map(|c| c.load(Ordering::SeqCst)).collect();
    let total: u64 = counts.iter().sum();
    let base = ctx.cfg.file_bytes;
    let mut out = Vec::new();
    let size = match ctx.fs.test_getattr(one.ino) {
        Ok(a) => a.size,
        Err(e) => return vec![mm("append", format!("getattr: {e:?}"))],
    };
    if size != base + total * RECORD as u64 {
        out.push(mm("append", format!("size {size}, want {base} + {total} records")));
    }
    let mut seen = vec![0u64; (counts.len() * MAX_APPENDS as usize).div_ceil(64)];
    let mut found = 0u64;
    let mut off = base;
    while off < size && out.len() < 50 {
        let want = (size - off).min(PIECE);
        let piece = match h::handle_read(&ctx.fs, one.ino, off, want as u32) {
            DataResult::Ok(d) if d.len() as u64 == want => d,
            DataResult::Ok(d) => {
                out.push(mm("append", format!("short read at {off}: {} of {want}", d.len())));
                break;
            }
            DataResult::Error(e) => {
                out.push(mm("append", format!("read at {off}: errno {e}")));
                break;
            }
        };
        for (i, r) in piece.chunks(RECORD).enumerate() {
            let at = off + (i * RECORD) as u64;
            if r.len() != RECORD {
                out.push(mm("append", format!("part of a record at {at}")));
                continue;
            }
            let t = u64::from_le_bytes(r[..8].try_into().unwrap());
            let k = u64::from_le_bytes(r[8..16].try_into().unwrap());
            if t as usize >= counts.len() || k >= counts[t as usize] {
                out.push(mm("append", format!("record ({t}, {k}) at {at}: not an appended record")));
                continue;
            }
            if r != append_record(ctx.cfg.seed, t, k) {
                out.push(mm("append", format!("record ({t}, {k}) at {at}: not whole")));
                continue;
            }
            let bit = t as usize * MAX_APPENDS as usize + k as usize;
            if seen[bit / 64] & (1 << (bit % 64)) != 0 {
                out.push(mm("append", format!("record ({t}, {k}) at {at}: twice")));
                continue;
            }
            seen[bit / 64] |= 1 << (bit % 64);
            found += 1;
        }
        off += want;
    }
    if found != total {
        out.push(mm("append", format!("{found} of {total} records found")));
    }
    out
}

/// Many-files mode, after the join: the names of every directory equal the
/// model, every name resolves to the model inode, and every file has the model
/// size and content.
fn verify_namespace(ctx: &Ctx, models: &[ThreadModel]) -> Vec<Mismatch> {
    let dirs = ctx.dirs.as_ref().expect("many-files context");
    let mut out = Vec::new();
    let mut hs = Hists::new();
    let mut all_dirs: Vec<u64> = dirs.per_thread.clone();
    all_dirs.push(dirs.shared);
    for dir in all_dirs {
        let mut want: BTreeMap<String, &Ent> = BTreeMap::new();
        for m in models {
            for ((d, name), ent) in &m.files {
                if *d == dir {
                    want.insert(name.clone(), ent);
                }
            }
        }
        let have: BTreeSet<String> = match h::handle_readdir(&ctx.fs, dir, 0) {
            ReaddirResult::Ok(v) => v.into_iter().map(|e| e.2).filter(|n| n != "." && n != "..").collect(),
            ReaddirResult::Error(e) => {
                out.push(mm("namespace", format!("readdir dir {dir}: errno {e}")));
                continue;
            }
        };
        let want_names: BTreeSet<String> = want.keys().cloned().collect();
        for n in want_names.difference(&have) {
            out.push(mm("namespace", format!("{n}: in the model, missing from readdir of dir {dir}")));
        }
        for n in have.difference(&want_names) {
            out.push(mm("namespace", format!("{n}: in readdir of dir {dir}, not in the model")));
        }
        for (name, ent) in want {
            match ctx.lookup(&mut hs, dir, &name, &mut out) {
                Some((ino, size)) => {
                    if ino != ent.ino {
                        out.push(mm("namespace", format!("{name}: lookup gave ino {ino}, model {}", ent.ino)));
                    }
                    if size != (ent.len * BLOCK) as u64 {
                        out.push(mm("namespace", format!("{name}: size {size}, model {}", ent.len * BLOCK)));
                    }
                }
                None => {
                    out.push(mm("namespace", format!("{name}: lookup in dir {dir} gave ENOENT, model ino {}", ent.ino)));
                    continue;
                }
            }
            if ent.len == 0 {
                continue;
            }
            let fh = match ctx.open_fh(&mut hs, ent.ino) {
                Ok(fh) => fh,
                Err(m) => {
                    out.push(mm("namespace", format!("{name}: open {}", m.detail)));
                    continue;
                }
            };
            ctx.read_ent(&mut hs, ent, &name, 0, ent.len, &mut out);
            ctx.release_fh(&mut hs, ent.ino, fh, &mut out);
        }
    }
    out
}

struct Report {
    mode: Mode,
    dirty_total: usize,
    hists: Hists,
    commit: CommitHists,
    ops: usize,
    max_inflight: usize,
}

fn run(cfg: Config) -> Outcome {
    start_deadlock_watchdog();
    let _watchdog = watchdog(cfg.watchdog, cfg.seed);
    chunk_store::lock_stats::reset();
    let dir = TempDir::new().unwrap();
    let fs = new_fs(dir.path(), cfg.wal.clone());
    // The forced-drop knobs of the long run (`Config::from_env`); off by default.
    if cfg.drop_every > 0 {
        fs.test_drop_every(cfg.drop_every);
    }
    if cfg.dirty_limit_kib > 0 {
        fs.test_set_dirty_limit(cfg.dirty_limit_kib << 10);
    }
    let gate = fs.commit_gate();
    let (one, dirs) = match cfg.mode {
        Mode::OneFile | Mode::MultiHandle => {
            let ino = match h::handle_create(&fs, ROOT, "one-file", S_IFREG | 0o644, 0o022, 501, 20, libc::O_RDWR) {
                CreateResult::Ok(ino, fh, _) => {
                    assert!(
                        matches!(
                            h::handle_setattr(&fs, ino, None, None, None, Some(cfg.file_bytes), Some(fh), None, None),
                            AttrResult::Ok(_)
                        ),
                        "seed={} setattr size",
                        cfg.seed
                    );
                    assert!(matches!(h::handle_release(&fs, ino, fh), EmptyResult::Ok), "seed={} release", cfg.seed);
                    ino
                }
                CreateResult::Error(e) => panic!("seed={} create errno {e}", cfg.seed),
            };
            let blocks = (cfg.file_bytes as usize) / BLOCK;
            assert!(blocks % STRIPE == 0, "file size must be a multiple of a stripe");
            let appends = (0..cfg.threads).map(|_| AtomicU64::new(0)).collect();
            (Some(OneFile { model: FileModel::new(blocks), ino, appends }), None)
        }
        Mode::ManyFiles => {
            let shared = mkdir(&fs, ROOT, "shared", cfg.seed);
            let per_thread = (0..cfg.threads).map(|t| mkdir(&fs, ROOT, &format!("t{t}"), cfg.seed)).collect();
            (None, Some(Dirs { shared, per_thread }))
        }
    };
    let ctx = Ctx {
        fs,
        _dir: dir,
        gate,
        serial: Mutex::new(()),
        cfg: cfg.clone(),
        one,
        dirs,
        inflight: AtomicUsize::new(0),
        max_inflight: AtomicUsize::new(0),
        deadline: cfg.secs.map(|s| Instant::now() + Duration::from_secs(s)),
        halt: AtomicBool::new(false),
        ops_done: AtomicUsize::new(0),
    };
    println!(
        "STRESS seed={} mode={} threads={} serialize={} ops_per_thread={} secs={}",
        cfg.seed,
        cfg.mode.name(),
        cfg.threads,
        cfg.serialize,
        if cfg.secs.is_some() { "timed".to_string() } else { cfg.ops_per_thread.to_string() },
        cfg.secs.map_or("-".to_string(), |s| s.to_string()),
    );
    let mut total = Hists::new();
    let mut mismatches = Vec::new();
    let mut models = Vec::new();
    let stop = AtomicBool::new(false);
    let mut commit = CommitHists::new();
    let mut dirty_total = 0;
    std::thread::scope(|s| {
        // The strict-gate rule (D-16): on the commit and worker threads, a
        // cell or stripe taken outside a gate hold panics in a debug build.
        // Setup and the final verify run on this thread, without it.
        let committer = s.spawn(|| {
            chunk_store::sync::rank::require_gate(true);
            ctx.commit_loop(&stop)
        });
        let handles: Vec<_> = (0..cfg.threads)
            .map(|t| {
                let ctx = &ctx;
                s.spawn(move || {
                    chunk_store::sync::rank::require_gate(true);
                    let mut hs = Hists::new();
                    let w = match ctx.cfg.mode {
                        Mode::OneFile => ctx.worker_one_file(t, &mut hs),
                        Mode::ManyFiles => ctx.worker_many_files(t, &mut hs),
                        Mode::MultiHandle => ctx.worker_multi_handle(t, &mut hs),
                    };
                    (hs, w)
                })
            })
            .collect();
        // Join every worker first: the commit thread must be stopped even if
        // a worker panicked, or the scope would wait for it forever.
        let joined: Vec<_> = handles.into_iter().map(|j| j.join()).collect();
        dirty_total = ctx.fs.test_dirty_total();
        stop.store(true, Ordering::SeqCst);
        let (ch, cm) = committer.join().expect("commit thread panicked");
        commit = ch;
        mismatches.extend(cm);
        for j in joined {
            let (hs, w) = j.expect("worker panicked");
            total.add(&hs);
            mismatches.extend(w.mismatches);
            models.extend(w.model);
        }
    });
    match cfg.mode {
        Mode::OneFile => mismatches.extend(verify_files(&ctx)),
        Mode::ManyFiles => mismatches.extend(verify_namespace(&ctx, &models)),
        Mode::MultiHandle => {
            mismatches.extend(verify_files(&ctx));
            mismatches.extend(verify_appends(&ctx));
        }
    }
    let ops = ctx.ops_done.load(Ordering::SeqCst);
    let max_inflight = ctx.max_inflight.load(Ordering::SeqCst);
    Outcome {
        ctx,
        models,
        mismatches,
        report: Report { mode: cfg.mode, dirty_total, hists: total, commit, ops, max_inflight },
    }
}

// ---- report ----------------------------------------------------------------

fn us(v: u64) -> f64 {
    v as f64 / 1000.0
}

fn row(name: &str, h: &Histogram<u64>) {
    println!(
        "op={} n={} p50_us={:.1} p99_us={:.1} p999_us={:.1} max_us={:.1}",
        name,
        h.len(),
        us(h.value_at_quantile(0.50)),
        us(h.value_at_quantile(0.99)),
        us(h.value_at_quantile(0.999)),
        us(h.max())
    );
}

fn print_report(r: &Report) {
    println!("report mode={} (closed loop: no coordinated-omission correction)", r.mode.name());
    for (name, h) in KIND_NAMES.iter().zip(&r.hists.ops) {
        row(name, h);
    }
    row("gate_wait", &r.hists.gate_wait);
    row("gate_hold", &r.hists.gate_hold);
    row("commit_gate_wait", &r.commit.gate_wait);
    row("commit_hold", &r.commit.hold);
    row("commit_total", &r.commit.total);
    println!("max_inflight={}", r.max_inflight);
    if chunk_store::lock_stats::ENABLED {
        for st in chunk_store::lock_stats::snapshot().iter().filter(|s| s.count > 0) {
            println!(
                "lock site={} n={} wait_p50_us={:.1} wait_p99_us={:.1} wait_max_us={:.1} hold_p50_us={:.1} hold_p99_us={:.1} hold_max_us={:.1}",
                st.label,
                st.count,
                us(st.wait_p50_ns),
                us(st.wait_p99_ns),
                us(st.wait_max_ns),
                us(st.hold_p50_ns),
                us(st.hold_p99_ns),
                us(st.hold_max_ns)
            );
        }
    } else {
        println!("lock-stats off (build with --features lock-stats)");
    }
}

fn assert_clean(seed: u64, o: &Outcome) {
    for m in o.mismatches.iter().take(20) {
        println!("MISMATCH seed={seed} op={} block={:?} {}", m.op, m.block, m.detail);
    }
    assert!(o.mismatches.is_empty(), "seed={seed}: {} mismatches", o.mismatches.len());
}

// ---- tests -------------------------------------------------------------------

fn short_cfg(mode: Mode) -> Config {
    Config {
        mode,
        threads: 4,
        // Calibrated so the whole binary runs in a few seconds: fsync and
        // release cost tens of ms each in many-files mode.
        ops_per_thread: if mode == Mode::OneFile { 150 } else { 40 },
        secs: None,
        file_bytes: 4 << 20,
        seed: seed_from_env(),
        serialize: true,
        wal: WalConfig::PerOp,
        watchdog: Duration::from_secs(60),
        crash: false,
        drop_every: 0,
        dirty_limit_kib: 0,
    }
}

fn check_short(cfg: &Config, o: &Outcome) {
    print_report(&o.report);
    assert_clean(cfg.seed, o);
    assert!(o.report.ops > 0);
    // Serialize on means one operation at a time: this proves the mode works.
    assert_eq!(o.report.max_inflight, 1, "seed={}: serialize is on, but ops overlapped", cfg.seed);
    assert!(
        o.report.dirty_total < DIRTY_BUDGET / 4,
        "seed={}: dirty total {} is near the budget",
        cfg.seed,
        o.report.dirty_total
    );
}

#[test]
fn short_one_file() {
    let cfg = short_cfg(Mode::OneFile);
    let o = run(cfg.clone());
    check_short(&cfg, &o);
}

#[test]
fn short_many_files() {
    let cfg = short_cfg(Mode::ManyFiles);
    let o = run(cfg.clone());
    check_short(&cfg, &o);
}

/// The harness must be able to fail: a write and an unlink behind the model's
/// back are both reported.
#[test]
fn model_detects_planted_bug() {
    // File part: one thread, a short run, then one block changes behind the model.
    let mut cfg = short_cfg(Mode::OneFile);
    cfg.threads = 1;
    cfg.ops_per_thread = 30;
    let o = run(cfg.clone());
    assert_clean(cfg.seed, &o);
    let one = o.ctx.one.as_ref().unwrap();
    let block = 0usize;
    let v = one.model.committed[block].load(Ordering::SeqCst) + 7;
    let fh = match h::handle_open(&o.ctx.fs, one.ino, libc::O_RDWR) {
        OpenResult::Ok(fh, _) => fh,
        OpenResult::Error(e) => panic!("seed={} open errno {e}", cfg.seed),
    };
    let planted = block_bytes(cfg.seed, FILE_ID, block as u64, v);
    assert!(matches!(h::handle_write(&o.ctx.fs, fh, 0, &planted), WriteResult::Ok(_)));
    assert!(matches!(h::handle_release(&o.ctx.fs, one.ino, fh), EmptyResult::Ok));
    let found = verify_files(&o.ctx);
    assert_eq!(found.len(), 1, "seed={}: want one mismatch, got {found:?}", cfg.seed);
    assert_eq!(found[0].block, Some(block as u64), "seed={}: {found:?}", cfg.seed);

    // Namespace part: a live file is unlinked behind the model's back.
    let mut cfg = short_cfg(Mode::ManyFiles);
    cfg.threads = 1;
    cfg.ops_per_thread = 30;
    let mut o = run(cfg.clone());
    assert_clean(cfg.seed, &o);
    let dirs = o.ctx.dirs.as_ref().unwrap();
    let own = dirs.per_thread[0];
    if o.models[0].files.is_empty() {
        // Make sure one live file exists: create it and add it to the model.
        let name = "t0-planted".to_string();
        let ino = match h::handle_create(&o.ctx.fs, own, &name, S_IFREG | 0o644, 0o022, 501, 20, libc::O_RDWR) {
            CreateResult::Ok(ino, fh, _) => {
                assert!(matches!(h::handle_release(&o.ctx.fs, ino, fh), EmptyResult::Ok));
                ino
            }
            CreateResult::Error(e) => panic!("seed={} create errno {e}", cfg.seed),
        };
        o.models[0].files.insert((own, name), Ent { ino, ver: [0; MAX_BLOCKS], len: 0, fh: None });
        assert!(verify_namespace(&o.ctx, &o.models).is_empty(), "seed={}", cfg.seed);
    }
    let (dir, name) = o.models[0].files.keys().next().unwrap().clone();
    assert!(matches!(h::handle_unlink(&o.ctx.fs, dir, &name), EmptyResult::Ok));
    let found = verify_namespace(&o.ctx, &o.models);
    assert!(
        found.iter().any(|m| m.detail.contains(&name)),
        "seed={}: no mismatch names {name}: {found:?}",
        cfg.seed
    );

    // Append part: record (0, 0) is appended once more behind the model's back.
    let mut cfg = short_cfg(Mode::MultiHandle);
    cfg.threads = 1;
    cfg.ops_per_thread = 30;
    let o = run(cfg.clone());
    assert_clean(cfg.seed, &o);
    let ino = o.ctx.one.as_ref().unwrap().ino;
    let ap = match h::handle_open(&o.ctx.fs, ino, libc::O_RDWR | libc::O_APPEND) {
        OpenResult::Ok(fh, _) => fh,
        OpenResult::Error(e) => panic!("seed={} open errno {e}", cfg.seed),
    };
    assert!(matches!(h::handle_write(&o.ctx.fs, ap, 0, &append_record(cfg.seed, 0, 0)), WriteResult::Ok(_)));
    assert!(matches!(h::handle_release(&o.ctx.fs, ino, ap), EmptyResult::Ok));
    let found = verify_appends(&o.ctx);
    assert!(found.iter().any(|m| m.detail.contains("(0, 0)")), "seed={}: {found:?}", cfg.seed);
    assert!(verify_files(&o.ctx).is_empty(), "seed={}: the append changed the block region", cfg.seed);
}

/// The strict-gate rule catches a request that takes a cell with no gate
/// helper held (debug builds: the rank checker).
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "taken without the commit gate")]
fn strict_gate_catches_a_request_without_the_gate() {
    struct Off;
    impl Drop for Off {
        fn drop(&mut self) {
            chunk_store::sync::rank::require_gate(false);
        }
    }
    let dir = TempDir::new().unwrap();
    let fs = new_fs(dir.path(), WalConfig::PerOp);
    let fh = match h::handle_create(&fs, ROOT, "f", S_IFREG | 0o644, 0o022, 501, 20, libc::O_RDWR) {
        CreateResult::Ok(_, fh, _) => fh,
        CreateResult::Error(e) => panic!("create errno {e}"),
    };
    // Declared after `fs`, so the rule is off again before `fs` drops.
    let _off = Off;
    chunk_store::sync::rank::require_gate(true);
    let _ = h::handle_write(&fs, fh, 0, b"x");
}

/// The long run, by hand (use `--release`). Env vars: see `Config::from_env`.
#[test]
#[ignore = "long run by hand: STRESS_THREADS STRESS_SECS STRESS_FILE_MIB STRESS_SEED STRESS_SERIALIZE STRESS_MODE STRESS_DROP_EVERY STRESS_DIRTY_LIMIT_KIB; use --release"]
fn long_run() {
    for mode in modes_from_env() {
        let cfg = Config::from_env(Config { mode, ..short_cfg(mode) });
        let o = run(cfg.clone());
        print_report(&o.report);
        println!("ops={} dirty_total={}", o.report.ops, o.report.dirty_total);
        assert_clean(cfg.seed, &o);
    }
}

/// Serialize off: the target of Phases 19-21, in the default suite since
/// Phase 19. All three modes, 4 threads, 2 s each (multi-handle 1 s, so the
/// binary stays under about 10 s; RESEARCH Risk 7); the files are small, so
/// the dirty total stays below the budget.
#[test]
fn serialize_off_target() {
    for mode in [Mode::OneFile, Mode::ManyFiles, Mode::MultiHandle] {
        let cfg = Config {
            threads: 4,
            ops_per_thread: usize::MAX,
            secs: Some(if mode == Mode::MultiHandle { 1 } else { 2 }),
            serialize: false,
            watchdog: Duration::from_secs(30),
            ..short_cfg(mode)
        };
        let o = run(cfg.clone());
        print_report(&o.report);
        assert_clean(cfg.seed, &o);
        assert!(o.report.dirty_total < DIRTY_BUDGET, "seed={}: dirty total {}", cfg.seed, o.report.dirty_total);
        assert!(o.report.max_inflight > 1, "seed={}: ops never overlapped", cfg.seed);
    }
}

/// ROADMAP Phase 19 criterion 5: after a crash, replay of the write log of
/// a serialize-off run gives the state the run applied (names, inode numbers
/// of creates, sizes, content, link counts, mtime and ctime). The commit
/// thread cuts the log every 250 ms during the run, but not in its last
/// 250 ms, so the log holds the end of the run. The workers release every
/// handle at the end, as a client would. Then the file system is dropped
/// with no commit and no shutdown, and the store is loaded and replayed.
#[test]
fn crash_replay_matches_apply() {
    for mode in [Mode::ManyFiles, Mode::OneFile] {
        let cfg = Config {
            threads: 4,
            ops_per_thread: usize::MAX,
            secs: Some(1),
            serialize: false,
            watchdog: Duration::from_secs(30),
            crash: true,
            ..short_cfg(mode)
        };
        let o = run(cfg.clone());
        assert_clean(cfg.seed, &o);
        let Outcome { ctx: Ctx { fs, _dir: dir, .. }, .. } = o;
        let records = common::log_len(&fs, dir.path());
        assert!(records > 0, "seed={} mode={}: the log after the last cut is empty", cfg.seed, mode.name());
        let live = common::dump(&fs);
        std::mem::forget(fs); // a crash: no commit, no destroy
        let replayed = common::dump(&common::reload_and_replay(dir.path()));
        println!("STRESS crash replay mode={} records={records} entries={}", mode.name(), live.len());
        if replayed != live {
            for d in common::first_differences(&live, &replayed, 5) {
                println!("DIFF seed={} mode={} {d}", cfg.seed, mode.name());
            }
            panic!("seed={} mode={}: replay after the crash differs from the live state", cfg.seed, mode.name());
        }
    }
}

// ---- spikes S3, S5, S6, S7 (Phase 18): by hand, one test alone per run ----------
//
// The lock recorder is process-global, so each spike test runs alone
// (`-- --ignored --exact <name>`) and calls `lock_stats::reset()` before it
// measures. All of them are ignored tests, so the suite and CI never run them.

const MIB: usize = 1 << 20;
const MIB_BLOCKS: usize = MIB / BLOCK;

/// A comma list from the environment, every value in `lo..=hi`.
fn env_list(name: &str, default: &str, lo: u64, hi: u64) -> Vec<u64> {
    let s = std::env::var(name).unwrap_or_else(|_| default.to_string());
    let v: Vec<u64> = s
        .split(',')
        .map(|p| p.trim().parse().unwrap_or_else(|_| panic!("{name} must be numbers in {lo}..={hi}, comma separated")))
        .collect();
    assert!(!v.is_empty(), "{name} is empty");
    for x in &v {
        assert!((lo..=hi).contains(x), "{name} values must be in {lo}..={hi}, got {x}");
    }
    v
}

/// A context for the spike tests: no mode, no models; the file system, the
/// gate and the harness mutex only.
fn spike_ctx(serialize: bool, seed: u64, threads: usize, wal: WalConfig) -> Ctx {
    let dir = TempDir::new().unwrap();
    let fs = new_fs(dir.path(), wal.clone());
    let gate = fs.commit_gate();
    Ctx {
        fs,
        _dir: dir,
        gate,
        serial: Mutex::new(()),
        cfg: Config {
            mode: Mode::OneFile,
            threads,
            ops_per_thread: usize::MAX,
            secs: None,
            file_bytes: 0,
            seed,
            serialize,
            wal,
            watchdog: Duration::from_secs(60),
            crash: false,
            drop_every: 0,
            dirty_limit_kib: 0,
        },
        one: None,
        dirs: None,
        inflight: AtomicUsize::new(0),
        max_inflight: AtomicUsize::new(0),
        deadline: None,
        halt: AtomicBool::new(false),
        ops_done: AtomicUsize::new(0),
    }
}

/// Create an empty file; the inode and the open handle.
fn create_file(fs: &SliceFsFilesystem, parent: u64, name: &str, seed: u64) -> (u64, u64) {
    match h::handle_create(fs, parent, name, S_IFREG | 0o644, 0o022, 501, 20, libc::O_RDWR) {
        CreateResult::Ok(ino, fh, _) => (ino, fh),
        CreateResult::Error(e) => panic!("seed={seed} create {name} errno {e}"),
    }
}

/// Peak resident set of this process in MiB. `ru_maxrss` is KiB on Linux and
/// bytes on macOS; this is the one place that knows.
fn peak_rss_mib() -> u64 {
    // SAFETY: getrusage writes one rusage struct; zeroed is a valid value for it.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    assert_eq!(rc, 0, "getrusage failed");
    let raw = ru.ru_maxrss as u64;
    if cfg!(target_os = "macos") {
        raw >> 20
    } else {
        raw >> 10
    }
}

/// Peak resident set of this process in MiB from the `VmHWM:` line of
/// `/proc/self/status` (kB). Linux only; `None` on other systems.
fn vmhwm_mib() -> Option<u64> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb >> 10)
}

/// S7 memory diagnosis (glibc only). `mem_now` gives the live heap from
/// `mallinfo2` (in use = uordblks + hblkhd) and `VmRSS`, `RssAnon`, `RssFile`
/// of `/proc/self/status`, all in MiB.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
mod s7mem {
    #[derive(Clone, Copy, Default)]
    pub struct Mem {
        pub live: u64,
        pub rss: u64,
        pub anon: u64,
        pub file: u64,
    }

    fn status_mib(status: &str, key: &str) -> u64 {
        status.lines().find(|l| l.starts_with(key)).and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok()).unwrap_or(0) >> 10
    }

    pub fn mem_now() -> Mem {
        // SAFETY: mallinfo2 takes no pointers; glibc owns the state.
        let mi = unsafe { libc::mallinfo2() };
        let st = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        Mem { live: ((mi.uordblks + mi.hblkhd) >> 20) as u64, rss: status_mib(&st, "VmRSS:"), anon: status_mib(&st, "RssAnon:"), file: status_mib(&st, "RssFile:") }
    }

    /// Sample `mem_now` every 50 ms until `stop`; the sample with the most
    /// live heap and the sample with the most RSS.
    pub fn sample(stop: &std::sync::atomic::AtomicBool) -> (Mem, Mem) {
        let (mut by_live, mut by_rss) = (Mem::default(), Mem::default());
        while !stop.load(std::sync::atomic::Ordering::SeqCst) {
            let m = mem_now();
            if m.live > by_live.live {
                by_live = m;
            }
            if m.rss > by_rss.rss {
                by_rss = m;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        (by_live, by_rss)
    }

    /// One `S7 heap` line: the two peak samples, then the state now, then
    /// `malloc_trim(0)` and the RSS after it.
    pub fn print(peaks: (Mem, Mem)) {
        let (pl, pr) = peaks;
        // SAFETY: as above.
        let mi = unsafe { libc::mallinfo2() };
        let now = mem_now();
        let released = unsafe { libc::malloc_trim(0) };
        let after = mem_now();
        println!(
            "S7 heap peak_live_mib={} rss_at_peak_live_mib={} peak_rss_sample_mib={} live_at_peak_rss_mib={} anon_at_peak_rss_mib={} file_at_peak_rss_mib={} \
             end_live_mib={} end_mmap_mib={} end_arena_mib={} end_free_in_arenas_mib={} end_rss_mib={} end_anon_mib={} end_file_mib={} rss_after_trim_mib={} anon_after_trim_mib={} trim_released={released}",
            pl.live, pl.rss, pr.rss, pr.live, pr.anon, pr.file,
            now.live, mi.hblkhd >> 20, mi.arena >> 20, mi.fordblks >> 20, now.rss, now.anon, now.file, after.rss, after.anon
        );
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
mod s7mem {
    #[derive(Clone, Copy, Default)]
    pub struct Mem;
    pub fn sample(_stop: &std::sync::atomic::AtomicBool) -> (Mem, Mem) {
        (Mem, Mem)
    }
    pub fn print(_peaks: (Mem, Mem)) {}
}

fn ms(v: u64) -> f64 {
    v as f64 / 1_000_000.0
}

fn print_first(label: &str, seed: u64, found: &[Mismatch]) {
    for m in found.iter().take(20) {
        println!("{label} MISMATCH seed={seed} op={} block={:?} {}", m.op, m.block, m.detail);
    }
}

// ---- S3: store lock contention ----------------------------------------------

/// 1 MiB of seeded bytes for thread `t`, the base of every write of that thread.
fn s3_base(seed: u64, t: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(MIB);
    for b in 0..MIB_BLOCKS as u64 {
        v.extend_from_slice(&block_bytes(seed, t as u64 + 1, b, 1));
    }
    v
}

/// The 1 MiB of write `i` of thread `t`: the base with an 8-byte stamp in each
/// 4 KiB, so every chunk of every write differs (no dedup shortcut) and the
/// read phase can rebuild the expected bytes without holding the file.
fn s3_fill(base: &[u8], seed: u64, t: usize, i: u64, buf: &mut Vec<u8>) {
    buf.clear();
    buf.extend_from_slice(base);
    let k0 = splitmix(seed ^ splitmix(((t as u64) << 32) ^ i));
    for k in 0..MIB_BLOCKS {
        let stamp = splitmix(k0 ^ k as u64).to_le_bytes();
        buf[k * BLOCK..k * BLOCK + 8].copy_from_slice(&stamp);
    }
}

struct S3Out {
    write_mb_s: f64,
    read_mb_s: f64,
    max_inflight: usize,
    mismatches: Vec<Mismatch>,
    gate_wait: Histogram<u64>,
}

/// One N of S3: a fresh file system, N files with one handle each, a write
/// phase and a read phase. Throughput is the bytes of a phase over the
/// longest time any thread spent inside the wrapped calls (the buffer fill and
/// the compare are not counted).
fn s3_one(n: usize, file_mib: u64, serialize: bool, seed: u64) -> S3Out {
    // Safe subset: every file keeps at most 4 MiB dirty, and N x 4 MiB stays
    // below the dirty budget, so no write takes another file's state out.
    assert!(n * (4 << 20) < DIRTY_BUDGET, "S3 n={n} leaves the safe subset (dirty budget)");
    let ctx = spike_ctx(serialize, seed, n, WalConfig::PerOp);
    ctx.fs.test_set_dirty_limit(4 << 20);
    let files: Vec<(u64, u64)> = (0..n).map(|t| create_file(&ctx.fs, ROOT, &format!("s3-{t}"), seed)).collect();
    chunk_store::lock_stats::reset();
    let barrier = Barrier::new(n);
    let mut gate_wait = hist();
    let mut mismatches = Vec::new();
    let (mut w_max, mut r_max) = (Duration::ZERO, Duration::ZERO);
    std::thread::scope(|s| {
        let joins: Vec<_> = files
            .iter()
            .enumerate()
            .map(|(t, &(ino, fh))| {
                let (ctx, barrier) = (&ctx, &barrier);
                s.spawn(move || {
                    let base = s3_base(seed, t);
                    let mut buf = Vec::with_capacity(MIB);
                    let mut hs = Hists::new();
                    let mut out: Vec<Mismatch> = Vec::new();
                    let (mut w_busy, mut r_busy) = (Duration::ZERO, Duration::ZERO);
                    barrier.wait();
                    for i in 0..file_mib {
                        s3_fill(&base, seed, t, i, &mut buf);
                        let t0 = Instant::now();
                        let r = ctx.gated(&mut hs, Kind::Write, || h::handle_write(&ctx.fs, fh, i * MIB as u64, &buf));
                        w_busy += t0.elapsed();
                        match r {
                            WriteResult::Ok(k) if k as usize == MIB => {}
                            WriteResult::Ok(k) => out.push(mm("write", format!("short write {k} of {MIB} at MiB {i}"))),
                            WriteResult::Error(e) => out.push(errno_mismatch("write", e)),
                        }
                        if out.len() >= 20 {
                            break;
                        }
                    }
                    barrier.wait();
                    for i in 0..file_mib {
                        let t0 = Instant::now();
                        let r = ctx.gated(&mut hs, Kind::Read, || h::handle_read(&ctx.fs, ino, i * MIB as u64, MIB as u32));
                        r_busy += t0.elapsed();
                        match r {
                            DataResult::Ok(d) => {
                                s3_fill(&base, seed, t, i, &mut buf);
                                if d != buf {
                                    out.push(Mismatch {
                                        op: "read",
                                        block: Some(i),
                                        detail: format!("thread {t}: 1 MiB at MiB {i} differs from the model"),
                                    });
                                }
                            }
                            DataResult::Error(e) => out.push(errno_mismatch("read", e)),
                        }
                        if out.len() >= 20 {
                            break;
                        }
                    }
                    if let EmptyResult::Error(e) = ctx.gated(&mut hs, Kind::Release, || h::handle_release(&ctx.fs, ino, fh)) {
                        out.push(errno_mismatch("release", e));
                    }
                    (hs, out, w_busy, r_busy)
                })
            })
            .collect();
        for j in joins {
            let (hs, out, w, r) = j.join().expect("S3 worker panicked");
            gate_wait.add(&hs.gate_wait).unwrap();
            mismatches.extend(out);
            w_max = w_max.max(w);
            r_max = r_max.max(r);
        }
    });
    let total_mib = (n as u64 * file_mib) as f64;
    S3Out {
        write_mb_s: total_mib / w_max.as_secs_f64().max(1e-9),
        read_mb_s: total_mib / r_max.as_secs_f64().max(1e-9),
        max_inflight: ctx.max_inflight.load(Ordering::SeqCst),
        mismatches,
        gate_wait,
    }
}

/// S3 (D-08): N threads, each on its own file and handle, no namespace ops
/// after setup. MB here are MiB. Run once without and once with
/// `--features lock-stats`; the MB/s of the second pass include the cost of
/// the recorder, so take MB/s from the first pass and lock rows from the second.
#[test]
#[ignore = "spike S3, by hand: S3_THREADS S3_FILE_MIB S3_SERIALIZE; use --release; run once without and once with --features lock-stats"]
fn s3_store_lock() {
    let threads = env_list("S3_THREADS", "1,2,4,8", 1, 15);
    let file_mib = env_num("S3_FILE_MIB", 256, 1, 4096);
    let serialize = env_num("S3_SERIALIZE", 0, 0, 1) == 1;
    let seed = seed_from_env();
    let _watchdog = watchdog(Duration::from_secs(30 * 60), seed);
    let on = if chunk_store::lock_stats::ENABLED { "on" } else { "off" };
    println!("S3 seed={seed} file_mib={file_mib} serialize={serialize} lock_stats={on} MB=MiB");
    let mut bad = 0;
    for &n in &threads {
        let n = n as usize;
        let o = s3_one(n, file_mib, serialize, seed);
        println!(
            "S3 n={n} lock_stats={on} write_mb_s={:.1} read_mb_s={:.1} max_inflight={} mismatches={} serialize={serialize}",
            o.write_mb_s,
            o.read_mb_s,
            o.max_inflight,
            o.mismatches.len()
        );
        print!("S3 n={n} ");
        row("gate_wait", &o.gate_wait);
        if chunk_store::lock_stats::ENABLED {
            const SITES: [&str; 6] = ["store", "io", "readers", "file_cell", "wal", "wal_writer"];
            for st in chunk_store::lock_stats::snapshot().iter().filter(|s| s.count > 0 && SITES.contains(&s.label)) {
                println!(
                    "S3 n={n} lock site={} count={} wait_p50_us={:.1} wait_p99_us={:.1} wait_max_us={:.1} hold_p50_us={:.1} hold_p99_us={:.1} hold_max_us={:.1}",
                    st.label,
                    st.count,
                    us(st.wait_p50_ns),
                    us(st.wait_p99_ns),
                    us(st.wait_max_ns),
                    us(st.hold_p50_ns),
                    us(st.hold_p99_ns),
                    us(st.hold_max_ns)
                );
            }
        }
        print_first(&format!("S3 n={n}"), seed, &o.mismatches);
        bad += o.mismatches.len();
    }
    assert_eq!(bad, 0, "seed={seed}: S3 found {bad} mismatches (with serialize={serialize})");
}

// ---- S5: commit gate fairness -------------------------------------------------

/// The p50, p99 and max of a histogram in ms, as `max=<x> p99=<x> p50=<x> n=<n>`.
fn tail_ms(h: &Histogram<u64>) -> String {
    format!(
        "max={:.1} p99={:.1} p50={:.1} n={}",
        ms(h.max()),
        ms(h.value_at_quantile(0.99)),
        ms(h.value_at_quantile(0.50)),
        h.len()
    )
}

/// S5 (D-08), measured on the current code: a store with `S5_FILES` files, a
/// commit every 1 s, and 4 threads that hold the gate shared for long times.
/// What counts is the wait of a NEW shared holder while a commit is active: the
/// std `RwLock` queues readers behind a waiting writer.
#[test]
#[ignore = "spike S5, by hand: S5_FILES S5_SECS S5_HOLD_MS; use --release"]
fn s5_gate_fairness() {
    let files = env_num("S5_FILES", 78_000, 1, 1_000_000) as usize;
    let secs = env_num("S5_SECS", 30, 1, 600);
    let holds = env_list("S5_HOLD_MS", "50,200,500", 1, 10_000);
    let seed = seed_from_env();
    let setup_wd = watchdog(Duration::from_secs(300 + files as u64 / 50), seed);
    let ctx = spike_ctx(false, seed, 1, WalConfig::PerOp);
    let setup = Instant::now();
    let dirs: Vec<u64> = (0..100).map(|d| mkdir(&ctx.fs, ROOT, &format!("d{d}"), seed)).collect();
    let work = mkdir(&ctx.fs, ROOT, "work", seed);
    for i in 0..files {
        let (ino, fh) = create_file(&ctx.fs, dirs[i % 100], &format!("f{i}"), seed);
        assert!(matches!(h::handle_release(&ctx.fs, ino, fh), EmptyResult::Ok), "seed={seed} release");
    }
    timed_commit(&ctx.fs, &ctx.gate).expect("setup commit");
    println!("S5 setup files={files} secs={:.1}", setup.elapsed().as_secs_f64());
    drop(setup_wd);
    let _watchdog = watchdog(Duration::from_secs(secs + 120), seed);

    let stop = AtomicBool::new(false);
    let commit_active = AtomicBool::new(false);
    let longest_hold = AtomicU64::new(0);
    let holds = &holds;
    let (mut commit_h, mut commit_errs, mut active_h, mut idle_h) = (CommitHists::new(), Vec::new(), hist(), hist());
    let mut op_errs = Vec::new();
    std::thread::scope(|s| {
        let (ctx, stop, commit_active, longest_hold) = (&ctx, &stop, &commit_active, &longest_hold);
        // Holders: the gate shared for a long time, then a short pause.
        for k in 0..4usize {
            s.spawn(move || {
                let mut i = k;
                while !stop.load(Ordering::SeqCst) {
                    let hold = Duration::from_millis(holds[i % holds.len()]);
                    i += 1;
                    let g = ctx.gate.read().unwrap_or_else(|e| e.into_inner());
                    let t = Instant::now();
                    std::thread::sleep(hold);
                    longest_hold.fetch_max(ns(t.elapsed()), Ordering::SeqCst);
                    drop(g);
                    std::thread::sleep(Duration::from_millis(2));
                }
            });
        }
        // Real ops: small files through the wrapper, so each commit has work.
        let ops = s.spawn(move || {
            let mut hs = Hists::new();
            let mut errs: Vec<Mismatch> = Vec::new();
            let data = block_bytes(seed, 1, 0, 1);
            let mut i = 0u64;
            while !stop.load(Ordering::SeqCst) {
                let name = format!("w{i}");
                i += 1;
                let r = ctx.gated(&mut hs, Kind::Create, || {
                    h::handle_create(&ctx.fs, work, &name, S_IFREG | 0o644, 0o022, 501, 20, libc::O_RDWR)
                });
                let (ino, fh) = match r {
                    CreateResult::Ok(ino, fh, _) => (ino, fh),
                    CreateResult::Error(e) => {
                        errs.push(errno_mismatch("create", e));
                        break;
                    }
                };
                if !matches!(ctx.gated(&mut hs, Kind::Write, || h::handle_write(&ctx.fs, fh, 0, &data)), WriteResult::Ok(_)) {
                    errs.push(mm("write", format!("{name} failed")));
                }
                if !matches!(ctx.gated(&mut hs, Kind::Release, || h::handle_release(&ctx.fs, ino, fh)), EmptyResult::Ok) {
                    errs.push(mm("release", format!("{name} failed")));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            errs
        });
        // The commit thread: every 1 s, a sync first, then the timed commit.
        let committer = s.spawn(move || {
            let mut ch = CommitHists::new();
            let mut errs: Vec<Mismatch> = Vec::new();
            'outer: loop {
                let t0 = Instant::now();
                while t0.elapsed() < Duration::from_secs(1) {
                    if stop.load(Ordering::SeqCst) {
                        break 'outer;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                let _ = ctx.fs.content().sync();
                commit_active.store(true, Ordering::SeqCst);
                let r = timed_commit(&ctx.fs, &ctx.gate);
                commit_active.store(false, Ordering::SeqCst);
                match r {
                    Ok(t) => {
                        ch.gate_wait.saturating_record(ns(t.gate_wait));
                        ch.hold.saturating_record(ns(t.hold));
                        ch.total.saturating_record(ns(t.total));
                    }
                    Err(e) => errs.push(mm("commit", format!("{e:?}"))),
                }
            }
            (ch, errs)
        });
        // The probe: a new shared acquire every 10 ms, tagged by the flag at its start.
        let probe = s.spawn(move || {
            let (mut active, mut idle) = (hist(), hist());
            while !stop.load(Ordering::SeqCst) {
                let was_active = commit_active.load(Ordering::SeqCst);
                let t = Instant::now();
                let g = ctx.gate.read().unwrap_or_else(|e| e.into_inner());
                let waited = ns(t.elapsed());
                drop(g);
                if was_active { &mut active } else { &mut idle }.saturating_record(waited);
                std::thread::sleep(Duration::from_millis(10));
            }
            (active, idle)
        });
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(secs) {
            std::thread::sleep(Duration::from_millis(50));
        }
        stop.store(true, Ordering::SeqCst);
        (commit_h, commit_errs) = committer.join().expect("commit thread panicked");
        (active_h, idle_h) = probe.join().expect("probe thread panicked");
        op_errs = ops.join().expect("ops thread panicked");
    });
    println!(
        "S5 platform={} files={files} commits={} commit_gate_wait_ms p50={:.1} max={:.1} commit_hold_ms p50={:.1} max={:.1} commit_total_ms p50={:.1} max={:.1}",
        std::env::consts::OS,
        commit_h.total.len(),
        ms(commit_h.gate_wait.value_at_quantile(0.5)),
        ms(commit_h.gate_wait.max()),
        ms(commit_h.hold.value_at_quantile(0.5)),
        ms(commit_h.hold.max()),
        ms(commit_h.total.value_at_quantile(0.5)),
        ms(commit_h.total.max())
    );
    println!(
        "S5 new_reader_wait_ms commit_active {} idle {} longest_hold_ms={:.1}",
        tail_ms(&active_h),
        tail_ms(&idle_h),
        ms(longest_hold.load(Ordering::SeqCst))
    );
    println!("S5 note: use the commit_active max; p50/p99 include time after the gate is released");
    commit_errs.extend(op_errs);
    print_first("S5", seed, &commit_errs);
    assert!(commit_errs.is_empty(), "seed={seed}: S5 had {} errors", commit_errs.len());
    assert!(commit_h.total.len() > 0, "seed={seed}: no commit ran in {secs} s");
}

// ---- S6 in process (macOS numbers, no mount) ------------------------------------

/// S6 in process (D-09, macOS): small files with a timed fsync per file, for
/// the three WAL strategies, two interleaved passes. No FUSE: the cost of
/// SliceFS only.
#[test]
#[ignore = "spike S6 in process (macOS numbers, no mount), by hand: S6_FILES; use --release"]
fn s6_small_fsync() {
    let files = env_num("S6_FILES", 2000, 10, 20_000) as usize;
    let seed = seed_from_env();
    let _watchdog = watchdog(Duration::from_secs(30 * 60), seed);
    let strategies: [(&str, WalConfig); 3] = [
        ("per-op", WalConfig::PerOp),
        ("flush-on-fsync", WalConfig::FlushOnFsync),
        ("periodic", WalConfig::Periodic { interval_secs: 5 }),
    ];
    println!("S6 seed={seed} files={files} bytes=4096 note: in process, no FUSE: SliceFS cost only");
    for pass in 1..=2 {
        for (name, wal) in &strategies {
            let ctx = spike_ctx(false, seed, 1, wal.clone());
            let dir = mkdir(&ctx.fs, ROOT, "s6", seed);
            let mut hs = Hists::new();
            let t0 = Instant::now();
            for i in 0..files {
                let data = block_bytes(seed, 1, i as u64, 1);
                let (ino, fh) = match ctx.gated(&mut hs, Kind::Create, || {
                    h::handle_create(&ctx.fs, dir, &format!("f{i}"), S_IFREG | 0o644, 0o022, 501, 20, libc::O_RDWR)
                }) {
                    CreateResult::Ok(ino, fh, _) => (ino, fh),
                    CreateResult::Error(e) => panic!("seed={seed} create errno {e}"),
                };
                assert!(matches!(ctx.gated(&mut hs, Kind::Write, || h::handle_write(&ctx.fs, fh, 0, &data)), WriteResult::Ok(_)));
                assert!(matches!(ctx.gated(&mut hs, Kind::Fsync, || h::handle_fsync(&ctx.fs, ino, fh)), EmptyResult::Ok));
                assert!(matches!(ctx.gated(&mut hs, Kind::Release, || h::handle_release(&ctx.fs, ino, fh)), EmptyResult::Ok));
            }
            let wall = t0.elapsed().as_secs_f64();
            let f = &hs.ops[Kind::Fsync as usize];
            println!(
                "S6 wal={name} pass={pass} files={files} fsyncs_per_s_fsync_only={:.1} files_per_s_wall={:.1} fsync_p50_ms={:.2} fsync_p99_ms={:.2} fsync_max_ms={:.2} wall_s={wall:.1}",
                // fsync_only = n / sum(fsync times); wall = n / (create+write+fsync+release).
                // Same two definitions as scripts/docker-linux-test/s6-smallfsync.sh.
                files as f64 / (f.mean() * f.len() as f64 / 1e9),
                files as f64 / wall,
                ms(f.value_at_quantile(0.5)),
                ms(f.value_at_quantile(0.99)),
                ms(f.max())
            );
        }
    }
    println!("S6 note: in process, no FUSE: SliceFS cost only");
}

// ---- S7: dirty-budget baseline ----------------------------------------------------

/// S7 (D-08), the dirty-budget baseline: `S7_THREADS` writer threads
/// (1..=64, default 4), each with its own big file, the bigwrite random-write mix, peak RSS against the cap. The cap is
/// NOT asserted (a baseline); the model check is. The model is one version per
/// 4 KiB block, so no file bytes are held. `S7_SERIALIZE` (default 1) selects
/// the serialized form or the parallel form. `S7_FLUSHER=1` (plan 20-07) is
/// the flusher form: serialize off, the flusher thread runs, and each write
/// goes through `throttle_write` before the gate. It asserts VmHWM <= CAP_MIB
/// on Linux when the writer count is at most `pool::DEFAULT_WORKERS_MAX` (the
/// counts that the default worker count can give, phase 21 [Step limit]).
/// Above that count the S7 line shows the result and nothing is asserted.
#[test]
#[ignore = "spike S7 baseline: S7_THREADS S7_FILE_MIB S7_WRITE_MIB S7_COMMIT_SECS S7_SERIALIZE S7_FLUSHER; use --release"]
fn s7_budget_baseline() {
    let threads = env_num("S7_THREADS", 4, 1, 64) as usize;
    // CAP_MIB = Pm + F + R, rounded up to a multiple of 8 (B + N x W + O + F + R).
    // Inputs: Pm = 267 (largest VmHWM of 3 P_old and 3 P0 runs), Spread = 20,
    // F = 8 (one flusher step), R = max(16, ceil(Pm / 10), 2 x Spread) = 40.
    // The cap was fixed in plan 20-01 from P_old and P0 before the flusher
    // existed; do not change it to fit a later run (phase 20 runs/s7-wave0.log
    // and runs/s7-wave0-NOTES.md).
    const CAP_MIB: u64 = 320;
    let file_mib = env_num("S7_FILE_MIB", 1024, 4, 8192);
    let write_mib = env_num("S7_WRITE_MIB", 128, 2, 4096);
    let commit_secs = env_num("S7_COMMIT_SECS", 30, 1, 600);
    let flusher_on = env_num("S7_FLUSHER", 0, 0, 1) == 1;
    let serialize = !flusher_on && env_num("S7_SERIALIZE", 1, 0, 1) == 1;
    // The cap is a hard bar only at the writer counts the default worker count can give.
    let cap_checked = flusher_on && threads <= slicefs_cli::pool::DEFAULT_WORKERS_MAX;
    let seed = seed_from_env();
    let _watchdog = watchdog(Duration::from_secs(60 * 60), seed);
    // Serialize on is the Phase 18 baseline form. With serialize off the writers
    // run in parallel (Phase 19 removed the cross-file take-out). The flusher
    // form has serialize off.
    let ctx = spike_ctx(serialize, seed, threads, WalConfig::PerOp);
    let files: Vec<(u64, u64)> = (0..threads).map(|t| create_file(&ctx.fs, ROOT, &format!("s7-{t}"), seed)).collect();
    let blocks = file_mib as usize * MIB_BLOCKS;
    let rss_start = peak_rss_mib();
    let max_dirty = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let mem_stop = &AtomicBool::new(false);
    let mut slowest = 0u64;
    let mut mismatches: Vec<Mismatch> = Vec::new();
    let mut commits = CommitHists::new();
    let mut rss_writes = 0;
    let mut models: Vec<Vec<u32>> = Vec::new();
    std::thread::scope(|s| {
        let (ctx, max_dirty) = (&ctx, &max_dirty);
        // Flusher form: the flusher runs in the scope. The guard stops it after
        // the check phase, or when the scope ends on a panic.
        let flusher_guard = flusher_on.then(|| StopFlusher(&ctx.fs));
        if flusher_on {
            s.spawn(|| flusher::run_flusher(&ctx.fs));
            let t = Instant::now();
            while !ctx.fs.test_flusher_running() {
                assert!(t.elapsed() < Duration::from_secs(10), "seed={seed}: the S7 flusher does not run");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let committer = s.spawn(|| ctx.commit_loop_every(&stop, Duration::from_secs(commit_secs)));
        // Memory diagnosis: peak live heap and peak RSS during the write phase.
        let sampler = s.spawn(|| s7mem::sample(mem_stop));
        // Stops the sampler also when a writer panics, so the scope join does not wait.
        let _mem_guard = SetOnDrop(mem_stop);
        // Write phase: the whole file in 1 MiB writes, then the random mix.
        let writers: Vec<_> = files
            .iter()
            .enumerate()
            .map(|(t, &(ino, fh))| {
                s.spawn(move || {
                    let fid = t as u64 + 1;
                    let mut hs = Hists::new();
                    let mut out: Vec<Mismatch> = Vec::new();
                    let mut ver = vec![0u32; blocks];
                    let mut rng = Rng(seed ^ ((t as u64 + 1) << 40));
                    let mut buf = Vec::with_capacity(MIB);
                    // One write of `n` blocks from block `first`: bump the versions, fill, write.
                    let put = |first: usize, n: usize, hs: &mut Hists, out: &mut Vec<Mismatch>, buf: &mut Vec<u8>, ver: &mut Vec<u32>| {
                        buf.clear();
                        for b in first..first + n {
                            ver[b] += 1;
                            buf.extend_from_slice(&block_bytes(seed, fid, b as u64, ver[b]));
                        }
                        // Flusher form: the throttle first, with no lock held (as in the
                        // FUSE write callback). Else the write pays at once (`handle_write`).
                        let admit = if flusher_on { ctx.fs.throttle_write() } else { Admit { pay: true } };
                        // Sample inside `gated`, after the write, while the harness mutex is held:
                        // no other write is changing a dirty count then.
                        let r = ctx.gated(hs, Kind::Write, || {
                            let r = h::handle_write_admitted(&ctx.fs, fh, (first * BLOCK) as u64, buf, 0, admit);
                            max_dirty.fetch_max(ctx.fs.test_dirty_total(), Ordering::SeqCst);
                            r
                        });
                        match r {
                            WriteResult::Ok(k) if k as usize == buf.len() => {}
                            WriteResult::Ok(k) => out.push(mm("write", format!("short write {k} of {}", buf.len()))),
                            WriteResult::Error(e) => out.push(errno_mismatch("write", e)),
                        }
                    };
                    for i in 0..file_mib as usize {
                        put(i * MIB_BLOCKS, MIB_BLOCKS, &mut hs, &mut out, &mut buf, &mut ver);
                    }
                    // One round: a 1 MiB write and 256 writes of 4 KiB, at random 4 KiB-aligned
                    // offsets; an fsync every 64 MiB of the mix (2 MiB per round).
                    for round in 0..write_mib / 2 {
                        if out.len() >= 20 {
                            break;
                        }
                        put(rng.below(blocks - MIB_BLOCKS + 1), MIB_BLOCKS, &mut hs, &mut out, &mut buf, &mut ver);
                        for _ in 0..MIB_BLOCKS {
                            put(rng.below(blocks), 1, &mut hs, &mut out, &mut buf, &mut ver);
                        }
                        if round % 32 == 31 {
                            if let EmptyResult::Error(e) = ctx.gated(&mut hs, Kind::Fsync, || h::handle_fsync(&ctx.fs, ino, fh)) {
                                out.push(errno_mismatch("fsync", e));
                            }
                        }
                    }
                    (hs.ops[Kind::Write as usize].max(), out, ver)
                })
            })
            .collect();
        for j in writers {
            let (slow, out, ver) = j.join().expect("S7 writer panicked");
            slowest = slowest.max(slow);
            mismatches.extend(out);
            models.push(ver);
        }
        rss_writes = peak_rss_mib();
        mem_stop.store(true, Ordering::SeqCst);
        s7mem::print(sampler.join().expect("S7 memory sampler panicked"));
        // Check phase: every block of every file against its version.
        let checkers: Vec<_> = files
            .iter()
            .zip(models.iter())
            .enumerate()
            .map(|(t, (&(ino, fh), ver))| {
                s.spawn(move || {
                    let mut hs = Hists::new();
                    let mut out: Vec<Mismatch> = Vec::new();
                    for i in 0..file_mib as usize {
                        let r = ctx.gated(&mut hs, Kind::Read, || h::handle_read(&ctx.fs, ino, (i * MIB) as u64, MIB as u32));
                        let d = match r {
                            DataResult::Ok(d) if d.len() == MIB => d,
                            DataResult::Ok(d) => {
                                out.push(mm("read", format!("thread {t}: short read {} at MiB {i}", d.len())));
                                continue;
                            }
                            DataResult::Error(e) => {
                                out.push(errno_mismatch("read", e));
                                continue;
                            }
                        };
                        for j in 0..MIB_BLOCKS {
                            let b = i * MIB_BLOCKS + j;
                            if d[j * BLOCK..(j + 1) * BLOCK] != block_bytes(seed, t as u64 + 1, b as u64, ver[b])[..] {
                                out.push(Mismatch { op: "read", block: Some(b as u64), detail: format!("thread {t}: block differs from version {}", ver[b]) });
                                break;
                            }
                        }
                        if out.len() >= 20 {
                            break;
                        }
                    }
                    if let EmptyResult::Error(e) = ctx.gated(&mut hs, Kind::Release, || h::handle_release(&ctx.fs, ino, fh)) {
                        out.push(errno_mismatch("release", e));
                    }
                    out
                })
            })
            .collect();
        for j in checkers {
            mismatches.extend(j.join().expect("S7 checker panicked"));
        }
        drop(flusher_guard);
        stop.store(true, Ordering::SeqCst);
        let (ch, cm) = committer.join().expect("commit thread panicked");
        commits = ch;
        mismatches.extend(cm);
    });
    let peak = peak_rss_mib();
    let hwm = vmhwm_mib();
    let vmhwm = hwm.map_or_else(|| "na".to_string(), |v| v.to_string());
    let fl = ctx.fs.test_flusher_stats();
    println!(
        "S7 threads={threads} serialize={serialize} flusher={} cap_checked={cap_checked} file_mib={file_mib} write_mib={write_mib} peak_rss_mib={peak} vmhwm_mib={vmhwm} cap_mib={CAP_MIB} within_cap={} slowest_write_ms={:.1} rebase_steps={} max_dirty_mib={} rss_start_mib={rss_start} rss_after_writes_mib={rss_writes} commit_every_s={commit_secs} commits={} commit_hold_max_ms={:.1} commit_total_max_ms={:.1} commit_gate_wait_max_ms={:.1} flusher_passes={} flusher_steps={} flusher_published={} flusher_drops={} flusher_skips={} flusher_errors={} throttled={} throttle_timeouts={} throttle_wait_max_ms={}",
        u8::from(flusher_on),
        peak <= CAP_MIB,
        ms(slowest),
        ctx.fs.test_rebase_count(),
        max_dirty.load(Ordering::SeqCst) >> 20,
        commits.total.len(),
        ms(commits.hold.max()),
        ms(commits.total.max()),
        ms(commits.gate_wait.max()),
        fl.passes,
        fl.steps,
        fl.published,
        fl.drops,
        fl.skips,
        fl.errors,
        fl.throttled,
        fl.timeouts,
        fl.max_wait_ms
    );
    println!("S7 peak_rss_mib is the whole test process (getrusage), also the model and the check buffers");
    print_first("S7", seed, &mismatches);
    assert!(mismatches.is_empty(), "seed={seed}: S7 found {} mismatches", mismatches.len());
    if cap_checked {
        match hwm {
            Some(v) => assert!(v <= CAP_MIB, "seed={seed}: S7 flusher form: VmHWM {v} MiB is above the cap {CAP_MIB} MiB"),
            None => println!("S7 cap not checked: no VmHWM on this system"),
        }
    }
}

/// Sets the flag when it drops (also on a panic).
struct SetOnDrop<'a>(&'a AtomicBool);

impl Drop for SetOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Stops the flusher of the S7 flusher form when it drops (also on a panic):
/// throttled writers then write and pay, and the scope join does not wait.
struct StopFlusher<'a>(&'a SliceFsFilesystem);

impl Drop for StopFlusher<'_> {
    fn drop(&mut self) {
        self.0.stop_flusher();
    }
}
