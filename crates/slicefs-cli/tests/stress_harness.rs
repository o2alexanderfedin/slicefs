//! In-process stress harness (Phase 18). N threads run mixed operations
//! against the `SliceFsFilesystem` core through the request handlers (no FUSE
//! mount). The data are checked against a model, and the report shows latency
//! histograms and the commit gate times.
//!
//! The core is safe with one operation at a time only (until Phase 20), so the
//! suite runs with `serialize` on: N threads, but a harness mutex lets one
//! operation run at a time. Lock order: the harness mutex first, then the
//! commit gate (never the harness mutex while the gate is held).
//!
//! File bytes are never held in RAM as a model: a block is a function of
//! (seed, file, block, version), and the model keeps only versions.

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use hdrhistogram::Histogram;
use metadata::store::DictMetadataStore;
use metadata::store_io::StoreIo;
use metadata::wal::{create_wal, WalConfig};
use slicefs_cli::filesystem::{SliceFsFilesystem, DIRTY_BUDGET};
use slicefs_traits::metadata::MetadataStore;
use slicefs_cli::handlers::{
    self as h, AttrResult, CreateResult, DataResult, EmptyResult, OpenResult, WriteResult,
};
use tempfile::TempDir;

const S_IFREG: u32 = 0o100_000;
const BLOCK: usize = 4096;
/// Blocks in a stripe: the unit of ownership. Only the owner thread writes it.
const STRIPE: usize = 16;
const ROOT: u64 = 1;
const FILE_ID: u64 = 1;

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

#[derive(Clone)]
struct Config {
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

/// Per 4 KiB block, two versions: `committed` (the last write that returned
/// Ok) and `pending` (the last write that was started).
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

#[derive(Debug)]
struct Mismatch {
    op: &'static str,
    block: Option<u64>,
    detail: String,
}

fn errno_mismatch(op: &'static str, errno: i32) -> Mismatch {
    Mismatch { op, block: None, detail: format!("errno {errno}") }
}

// ---- histograms ----------------------------------------------------------

fn hist() -> Histogram<u64> {
    Histogram::<u64>::new_with_bounds(1, 60_000_000_000, 3).unwrap()
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Read,
    Write,
}

struct Hists {
    read: Histogram<u64>,
    write: Histogram<u64>,
    gate_wait: Histogram<u64>,
    gate_hold: Histogram<u64>,
}

impl Hists {
    fn new() -> Self {
        Self { read: hist(), write: hist(), gate_wait: hist(), gate_hold: hist() }
    }
    fn add(&mut self, o: &Hists) {
        self.read.add(&o.read).unwrap();
        self.write.add(&o.write).unwrap();
        self.gate_wait.add(&o.gate_wait).unwrap();
        self.gate_hold.add(&o.gate_hold).unwrap();
    }
}

fn ns(d: std::time::Duration) -> u64 {
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

struct Ctx<'a> {
    fs: &'a SliceFsFilesystem,
    gate: Arc<RwLock<()>>,
    serial: Mutex<()>,
    cfg: Config,
    model: FileModel,
    ino: u64,
}

impl Ctx<'_> {
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
        match kind {
            Kind::Read => hs.read.saturating_record(op_ns),
            Kind::Write => hs.write.saturating_record(op_ns),
        }
        hs.gate_hold.saturating_record(hold);
        r
    }

    fn read_blocks(&self, hs: &mut Hists, fh_ino: u64, first: usize, n: usize, out: &mut Vec<Mismatch>) {
        let c0: Vec<u32> = (first..first + n).map(|b| self.model.committed[b].load(Ordering::SeqCst)).collect();
        let r = self.gated(hs, Kind::Read, || {
            h::handle_read(self.fs, fh_ino, (first * BLOCK) as u64, (n * BLOCK) as u32)
        });
        let p1: Vec<u32> = (first..first + n).map(|b| self.model.pending[b].load(Ordering::SeqCst)).collect();
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
        let mut buf = Vec::with_capacity(n * BLOCK);
        for b in first..first + n {
            // Only the owner writes this block: pending is the last version.
            let v = self.model.pending[b].load(Ordering::SeqCst) + 1;
            self.model.pending[b].store(v, Ordering::SeqCst);
            buf.extend_from_slice(&block_bytes(self.cfg.seed, FILE_ID, b as u64, v));
        }
        let r = self.gated(hs, Kind::Write, || h::handle_write(self.fs, fh, (first * BLOCK) as u64, &buf));
        match r {
            WriteResult::Ok(k) if k as usize == buf.len() => {
                for b in first..first + n {
                    let v = self.model.pending[b].load(Ordering::SeqCst);
                    self.model.committed[b].store(v, Ordering::SeqCst);
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

    fn worker(&self, t: usize, hs: &mut Hists) -> Vec<Mismatch> {
        let mut out = Vec::new();
        let blocks = self.model.committed.len();
        let stripes = blocks / STRIPE;
        let owned: Vec<usize> = (0..stripes).filter(|s| s % self.cfg.threads == t).collect();
        let mut rng = Rng(self.cfg.seed ^ ((t as u64 + 1) << 40));
        let fh = match h::handle_open(self.fs, self.ino, libc::O_RDWR) {
            OpenResult::Ok(fh, _) => fh,
            OpenResult::Error(e) => return vec![errno_mismatch("open", e)],
        };
        for _ in 0..self.cfg.ops_per_thread {
            if out.len() >= 50 {
                break;
            }
            if !owned.is_empty() && rng.below(100) < 60 {
                let first = owned[rng.below(owned.len())] * STRIPE;
                let n = 1 + rng.below(STRIPE);
                let start = first + rng.below(STRIPE - n + 1);
                self.write_blocks(hs, fh, start, n, &mut out);
                // A check after each step: read the written range back.
                self.read_blocks(hs, self.ino, start, n, &mut out);
            } else {
                let n = 1 + rng.below(STRIPE);
                let start = rng.below(blocks - n + 1);
                self.read_blocks(hs, self.ino, start, n, &mut out);
            }
        }
        if let EmptyResult::Error(e) = h::handle_release(self.fs, self.ino, fh) {
            out.push(errno_mismatch("release", e));
        }
        out
    }
}

/// After the join: every block of the file, read through the handler, must
/// equal the model's `committed` version.
fn verify_files(ctx: &Ctx) -> Vec<Mismatch> {
    let mut out = Vec::new();
    let mut hs = Hists::new();
    for b in 0..ctx.model.committed.len() {
        // No concurrent writer now: pending == committed.
        ctx.read_blocks(&mut hs, ctx.ino, b, 1, &mut out);
        let c = ctx.model.committed[b].load(Ordering::SeqCst);
        let p = ctx.model.pending[b].load(Ordering::SeqCst);
        if c != p {
            out.push(Mismatch { op: "verify", block: Some(b as u64), detail: format!("committed {c} != pending {p}") });
        }
    }
    out
}

struct Report {
    mismatches: Vec<Mismatch>,
    dirty_total: usize,
    hists: Hists,
    ops: usize,
}

fn run_one_file(cfg: Config) -> Report {
    chunk_store::lock_stats::reset();
    let dir = TempDir::new().unwrap();
    let fs = new_fs(dir.path(), cfg.wal.clone());
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
    let ctx = Ctx {
        fs: &fs,
        gate: fs.commit_gate(),
        serial: Mutex::new(()),
        cfg: cfg.clone(),
        model: FileModel::new(blocks),
        ino,
    };
    println!(
        "STRESS seed={} mode=one-file threads={} serialize={} ops={}",
        cfg.seed,
        cfg.threads,
        cfg.serialize,
        cfg.threads * cfg.ops_per_thread
    );
    let mut total = Hists::new();
    let mut mismatches = Vec::new();
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..cfg.threads)
            .map(|t| {
                let ctx = &ctx;
                s.spawn(move || {
                    let mut hs = Hists::new();
                    let m = ctx.worker(t, &mut hs);
                    (hs, m)
                })
            })
            .collect();
        for j in handles {
            let (hs, m) = j.join().expect("worker panicked");
            total.add(&hs);
            mismatches.extend(m);
        }
    });
    mismatches.extend(verify_files(&ctx));
    let dirty_total = fs.test_dirty_total();
    Report { mismatches, dirty_total, hists: total, ops: cfg.threads * cfg.ops_per_thread }
}

// ---- report ----------------------------------------------------------------

fn us(v: u64) -> f64 {
    v as f64 / 1000.0
}

fn print_report(r: &Report) {
    println!("closed loop: no coordinated-omission correction");
    for (name, h) in [
        ("read", &r.hists.read),
        ("write", &r.hists.write),
        ("gate_wait", &r.hists.gate_wait),
        ("gate_hold", &r.hists.gate_hold),
    ] {
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

fn assert_clean(seed: u64, r: &Report) {
    for m in r.mismatches.iter().take(20) {
        println!("MISMATCH seed={seed} op={} block={:?} {}", m.op, m.block, m.detail);
    }
    assert!(r.mismatches.is_empty(), "seed={seed}: {} mismatches", r.mismatches.len());
}

// ---- tests -------------------------------------------------------------------

#[test]
fn short_one_file() {
    let cfg = Config {
        threads: 4,
        ops_per_thread: 300,
        file_bytes: 4 << 20,
        seed: seed_from_env(),
        serialize: true,
        wal: WalConfig::PerOp,
    };
    let r = run_one_file(cfg.clone());
    print_report(&r);
    assert_clean(cfg.seed, &r);
    assert!(r.ops > 0);
    assert!(
        r.dirty_total < DIRTY_BUDGET / 4,
        "seed={}: dirty total {} is near the budget",
        cfg.seed,
        r.dirty_total
    );
}
