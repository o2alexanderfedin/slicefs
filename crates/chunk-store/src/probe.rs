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

const ALL: [(&str, &AtomicU64); 21] = [
    ("put_n", &PUT_N), ("put_new", &PUT_NEW), ("put_new_mib", &PUT_NEW_BYTES), ("put_ms", &PUT_NS),
    ("hash_ms", &HASH_NS), ("zstd_ms", &ZSTD_NS), ("lock_ms", &LOCK_NS), ("append_ms", &APPEND_NS),
    ("write_ms", &WRITE_NS), ("write_max_ms", &WRITE_MAX_NS), ("sync_n", &SYNC_N), ("sync_ms", &SYNC_NS),
    ("sync_max_ms", &SYNC_MAX_NS), ("seal_n", &SEAL_N), ("seal_ms", &SEAL_NS), ("read_n", &READ_N),
    ("read_mib", &READ_BYTES), ("read_ms", &READ_NS), ("cdc_ms", &CDC_NS), ("src_ms", &SRC_NS),
    ("node_loads", &NODE_LOADS),
];

pub fn snap() -> [u64; 21] {
    let mut out = [0; 21];
    for (i, (_, c)) in ALL.iter().enumerate() {
        out[i] = c.load(Relaxed);
    }
    out
}

/// The difference since `before` as text (ms for *_ms, MiB for *_mib; max fields absolute).
pub fn diff(before: &[u64; 21]) -> String {
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
}
