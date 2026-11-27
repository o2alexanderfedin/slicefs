//! POOL-03 (plan 21-03): a panic on a request thread aborts the process
//! before the unwind, and the next load replays the write log.
//!
//! Each test runs this binary again as a child process (the ignored test
//! `panic_child`, with `POOL_PANIC_CHILD=<mode>:<store dir>`), because the
//! abort ends the process. Only the child installs the abort hook; the
//! parent and the other tests do not (research Pitfall 2). The parent
//! kills a child that has not ended after `CHILD_LIMIT`.

mod common;

use std::fs::File;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use common::{new_fs_with, reload_and_replay};
use metadata::wal::WalConfig;
use slicefs_cli::filesystem::SliceFsFilesystem;
use slicefs_cli::handlers::{self as h, CreateResult, DataResult, EmptyResult, EntryResult, WriteResult};
use slicefs_cli::pool::{self, Pool};
use tempfile::TempDir;

const S_IFREG: u32 = 0o100_000;

/// The size of file A, written and fsynced before the panic.
const A_LEN: usize = 64 << 10;

/// The longest time a child may run; then the parent kills it. A child
/// run takes well under 1 s of its own; the start of the test binary can
/// take 5 to 20 s on a loaded host, so this limit is only for a hang.
const CHILD_LIMIT: Duration = Duration::from_secs(60);

/// The message of the planted panic in the create of B.
const PLANTED: &str = "planted panic in the create of b";

fn data(seed: u64, n: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..n).map(|_| { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x as u8 }).collect()
}

fn create(fs: &SliceFsFilesystem, name: &str) -> (u64, u64) {
    match h::handle_create(fs, 1, name, S_IFREG | 0o644, 0o022, 501, 20, libc::O_RDWR) {
        CreateResult::Ok(ino, fh, _) => (ino, fh),
        _ => panic!("create {name}"),
    }
}

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()
}

/// The result of one child run.
struct Child {
    status: ExitStatus,
    stdout: String,
    stderr: String,
    took: Duration,
    /// When the parent saw the end, in ms since the Unix epoch.
    ended_ms: u128,
}

impl Child {
    fn text(&self) -> String {
        format!("status {:?}, {:?}\n--- stdout\n{}--- stderr\n{}", self.status, self.took, self.stdout, self.stderr)
    }
}

/// Run `panic_child` in `mode` on `store`, with stdin from null and the
/// output to files (no pipe can fill). Kill it after `CHILD_LIMIT`.
fn run_child(mode: &str, store: &Path) -> Child {
    let out_dir = TempDir::new().unwrap();
    let (out, err) = (out_dir.path().join("out"), out_dir.path().join("err"));
    let start = Instant::now();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "panic_child", "--ignored", "--nocapture", "--test-threads", "1"])
        .env("POOL_PANIC_CHILD", format!("{mode}:{}", store.display()))
        .env("RUST_BACKTRACE", "0")
        .stdin(Stdio::null())
        .stdout(File::create(&out).unwrap())
        .stderr(File::create(&err).unwrap())
        .spawn()
        .unwrap();
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if start.elapsed() >= CHILD_LIMIT {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "the child ({mode}) did not end in {CHILD_LIMIT:?}; killed\n--- stdout\n{}--- stderr\n{}",
                std::fs::read_to_string(&out).unwrap_or_default(),
                std::fs::read_to_string(&err).unwrap_or_default()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    Child {
        status,
        stdout: std::fs::read_to_string(&out).unwrap(),
        stderr: std::fs::read_to_string(&err).unwrap(),
        took: start.elapsed(),
        ended_ms: now_ms(),
    }
}

/// The parent checks of a panic in the create of B: the child aborted at
/// the planted panic, no "reply" was sent, and a reload with replay gives
/// A with its content and no B.
fn check_abort_and_replay(mode: &str) {
    let dir = TempDir::new().unwrap();
    let c = run_child(mode, dir.path());
    assert_eq!(c.status.signal(), Some(libc::SIGABRT), "the child ended by SIGABRT: {}", c.text());
    assert!(c.stderr.contains(PLANTED), "the abort came from the planted panic: {}", c.text());
    assert!(c.stderr.contains("panicked: abort"), "the hook printed its line: {}", c.text());
    assert!(!c.stdout.contains("REPLY B"), "no reply after the half-done create: {}", c.text());
    assert!(!c.stdout.contains("CHILD END"), "the child did not go on: {}", c.text());

    let fs = reload_and_replay(dir.path());
    let a = match h::handle_lookup(&fs, 1, "a") {
        EntryResult::Ok(ino, _) => ino,
        _ => panic!("A is not there after the replay"),
    };
    match h::handle_read(&fs, a, 0, A_LEN as u32 + 1) {
        DataResult::Ok(got) => assert!(got == data(1, A_LEN), "A has its {A_LEN} bytes (got {})", got.len()),
        _ => panic!("read of A failed"),
    }
    assert!(
        matches!(h::handle_lookup(&fs, 1, "b"), EntryResult::Error(e) if e == libc::ENOENT),
        "the half-done B is not there after the replay"
    );
}

#[test]
fn a_worker_panic_aborts_and_the_remount_replays() {
    check_abort_and_replay("worker");
}

#[test]
fn a_reader_thread_panic_aborts_and_the_remount_replays() {
    check_abort_and_replay("reader");
}

#[test]
fn a_full_queue_that_stays_full_aborts() {
    let dir = TempDir::new().unwrap();
    let c = run_child("full", dir.path());
    assert_eq!(c.status.signal(), Some(libc::SIGABRT), "the child ended by SIGABRT: {}", c.text());
    // From the third submit to the end that the parent saw (the start of
    // the binary is not counted).
    let third: u128 = c.stdout.lines().find_map(|l| l.split_once("THIRD SUBMIT AT ")?.1.trim().parse().ok())
        .unwrap_or_else(|| panic!("the child printed no THIRD SUBMIT line: {}", c.text()));
    assert!(c.ended_ms.saturating_sub(third) < 10_000, "the abort came within 10 s of the third submit: {}", c.text());
    assert!(c.stderr.contains("queue stayed full"), "the submit limit printed its line: {}", c.text());
    assert!(!c.stdout.contains("SUBMIT RETURNED"), "the third submit did not return: {}", c.text());
}

#[test]
fn a_panic_on_another_thread_does_not_abort() {
    let dir = TempDir::new().unwrap();
    let c = run_child("other", dir.path());
    assert_eq!(c.status.code(), Some(0), "the child ended with status 0: {}", c.text());
    assert!(c.stdout.contains("ALIVE"), "the child went on after the panic: {}", c.text());
    assert!(!c.stderr.contains("panicked: abort"), "the hook did not abort: {}", c.text());
}

/// The child side. Returns at once when `POOL_PANIC_CHILD` is not set.
#[test]
#[ignore]
fn panic_child() {
    let Ok(arg) = std::env::var("POOL_PANIC_CHILD") else { return };
    let (mode, store) = arg.split_once(':').expect("POOL_PANIC_CHILD=<mode>:<store dir>");
    match mode {
        "worker" | "reader" => child_create_b(mode, Path::new(store)),
        "full" => child_full_queue(),
        "other" => child_other_thread(),
        _ => panic!("unknown child mode {mode}"),
    }
}

/// A is written, fsynced and released; then B is created on a request
/// thread (a pool worker, or a thread named `fuser-0` as the inline path)
/// with a panic at the half-done point (B linked, its record not appended).
fn child_create_b(mode: &str, store: &Path) {
    let fs = Arc::new(new_fs_with(store, WalConfig::PerOp));
    let (a, fa) = create(&fs, "a");
    assert!(matches!(h::handle_write(&fs, fa, 0, &data(1, A_LEN)), WriteResult::Ok(_)));
    assert!(matches!(h::handle_fsync(&fs, a, fa), EmptyResult::Ok));
    assert!(matches!(h::handle_release(&fs, a, fa), EmptyResult::Ok));

    pool::install_abort_hook();
    fs.test_set_mkdir_hook(Some(Arc::new(|| panic!("{PLANTED}"))));

    let (tx, rx) = mpsc::channel::<()>();
    let job_fs = Arc::clone(&fs);
    // As a request job: the gate around the handler (as `serve`), then the
    // reply after it.
    let job = move || {
        {
            let _op = job_fs.gate_shared();
            let _ = create(&job_fs, "b");
        }
        println!("REPLY B");
        let _ = tx.send(());
    };
    let pool = if mode == "worker" {
        let pool = Pool::new(2).unwrap();
        pool.submit(0, Box::new(job));
        Some(pool)
    } else {
        std::thread::Builder::new().name("fuser-0".into()).spawn(job).unwrap();
        None
    };
    let _ = rx.recv_timeout(Duration::from_secs(3));
    println!("CHILD END");
    // A crash form: no commit and no drop of the file system.
    std::mem::forget(pool);
    std::mem::forget(fs);
}

/// One worker parked in a job, one job in the single slot, then one more
/// submit: it must abort at the submit limit (300 ms here).
fn child_full_queue() {
    pool::install_abort_hook();
    let pool = Pool::with_limits(1, 1, 1 << 20, Duration::from_millis(300)).unwrap();
    let (started_tx, started_rx) = mpsc::channel::<()>();
    let (_park_tx, park_rx) = mpsc::channel::<()>();
    pool.submit(0, Box::new(move || {
        let _ = started_tx.send(());
        let _ = park_rx.recv_timeout(Duration::from_secs(10));
    }));
    started_rx.recv_timeout(Duration::from_secs(5)).expect("the worker took the first job");
    pool.submit(0, Box::new(|| {}));
    println!("THIRD SUBMIT AT {}", now_ms());
    pool.submit(0, Box::new(|| {}));
    println!("SUBMIT RETURNED");
    std::mem::forget(pool);
}

/// A panic on a thread that is not a request thread (the flusher name)
/// keeps the process alive.
fn child_other_thread() {
    pool::install_abort_hook();
    let h = std::thread::Builder::new()
        .name("slicefs-flusher".into())
        .spawn(|| panic!("planted panic on the flusher thread"))
        .unwrap();
    assert!(h.join().is_err(), "the flusher thread panicked");
    println!("ALIVE");
}
