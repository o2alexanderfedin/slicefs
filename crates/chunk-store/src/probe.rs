//! PROBE (temporary, slow-write investigation): global timing counters.
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

pub static PUT_N: AtomicU64 = AtomicU64::new(0);
pub static PUT_NEW: AtomicU64 = AtomicU64::new(0);
pub static PUT_NEW_BYTES: AtomicU64 = AtomicU64::new(0);
pub static PUT_NS: AtomicU64 = AtomicU64::new(0);
pub static HASH_NS: AtomicU64 = AtomicU64::new(0);
pub static ZSTD_NS: AtomicU64 = AtomicU64::new(0);
pub static LOCK_NS: AtomicU64 = AtomicU64::new(0);
pub static APPEND_NS: AtomicU64 = AtomicU64::new(0);
pub static WRITE_NS: AtomicU64 = AtomicU64::new(0);
pub static WRITE_MAX_NS: AtomicU64 = AtomicU64::new(0);
pub static SYNC_N: AtomicU64 = AtomicU64::new(0);
pub static SYNC_NS: AtomicU64 = AtomicU64::new(0);
pub static SYNC_MAX_NS: AtomicU64 = AtomicU64::new(0);
pub static SEAL_N: AtomicU64 = AtomicU64::new(0);
pub static SEAL_NS: AtomicU64 = AtomicU64::new(0);
pub static READ_N: AtomicU64 = AtomicU64::new(0);
pub static READ_BYTES: AtomicU64 = AtomicU64::new(0);
pub static READ_NS: AtomicU64 = AtomicU64::new(0);
pub static CDC_NS: AtomicU64 = AtomicU64::new(0);
pub static SRC_NS: AtomicU64 = AtomicU64::new(0);
pub static NODE_LOADS: AtomicU64 = AtomicU64::new(0);
pub static READ_MAX_NS: AtomicU64 = AtomicU64::new(0);
pub static PUT_MAX_NS: AtomicU64 = AtomicU64::new(0);

/// (on-CPU ns, run-queue wait ns) of this thread (Linux; zeros elsewhere).
pub fn sched() -> (u64, u64) {
    let s = std::fs::read_to_string("/proc/thread-self/schedstat").unwrap_or_default();
    let mut it = s.split_whitespace().map(|x| x.parse::<u64>().unwrap_or(0));
    (it.next().unwrap_or(0), it.next().unwrap_or(0))
}

pub fn sched_diff(before: (u64, u64)) -> String {
    let (c, w) = sched();
    format!(" oncpu_ms={:.1} runq_wait_ms={:.1}", (c - before.0) as f64 / 1e6, (w - before.1) as f64 / 1e6)
}

pub fn add(c: &AtomicU64, t: Instant) -> u64 {
    let ns = t.elapsed().as_nanos() as u64;
    c.fetch_add(ns, Relaxed);
    ns
}

pub fn max(c: &AtomicU64, ns: u64) {
    c.fetch_max(ns, Relaxed);
}

pub fn inc(c: &AtomicU64, n: u64) {
    c.fetch_add(n, Relaxed);
}

const ALL: [(&str, &AtomicU64); 23] = [
    ("put_n", &PUT_N), ("put_new", &PUT_NEW), ("put_new_mib", &PUT_NEW_BYTES), ("put_ms", &PUT_NS),
    ("hash_ms", &HASH_NS), ("zstd_ms", &ZSTD_NS), ("lock_ms", &LOCK_NS), ("append_ms", &APPEND_NS),
    ("write_ms", &WRITE_NS), ("write_max_ms", &WRITE_MAX_NS), ("sync_n", &SYNC_N), ("sync_ms", &SYNC_NS),
    ("sync_max_ms", &SYNC_MAX_NS), ("seal_n", &SEAL_N), ("seal_ms", &SEAL_NS), ("read_n", &READ_N),
    ("read_mib", &READ_BYTES), ("read_ms", &READ_NS), ("cdc_ms", &CDC_NS), ("src_ms", &SRC_NS),
    ("node_loads", &NODE_LOADS), ("read_max_ms", &READ_MAX_NS), ("put_max_ms", &PUT_MAX_NS),
];

pub fn snap() -> [u64; 23] {
    let mut out = [0; 23];
    for (i, (_, c)) in ALL.iter().enumerate() {
        out[i] = c.load(Relaxed);
    }
    out
}

/// The difference since `before` as text (ms for *_ms, MiB for *_mib; max fields absolute).
pub fn diff(before: &[u64; 23]) -> String {
    let now = snap();
    let mut s = String::new();
    for (i, (name, _)) in ALL.iter().enumerate() {
        let v = if name.ends_with("max_ms") { now[i] } else { now[i] - before[i] };
        let v = if name.ends_with("_ms") { format!("{:.1}", v as f64 / 1e6) }
            else if name.ends_with("_mib") { format!("{:.1}", v as f64 / 1048576.0) }
            else { v.to_string() };
        s.push_str(&format!(" {name}={v}"));
    }
    s
}

pub fn reset_max() {
    WRITE_MAX_NS.store(0, Relaxed);
    SYNC_MAX_NS.store(0, Relaxed);
    READ_MAX_NS.store(0, Relaxed);
    PUT_MAX_NS.store(0, Relaxed);
}
