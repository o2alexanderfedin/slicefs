//! Index run files on real files (FileIo, a temporary directory): runs are
//! mapped for lookups, and merges and GC installs replace them and remove
//! the old files. A mapping of a removed file stays valid; a mapping of a
//! file that is cut or written in place is not (SIGBUS or wrong bytes).
//! These tests check that the store only removes its sealed runs and never
//! writes or cuts them, that lookups stay right while runs are replaced, and
//! that a run file that is short at open is damage (a rebuild), not SIGBUS.
//! No mounts.

use chunk_store::hash::{Blake3_256, ObjectHasher};
use chunk_store::pack_io::{FileIo, Kind, PackFile, PackIo, PackRead, RunMap};
use chunk_store::retry::RetryingIo;
use chunk_store::{ChunkStore, ObjectId, Options};
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn random(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

fn files(dir: &Path) -> Arc<dyn PackIo> {
    Arc::new(RetryingIo::new(Arc::new(FileIo::new(dir.join("packs")))))
}

fn sorted(mut v: Vec<u64>) -> Vec<u64> {
    v.sort_unstable();
    v
}

/// What the guard saw on the run files.
#[derive(Default)]
struct Seen {
    /// Run numbers that were mapped and not removed since.
    mapped: HashSet<u64>,
    /// Writes, cuts or replacements of a run file that may be mapped.
    violations: Vec<String>,
}

/// Files, with checks on the run files: a run file is never written, cut
/// or replaced after a mapping of it was made (only removed), and is never
/// written with `write_durable`.
struct Guard {
    inner: Arc<dyn PackIo>,
    seen: Arc<Mutex<Seen>>,
    removed_runs: Arc<AtomicU64>,
}

struct GuardedRun {
    inner: Box<dyn PackFile>,
    num: u64,
    seen: Arc<Mutex<Seen>>,
}

impl GuardedRun {
    fn check(&self, what: &str) {
        let mut s = self.seen.lock().unwrap();
        if s.mapped.contains(&self.num) {
            s.violations.push(format!("{what} of run {:016x} while it is mapped", self.num));
        }
    }
}

impl PackRead for GuardedRun {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.inner.read_at(buf, offset)
    }
}

impl PackFile for GuardedRun {
    fn write_at(&mut self, data: &[u8], offset: u64) -> io::Result<()> {
        self.check("write_at");
        self.inner.write_at(data, offset)
    }
    fn set_len(&mut self, len: u64) -> io::Result<()> {
        self.check("set_len");
        self.seen.lock().unwrap().violations.push(format!("set_len({len}) of run {:016x}", self.num));
        self.inner.set_len(len)
    }
    fn sync(&mut self) -> io::Result<()> {
        self.inner.sync()
    }
}

impl PackIo for Guard {
    fn prepare(&self, read_only: bool) -> io::Result<bool> {
        self.inner.prepare(read_only)
    }
    fn list(&self) -> io::Result<Vec<u64>> {
        self.inner.list()
    }
    fn exists(&self, num: u64, kind: Kind) -> bool {
        self.inner.exists(num, kind)
    }
    fn read(&self, num: u64, kind: Kind) -> io::Result<Vec<u8>> {
        self.inner.read(num, kind)
    }
    fn write_durable(&self, num: u64, kind: Kind, data: &[u8]) -> io::Result<()> {
        if kind == Kind::Run {
            self.seen.lock().unwrap().violations.push(format!("write_durable of run {num:016x}"));
        }
        self.inner.write_durable(num, kind, data)
    }
    fn remove(&self, num: u64, kind: Kind) -> io::Result<()> {
        let r = self.inner.remove(num, kind);
        if kind == Kind::Run && r.is_ok() {
            self.seen.lock().unwrap().mapped.remove(&num);
            self.removed_runs.fetch_add(1, Ordering::SeqCst);
        }
        r
    }
    fn sync_dir(&self) -> io::Result<()> {
        self.inner.sync_dir()
    }
    fn create(&self, num: u64) -> io::Result<Box<dyn PackFile>> {
        self.inner.create(num)
    }
    fn open_rw(&self, num: u64) -> io::Result<Box<dyn PackFile>> {
        self.inner.open_rw(num)
    }
    fn open_read(&self, num: u64) -> io::Result<Arc<dyn PackRead>> {
        self.inner.open_read(num)
    }
    fn can_punch(&self) -> bool {
        self.inner.can_punch()
    }
    fn punch(&self, num: u64, ranges: &[(u64, u64)]) -> io::Result<()> {
        self.inner.punch(num, ranges)
    }
    fn list_runs(&self) -> io::Result<Vec<u64>> {
        self.inner.list_runs()
    }
    fn create_run(&self, num: u64) -> io::Result<Box<dyn PackFile>> {
        if self.seen.lock().unwrap().mapped.contains(&num) {
            self.seen.lock().unwrap().violations.push(format!("create_run of mapped run {num:016x}"));
        }
        let inner = self.inner.create_run(num)?;
        Ok(Box::new(GuardedRun { inner, num, seen: Arc::clone(&self.seen) }))
    }
    fn open_run(&self, num: u64) -> io::Result<Arc<dyn PackRead>> {
        self.inner.open_run(num)
    }
    fn map_run(&self, num: u64) -> io::Result<Option<RunMap>> {
        let m = self.inner.map_run(num)?;
        if m.is_some() {
            self.seen.lock().unwrap().mapped.insert(num);
        }
        Ok(m)
    }
}

/// Sets the flag when dropped, so that the reader stops also when the
/// writer panics.
struct StopOnDrop<'a>(&'a AtomicBool);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// A reader does lookups all the time on files while puts (flushes and
/// background merges), a GC install and a full merge replace and remove
/// the run files. Each lookup gives the right bytes or a correct miss; the
/// store never writes, cuts or replaces a run file that it mapped; and a
/// mapping that the test holds of each old run keeps the bytes the file had.
#[test]
fn lookups_on_files_stay_right_while_merges_and_gc_replace_the_runs() {
    let dir = tempfile::tempdir().unwrap();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let removed = Arc::new(AtomicU64::new(0));
    let io = Arc::new(Guard { inner: files(dir.path()), seen: Arc::clone(&seen), removed_runs: Arc::clone(&removed) });
    // About 25 flushes in all: each one syncs (F_FULLFSYNC on macOS).
    let o = Options { pack_limit: 128 << 10, index_memtable_limit: 150, ..Options::default() };
    let s = ChunkStore::open_with(io, o.clone()).unwrap();

    let put = |from: u64, n: u64| -> Vec<(ObjectId, Vec<u8>)> {
        (from..from + n)
            .map(|i| {
                let d = random(600, i);
                (s.put(&d).unwrap(), d)
            })
            .collect()
    };
    // Read with `get` during the GC. A read keeps its object and the pack
    // of that object (a read during a GC counts as a use), so these are in
    // packs of their own, which the GC does not change anyway.
    let stable = put(0, 400);
    s.gc_begin().unwrap(); // seals the active pack
    s.gc_abort();
    // Looked up with `contains` (not a use), so the GC can change their packs.
    let first = put(100_000, 1200);
    s.wait_index_idle();
    let runs_before = sorted(files(dir.path()).list_runs().unwrap());
    assert!(runs_before.len() >= 2, "runs: {runs_before:?}");

    // Mappings and copies of the old runs, held by the test.
    let fio = FileIo::new(dir.path().join("packs"));
    let held: Vec<(u64, RunMap, Vec<u8>)> = runs_before
        .iter()
        .map(|&n| {
            let bytes = std::fs::read(dir.path().join("index").join(format!("{n:016x}.run"))).unwrap();
            (n, fio.map_run(n).unwrap().unwrap(), bytes)
        })
        .collect();

    // Keep one object in three: the packs are mostly dead, so the GC
    // compacts them (copies the live records, then deletes the packs).
    let live: Vec<(ObjectId, Vec<u8>)> = first.iter().enumerate().filter(|(i, _)| i % 3 == 0).map(|(_, x)| x.clone()).collect();
    let victims: Vec<ObjectId> = first.iter().enumerate().filter(|(i, _)| i % 3 != 0).map(|(_, (id, _))| *id).collect();
    let never: Vec<ObjectId> = (0..200u64).map(|i| Blake3_256.id(&random(600, 1_000_000 + i))).collect();

    let stop = AtomicBool::new(false);
    let gc_done = AtomicBool::new(false);
    let lookups = AtomicU64::new(0);
    // The run removal counts that the reader saw at its lookups.
    let removals_seen = Mutex::new(HashSet::new());

    std::thread::scope(|sc| {
        let reader = sc.spawn(|| {
            let deadline = Instant::now() + Duration::from_secs(60);
            let mut i = 0usize;
            while !stop.load(Ordering::SeqCst) && Instant::now() < deadline {
                removals_seen.lock().unwrap().insert(removed.load(Ordering::SeqCst));
                let (id, d) = &stable[i % stable.len()];
                assert_eq!(s.get(id).as_deref(), Some(d.as_slice()), "a stable object {i}");
                assert!(s.contains(&live[i % live.len()].0), "a live object {i}");
                let n = &never[i % never.len()];
                assert!(!s.contains(n), "an object that was never put");
                let v = &victims[i % victims.len()];
                if gc_done.load(Ordering::SeqCst) {
                    assert!(!s.contains(v), "an object that the GC removed");
                }
                lookups.fetch_add(1, Ordering::SeqCst);
                i += 1;
            }
        });

        {
            let _stop = StopOnDrop(&stop);
            let more = put(10_000, 1200);
            let keep: HashSet<ObjectId> = stable.iter().chain(&live).chain(&more).map(|(id, _)| *id).collect();
            let b = s.gc_begin().unwrap();
            let st = s.gc_finish(&keep, b).unwrap();
            assert_eq!(st.removed_objects, victims.len() as u64);
            gc_done.store(true, Ordering::SeqCst);
            assert!(s.merge_index_all().unwrap(), "a full merge replaces every run");
            put(20_000, 300);
            s.wait_index_idle();
            // Let the reader see the final state too.
            let at = lookups.load(Ordering::SeqCst);
            let t = Instant::now();
            while lookups.load(Ordering::SeqCst) < at + 50 && t.elapsed() < Duration::from_secs(10) && !reader.is_finished() {
                std::thread::yield_now();
            }
        }
        reader.join().unwrap();
    });

    let seen = seen.lock().unwrap();
    assert!(seen.violations.is_empty(), "{:?}", seen.violations);
    assert!(removed.load(Ordering::SeqCst) > 0, "no run file was removed");
    let runs_after = sorted(files(dir.path()).list_runs().unwrap());
    let total = removed.load(Ordering::SeqCst);
    let seen_counts = removals_seen.into_inner().unwrap();
    eprintln!(
        "runs {runs_before:?} -> {runs_after:?}, {total} removed, {} lookups, removal counts seen: {}",
        lookups.load(Ordering::SeqCst),
        seen_counts.len()
    );
    assert!(
        seen_counts.iter().any(|&c| c > 0 && c < total),
        "the reader made no lookup between the first and the last run removal"
    );
    // Each old run file was removed, and its mapping still has its bytes.
    for (n, map, bytes) in &held {
        assert!(!runs_after.contains(n), "old run {n:016x} is still listed");
        assert!(!dir.path().join("index").join(format!("{n:016x}.run")).exists());
        assert!(map.bytes() == bytes.as_slice(), "the mapping of removed run {n:016x} changed");
    }
    drop(seen);

    // And after a reopen.
    s.close().unwrap();
    drop(s);
    let s = ChunkStore::open_with(files(dir.path()), o).unwrap();
    for (id, d) in stable.iter().chain(&live) {
        assert_eq!(s.get(id).as_deref(), Some(d.as_slice()));
    }
    assert!(victims.iter().all(|v| !s.contains(v)));
}

/// Cut one run file of a closed store on disk (as damage or another
/// program could), then reopen. The MANIFEST still gives the old length.
fn cut_run_and_reopen(cut: impl Fn(u64) -> u64, what: &str) {
    let dir = tempfile::tempdir().unwrap();
    let o = Options { pack_limit: 128 << 10, index_memtable_limit: 300, index_background_merge: false, ..Options::default() };
    let objs: Vec<(ObjectId, Vec<u8>)> = {
        let s = ChunkStore::open(dir.path(), o.clone()).unwrap();
        let objs = (0..700u64)
            .map(|i| {
                let d = random(600, i);
                (s.put(&d).unwrap(), d)
            })
            .collect();
        s.close().unwrap();
        objs
    };
    let runs = sorted(files(dir.path()).list_runs().unwrap());
    assert!(runs.len() >= 2, "{what}: runs {runs:?}");
    let victim = runs[runs.len() / 2];
    let path = dir.path().join("index").join(format!("{victim:016x}.run"));
    let len = std::fs::metadata(&path).unwrap().len();
    let new_len = cut(len);
    assert!(new_len < len, "{what}");
    OpenOptions::new().write(true).open(&path).unwrap().set_len(new_len).unwrap();

    let check = |s: &ChunkStore, how: &str| {
        assert_eq!(s.stats().objects, objs.len() as u64, "{what}, {how}");
        for (id, d) in &objs {
            assert_eq!(s.get(id).as_deref(), Some(d.as_slice()), "{what}, {how}");
        }
    };

    // A read-only open (a tool next to a mount) reads every pack and
    // changes nothing on disk.
    {
        let ro = Options { read_only: true, ..o.clone() };
        let s = ChunkStore::open(dir.path(), ro).unwrap();
        check(&s, "read-only open");
        assert_eq!(sorted(files(dir.path()).list_runs().unwrap()), runs, "{what}: a read-only open removes nothing");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), new_len);
    }
    // A writable open builds the index again.
    {
        let s = ChunkStore::open(dir.path(), o.clone()).unwrap();
        check(&s, "writable open");
        s.close().unwrap();
    }
    let rebuilt = sorted(files(dir.path()).list_runs().unwrap());
    assert!(rebuilt.iter().all(|n| !runs.contains(n)), "{what}: the old runs are replaced: {runs:?} -> {rebuilt:?}");
    // The new index is clean: a second open uses it as is.
    let s = ChunkStore::open(dir.path(), o).unwrap();
    check(&s, "second writable open");
    s.close().unwrap();
    assert_eq!(sorted(files(dir.path()).list_runs().unwrap()), rebuilt, "{what}: no second rebuild");
}

#[test]
fn a_run_file_cut_before_the_open_is_rebuilt_not_mapped_past_its_end() {
    cut_run_and_reopen(|_| 0, "cut to 0 bytes");
    cut_run_and_reopen(|_| 10, "cut to less than a footer");
    cut_run_and_reopen(|l| l - 4096, "one page short");
    cut_run_and_reopen(|l| l - 3, "3 bytes short");
    cut_run_and_reopen(|l| l / 2, "half");
}

/// A sanity check of the guard: it sees a write to a mapped run.
#[test]
fn the_guard_sees_a_write_to_a_mapped_run() {
    let dir = tempfile::tempdir().unwrap();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let g = Guard { inner: files(dir.path()), seen: Arc::clone(&seen), removed_runs: Arc::new(AtomicU64::new(0)) };
    g.prepare(false).unwrap();
    let mut f = g.create_run(7).unwrap();
    f.write_at(&[1; 64], 0).unwrap();
    f.sync().unwrap();
    assert!(seen.lock().unwrap().violations.is_empty());
    let _m = g.map_run(7).unwrap();
    f.write_at(&[2; 64], 0).unwrap();
    assert_eq!(seen.lock().unwrap().violations.len(), 1);
}
