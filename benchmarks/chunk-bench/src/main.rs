//! Compare FastCDC + BLAKE3 with blockset on real data (#17).
//!
//! ```text
//! chunk-bench <engine> <label=path>...
//!   engine: blockset | fastcdc:<avg KiB> (min = avg/4, max = avg*4) | fastcdc:<min>:<avg>:<max> (KiB)
//!   path:   a file (for example a tar) or a directory (read as a tree, sorted)
//! ```
//!
//! The inputs are one series: they share one dedup set, in the given order.
//! For each input the tool prints the logical bytes, the bytes that earlier
//! inputs did not already store, and the CPU time (user + system) of the
//! chunk + hash work only. File reads are not in the CPU time.
//! CPU time is used, not wall-clock time, because wall-clock time depends on
//! the other load on the machine.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

use blockset::{FileStorageAdd, Io, State, Tree};

/// User + system CPU seconds of this process.
fn cpu_secs() -> f64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let t = |tv: libc::timeval| tv.tv_sec as f64 + tv.tv_usec as f64 / 1e6;
    t(ru.ru_utime) + t(ru.ru_stime)
}

/// Every regular file under `path` (or `path` itself), sorted by path.
fn files(path: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut todo = vec![path.to_path_buf()];
    while let Some(p) = todo.pop() {
        let Ok(md) = std::fs::symlink_metadata(&p) else { continue };
        if md.is_dir() {
            if let Ok(rd) = std::fs::read_dir(&p) {
                todo.extend(rd.filter_map(|e| e.ok()).map(|e| e.path()));
            }
        } else if md.is_file() {
            out.push(p);
        }
    }
    out.sort();
    out
}

/// In-memory blockset store. It keeps object contents, because blockset reads
/// nodes back while it writes.
#[derive(Default)]
struct MemIo {
    objects: HashMap<String, Vec<u8>>,
    new_objects: u64,
    new_bytes: u64,
    new_sizes: Vec<u32>,
}

impl Io for MemIo {
    type Args = std::vec::IntoIter<String>;
    fn read(&mut self, name: &str) -> io::Result<Vec<u8>> {
        self.objects.get(name).cloned().ok_or_else(|| io::ErrorKind::NotFound.into())
    }
    fn write(&mut self, name: &str, data: &[u8]) -> io::Result<()> {
        if !self.objects.contains_key(name) {
            self.new_objects += 1;
            self.new_bytes += data.len() as u64;
            self.new_sizes.push(data.len() as u32);
            self.objects.insert(name.to_string(), data.to_vec());
        }
        Ok(())
    }
    fn args(&self) -> Self::Args {
        Vec::new().into_iter()
    }
    fn print(&mut self, _text: &str) {}
}

enum Engine {
    Blockset(MemIo),
    FastCdc { min: u32, avg: u32, max: u32, seen: HashSet<[u8; 32]>, new_bytes: u64, new_chunks: u64, refs: u64, sizes: Vec<u32> },
}

impl Engine {
    fn add(&mut self, data: &[u8]) {
        match self {
            Engine::Blockset(io) => {
                let mut fsa = FileStorageAdd::new(io);
                let _ = State::push_all(&mut fsa, data);
            }
            Engine::FastCdc { min, avg, max, seen, new_bytes, new_chunks, refs, sizes } => {
                for c in fastcdc::v2020::FastCDC::new(data, *min, *avg, *max) {
                    let bytes = &data[c.offset..c.offset + c.length];
                    *refs += 1;
                    sizes.push(c.length as u32);
                    if seen.insert(*blake3::hash(bytes).as_bytes()) {
                        *new_bytes += c.length as u64;
                        *new_chunks += 1;
                    }
                }
            }
        }
    }

    /// (new units, new stored bytes, size samples) since the last call.
    fn take(&mut self) -> (u64, u64, Vec<u32>) {
        match self {
            Engine::Blockset(io) => (
                std::mem::take(&mut io.new_objects),
                std::mem::take(&mut io.new_bytes),
                std::mem::take(&mut io.new_sizes),
            ),
            Engine::FastCdc { new_bytes, new_chunks, sizes, .. } => (
                std::mem::take(new_chunks),
                std::mem::take(new_bytes),
                std::mem::take(sizes),
            ),
        }
    }
}

fn dist(mut v: Vec<u32>) -> String {
    if v.is_empty() {
        return "-".into();
    }
    v.sort_unstable();
    let p = |q: f64| v[((v.len() - 1) as f64 * q) as usize];
    let mean = v.iter().map(|&x| x as u64).sum::<u64>() / v.len() as u64;
    format!("min {} p50 {} mean {} p90 {} max {}", v[0], p(0.5), mean, p(0.9), v[v.len() - 1])
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("usage: chunk-bench <blockset|fastcdc:AVG_KIB|fastcdc:MIN:AVG:MAX> <label=path>...");
        std::process::exit(2);
    }
    let mut engine = match args[0].as_str() {
        "blockset" => Engine::Blockset(MemIo::default()),
        s if s.starts_with("fastcdc:") => {
            let k: Vec<u32> = s["fastcdc:".len()..].split(':').map(|v| v.parse::<u32>().expect("KiB") * 1024).collect();
            let (min, avg, max) = match k[..] {
                [a] => (a / 4, a, a * 4),
                [mn, a, mx] => (mn, a, mx),
                _ => panic!("fastcdc:<avg> or fastcdc:<min>:<avg>:<max> (KiB)"),
            };
            Engine::FastCdc {
                min,
                avg,
                max,
                seen: HashSet::new(),
                new_bytes: 0,
                new_chunks: 0,
                refs: 0,
                sizes: Vec::new(),
            }
        }
        e => panic!("unknown engine {e}"),
    };

    println!("| engine | input | files | logical MB | new units | new stored MB | new / logical | CPU s | MB/s (CPU) | new unit sizes (bytes) |");
    println!("|---|---|---|---|---|---|---|---|---|---|");
    let (mut total_logical, mut total_stored) = (0u64, 0u64);
    for spec in &args[1..] {
        let (label, path) = spec.split_once('=').expect("label=path");
        let list = files(Path::new(path));
        let (mut logical, mut cpu) = (0u64, 0f64);
        for f in &list {
            let Ok(data) = std::fs::read(f) else { continue };
            logical += data.len() as u64;
            let t = cpu_secs();
            engine.add(&data);
            cpu += cpu_secs() - t;
        }
        let (units, stored, sizes) = engine.take();
        total_logical += logical;
        total_stored += stored;
        let mb = |b: u64| b as f64 / 1e6;
        println!(
            "| {} | {} | {} | {:.1} | {} | {:.1} | {:.3} | {:.1} | {:.1} | {} |",
            args[0], label, list.len(), mb(logical), units, mb(stored),
            stored as f64 / logical.max(1) as f64, cpu, mb(logical) / cpu.max(1e-9), dist(sizes)
        );
    }
    if let Engine::FastCdc { refs, .. } = &engine {
        println!("\nchunk references (manifest entries, 32 B each): {refs}");
    }
    println!(
        "\n{}: series total logical {:.1} MB, stored {:.1} MB, dedup ratio {:.2}x",
        args[0],
        total_logical as f64 / 1e6,
        total_stored as f64 / 1e6,
        total_logical as f64 / total_stored.max(1) as f64
    );
}
