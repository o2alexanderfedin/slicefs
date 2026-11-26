//! Benchmark of the chunk profiles (#67, PROF-03), in process on the chunk
//! store API: no mount. `scripts/chunk-profiles/bench.sh` drives it; see
//! `scripts/chunk-profiles/README.md`. It runs by hand, never in `cargo test`.
//!
//! ```text
//! chunk_profile_bench gen tree --dir D --files N --seed S [--max-bytes M] [--change-every K]
//! chunk_profile_bench gen file --path F --bytes N --seed S [--sparse]
//! chunk_profile_bench ingest --profile MIN/AVG/MAX --store DIR INPUT...
//! chunk_profile_bench open --profile MIN/AVG/MAX --store DIR
//! chunk_profile_bench probe --profile MIN/AVG/MAX --store DIR --key HEX [--seq-bytes N]
//!     [--reads N] [--seed N] [--edit-bytes N] [--edit-size N]
//! ```
//!
//! The profile is in whole KiB. Each subcommand prints its result lines to
//! stdout: the subcommand word, then `key=value` fields. Progress goes to
//! stderr. A bad argument exits 2, a store or file failure exits 1.
//!
//! Memory is bounded (D-03): files stream in 1 MiB pieces, and an edit reads
//! its base through a `FileReader`, with no copy of a whole file.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::ops::Range;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use chunk_store::file::{splice, FileReader, FileWriter, SpliceSource};
use chunk_store::{ChunkProfile, ChunkStore, ObjectId, Options, ID_LEN};

/// The size of a file read and of a writer feed.
const PIECE: usize = 1 << 20;
/// The size of one timed read.
const READ: usize = 4096;
/// Bytes compared on each side of an edit.
const CHECK_SIDE: u64 = 32 << 10;

const USAGE: &str = "usage: chunk_profile_bench gen tree --dir D --files N --seed S [--max-bytes M] [--change-every K]
       chunk_profile_bench gen file --path F --bytes N --seed S [--sparse]
       chunk_profile_bench ingest --profile MIN/AVG/MAX --store DIR INPUT...
       chunk_profile_bench open --profile MIN/AVG/MAX --store DIR
       chunk_profile_bench probe --profile MIN/AVG/MAX --store DIR --key HEX [--seq-bytes N] [--reads N] [--seed N] [--edit-bytes N] [--edit-size N]
(profile sizes in KiB, other sizes in bytes)";

/// Why a subcommand stopped.
enum Fail {
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

type Res<T> = Result<T, Fail>;

fn usage<T>(msg: impl Into<String>) -> Res<T> {
    Err(Fail::Usage(msg.into()))
}

fn run_fail<T>(msg: impl Into<String>) -> Res<T> {
    Err(Fail::Run(msg.into()))
}

/// The xorshift64 of the tests: the state is `seed * 0x9E37_79B9_7F4A_7C15`, or 1.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn fill(&mut self, buf: &mut [u8]) {
        for b in buf {
            *b = self.next() as u8;
        }
    }

    /// Uniform in `[0, n]`.
    fn upto(&mut self, n: u64) -> u64 {
        match n.checked_add(1) {
            Some(m) => self.next() % m,
            None => self.next(),
        }
    }
}

/// Parsed arguments: `--name value` flags, `--name` switches and positionals.
struct Args {
    flags: BTreeMap<String, String>,
    switches: Vec<String>,
    rest: Vec<String>,
}

impl Args {
    fn parse(args: &[String], switch_names: &[&str]) -> Res<Args> {
        let mut a = Args { flags: BTreeMap::new(), switches: Vec::new(), rest: Vec::new() };
        let mut i = 0;
        while i < args.len() {
            let s = &args[i];
            if let Some(name) = s.strip_prefix("--") {
                if switch_names.contains(&name) {
                    a.switches.push(name.to_string());
                } else {
                    let Some(v) = args.get(i + 1) else { return usage(format!("--{name} needs a value")) };
                    a.flags.insert(name.to_string(), v.clone());
                    i += 1;
                }
            } else {
                a.rest.push(s.clone());
            }
            i += 1;
        }
        Ok(a)
    }

    fn check(&self, known: &[&str]) -> Res<()> {
        match self.flags.keys().find(|k| !known.contains(&k.as_str())) {
            Some(k) => usage(format!("unknown option --{k}")),
            None => Ok(()),
        }
    }

    fn str(&self, name: &str) -> Res<&str> {
        match self.flags.get(name) {
            Some(v) => Ok(v),
            None => usage(format!("--{name} is required")),
        }
    }

    fn num(&self, name: &str, default: Option<u64>) -> Res<u64> {
        match (self.flags.get(name), default) {
            (Some(v), _) => match v.parse::<u64>() {
                Ok(n) => Ok(n),
                Err(_) => usage(format!("--{name}: not a number: {v}")),
            },
            (None, Some(d)) => Ok(d),
            (None, None) => usage(format!("--{name} is required")),
        }
    }

    fn has(&self, name: &str) -> bool {
        self.switches.iter().any(|s| s == name)
    }

    fn profile(&self) -> Res<ChunkProfile> {
        let p = self.str("profile")?;
        let kib: Vec<u32> = p.split('/').map(|s| s.parse::<u32>()).collect::<Result<_, _>>().unwrap_or_default();
        let [min, avg, max] = kib[..] else { return usage(format!("--profile: want MIN/AVG/MAX in KiB, got {p}")) };
        let bytes = |k: u32| k.checked_mul(1024);
        let (Some(min), Some(avg), Some(max)) = (bytes(min), bytes(avg), bytes(max)) else {
            return usage(format!("--profile: too large: {p}"));
        };
        ChunkProfile::new(min, avg, max).or_else(|reason| usage(format!("--profile {p} KiB: {reason}")))
    }
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let res = match argv.first().map(String::as_str) {
        Some("gen") => gen(&argv[1..]),
        Some("ingest") => ingest(&argv[1..]),
        Some("open") => open(&argv[1..]),
        Some("probe") => probe(&argv[1..]),
        _ => usage("no subcommand"),
    };
    match res {
        Ok(()) => {}
        Err(Fail::Usage(m)) => {
            eprintln!("chunk_profile_bench: {m}\n{USAGE}");
            std::process::exit(2);
        }
        Err(Fail::Run(m)) => {
            eprintln!("chunk_profile_bench: {m}");
            std::process::exit(1);
        }
    }
}

// ── gen ──────────────────────────────────────────────────────────────────────

fn gen(argv: &[String]) -> Res<()> {
    match argv.first().map(String::as_str) {
        Some("tree") => gen_tree(&Args::parse(&argv[1..], &[])?),
        Some("file") => gen_file(&Args::parse(&argv[1..], &["sparse"])?),
        _ => usage("gen: want tree or file"),
    }
}

/// Write `n` bytes of `rng` to `f` in pieces.
fn write_random(f: &mut fs::File, rng: &mut Rng, mut n: u64) -> Res<()> {
    let mut buf = vec![0u8; PIECE];
    while n > 0 {
        let k = n.min(PIECE as u64) as usize;
        rng.fill(&mut buf[..k]);
        f.write_all(&buf[..k])?;
        n -= k as u64;
    }
    Ok(())
}

/// N files in 16 subdirectories. Each file has its own generator, made from
/// the seed and the file number, so a change in one file does not move the
/// bytes of the others. With `--change-every K`, every K-th file gets other
/// bytes in 1 KiB at its middle (a second version that shares most content).
fn gen_tree(a: &Args) -> Res<()> {
    a.check(&["dir", "files", "seed", "max-bytes", "change-every"])?;
    let dir = PathBuf::from(a.str("dir")?);
    let files = a.num("files", None)?;
    let seed = a.num("seed", None)?;
    let max_bytes = a.num("max-bytes", Some(262_144))?;
    let change = a.num("change-every", Some(0))?;
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

/// A random file, or with `--sparse` a file of N bytes made by `set_len`
/// with 64 KiB of random bytes at every 16 MiB.
fn gen_file(a: &Args) -> Res<()> {
    a.check(&["path", "bytes", "seed"])?;
    let path = PathBuf::from(a.str("path")?);
    let bytes = a.num("bytes", None)?;
    let mut rng = Rng::new(a.num("seed", None)?);
    if let Some(p) = path.parent() {
        fs::create_dir_all(p)?;
    }
    let mut f = fs::File::create(&path)?;
    if a.has("sparse") {
        f.set_len(bytes)?;
        let mut at = 0u64;
        while at < bytes {
            let n = (bytes - at).min(64 << 10);
            std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(at))?;
            write_random(&mut f, &mut rng, n)?;
            at += 16 << 20;
        }
    } else {
        write_random(&mut f, &mut rng, bytes)?;
    }
    f.sync_all()?;
    println!("gen files=1 bytes={bytes}");
    Ok(())
}

// ── ingest ───────────────────────────────────────────────────────────────────

/// Every regular file under `path` (or `path` itself), sorted by path.
/// Symlinks are skipped.
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

fn parse_key(s: &str) -> Res<ObjectId> {
    let b = s.as_bytes();
    if b.len() != 2 * ID_LEN {
        return usage(format!("--key: want {} hex characters, got {}", 2 * ID_LEN, b.len()));
    }
    let nib = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut id = [0u8; ID_LEN];
    for (i, out) in id.iter_mut().enumerate() {
        match (nib(b[2 * i]), nib(b[2 * i + 1])) {
            (Some(h), Some(l)) => *out = h << 4 | l,
            _ => return usage("--key: not hex"),
        }
    }
    Ok(ObjectId(id))
}

fn ingest(argv: &[String]) -> Res<()> {
    let a = Args::parse(argv, &[])?;
    a.check(&["profile", "store"])?;
    let profile = a.profile()?;
    let dir = PathBuf::from(a.str("store")?);
    if a.rest.is_empty() {
        return usage("ingest: no INPUT");
    }
    let t = Instant::now();
    let store = ChunkStore::open(&dir, Options { profile, ..Options::default() })?;
    let mut buf = vec![0u8; PIECE];
    let (mut in_bytes, mut in_files) = (0u64, 0u64);
    let mut largest: Option<(u64, ObjectId)> = None;
    for input in &a.rest {
        let list = files_of(Path::new(input))?;
        eprintln!("ingest: {} files from input {}", list.len(), in_files_label(input));
        for p in list {
            let mut f = fs::File::open(&p)?;
            let mut w = FileWriter::new();
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                w.write(&store, &buf[..n])?;
            }
            let len = w.len();
            let key = w.finish(&store)?;
            in_bytes += len;
            in_files += 1;
            if largest.is_none_or(|(l, _)| len > l) {
                largest = Some((len, key));
            }
        }
    }
    store.close()?;
    let secs = t.elapsed().as_secs_f64();
    let st = store.stats();
    let (key_len, key) = match largest {
        Some((l, k)) => (l, k.to_hex()),
        None => (0, "NA".to_string()),
    };
    println!(
        "ingest in_bytes={in_bytes} in_files={in_files} secs={secs:.3} objects={} raw_bytes={} stored_bytes={} key={key} key_len={key_len}",
        st.objects, st.raw_bytes, st.stored_bytes
    );
    Ok(())
}

/// The last part of an input path, for progress lines (no full path).
fn in_files_label(input: &str) -> String {
    Path::new(input).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

// ── open ─────────────────────────────────────────────────────────────────────

fn open(argv: &[String]) -> Res<()> {
    let a = Args::parse(argv, &[])?;
    a.check(&["profile", "store"])?;
    let profile = a.profile()?;
    let dir = PathBuf::from(a.str("store")?);
    if !dir.is_dir() {
        return run_fail("open: the store dir does not exist");
    }
    let t = Instant::now();
    let store = ChunkStore::open(&dir, Options { profile, read_only: true, ..Options::default() })?;
    let ix = store.index_info();
    let open_ms = t.elapsed().as_secs_f64() * 1e3;
    let (mut apparent, mut alloc, mut files) = (0u64, 0u64, 0u64);
    for p in files_of(&dir)? {
        let md = fs::symlink_metadata(&p)?;
        apparent += md.len();
        alloc += md.blocks() * 512;
        files += 1;
    }
    println!(
        "open open_ms={open_ms:.3} index_runs={} index_run_entries={} index_memtable_entries={} index_filter_bytes={} index_fence_bytes={} apparent_bytes={apparent} alloc_bytes={alloc} files={files}",
        ix.runs, ix.run_entries, ix.memtable_entries, ix.filter_bytes, ix.fence_bytes
    );
    Ok(())
}

// ── probe ────────────────────────────────────────────────────────────────────

/// Where a byte range of the new content comes from.
#[derive(Clone, Copy)]
enum Origin {
    /// The base, from this base offset.
    Base(u64),
    /// The new bytes, from this offset in them.
    New(u64),
}

/// The new content of an edit: segments of the new content in order, each
/// from the base or from the new bytes. Holds the new bytes only (a few
/// KiB) and reads the base through a `FileReader` (no copy of the base).
struct EditSource<'a> {
    store: &'a ChunkStore,
    base: FileReader,
    new: Vec<u8>,
    segs: Vec<(Range<u64>, Origin)>,
}

impl EditSource<'_> {
    fn bytes(&mut self, offset: u64, len: usize) -> chunk_store::Result<Vec<u8>> {
        let end = offset + len as u64;
        let mut out = Vec::with_capacity(len);
        for (r, from) in &self.segs {
            let (s, e) = (r.start.max(offset), r.end.min(end));
            if s >= e {
                continue;
            }
            match *from {
                Origin::Base(b) => {
                    let piece = self.base.read(self.store, b + (s - r.start), (e - s) as usize)?;
                    if piece.len() as u64 != e - s {
                        return Err(chunk_store::Error::Corrupt("edit source: short base read".into()));
                    }
                    out.extend_from_slice(&piece);
                }
                Origin::New(n) => {
                    let i = (n + (s - r.start)) as usize;
                    out.extend_from_slice(&self.new[i..i + (e - s) as usize]);
                }
            }
        }
        if out.len() != len {
            return Err(chunk_store::Error::Corrupt(format!("edit source: {} bytes at {offset}, asked {len}", out.len())));
        }
        Ok(out)
    }
}

impl SpliceSource for EditSource<'_> {
    fn read(&mut self, offset: u64, len: usize) -> chunk_store::Result<Vec<u8>> {
        self.bytes(offset, len)
    }
}

/// An edit of the base of length `l`, with `k` new bytes at `p`: the
/// segments of the new content, `keep`, the changed ranges and the new length.
struct Edit {
    p: u64,
    segs: Vec<(Range<u64>, Origin)>,
    keep: u64,
    changed: Option<Range<u64>>,
    new_len: u64,
}

fn edit(kind: &str, at: &str, l: u64, k: u64) -> Edit {
    let p = match (kind, at) {
        (_, "start") => 0,
        (_, "middle") => l / 2,
        ("insert", _) => l,
        _ => l - k,
    };
    let (mut segs, keep, changed, new_len) = match kind {
        "modify" => {
            let segs = vec![(0..p, Origin::Base(0)), (p..p + k, Origin::New(0)), (p + k..l, Origin::Base(p + k))];
            (segs, l, Some(p..p + k), l)
        }
        "insert" => {
            let segs = vec![(0..p, Origin::Base(0)), (p..p + k, Origin::New(0)), (p + k..l + k, Origin::Base(p))];
            (segs, p, Some(p..l + k), l + k)
        }
        _ => {
            let segs = vec![(0..p, Origin::Base(0)), (p..l - k, Origin::Base(p + k))];
            (segs, p, (p < l - k).then_some(p..l - k), l - k)
        }
    };
    segs.retain(|(r, _)| !r.is_empty());
    Edit { p, segs, keep, changed, new_len }
}

/// Nearest-rank percentile `q` (0..100) of sorted microsecond values.
fn pct(sorted: &[f64], q: f64) -> String {
    if sorted.is_empty() {
        return "NA".into();
    }
    let rank = ((q / 100.0) * sorted.len() as f64).ceil() as usize;
    format!("{:.1}", sorted[rank.clamp(1, sorted.len()) - 1])
}

fn timed_reads(store: &ChunkStore, r: &mut FileReader, offsets: impl Iterator<Item = u64>) -> Res<Vec<f64>> {
    let mut us = Vec::new();
    for off in offsets {
        let t = Instant::now();
        let got = r.read(store, off, READ)?;
        us.push(t.elapsed().as_secs_f64() * 1e6);
        if got.is_empty() {
            return run_fail(format!("probe: empty read at {off}"));
        }
    }
    us.sort_by(f64::total_cmp);
    Ok(us)
}

fn probe(argv: &[String]) -> Res<()> {
    let a = Args::parse(argv, &[])?;
    a.check(&["profile", "store", "key", "seq-bytes", "reads", "seed", "edit-bytes", "edit-size"])?;
    let profile = a.profile()?;
    let dir = PathBuf::from(a.str("store")?);
    let key = parse_key(a.str("key")?)?;
    let seq_bytes = a.num("seq-bytes", Some(64 << 20))?;
    let reads = a.num("reads", Some(10_000))?;
    let seed = a.num("seed", Some(1))?;
    let edit_bytes = a.num("edit-bytes", Some(64 << 20))?;
    let k = a.num("edit-size", Some(4096))?;
    if k == 0 {
        return usage("--edit-size must be at least 1");
    }
    let store = ChunkStore::open(&dir, Options { profile, ..Options::default() })?;
    let mut r = FileReader::open(&store, key)?;
    let len = r.len();
    if len < READ as u64 {
        return run_fail(format!("probe: the file has {len} bytes, under one read of {READ}"));
    }

    // Sequential 4 KiB reads from 0, then random ones on a new reader.
    eprintln!("probe: sequential reads");
    let seq_end = len.min(seq_bytes);
    let seq = timed_reads(&store, &mut r, (0..seq_end).step_by(READ))?;
    eprintln!("probe: random reads");
    let mut rng = Rng::new(seed);
    let mut r = FileReader::open(&store, key)?;
    let last = len - READ as u64;
    let rand = timed_reads(&store, &mut r, (0..reads).map(|_| rng.upto(last)))?;

    // The edit base: the first L bytes of the file, written as their own content.
    let l = len.min(edit_bytes);
    if l < 2 * k {
        return run_fail(format!("probe: an edit base of {l} bytes is under two edits of {k}"));
    }
    eprintln!("probe: edit base of {l} bytes");
    let mut w = FileWriter::new();
    let mut at = 0u64;
    while at < l {
        let piece = r.read(&store, at, (l - at).min(PIECE as u64) as usize)?;
        if piece.is_empty() {
            return run_fail("probe: short read of the edit base");
        }
        w.write(&store, &piece)?;
        at += piece.len() as u64;
    }
    let base = w.finish(&store)?;

    let (mut sum_obj, mut sum_stored, mut sum_re) = (0u64, 0u64, 0u64);
    for kind in ["modify", "insert", "delete"] {
        for place in ["start", "middle", "end"] {
            let e = edit(kind, place, l, k);
            let mut new = vec![0u8; k as usize];
            rng.fill(&mut new);
            let mut src = EditSource { store: &store, base: FileReader::open(&store, base)?, new, segs: e.segs.clone() };
            let before = store.stats();
            let changed = e.changed.clone().into_iter().collect::<Vec<_>>();
            let (id, ss) = splice(&store, Some(base), e.keep, &changed, e.new_len, &mut src)?;
            let after = store.stats();
            let objects = after.objects - before.objects;
            let stored = after.stored_bytes - before.stored_bytes;

            // Read the edited window back and compare it with the expected bytes.
            let mut nr = FileReader::open(&store, id)?;
            if nr.len() != e.new_len {
                return run_fail(format!("probe: {kind} {place}: length {} not {}", nr.len(), e.new_len));
            }
            let lo = e.p.saturating_sub(CHECK_SIDE);
            let hi = e.new_len.min(e.p + k + CHECK_SIDE);
            if lo < hi {
                let got = nr.read(&store, lo, (hi - lo) as usize)?;
                let want = src.bytes(lo, (hi - lo) as usize)?;
                if got != want {
                    return run_fail(format!("probe: {kind} {place}: the read-back differs from the expected bytes"));
                }
            }
            println!("edit kind={kind} where={place} objects={objects} stored_bytes={stored} rechunked_bytes={}", ss.rechunked);
            sum_obj += objects;
            sum_stored += stored;
            sum_re += ss.rechunked;
        }
    }
    println!(
        "probe seq_reads={} seq_p50_us={} seq_p99_us={} rand_reads={} rand_p50_us={} rand_p99_us={} edit_objects={sum_obj} edit_stored_bytes={sum_stored} edit_rechunked_bytes={sum_re}",
        seq.len(),
        pct(&seq, 50.0),
        pct(&seq, 99.0),
        rand.len(),
        pct(&rand, 50.0),
        pct(&rand, 99.0)
    );
    store.close()?;
    Ok(())
}
