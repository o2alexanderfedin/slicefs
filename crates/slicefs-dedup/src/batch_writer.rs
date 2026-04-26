//! `BatchWriter` — single-writer thread that drains an MPSC of insert
//! requests, coalesces them into a group-commit window, and applies the
//! batch as one redb write transaction.
//!
//! See ARCHITECTURE §8.1 (sequence) and §13.1 (durability tiers).
//!
//! # Strict ordering (I5)
//!
//! For every batch the thread performs, in order:
//!
//! 1. `redb_txn.commit()` — durable per the configured [`Durability`]
//!    level for this mode.
//! 2. `bloom.set_all(...)` + `high_water.fetch_add(n)` — only AFTER the
//!    commit is durable. This is what makes the bloom filter and HWM
//!    safe to consult from readers / snapshotters: if a hash is in the
//!    bloom, it has already been committed to redb.
//! 3. `reply.send(...)` — fan replies back to the per-insert oneshot
//!    channels so [`BatchWriter::submit`] can return synchronously.
//!
//! # redb 4.1 durability mapping
//!
//! redb 4.1's `Durability` enum is `#[non_exhaustive]` and exposes only
//! two variants: [`Durability::None`] and [`Durability::Immediate`]
//! (the historical `Eventual` variant from earlier redb releases is
//! gone). We therefore map our [`DurabilityMode`] tiers as:
//!
//! | Mode      | redb durability      | batch size                    |
//! |-----------|----------------------|-------------------------------|
//! | Seed      | `None`               | `cfg.batch_size_seed`         |
//! | Default   | `Immediate`          | `cfg.batch_size_default`      |
//! | Paranoid  | `Immediate`          | `1`                           |
//!
//! `Default` still gets group-commit semantics: many inserts coalesce
//! into one transaction within `cfg.batcher_coalesce_window`, then that
//! single transaction commits durably. Paranoid commits one insert per
//! transaction (no coalescing) for caller-observable F_FULLFSYNC per
//! request.

use crate::atomic_bloom::AtomicBloomFilter;
use crate::config::{DedupIndexConfig, DurabilityMode};
use crate::error::DedupIndexError;
use crate::redb_dedup_index::DEDUP_TABLE;
use crate::stats::StatsCounters;
use crossbeam_channel::{Sender, bounded};
use redb::{Database, Durability};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// 28-byte content address used as the redb key.
pub(crate) type Hash28 = [u8; 28];

/// One pending insert: the hash to write plus a oneshot reply channel
/// for the synchronous [`BatchWriter::submit`] caller.
pub(crate) struct InsertReq {
    pub hash: Hash28,
    pub reply: Sender<Result<(), DedupIndexError>>,
}

/// Owns the batcher thread and the channels used to talk to it.
///
/// Drop this via [`BatchWriter::shutdown`] for an orderly stop. A bare
/// drop will not signal the thread; it will only exit when the `tx`
/// side is dropped (channel disconnect).
//
// Fields and methods are wired into `RedbDedupIndex` by G2; until then
// some are unused at the crate level, hence the broad allow.
#[allow(dead_code)]
pub(crate) struct BatchWriter {
    pub(crate) tx: Sender<InsertReq>,
    pub(crate) handle: Option<JoinHandle<()>>,
    pub(crate) shutdown: Sender<()>,
    pub(crate) high_water: Arc<AtomicU64>,
}

#[allow(dead_code)]
impl BatchWriter {
    /// Spawn the batcher thread and return its control handle.
    pub(crate) fn spawn(
        cfg: DedupIndexConfig,
        db: Arc<Database>,
        bloom: AtomicBloomFilter,
        stats: Arc<StatsCounters>,
        high_water: Arc<AtomicU64>,
    ) -> Self {
        let (tx, rx) = bounded::<InsertReq>(cfg.mpsc_capacity);
        let (shutdown_tx, shutdown_rx) = bounded::<()>(1);

        let coalesce = cfg.batcher_coalesce_window;
        let max_batch = match cfg.durability {
            DurabilityMode::Seed => cfg.batch_size_seed,
            DurabilityMode::Default => cfg.batch_size_default,
            DurabilityMode::Paranoid => 1,
        };
        // redb 4.1 Durability has only `None` and `Immediate`; map our
        // tiers accordingly. See module docs.
        let durability = match cfg.durability {
            DurabilityMode::Seed => Durability::None,
            DurabilityMode::Default => Durability::Immediate,
            DurabilityMode::Paranoid => Durability::Immediate,
        };

        let bloom_for_thread = bloom.clone_handle();
        let hw = Arc::clone(&high_water);
        let stats_for_thread = Arc::clone(&stats);

        // Capture snapshot-related extras OUTSIDE the loop so we don't
        // re-clone every iteration. `cfg.bloom` is `Copy`; only the
        // `dedup_root` `PathBuf` needs an explicit clone.
        let bloom_cfg = cfg.bloom;
        let dedup_root = cfg.dedup_root.clone();

        let handle = std::thread::Builder::new()
            .name("slicefs-dedup-batcher".into())
            .spawn(move || {
                'outer: loop {
                    if shutdown_rx.try_recv().is_ok() {
                        break;
                    }
                    let mut batch: Vec<InsertReq> = Vec::with_capacity(max_batch.min(10_000));
                    // Block for the first request, but with a short
                    // timeout so we remain responsive to shutdown.
                    match rx.recv_timeout(Duration::from_millis(50)) {
                        Ok(req) => batch.push(req),
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break 'outer,
                    }

                    // Coalesce: keep pulling until we hit the deadline
                    // or the per-mode batch cap.
                    let deadline = Instant::now() + coalesce;
                    while batch.len() < max_batch {
                        let now = Instant::now();
                        if now >= deadline {
                            break;
                        }
                        match rx.recv_timeout(deadline - now) {
                            Ok(req) => batch.push(req),
                            Err(_) => break,
                        }
                    }

                    let result = (|| -> Result<(), DedupIndexError> {
                        let mut txn = db.begin_write()?;
                        // redb 4.1: set_durability returns Result; the
                        // only way it can fail here is a persistent
                        // savepoint clash, which we never create.
                        let _ = txn.set_durability(durability);
                        {
                            let mut t = txn.open_table(DEDUP_TABLE)?;
                            for r in &batch {
                                t.insert(&r.hash, ())?;
                            }
                        }
                        txn.commit()?;
                        Ok(())
                    })();

                    if result.is_err() {
                        stats_for_thread
                            .commit_failures_total
                            .fetch_add(1, Ordering::Relaxed);
                    } else {
                        // I5: bloom + HWM strictly AFTER commit, before
                        // we reply to the submitter.
                        let refs: Vec<&[u8]> = batch.iter().map(|r| r.hash.as_slice()).collect();
                        bloom_for_thread.set_all(&refs);
                        hw.fetch_add(batch.len() as u64, Ordering::AcqRel);
                        stats_for_thread
                            .commits_total
                            .fetch_add(1, Ordering::Relaxed);
                        stats_for_thread
                            .inserts_total
                            .fetch_add(batch.len() as u64, Ordering::Relaxed);

                        // J1: snapshot trigger. Fire one bloom snapshot
                        // when the cumulative inserts counter crosses an
                        // N-multiple boundary (N = bloom.snapshot_every).
                        //
                        // Performance note: `to_bytes()` clones the
                        // entire bit-array (~1.2 GB at N=10^9). For MVP
                        // this is acceptable since snapshots happen every
                        // 100K inserts (Default mode). For higher
                        // throughput we'd hand a clone of the
                        // `Arc<RwLock<BloomFilter>>` to a separate
                        // snapshotter thread instead of doing it in the
                        // batcher hot path.
                        let total_after = stats_for_thread.inserts_total.load(Ordering::Relaxed);
                        let total_before = total_after - batch.len() as u64;
                        let n = bloom_cfg.snapshot_every as u64;
                        if n > 0 && n != u64::MAX {
                            let prev_window = total_before / n;
                            let now_window = total_after / n;
                            if now_window > prev_window {
                                let payload = bloom_for_thread.to_bytes();
                                let meta = crate::bloom_snapshot::BloomSnapshotMeta {
                                    bloom_capacity: bloom_cfg.capacity as u64,
                                    bloom_fpr_bits: bloom_cfg.fpr,
                                    entries_at_snapshot: total_after,
                                    redb_hwm_at_snapshot: hw.load(Ordering::Acquire),
                                };
                                let root = crate::paths::DedupRoot::new(&dedup_root);
                                if let Err(e) =
                                    crate::bloom_snapshot::write_atomic(&root, &meta, &payload)
                                {
                                    tracing::warn!("bloom snapshot failed: {e}");
                                    stats_for_thread
                                        .bloom_snapshot_failures_total
                                        .fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                    }

                    // Build a cloneable reply outcome. We turn the
                    // (non-Clone) DedupIndexError into a Recovery-string
                    // for fan-out; the original error is logged in the
                    // commit-failures counter above.
                    let reply_outcome: Result<(), DedupIndexError> = match &result {
                        Ok(()) => Ok(()),
                        Err(e) => Err(DedupIndexError::Recovery(format!(
                            "batch commit failed: {e}"
                        ))),
                    };
                    for r in batch {
                        // We have to re-clone per recipient because
                        // DedupIndexError is not Clone.
                        let outcome: Result<(), DedupIndexError> = match &reply_outcome {
                            Ok(()) => Ok(()),
                            Err(e) => Err(DedupIndexError::Recovery(e.to_string())),
                        };
                        let _ = r.reply.send(outcome);
                    }
                }
            })
            .expect("spawn batcher thread");

        Self {
            tx,
            handle: Some(handle),
            shutdown: shutdown_tx,
            high_water,
        }
    }

    /// Submit one hash and block on the batcher's reply.
    ///
    /// Synchronous: returns only after the batcher has committed (or
    /// failed) the batch this hash was part of, and after the bloom
    /// filter and HWM have been updated. Callers can therefore observe
    /// the post-commit invariants immediately upon return.
    pub(crate) fn submit(&self, hash: Hash28) -> Result<(), DedupIndexError> {
        let (rtx, rrx) = bounded::<Result<(), DedupIndexError>>(1);
        self.tx
            .send(InsertReq { hash, reply: rtx })
            .map_err(|_| DedupIndexError::Recovery("batcher channel closed".into()))?;
        rrx.recv()
            .map_err(|_| DedupIndexError::Recovery("batcher reply lost".into()))?
    }

    /// Signal the batcher to stop and join the thread within `timeout`.
    ///
    /// Best-effort: if the thread has not finished within `timeout`,
    /// we still call `join()` (which blocks until it does); the timeout
    /// is the polling budget for [`JoinHandle::is_finished`].
    pub(crate) fn shutdown(mut self, timeout: Duration) {
        let _ = self.shutdown.send(());
        if let Some(h) = self.handle.take() {
            let start = Instant::now();
            while !h.is_finished() && start.elapsed() < timeout {
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DedupIndexConfig;
    use crate::redb_dedup_index::RedbDedupIndex;

    #[test]
    fn submit_one_then_commit_increments_hwm() {
        let td = tempfile::tempdir().unwrap();
        let cas = td.path().join("cas");
        std::fs::create_dir_all(&cas).unwrap();
        let cfg = DedupIndexConfig::builder(&cas)
            .mode(DurabilityMode::Default)
            .build();
        let idx = RedbDedupIndex::create(cfg.clone()).unwrap();

        let bloom = AtomicBloomFilter::new(&cfg.bloom);
        let stats = Arc::new(StatsCounters::default());
        let hw = Arc::new(AtomicU64::new(0));
        let bw = BatchWriter::spawn(cfg, Arc::clone(&idx.db), bloom, stats, Arc::clone(&hw));

        let mut h = [0u8; 28];
        h[0] = 0xAA;
        bw.submit(h).unwrap();
        // submit() blocks on the oneshot reply, which the batcher only
        // sends AFTER bumping HWM (I5). So this must hold immediately.
        assert_eq!(hw.load(Ordering::Acquire), 1);

        bw.shutdown(Duration::from_secs(2));
    }
}
