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
//! Concurrency: the index is shared (`Arc<Index>`) by the store and by
//! lookups that run outside the store lock. The memtable has [`SHARDS`]
//! shards by digest prefix, each with its own lock; the run list is an
//! `Arc` snapshot behind a read-write lock, so a lookup holds no lock while
//! it reads a run page. Every change of the memtable, the runs and the
//! MANIFEST happens under the store lock (lock order: the store lock, then
//! one shard lock or the run-list lock; never the reverse, and a lookup
//! never holds two of them). A generation counter ([`Index::gen`]) goes up
//! at each run install and GC install, so that a caller can see under the
//! store lock that the answer of a lookup outside it may be stale.
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
use crate::run::{Entry, Run, RunWriter, PER_PAGE};
use crate::{Error, Loc, ObjectId, Result};
use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

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
/// Bloom filter bits per key of new runs, unless the store sets another.
pub(crate) const DEFAULT_BLOOM_BITS: u8 = 10;
/// Memtable shards (by the high 4 bits of digest byte 0).
pub(crate) const SHARDS: usize = 16;
/// The idle merge (#111) acts only when the index has more runs than this.
pub(crate) const IDLE_TARGET_RUNS: usize = 2;
/// The idle merge takes an older run into its window only if the newer
/// runs of the window have at least 1/this of its entries: a run is
/// rewritten only for new data of at least 1/this of its size, so the
/// bytes that the idle merges write stay bounded per byte of new data.
pub(crate) const IDLE_MAX_REWRITE: u64 = 4;
/// No new entry and no flush, merge or GC for this long: the store is
/// idle, and the merge thread runs the idle merge (#111).
pub(crate) const IDLE_MERGE_AFTER: std::time::Duration = std::time::Duration::from_secs(5);

fn shard_of(id: &ObjectId) -> usize {
    (id.digest()[0] >> 4) as usize
}

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
    shards: [Mutex<BTreeMap<ObjectId, Entry>>; SHARDS],
    /// Entries in the shards (changed only under the store lock).
    mem_len: AtomicUsize,
    /// From the newest to the oldest, as in `man.runs`. A lookup clones the
    /// `Arc` and reads pages with no lock held.
    runs: RwLock<Arc<Vec<Arc<Run>>>>,
    man: Mutex<Manifest>,
    /// See [`Index::gen`].
    gen: AtomicU64,
    /// Bloom filter bits per key of new runs (0: no filter).
    bloom_bits: u8,
    /// Bytes of the merge outputs installed since the open.
    merge_bytes: AtomicU64,
}

/// A copy of the memtable and a snapshot of the runs, for a full pass.
pub(crate) struct Snapshot {
    mem: Vec<(ObjectId, Entry)>,
    runs: Arc<Vec<Arc<Run>>>,
}

impl Snapshot {
    /// Every current put entry in ID order: the memtable and the runs merged,
    /// the newest entry of each ID wins, and tombstones hide. A run read
    /// error ends the iteration with that error.
    pub fn iter_all(&self) -> MergeIter<'_> {
        let mut srcs: Vec<Source<'_>> = Vec::with_capacity(1 + self.runs.len());
        srcs.push(Box::new(self.mem.iter().map(|(id, e)| Ok((*id, *e)))));
        for r in self.runs.iter() {
            srcs.push(Box::new(r.iter()));
        }
        MergeIter { newest: Newest::new(srcs), done: false }
    }

    /// The current place of `id` in the snapshot (a tombstone gives
    /// `None`), through the verified page reads of the runs (every page CRC
    /// checked, after the filter): damage is always `Corrupt`, and a read
    /// error is returned, never taken as "not found". For GC decisions.
    pub fn get_verified(&self, id: &ObjectId) -> Result<Option<Loc>> {
        if let Ok(i) = self.mem.binary_search_by_key(id, |(k, _)| *k) {
            return Ok(put_loc(self.mem[i].1));
        }
        for r in self.runs.iter() {
            if !r.may_contain(id) {
                continue;
            }
            if let Some(e) = r.get_verified(id)? {
                return Ok(put_loc(e));
            }
        }
        Ok(None)
    }
}

/// A merge of runs that [`Index::pick_merge`] chose under the store lock.
pub(crate) struct MergePlan {
    /// The input runs: adjacent in the run list, from the newest to the
    /// oldest.
    inputs: Vec<Arc<Run>>,
    /// The number of the output run (taken from the next run number of the
    /// MANIFEST in RAM, so no flush can take it).
    out: u64,
    /// The window holds the oldest run: nothing older exists for a
    /// tombstone to hide, so the output drops the tombstones.
    drop_tombs: bool,
}

/// The output of a merge (see [`Index::write_merge`]).
pub(crate) enum MergeOut {
    /// The new run, durable with its name.
    Run(Run),
    /// Nothing is left (every entry was a dropped tombstone): the inputs go
    /// and no run replaces them.
    Empty,
}

/// The idle merge window (#111) of runs with these entry counts (from the
/// newest to the oldest): the number of newest runs to merge, or 0 for no
/// merge. Nothing if there are at most `target` runs. Else the window
/// starts with the newest run and takes the next older run while the
/// window has at least 1/`max_rewrite` of that run's entries; it must hold
/// at least 2 runs. Only a prefix of the list (newest runs) merges, so the
/// newest entry of an ID stays the newest.
pub fn idle_window(entries: &[u64], target: usize, max_rewrite: u64) -> usize {
    if entries.len() <= target.max(1) {
        return 0;
    }
    let mut sum = entries[0];
    let mut k = 1;
    while k < entries.len() && sum.saturating_mul(max_rewrite.max(1)) >= entries[k] {
        sum += entries[k];
        k += 1;
    }
    if k >= 2 {
        k
    } else {
        0
    }
}

/// The tier of a run of `entries` entries: the integer floor of log base
/// `fan_in` of max(1, entries / `limit`). Runs of one tier have a similar
/// size; a merge of `fan_in` of them gives a run of the next tier.
pub fn tier(entries: u64, limit: u64, fan_in: u64) -> u32 {
    let fan_in = fan_in.max(2);
    let mut n = (entries / limit.max(1)).max(1);
    let mut t = 0;
    while n >= fan_in {
        n /= fan_in;
        t += 1;
    }
    t
}

impl Index {
    fn with(runs: Vec<Arc<Run>>, man: Manifest) -> Self {
        Index {
            shards: std::array::from_fn(|_| Mutex::new(BTreeMap::new())),
            mem_len: AtomicUsize::new(0),
            runs: RwLock::new(Arc::new(runs)),
            man: Mutex::new(man),
            gen: AtomicU64::new(0),
            bloom_bits: DEFAULT_BLOOM_BITS,
            merge_bytes: AtomicU64::new(0),
        }
    }

    /// An index with nothing in it (a store with no files).
    pub fn empty() -> Self {
        Self::with(Vec::new(), Manifest::empty())
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
            let map = match io.map_run(r.num) {
                Ok(m) => m,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    return Ok(Err(Damage::MissingRun(format!("the MANIFEST lists run {:016x}, which is missing", r.num))))
                }
                Err(e) => return Err(e.into()),
            };
            let run = match Run::open(file, map, r.num, r.len) {
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
        Ok(Ok(Index::with(runs, man)))
    }

    /// The last steps of a good open: the rebuild flag, and for a writable
    /// open the removal of runs that the MANIFEST does not use.
    fn opened(io: &dyn PackIo, read_only: bool, mut ix: Index) -> Result<Index> {
        if ix.rebuild_requested() {
            return Self::rebuild(io, read_only, "a rebuild of the index was requested");
        }
        if read_only {
            return Ok(ix);
        }
        let on_disk = io.list_runs()?;
        let man = ix.man.get_mut().unwrap();
        // A run file that a failed flush left behind must not block the next
        // flush with the same number (also if its removal below fails).
        if let Some(max) = on_disk.iter().max() {
            man.next_run = man.next_run.max(max.saturating_add(1));
        }
        let live: Vec<u64> = man.runs.iter().map(|r| r.num).collect();
        let mut gone = man.obsolete.clone();
        gone.extend(on_disk.into_iter().filter(|n| !live.contains(n) && !man.obsolete.contains(n)));
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
        ix.man.get_mut().unwrap().next_run = on_disk.iter().max().map_or(1, |m| m.saturating_add(1));
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
    /// The caller holds the store lock.
    pub fn request_rebuild(&self, io: &dyn PackIo, reason: &str) -> Result<()> {
        tracing::error!("chunk store: index damage ({reason}); the index is rebuilt from the packs at the next writable open");
        let mut man = self.man.lock().unwrap();
        man.flags |= FLAG_REBUILD;
        man.seq += 1;
        io.write_durable(0, Kind::Manifest, &man.encode())?;
        Ok(())
    }

    /// True if the MANIFEST asks for a rebuild at the next writable open.
    pub fn rebuild_requested(&self) -> bool {
        self.man.lock().unwrap().flags & FLAG_REBUILD != 0
    }

    /// Bloom filter bits per key of the runs that this index writes from
    /// now on (0: no filter). Runs on disk keep their own.
    pub fn set_bloom_bits(&mut self, bits: u8) {
        self.bloom_bits = bits;
    }

    pub fn hwm(&self) -> Hwm {
        self.man.lock().unwrap().hwm
    }

    /// The counts as of the last flush.
    pub fn counts(&self) -> Counts {
        self.man.lock().unwrap().counts
    }

    pub fn mem_len(&self) -> usize {
        self.mem_len.load(Ordering::SeqCst)
    }

    /// Bytes of the merge outputs installed since the open (the write cost
    /// of the merges; for tests and tools).
    pub fn merge_bytes(&self) -> u64 {
        self.merge_bytes.load(Ordering::SeqCst)
    }

    /// The live runs now, from the newest to the oldest.
    pub fn runs(&self) -> Arc<Vec<Arc<Run>>> {
        Arc::clone(&self.runs.read().unwrap())
    }

    /// The generation: it goes up, under the store lock, after each run
    /// install (a flush or a merge) and after each GC install (the
    /// tombstones and new places of a GC are durable, before any pack
    /// changes). A caller reads it before a lookup outside the store lock;
    /// if it is the same under the lock, and the memtable has no entry of
    /// the ID, the answer of the lookup is still the current one: every
    /// other change of an entry is a memtable insert under the store lock.
    pub fn gen(&self) -> u64 {
        self.gen.load(Ordering::SeqCst)
    }

    /// See [`Index::gen`]. The caller holds the store lock.
    pub fn bump_gen(&self) {
        self.gen.fetch_add(1, Ordering::SeqCst);
    }

    fn shard(&self, id: &ObjectId) -> &Mutex<BTreeMap<ObjectId, Entry>> {
        &self.shards[shard_of(id)]
    }

    /// True if the memtable has an entry (put or tombstone) of `id`.
    pub fn mem_has(&self, id: &ObjectId) -> bool {
        self.shard(id).lock().unwrap().contains_key(id)
    }

    /// The current place of `id`: the newest entry decides (a tombstone
    /// gives `None`). A run read error is returned at once: the search never
    /// goes on to an older run, whose put entry could be one that a
    /// tombstone in the unread page hides.
    ///
    /// Safe outside the store lock: the shard first, then a snapshot of the
    /// runs. A flush installs its run before it clears the shards, so an
    /// entry that moves from the memtable to a run is seen in one of them.
    pub fn get(&self, id: &ObjectId) -> Result<Option<Loc>> {
        let e = self.shard(id).lock().unwrap().get(id).copied();
        if let Some(e) = e {
            return Ok(put_loc(e));
        }
        for r in self.runs().iter() {
            if let Some(e) = r.get(id)? {
                return Ok(put_loc(e));
            }
        }
        Ok(None)
    }

    /// [`Index::get`] through the verified page reads of the runs (every
    /// page CRC checked, in the runs whose filter may have the ID): damage
    /// is always `Corrupt`.
    pub fn get_verified(&self, id: &ObjectId) -> Result<Option<Loc>> {
        let e = self.shard(id).lock().unwrap().get(id).copied();
        if let Some(e) = e {
            return Ok(put_loc(e));
        }
        for r in self.runs().iter() {
            if !r.may_contain(id) {
                continue;
            }
            if let Some(e) = r.get_verified(id)? {
                return Ok(put_loc(e));
            }
        }
        Ok(None)
    }

    /// Put `id` at `loc` in the memtable (no I/O). The caller holds the
    /// store lock.
    pub fn insert(&self, id: ObjectId, loc: Loc) {
        self.set(id, Entry::Put(loc));
    }

    fn set(&self, id: ObjectId, e: Entry) {
        if self.shard(&id).lock().unwrap().insert(id, e).is_none() {
            self.mem_len.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// A tombstone of the entry of `id` in `pack` in the memtable (no I/O),
    /// unless the memtable has a put of `id` in another pack (a newer copy).
    /// For tests: GC writes its tombstones to pending runs on disk (see
    /// [`Index::write_pending`]).
    #[cfg(test)]
    pub fn tombstone(&self, id: ObjectId, pack: u64) {
        let newer = matches!(self.shard(&id).lock().unwrap().get(&id), Some(Entry::Put(l)) if l.pack != pack);
        if !newer {
            self.set(id, Entry::Tomb(pack));
        }
    }

    /// The memtable entries in ID order (a copy). The shards split by
    /// digest, but IDs sort by tag first, so the copy is sorted as a whole.
    fn sorted_mem(&self) -> Vec<(ObjectId, Entry)> {
        let mut v = Vec::with_capacity(self.mem_len());
        for s in &self.shards {
            v.extend(s.lock().unwrap().iter().map(|(id, e)| (*id, *e)));
        }
        v.sort_unstable_by_key(|(id, _)| *id);
        v
    }

    /// A copy of the memtable and a snapshot of the runs. The caller holds
    /// the store lock, so the two agree.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot { mem: self.sorted_mem(), runs: self.runs() }
    }

    /// Write the memtable to a new run and commit it with a new MANIFEST
    /// (the hwm `hwm`, the counts `counts`). Nothing to do if the memtable is
    /// empty. The caller holds the store lock, and has synced every pack
    /// record that the memtable points to: a run never points to bytes that
    /// are not durable.
    ///
    /// The entries stay visible to lookups outside the store lock: the new
    /// run goes into the run list first, then the shards are cleared, then
    /// the generation goes up.
    ///
    /// On an error the memtable and the old MANIFEST values stay. A partial
    /// run file from before the MANIFEST write is removed (best effort).
    /// After a failed MANIFEST write the run file stays: the write may still
    /// be durable, and then the MANIFEST lists that run.
    pub fn flush(&self, io: &dyn PackIo, hwm: Hwm, counts: Counts) -> Result<()> {
        if self.mem_len() == 0 {
            return Ok(());
        }
        let entries = self.sorted_mem();
        let mut man = self.man.lock().unwrap();
        let num = man.next_run;
        let run = match Self::write_run(io, num, &entries, self.bloom_bits) {
            Ok(r) => r,
            Err(e) => {
                let _ = io.remove(num, Kind::Run);
                return Err(e);
            }
        };
        let mut new = man.clone();
        new.seq += 1;
        new.next_run = num + 1;
        new.hwm = hwm;
        new.counts = counts;
        new.runs.insert(0, RunMeta { num, len: run.len(), entries: run.entries() });
        new.obsolete.clear();
        io.write_durable(0, Kind::Manifest, &new.encode())?;
        *man = new;
        drop(man);
        {
            let mut runs = self.runs.write().unwrap();
            let mut v = Vec::with_capacity(runs.len() + 1);
            v.push(Arc::new(run));
            v.extend(runs.iter().cloned());
            *runs = Arc::new(v);
        }
        for s in &self.shards {
            s.lock().unwrap().clear();
        }
        self.mem_len.store(0, Ordering::SeqCst);
        self.bump_gen();
        Ok(())
    }

    /// The next merge, chosen under the store lock (`None`: no merge).
    ///
    /// The policy (size-tiered): walk the runs from the oldest to the
    /// newest; the first window of `fan_in` adjacent runs of one tier (see
    /// [`tier`]) is the next merge. With `all`, the window is every run (also
    /// one run alone if it has tombstones: the merge drops them). Only
    /// adjacent runs merge, so the newest entry of an ID stays the newest.
    ///
    /// No merge while a rebuild is requested: the index is then damaged,
    /// and the next writable open rebuilds it from the packs.
    pub fn pick_merge(&self, fan_in: usize, limit: usize, all: bool) -> Option<MergePlan> {
        if self.rebuild_requested() {
            return None;
        }
        let runs = self.runs();
        let n = runs.len();
        let window = if all {
            if n == 0 || (n == 1 && runs[0].tombstones() == 0) {
                return None;
            }
            0..n
        } else {
            let f = fan_in.max(2);
            if n < f {
                return None;
            }
            let tiers: Vec<u32> = runs.iter().map(|r| tier(r.entries(), limit as u64, f as u64)).collect();
            // `end`: one past the oldest run of the window (the list is in
            // order from the newest to the oldest).
            let end = (f..=n).rev().find(|&end| tiers[end - f..end].iter().all(|&t| t == tiers[end - 1]))?;
            end - f..end
        };
        let mut man = self.man.lock().unwrap();
        let out = man.next_run;
        man.next_run += 1;
        Some(MergePlan { inputs: runs[window.clone()].to_vec(), out, drop_tombs: window.end == n })
    }

    /// The window of the idle merge now (see [`idle_window`]), or `None`.
    fn idle_range(&self) -> Option<std::ops::Range<usize>> {
        if self.rebuild_requested() {
            return None;
        }
        let runs = self.runs();
        let entries: Vec<u64> = runs.iter().map(|r| r.entries()).collect();
        let k = idle_window(&entries, IDLE_TARGET_RUNS, IDLE_MAX_REWRITE);
        (k >= 2).then_some(0..k)
    }

    /// True if the idle merge has a window now (no lock needed: a hint for
    /// the merge thread; [`Index::pick_idle_merge`] decides).
    pub fn idle_merge_due(&self) -> bool {
        self.idle_range().is_some()
    }

    /// The idle merge (#111), chosen under the store lock (`None`: no
    /// merge): the newest runs of [`idle_window`] into one. Tombstones are
    /// dropped if the window holds the oldest run.
    pub fn pick_idle_merge(&self) -> Option<MergePlan> {
        let window = self.idle_range()?;
        let runs = self.runs();
        let mut man = self.man.lock().unwrap();
        let out = man.next_run;
        man.next_run += 1;
        Some(MergePlan { inputs: runs[window.clone()].to_vec(), out, drop_tombs: window.end == runs.len() })
    }

    /// Write the merge of `plan` to its output run, with no lock held. The
    /// inputs are read through the verified page reads (every page CRC
    /// checked); for each ID the entry of the newest input wins; tombstones
    /// are dropped only if the window holds the oldest run. The output is
    /// synced (every 64 MiB and at the end) and its name made durable.
    ///
    /// A read error of an input ends the merge with that error: a page is
    /// never skipped (a skipped tombstone would bring back an older entry).
    /// `Ok(None)`: `stop` was set. After an error or a stop, the partial
    /// output is removed (best effort) and nothing else changed.
    pub fn write_merge(&self, io: &dyn PackIo, plan: &MergePlan, stop: &AtomicBool) -> Result<Option<MergeOut>> {
        let out = plan.out;
        let r = (|| {
            let expected: u64 = plan.inputs.iter().map(|r| r.entries()).sum();
            let mut w = RunWriter::create(io, out, expected, self.bloom_bits)?;
            let srcs: Vec<Source<'_>> = plan.inputs.iter().map(|r| Box::new(r.iter()) as Source<'_>).collect();
            let mut merged = Newest::new(srcs);
            let mut seen = 0usize;
            while let Some((id, e)) = merged.next_entry()? {
                // About once per input page.
                if seen.is_multiple_of(PER_PAGE) && stop.load(Ordering::SeqCst) {
                    return Ok(None);
                }
                seen += 1;
                if plan.drop_tombs && matches!(e, Entry::Tomb(_)) {
                    continue;
                }
                w.push(&id, &e)?;
            }
            let info = w.finish()?;
            if info.entries == 0 {
                io.remove(out, Kind::Run)?;
                return Ok(Some(MergeOut::Empty));
            }
            io.sync_dir()?;
            Ok(Some(MergeOut::Run(Run::open(io.open_run(out)?, io.map_run(out)?, out, info.len)?)))
        })();
        if !matches!(r, Ok(Some(_))) {
            let _ = io.remove(out, Kind::Run);
        }
        r
    }

    /// Install the merge `plan` with its output `out`. The caller holds the
    /// store lock. The new run list and MANIFEST come from the state now
    /// (runs flushed during the merge stay): the window is replaced by the
    /// output. The MANIFEST keeps the hwm, the counts and the flags (a merge
    /// changes no content) and lists the inputs as obsolete; it is written
    /// with `write_durable` before anything else changes.
    ///
    /// Returns the input run numbers, to remove after the store lock is
    /// released (readers that hold the old runs keep reading them), or
    /// `None` if the window is no longer in the run list (then the output is
    /// removed). After a failed MANIFEST write the output file stays (the
    /// write may still be durable, and then that MANIFEST lists it); its
    /// number is never used again, and a writable open removes it if no
    /// MANIFEST lists it.
    pub fn install_merge(&self, io: &dyn PackIo, plan: &MergePlan, out: MergeOut) -> Result<Option<Vec<u64>>> {
        let nums: Vec<u64> = plan.inputs.iter().map(|r| r.num()).collect();
        let k = nums.len();
        let cur = self.runs();
        let mut man = self.man.lock().unwrap();
        let same = |it: &mut dyn Iterator<Item = u64>| it.eq(nums.iter().copied());
        let at = cur.windows(k).position(|w| same(&mut w.iter().map(|r| r.num()))).filter(|&at| {
            man.runs.get(at..at + k).is_some_and(|m| same(&mut m.iter().map(|r| r.num)))
                && (!plan.drop_tombs || at + k == cur.len())
        });
        let Some(at) = at else {
            drop(man);
            if matches!(out, MergeOut::Run(_)) {
                let _ = io.remove(plan.out, Kind::Run);
            }
            return Ok(None);
        };
        let run = match out {
            MergeOut::Run(r) => Some(r),
            MergeOut::Empty => None,
        };
        let mut new = man.clone();
        new.seq += 1;
        let meta = run.as_ref().map(|r| RunMeta { num: r.num(), len: r.len(), entries: r.entries() });
        new.runs.splice(at..at + k, meta);
        new.obsolete = nums.clone();
        io.write_durable(0, Kind::Manifest, &new.encode())?;
        *man = new;
        drop(man);
        if let Some(r) = &run {
            self.merge_bytes.fetch_add(r.len(), Ordering::SeqCst);
        }
        {
            let mut runs = self.runs.write().unwrap();
            let mut v: Vec<Arc<Run>> = runs.iter().cloned().collect();
            v.splice(at..at + k, run.map(Arc::new));
            *runs = Arc::new(v);
        }
        self.bump_gen();
        Ok(Some(nums))
    }

    /// Remove the input runs of an installed merge (its MANIFEST is
    /// durable). A failed removal is only logged: the MANIFEST lists them as
    /// obsolete, and a writable open removes every run that the MANIFEST
    /// does not list.
    pub fn remove_runs(io: &dyn PackIo, nums: &[u64]) {
        for &num in nums {
            match io.remove(num, Kind::Run) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => {
                    tracing::warn!("chunk store: cannot remove the merged index run {num:016x}: {e}")
                }
                _ => {}
            }
        }
    }

    /// Write `entries` to run `num`, make its name durable, and open it.
    /// Every step that can fail comes before the MANIFEST write.
    fn write_run(io: &dyn PackIo, num: u64, entries: &[(ObjectId, Entry)], bloom_bits: u8) -> Result<Run> {
        let mut w = RunWriter::create(io, num, entries.len() as u64, bloom_bits)?;
        for (id, e) in entries {
            w.push(id, e)?;
        }
        let info = w.finish()?;
        io.sync_dir()?;
        Run::open(io.open_run(num)?, io.map_run(num)?, num, info.len)
    }

    // ── GC (17-07) ───────────────────────────────────────────────────────────

    /// A new run number (taken from the next run number of the MANIFEST in
    /// RAM, so no flush or merge can take it). The caller holds the store
    /// lock.
    pub fn take_run_num(&self) -> u64 {
        let mut man = self.man.lock().unwrap();
        let num = man.next_run;
        man.next_run += 1;
        num
    }

    /// Write a pending GC run `num`: a tombstone `(id, pack)` for each
    /// victim, sorted by ID, synced, and opened. No lock is needed: the run
    /// is in no MANIFEST until [`Index::install_gc`], so lookups never see
    /// it, and an open after a stop removes it as an orphan. Its name is
    /// made durable by the `sync_dir` before the install. On an error the
    /// partial file is removed (best effort).
    pub fn write_pending(&self, io: &dyn PackIo, num: u64, victims: &[(ObjectId, u64)]) -> Result<Run> {
        let r = (|| {
            let mut w = RunWriter::create(io, num, victims.len() as u64, self.bloom_bits)?;
            for (id, pack) in victims {
                w.push(id, &Entry::Tomb(*pack))?;
            }
            let info = w.finish()?;
            Run::open(io.open_run(num)?, io.map_run(num)?, num, info.len)
        })();
        if r.is_err() {
            let _ = io.remove(num, Kind::Run);
        }
        r
    }

    /// The GC install, under the store lock: write the memtable to a run
    /// (if it has entries), then one MANIFEST with the pending GC runs as
    /// the newest runs, that run right after them, and the runs of the
    /// current list (also those that a flush or merge installed during the
    /// GC). This MANIFEST write is the durable point of a GC.
    ///
    /// Order: a victim's put entry can still be in the memtable (it was put
    /// before the GC and not flushed), so the tombstones must be newer than
    /// the memtable run. No entry in the memtable is newer than a tombstone
    /// of a pending run: an object put or read during the GC drops the plan
    /// of its pack (and with it the pending run) before this call.
    ///
    /// The caller has synced the active pack and the directory (so the
    /// pending runs have durable names). `hwm` is the end of the active pack
    /// (or the start of the next pack): the new hwm, also if the memtable is
    /// empty; `counts` are the counts after the GC. On an error before the
    /// MANIFEST write, a partial memtable run is
    /// removed; after a failed MANIFEST write the files stay (the write may
    /// be durable). Either way the index in RAM does not change.
    ///
    /// Returns the numbers of the added runs, from the newest.
    pub fn install_gc(&self, io: &dyn PackIo, pending: Vec<Arc<Run>>, hwm: Hwm, counts: Counts) -> Result<Vec<u64>> {
        let mut man = self.man.lock().unwrap();
        let mem_run = if self.mem_len() > 0 {
            let num = man.next_run;
            let entries = self.sorted_mem();
            match Self::write_run(io, num, &entries, self.bloom_bits) {
                Ok(r) => Some(Arc::new(r)),
                Err(e) => {
                    let _ = io.remove(num, Kind::Run);
                    return Err(e);
                }
            }
        } else {
            None
        };
        let mut new = man.clone();
        new.seq += 1;
        if let Some(r) = &mem_run {
            new.next_run = new.next_run.max(r.num() + 1);
        }
        // Also with an empty memtable: the hwm must move past every pack
        // that the GC changes (the hwm pack must exist, and its records
        // below the hwm must have no gap; see `covers` in lib.rs). Every
        // record below `hwm` is then in a run.
        new.hwm = hwm;
        new.counts = counts;
        let added: Vec<Arc<Run>> = pending.into_iter().chain(mem_run).collect();
        let metas = added.iter().map(|r| RunMeta { num: r.num(), len: r.len(), entries: r.entries() });
        new.runs.splice(0..0, metas);
        new.obsolete.clear();
        io.write_durable(0, Kind::Manifest, &new.encode())?;
        *man = new;
        drop(man);
        let nums = added.iter().map(|r| r.num()).collect();
        {
            let mut runs = self.runs.write().unwrap();
            let mut v = added;
            v.extend(runs.iter().cloned());
            *runs = Arc::new(v);
        }
        for s in &self.shards {
            s.lock().unwrap().clear();
        }
        self.mem_len.store(0, Ordering::SeqCst);
        self.bump_gen();
        Ok(nums)
    }

    /// A merge of exactly the runs `nums` (adjacent in the run list, from
    /// the newest to the oldest), chosen under the store lock; `None` if
    /// they are no longer adjacent there, or a rebuild is requested. For the
    /// runs that one GC install added. The output keeps the tombstones also
    /// if the window holds the oldest run: the size-tiered merges drop them
    /// later, as for any run.
    pub fn plan_merge_of(&self, nums: &[u64]) -> Option<MergePlan> {
        if self.rebuild_requested() || nums.len() < 2 {
            return None;
        }
        let runs = self.runs();
        let k = nums.len();
        let at = runs.windows(k).position(|w| w.iter().map(|r| r.num()).eq(nums.iter().copied()))?;
        let mut man = self.man.lock().unwrap();
        let out = man.next_run;
        man.next_run += 1;
        Some(MergePlan { inputs: runs[at..at + k].to_vec(), out, drop_tombs: false })
    }
}

type Source<'a> = Box<dyn Iterator<Item = Result<(ObjectId, Entry)>> + 'a>;

/// A merge of sorted sources, given in order from the newest to the oldest:
/// each ID once, with the entry of the newest source that has it
/// (tombstones included). The one place of the newest-wins rule, for
/// [`Snapshot::iter_all`] and for merges. A source error is returned.
struct Newest<'a> {
    srcs: Vec<Source<'a>>,
    /// The next entry of each source.
    heads: Vec<Option<(ObjectId, Entry)>>,
    started: bool,
}

impl<'a> Newest<'a> {
    fn new(srcs: Vec<Source<'a>>) -> Self {
        let heads = vec![None; srcs.len()];
        Newest { srcs, heads, started: false }
    }

    fn fill(&mut self, i: usize) -> Result<()> {
        self.heads[i] = self.srcs[i].next().transpose()?;
        Ok(())
    }

    fn next_entry(&mut self) -> Result<Option<(ObjectId, Entry)>> {
        if !self.started {
            for i in 0..self.srcs.len() {
                self.fill(i)?;
            }
            self.started = true;
        }
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
        Ok(newest.map(|e| (min, e)))
    }
}

/// See [`Snapshot::iter_all`]: the put entries of [`Newest`].
pub(crate) struct MergeIter<'a> {
    newest: Newest<'a>,
    done: bool,
}

impl Iterator for MergeIter<'_> {
    type Item = Result<(ObjectId, Loc)>;

    fn next(&mut self) -> Option<Self::Item> {
        while !self.done {
            match self.newest.next_entry() {
                Ok(Some((id, Entry::Put(l)))) => return Some(Ok((id, l))),
                Ok(Some((_, Entry::Tomb(_)))) => {}
                Ok(None) => self.done = true,
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            }
        }
        None
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
        let ix = Index::empty();
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

        let all: Vec<(ObjectId, Loc)> = ix.snapshot().iter_all().map(|r| r.unwrap()).collect();
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
        let ix = Index::empty();
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
        let items: Vec<_> = ix.snapshot().iter_all().collect();
        assert_eq!(items.len(), 1);
        assert!(items[0].is_err());
        mem.set_fail_reads(false);
        mem.fail_reads_after(2);
        let r: Result<Vec<_>> = ix.snapshot().iter_all().collect();
        mem.clear_fail_reads_after();
        assert!(r.is_err(), "an error after some pages ends the iteration with the error");
    }

    #[test]
    fn the_obsolete_runs_of_the_manifest_are_removed_at_open() {
        let mem = MemIo::new();
        let ix = Index::empty();
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

    // A merge whose stop flag is set (close or Drop of the store) writes
    // nothing that stays: the partial output is removed, the runs and the
    // MANIFEST do not change.
    #[test]
    fn a_stopped_merge_leaves_no_output() {
        let mem = MemIo::new();
        let ix = Index::empty();
        for n in 0..4 {
            for i in 0..100 {
                ix.insert(id(n * 100 + i), loc(1, 100 + n * 100 + i));
            }
            ix.flush(&mem, Hwm { pack: 1, offset: 1000 + n }, Counts::default()).unwrap();
        }
        let mut runs = mem.list_runs().unwrap();
        runs.sort_unstable();
        let manifest = mem.read(0, Kind::Manifest).unwrap();
        let plan = ix.pick_merge(4, 100, false).expect("four runs of one tier");
        assert!(ix.write_merge(&mem, &plan, &AtomicBool::new(true)).unwrap().is_none());
        let mut after = mem.list_runs().unwrap();
        after.sort_unstable();
        assert_eq!(after, runs, "the partial output is removed");
        assert_eq!(mem.read(0, Kind::Manifest).unwrap(), manifest);
        assert_eq!(ix.runs().len(), 4);
        // The next merge works, with a new output number.
        let plan = ix.pick_merge(4, 100, false).unwrap();
        let out = ix.write_merge(&mem, &plan, &AtomicBool::new(false)).unwrap().unwrap();
        assert_eq!(ix.install_merge(&mem, &plan, out).unwrap(), Some(runs.iter().rev().copied().collect()));
        assert_eq!(ix.runs().len(), 1);
        assert_eq!(ix.get(&id(250)).unwrap(), Some(loc(1, 350)));
    }

    #[test]
    fn the_idle_window_rewrites_a_run_only_for_enough_new_data() {
        let w = |e: &[u64]| idle_window(e, 2, 4);
        // At most the target: nothing.
        assert_eq!(w(&[]), 0);
        assert_eq!(w(&[1, 100]), 0);
        // The runs of a long create (size-tiered, sizes in flushes): all.
        assert_eq!(w(&[1, 1, 4, 16, 16]), 5);
        assert_eq!(w(&[1, 4, 4, 4, 16, 16, 16, 64, 256]), 9);
        // A small flush does not pull in a run more than 4 times its size.
        assert_eq!(w(&[1, 8, 100]), 0);
        assert_eq!(w(&[1, 1, 8, 100]), 3, "2 * 4 >= 8; 10 * 4 < 100");
        assert_eq!(w(&[1, 1, 1, 1, 100]), 4);
        assert_eq!(w(&[25, 100]), 0, "two runs: at the target");
        assert_eq!(w(&[1, 24, 100]), 0);
        assert_eq!(w(&[6, 24, 100]), 3);
    }

    #[test]
    fn a_failed_flush_keeps_the_memtable_and_the_old_manifest() {
        let mem = MemIo::new();
        let ix = Index::empty();
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
