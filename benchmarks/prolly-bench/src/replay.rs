//! The map-only modes: `batches` extracts the real `inode_data` change of
//! each commit; `prolly` replays the changes through the prototype in a
//! scratch store. Neither opens `DictMetadataStore`.

use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use chunk_store::file::FileReader;
use chunk_store::{ChunkStore, ObjectId, Options};
use metadata::store_io::to_id;
use prolly::{Config, Edit};
use slicefs_traits::key::ObjectKey;

use crate::{
    key_of_hex, ms, print_disk, root_record, run_fail, rusage, stats_delta, usage_fields, Args, Fail, Res, KEY_LEN,
    MAP_ENTRY, PIECE,
};

/// One batch record: op u8 (1 put, 0 delete), ino u64 LE, key.
const BATCH_REC: usize = 1 + MAP_ENTRY;

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
    let cs = ChunkStore::open(&store, Options::default())?;
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
        let (mut puts, mut dels) = (0u64, 0u64);
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
                    n = new.next_entry()?;
                }
            }
        }
        f.flush()?;
        let line = format!("batch seq={} pattern={} k={} puts={puts} dels={dels}", r.seq, r.pattern, r.k);
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

/// The records of one batch file: (put, ino, key), sorted by ino.
fn read_batch(dir: &Path, seq: u64) -> Res<Vec<(bool, u64, [u8; KEY_LEN])>> {
    let b = fs::read(batch_path(dir, seq))?;
    if b.len() % BATCH_REC != 0 {
        return run_fail(format!("batch {seq}: {} bytes is not whole records", b.len()));
    }
    let mut out = Vec::with_capacity(b.len() / BATCH_REC);
    for r in b.chunks_exact(BATCH_REC) {
        let put = match r[0] {
            1 => true,
            0 => false,
            op => return run_fail(format!("batch {seq}: bad op {op}")),
        };
        out.push((put, u64::from_le_bytes(r[1..9].try_into().expect("8 bytes")), r[9..].try_into().expect("33 bytes")));
    }
    Ok(out)
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

/// Times `f` with the store stats and the usage around it and prints one
/// `commit` line of a replay mode.
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
    let s0 = cs.stats();
    let u0 = rusage();
    let t = Instant::now();
    let (out, ser, nodes) = f()?;
    let total = ms(t);
    let u1 = rusage();
    let s1 = cs.stats();
    let nodes = match nodes {
        Some((r, w)) => format!("nodes_read={r} nodes_written={w}"),
        None => "nodes_read=NA nodes_written=NA".to_string(),
    };
    println!(
        "commit mode={mode} pattern={pattern} k={k} seq={seq} entries_changed={changed} serialized_bytes={ser} begin_ms=NA finish_ms={total:.3} total_ms={total:.3} {} {} {nodes}",
        usage_fields(&u0, &u1),
        stats_delta(&s0, &s1)
    );
    Ok(out)
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

    let base_n = fs::metadata(dir.join("base.bin"))?.len() / MAP_ENTRY as u64;
    let mut root: ObjectId = timed("prolly", &cs, "bulk", 0, 0, base_n, || {
        let mut b = prolly::Builder::new(&cs, cfg);
        for_each_base(&dir, |ino, key| Ok(b.push(&pkey(ino), key)?))?;
        let (root, st) = b.finish()?;
        cs.sync()?;
        Ok((root, st.bytes_written, Some((0, st.nodes_written))))
    })?;
    print_disk("prolly", "start", &store)?;

    for bl in &plan.batches {
        let recs = read_batch(&dir, bl.seq)?;
        let edits: Vec<Edit> = recs
            .iter()
            .map(|(put, ino, key)| {
                if *put {
                    Edit::Put { key: pkey(*ino).to_vec(), value: key.to_vec() }
                } else {
                    Edit::Delete { key: pkey(*ino).to_vec() }
                }
            })
            .collect();
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
    print_disk("prolly", "end", &store)?;
    cs.close()?;
    Ok(())
}
