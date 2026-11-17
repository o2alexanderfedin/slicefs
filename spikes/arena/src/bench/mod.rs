//! Bench frame. `bench suite <name> --variants A,B --reps N --out F [--k v ...]`
//! runs `bench one <name> --variant V --rep R [--k v ...]` in a child process
//! per run (see `measure::interleave`). Each spike adds its subcommands in
//! its own module (`a.rs`; B adds `b.rs`, C adds `c.rs`) and one line in
//! `run_one`.

pub mod a;
pub mod c;

use crate::file_fork::ObjStore;
use crate::{Options, Prealloc, SpikeStore};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// `--key value` arguments.
#[derive(Clone, Debug, Default)]
pub struct Args(pub HashMap<String, String>);

impl Args {
    pub fn parse(a: &[String]) -> Args {
        let mut m = HashMap::new();
        let mut i = 0;
        while i < a.len() {
            if let Some(k) = a[i].strip_prefix("--") {
                let v = a.get(i + 1).filter(|v| !v.starts_with("--")).cloned();
                i += if v.is_some() { 2 } else { 1 };
                m.insert(k.to_string(), v.unwrap_or_else(|| "1".into()));
            } else {
                i += 1;
            }
        }
        Args(m)
    }

    pub fn str(&self, k: &str, d: &str) -> String {
        self.0.get(k).cloned().unwrap_or_else(|| d.to_string())
    }

    pub fn u64(&self, k: &str, d: u64) -> u64 {
        self.0.get(k).and_then(|v| v.parse().ok()).unwrap_or(d)
    }

    pub fn f64(&self, k: &str, d: f64) -> f64 {
        self.0.get(k).and_then(|v| v.parse().ok()).unwrap_or(d)
    }
}

/// A fresh directory for one run, on the same volume as the temp dir (or
/// `ARENA_BENCH_DIR`).
pub fn run_dir() -> tempfile::TempDir {
    match std::env::var("ARENA_BENCH_DIR") {
        Ok(d) => {
            std::fs::create_dir_all(&d).unwrap();
            tempfile::TempDir::new_in(d).unwrap()
        }
        Err(_) => tempfile::TempDir::new().unwrap(),
    }
}

/// Open the store of `variant`: `RS` = the real `chunk_store::ChunkStore`,
/// `V0`..`V3` = `SpikeStore` with that preallocation. `size` = pack limit /
/// arena size.
pub fn open_store(variant: &str, dir: &Path, size: u64) -> Box<dyn ObjStore> {
    if variant == "RS" {
        let o = chunk_store::Options { pack_limit: size, ..Default::default() };
        return Box::new(chunk_store::ChunkStore::open(dir, o).unwrap());
    }
    if let Some(o) = reuse_opts(variant, size) {
        return Box::new(crate::ReuseStore::open(dir, o).unwrap());
    }
    Box::new(SpikeStore::open(dir, spike_opts(variant, size)).unwrap())
}

/// `V0`..`V3`; a trailing `P` (e.g. `V1P`) turns on `prepare_next`.
pub fn spike_opts(variant: &str, size: u64) -> Options {
    let (base, prep) = match variant.strip_suffix('P') {
        Some(b) => (b, true),
        None => (variant, false),
    };
    let p = Prealloc::parse(base).unwrap_or_else(|| panic!("unknown variant {variant}"));
    let mut o = Options::with_prealloc(p);
    o.arena.size = size;
    o.prepare_next = prep;
    o
}

/// Reuse-mode options (spike C): `C<k>` = reuse with allocator V<k> at
/// granule 64 on V1 arenas, then any of: `g<G>` granule, `d<K>` keep up to
/// K arenas dirty (`max_dirty`), `e` evacuation below 10 % live (always on
/// for V3), `s` recovery scan skips holes (`SEEK_DATA`). E.g. `C4d4`.
pub fn reuse_opts(variant: &str, size: u64) -> Option<Options> {
    let rest = variant.strip_prefix('C')?;
    let b = rest.as_bytes();
    let kind = crate::AllocKind::parse(&format!("V{}", *b.first()? as char))?;
    let mut cfg = crate::ReuseCfg { alloc: kind, ..Default::default() };
    let mut g = 64u32;
    let mut i = 1;
    let num = |i: &mut usize| -> Option<u64> {
        let st = *i;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
        rest[st..*i].parse().ok()
    };
    while i < b.len() {
        let c = b[i];
        i += 1;
        match c {
            b'g' => g = num(&mut i)? as u32,
            b'd' => cfg.max_dirty = num(&mut i)? as usize,
            b'e' => cfg.evacuate_below = Some(0.10),
            b's' => cfg.seek_data = true,
            _ => return None,
        }
    }
    if kind == crate::AllocKind::Hybrid {
        cfg.evacuate_below = Some(0.10);
    }
    let mut o = Options::with_prealloc(Prealloc::Sparse);
    o.arena.size = size;
    o.arena.granule = g;
    o.reuse = Some(cfg);
    Some(o)
}

/// The arena / pack files of a store directory.
pub fn data_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir.join("packs"))
        .map(|r| {
            r.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "arena" || x == "pack"))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// Run one bench in this process. Returns false for an unknown bench.
pub fn run_one(bench: &str, variant: &str, args: &Args) -> bool {
    if a::run(bench, variant, args) || c::run(bench, variant, args) {
        return true;
    }
    false
}
