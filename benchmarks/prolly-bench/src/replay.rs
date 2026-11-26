//! The map-only modes: `batches` extracts the real `inode_data` change of
//! each commit; `prolly` replays the changes through the prototype in a
//! scratch store. Neither opens `DictMetadataStore`.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use chunk_store::file::FileReader;
use chunk_store::{ChunkStore, ObjectId, Options, Stats};
use metadata::store_io::{put_value, to_id};
use prolly::{Config, Edit};
use slicefs_traits::key::ObjectKey;

use crate::{
    key_of_hex, ms, print_disk, root_record, run_fail, rusage, stats_delta, usage, usage_fields, Args, Fail, Res,
    Rng, Usage, KEY_LEN, MAP_ENTRY, PIECE,
};

/// One batch record: op u8 (1 put, 0 delete), ino u64 LE, key.
const BATCH_REC: usize = 1 + MAP_ENTRY;

/// Base records per read of the hash-cost loop (about 41 KB).
const HASH_PIECE_ENTRIES: usize = 1024;

/// Entries per piece of a build that is timed in pieces (the prolly bulk
/// build and the prolly scale build): the input of a piece is read or made
/// outside the timer, as flat's input is.
const BUILD_PIECE_ENTRIES: usize = 16_384;

/// The entries of one `u64 -> key` map node, in order, read in `PIECE`
/// reads: it holds at most one piece and one partial record.
struct MapStream<'s> {
    cs: &'s ChunkStore,
    r: FileReader,
    /// The next byte offset to read.
    off: u64,
    len: u64,
    left: u64,
    buf: Vec<u8>,
    pos: usize,
}

impl<'s> MapStream<'s> {
    fn open(cs: &'s ChunkStore, key: &ObjectKey) -> Res<Self> {
        let mut r = FileReader::open(cs, to_id(key))?;
        let len = r.len();
        if len < 8 {
            return run_fail("map node shorter than its count");
        }
        let head = r.read(cs, 0, 8)?;
        let count = u64::from_le_bytes(head.as_slice().try_into().map_err(|_| Fail::Run("short map node".into()))?);
        if count.checked_mul(MAP_ENTRY as u64).and_then(|n| n.checked_add(8)) != Some(len) {
            return run_fail(format!("map node of {len} bytes does not hold {count} entries"));
        }
        Ok(MapStream { cs, r, off: 8, len, left: count, buf: Vec::new(), pos: 0 })
    }

    fn next_entry(&mut self) -> Res<Option<(u64, [u8; KEY_LEN])>> {
        if self.left == 0 {
            return Ok(None);
        }
        if self.buf.len() - self.pos < MAP_ENTRY {
            let n = (self.len - self.off).min(PIECE as u64) as usize;
            let piece = self.r.read(self.cs, self.off, n)?;
            self.off += piece.len() as u64;
            self.buf.drain(..self.pos);
            self.pos = 0;
            self.buf.extend_from_slice(&piece);
            if self.buf.len() < MAP_ENTRY {
                return run_fail("map node ends inside an entry");
            }
        }
        let e = &self.buf[self.pos..self.pos + MAP_ENTRY];
        let ino = u64::from_le_bytes(e[..8].try_into().expect("8 bytes"));
        let key: [u8; KEY_LEN] = e[8..].try_into().expect("33 bytes");
        self.pos += MAP_ENTRY;
        self.left -= 1;
        Ok(Some((ino, key)))
    }
}

/// The count and BLAKE3 digest of a map: over its 41-byte records (ino LE
/// and key) in ino order, without the count.
struct Digest {
    h: blake3::Hasher,
    entries: u64,
}

impl Digest {
    fn new() -> Self {
        Digest { h: blake3::Hasher::new(), entries: 0 }
    }

    fn add(&mut self, ino: u64, key: &[u8]) {
        self.h.update(&ino.to_le_bytes());
        self.h.update(key);
        self.entries += 1;
    }

    fn hex(&self) -> String {
        self.h.finalize().to_hex().to_string()
    }
}

/// One line of the roots file: `seq pattern k root_hex`.
struct RootLine {
    seq: u64,
    pattern: String,
    k: u64,
    root: ObjectKey,
}

fn read_roots(path: &Path) -> Res<Vec<RootLine>> {
    let mut out = Vec::new();
    for line in BufReader::new(fs::File::open(path)?).lines() {
        let line = line?;
        let w: Vec<&str> = line.split_whitespace().collect();
        let [seq, pattern, k, root] = w[..] else { return run_fail(format!("bad roots line: {line}")) };
        let num = |s: &str| s.parse::<u64>().map_err(|_| Fail::Run(format!("bad roots line: {line}")));
        out.push(RootLine { seq: num(seq)?, pattern: pattern.to_string(), k: num(k)?, root: key_of_hex(root)? });
    }
    if out.is_empty() {
        return run_fail("the roots file is empty");
    }
    Ok(out)
}

fn batch_path(dir: &Path, seq: u64) -> PathBuf {
    dir.join(format!("batch-{seq:05}.bin"))
}

// ── batches ──────────────────────────────────────────────────────────────────

/// Writes `base.bin` (the seq-0 `inode_data`), one sorted batch file per
/// later commit (the merge diff of two consecutive `inode_data` nodes) and
/// `batches.txt`. Memory: two map streams, never a whole map (D-02).
pub(crate) fn batches(argv: &[String]) -> Res<()> {
    let a = Args::parse(argv)?;
    a.check(&["store", "roots", "out"])?;
    let store = a.path("store")?;
    let roots = read_roots(&a.path("roots")?)?;
    let out = a.path("out")?;
    fs::create_dir_all(&out)?;
    // Read only: the store of whole was measured, so this step must not
    // recover, merge or write it.
    let cs = ChunkStore::open(&store, Options { read_only: true, ..Options::default() })?;
    let maps: Vec<ObjectKey> =
        roots.iter().map(|r| root_record(&cs, &r.root).map(|rr| rr.inode_data())).collect::<Res<_>>()?;
    let mut txt = BufWriter::new(fs::File::create(out.join("batches.txt"))?);

    let mut base = BufWriter::new(fs::File::create(out.join("base.bin"))?);
    let mut s = MapStream::open(&cs, &maps[0])?;
    while let Some((ino, key)) = s.next_entry()? {
        base.write_all(&ino.to_le_bytes())?;
        base.write_all(&key)?;
    }
    base.flush()?;

    for (i, r) in roots.iter().enumerate().skip(1) {
        let mut old = MapStream::open(&cs, &maps[i - 1])?;
        let mut new = MapStream::open(&cs, &maps[i])?;
        let mut f = BufWriter::new(fs::File::create(batch_path(&out, r.seq))?);
        // puts: new or changed values; inserts: the puts of keys that the
        // old map does not have.
        let (mut puts, mut dels, mut inserts) = (0u64, 0u64, 0u64);
        let rec = |f: &mut BufWriter<fs::File>, op: u8, ino: u64, key: &[u8; KEY_LEN]| -> Res<()> {
            f.write_all(&[op])?;
            f.write_all(&ino.to_le_bytes())?;
            f.write_all(key)?;
            Ok(())
        };
        let (mut o, mut n) = (old.next_entry()?, new.next_entry()?);
        loop {
            match (o, n) {
                (None, None) => break,
                (Some((oi, ok)), Some((ni, nk))) if oi == ni => {
                    if ok != nk {
                        rec(&mut f, 1, ni, &nk)?;
                        puts += 1;
                    }
                    o = old.next_entry()?;
                    n = new.next_entry()?;
                }
                (Some((oi, ok)), Some((ni, _))) if oi < ni => {
                    rec(&mut f, 0, oi, &ok)?;
                    dels += 1;
                    o = old.next_entry()?;
                }
                (Some((oi, ok)), None) => {
                    rec(&mut f, 0, oi, &ok)?;
                    dels += 1;
                    o = old.next_entry()?;
                }
                (_, Some((ni, nk))) => {
                    rec(&mut f, 1, ni, &nk)?;
                    puts += 1;
                    inserts += 1;
                    n = new.next_entry()?;
                }
            }
        }
        f.flush()?;
        let line = format!("batch seq={} pattern={} k={} puts={puts} dels={dels} inserts={inserts}", r.seq, r.pattern, r.k);
        println!("{line}");
        writeln!(txt, "{line}")?;
    }

    let mut d = Digest::new();
    let mut s = MapStream::open(&cs, maps.last().expect("at least one root"))?;
    while let Some((ino, key)) = s.next_entry()? {
        d.add(ino, &key);
    }
    let line = format!("final entries={} digest={}", d.entries, d.hex());
    println!("{line}");
    writeln!(txt, "{line}")?;
    txt.flush()?;
    cs.close()?;
    Ok(())
}

// ── replay input ─────────────────────────────────────────────────────────────

/// One `batch` line of `batches.txt`.
struct BatchLine {
    seq: u64,
    pattern: String,
    k: u64,
}

/// The `batch` lines and the `final` entries and digest of `batches.txt`.
struct Plan {
    batches: Vec<BatchLine>,
    entries: u64,
    digest: String,
}

fn field<'a>(line: &'a str, name: &str) -> Res<&'a str> {
    line.split_whitespace()
        .find_map(|w| w.strip_prefix(name).and_then(|r| r.strip_prefix('=')))
        .ok_or_else(|| Fail::Run(format!("no {name}= in: {line}")))
}

fn num_field(line: &str, name: &str) -> Res<u64> {
    field(line, name)?.parse().map_err(|_| Fail::Run(format!("bad {name}= in: {line}")))
}

fn read_plan(dir: &Path) -> Res<Plan> {
    let mut batches = Vec::new();
    let mut fin = None;
    for line in BufReader::new(fs::File::open(dir.join("batches.txt"))?).lines() {
        let line = line?;
        match line.split_whitespace().next() {
            Some("batch") => batches.push(BatchLine {
                seq: num_field(&line, "seq")?,
                pattern: field(&line, "pattern")?.to_string(),
                k: num_field(&line, "k")?,
            }),
            Some("final") => fin = Some((num_field(&line, "entries")?, field(&line, "digest")?.to_string())),
            _ => return run_fail(format!("bad batches.txt line: {line}")),
        }
    }
    let Some((entries, digest)) = fin else { return run_fail("batches.txt has no final line") };
    Ok(Plan { batches, entries, digest })
}

/// Streams the 41-byte records of `base.bin`.
fn for_each_base(dir: &Path, mut f: impl FnMut(u64, &[u8; KEY_LEN]) -> Res<()>) -> Res<()> {
    let mut r = BufReader::new(fs::File::open(dir.join("base.bin"))?);
    let mut rec = [0u8; MAP_ENTRY];
    loop {
        match r.read_exact(&mut rec) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e.into()),
        }
        let ino = u64::from_le_bytes(rec[..8].try_into().expect("8 bytes"));
        f(ino, rec[8..].try_into().expect("33 bytes"))?;
    }
}

/// Streams the records of one batch file (sorted by ino) to `f` as (put,
/// ino, key); returns the record count. No copy of the file is held.
fn for_each_batch(dir: &Path, seq: u64, mut f: impl FnMut(bool, u64, &[u8; KEY_LEN])) -> Res<u64> {
    let path = batch_path(dir, seq);
    let len = fs::metadata(&path)?.len();
    if len % BATCH_REC as u64 != 0 {
        return run_fail(format!("batch {seq}: {len} bytes is not whole records"));
    }
    let mut r = BufReader::new(fs::File::open(&path)?);
    let mut rec = [0u8; BATCH_REC];
    for _ in 0..len / BATCH_REC as u64 {
        r.read_exact(&mut rec)?;
        let put = match rec[0] {
            1 => true,
            0 => false,
            op => return run_fail(format!("batch {seq}: bad op {op}")),
        };
        f(put, u64::from_le_bytes(rec[1..9].try_into().expect("8 bytes")), rec[9..].try_into().expect("33 bytes"));
    }
    Ok(len / BATCH_REC as u64)
}

/// Prints the `check` line; a mismatch with `batches.txt` is a failure.
fn check(mode: &str, plan: &Plan, d: &Digest) -> Res<()> {
    let digest = d.hex();
    let ok = d.entries == plan.entries && digest == plan.digest;
    println!("check mode={mode} entries={} digest={digest} ok={}", d.entries, u8::from(ok));
    if !ok {
        return run_fail(format!("{mode}: the final map differs from the real one (want {} entries, {})", plan.entries, plan.digest));
    }
    Ok(())
}

/// Wall time, usage and store stats around one timed step.
struct Measured {
    ms: f64,
    u0: Usage,
    u1: Usage,
    s0: Stats,
    s1: Stats,
}

fn measure<T>(cs: &ChunkStore, f: impl FnOnce() -> Res<T>) -> Res<(T, Measured)> {
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

/// Reads into `buf` until it is full or the file ends; returns the bytes read.
fn read_piece(r: &mut impl Read, buf: &mut [u8]) -> Res<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// Times `f` (it returns its result, the serialized bytes and, for
/// prolly, the nodes read and written) and prints one `commit` line of a
/// replay mode.
#[allow(clippy::too_many_arguments, reason = "one output line of many fields")]
fn timed<T>(
    mode: &str,
    cs: &ChunkStore,
    pattern: &str,
    k: u64,
    seq: u64,
    changed: u64,
    f: impl FnOnce() -> Res<(T, u64, Option<(u64, u64)>)>,
) -> Res<T> {
    let ((out, ser, nodes), m) = measure(cs, f)?;
    commit_line(mode, pattern, k, seq, changed, ser, nodes, &m);
    Ok(out)
}

/// Prints one `commit` line of a replay mode.
#[allow(clippy::too_many_arguments, reason = "one output line of many fields")]
fn commit_line(
    mode: &str,
    pattern: &str,
    k: u64,
    seq: u64,
    changed: u64,
    ser: u64,
    nodes: Option<(u64, u64)>,
    m: &Measured,
) {
    let nodes = match nodes {
        Some((r, w)) => format!("nodes_read={r} nodes_written={w}"),
        None => "nodes_read=NA nodes_written=NA".to_string(),
    };
    println!(
        "commit mode={mode} pattern={pattern} k={k} seq={seq} entries_changed={changed} serialized_bytes={ser} begin_ms=NA finish_ms={t:.3} total_ms={t:.3} {} {} {nodes}",
        usage_fields(&m.u0, &m.u1),
        stats_delta(&m.s0, &m.s1),
        t = m.ms
    );
}

// ── flat ─────────────────────────────────────────────────────────────────────

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

/// The current design for one map: the whole map in RAM, interned in full
/// per commit, in a scratch store; ends with the digest check.
pub(crate) fn flat(argv: &[String]) -> Res<()> {
    let a = Args::parse(argv)?;
    a.check(&["batches", "store"])?;
    let dir = a.path("batches")?;
    let store = a.path("store")?;
    let plan = read_plan(&dir)?;
    let cs = ChunkStore::open(&store, Options::default())?;

    // The in-RAM map is the cost of the current design that this mode measures.
    let mut map: BTreeMap<u64, [u8; KEY_LEN]> = BTreeMap::new();
    for_each_base(&dir, |ino, key| {
        map.insert(ino, *key);
        Ok(())
    })?;
    let mut last = timed("flat", &cs, "bulk", 0, 0, map.len() as u64, || {
        let (key, ser) = intern_flat(&cs, &map)?;
        cs.sync()?;
        Ok((key, ser, None))
    })?;
    // The disk sizes are taken after a close, which writes the pack index,
    // as for whole (D-23: index included); the store is opened again.
    cs.close()?;
    print_disk("flat", "start", &store)?;
    let cs = ChunkStore::open(&store, Options::default())?;

    for bl in &plan.batches {
        let changed = for_each_batch(&dir, bl.seq, |put, ino, key| {
            if put {
                map.insert(ino, *key);
            } else {
                map.remove(&ino);
            }
        })?;
        last = timed("flat", &cs, &bl.pattern, bl.k, bl.seq, changed, || {
            let (key, ser) = intern_flat(&cs, &map)?;
            cs.sync()?;
            Ok((key, ser, None))
        })?;
    }

    // The check reads the last stored value back from the store (as prolly
    // iterates its stored tree), so it proves what was written.
    let mut d = Digest::new();
    let mut s = MapStream::open(&cs, &last)?;
    while let Some((ino, key)) = s.next_entry()? {
        d.add(ino, &key);
    }
    check("flat", &plan, &d)?;
    cs.close()?;
    print_disk("flat", "end", &store)?;
    Ok(())
}

// ── prolly ───────────────────────────────────────────────────────────────────

/// The key of the prolly map: the ino big-endian, so the byte order is
/// the ino order.
fn pkey(ino: u64) -> [u8; 8] {
    ino.to_be_bytes()
}

/// Bulk build of `base.bin`, then one `prolly::apply` per batch file, in a
/// scratch store; ends with a digest check against the real final map.
pub(crate) fn prolly(argv: &[String]) -> Res<()> {
    let a = Args::parse(argv)?;
    a.check(&["batches", "store"])?;
    let dir = a.path("batches")?;
    let store = a.path("store")?;
    let plan = read_plan(&dir)?;
    let cfg = Config::DEFAULT;
    let cs = ChunkStore::open(&store, Options::default())?;

    // The cost of the boundary hash, once over all base keys (D-12). The
    // keys are read in small pieces (so the peak RSS of this process stays
    // the prototype's own) and only the hash loop of each piece is timed,
    // so the file reads are not in the number.
    let base_n = fs::metadata(dir.join("base.bin"))?.len() / MAP_ENTRY as u64;
    let mut hash_ns = 0u128;
    let mut acc = 0u32;
    let mut r = fs::File::open(dir.join("base.bin"))?;
    let mut piece = vec![0u8; HASH_PIECE_ENTRIES * MAP_ENTRY];
    loop {
        let n = read_piece(&mut r, &mut piece)?;
        if n == 0 {
            break;
        }
        let t = Instant::now();
        for rec in piece[..n - n % MAP_ENTRY].as_chunks::<MAP_ENTRY>().0 {
            let ino = u64::from_le_bytes(rec[..8].try_into().expect("8 bytes"));
            acc ^= prolly::boundary_hash(0, std::hint::black_box(&pkey(ino)));
        }
        hash_ns += t.elapsed().as_nanos();
    }
    std::hint::black_box(acc);
    println!("hash boundary_hash_ns_per_key={:.1}", hash_ns as f64 / base_n.max(1) as f64);

    // The bulk build, timed in pieces: base.bin is read outside the timer,
    // as flat reads it into its map before its timed intern.
    let mut tm = Pieces::new(&cs);
    let mut b = prolly::Builder::new(&cs, cfg);
    let mut r = fs::File::open(dir.join("base.bin"))?;
    let mut piece = vec![0u8; BUILD_PIECE_ENTRIES * MAP_ENTRY];
    loop {
        let n = read_piece(&mut r, &mut piece)?;
        if n == 0 {
            break;
        }
        if n % MAP_ENTRY != 0 {
            return run_fail("base.bin is not whole records");
        }
        tm.time(|| {
            for rec in piece[..n].as_chunks::<MAP_ENTRY>().0 {
                let ino = u64::from_le_bytes(rec[..8].try_into().expect("8 bytes"));
                b.push(&pkey(ino), &rec[8..])?;
            }
            Ok(())
        })?;
    }
    drop(piece);
    let (mut root, st) = tm.time(|| {
        let (root, st) = b.finish()?;
        cs.sync()?;
        Ok((root, st))
    })?;
    commit_line("prolly", "bulk", 0, 0, base_n, st.bytes_written, Some((0, st.nodes_written)), &tm.done(&cs));
    // As in flat: the sizes after a close (pack index included), then open again.
    cs.close()?;
    print_disk("prolly", "start", &store)?;
    let cs = ChunkStore::open(&store, Options::default())?;

    for bl in &plan.batches {
        // One batch in RAM (D-02), read straight into edits.
        let mut edits: Vec<Edit> = Vec::new();
        for_each_batch(&dir, bl.seq, |put, ino, key| {
            edits.push(if put {
                Edit::Put { key: pkey(ino).to_vec(), value: key.to_vec() }
            } else {
                Edit::Delete { key: pkey(ino).to_vec() }
            });
        })?;
        root = timed("prolly", &cs, &bl.pattern, bl.k, bl.seq, edits.len() as u64, || {
            let (r, st) = prolly::apply(&cs, cfg, &root, &edits)?;
            cs.sync()?;
            Ok((r, st.bytes_written, Some((st.nodes_read, st.nodes_written))))
        })?;
    }

    let mut d = Digest::new();
    for e in prolly::iter(&cs, &root)? {
        let (k, v) = e?;
        let ino = u64::from_be_bytes(k.as_slice().try_into().map_err(|_| Fail::Run("prolly key is not 8 bytes".into()))?);
        d.add(ino, &v);
    }
    check("prolly", &plan, &d)?;
    print_tree(&cs, &root)?;
    cs.close()?;
    print_disk("prolly", "end", &store)?;
    Ok(())
}

/// Prints the shape of the tree `root` (one `walk`, one path held).
fn print_tree(cs: &ChunkStore, root: &ObjectId) -> Res<()> {
    let (mut depth, mut nodes, mut leaves, mut internal) = (0u8, 0u64, 0u64, 0u64);
    let (mut leaf_min, mut leaf_max, mut leaf_sum, mut internal_sum) = (usize::MAX, 0usize, 0u64, 0u64);
    prolly::walk(cs, root, |n| {
        depth = depth.max(n.level + 1);
        nodes += 1;
        if n.level == 0 {
            leaves += 1;
            leaf_min = leaf_min.min(n.bytes);
            leaf_max = leaf_max.max(n.bytes);
            leaf_sum += n.bytes as u64;
        } else {
            internal += 1;
            internal_sum += n.bytes as u64;
        }
    })?;
    let mean = |sum: u64, n: u64| sum.checked_div(n).unwrap_or(0);
    println!(
        "tree depth={depth} nodes={nodes} leaves={leaves} leaf_bytes_min={} leaf_bytes_mean={} leaf_bytes_max={leaf_max} internal_nodes={internal} internal_bytes_mean={}",
        if leaves == 0 { 0 } else { leaf_min },
        mean(leaf_sum, leaves),
        mean(internal_sum, internal)
    );
    Ok(())
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
