//! The store index: a memtable of recent entries in RAM, and sorted run
//! files on disk (see `run.rs`), listed in a MANIFEST.
//!
//! The packs stay the source of truth. The MANIFEST holds a high-water mark
//! (hwm): a (pack, offset) below which every pack record is in a durable run
//! (or hidden by a durable tombstone). Open reads the MANIFEST and the run
//! footers and tables, and replays only the pack records at or above the
//! hwm into the memtable.
//!
//! Lookups take the newest entry of an ID: the memtable first, then the runs
//! from the newest to the oldest. A put entry gives the place of the object;
//! a tombstone says that GC removed it.
//!
//! Layout of the MANIFEST (integers are little-endian): magic `SFMF`,
//! version u16, flags u32 (bit 0: rebuild the index from the packs at the
//! next writable open; any other bit is damage), sequence u64, next run
//! number u64, hwm pack u64 and offset u64, the counts of the store
//! (objects, raw bytes, stored bytes, u64 each), the live runs from the
//! newest to the oldest (count u32, then per run: number u64, byte length
//! u64, entry count u64), the obsolete run numbers (count u32, then u64
//! each: runs that this MANIFEST replaced, to delete once it is durable),
//! CRC32C u32 of every byte before it.

use crate::pack_io::{Kind, PackIo};
use crate::run::{Entry, Run, RunWriter};
use crate::{Error, Loc, ObjectId, Result};
use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;

const MAGIC: &[u8; 4] = b"SFMF";
/// Format version of the MANIFEST.
const VERSION: u16 = 1;
/// MANIFEST flag: rebuild the index from the packs at the next writable open.
pub(crate) const FLAG_REBUILD: u32 = 1;
const KNOWN_FLAGS: u32 = FLAG_REBUILD;
/// Magic, version, flags, sequence, next run, hwm pack and offset, three counts.
const FIXED: usize = 4 + 2 + 4 + 8 + 8 + 8 + 8 + 3 * 8;
/// Number, byte length and entry count of one live run.
const RUN_ROW: usize = 24;
/// A list count (u32).
const COUNT: usize = 4;
const CRC: usize = 4;

/// A place in the packs: every record below it is in a durable run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Hwm {
    pub pack: u64,
    pub offset: u64,
}

/// The object counts of the store (the `Stats` without the pack count).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    pub objects: u64,
    pub raw_bytes: u64,
    pub stored_bytes: u64,
}

/// One live run as the MANIFEST lists it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RunMeta {
    pub num: u64,
    pub len: u64,
    pub entries: u64,
}

/// The values of a MANIFEST.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Manifest {
    pub flags: u32,
    pub seq: u64,
    pub next_run: u64,
    pub hwm: Hwm,
    pub counts: Counts,
    /// From the newest to the oldest.
    pub runs: Vec<RunMeta>,
    pub obsolete: Vec<u64>,
}

fn le_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn le_u64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

impl Manifest {
    /// The state of a store without a MANIFEST: no runs, nothing covered.
    pub fn empty() -> Self {
        Manifest {
            flags: 0,
            seq: 0,
            next_run: 1,
            hwm: Hwm::default(),
            counts: Counts::default(),
            runs: Vec::new(),
            obsolete: Vec::new(),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(FIXED + 2 * COUNT + self.runs.len() * RUN_ROW + self.obsolete.len() * 8 + CRC);
        b.extend_from_slice(MAGIC);
        b.extend_from_slice(&VERSION.to_le_bytes());
        b.extend_from_slice(&self.flags.to_le_bytes());
        for v in [
            self.seq,
            self.next_run,
            self.hwm.pack,
            self.hwm.offset,
            self.counts.objects,
            self.counts.raw_bytes,
            self.counts.stored_bytes,
        ] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&(self.runs.len() as u32).to_le_bytes());
        for r in &self.runs {
            b.extend_from_slice(&r.num.to_le_bytes());
            b.extend_from_slice(&r.len.to_le_bytes());
            b.extend_from_slice(&r.entries.to_le_bytes());
        }
        b.extend_from_slice(&(self.obsolete.len() as u32).to_le_bytes());
        for n in &self.obsolete {
            b.extend_from_slice(&n.to_le_bytes());
        }
        let crc = crc32c::crc32c(&b);
        b.extend_from_slice(&crc.to_le_bytes());
        b
    }

    /// The MANIFEST in `b`. Damage (magic, CRC, version, an unknown flag, a
    /// count that does not fit the length, a run number that is not below
    /// the next run number) is `Corrupt`.
    pub fn decode(b: &[u8]) -> Result<Manifest> {
        let bad = |what: &str| Error::Corrupt(format!("index MANIFEST: {what}"));
        if b.len() < FIXED + 2 * COUNT + CRC {
            return Err(bad("too short"));
        }
        if &b[..4] != MAGIC {
            return Err(bad("no MANIFEST magic"));
        }
        let (body, crc) = b.split_at(b.len() - CRC);
        if crc32c::crc32c(body) != u32::from_le_bytes(crc.try_into().unwrap()) {
            return Err(bad("bad CRC"));
        }
        let version = u16::from_le_bytes([body[4], body[5]]);
        if version != VERSION {
            return Err(Error::Corrupt(format!(
                "index MANIFEST has format version {version}; this SliceFS reads version {VERSION}"
            )));
        }
        let flags = le_u32(body, 6);
        if flags & !KNOWN_FLAGS != 0 {
            return Err(bad("unknown flag bits"));
        }
        let v = |i: usize| le_u64(body, 10 + 8 * i);
        let mut m = Manifest {
            flags,
            seq: v(0),
            next_run: v(1),
            hwm: Hwm { pack: v(2), offset: v(3) },
            counts: Counts { objects: v(4), raw_bytes: v(5), stored_bytes: v(6) },
            runs: Vec::new(),
            obsolete: Vec::new(),
        };
        // The counts are untrusted: check each size against the length
        // before any allocation.
        let mut at = FIXED;
        let n = le_u32(body, at) as usize;
        at += COUNT;
        let runs_end = n.checked_mul(RUN_ROW).and_then(|s| s.checked_add(at)).filter(|&e| e + COUNT <= body.len());
        let runs_end = runs_end.ok_or_else(|| bad("run count does not fit"))?;
        m.runs = body[at..runs_end]
            .chunks_exact(RUN_ROW)
            .map(|r| RunMeta { num: le_u64(r, 0), len: le_u64(r, 8), entries: le_u64(r, 16) })
            .collect();
        at = runs_end;
        let n = le_u32(body, at) as usize;
        at += COUNT;
        if n.checked_mul(8).and_then(|s| s.checked_add(at)) != Some(body.len()) {
            return Err(bad("obsolete run count does not fit"));
        }
        m.obsolete = body[at..].chunks_exact(8).map(|c| le_u64(c, 0)).collect();
        if m.runs.iter().map(|r| r.num).chain(m.obsolete.iter().copied()).any(|n| n >= m.next_run) {
            return Err(bad("a run number is not below the next run number"));
        }
        Ok(m)
    }
}

/// The place of the object for a put entry; `None` for a tombstone.
fn put_loc(e: Entry) -> Option<Loc> {
    match e {
        Entry::Put(l) => Some(l),
        Entry::Tomb(_) => None,
    }
}

/// Why the index files on disk cannot be used.
enum Damage {
    /// A run that the MANIFEST lists is missing (a read-only open reads the
    /// MANIFEST once more).
    MissingRun(String),
    Other(String),
}

/// The index: the memtable, the open runs and the MANIFEST values.
pub(crate) struct Index {
    mem: BTreeMap<ObjectId, Entry>,
    /// From the newest to the oldest, as in `man.runs`.
    runs: Vec<Arc<Run>>,
    man: Manifest,
}

impl Index {
    /// An index with nothing in it (a store with no files).
    pub fn empty() -> Self {
        Index { mem: BTreeMap::new(), runs: Vec::new(), man: Manifest::empty() }
    }

    /// Open the index of a store: read the MANIFEST (absent: an empty
    /// index) and open each live run (its footer and tables only).
    ///
    /// The run files and the MANIFEST are derived data: the packs are the
    /// truth. So damage (a MANIFEST or run that is `Corrupt`, of another
    /// version, or a listed run that is missing) or the rebuild flag gives
    /// [`Index::rebuild`]: an empty index, and the caller replays every pack.
    /// A read-only open first reads the MANIFEST once more if a listed run
    /// is missing (a merge of a writer process can replace both between the
    /// two reads). Any other error (for example EIO) fails the open: one
    /// transient read error must not remove the index.
    ///
    /// A writable open removes the obsolete runs that the MANIFEST names and
    /// every run file that it does not list (orphans of a failed flush or
    /// merge). A read-only open removes and writes nothing.
    pub fn open(io: &dyn PackIo, read_only: bool) -> Result<Index> {
        let mut reason = match Self::load(io)? {
            Ok(ix) => return Self::opened(io, read_only, ix),
            Err(d) => d,
        };
        if read_only && matches!(reason, Damage::MissingRun(_)) {
            reason = match Self::load(io)? {
                Ok(ix) => return Self::opened(io, read_only, ix),
                Err(d) => d,
            };
        }
        let (Damage::MissingRun(r) | Damage::Other(r)) = reason;
        Self::rebuild(io, read_only, &r)
    }

    /// The MANIFEST and its runs as they are on disk, or the damage that
    /// makes them unusable.
    fn load(io: &dyn PackIo) -> Result<std::result::Result<Index, Damage>> {
        let man = match io.read(0, Kind::Manifest) {
            Ok(b) => match Manifest::decode(&b) {
                Ok(m) => m,
                Err(Error::Corrupt(m)) => return Ok(Err(Damage::Other(m))),
                Err(e) => return Err(e),
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => Manifest::empty(),
            Err(e) => return Err(e.into()),
        };
        let mut runs = Vec::with_capacity(man.runs.len());
        for r in &man.runs {
            let file = match io.open_run(r.num) {
                Ok(f) => f,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    return Ok(Err(Damage::MissingRun(format!("the MANIFEST lists run {:016x}, which is missing", r.num))))
                }
                Err(e) => return Err(e.into()),
            };
            let run = match Run::open(file, r.num, r.len) {
                Ok(run) => run,
                Err(Error::Corrupt(m)) => return Ok(Err(Damage::Other(m))),
                Err(e) => return Err(e),
            };
            if run.entries() != r.entries {
                return Ok(Err(Damage::Other(format!(
                    "index run {:016x} has {} entries; the MANIFEST says {}",
                    r.num,
                    run.entries(),
                    r.entries
                ))));
            }
            runs.push(Arc::new(run));
        }
        Ok(Ok(Index { mem: BTreeMap::new(), runs, man }))
    }

    /// The last steps of a good open: the rebuild flag, and for a writable
    /// open the removal of runs that the MANIFEST does not use.
    fn opened(io: &dyn PackIo, read_only: bool, mut ix: Index) -> Result<Index> {
        if ix.man.flags & FLAG_REBUILD != 0 {
            return Self::rebuild(io, read_only, "a rebuild of the index was requested");
        }
        if read_only {
            return Ok(ix);
        }
        let on_disk = io.list_runs()?;
        // A run file that a failed flush left behind must not block the next
        // flush with the same number (also if its removal below fails).
        if let Some(max) = on_disk.iter().max() {
            ix.man.next_run = ix.man.next_run.max(max.saturating_add(1));
        }
        let live: Vec<u64> = ix.man.runs.iter().map(|r| r.num).collect();
        let mut gone = ix.man.obsolete.clone();
        gone.extend(on_disk.into_iter().filter(|n| !live.contains(n) && !ix.man.obsolete.contains(n)));
        let mut removed = false;
        for num in gone {
            match io.remove(num, Kind::Run) {
                Ok(()) => removed = true,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                // Harmless: the next writable open tries again.
                Err(e) => tracing::warn!("chunk store: cannot remove the unused index run {num:016x}: {e}"),
            }
        }
        if removed {
            io.sync_dir()?;
        }
        Ok(ix)
    }

    /// An empty index that replaces one that cannot be used (`reason`); the
    /// caller then replays every pack. A writable open removes the MANIFEST
    /// and every run file first (the MANIFEST first: without it, a run file
    /// that is left is only an orphan), and new run numbers start past every
    /// old one. A read-only open writes nothing and keeps the index in RAM.
    pub fn rebuild(io: &dyn PackIo, read_only: bool, reason: &str) -> Result<Index> {
        if read_only {
            tracing::warn!("chunk store: the index cannot be used ({reason}); this read-only open reads every pack");
            return Ok(Index::empty());
        }
        tracing::warn!("chunk store: the index cannot be used ({reason}); it is rebuilt from the packs");
        let on_disk = io.list_runs()?;
        let mut ix = Index::empty();
        ix.man.next_run = on_disk.iter().max().map_or(1, |m| m.saturating_add(1));
        match io.remove(0, Kind::Manifest) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        for num in on_disk {
            if let Err(e) = io.remove(num, Kind::Run) {
                if e.kind() != io::ErrorKind::NotFound {
                    tracing::warn!("chunk store: cannot remove the index run {num:016x}: {e}");
                }
            }
        }
        io.sync_dir()?;
        Ok(ix)
    }

    /// Set the rebuild flag in a durable MANIFEST (the same runs, hwm and
    /// counts): the next writable open rebuilds the index from the packs.
    /// The flag stays in RAM too, so a later flush keeps it. Reads no run.
    pub fn request_rebuild(&mut self, io: &dyn PackIo, reason: &str) -> Result<()> {
        tracing::error!("chunk store: index damage ({reason}); the index is rebuilt from the packs at the next writable open");
        self.man.flags |= FLAG_REBUILD;
        self.man.seq += 1;
        io.write_durable(0, Kind::Manifest, &self.man.encode())?;
        Ok(())
    }

    pub fn hwm(&self) -> Hwm {
        self.man.hwm
    }

    /// The counts as of the last flush.
    pub fn counts(&self) -> Counts {
        self.man.counts
    }

    pub fn mem_len(&self) -> usize {
        self.mem.len()
    }

    pub fn runs(&self) -> &[Arc<Run>] {
        &self.runs
    }

    /// The current place of `id`: the newest entry decides (a tombstone
    /// gives `None`). A run read error is returned at once: the search never
    /// goes on to an older run, whose put entry could be one that a
    /// tombstone in the unread page hides.
    pub fn get(&self, id: &ObjectId) -> Result<Option<Loc>> {
        if let Some(e) = self.mem.get(id) {
            return Ok(put_loc(*e));
        }
        for r in &self.runs {
            if let Some(e) = r.get(id)? {
                return Ok(put_loc(e));
            }
        }
        Ok(None)
    }

    /// Put `id` at `loc` in the memtable (no I/O).
    pub fn insert(&mut self, id: ObjectId, loc: Loc) {
        self.mem.insert(id, Entry::Put(loc));
    }

    /// Record that GC removed the entry of `id` in `pack` (no I/O). The
    /// caller has proved, with a complete read of the index under the same
    /// lock, that this entry is the current one. A put of `id` in another
    /// pack in the memtable is a newer copy and stays.
    ///
    /// Lookups and merges take the newest entry of an ID, so the tombstone
    /// hides exactly the entry it was written for, and no older entry of
    /// the same ID can come back.
    pub fn tombstone(&mut self, id: ObjectId, pack: u64) {
        if matches!(self.mem.get(&id), Some(Entry::Put(l)) if l.pack != pack) {
            return;
        }
        self.mem.insert(id, Entry::Tomb(pack));
    }

    /// Write the memtable to a new run and commit it with a new MANIFEST
    /// (the hwm `hwm`, the counts `counts`). Nothing to do if the memtable is
    /// empty. The caller has synced every pack record that the memtable
    /// points to: a run never points to bytes that are not durable.
    ///
    /// On an error the memtable and the old MANIFEST values stay. A partial
    /// run file from before the MANIFEST write is removed (best effort).
    /// After a failed MANIFEST write the run file stays: the write may still
    /// be durable, and then the MANIFEST lists that run.
    pub fn flush(&mut self, io: &dyn PackIo, hwm: Hwm, counts: Counts) -> Result<()> {
        if self.mem.is_empty() {
            return Ok(());
        }
        let num = self.man.next_run;
        let run = match self.write_run(io, num) {
            Ok(r) => r,
            Err(e) => {
                let _ = io.remove(num, Kind::Run);
                return Err(e);
            }
        };
        let mut man = self.man.clone();
        man.seq += 1;
        man.next_run = num + 1;
        man.hwm = hwm;
        man.counts = counts;
        man.runs.insert(0, RunMeta { num, len: run.len(), entries: run.entries() });
        man.obsolete.clear();
        io.write_durable(0, Kind::Manifest, &man.encode())?;
        self.man = man;
        self.runs.insert(0, Arc::new(run));
        self.mem.clear();
        Ok(())
    }

    /// Write the memtable to run `num`, make its name durable, and open it.
    /// Every step that can fail comes before the MANIFEST write.
    fn write_run(&self, io: &dyn PackIo, num: u64) -> Result<Run> {
        let mut w = RunWriter::create(io, num, self.mem.len() as u64)?;
        for (id, e) in &self.mem {
            w.push(id, e)?;
        }
        let info = w.finish()?;
        io.sync_dir()?;
        Run::open(io.open_run(num)?, num, info.len)
    }

    /// Every current put entry in ID order: the memtable and the runs merged,
    /// the newest entry of each ID wins, and tombstones hide. A run read
    /// error ends the iteration with that error.
    pub fn iter_all(&self) -> MergeIter<'_> {
        let mut srcs: Vec<Source<'_>> = Vec::with_capacity(1 + self.runs.len());
        srcs.push(Box::new(self.mem.iter().map(|(id, e)| Ok((*id, *e)))));
        for r in &self.runs {
            srcs.push(Box::new(r.iter()));
        }
        let heads = vec![None; srcs.len()];
        MergeIter { srcs, heads, started: false, done: false }
    }
}

type Source<'a> = Box<dyn Iterator<Item = Result<(ObjectId, Entry)>> + 'a>;

/// See [`Index::iter_all`]. The sources are in order from the newest (the
/// memtable) to the oldest run.
pub(crate) struct MergeIter<'a> {
    srcs: Vec<Source<'a>>,
    /// The next entry of each source.
    heads: Vec<Option<(ObjectId, Entry)>>,
    started: bool,
    done: bool,
}

impl MergeIter<'_> {
    fn fill(&mut self, i: usize) -> Result<()> {
        self.heads[i] = self.srcs[i].next().transpose()?;
        Ok(())
    }

    fn step(&mut self) -> Result<Option<(ObjectId, Loc)>> {
        if !self.started {
            for i in 0..self.srcs.len() {
                self.fill(i)?;
            }
            self.started = true;
        }
        loop {
            let Some(min) = self.heads.iter().flatten().map(|(id, _)| *id).min() else {
                return Ok(None);
            };
            let mut newest = None;
            for i in 0..self.heads.len() {
                if matches!(self.heads[i], Some((id, _)) if id == min) {
                    let (_, e) = self.heads[i].take().unwrap();
                    newest.get_or_insert(e);
                    self.fill(i)?;
                }
            }
            if let Some(Entry::Put(l)) = newest {
                return Ok(Some((min, l)));
            }
        }
    }
}

impl Iterator for MergeIter<'_> {
    type Item = Result<(ObjectId, Loc)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let r = self.step();
        if !matches!(r, Ok(Some(_))) {
            self.done = true;
        }
        r.transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::MemIo;
    use crate::DIGEST_LEN;

    fn id(i: u64) -> ObjectId {
        let mut d = [0u8; DIGEST_LEN];
        d[..8].copy_from_slice(&i.to_be_bytes());
        ObjectId::new(1, d)
    }

    fn loc(pack: u64, offset: u64) -> Loc {
        Loc { pack, offset, stored: 10, raw: 20, flags: 0 }
    }

    #[test]
    fn a_manifest_round_trips_and_damage_is_refused() {
        let m = Manifest {
            flags: FLAG_REBUILD,
            seq: 7,
            next_run: 9,
            hwm: Hwm { pack: 3, offset: 1234 },
            counts: Counts { objects: 5, raw_bytes: 6, stored_bytes: 7 },
            runs: vec![RunMeta { num: 8, len: 4096, entries: 3 }, RunMeta { num: 2, len: 8192, entries: 70 }],
            obsolete: vec![4, 5],
        };
        let b = m.encode();
        assert_eq!(Manifest::decode(&b).unwrap(), m);
        assert_eq!(Manifest::decode(&Manifest::empty().encode()).unwrap(), Manifest::empty());
        for at in 0..b.len() {
            let mut d = b.clone();
            d[at] ^= 0x40;
            assert!(matches!(Manifest::decode(&d), Err(Error::Corrupt(_))), "flip at {at}");
        }
        for len in 0..b.len() {
            assert!(Manifest::decode(&b[..len]).is_err(), "cut at {len}");
        }
        // Counts that do not fit, with a valid CRC.
        let reseal = |mut d: Vec<u8>| {
            d.truncate(d.len() - CRC);
            let crc = crc32c::crc32c(&d);
            d.extend_from_slice(&crc.to_le_bytes());
            d
        };
        for n in [3u32, u32::MAX] {
            let mut d = b.clone();
            d[FIXED..FIXED + 4].copy_from_slice(&n.to_le_bytes());
            assert!(Manifest::decode(&reseal(d)).is_err(), "run count {n}");
        }
        let mut d = b.clone();
        d[6..10].copy_from_slice(&2u32.to_le_bytes());
        assert!(Manifest::decode(&reseal(d)).is_err(), "unknown flag");
        let mut bad = m.clone();
        bad.next_run = 8;
        assert!(Manifest::decode(&bad.encode()).is_err(), "run number not below next_run");
    }

    #[test]
    fn the_newest_entry_wins_in_get_and_iter_all() {
        let mem = MemIo::new();
        let mut ix = Index::empty();
        let c = Counts::default();
        // Run 1: ids 0..10 in pack 1.
        for i in 0..10 {
            ix.insert(id(i), loc(1, 100 + i));
        }
        ix.flush(&mem, Hwm { pack: 2, offset: 0 }, c).unwrap();
        // Run 2: id 3 moved to pack 2, id 4 removed, id 20 new.
        ix.insert(id(3), loc(2, 14));
        ix.tombstone(id(4), 1);
        ix.insert(id(20), loc(2, 50));
        ix.flush(&mem, Hwm { pack: 3, offset: 0 }, c).unwrap();
        // Memtable: id 5 removed, id 3 put again in pack 3 (a tombstone of
        // its pack-2 entry must then not hide the newer copy).
        ix.tombstone(id(5), 1);
        ix.insert(id(6), loc(3, 14));
        ix.tombstone(id(6), 1);
        assert_eq!(ix.runs().len(), 2);

        assert_eq!(ix.get(&id(0)).unwrap(), Some(loc(1, 100)));
        assert_eq!(ix.get(&id(3)).unwrap(), Some(loc(2, 14)));
        assert_eq!(ix.get(&id(4)).unwrap(), None);
        assert_eq!(ix.get(&id(5)).unwrap(), None);
        assert_eq!(ix.get(&id(6)).unwrap(), Some(loc(3, 14)), "a newer copy survives a tombstone of an older pack");
        assert_eq!(ix.get(&id(20)).unwrap(), Some(loc(2, 50)));
        assert_eq!(ix.get(&id(99)).unwrap(), None);

        let all: Vec<(ObjectId, Loc)> = ix.iter_all().map(|r| r.unwrap()).collect();
        let mut want: Vec<(ObjectId, Loc)> = (0..10).filter(|i| ![3, 4, 5, 6].contains(i)).map(|i| (id(i), loc(1, 100 + i))).collect();
        want.extend([(id(3), loc(2, 14)), (id(6), loc(3, 14)), (id(20), loc(2, 50))]);
        want.sort_by_key(|(i, _)| *i);
        assert_eq!(all, want);

        // Reopen: the runs and the MANIFEST give the same answers.
        let back = Index::open(&mem, false).unwrap();
        assert_eq!(back.hwm(), Hwm { pack: 3, offset: 0 });
        assert_eq!(back.get(&id(3)).unwrap(), Some(loc(2, 14)));
        assert_eq!(back.get(&id(4)).unwrap(), None);
        assert_eq!(back.get(&id(5)).unwrap(), Some(loc(1, 105)), "the memtable tombstone was not flushed");
    }

    #[test]
    fn a_read_error_ends_get_and_iter_all_with_the_error() {
        let mem = MemIo::new();
        let mut ix = Index::empty();
        for i in 0..200 {
            ix.insert(id(i), loc(1, 100 + i));
        }
        ix.flush(&mem, Hwm { pack: 2, offset: 0 }, Counts::default()).unwrap();
        ix.tombstone(id(7), 1);
        ix.flush(&mem, Hwm { pack: 2, offset: 0 }, Counts::default()).unwrap();
        mem.set_fail_reads(true);
        // id 7: the newest run would say "removed"; the error must not turn
        // into the older run's put entry or into "not found".
        assert!(ix.get(&id(7)).is_err());
        let items: Vec<_> = ix.iter_all().collect();
        assert_eq!(items.len(), 1);
        assert!(items[0].is_err());
        mem.set_fail_reads(false);
        mem.fail_reads_after(2);
        let r: Result<Vec<_>> = ix.iter_all().collect();
        mem.clear_fail_reads_after();
        assert!(r.is_err(), "an error after some pages ends the iteration with the error");
    }

    #[test]
    fn the_obsolete_runs_of_the_manifest_are_removed_at_open() {
        let mem = MemIo::new();
        let mut ix = Index::empty();
        for n in 0..3 {
            ix.insert(id(n), loc(1, 100 + n));
            ix.flush(&mem, Hwm { pack: 1, offset: 200 + n }, Counts::default()).unwrap();
        }
        assert_eq!(mem.list_runs().unwrap().len(), 3);
        // A MANIFEST that replaced run 1 (as a merge does) and names it as
        // obsolete; its file is still there.
        let mut man = Manifest::decode(&mem.read(0, Kind::Manifest).unwrap()).unwrap();
        let old = man.runs.pop().unwrap();
        man.obsolete.push(old.num);
        man.seq += 1;
        mem.write_durable(0, Kind::Manifest, &man.encode()).unwrap();

        let ro = Index::open(&mem, true).unwrap();
        assert_eq!(ro.runs().len(), 2);
        assert!(mem.exists(old.num, Kind::Run), "a read-only open removes nothing");
        let ix = Index::open(&mem, false).unwrap();
        assert_eq!(ix.runs().len(), 2);
        assert!(!mem.exists(old.num, Kind::Run));
        assert_eq!(ix.get(&id(0)).unwrap(), None, "run 1 is not read");
        assert_eq!(ix.get(&id(2)).unwrap(), Some(loc(1, 102)));
    }

    #[test]
    fn a_failed_flush_keeps_the_memtable_and_the_old_manifest() {
        let mem = MemIo::new();
        let mut ix = Index::empty();
        ix.insert(id(1), loc(1, 14));
        ix.flush(&mem, Hwm { pack: 1, offset: 100 }, Counts::default()).unwrap();
        let before = mem.read(0, Kind::Manifest).unwrap();
        ix.insert(id(2), loc(1, 100));
        mem.fail_next_syncs(1); // the run sync
        assert!(ix.flush(&mem, Hwm { pack: 1, offset: 200 }, Counts::default()).is_err());
        assert_eq!(ix.mem_len(), 1);
        assert_eq!(mem.read(0, Kind::Manifest).unwrap(), before);
        assert_eq!(mem.list_runs().unwrap(), vec![1], "the partial run is removed");
        assert_eq!(ix.get(&id(2)).unwrap(), Some(loc(1, 100)));
        ix.flush(&mem, Hwm { pack: 1, offset: 200 }, Counts::default()).unwrap();
        assert_eq!(ix.runs().len(), 2);
        assert_eq!(Index::open(&mem, false).unwrap().get(&id(2)).unwrap(), Some(loc(1, 100)));
    }
}
