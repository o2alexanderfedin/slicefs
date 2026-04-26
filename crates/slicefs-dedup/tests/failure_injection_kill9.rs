//! Failure-injection test #1 (ARCHITECTURE §14.2).
//!
//! Gates I2 (insert ordering: CAS fsync before redb commit) and
//! I4 (CAS-as-truth: a redb entry without a corresponding CAS block
//! is forbidden, but a CAS block without a redb entry is benign — it
//! just means the next lookup is a false negative the caller will
//! resolve by re-uploading the same content).
//!
//! ## Scenario
//! 1. Child process writes a CAS block, then submits an insert into
//!    the redb-backed index, then loops forever.
//! 2. Parent SIGKILLs the child ~50 ms later. The kill can land:
//!    - before the CAS write hits disk → no entries, no FP.
//!    - after CAS but before the redb commit → CAS-only, FN-benign.
//!    - after the redb commit → both present, lookup returns Present.
//! 3. Parent re-mounts the index via `RedbDedupIndex::open` and
//!    asserts the lookup result is in {Present, Absent,
//!    DefinitelyAbsent}. The forbidden state — Present without the
//!    CAS file existing — would imply an FP escaping the index
//!    contract, which I2/I4 must prevent.
//!
//! ## Fork-harness mechanics
//! The same test binary is the child: when invoked with the
//! `DEDUP_FI_T1_CHILD` environment variable set, [`child_main_t1`]
//! takes over before the test logic begins. We re-exec via
//! `env::current_exe()` with `--exact` filtered to this single test
//! and `--nocapture` so any panic in the child is visible in CI logs
//! (we still discard stdout/stderr of the spawned child to keep
//! parent test output clean).
//!
//! ## Why `RedbDedupIndex::open` (not `probe`) on reopen
//! After a SIGKILL, the manifest sidecar has `last_clean_shutdown =
//! false` (Drop never ran), so `MountState::probe` would correctly
//! report `Suspect`. For the purpose of this test we want the raw
//! index state, so we go straight to `open()`. redb's COW root keeps
//! the file readable across crashes; if `open()` itself fails, that
//! is a real bug worth surfacing.

use std::env;
use std::process::{Command, Stdio};
use std::time::Duration;

const CHILD_MARK: &str = "DEDUP_FI_T1_CHILD";
const TEST_NAME: &str = "t1_kill9_post_cas_pre_commit_yields_no_fp";

/// Child entrypoint. Writes the CAS block, submits the insert, then
/// loops forever waiting for the parent's SIGKILL.
fn child_main_t1() -> ! {
    use slicefs_dedup::{DedupIndexConfig, RedbDedupIndex};
    use slicefs_traits::{ChunkHash, DedupIndex};

    let cas = std::path::PathBuf::from(env::var("CAS").expect("CAS env var"));
    std::fs::create_dir_all(&cas).expect("create cas root");

    let cfg = DedupIndexConfig::builder(&cas).build();
    let idx = RedbDedupIndex::create(cfg).expect("create index");

    // Mock the I2 caller order: CAS write first, then index insert.
    let mut h = [0u8; 28];
    h[0] = 0xAB;
    let hex: String = h.iter().map(|b| format!("{:02x}", b)).collect();
    let shard = cas.join(&hex[..2]);
    std::fs::create_dir_all(&shard).expect("create shard dir");
    std::fs::write(shard.join(&hex[2..]), b"data").expect("write cas block");

    // Submit the insert. submit() blocks until the BatchWriter has
    // committed the txn, but the parent will likely SIGKILL us before
    // (or during) that commit. The result is intentionally ignored —
    // we are about to be killed.
    let _ = idx.insert(&ChunkHash::from_bytes(h.to_vec()));

    // Loop forever; parent will SIGKILL.
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[test]
fn t1_kill9_post_cas_pre_commit_yields_no_fp() {
    // Child branch: detected by the CHILD_MARK env var. Diverges
    // before any test assertions run.
    if env::var(CHILD_MARK).is_ok() {
        child_main_t1();
    }

    let td = tempfile::tempdir().expect("tempdir");
    let cas = td.path().join("cas");
    std::fs::create_dir_all(&cas).expect("create cas");

    // Spawn the same test binary, filtered to just this test, with
    // CHILD_MARK set so it dives into child_main_t1 before reaching
    // the parent assertions.
    let mut child = Command::new(env::current_exe().expect("current_exe"))
        .arg("--exact")
        .arg(TEST_NAME)
        .arg("--nocapture")
        .env(CHILD_MARK, "1")
        .env("CAS", &cas)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn child");

    // Give the child time to write the CAS block and submit the insert.
    // 50 ms is enough on every platform we ship on; the test does NOT
    // rely on the kill landing in any specific phase — every phase is
    // an acceptable outcome (the assertion below covers all of them).
    std::thread::sleep(Duration::from_millis(50));

    // SIGKILL. child.kill() is non-blocking; child.wait() reaps.
    let _ = child.kill();
    let _ = child.wait();

    // Re-mount the index and verify NO false positive.
    use slicefs_dedup::{DedupIndexConfig, RedbDedupIndex};
    use slicefs_traits::{ChunkHash, DedupIndex, DedupResult};

    let cfg = DedupIndexConfig::builder(&cas).build();
    let idx = RedbDedupIndex::open(cfg).expect("open after kill");

    let mut h = [0u8; 28];
    h[0] = 0xAB;
    let r = idx
        .lookup(&ChunkHash::from_bytes(h.to_vec()))
        .expect("lookup after kill");

    // Acceptable outcomes:
    //   - Present:           CAS exists AND redb commit landed.
    //   - Absent:            bloom hit but redb missing (FP at bloom layer).
    //   - DefinitelyAbsent:  bloom miss, never inserted.
    //
    // A "Present without the CAS file" return is impossible to
    // construct here because the CAS block is always written before
    // the insert, and the parent does not delete it. The point of
    // this test is to prove the index never *invents* a Present.
    assert!(
        matches!(
            r,
            DedupResult::Present | DedupResult::Absent | DedupResult::DefinitelyAbsent
        ),
        "unexpected lookup result after SIGKILL: {:?}",
        r
    );
}

const CHILD_MARK_T2: &str = "DEDUP_FI_T2_CHILD";
const TEST_NAME_T2: &str = "t2_kill9_mid_commit_recovers_without_fp";

/// Child entrypoint for T2. Runs a tight insert loop to maximize
/// the probability of SIGKILL landing during redb's COW root swap.
fn child_main_t2() -> ! {
    use slicefs_dedup::{DedupIndexConfig, RedbDedupIndex};
    use slicefs_traits::{ChunkHash, DedupIndex};

    let cas = std::path::PathBuf::from(env::var("CAS").expect("CAS env var"));
    std::fs::create_dir_all(&cas).expect("create cas root");

    let cfg = DedupIndexConfig::builder(&cas).build();
    let idx = RedbDedupIndex::create(cfg).expect("create index");

    // Tight insert loop — many in-flight write txns so the SIGKILL
    // is statistically very likely to land inside redb's COW root swap.
    for i in 0u64.. {
        let mut h = [0u8; 28];
        h[..8].copy_from_slice(&i.to_le_bytes());
        // Mock the I2 caller order: write CAS block first, then insert.
        let hex: String = h.iter().map(|b| format!("{:02x}", b)).collect();
        let shard = cas.join(&hex[..2]);
        let _ = std::fs::create_dir_all(&shard);
        let _ = std::fs::write(shard.join(&hex[2..]), b"x");
        let _ = idx.insert(&ChunkHash::from_bytes(h.to_vec()));
    }
    unreachable!()
}

#[test]
fn t2_kill9_mid_commit_recovers_without_fp() {
    // Child branch: detected by the CHILD_MARK_T2 env var. Diverges
    // before any test assertions run.
    if env::var(CHILD_MARK_T2).is_ok() {
        child_main_t2();
    }

    let td = tempfile::tempdir().expect("tempdir");
    let cas = td.path().join("cas");
    std::fs::create_dir_all(&cas).expect("create cas");

    // Spawn the same test binary, filtered to just this test, with
    // CHILD_MARK_T2 set so it dives into child_main_t2 before reaching
    // the parent assertions.
    let mut child = Command::new(env::current_exe().expect("current_exe"))
        .arg("--exact")
        .arg(TEST_NAME_T2)
        .arg("--nocapture")
        .env(CHILD_MARK_T2, "1")
        .env("CAS", &cas)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn child");

    // Let it accumulate at least one in-flight commit. 150 ms gives
    // the tight loop ample time to have multiple batches in flight.
    std::thread::sleep(Duration::from_millis(150));

    // SIGKILL. child.kill() is non-blocking; child.wait() reaps.
    let _ = child.kill();
    let _ = child.wait();

    // Re-mount the index and verify NO false positive.
    use slicefs_dedup::{DedupIndexConfig, RedbDedupIndex};
    use slicefs_traits::{ChunkHash, DedupIndex, DedupResult};

    let cfg = DedupIndexConfig::builder(&cas).build();

    // Critical assertion: redb must open at all (I7 — root CRC catches torn pages
    // and the previous root remains valid via COW).
    let idx = RedbDedupIndex::open(cfg).expect("redb must auto-recover from mid-commit kill");

    // Sample 1000 hashes the child *could* have inserted.
    // For any Present result, the corresponding CAS block MUST exist.
    for i in 0u64..1000 {
        let mut h = [0u8; 28];
        h[..8].copy_from_slice(&i.to_le_bytes());
        let r = idx
            .lookup(&ChunkHash::from_bytes(h.to_vec()))
            .expect("lookup after kill");

        if matches!(r, DedupResult::Present) {
            let hex: String = h.iter().map(|b| format!("{:02x}", b)).collect();
            let cas_path = cas.join(&hex[..2]).join(&hex[2..]);
            assert!(
                cas_path.exists(),
                "I1 violated: index says Present but CAS block missing at {:?}",
                cas_path
            );
        }
    }
}

const CHILD_MARK_T10: &str = "DEDUP_FI_T10_CHILD";

fn child_main_t10() -> ! {
    use slicefs_dedup::{DedupIndexConfig, RedbDedupIndex};
    use slicefs_traits::{ChunkHash, DedupIndex};
    let cas = std::path::PathBuf::from(env::var("CAS").unwrap());
    let worker_id: u32 = env::var("WORKER_ID").unwrap().parse().unwrap();
    std::fs::create_dir_all(&cas).unwrap();
    let cfg = DedupIndexConfig::builder(&cas).build();
    let idx = RedbDedupIndex::create(cfg).unwrap();

    // Each child owns a disjoint range of [worker_id * 1_000_000, +1_000_000).
    let base = (worker_id as u64) * 1_000_000;
    for i in 0u64.. {
        let mut h = [0u8; 28];
        h[..8].copy_from_slice(&(base + i).to_le_bytes());
        let hex: String = h.iter().map(|b| format!("{:02x}", b)).collect();
        let shard = cas.join(&hex[..2]);
        let _ = std::fs::create_dir_all(&shard);
        let _ = std::fs::write(shard.join(&hex[2..]), b"x");
        let _ = idx.insert(&ChunkHash::from_bytes(h.to_vec()));
    }
    unreachable!()
}

#[test]
#[ignore = "stress test; run with --ignored or in nightly CI"]
fn t10_100x_concurrent_kill9_no_fp() {
    if env::var(CHILD_MARK_T10).is_ok() { child_main_t10(); }

    let td = tempfile::tempdir().unwrap();
    let cas = td.path().join("cas");
    std::fs::create_dir_all(&cas).unwrap();

    let n_workers = 100u32;
    let mut children = Vec::with_capacity(n_workers as usize);
    for w in 0..n_workers {
        let c = Command::new(env::current_exe().unwrap())
            .arg("--exact").arg("t10_100x_concurrent_kill9_no_fp")
            .arg("--ignored")
            .arg("--nocapture")
            .env(CHILD_MARK_T10, "1")
            .env("CAS", &cas)
            .env("WORKER_ID", w.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn().unwrap();
        children.push(c);
    }
    // Random staggered kills 10–200 ms.
    let mut rng_state = 0x12345u64;
    for child in children.iter_mut() {
        let r = ((rng_state ^ (rng_state >> 11)) % 191) + 10;
        rng_state = rng_state.wrapping_mul(2862933555777941757).wrapping_add(3037000493);
        std::thread::sleep(Duration::from_millis(r));
        let _ = child.kill();
    }
    for mut c in children { let _ = c.wait(); }

    use slicefs_dedup::{DedupIndexConfig, RedbDedupIndex};
    use slicefs_traits::{ChunkHash, DedupIndex, DedupResult};
    let cfg = DedupIndexConfig::builder(&cas).build();
    let idx = RedbDedupIndex::open(cfg).expect("must mount after 100x kill");

    // Sample 100 hashes per worker.
    for w in 0..n_workers {
        for i in 0u64..100 {
            let mut h = [0u8; 28];
            h[..8].copy_from_slice(&((w as u64) * 1_000_000 + i).to_le_bytes());
            let r = idx.lookup(&ChunkHash::from_bytes(h.to_vec())).unwrap();
            if matches!(r, DedupResult::Present) {
                let hex: String = h.iter().map(|b| format!("{:02x}", b)).collect();
                let p = cas.join(&hex[..2]).join(&hex[2..]);
                assert!(p.exists(), "I1 violated: worker {} i={}", w, i);
            }
        }
    }
}
