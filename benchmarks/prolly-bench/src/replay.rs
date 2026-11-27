//! The `scale` mode: flat against prolly on a synthetic map of N entries,
//! with no product store. The modes that read the flat metadata maps of a
//! product store (`batches`, `flat`, `prolly`) are retired; tag `v2.3.0`
//! keeps them.

use std::collections::BTreeMap;
use std::time::Instant;

use chunk_store::{ChunkStore, Options, Stats};
use metadata::store_io::put_value;
use prolly::{Config, Edit};
use slicefs_traits::key::ObjectKey;

use crate::{ms, rusage, usage, usage_fields, Args, Res, Rng, Usage, KEY_LEN, MAP_ENTRY};

/// Entries per piece of the prolly build that is timed in pieces: the input
/// of a piece is made outside the timer, as flat's input is.
const BUILD_PIECE_ENTRIES: usize = 16_384;

/// Wall time, usage and store stats around one timed step.
pub(crate) struct Measured {
    pub(crate) ms: f64,
    pub(crate) u0: Usage,
    pub(crate) u1: Usage,
    pub(crate) s0: Stats,
    pub(crate) s1: Stats,
}

pub(crate) fn measure<T>(cs: &ChunkStore, f: impl FnOnce() -> Res<T>) -> Res<(T, Measured)> {
    let s0 = cs.stats();
    let u0 = rusage();
    let t = Instant::now();
    let out = f()?;
    let ms = ms(t);
    let u1 = rusage();
    let s1 = cs.stats();
    Ok((out, Measured { ms, u0, u1, s0, s1 }))
}

/// A step timed in pieces: wall and CPU time add up over the timed pieces
/// only, so the work between them (reading or making the input) is not in
/// the numbers. The store stats are taken at the start and at the end.
struct Pieces {
    ms: f64,
    user_s: f64,
    sys_s: f64,
    s0: Stats,
}

impl Pieces {
    fn new(cs: &ChunkStore) -> Self {
        Pieces { ms: 0.0, user_s: 0.0, sys_s: 0.0, s0: cs.stats() }
    }

    /// Runs and times one piece.
    fn time<T>(&mut self, f: impl FnOnce() -> Res<T>) -> Res<T> {
        let u0 = rusage();
        let t = Instant::now();
        let out = f()?;
        self.ms += ms(t);
        let u1 = rusage();
        self.user_s += u1.user_s - u0.user_s;
        self.sys_s += u1.sys_s - u0.sys_s;
        Ok(out)
    }

    /// The sums as one `Measured` (the RSS is the high-water mark now).
    fn done(self, cs: &ChunkStore) -> Measured {
        let now = rusage();
        Measured {
            ms: self.ms,
            u0: Usage { user_s: 0.0, sys_s: 0.0, maxrss_bytes: 0 },
            u1: Usage { user_s: self.user_s, sys_s: self.sys_s, maxrss_bytes: now.maxrss_bytes },
            s0: self.s0,
            s1: cs.stats(),
        }
    }
}

// ── flat map ─────────────────────────────────────────────────────────────────

/// A copy of `intern_u64_digest_map` and `NodeWriter::put_node` of
/// `crates/metadata/src/store.rs` (private there; copied so that `metadata`
/// is not changed, D-01, D-22): the count u64 LE, then ino u64 LE and the
/// key per entry in ino order, stored as one value through
/// `store_io::put_value` (FastCDC chunks under a manifest tree). Returns
/// the key of the value and its byte length.
fn intern_flat(cs: &ChunkStore, map: &BTreeMap<u64, [u8; KEY_LEN]>) -> Res<(ObjectKey, u64)> {
    let mut bytes = Vec::with_capacity(8 + map.len() * MAP_ENTRY);
    bytes.extend_from_slice(&(map.len() as u64).to_le_bytes());
    for (ino, key) in map {
        bytes.extend_from_slice(&ino.to_le_bytes());
        bytes.extend_from_slice(key);
    }
    let key = put_value(cs, &bytes)?;
    Ok((key, bytes.len() as u64))
}

/// The key of the prolly map: the ino big-endian, so the byte order is
/// the ino order.
fn pkey(ino: u64) -> [u8; 8] {
    ino.to_be_bytes()
}

// ── scale ────────────────────────────────────────────────────────────────────

/// One synthetic map value: a 33-byte key with the tag 0x1e.
fn synth_value(rng: &mut Rng) -> [u8; KEY_LEN] {
    let mut v = [0u8; KEY_LEN];
    rng.fill(&mut v);
    v[0] = 0x1e;
    v
}

/// Flat against prolly on a synthetic map of N entries (ino 2 to N + 1):
/// a full build, then one changed middle value. Each op ends with a sync.
pub(crate) fn scale(argv: &[String]) -> Res<()> {
    let a = Args::parse(argv)?;
    a.check(&["mode", "n", "store", "seed"])?;
    let mode = a.str("mode")?.to_string();
    let n = a.num("n", None)?;
    let store = a.path("store")?;
    let mut rng = Rng::new(a.num("seed", Some(24))?);
    if n == 0 {
        return usage("--n: at least 1");
    }
    let cs = ChunkStore::open(&store, Options::default())?;
    let mid = 2 + n / 2;
    let line = |op: &str, ser: u64, m: &Measured| {
        println!(
            "scale mode={mode} n={n} op={op} ms={:.3} {} serialized_bytes={ser} new_objects={} new_stored_bytes={}",
            m.ms,
            usage_fields(&m.u0, &m.u1),
            m.s1.objects.saturating_sub(m.s0.objects),
            m.s1.stored_bytes.saturating_sub(m.s0.stored_bytes)
        );
    };
    match mode.as_str() {
        "flat" => {
            let mut map: BTreeMap<u64, [u8; KEY_LEN]> = (2..n + 2).map(|ino| (ino, synth_value(&mut rng))).collect();
            let (ser, m) = measure(&cs, || {
                let (_, ser) = intern_flat(&cs, &map)?;
                cs.sync()?;
                Ok(ser)
            })?;
            line("build", ser, &m);
            map.insert(mid, synth_value(&mut rng));
            let (ser, m) = measure(&cs, || {
                let (_, ser) = intern_flat(&cs, &map)?;
                cs.sync()?;
                Ok(ser)
            })?;
            line("apply1", ser, &m);
        }
        "prolly" => {
            let cfg = Config::DEFAULT;
            // Timed in pieces: the values of a piece are made outside the
            // timer, as flat makes its map before its timed intern; the same
            // RNG order as flat, so both get the same values.
            let mut tm = Pieces::new(&cs);
            let mut b = prolly::Builder::new(&cs, cfg);
            let mut vals: Vec<[u8; KEY_LEN]> = Vec::with_capacity(BUILD_PIECE_ENTRIES);
            let mut from = 2;
            while from < n + 2 {
                let to = (from + BUILD_PIECE_ENTRIES as u64).min(n + 2);
                vals.clear();
                vals.extend((from..to).map(|_| synth_value(&mut rng)));
                tm.time(|| {
                    for (ino, v) in (from..to).zip(&vals) {
                        b.push(&pkey(ino), v)?;
                    }
                    Ok(())
                })?;
                from = to;
            }
            drop(vals);
            let (root, st) = tm.time(|| {
                let (root, st) = b.finish()?;
                cs.sync()?;
                Ok((root, st))
            })?;
            line("build", st.bytes_written, &tm.done(&cs));
            let edit = [Edit::Put { key: pkey(mid).to_vec(), value: synth_value(&mut rng).to_vec() }];
            let (ser, m) = measure(&cs, || {
                let (_, st) = prolly::apply(&cs, cfg, &root, &edit)?;
                cs.sync()?;
                Ok(st.bytes_written)
            })?;
            line("apply1", ser, &m);
        }
        other => return usage(format!("--mode: want flat or prolly, got {other}")),
    }
    cs.close()?;
    Ok(())
}
