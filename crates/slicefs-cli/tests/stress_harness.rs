//! In-process stress harness (Phase 18). N threads run mixed operations
//! against the `SliceFsFilesystem` core through the request handlers (no FUSE
//! mount). The data are checked against a model, and the report shows latency
//! histograms and the commit gate times.
//!
//! Two modes: `OneFile` (all threads on one shared file, each thread owns a
//! set of stripes) and `ManyFiles` (each thread owns its names in its own
//! directory and in `shared`, and creates, renames, unlinks, writes, fsyncs
//! them). A commit thread runs `timed_commit` beside the workers.
//!
//! The core is safe with one operation at a time only (until Phase 20), so the
//! suite runs with `serialize` on: N threads, but a harness mutex lets one
//! operation run at a time. Lock order: the harness mutex first, then the
//! commit gate (never the harness mutex while the gate is held). The commit
//! thread does not take the harness mutex (as in the daemon).
//!
//! File bytes are never held in RAM as a model: a block is a function of
//! (seed, file, block, version), and the model keeps only versions.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hdrhistogram::Histogram;
use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use metadata::wal::{create_wal, WalConfig};
use slicefs_cli::commit_timer::timed_commit;
use slicefs_cli::filesystem::{SliceFsFilesystem, DIRTY_BUDGET};
use slicefs_traits::metadata::MetadataStore;
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
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::OneFile => "one-file",
            Mode::ManyFiles => "many-files",
        }
    }
}

#[derive(Clone)]
struct Config {
    mode: Mode,
    threads: usize,
    ops_per_thread: usize,
    file_bytes: u64,
    seed: u64,
    serialize: bool,
    wal: WalConfig,
}

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

struct OneFile {
    model: FileModel,
    ino: u64,
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
}

const KIND_NAMES: [&str; 9] =
    ["read", "write", "fsync", "create", "rename", "unlink", "open", "release", "lookup"];

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
    std::fs::create_dir_all(store.join("segments")).unwrap();
    let wal = create_wal(config, store, 1).unwrap();
    let io = Arc::new(Mutex::new(StoreIo::open(store).unwrap()));
    let mut meta = DictMetadataStore::new(io.clone());
    meta.set_wal(wal);
    let fs = SliceFsFilesystem::new(meta, io, Some(store.to_path_buf()));
    fs.meta().commit().unwrap();
    fs
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
    /// One operation: the harness mutex (serialize mode), then the commit gate
    /// (as `fuse_callbacks.rs` does), then the handler. The latency is the
    /// handler call only; the gate wait and hold are timed apart.
    fn gated<R>(&self, hs: &mut Hists, kind: Kind, f: impl FnOnce() -> R) -> R {
        let _s = self.cfg.serialize.then(|| self.serial.lock().unwrap());
        let w = Instant::now();
        let g = self.gate.read().unwrap_or_else(|e| e.into_inner());
        let acquired = Instant::now();
        hs.gate_wait.saturating_record(ns(acquired - w));
        let r = f();
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
            if out.len() >= 50 {
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

    // ---- handler wrappers used by both modes ----

    fn open_fh(&self, hs: &mut Hists, ino: u64) -> Result<u64, Mismatch> {
        match self.gated(hs, Kind::Open, || h::handle_open(&self.fs, ino, libc::O_RDWR)) {
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
            if out.len() >= 50 {
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
        let mut ch = CommitHists::new();
        let mut out = Vec::new();
        loop {
            let t0 = Instant::now();
            let mut stopped = false;
            while t0.elapsed() < COMMIT_EVERY {
                if stop.load(Ordering::SeqCst) {
                    stopped = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            match timed_commit(&self.fs, &self.gate) {
                Ok(t) => {
                    ch.gate_wait.saturating_record(ns(t.gate_wait));
                    ch.hold.saturating_record(ns(t.hold));
                    ch.total.saturating_record(ns(t.total));
                }
                Err(e) => out.push(mm("commit", format!("{e:?}"))),
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
}

fn run(cfg: Config) -> Outcome {
    chunk_store::lock_stats::reset();
    let dir = TempDir::new().unwrap();
    let fs = new_fs(dir.path(), cfg.wal.clone());
    let gate = fs.commit_gate();
    let (one, dirs) = match cfg.mode {
        Mode::OneFile => {
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
            (Some(OneFile { model: FileModel::new(blocks), ino }), None)
        }
        Mode::ManyFiles => {
            let shared = mkdir(&fs, ROOT, "shared", cfg.seed);
            let per_thread = (0..cfg.threads).map(|t| mkdir(&fs, ROOT, &format!("t{t}"), cfg.seed)).collect();
            (None, Some(Dirs { shared, per_thread }))
        }
    };
    let ctx = Ctx { fs, _dir: dir, gate, serial: Mutex::new(()), cfg: cfg.clone(), one, dirs };
    println!(
        "STRESS seed={} mode={} threads={} serialize={} ops={}",
        cfg.seed,
        cfg.mode.name(),
        cfg.threads,
        cfg.serialize,
        cfg.threads * cfg.ops_per_thread
    );
    let mut total = Hists::new();
    let mut mismatches = Vec::new();
    let mut models = Vec::new();
    let stop = AtomicBool::new(false);
    let mut commit = CommitHists::new();
    let mut dirty_total = 0;
    std::thread::scope(|s| {
        let committer = s.spawn(|| ctx.commit_loop(&stop));
        let handles: Vec<_> = (0..cfg.threads)
            .map(|t| {
                let ctx = &ctx;
                s.spawn(move || {
                    let mut hs = Hists::new();
                    let w = match ctx.cfg.mode {
                        Mode::OneFile => ctx.worker_one_file(t, &mut hs),
                        Mode::ManyFiles => ctx.worker_many_files(t, &mut hs),
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
    }
    let ops = cfg.threads * cfg.ops_per_thread;
    Outcome { ctx, models, mismatches, report: Report { mode: cfg.mode, dirty_total, hists: total, commit, ops } }
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
        file_bytes: 4 << 20,
        seed: seed_from_env(),
        serialize: true,
        wal: WalConfig::PerOp,
    }
}

fn check_short(cfg: &Config, o: &Outcome) {
    print_report(&o.report);
    assert_clean(cfg.seed, o);
    assert!(o.report.ops > 0);
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
