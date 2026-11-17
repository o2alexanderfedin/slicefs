//! Spike E bench `e1`: put throughput, CPU, page faults and sync cost of the
//! three write paths (`mmap_write::Mode`), one writer thread.
//!
//! Variant: `alloc` | `pwbuf` | `mmap`, on `Reserve` (V2) arenas; a `V3`
//! suffix (`pwbufV3`, `mmapV3`) uses `Zeroed` arenas.
//! `--kind random|comp`, `--sync none|n64|mib1`, `--mib` raw MiB per run.
//! CPU and faults of the puts = the whole span minus the sync calls. Kernel
//! writeback (dirty pages written later by the kernel) is in neither path's
//! numbers.

use super::{run_dir, Args};
use crate::measure::{median, pct, Rec, Span, Usage};
use crate::mmap_write::{Mode, Writer};
use crate::{workload, ArenaCfg, Prealloc};
use std::time::Instant;

const MIB: u64 = 1 << 20;

pub fn run(bench: &str, variant: &str, a: &Args) -> bool {
    match bench {
        "e1" => e1(variant, a),
        _ => return false,
    }
    true
}

fn objects(kind: &str, mib: u64, seed: u64) -> Vec<Vec<u8>> {
    let sizes = workload::cdc_sizes(seed ^ 0x5151);
    let mut out = Vec::new();
    let mut total = 0u64;
    let mut i = 0usize;
    while total < mib * MIB {
        let len = sizes[i % sizes.len()];
        let s = seed.wrapping_mul(1_000_003).wrapping_add(i as u64);
        out.push(if kind == "comp" { workload::compressible(len, s) } else { workload::random(len, s) });
        total += len as u64;
        i += 1;
    }
    out
}

fn e1(variant: &str, a: &Args) {
    let (base, p) = match variant.strip_suffix("V3") {
        Some(b) => (b, Prealloc::Zeroed),
        None => (variant, Prealloc::Reserve),
    };
    let mode = Mode::parse(base).unwrap_or_else(|| panic!("unknown variant {variant}"));
    let kind = a.str("kind", "random");
    let sync = a.str("sync", "none");
    let mib = a.u64("mib", 64);
    let rep = a.u64("rep", 0);
    let objs = objects(&kind, mib, a.u64("seed", 1) + rep * 7919);
    let raw: u64 = objs.iter().map(|o| o.len() as u64).sum();
    let d = run_dir();
    let cfg = ArenaCfg { size: 64 * MIB, prealloc: p, granule: 1, trim_on_seal: false };
    let mut w = Writer::create(d.path(), 1, cfg, mode, 3).unwrap();
    let first_arena_ms = w.times.new_arena[0] * 1e3;
    w.times.new_arena.clear();

    let mut put_lat = Vec::with_capacity(objs.len());
    let mut sync_lat = Vec::new();
    let mut sync_use = Usage::default();
    let mut since = (0usize, 0u64);
    let span = Span::start();
    for o in &objs {
        let t = Instant::now();
        w.put(o).unwrap();
        put_lat.push(t.elapsed().as_secs_f64());
        since.0 += 1;
        since.1 += o.len() as u64;
        let due = match sync.as_str() {
            "n64" => since.0 >= 64,
            "mib1" => since.1 >= MIB,
            _ => false,
        };
        if due {
            let u0 = Usage::now();
            let t = Instant::now();
            w.sync().unwrap();
            sync_lat.push(t.elapsed().as_secs_f64());
            let u1 = Usage::now();
            sync_use.user_us += u1.user_us - u0.user_us;
            sync_use.sys_us += u1.sys_us - u0.sys_us;
            sync_use.minflt += u1.minflt - u0.minflt;
            sync_use.majflt += u1.majflt - u0.majflt;
            since = (0, 0);
        }
    }
    let m = span.end();
    let ms = |v: &[f64]| if v.is_empty() { f64::NAN } else { median(v) * 1e3 };
    let (msync_med, fullsync_med) = (ms(&w.times.msync), ms(&w.times.fullsync));
    let t = Instant::now();
    w.sync().unwrap();
    let final_sync_ms = t.elapsed().as_secs_f64() * 1e3;

    let n = put_lat.len() as f64;
    let put_user = m.user_s - sync_use.user_us as f64 / 1e6;
    let put_sys = m.sys_s - sync_use.sys_us as f64 / 1e6;
    let ns = sync_lat.len().max(1) as f64;
    let put_sum: f64 = put_lat.iter().sum();
    let mut r = Rec::new("e1", variant);
    r.s("cfg", &format!("kind={kind} sync={sync} mib={mib}"))
        .s("kind", &kind)
        .s("sync", &sync)
        .n("rep", rep as f64)
        .n("puts", n)
        .n("raw_mib", raw as f64 / MIB as f64)
        .n("stored_mib", w.stored_bytes as f64 / MIB as f64)
        .m(&m)
        .n("mib_per_s", raw as f64 / MIB as f64 / m.wall_s)
        .n("put_wall_us", put_sum / n * 1e6)
        .n("put_p50_us", pct(&mut put_lat.clone(), 50.0) * 1e6)
        .n("put_p99_us", pct(&mut put_lat, 99.0) * 1e6)
        .n("put_user_us", put_user / n * 1e6)
        .n("put_sys_us", put_sys / n * 1e6)
        .n("put_cpu_us", (put_user + put_sys) / n * 1e6)
        .n("put_minflt", (m.minflt - sync_use.minflt) as f64 / n)
        .n("put_majflt", (m.majflt - sync_use.majflt) as f64 / n)
        .n("syncs", sync_lat.len() as f64)
        .n("sync_p50_ms", ms(&sync_lat))
        .n("sync_p90_ms", if sync_lat.is_empty() { f64::NAN } else { pct(&mut sync_lat.clone(), 90.0) * 1e3 })
        .n("msync_p50_ms", msync_med)
        .n("fullsync_p50_ms", fullsync_med)
        .n("sync_cpu_us", (sync_use.user_us + sync_use.sys_us) as f64 / ns)
        .n("sync_minflt", sync_use.minflt as f64 / ns)
        .n("final_sync_ms", final_sync_ms)
        .n("arenas", w.arenas as f64)
        .n("first_arena_ms", first_arena_ms)
        .n("new_arena_ms", ms(&w.times.new_arena))
        .n("unmap_ms", ms(&w.times.unmap));
    r.emit();
}
