//! prolly-bench: measure the prolly prototype (`crates/prolly`) against the
//! metadata commit path of SliceFS (#19, Phase 24). A by-hand tool, built
//! in release mode; `cargo test` runs only its smoke test.
//!
//! Each mode runs in its own process, so the peak RSS of a mode is its own:
//! `whole` commits on the real `DictMetadataStore`, `batches` extracts the
//! `inode_data` change of each commit, and `flat` (a copy of today's map
//! serializer) and `prolly` (the prototype) replay the changes, each in its
//! own scratch store. `scale` compares the two on synthetic maps. `maps`
//! extracts the change of all five metadata maps of each commit, and
//! `layout` replays them into prolly trees of one layout (Phase 25).
//!
//! Output: one line per record, the first word is the kind, then
//! `key=value` fields. Progress goes to stderr. A bad argument exits 2,
//! another failure exits 1.

mod layout;
mod replay;
mod whole;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use chunk_store::file::FileReader;
use chunk_store::{ChunkStore, Options, Stats};
use metadata::store_io::{get_value, to_id};
use slicefs_traits::key::ObjectKey;

const USAGE: &str = "usage: prolly-bench gen tree --dir D --files N --seed S [--max-bytes M] [--change-every K]
       prolly-bench seed --tree DIR --store S
       prolly-bench whole --store S --roots FILE [--seed N] [--ks \"1 10 100 1000\"] [--commits N] [--patterns \"scatter cluster insdel\"] [--next-tree DIR]
       prolly-bench batches --store S --roots FILE --out DIR
       prolly-bench flat --batches DIR --store F
       prolly-bench prolly --batches DIR --store P
       prolly-bench scale --mode <flat|prolly> --n N --store DIR [--seed S]
       prolly-bench maps --store S --roots FILE --out DIR
       prolly-bench layout --maps DIR --layout <a|b|c> --store P [--l L] [--node-max M]";

// Root record layout: a copy of `root_rec` in crates/metadata/src/store.rs
// (it is `pub(crate)`), with 33-byte keys.
/// Size of the root record.
pub(crate) const ROOT_REC_SIZE: usize = 197;
/// Offset of `next_ino` (u64 LE).
pub(crate) const RR_NEXT_INO: usize = 8;
/// Offset of the `inode_data` key.
pub(crate) const RR_INODE_DATA: usize = 16;
/// Offset of the `dir_data` key.
pub(crate) const RR_DIR_DATA: usize = 49;
/// Offset of the `manifest_data` key.
pub(crate) const RR_MANIFEST_DATA: usize = 82;
/// Offset of the `xattr_data` key.
pub(crate) const RR_XATTR_DATA: usize = 115;
/// Offset of the `refcounts` key.
pub(crate) const RR_REFCOUNTS: usize = 148;
/// Offset of `inode_count` (u64 LE).
pub(crate) const RR_INODE_COUNT: usize = 189;
/// Bytes of an `ObjectKey` (tag and digest).
pub(crate) const KEY_LEN: usize = 33;
/// One map entry: ino u64 LE and a key (`MAP_ENTRY` of store.rs).
pub(crate) const MAP_ENTRY: usize = 8 + KEY_LEN;
/// The chunk-store record bytes per object besides its payload: the
/// header (`ID_LEN + 9` = 42) and the CRC (4).
pub(crate) const REC_OVERHEAD_BYTES: u64 = 42 + 4;
/// Bytes of new content for a content change or a create.
pub(crate) const CONTENT_BYTES: usize = 4096;
/// Read size of the map streams and of file writes.
pub(crate) const PIECE: usize = 1 << 20;
/// Mask of the file type bits of a mode.
pub(crate) const S_IFMT: u32 = 0o170_000;
/// Type bits of a regular file.
pub(crate) const S_IFREG: u32 = 0o100_000;
/// Type bits of a directory.
pub(crate) const S_IFDIR: u32 = 0o040_000;

/// Why a subcommand stopped.
#[derive(Debug)]
pub(crate) enum Fail {
    /// A bad argument: print the usage, exit 2.
    Usage(String),
    /// A store or file failure: exit 1.
    Run(String),
}

impl From<chunk_store::Error> for Fail {
    fn from(e: chunk_store::Error) -> Self {
        Fail::Run(e.to_string())
    }
}

impl From<std::io::Error> for Fail {
    fn from(e: std::io::Error) -> Self {
        Fail::Run(e.to_string())
    }
}

impl From<slicefs_traits::metadata::MetaError> for Fail {
    fn from(e: slicefs_traits::metadata::MetaError) -> Self {
        Fail::Run(format!("metadata: {e}"))
    }
}

impl From<prolly::Error> for Fail {
    fn from(e: prolly::Error) -> Self {
        Fail::Run(e.to_string())
    }
}

pub(crate) type Res<T> = Result<T, Fail>;

pub(crate) fn usage<T>(msg: impl Into<String>) -> Res<T> {
    Err(Fail::Usage(msg.into()))
}

pub(crate) fn run_fail<T>(msg: impl Into<String>) -> Res<T> {
    Err(Fail::Run(msg.into()))
}

/// The xorshift64 of the tests: the state is `seed * 0x9E37_79B9_7F4A_7C15`, or 1.
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub(crate) fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    pub(crate) fn fill(&mut self, buf: &mut [u8]) {
        for b in buf {
            *b = self.next() as u8;
        }
    }

    /// Uniform in `[0, n]`.
    pub(crate) fn upto(&mut self, n: u64) -> u64 {
        match n.checked_add(1) {
            Some(m) => self.next() % m,
            None => self.next(),
        }
    }
}

/// Parsed arguments: `--name value` flags and positionals.
pub(crate) struct Args {
    flags: BTreeMap<String, String>,
    rest: Vec<String>,
}

impl Args {
    pub(crate) fn parse(args: &[String]) -> Res<Args> {
        let mut a = Args { flags: BTreeMap::new(), rest: Vec::new() };
        let mut i = 0;
        while i < args.len() {
            let s = &args[i];
            if let Some(name) = s.strip_prefix("--") {
                let Some(v) = args.get(i + 1) else { return usage(format!("--{name} needs a value")) };
                a.flags.insert(name.to_string(), v.clone());
                i += 1;
            } else {
                a.rest.push(s.clone());
            }
            i += 1;
        }
        Ok(a)
    }

    /// Refuses an unknown flag and any positional.
    pub(crate) fn check(&self, known: &[&str]) -> Res<()> {
        if let Some(r) = self.rest.first() {
            return usage(format!("unexpected argument {r}"));
        }
        match self.flags.keys().find(|k| !known.contains(&k.as_str())) {
            Some(k) => usage(format!("unknown option --{k}")),
            None => Ok(()),
        }
    }

    pub(crate) fn str(&self, name: &str) -> Res<&str> {
        match self.flags.get(name) {
            Some(v) => Ok(v),
            None => usage(format!("--{name} is required")),
        }
    }

    pub(crate) fn opt(&self, name: &str) -> Option<&str> {
        self.flags.get(name).map(String::as_str)
    }

    pub(crate) fn path(&self, name: &str) -> Res<PathBuf> {
        self.str(name).map(PathBuf::from)
    }

    pub(crate) fn num(&self, name: &str, default: Option<u64>) -> Res<u64> {
        match (self.flags.get(name), default) {
            (Some(v), _) => match v.parse::<u64>() {
                Ok(n) => Ok(n),
                Err(_) => usage(format!("--{name}: not a number: {v}")),
            },
            (None, Some(d)) => Ok(d),
            (None, None) => usage(format!("--{name} is required")),
        }
    }

    /// A space-separated list of numbers.
    pub(crate) fn nums(&self, name: &str, default: &str) -> Res<Vec<u64>> {
        let s = self.opt(name).unwrap_or(default);
        let v: Option<Vec<u64>> = s.split_whitespace().map(|w| w.parse().ok()).collect();
        match v {
            Some(v) if !v.is_empty() => Ok(v),
            _ => usage(format!("--{name}: want numbers, got {s:?}")),
        }
    }

    /// A space-separated list of words.
    pub(crate) fn words(&self, name: &str, default: &str) -> Vec<String> {
        self.opt(name).unwrap_or(default).split_whitespace().map(str::to_string).collect()
    }
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let rest = argv.get(1..).unwrap_or(&[]);
    let res = match argv.first().map(String::as_str) {
        Some("gen") => gen(rest),
        Some("seed") => seed(rest),
        Some("whole") => whole::run(rest),
        Some("batches") => replay::batches(rest),
        Some("flat") => replay::flat(rest),
        Some("prolly") => replay::prolly(rest),
        Some("scale") => replay::scale(rest),
        Some("maps") => layout::maps(rest),
        Some("layout") => layout::layout(rest),
        Some(s) => usage(format!("unknown subcommand {s}")),
        None => usage("no subcommand"),
    };
    match res {
        Ok(()) => {}
        Err(Fail::Usage(m)) => {
            eprintln!("prolly-bench: {m}\n{USAGE}");
            std::process::exit(2);
        }
        Err(Fail::Run(m)) => {
            eprintln!("prolly-bench: {m}");
            std::process::exit(1);
        }
    }
}

// ── gen and seed ─────────────────────────────────────────────────────────────

fn gen(argv: &[String]) -> Res<()> {
    match argv.first().map(String::as_str) {
        Some("tree") => gen_tree(&Args::parse(&argv[1..])?),
        _ => usage("gen: want tree"),
    }
}

/// N files in 16 subdirectories (a copy of `gen tree` of
/// `crates/chunk-store/examples/chunk_profile_bench.rs`). Each file has its
/// own generator, made from the seed and the file number, so a change in
/// one file does not move the bytes of the others. With `--change-every K`,
/// every K-th file gets other bytes in 1 KiB at its middle.
fn gen_tree(a: &Args) -> Res<()> {
    a.check(&["dir", "files", "seed", "max-bytes", "change-every"])?;
    let dir = a.path("dir")?;
    let files = a.num("files", None)?;
    let seed = a.num("seed", None)?;
    let max_bytes = a.num("max-bytes", Some(4096))?;
    let change = a.num("change-every", Some(0))?;
    if max_bytes > PIECE as u64 {
        return usage(format!("--max-bytes: at most {PIECE}"));
    }
    let mut sizes = Rng::new(seed);
    let mut total = 0u64;
    for i in 0..files {
        let len = sizes.upto(max_bytes);
        let sub = dir.join(format!("d{:02}", i % 16));
        fs::create_dir_all(&sub)?;
        let mut data = vec![0u8; len as usize];
        Rng::new(seed ^ (i + 1).wrapping_mul(0xD6E8_FEB8_6659_FD93)).fill(&mut data);
        if change > 0 && (i + 1) % change == 0 && len > 0 {
            let from = (len / 2) as usize;
            let to = (from + 1024).min(len as usize);
            Rng::new(!seed ^ (i + 1)).fill(&mut data[from..to]);
        }
        fs::write(sub.join(format!("f{i:06}.bin")), &data)?;
        total += len;
    }
    println!("gen files={files} bytes={total}");
    Ok(())
}

/// Seeds a new store from a tree with the product seed (default hash and
/// chunk profile).
fn seed(argv: &[String]) -> Res<()> {
    let a = Args::parse(argv)?;
    a.check(&["tree", "store"])?;
    let tree = a.path("tree")?;
    let store = a.path("store")?;
    let t = Instant::now();
    slicefs_cli::seed::run_seed_with(&store, &tree, None, None).map_err(|e| Fail::Run(format!("seed: {e}")))?;
    let secs = t.elapsed().as_secs_f64();
    let root = segment_root(&store)?;
    let cs = ChunkStore::open(&store, Options { read_only: true, ..Options::default() })?;
    let rr = root_record(&cs, &root)?;
    println!("seed secs={secs:.3} inode_count={}", rr.inode_count);
    Ok(())
}

// ── store helpers ────────────────────────────────────────────────────────────

/// The last root of the segments of `store`.
pub(crate) fn segment_root(store: &Path) -> Res<ObjectKey> {
    let (root, _) = metadata::segment::load_store_from_segments(&store.join("segments"))
        .map_err(|e| Fail::Run(format!("segments: {e}")))?;
    root.ok_or_else(|| Fail::Run("the store has no root".into()))
}

/// The fields of a root record that the bench reads.
pub(crate) struct RootRec {
    /// The keys of the five maps: inode_data, dir_data, manifest_data,
    /// xattr_data, refcounts.
    pub(crate) maps: [ObjectKey; 5],
    pub(crate) next_ino: u64,
    pub(crate) inode_count: u64,
}

impl RootRec {
    pub(crate) fn inode_data(&self) -> ObjectKey {
        self.maps[0]
    }
}

pub(crate) fn root_record(cs: &ChunkStore, root: &ObjectKey) -> Res<RootRec> {
    let b = get_value(cs, root)?;
    if b.len() != ROOT_REC_SIZE {
        return run_fail(format!("root record of {} bytes, want {ROOT_REC_SIZE}", b.len()));
    }
    let u64_at = |at: usize| u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"));
    let maps = [RR_INODE_DATA, RR_DIR_DATA, RR_MANIFEST_DATA, RR_XATTR_DATA, RR_REFCOUNTS]
        .map(|at| ObjectKey::read(&b, at));
    Ok(RootRec { maps, next_ino: u64_at(RR_NEXT_INO), inode_count: u64_at(RR_INODE_COUNT) })
}

/// The byte length of a value: reads only its top manifest node.
pub(crate) fn map_len(cs: &ChunkStore, key: &ObjectKey) -> Res<u64> {
    Ok(FileReader::open(cs, to_id(key))?.len())
}

/// The serialized bytes of a commit: the five maps and the root record.
pub(crate) fn serialized_bytes(cs: &ChunkStore, rr: &RootRec) -> Res<u64> {
    let mut n = ROOT_REC_SIZE as u64;
    for k in &rr.maps {
        n += map_len(cs, k)?;
    }
    Ok(n)
}

/// The `new_*` fields of a `stats()` delta; record bytes count the record
/// header and CRC of each new object.
pub(crate) fn stats_delta(before: &Stats, after: &Stats) -> String {
    let objects = after.objects.saturating_sub(before.objects);
    let raw = after.raw_bytes.saturating_sub(before.raw_bytes);
    let stored = after.stored_bytes.saturating_sub(before.stored_bytes);
    let record = stored + REC_OVERHEAD_BYTES * objects;
    format!("new_objects={objects} new_raw_bytes={raw} new_stored_bytes={stored} new_record_bytes={record}")
}

/// Lowercase hex of bytes.
pub(crate) fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A key from its hex.
pub(crate) fn key_of_hex(s: &str) -> Res<ObjectKey> {
    let b = s.as_bytes();
    if b.len() != 2 * KEY_LEN {
        return run_fail(format!("bad key hex {s}"));
    }
    let mut k = [0u8; KEY_LEN];
    for (i, out) in k.iter_mut().enumerate() {
        let h = std::str::from_utf8(&b[2 * i..2 * i + 2]).ok().and_then(|p| u8::from_str_radix(p, 16).ok());
        *out = h.ok_or_else(|| Fail::Run(format!("bad key hex {s}")))?;
    }
    Ok(ObjectKey(k))
}

// ── measurement helpers ──────────────────────────────────────────────────────

/// CPU and peak memory of this process so far.
#[derive(Clone, Copy)]
pub(crate) struct Usage {
    pub(crate) user_s: f64,
    pub(crate) sys_s: f64,
    pub(crate) maxrss_bytes: u64,
}

/// `getrusage(RUSAGE_SELF)`: user and system CPU seconds apart, and the
/// peak RSS in bytes (`ru_maxrss` is bytes on macOS and KiB on Linux).
/// The only FFI of the bench.
pub(crate) fn rusage() -> Usage {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let t = |tv: libc::timeval| tv.tv_sec as f64 + tv.tv_usec as f64 / 1e6;
    let unit = if cfg!(target_os = "macos") { 1 } else { 1024 };
    Usage { user_s: t(ru.ru_utime), sys_s: t(ru.ru_stime), maxrss_bytes: (ru.ru_maxrss.max(0) as u64) * unit }
}

/// The CPU and RSS fields of a timed step.
pub(crate) fn usage_fields(before: &Usage, after: &Usage) -> String {
    format!(
        "cpu_user_s={:.6} cpu_sys_s={:.6} maxrss_bytes={}",
        after.user_s - before.user_s,
        after.sys_s - before.sys_s,
        after.maxrss_bytes
    )
}

/// Every regular file under `path`, sorted by path. Symlinks are skipped.
fn files_of(path: &Path) -> Res<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut todo = vec![path.to_path_buf()];
    while let Some(p) = todo.pop() {
        let md = fs::symlink_metadata(&p)?;
        if md.is_dir() {
            for e in fs::read_dir(&p)? {
                todo.push(e?.path());
            }
        } else if md.is_file() {
            out.push(p);
        }
    }
    out.sort();
    Ok(out)
}

/// Prints the on-disk size of a store dir: apparent bytes, allocated bytes
/// and files (before any GC: dead objects are counted).
pub(crate) fn print_disk(mode: &str, phase: &str, dir: &Path) -> Res<()> {
    let (mut apparent, mut alloc, mut files) = (0u64, 0u64, 0u64);
    for p in files_of(dir)? {
        let md = fs::symlink_metadata(&p)?;
        apparent += md.len();
        alloc += md.blocks() * 512;
        files += 1;
    }
    println!("disk mode={mode} phase={phase} apparent_bytes={apparent} alloc_bytes={alloc} files={files}");
    Ok(())
}

/// Milliseconds since `t`.
pub(crate) fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}
