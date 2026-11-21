//! Spike S1 probe: does the FUSE transport overlap requests, and does it accept
//! replies in a different order than the requests came in?
//!
//! One reader thread (the fuser session loop) hands every request to a pool of 4
//! workers. A READ of `slow0`..`slow9` is delayed 200 ms in the worker. All other
//! requests are answered at once. Zero TTL on every reply, so a new name always
//! reaches this server. The server prints on stdout:
//!   S1 RESULT arrived=<n> max_inflight=<n> reply_order_inversions=<n>
//! and per-request ARR/REP lines on stderr.
//!
//! usage: s1_probe <mountpoint>      (run by scripts/macos-mount-test/s1-overlap.sh)
use fuser::{
    Config, FileAttr, FileHandle, FileType, Filesystem, Generation, INodeNo, LockOwner, MountOption, OpenFlags,
    ReplyAttr, ReplyData, ReplyEntry, Request, SessionACL,
};
use std::ffi::OsStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

/// First inode of the slow files (`slow0` is 10, `slow9` is 19).
const SLOW_BASE: u64 = 10;
const SLOW_COUNT: u64 = 10;

enum Kind {
    Lookup(ReplyEntry, u64),
    Attr(ReplyAttr, u64),
    Read(ReplyData, bool),
}
struct Job {
    seq: u64,
    unique: u64,
    kind: Kind,
}
#[derive(Default)]
struct Stats {
    arrived: AtomicU64,
    inflight: AtomicU64,
    max_inflight: AtomicU64,
    last_replied: AtomicU64,
    inversions: AtomicU64,
    printed: AtomicBool,
}
impl Stats {
    /// Print the S1 RESULT line once (from `destroy` or when `mount` returns).
    fn print_result(&self) {
        if self.printed.swap(true, SeqCst) {
            return;
        }
        println!(
            "S1 RESULT arrived={} max_inflight={} reply_order_inversions={}",
            self.arrived.load(SeqCst),
            self.max_inflight.load(SeqCst),
            self.inversions.load(SeqCst)
        );
    }
}
struct Probe {
    tx: Mutex<Sender<Job>>,
    st: Arc<Stats>,
    t0: Instant,
}

fn is_slow(ino: u64) -> bool {
    (SLOW_BASE..SLOW_BASE + SLOW_COUNT).contains(&ino)
}
fn attr(ino: u64, kind: FileType) -> FileAttr {
    FileAttr {
        ino: INodeNo(ino),
        size: 4096,
        blocks: 8,
        atime: UNIX_EPOCH,
        mtime: UNIX_EPOCH,
        ctime: UNIX_EPOCH,
        crtime: UNIX_EPOCH,
        kind,
        perm: 0o755,
        nlink: 1,
        uid: 501,
        gid: 20,
        rdev: 0,
        flags: 0,
        blksize: 4096,
    }
}
/// `slow<N>` with N in 0..10 maps to inode 10+N; every other name to inode 3.
fn ino_for(name: &OsStr) -> u64 {
    name.to_str()
        .and_then(|s| s.strip_prefix("slow"))
        .and_then(|n| n.parse::<u64>().ok())
        .filter(|n| *n < SLOW_COUNT)
        .map_or(3, |n| SLOW_BASE + n)
}

impl Probe {
    /// Runs on the single fuser reader thread.
    fn submit(&self, req: &Request, kind: Kind) {
        let seq = self.st.arrived.fetch_add(1, SeqCst) + 1;
        let n = self.st.inflight.fetch_add(1, SeqCst) + 1;
        self.st.max_inflight.fetch_max(n, SeqCst);
        let unique = req.unique().0;
        eprintln!("ARR seq={seq} unique={unique} inflight={n} t_ms={}", self.t0.elapsed().as_millis());
        self.tx.lock().unwrap().send(Job { seq, unique, kind }).unwrap();
    }
}
impl Filesystem for Probe {
    fn lookup(&self, req: &Request, _p: INodeNo, name: &OsStr, reply: ReplyEntry) {
        self.submit(req, Kind::Lookup(reply, ino_for(name)));
    }
    fn getattr(&self, req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        self.submit(req, Kind::Attr(reply, ino.0));
    }
    fn read(
        &self,
        req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        _o: u64,
        _s: u32,
        _f: OpenFlags,
        _l: Option<LockOwner>,
        reply: ReplyData,
    ) {
        self.submit(req, Kind::Read(reply, is_slow(ino.0)));
    }
    fn destroy(&mut self) {
        self.st.print_result();
    }
}

/// Record that request `seq` is done. This runs BEFORE the reply is sent: a
/// client that waits for the reply can then only see `inflight` too low, never
/// too high, so a serial transport cannot show `max_inflight` of 2.
fn finish(st: &Stats, seq: u64) {
    // A reply whose seq is lower than one already sent is an inversion.
    if seq < st.last_replied.fetch_max(seq, SeqCst) {
        st.inversions.fetch_add(1, SeqCst);
    }
    st.inflight.fetch_sub(1, SeqCst);
}

fn worker(rx: Arc<Mutex<Receiver<Job>>>, st: Arc<Stats>, t0: Instant) {
    loop {
        let Ok(job) = rx.lock().unwrap().recv() else { return };
        let (seq, unique) = (job.seq, job.unique);
        match job.kind {
            Kind::Read(reply, slow) => {
                if slow {
                    std::thread::sleep(Duration::from_millis(200));
                }
                finish(&st, seq);
                reply.data(&[0u8; 4096]);
            }
            Kind::Lookup(reply, ino) => {
                finish(&st, seq);
                reply.entry(&Duration::ZERO, &attr(ino, FileType::RegularFile), Generation(0))
            }
            Kind::Attr(reply, ino) => {
                finish(&st, seq);
                reply.attr(
                    &Duration::ZERO,
                    &attr(ino, if ino == 1 { FileType::Directory } else { FileType::RegularFile }),
                )
            }
        }
        eprintln!("REP seq={seq} unique={unique} t_ms={}", t0.elapsed().as_millis());
    }
}

fn main() {
    let (tx, rx) = channel();
    let rx = Arc::new(Mutex::new(rx));
    let st = Arc::new(Stats::default());
    let t0 = Instant::now();
    for _ in 0..4 {
        let (rx, st) = (rx.clone(), st.clone());
        std::thread::spawn(move || worker(rx, st, t0));
    }
    let fs = Probe { tx: Mutex::new(tx), st: st.clone(), t0 };
    let mut cfg = Config::default();
    cfg.mount_options = vec![MountOption::FSName("s1probe".into())];
    #[cfg(target_os = "macos")]
    cfg.mount_options.push(MountOption::CUSTOM("backend=fskit".into()));
    cfg.acl = SessionACL::Owner;
    let r = fuser::mount(fs, std::env::args().nth(1).expect("mountpoint"), &cfg);
    eprintln!("mount returned {r:?}");
    st.print_result();
}
