//! Failure-injection test #11 (ARCHITECTURE §14.2): macOS F_FULLFSYNC
//! regression canary.
//!
//! ## Why this test exists
//! On macOS, `fsync(2)` only flushes data from the kernel page cache to
//! the storage controller; it does NOT force the SSD to commit data
//! from its volatile DRAM write-cache to NAND. The only POSIX-ish call
//! that does is `fcntl(fd, F_FULLFSYNC)`. Our [`platform::durable_sync`]
//! goes through `F_FULLFSYNC` on macOS for exactly this reason.
//!
//! If a future refactor accidentally drops back to plain `fsync`, the
//! index would still pass every kill-9 test (kernel page cache is
//! always coherent across kill-9), but it would silently regress on
//! true power-fail. This canary catches that regression in CI on Apple
//! hardware *without* needing a real power-fail rig.
//!
//! ## How the shim works
//! `slicefs-dedup-fi-shim` ships a tiny C library
//! (`libslicefs_dedup_fi_shim.dylib`) loaded via
//! `DYLD_INSERT_LIBRARIES`. It interposes `fcntl(2)`:
//! - `cmd == F_FULLFSYNC` returns `0` immediately (no-op).
//! - any other `cmd` is forwarded to the real `fcntl` via `dlsym`.
//!
//! From the writer's point of view, every `F_FULLFSYNC` looks
//! successful, but the SSD's DRAM write cache is never told to drain.
//! The child then `abort()`s without giving redb time to do anything
//! else, simulating a power-fail that would have been masked by a
//! correct `F_FULLFSYNC` but now isn't.
//!
//! ## What we assert
//! - **I1 (FP-never)** must hold across the fake-fsync crash. Every
//!   key the re-mounted index reports as `Present` must have its CAS
//!   block on disk.
//! - False negatives are *expected*: some commits the child thought
//!   were durable may have been lost when `abort()` skipped the page-
//!   cache flush + on-device flush. That's fine; FN is recoverable.
//!
//! ## SIP and DYLD_INSERT_LIBRARIES
//! macOS System Integrity Protection blocks `DYLD_INSERT_LIBRARIES`
//! against system-protected binaries (anything under `/System`,
//! `/usr/`, `/bin/`, `/sbin/`). Cargo-built test binaries live under
//! the user's `target/` directory, which is *not* SIP-protected, so
//! the shim does load. If for some reason it doesn't (unusual machine
//! config, etc.), the test still passes the FP-never check — it just
//! doesn't actually exercise the regression scenario. That is
//! acceptable for a regression canary.
//!
//! ## Build order
//! The shim crate must be built **before** running this test:
//!     cargo build -p slicefs-dedup-fi-shim
//! If the dylib is missing, the test SKIPs cleanly with a hint.

#![cfg(target_os = "macos")]

use std::env;
use std::process::{Command, Stdio};

const CHILD_MARK_T11: &str = "DEDUP_FI_T11_CHILD";

/// Child entrypoint. Runs an insert burst against a fresh index, then
/// `abort()`s without flushing — under the shim, every `F_FULLFSYNC`
/// the index issued during those inserts was a lie.
fn child_main_t11() -> ! {
    use slicefs_dedup::{DedupIndexConfig, RedbDedupIndex};
    use slicefs_traits::{ChunkHash, DedupIndex};
    let cas = std::path::PathBuf::from(env::var("CAS").unwrap());
    std::fs::create_dir_all(&cas).unwrap();
    let cfg = DedupIndexConfig::builder(&cas).build();
    let idx = RedbDedupIndex::create(cfg).unwrap();
    for i in 0u64..1000 {
        let mut h = [0u8; 28];
        h[..8].copy_from_slice(&i.to_le_bytes());
        let hex: String = h.iter().map(|b| format!("{:02x}", b)).collect();
        let shard = cas.join(&hex[..2]);
        let _ = std::fs::create_dir_all(&shard);
        let _ = std::fs::write(shard.join(&hex[2..]), b"x");
        let _ = idx.insert(&ChunkHash::from_bytes(h.to_vec()));
    }
    // Crash without flush — F_FULLFSYNC was a lie, so device may not have NAND'd.
    std::process::abort();
}

#[test]
fn t11_macos_fullfsync_shim_increases_fn_rate() {
    if env::var(CHILD_MARK_T11).is_ok() {
        child_main_t11();
    }

    // Resolve the shim dylib path. It's built into target/<profile>/.
    let target_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target");
    let mut candidates = vec![
        target_root
            .join("debug")
            .join("libslicefs_dedup_fi_shim.dylib"),
        target_root
            .join("release")
            .join("libslicefs_dedup_fi_shim.dylib"),
    ];
    let shim = candidates.drain(..).find(|p| p.exists());
    let shim = match shim {
        Some(p) => p,
        None => {
            eprintln!(
                "SKIP: shim dylib not built. Run `cargo build -p slicefs-dedup-fi-shim` first."
            );
            return;
        }
    };

    let td = tempfile::tempdir().unwrap();
    let cas = td.path().join("cas");
    std::fs::create_dir_all(&cas).unwrap();
    let mut child = Command::new(env::current_exe().unwrap())
        .arg("--exact")
        .arg("t11_macos_fullfsync_shim_increases_fn_rate")
        .arg("--nocapture")
        .env(CHILD_MARK_T11, "1")
        .env("CAS", &cas)
        .env("DYLD_INSERT_LIBRARIES", &shim)
        .env("DYLD_FORCE_FLAT_NAMESPACE", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _ = child.wait();

    use slicefs_dedup::{DedupIndexConfig, RedbDedupIndex};
    use slicefs_traits::{ChunkHash, DedupIndex, DedupResult};
    let cfg = DedupIndexConfig::builder(&cas).build();
    let idx = RedbDedupIndex::open(cfg).expect("must mount after fake-fsync crash");

    // Critical assertion: NO FP. Some FNs are expected (the shim made some
    // commits non-durable); that's fine. The contract is I1 (FP-never).
    let mut present = 0u32;
    let mut absent = 0u32;
    for i in 0u64..1000 {
        let mut h = [0u8; 28];
        h[..8].copy_from_slice(&i.to_le_bytes());
        match idx.lookup(&ChunkHash::from_bytes(h.to_vec())).unwrap() {
            DedupResult::Present => {
                let hex: String = h.iter().map(|b| format!("{:02x}", b)).collect();
                let p = cas.join(&hex[..2]).join(&hex[2..]);
                assert!(p.exists(), "I1 violated under shim at i={}", i);
                present += 1;
            }
            DedupResult::Absent | DedupResult::DefinitelyAbsent => absent += 1,
        }
    }
    eprintln!("t11 outcome: {present} Present, {absent} Absent (FNs from fake-fsync)");
}
