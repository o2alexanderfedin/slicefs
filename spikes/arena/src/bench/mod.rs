//! Bench frame. `bench suite <name> --variants A,B --reps N --out F [--k v ...]`
//! runs `bench one <name> --variant V --rep R [--k v ...]` in a child process
//! per run (see `measure::interleave`). Each spike adds its subcommands in
//! its own module (`a.rs`; B adds `b.rs`, C adds `c.rs`) and one line in
//! `run_one`.

pub mod a;
pub mod b;

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
    if variant.starts_with("RS") {
        let o = chunk_store::Options { pack_limit: size, ..Default::default() };
        return Box::new(chunk_store::ChunkStore::open(dir, o).unwrap());
    }
    Box::new(SpikeStore::open(dir, spike_opts(variant, size)).unwrap())
}

/// `V0`..`V3`, then flags: `P` (e.g. `V1P`) turns on `prepare_next`; `M`
/// reads through mmap (advice Default), `R` through mmap with MADV_RANDOM
/// (spike B). Other trailing letters are labels only (e.g. `V1b` for an
/// A/A noise run).
pub fn spike_opts(variant: &str, size: u64) -> Options {
    let base = variant.get(..2).unwrap_or(variant);
    let flags = variant.get(2..).unwrap_or("");
    let p = Prealloc::parse(base).unwrap_or_else(|| panic!("unknown variant {variant}"));
    let mut o = Options::with_prealloc(p);
    o.arena.size = size;
    o.prepare_next = flags.contains('P');
    if flags.contains('M') {
        o.read = crate::ReadMode::Mmap { advice: crate::Advice::Default };
    }
    if flags.contains('R') {
        o.read = crate::ReadMode::Mmap { advice: crate::Advice::Random };
    }
    o
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
    if a::run(bench, variant, args) {
        return true;
    }
    if b::run(bench, variant, args) {
        return true;
    }
    false
}
