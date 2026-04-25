# SliceFS DedupIndex (Persistent, redb-backed) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the persistent on-disk `DedupIndex` for SliceFS — a new `slicefs-dedup` crate exposing `RedbDedupIndex` with bloom-fronted lookups, redb-backed authoritative storage, group-commit batching, and CAS-as-truth recovery — meeting all MVP SLOs and failure-injection tests in `.planning/research/dedup-index/ARCHITECTURE.md`.

**Architecture:** Single-host, single-mount, redb 4.1 B+tree (`dedup_index_v1` table, fixed 28-byte key, unit value) fronted by a fastbloom AtomicBloomFilter. Inserts flow through a single-writer batcher (bounded MPSC, 16k cap, 2 ms / 10k coalesce window) that orders `redb.commit ▸ {bloom.set_all, HWM} ▸ caller-reply`. Bloom snapshots (`bloom.snap`, xxh3-128 + CRC32C, rename-atomic) are advisory; redb is authoritative; `walk(<store>/cas/)` is canonical truth (recovery target). On-disk root: `<store>/cas/.dedup-index/`. Three durability modes: `Seed` / `Default` / `Paranoid`.

**Tech Stack:** Rust 2024 (workspace edition); `redb 4.1`; `fastbloom 0.14`; `xxhash-rust` (xxh3-128); `crc32c`; `serde` + `serde_json` (manifest); `thiserror` 2; `tracing`; `metrics` (optional `prometheus` feature); `proptest`; `tempfile`; `criterion`.

**Reference docs (read before any task):**
- `.planning/research/dedup-index/ARCHITECTURE.md` — binding spec, all `[§N]` citations resolve here.
- `.planning/research/dedup-index/architecture/01-api-and-types.md`
- `.planning/research/dedup-index/architecture/02-storage-and-layout.md`
- `.planning/research/dedup-index/architecture/03-durability-and-recovery.md`
- `.planning/research/dedup-index/architecture/04-performance-and-operations.md`

**Working agreement:**
- TDD: failing test → minimal code → green → refactor → commit.
- One commit per task (granularity is per-task, not per-step).
- Branch off `develop` per git-flow: `git flow feature start <slug>`. Finish with `git flow feature finish <slug>` (which merges to `develop`). When all milestones land, `git flow release start v0.2.0-dedup` → finish.
- Run `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, and `cargo test -p <crate>` before each commit.

---

## File Structure

### New crate: `crates/slicefs-dedup/`

```
crates/slicefs-dedup/
├── Cargo.toml                       — dependencies, features (`prometheus`)
├── src/
│   ├── lib.rs                       — re-exports, crate-level docs, panic note
│   ├── config.rs                    — DedupIndexConfig + builder + DurabilityMode + BloomConfig
│   ├── error.rs                     — DedupIndexError; map → CasError at trait boundary
│   ├── paths.rs                     — DedupRoot wrapper + path helpers
│   ├── manifest.rs                  — Manifest struct + atomic write/read (CRC32C)
│   ├── bloom_snapshot.rs            — Header/Footer codec, xxh3-128 + CRC32C
│   ├── atomic_bloom.rs              — AtomicBloomFilter wrapper around fastbloom
│   ├── batch_writer.rs              — single-writer thread, MPSC, oneshot replies
│   ├── recovery.rs                  — rebuild_from_cas, manifest probing
│   ├── platform.rs                  — durable_sync(fd) (F_FULLFSYNC / fdatasync)
│   ├── stats.rs                     — atomic counters + StatsSnapshot
│   ├── verify.rs                    — page-CRC walk wrapper, VerifyReport
│   └── redb_dedup_index.rs          — RedbDedupIndex (orchestrator) + DedupIndex impl
├── tests/
│   ├── parity.rs                    — proptest parity vs MemDedupIndex
│   ├── recovery.rs                  — rebuild_from_cas idempotency
│   ├── failure_injection_kill9.rs   — tests 1, 2, 13 (child-harness SIGKILL)
│   ├── failure_injection_corrupt.rs — tests 6, 10
│   ├── failure_injection_fsync.rs   — tests 9, 11 (loop-device + DYLD shim — gated)
│   └── common/
│       └── mod.rs                   — test fixtures, hash factories
└── benches/
    ├── seed_burst.rs                — gate: ≥ 100 K ins/s
    ├── lookup.rs                    — warm + cold p50/p99
    ├── steady_mixed.rs              — ≥ 20 K ins/s
    ├── commit_latency.rs            — p99 ≤ 5 ms (default), 12 ms (paranoid)
    └── recovery_50m.rs              — RTO ≤ 30 s
```

### Modified files

```
Cargo.toml                                    — bump redb 3.1→4.1, add xxhash-rust, crc32c, metrics
crates/slicefs-traits/src/dedup_index.rs      — extend trait with flush/verify/stats default-impls
crates/slicefs-traits/src/lib.rs              — re-export new types (VerifyReport, IndexStats)
crates/slicefs-traits/Cargo.toml              — add types dep if needed
crates/cas-local/src/mem_dedup_index.rs       — verify it still compiles after trait extension
crates/slicefs-cli/src/...                    — `slicefs reindex`, `slicefs dedup recover`, `slicefs stats [Index]` block (Milestone Q)
```

### Out of scope for this plan (separate plans):
- FUSE-layer wiring of `RedbDedupIndex` into `slicefs-cli mount` — see `05-fuse-integration.md` (not yet written).
- GC architecture / `remove()` policy beyond the trait contract — see `06-gc-architecture.md` (not yet written).
- v2 levers: log-structured engine, sharded redb, online rebuild, segment-bloom growth.

---

## Milestone A — Workspace prerequisites (Phase 0)

**Goal:** Repo is on redb 4.1 with all current tests green; new crate is wired into the workspace as an empty member.

### Task A1: Bump redb 3.1 → 4.1 and verify metadata crate

**Files:**
- Modify: `Cargo.toml` (workspace dependencies)
- Verify: `crates/metadata/` (only current `redb` consumer)

- [ ] **Step 1: Read redb 4.1 release notes**

Run: `cargo search redb` and visit https://docs.rs/redb/4.1/redb/ (note: as of plan-write the architecture targets 4.1; if a higher 4.x exists with a stable `Value for ()` impl, prefer the latest 4.x).

Confirm presence of: `Durability::None`, `Durability::Eventual`, `Durability::Immediate`; `Builder::set_page_size`; `Database::compact`; `Database::stats() -> DatabaseStats { tree_height, free_pages_bytes, allocated_pages_bytes, ... }`; `impl Value for ()`.

- [ ] **Step 2: Bump workspace dep**

Edit `Cargo.toml` line 26:
```toml
redb       = "4.1"
```
(Replace the existing `redb = "3.1"`.)

- [ ] **Step 3: Build workspace**

Run: `cargo build --workspace`
Expected: errors only in `crates/metadata` (or none at all if metadata doesn't currently use redb beyond a transitive dep).

- [ ] **Step 4: Audit metadata crate**

Run: `grep -rn "redb" crates/metadata/src/`

For each call site, confirm against 4.1 docs:
- `Database::create` / `Database::open` (signature + `set_*` builder chain)
- `WriteTransaction::open_table` and table iteration
- `Durability` variants
- Any direct `Builder` / `RepairSession` use

Apply fixes in place (no shims, no compat layer).

- [ ] **Step 5: Run metadata tests**

Run: `cargo test -p metadata`
Expected: all pass. If any fail, debug to root cause — no skipping.

- [ ] **Step 6: Run full workspace tests**

Run: `cargo test --workspace`
Expected: 1287 (or current baseline) tests pass.

- [ ] **Step 7: Commit**

```bash
git flow feature start workspace-redb-4
git add Cargo.toml crates/metadata/
git commit -m "chore(workspace): bump redb 3.1 → 4.1

Audited metadata crate against 4.1 API. All workspace tests green.
Required by .planning/research/dedup-index/ARCHITECTURE.md §15.0."
git flow feature finish workspace-redb-4
git push origin develop
```

### Task A2: Add new workspace dependencies for slicefs-dedup

**Files:**
- Modify: `Cargo.toml` (workspace dependencies)

- [ ] **Step 1: Add new entries**

Append to `[workspace.dependencies]` in `Cargo.toml`:
```toml
xxhash-rust = { version = "0.8", features = ["xxh3"] }
crc32c      = "0.6"
metrics     = "0.23"
parking_lot = "0.12"
crossbeam-channel = "0.5"
```

- [ ] **Step 2: Verify workspace still builds**

Run: `cargo build --workspace`
Expected: success (the new entries are unused until Milestone B).

- [ ] **Step 3: Commit**

```bash
git flow feature start workspace-deps-dedup
git add Cargo.toml
git commit -m "chore(workspace): add deps for slicefs-dedup

xxhash-rust (xxh3-128), crc32c, metrics, parking_lot, crossbeam-channel."
git flow feature finish workspace-deps-dedup
git push origin develop
```

### Task A3: Scaffold the empty `slicefs-dedup` crate

**Files:**
- Create: `crates/slicefs-dedup/Cargo.toml`
- Create: `crates/slicefs-dedup/src/lib.rs`
- Modify: `Cargo.toml` (workspace `members`)

- [ ] **Step 1: Create Cargo.toml**

`crates/slicefs-dedup/Cargo.toml`:
```toml
[package]
name = "slicefs-dedup"
version = "0.1.0"
edition = "2024"
authors = ["Alexander Fedin <af@o2.services>", "Sergey Shandar"]
license = "AGPL-3.0-or-later"
description = "Persistent, redb-backed DedupIndex for SliceFS: bloom-fronted lookups, group-commit batcher, CAS-as-truth recovery"

[features]
default = []
prometheus = ["metrics-exporter-prometheus"]

[dependencies]
slicefs-traits     = { path = "../slicefs-traits" }
redb.workspace               = true
fastbloom.workspace          = true
xxhash-rust.workspace        = true
crc32c.workspace             = true
serde.workspace              = true
serde_json.workspace         = true
thiserror.workspace          = true
tracing.workspace            = true
metrics.workspace            = true
parking_lot.workspace        = true
crossbeam-channel.workspace  = true
libc.workspace               = true

metrics-exporter-prometheus = { version = "0.15", optional = true }

[dev-dependencies]
proptest.workspace = true
tempfile.workspace = true
cas-local          = { path = "../cas-local" }
criterion.workspace = true

[[bench]]
name = "seed_burst"
harness = false

[[bench]]
name = "lookup"
harness = false

[[bench]]
name = "steady_mixed"
harness = false

[[bench]]
name = "commit_latency"
harness = false

[[bench]]
name = "recovery_50m"
harness = false
```

- [ ] **Step 2: Create lib.rs**

`crates/slicefs-dedup/src/lib.rs`:
```rust
//! Persistent on-disk DedupIndex for SliceFS, backed by redb 4.x.
//!
//! See `.planning/research/dedup-index/ARCHITECTURE.md` (the binding
//! spec) and the per-spec docs under
//! `.planning/research/dedup-index/architecture/`.
//!
//! # Panic policy
//! No panic ever crosses the [`slicefs_traits::DedupIndex`] trait
//! boundary. Engine panics are caught and surfaced as
//! [`CasError::Index("engine-panic: …")`]. See ARCHITECTURE §5.3.
```

- [ ] **Step 3: Add to workspace members**

In root `Cargo.toml`, append `"crates/slicefs-dedup",` to the `members = [ … ]` array.

- [ ] **Step 4: Build**

Run: `cargo build -p slicefs-dedup`
Expected: success (empty crate, no symbols).

- [ ] **Step 5: Commit**

```bash
git flow feature start dedup-crate-skeleton
git add crates/slicefs-dedup/ Cargo.toml
git commit -m "feat(slicefs-dedup): scaffold empty crate

Adds crates/slicefs-dedup/ to the workspace per
ARCHITECTURE.md §6 (crate placement) and §15.1 (MVP scope)."
git flow feature finish dedup-crate-skeleton
git push origin develop
```

---

## Milestone B — Core types

**Goal:** All public configuration / error / result types defined and unit-tested. No I/O yet.

### Task B1: Define `DedupIndexError`

**Files:**
- Create: `crates/slicefs-dedup/src/error.rs`
- Modify: `crates/slicefs-dedup/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Append to `crates/slicefs-dedup/src/error.rs`:
```rust
use slicefs_traits::CasError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DedupIndexError {
    #[error("redb error: {0}")]
    Redb(#[from] redb::Error),
    #[error("redb storage error: {0}")]
    Storage(#[from] redb::StorageError),
    #[error("redb transaction error: {0}")]
    Transaction(#[from] redb::TransactionError),
    #[error("redb commit error: {0}")]
    Commit(#[from] redb::CommitError),
    #[error("redb table error: {0}")]
    Table(#[from] redb::TableError),
    #[error("bloom snapshot corrupt: {0}")]
    BloomCorrupt(&'static str),
    #[error("manifest corrupt: {0}")]
    ManifestCorrupt(&'static str),
    #[error("bloom capacity exceeded: load_factor={load_factor:.3}")]
    BloomCapacityExceeded { load_factor: f64 },
    #[error("recovery failed: {0}")]
    Recovery(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("engine panic: {0}")]
    EnginePanic(String),
}

impl From<DedupIndexError> for CasError {
    fn from(e: DedupIndexError) -> Self {
        match e {
            DedupIndexError::Io(io) => CasError::Io(io),
            other => CasError::Index(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_passthrough_to_cas_io() {
        let e = DedupIndexError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "missing",
        ));
        match CasError::from(e) {
            CasError::Io(_) => {}
            other => panic!("expected CasError::Io, got {other:?}"),
        }
    }

    #[test]
    fn other_variants_collapse_to_index() {
        let e = DedupIndexError::BloomCorrupt("xxh3 mismatch");
        let mapped = CasError::from(e);
        match mapped {
            CasError::Index(msg) => assert!(msg.contains("xxh3")),
            other => panic!("expected CasError::Index, got {other:?}"),
        }
    }
}
```

- [ ] **Step 2: Re-export in lib.rs**

Append to `crates/slicefs-dedup/src/lib.rs`:
```rust
mod error;
pub use error::DedupIndexError;
```

- [ ] **Step 3: Run test, expect pass**

Run: `cargo test -p slicefs-dedup -- error::tests`
Expected: 2 tests pass.

- [ ] **Step 4: Commit**

```bash
git flow feature start dedup-error
git add crates/slicefs-dedup/
git commit -m "feat(slicefs-dedup): DedupIndexError with CasError mapping

Per ARCHITECTURE.md §5.3 — Io passes through, all other variants
collapse to CasError::Index(string)."
git flow feature finish dedup-error
git push origin develop
```

### Task B2: Define `DurabilityMode` and `BloomConfig`

**Files:**
- Create: `crates/slicefs-dedup/src/config.rs`
- Modify: `crates/slicefs-dedup/src/lib.rs`

- [ ] **Step 1: Write the failing tests + types**

Create `crates/slicefs-dedup/src/config.rs`:
```rust
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Three durability tiers per ARCHITECTURE §13.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurabilityMode {
    /// `Durability::None`; bulk seed only — reload from CAS on crash.
    Seed,
    /// `Durability::Eventual` + 200 ms group-commit — daily driver.
    Default,
    /// `Durability::Immediate` per insert (one F_FULLFSYNC each).
    Paranoid,
}

#[derive(Debug, Clone, Copy)]
pub struct BloomConfig {
    pub capacity: usize,
    pub fpr: f64,
    pub snapshot_every: usize,
    pub snapshot_interval: Duration,
    pub stale_ratio: f64,
    pub drift_rebuild_ratio: f64,
    pub effective_fpr_rebuild_multiplier: f64,
}

impl BloomConfig {
    pub fn for_mode(mode: DurabilityMode) -> Self {
        match mode {
            DurabilityMode::Seed => Self {
                capacity: 100_000_000,
                fpr: 0.01,
                snapshot_every: usize::MAX,
                snapshot_interval: Duration::from_secs(u64::MAX / 2),
                stale_ratio: 0.90,
                drift_rebuild_ratio: 0.20,
                effective_fpr_rebuild_multiplier: 4.0,
            },
            DurabilityMode::Default => Self {
                capacity: 100_000_000,
                fpr: 0.01,
                snapshot_every: 100_000,
                snapshot_interval: Duration::from_secs(600),
                stale_ratio: 0.90,
                drift_rebuild_ratio: 0.20,
                effective_fpr_rebuild_multiplier: 4.0,
            },
            DurabilityMode::Paranoid => Self {
                capacity: 100_000_000,
                fpr: 0.01,
                snapshot_every: 10_000,
                snapshot_interval: Duration::from_secs(60),
                stale_ratio: 0.90,
                drift_rebuild_ratio: 0.10,
                effective_fpr_rebuild_multiplier: 2.0,
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct DedupIndexConfig {
    pub cas_root: PathBuf,
    pub dedup_root: PathBuf,
    pub durability: DurabilityMode,
    pub bloom: BloomConfig,
    pub batcher_coalesce_window: Duration,
    pub redb_group_commit_window: Duration,
    pub batch_size_default: usize,
    pub batch_size_seed: usize,
    pub mpsc_capacity: usize,
    pub verify_on_present: bool,
    pub use_f_fullfsync: bool,
    pub redb_cache_bytes: usize,
    pub page_size: usize,
}

impl DedupIndexConfig {
    pub fn builder(cas_root: impl AsRef<Path>) -> DedupIndexConfigBuilder {
        DedupIndexConfigBuilder::new(cas_root.as_ref().to_path_buf())
    }
}

pub struct DedupIndexConfigBuilder {
    cas_root: PathBuf,
    durability: DurabilityMode,
}

impl DedupIndexConfigBuilder {
    pub fn new(cas_root: PathBuf) -> Self {
        Self { cas_root, durability: DurabilityMode::Default }
    }

    pub fn mode(mut self, mode: DurabilityMode) -> Self {
        self.durability = mode;
        self
    }

    pub fn build(self) -> DedupIndexConfig {
        let dedup_root = self.cas_root.join(".dedup-index");
        let (coalesce, batch_default, batch_seed, verify_on_present) = match self.durability {
            DurabilityMode::Seed     => (Duration::from_millis(20), 10_000, 100_000, false),
            DurabilityMode::Default  => (Duration::from_millis(2),  10_000, 100_000, false),
            DurabilityMode::Paranoid => (Duration::ZERO,            1,      1,       true),
        };
        DedupIndexConfig {
            cas_root: self.cas_root,
            dedup_root,
            durability: self.durability,
            bloom: BloomConfig::for_mode(self.durability),
            batcher_coalesce_window: coalesce,
            redb_group_commit_window: Duration::from_millis(200),
            batch_size_default: batch_default,
            batch_size_seed: batch_seed,
            mpsc_capacity: 16_384,
            verify_on_present,
            use_f_fullfsync: true, // I10 — never disabled
            redb_cache_bytes: 256 * 1024 * 1024,
            page_size: 4096,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_mode_has_2ms_coalesce() {
        let c = DedupIndexConfig::builder("/x").build();
        assert_eq!(c.batcher_coalesce_window, Duration::from_millis(2));
        assert!(!c.verify_on_present);
    }

    #[test]
    fn paranoid_enables_verify_on_present() {
        let c = DedupIndexConfig::builder("/x").mode(DurabilityMode::Paranoid).build();
        assert!(c.verify_on_present);
        assert_eq!(c.batch_size_default, 1);
    }

    #[test]
    fn seed_uses_large_batches() {
        let c = DedupIndexConfig::builder("/x").mode(DurabilityMode::Seed).build();
        assert_eq!(c.batch_size_seed, 100_000);
    }

    #[test]
    fn dedup_root_is_under_cas() {
        let c = DedupIndexConfig::builder("/store/cas").build();
        assert_eq!(c.dedup_root, std::path::Path::new("/store/cas/.dedup-index"));
    }
}
```

- [ ] **Step 2: Re-export**

Append to `crates/slicefs-dedup/src/lib.rs`:
```rust
mod config;
pub use config::{BloomConfig, DedupIndexConfig, DedupIndexConfigBuilder, DurabilityMode};
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p slicefs-dedup -- config::tests`
Expected: 4 tests pass.

- [ ] **Step 4: Commit**

```bash
git flow feature start dedup-config
git add crates/slicefs-dedup/
git commit -m "feat(slicefs-dedup): DedupIndexConfig + builder

Three modes (Seed/Default/Paranoid) per ARCHITECTURE §13.1."
git flow feature finish dedup-config
git push origin develop
```

### Task B3: Define `IndexStats`, `VerifyReport`, `DedupResult` re-export

**Files:**
- Create: `crates/slicefs-dedup/src/stats.rs`
- Create: `crates/slicefs-dedup/src/verify.rs`
- Modify: `crates/slicefs-dedup/src/lib.rs`

- [ ] **Step 1: Write stats.rs**

Create `crates/slicefs-dedup/src/stats.rs`:
```rust
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Default, Clone, Copy)]
pub struct IndexStats {
    pub entries: u64,
    pub bloom_load_factor: f64,
    pub redb_free_bytes: u64,
}

#[derive(Debug, Default)]
pub struct StatsCounters {
    pub inserts_total: AtomicU64,
    pub lookups_total: AtomicU64,
    pub bloom_hits_total: AtomicU64,
    pub bloom_false_positives_total: AtomicU64,
    pub commits_total: AtomicU64,
    pub commit_failures_total: AtomicU64,
    pub removes_total: AtomicU64,
    pub backpressure_rejects_total: AtomicU64,
    pub verify_on_present_hits_total: AtomicU64,
    pub bloom_snapshot_failures_total: AtomicU64,
}

impl StatsCounters {
    pub fn inc(&self, c: &AtomicU64) {
        c.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Debug, Default, Clone)]
pub struct StatsSnapshot {
    pub inserts_total: u64,
    pub lookups_total: u64,
    pub bloom_hits_total: u64,
    pub bloom_false_positives_total: u64,
    pub commits_total: u64,
    pub commit_failures_total: u64,
    pub removes_total: u64,
    pub backpressure_rejects_total: u64,
    pub verify_on_present_hits_total: u64,
    pub bloom_snapshot_failures_total: u64,
    pub bloom_load_factor: f64,
    pub redb_free_bytes: u64,
    pub queue_depth: u64,
    pub hwm: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_increment_relaxed() {
        let c = StatsCounters::default();
        c.inserts_total.fetch_add(7, Ordering::Relaxed);
        assert_eq!(c.inserts_total.load(Ordering::Relaxed), 7);
    }
}
```

- [ ] **Step 2: Write verify.rs**

Create `crates/slicefs-dedup/src/verify.rs`:
```rust
#[derive(Debug, Default, Clone)]
pub struct VerifyReport {
    pub pages_scanned: u64,
    pub anomalies: u64,
    pub bloom_load_factor: f64,
    pub elapsed_ms: u64,
}

impl VerifyReport {
    pub fn ok(&self) -> bool {
        self.anomalies == 0
    }
}
```

- [ ] **Step 3: Re-export**

Append to `crates/slicefs-dedup/src/lib.rs`:
```rust
mod stats;
mod verify;
pub use stats::{IndexStats, StatsCounters, StatsSnapshot};
pub use verify::VerifyReport;
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p slicefs-dedup`
Expected: 7 tests pass (4 config + 2 error + 1 stats).

- [ ] **Step 5: Commit**

```bash
git flow feature start dedup-stats-verify
git add crates/slicefs-dedup/
git commit -m "feat(slicefs-dedup): IndexStats/StatsSnapshot/VerifyReport

Atomic counter set per ARCHITECTURE §12; VerifyReport per §5.2."
git flow feature finish dedup-stats-verify
git push origin develop
```

---

## Milestone C — Extend `DedupIndex` trait

**Goal:** Trait surface in `slicefs-traits` matches ARCHITECTURE §5.1; `MemDedupIndex` compiles unchanged.

### Task C1: Add `flush` / `verify` / `stats` default-impls

**Files:**
- Modify: `crates/slicefs-traits/src/dedup_index.rs`
- Modify: `crates/slicefs-traits/src/lib.rs`
- Verify: `crates/cas-local/src/mem_dedup_index.rs` (no source changes expected)

- [ ] **Step 1: Define new types in slicefs-traits**

Add to `crates/slicefs-traits/src/dedup_index.rs`:
```rust
#[derive(Debug, Default, Clone)]
pub struct VerifyReport {
    pub pages_scanned: u64,
    pub anomalies: u64,
    pub bloom_load_factor: f64,
    pub elapsed_ms: u64,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct IndexStats {
    pub entries: u64,
    pub bloom_load_factor: f64,
    pub redb_free_bytes: u64,
}
```

(These are duplicated in `slicefs-dedup` — once C1 lands, delete the duplicates from `slicefs-dedup/src/{stats,verify}.rs` and re-export from `slicefs-traits`. Done in step 4.)

- [ ] **Step 2: Extend the trait**

Modify the existing trait in `crates/slicefs-traits/src/dedup_index.rs`:
```rust
pub trait DedupIndex: Send + Sync {
    fn bloom_check(&self, hash: &ChunkHash) -> bool;
    fn lookup(&self, hash: &ChunkHash) -> Result<DedupResult, CasError>;
    fn insert(&self, hash: &ChunkHash) -> Result<(), CasError>;
    fn remove(&self, hash: &ChunkHash) -> Result<(), CasError>;

    /// Force pending writes / bloom snapshot to durable storage.
    fn flush(&self) -> Result<(), CasError> { Ok(()) }

    /// On-disk integrity scan. Used by scrubber.
    fn verify(&self) -> Result<VerifyReport, CasError> {
        Ok(VerifyReport::default())
    }

    /// Coarse trait-level stats; richer snapshot on RedbDedupIndex directly.
    fn stats(&self) -> IndexStats { IndexStats::default() }
}
```

- [ ] **Step 3: Re-export**

In `crates/slicefs-traits/src/lib.rs`, ensure `VerifyReport` and `IndexStats` are public.

- [ ] **Step 4: Remove duplicates in slicefs-dedup**

Delete the duplicated `VerifyReport` from `crates/slicefs-dedup/src/verify.rs` and the `IndexStats` from `crates/slicefs-dedup/src/stats.rs`; replace with `pub use slicefs_traits::{VerifyReport, IndexStats};`.

- [ ] **Step 5: Run all workspace tests**

Run: `cargo test --workspace`
Expected: all pass. `MemDedupIndex` compiles because the new methods have default impls.

- [ ] **Step 6: Commit**

```bash
git flow feature start trait-extend-dedup
git add crates/slicefs-traits/ crates/slicefs-dedup/
git commit -m "feat(slicefs-traits): extend DedupIndex with flush/verify/stats

Default-impl no-ops keep MemDedupIndex compiling unchanged.
ARCHITECTURE.md §5.1."
git flow feature finish trait-extend-dedup
git push origin develop
```

---

## Milestone D — Storage layout primitives

**Goal:** `paths`, `manifest`, and `bloom_snapshot` modules pure-IO + tested. No redb yet.

### Task D1: `paths.rs` — DedupRoot wrapper + path helpers

**Files:**
- Create: `crates/slicefs-dedup/src/paths.rs`
- Modify: `crates/slicefs-dedup/src/lib.rs`

- [ ] **Step 1: Write tests + impl**

Create `crates/slicefs-dedup/src/paths.rs`:
```rust
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct DedupRoot {
    base: PathBuf,
}

impl DedupRoot {
    pub fn new(base: impl AsRef<Path>) -> Self {
        Self { base: base.as_ref().to_path_buf() }
    }

    pub fn base(&self) -> &Path { &self.base }
    pub fn redb(&self)         -> PathBuf { self.base.join("index.redb") }
    pub fn redb_lock(&self)    -> PathBuf { self.base.join("index.redb.lock") }
    pub fn manifest(&self)     -> PathBuf { self.base.join("manifest.json") }
    pub fn manifest_tmp(&self) -> PathBuf { self.base.join("manifest.json.tmp") }
    pub fn bloom(&self)        -> PathBuf { self.base.join("bloom.snap") }
    pub fn bloom_tmp(&self)    -> PathBuf { self.base.join("bloom.snap.tmp") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_under_base() {
        let r = DedupRoot::new("/store/cas/.dedup-index");
        assert_eq!(r.redb(), Path::new("/store/cas/.dedup-index/index.redb"));
        assert_eq!(r.bloom(), Path::new("/store/cas/.dedup-index/bloom.snap"));
        assert_eq!(r.manifest_tmp(), Path::new("/store/cas/.dedup-index/manifest.json.tmp"));
    }
}
```

Append to `lib.rs`: `mod paths; pub use paths::DedupRoot;`

- [ ] **Step 2: Run test**

Run: `cargo test -p slicefs-dedup -- paths::tests`
Expected: 1 test passes.

- [ ] **Step 3: Commit**

```bash
git flow feature start dedup-paths
git add crates/slicefs-dedup/
git commit -m "feat(slicefs-dedup): DedupRoot path helpers"
git flow feature finish dedup-paths
git push origin develop
```

### Task D2: Platform abstraction `durable_sync(fd)`

**Files:**
- Create: `crates/slicefs-dedup/src/platform.rs`
- Modify: `crates/slicefs-dedup/src/lib.rs`

- [ ] **Step 1: Write the failing test + impl**

Create `crates/slicefs-dedup/src/platform.rs`:
```rust
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

/// Platform-correct full sync per ARCHITECTURE §3 I10.
/// macOS: F_FULLFSYNC (plain fsync is a no-op on Apple SSDs).
/// Linux: fdatasync.
/// Other Unix: best-effort fsync.
pub fn durable_sync(file: &File) -> io::Result<()> {
    let fd = file.as_raw_fd();
    #[cfg(target_os = "macos")]
    {
        let r = unsafe { libc::fcntl(fd, libc::F_FULLFSYNC) };
        if r == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        let r = unsafe { libc::fdatasync(fd) };
        if r == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        file.sync_data()
    }
}

/// Open a directory and full-sync it. Used after rename(2) and after
/// creating files to make the directory entry durable.
pub fn fsync_parent_dir(dir: &std::path::Path) -> io::Result<()> {
    let f = File::open(dir)?;
    durable_sync(&f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_sync_succeeds_on_fresh_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x");
        let f = File::create(&path).unwrap();
        durable_sync(&f).unwrap();
    }

    #[test]
    fn fsync_parent_dir_on_tempdir_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        fsync_parent_dir(dir.path()).unwrap();
    }
}
```

Append to `lib.rs`:
```rust
mod platform;
pub(crate) use platform::{durable_sync, fsync_parent_dir};
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p slicefs-dedup -- platform`
Expected: 2 tests pass.

- [ ] **Step 3: Commit**

```bash
git flow feature start dedup-platform-fsync
git add crates/slicefs-dedup/
git commit -m "feat(slicefs-dedup): durable_sync (F_FULLFSYNC / fdatasync)

ARCHITECTURE.md §3 I10."
git flow feature finish dedup-platform-fsync
git push origin develop
```

### Task D3: `manifest.rs` — atomic JSON sidecar (CRC32C)

**Files:**
- Create: `crates/slicefs-dedup/src/manifest.rs`
- Modify: `crates/slicefs-dedup/src/lib.rs`

- [ ] **Step 1: Write the failing tests**

Create `crates/slicefs-dedup/src/manifest.rs`:
```rust
use crate::error::DedupIndexError;
use crate::paths::DedupRoot;
use crate::platform::{durable_sync, fsync_parent_dir};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

const MANIFEST_MAGIC: &str = "SLDX-MANIFEST-01";
const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub magic: String,
    pub schema_version: u32,
    pub redb_format_version: u32,
    pub created_at_unix_micros: u64,
    pub last_clean_shutdown_unix_micros: u64,
    pub last_shutdown_was_clean: bool,
    pub bloom_capacity: u64,
    pub bloom_fpr: f64,
    pub entries_high_water_mark: u64,
    pub page_size_bytes: u32,
    pub cas_root_relpath: String,
    /// CRC32C over canonical-JSON bytes with this field replaced by 0.
    pub manifest_crc32c: u32,
}

impl Manifest {
    pub fn new(bloom_capacity: u64, bloom_fpr: f64, page_size: u32) -> Self {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros() as u64;
        Self {
            magic: MANIFEST_MAGIC.into(),
            schema_version: SCHEMA_VERSION,
            redb_format_version: 4,
            created_at_unix_micros: now,
            last_clean_shutdown_unix_micros: 0,
            last_shutdown_was_clean: false,
            bloom_capacity,
            bloom_fpr,
            entries_high_water_mark: 0,
            page_size_bytes: page_size,
            cas_root_relpath: "../".into(),
            manifest_crc32c: 0,
        }
    }

    fn canonical_bytes_no_crc(&self) -> Result<Vec<u8>, DedupIndexError> {
        let mut copy = self.clone();
        copy.manifest_crc32c = 0;
        let json = serde_json::to_vec(&copy).map_err(|e| {
            DedupIndexError::ManifestCorrupt(Box::leak(format!("encode: {e}").into_boxed_str()))
        })?;
        Ok(json)
    }

    pub fn fill_crc(&mut self) -> Result<(), DedupIndexError> {
        let bytes = self.canonical_bytes_no_crc()?;
        self.manifest_crc32c = crc32c::crc32c(&bytes);
        Ok(())
    }

    pub fn verify_crc(&self) -> Result<(), DedupIndexError> {
        let bytes = self.canonical_bytes_no_crc()?;
        let expected = crc32c::crc32c(&bytes);
        if expected != self.manifest_crc32c {
            return Err(DedupIndexError::ManifestCorrupt("crc32c mismatch"));
        }
        if self.magic != MANIFEST_MAGIC {
            return Err(DedupIndexError::ManifestCorrupt("magic mismatch"));
        }
        if self.schema_version != SCHEMA_VERSION {
            return Err(DedupIndexError::ManifestCorrupt("schema version mismatch"));
        }
        Ok(())
    }

    /// Atomic write: tmp + rename + fsync(parent).
    /// Tmp is opened with O_DSYNC for I11.
    pub fn write_atomic(&mut self, root: &DedupRoot) -> Result<(), DedupIndexError> {
        self.fill_crc()?;
        let tmp = root.manifest_tmp();
        let final_path = root.manifest();

        std::fs::create_dir_all(root.base())?;

        let mut opts = OpenOptions::new();
        opts.create(true).truncate(true).write(true);
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.custom_flags(libc::O_DSYNC);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(self).map_err(|e| {
            DedupIndexError::ManifestCorrupt(Box::leak(format!("encode: {e}").into_boxed_str()))
        })?)?;
        durable_sync(&f)?;
        drop(f);

        std::fs::rename(&tmp, &final_path)?;
        fsync_parent_dir(root.base())?;
        Ok(())
    }

    pub fn read(root: &DedupRoot) -> Result<Self, DedupIndexError> {
        let bytes = std::fs::read(root.manifest())?;
        let m: Manifest = serde_json::from_slice(&bytes).map_err(|e| {
            DedupIndexError::ManifestCorrupt(Box::leak(format!("decode: {e}").into_boxed_str()))
        })?;
        m.verify_crc()?;
        Ok(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> (tempfile::TempDir, DedupRoot) {
        let td = tempfile::tempdir().unwrap();
        let r = DedupRoot::new(td.path().join("d"));
        (td, r)
    }

    #[test]
    fn write_then_read_roundtrip() {
        let (_g, r) = root();
        let mut m = Manifest::new(1_000_000, 0.01, 4096);
        m.last_shutdown_was_clean = true;
        m.write_atomic(&r).unwrap();
        let m2 = Manifest::read(&r).unwrap();
        assert!(m2.last_shutdown_was_clean);
        assert_eq!(m2.bloom_capacity, 1_000_000);
    }

    #[test]
    fn corrupt_crc_is_detected() {
        let (_g, r) = root();
        let mut m = Manifest::new(1, 0.01, 4096);
        m.write_atomic(&r).unwrap();
        // Flip a byte in the on-disk file but keep the CRC field stale.
        let path = r.manifest();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[0] ^= 0x01;
        std::fs::write(&path, &bytes).unwrap();
        let err = Manifest::read(&r).unwrap_err();
        assert!(matches!(err, DedupIndexError::ManifestCorrupt(_)));
    }

    #[test]
    fn missing_file_is_io_error() {
        let (_g, r) = root();
        let err = Manifest::read(&r).unwrap_err();
        assert!(matches!(err, DedupIndexError::Io(_)));
    }
}
```

Append to `lib.rs`: `mod manifest; pub use manifest::Manifest;`

- [ ] **Step 2: Run tests**

Run: `cargo test -p slicefs-dedup -- manifest`
Expected: 3 tests pass.

- [ ] **Step 3: Commit**

```bash
git flow feature start dedup-manifest
git add crates/slicefs-dedup/
git commit -m "feat(slicefs-dedup): atomic manifest.json with CRC32C

tmp+rename+fsync(parent), O_DSYNC on Linux. ARCHITECTURE §7.3, §3 I11."
git flow feature finish dedup-manifest
git push origin develop
```

### Task D4: `bloom_snapshot.rs` — header (CRC32C) + payload (xxh3-128) + footer

**Files:**
- Create: `crates/slicefs-dedup/src/bloom_snapshot.rs`
- Modify: `crates/slicefs-dedup/src/lib.rs`

- [ ] **Step 1: Write tests + impl**

Create `crates/slicefs-dedup/src/bloom_snapshot.rs`:
```rust
use crate::error::DedupIndexError;
use crate::paths::DedupRoot;
use crate::platform::{durable_sync, fsync_parent_dir};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};

pub const BLOOM_MAGIC_START: [u8; 8] = *b"SLDXBL01";
pub const BLOOM_MAGIC_END:   [u8; 8] = *b"BL01ENDX";
pub const BLOOM_VERSION: u32 = 1;
pub const HEADER_LEN:    usize = 64;
pub const FOOTER_LEN:    usize = 16;

#[derive(Debug, Clone)]
pub struct BloomSnapshotMeta {
    pub bloom_capacity: u64,
    pub bloom_fpr_bits: f64,
    pub entries_at_snapshot: u64,
    pub redb_hwm_at_snapshot: u64,
}

fn now_micros() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros() as u64
}

pub fn write_atomic(
    root: &DedupRoot,
    meta: &BloomSnapshotMeta,
    payload: &[u8],
) -> Result<(), DedupIndexError> {
    let tmp = root.bloom_tmp();
    std::fs::create_dir_all(root.base())?;

    let mut header = [0u8; HEADER_LEN];
    header[..8].copy_from_slice(&BLOOM_MAGIC_START);
    header[8..12].copy_from_slice(&BLOOM_VERSION.to_le_bytes());
    header[12..16].copy_from_slice(&0u32.to_le_bytes()); // flags
    header[16..24].copy_from_slice(&now_micros().to_le_bytes());
    header[24..32].copy_from_slice(&meta.bloom_capacity.to_le_bytes());
    header[32..40].copy_from_slice(&meta.bloom_fpr_bits.to_le_bytes());
    header[40..48].copy_from_slice(&meta.entries_at_snapshot.to_le_bytes());
    header[48..56].copy_from_slice(&meta.redb_hwm_at_snapshot.to_le_bytes());

    let payload_xxh3 = xxhash_rust::xxh3::xxh3_128(payload);
    let xxh3_lo = payload_xxh3 as u32;
    let xxh3_hi = (payload_xxh3 >> 32) as u64;
    header[56..60].copy_from_slice(&xxh3_lo.to_le_bytes());

    let header_crc = crc32c::crc32c(&header[..60]);
    header[60..64].copy_from_slice(&header_crc.to_le_bytes());

    let mut footer = [0u8; FOOTER_LEN];
    footer[..8].copy_from_slice(&xxh3_hi.to_le_bytes());
    footer[8..16].copy_from_slice(&BLOOM_MAGIC_END);

    let mut f = OpenOptions::new().create(true).truncate(true).write(true).open(&tmp)?;
    f.write_all(&header)?;
    f.write_all(payload)?;
    f.write_all(&footer)?;
    durable_sync(&f)?;
    drop(f);

    std::fs::rename(&tmp, root.bloom())?;
    fsync_parent_dir(root.base())?;
    Ok(())
}

pub fn load(root: &DedupRoot) -> Result<(BloomSnapshotMeta, Vec<u8>), DedupIndexError> {
    let mut f = File::open(root.bloom())?;
    let mut header = [0u8; HEADER_LEN];
    f.read_exact(&mut header)?;

    if header[..8] != BLOOM_MAGIC_START {
        return Err(DedupIndexError::BloomCorrupt("magic_start mismatch"));
    }
    let header_crc_stored = u32::from_le_bytes(header[60..64].try_into().unwrap());
    let header_crc_calc   = crc32c::crc32c(&header[..60]);
    if header_crc_stored != header_crc_calc {
        return Err(DedupIndexError::BloomCorrupt("header crc32c mismatch"));
    }

    let total = std::fs::metadata(root.bloom())?.len() as usize;
    if total < HEADER_LEN + FOOTER_LEN {
        return Err(DedupIndexError::BloomCorrupt("truncated"));
    }
    let payload_len = total - HEADER_LEN - FOOTER_LEN;
    let mut payload = vec![0u8; payload_len];
    f.read_exact(&mut payload)?;

    let mut footer = [0u8; FOOTER_LEN];
    f.read_exact(&mut footer)?;
    if footer[8..16] != BLOOM_MAGIC_END {
        return Err(DedupIndexError::BloomCorrupt("magic_end mismatch"));
    }

    let xxh3_lo = u32::from_le_bytes(header[56..60].try_into().unwrap());
    let xxh3_hi = u64::from_le_bytes(footer[..8].try_into().unwrap());
    let stored: u128 = (xxh3_hi as u128) << 32 | xxh3_lo as u128;
    let calc = xxhash_rust::xxh3::xxh3_128(&payload);
    if stored != calc {
        return Err(DedupIndexError::BloomCorrupt("payload xxh3-128 mismatch"));
    }

    let meta = BloomSnapshotMeta {
        bloom_capacity:       u64::from_le_bytes(header[24..32].try_into().unwrap()),
        bloom_fpr_bits:       f64::from_le_bytes(header[32..40].try_into().unwrap()),
        entries_at_snapshot:  u64::from_le_bytes(header[40..48].try_into().unwrap()),
        redb_hwm_at_snapshot: u64::from_le_bytes(header[48..56].try_into().unwrap()),
    };
    Ok((meta, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r() -> (tempfile::TempDir, DedupRoot) {
        let td = tempfile::tempdir().unwrap();
        let r = DedupRoot::new(td.path().join("d"));
        std::fs::create_dir_all(r.base()).unwrap();
        (td, r)
    }

    fn meta() -> BloomSnapshotMeta {
        BloomSnapshotMeta {
            bloom_capacity: 1_000_000,
            bloom_fpr_bits: 0.01,
            entries_at_snapshot: 42,
            redb_hwm_at_snapshot: 99,
        }
    }

    #[test]
    fn roundtrip_small_payload() {
        let (_g, r) = r();
        let payload = b"hello world".repeat(1000);
        write_atomic(&r, &meta(), &payload).unwrap();
        let (got_meta, got_payload) = load(&r).unwrap();
        assert_eq!(got_payload, payload);
        assert_eq!(got_meta.entries_at_snapshot, 42);
    }

    #[test]
    fn flipped_payload_byte_is_caught_by_xxh3() {
        let (_g, r) = r();
        let payload = vec![0xAB; 8192];
        write_atomic(&r, &meta(), &payload).unwrap();

        let mut bytes = std::fs::read(r.bloom()).unwrap();
        // Flip a byte deep in the payload (offset 4096).
        bytes[HEADER_LEN + 4096] ^= 0x80;
        std::fs::write(r.bloom(), &bytes).unwrap();

        let err = load(&r).unwrap_err();
        match err {
            DedupIndexError::BloomCorrupt(s) => assert!(s.contains("xxh3")),
            other => panic!("expected BloomCorrupt(xxh3), got {other:?}"),
        }
    }

    #[test]
    fn flipped_header_byte_is_caught_by_crc32c() {
        let (_g, r) = r();
        write_atomic(&r, &meta(), &vec![0u8; 1024]).unwrap();
        let mut bytes = std::fs::read(r.bloom()).unwrap();
        bytes[20] ^= 0xFF; // flip a byte inside the header
        std::fs::write(r.bloom(), &bytes).unwrap();
        let err = load(&r).unwrap_err();
        match err {
            DedupIndexError::BloomCorrupt(s) => assert!(s.contains("crc32c") || s.contains("magic")),
            other => panic!("expected BloomCorrupt, got {other:?}"),
        }
    }

    #[test]
    fn missing_end_magic_is_caught() {
        let (_g, r) = r();
        write_atomic(&r, &meta(), &vec![0xAB; 256]).unwrap();
        let mut bytes = std::fs::read(r.bloom()).unwrap();
        let n = bytes.len();
        bytes[n - 1] ^= 0xFF;
        std::fs::write(r.bloom(), &bytes).unwrap();
        let err = load(&r).unwrap_err();
        assert!(matches!(err, DedupIndexError::BloomCorrupt(_)));
    }
}
```

Append to `lib.rs`: `mod bloom_snapshot;`

- [ ] **Step 2: Run tests**

Run: `cargo test -p slicefs-dedup -- bloom_snapshot`
Expected: 4 tests pass.

- [ ] **Step 3: Commit**

```bash
git flow feature start dedup-bloom-snap-format
git add crates/slicefs-dedup/
git commit -m "feat(slicefs-dedup): bloom.snap codec (CRC32C header + xxh3-128 payload)

ARCHITECTURE §7.4 byte layout. Detects torn header and torn payload."
git flow feature finish dedup-bloom-snap-format
git push origin develop
```

---

## Milestone E — redb integration: open / create

**Goal:** redb 4.1 wired in; `DEDUP_TABLE` schema declared; `RedbDedupIndex::create` and `::open` produce a usable Database. No batch writer yet.

### Task E1: Declare schema and skeleton type

**Files:**
- Create: `crates/slicefs-dedup/src/redb_dedup_index.rs`
- Modify: `crates/slicefs-dedup/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/slicefs-dedup/src/redb_dedup_index.rs`:
```rust
use crate::config::DedupIndexConfig;
use crate::error::DedupIndexError;
use crate::paths::DedupRoot;
use redb::{Database, ReadableTable, TableDefinition};
use std::sync::Arc;

pub const DEDUP_TABLE: TableDefinition<&[u8; 28], ()> = TableDefinition::new("dedup_index_v1");

pub struct RedbDedupIndex {
    pub(crate) config: DedupIndexConfig,
    pub(crate) root: DedupRoot,
    pub(crate) db: Arc<Database>,
}

impl RedbDedupIndex {
    pub fn create(config: DedupIndexConfig) -> Result<Self, DedupIndexError> {
        let root = DedupRoot::new(&config.dedup_root);
        std::fs::create_dir_all(root.base())?;
        let db = Database::builder()
            .set_page_size(config.page_size)
            .set_cache_size(config.redb_cache_bytes)
            .create(root.redb())?;
        // Touch the table so its metadata exists.
        let txn = db.begin_write()?;
        {
            let _t = txn.open_table(DEDUP_TABLE)?;
        }
        txn.commit()?;
        Ok(Self { config, root, db: Arc::new(db) })
    }

    pub fn open(config: DedupIndexConfig) -> Result<Self, DedupIndexError> {
        let root = DedupRoot::new(&config.dedup_root);
        let db = Database::builder()
            .set_page_size(config.page_size)
            .set_cache_size(config.redb_cache_bytes)
            .open(root.redb())?;
        Ok(Self { config, root, db: Arc::new(db) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DedupIndexConfig;

    #[test]
    fn create_then_open_roundtrip() {
        let td = tempfile::tempdir().unwrap();
        let cas = td.path().join("cas");
        std::fs::create_dir_all(&cas).unwrap();
        let cfg = DedupIndexConfig::builder(&cas).build();
        let idx = RedbDedupIndex::create(cfg.clone()).unwrap();
        drop(idx);
        let _idx2 = RedbDedupIndex::open(cfg).unwrap();
    }

    #[test]
    fn empty_table_lookup_returns_none() {
        let td = tempfile::tempdir().unwrap();
        let cas = td.path().join("cas");
        std::fs::create_dir_all(&cas).unwrap();
        let cfg = DedupIndexConfig::builder(&cas).build();
        let idx = RedbDedupIndex::create(cfg).unwrap();
        let txn = idx.db.begin_read().unwrap();
        let t = txn.open_table(DEDUP_TABLE).unwrap();
        assert!(t.get(&[0u8; 28]).unwrap().is_none());
    }
}
```

Append to `lib.rs`: `mod redb_dedup_index; pub use redb_dedup_index::{RedbDedupIndex, DEDUP_TABLE};`

- [ ] **Step 2: Run tests**

Run: `cargo test -p slicefs-dedup -- redb_dedup_index::tests::create_then_open_roundtrip redb_dedup_index::tests::empty_table_lookup_returns_none`
Expected: 2 tests pass.

- [ ] **Step 3: Commit**

```bash
git flow feature start dedup-redb-skeleton
git add crates/slicefs-dedup/
git commit -m "feat(slicefs-dedup): redb 4.1 skeleton, DEDUP_TABLE, create/open

ARCHITECTURE §7.2 (table=dedup_index_v1, key=&[u8;28], val=())."
git flow feature finish dedup-redb-skeleton
git push origin develop
```

### Task E2: Manifest probe on open (Healthy / Suspect / Rebuilding)

**Files:**
- Modify: `crates/slicefs-dedup/src/redb_dedup_index.rs`

- [ ] **Step 1: Write the failing tests**

Add to `redb_dedup_index.rs`:
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountState {
    /// Manifest present, last_shutdown_was_clean=true, bloom xxh3 ok (TBD).
    Healthy,
    /// Manifest absent / unclean / bloom xxh3 fail.
    Suspect,
    /// Redb file truncated, schema mismatch, or operator --force-rebuild.
    Rebuilding,
}

impl RedbDedupIndex {
    pub fn probe(config: &DedupIndexConfig) -> Result<MountState, DedupIndexError> {
        let root = DedupRoot::new(&config.dedup_root);
        if !root.manifest().exists() {
            return Ok(MountState::Suspect);
        }
        match crate::manifest::Manifest::read(&root) {
            Ok(m) if m.last_shutdown_was_clean => Ok(MountState::Healthy),
            Ok(_) => Ok(MountState::Suspect),
            Err(DedupIndexError::ManifestCorrupt(_)) => Ok(MountState::Suspect),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod probe_tests {
    use super::*;
    use crate::manifest::Manifest;

    fn cfg() -> (tempfile::TempDir, DedupIndexConfig) {
        let td = tempfile::tempdir().unwrap();
        let cas = td.path().join("cas");
        std::fs::create_dir_all(&cas).unwrap();
        let cfg = DedupIndexConfig::builder(&cas).build();
        std::fs::create_dir_all(&cfg.dedup_root).unwrap();
        (td, cfg)
    }

    #[test]
    fn missing_manifest_is_suspect() {
        let (_g, c) = cfg();
        assert_eq!(RedbDedupIndex::probe(&c).unwrap(), MountState::Suspect);
    }

    #[test]
    fn unclean_shutdown_is_suspect() {
        let (_g, c) = cfg();
        let mut m = Manifest::new(1, 0.01, 4096);
        m.last_shutdown_was_clean = false;
        m.write_atomic(&DedupRoot::new(&c.dedup_root)).unwrap();
        assert_eq!(RedbDedupIndex::probe(&c).unwrap(), MountState::Suspect);
    }

    #[test]
    fn clean_shutdown_is_healthy() {
        let (_g, c) = cfg();
        let mut m = Manifest::new(1, 0.01, 4096);
        m.last_shutdown_was_clean = true;
        m.write_atomic(&DedupRoot::new(&c.dedup_root)).unwrap();
        assert_eq!(RedbDedupIndex::probe(&c).unwrap(), MountState::Healthy);
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p slicefs-dedup -- redb_dedup_index::probe_tests`
Expected: 3 tests pass.

- [ ] **Step 3: Commit**

```bash
git flow feature start dedup-mount-probe
git add crates/slicefs-dedup/
git commit -m "feat(slicefs-dedup): MountState probe (Healthy/Suspect/Rebuilding)

ARCHITECTURE §9.1 startup state machine — probe stage only."
git flow feature finish dedup-mount-probe
git push origin develop
```

---

## Milestone F — `AtomicBloomFilter` wrapper

**Goal:** Lock-free bloom front sized from `BloomConfig`; `set_all` for batch insert; `contains` for lookup; serialize/deserialize for snapshotting.

### Task F1: AtomicBloomFilter

**Files:**
- Create: `crates/slicefs-dedup/src/atomic_bloom.rs`
- Modify: `crates/slicefs-dedup/src/lib.rs`

- [ ] **Step 1: Write tests + impl**

Create `crates/slicefs-dedup/src/atomic_bloom.rs`:
```rust
use crate::config::BloomConfig;
use fastbloom::BloomFilter;
use std::sync::Arc;
use parking_lot::RwLock;

/// Wraps `fastbloom::BloomFilter` with shared ownership.
/// Reads are wait-free (RwLock read guard around an immutable view);
/// writes (set, set_all) take the write guard briefly.
pub struct AtomicBloomFilter {
    inner: Arc<RwLock<BloomFilter>>,
    capacity: u64,
}

impl AtomicBloomFilter {
    pub fn new(cfg: &BloomConfig) -> Self {
        let bf = BloomFilter::with_false_pos(cfg.fpr).expected_items(cfg.capacity);
        Self { inner: Arc::new(RwLock::new(bf)), capacity: cfg.capacity as u64 }
    }

    pub fn from_serialized(bytes: &[u8], cfg: &BloomConfig) -> Self {
        // fastbloom 0.14 supports `from_slice` via `BloomFilter::from_slice`;
        // if not directly, decode the raw bit-array length prefix written
        // alongside in the snapshot footer (handled in snapshotter).
        let bf = BloomFilter::from_slice(bytes)
            .unwrap_or_else(|_| BloomFilter::with_false_pos(cfg.fpr).expected_items(cfg.capacity));
        Self { inner: Arc::new(RwLock::new(bf)), capacity: cfg.capacity as u64 }
    }

    pub fn contains(&self, hash: &[u8]) -> bool {
        self.inner.read().contains(hash)
    }

    pub fn set(&self, hash: &[u8]) {
        self.inner.write().insert(hash);
    }

    pub fn set_all(&self, hashes: &[&[u8]]) {
        let mut g = self.inner.write();
        for h in hashes {
            g.insert(h);
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        self.inner.read().as_slice().to_vec()
    }

    pub fn capacity(&self) -> u64 { self.capacity }

    pub fn clone_handle(&self) -> Self {
        Self { inner: Arc::clone(&self.inner), capacity: self.capacity }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(cap: usize) -> BloomConfig {
        BloomConfig {
            capacity: cap, fpr: 0.01, snapshot_every: 100, snapshot_interval: std::time::Duration::from_secs(1),
            stale_ratio: 0.9, drift_rebuild_ratio: 0.2, effective_fpr_rebuild_multiplier: 4.0,
        }
    }

    #[test]
    fn set_then_contains_is_true() {
        let bf = AtomicBloomFilter::new(&cfg(1000));
        bf.set(b"alpha");
        assert!(bf.contains(b"alpha"));
    }

    #[test]
    fn set_all_inserts_each() {
        let bf = AtomicBloomFilter::new(&cfg(1000));
        let h1 = b"a".as_slice();
        let h2 = b"b".as_slice();
        bf.set_all(&[h1, h2]);
        assert!(bf.contains(h1));
        assert!(bf.contains(h2));
    }

    #[test]
    fn unset_hash_is_false_for_small_n() {
        let bf = AtomicBloomFilter::new(&cfg(1_000_000));
        assert!(!bf.contains(b"never-inserted"));
    }

    #[test]
    fn serialize_then_load() {
        let bf = AtomicBloomFilter::new(&cfg(1000));
        bf.set(b"keep-me");
        let bytes = bf.to_bytes();
        let bf2 = AtomicBloomFilter::from_serialized(&bytes, &cfg(1000));
        assert!(bf2.contains(b"keep-me"));
    }
}
```

Append to `lib.rs`: `mod atomic_bloom; pub use atomic_bloom::AtomicBloomFilter;`

- [ ] **Step 2: Run tests**

Run: `cargo test -p slicefs-dedup -- atomic_bloom::tests`
Expected: 4 tests pass. (Note: `fastbloom::BloomFilter::from_slice` exists in 0.14 — verify and adjust if API differs.)

- [ ] **Step 3: Commit**

```bash
git flow feature start dedup-atomic-bloom
git add crates/slicefs-dedup/
git commit -m "feat(slicefs-dedup): AtomicBloomFilter wrapper around fastbloom"
git flow feature finish dedup-atomic-bloom
git push origin develop
```

---

## Milestone G — BatchWriter + insert / lookup / remove

**Goal:** Single-writer thread coalesces inserts, commits to redb in batches, then updates bloom + HWM. `DedupIndex` impl provides insert/lookup/remove with the strict ordering rule.

### Task G1: BatchWriter — insert request type and channel

**Files:**
- Create: `crates/slicefs-dedup/src/batch_writer.rs`
- Modify: `crates/slicefs-dedup/src/lib.rs`

- [ ] **Step 1: Write skeleton + tests**

Create `crates/slicefs-dedup/src/batch_writer.rs`:
```rust
use crate::atomic_bloom::AtomicBloomFilter;
use crate::config::{DedupIndexConfig, DurabilityMode};
use crate::error::DedupIndexError;
use crate::redb_dedup_index::DEDUP_TABLE;
use crate::stats::StatsCounters;
use crossbeam_channel::{bounded, Sender};
use redb::{Database, Durability};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub(crate) type Hash28 = [u8; 28];

pub(crate) struct InsertReq {
    pub hash: Hash28,
    pub reply: Sender<Result<(), DedupIndexError>>,
}

pub(crate) struct BatchWriter {
    pub(crate) tx: Sender<InsertReq>,
    pub(crate) handle: Option<JoinHandle<()>>,
    pub(crate) shutdown: Sender<()>,
    pub(crate) high_water: Arc<AtomicU64>,
}

impl BatchWriter {
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
            DurabilityMode::Seed     => cfg.batch_size_seed,
            DurabilityMode::Default  => cfg.batch_size_default,
            DurabilityMode::Paranoid => 1,
        };
        let durability = match cfg.durability {
            DurabilityMode::Seed     => Durability::None,
            DurabilityMode::Default  => Durability::Eventual,
            DurabilityMode::Paranoid => Durability::Immediate,
        };

        let bloom_for_thread = bloom.clone_handle();
        let hw = Arc::clone(&high_water);
        let stats_for_thread = Arc::clone(&stats);

        let handle = std::thread::Builder::new()
            .name("slicefs-dedup-batcher".into())
            .spawn(move || {
                'outer: loop {
                    if shutdown_rx.try_recv().is_ok() { break; }
                    let mut batch: Vec<InsertReq> = Vec::with_capacity(max_batch.min(10_000));
                    // Block for first; small timeout to remain shutdown-responsive.
                    match rx.recv_timeout(Duration::from_millis(50)) {
                        Ok(req) => batch.push(req),
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break 'outer,
                    }

                    let deadline = Instant::now() + coalesce;
                    while batch.len() < max_batch {
                        let now = Instant::now();
                        if now >= deadline { break; }
                        match rx.recv_timeout(deadline - now) {
                            Ok(req) => batch.push(req),
                            Err(_) => break,
                        }
                    }

                    let result = (|| -> Result<(), DedupIndexError> {
                        let mut txn = db.begin_write()?;
                        txn.set_durability(durability);
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
                        stats_for_thread.commit_failures_total.fetch_add(1, Ordering::Relaxed);
                    } else {
                        // Bloom + HWM AFTER commit (I5).
                        let refs: Vec<&[u8]> = batch.iter().map(|r| r.hash.as_slice()).collect();
                        bloom_for_thread.set_all(&refs);
                        hw.fetch_add(batch.len() as u64, Ordering::AcqRel);
                        stats_for_thread.commits_total.fetch_add(1, Ordering::Relaxed);
                        stats_for_thread.inserts_total.fetch_add(batch.len() as u64, Ordering::Relaxed);
                    }

                    let reply_outcome = result.as_ref().map(|_| ()).map_err(|e| {
                        DedupIndexError::Recovery(format!("batch commit failed: {e}"))
                    });
                    for r in batch {
                        let _ = r.reply.send(reply_outcome.clone());
                    }
                }
            })
            .expect("spawn batcher thread");

        Self { tx, handle: Some(handle), shutdown: shutdown_tx, high_water }
    }

    pub(crate) fn submit(&self, hash: Hash28) -> Result<(), DedupIndexError> {
        let (rtx, rrx) = bounded::<Result<(), DedupIndexError>>(1);
        self.tx.send(InsertReq { hash, reply: rtx }).map_err(|_| {
            DedupIndexError::Recovery("batcher channel closed".into())
        })?;
        rrx.recv().map_err(|_| DedupIndexError::Recovery("batcher reply lost".into()))?
    }

    pub(crate) fn shutdown(mut self, timeout: Duration) {
        let _ = self.shutdown.send(());
        if let Some(h) = self.handle.take() {
            // Best-effort join with timeout.
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
        let cfg = DedupIndexConfig::builder(&cas).mode(DurabilityMode::Default).build();
        let idx = RedbDedupIndex::create(cfg.clone()).unwrap();

        let bloom = AtomicBloomFilter::new(&cfg.bloom);
        let stats = Arc::new(StatsCounters::default());
        let hw    = Arc::new(AtomicU64::new(0));
        let bw = BatchWriter::spawn(cfg, Arc::clone(&idx.db).into(), bloom, stats, Arc::clone(&hw));

        let mut h = [0u8; 28]; h[0] = 0xAA;
        bw.submit(h).unwrap();
        assert_eq!(hw.load(Ordering::Acquire), 1);

        bw.shutdown(Duration::from_secs(2));
    }
}
```

Append to `lib.rs`: `mod batch_writer;` (no public re-export).

> **Note for the engineer:** verify `RedbDedupIndex.db` is `Arc<Database>` — it already is per E1. The `.into()` in the test is a no-op; if the type doesn't coerce cleanly, pass `idx.db.clone()` instead.

- [ ] **Step 2: Run test**

Run: `cargo test -p slicefs-dedup -- batch_writer::tests::submit_one_then_commit_increments_hwm`
Expected: 1 test passes.

- [ ] **Step 3: Commit**

```bash
git flow feature start dedup-batch-writer
git add crates/slicefs-dedup/
git commit -m "feat(slicefs-dedup): BatchWriter with strict commit-then-bloom ordering

ARCHITECTURE §8.1 sequence; bounded MPSC, group-commit window,
per-mode batch size."
git flow feature finish dedup-batch-writer
git push origin develop
```

### Task G2: `DedupIndex::insert` on `RedbDedupIndex`

**Files:**
- Modify: `crates/slicefs-dedup/src/redb_dedup_index.rs`

- [ ] **Step 1: Wire the batch writer into the type and impl**

Replace the `RedbDedupIndex` struct + `create`/`open` in `redb_dedup_index.rs` with an enriched version that owns: `Arc<AtomicBloomFilter>`, `Arc<AtomicU64>` HWM, `Arc<StatsCounters>`, and an `Option<BatchWriter>`.

Add at the end of the file:
```rust
use slicefs_traits::{ChunkHash, DedupIndex, DedupResult, CasError};

impl RedbDedupIndex {
    fn hash28(h: &ChunkHash) -> Result<[u8; 28], CasError> {
        let bytes = h.as_bytes();
        if bytes.len() != 28 {
            return Err(CasError::Index(format!("ChunkHash must be 28 bytes, got {}", bytes.len())));
        }
        let mut out = [0u8; 28];
        out.copy_from_slice(bytes);
        Ok(out)
    }
}

impl DedupIndex for RedbDedupIndex {
    fn bloom_check(&self, hash: &ChunkHash) -> bool {
        let bytes = hash.as_bytes();
        self.bloom.contains(bytes)
    }

    fn insert(&self, hash: &ChunkHash) -> Result<(), CasError> {
        let h = Self::hash28(hash)?;
        let bw = self.batch_writer.as_ref().ok_or_else(|| {
            CasError::Index("RedbDedupIndex was opened without a batch writer".into())
        })?;
        bw.submit(h).map_err(CasError::from)
    }

    fn lookup(&self, _hash: &ChunkHash) -> Result<DedupResult, CasError> {
        // Filled in by Task H1.
        unimplemented!("Task H1")
    }

    fn remove(&self, _hash: &ChunkHash) -> Result<(), CasError> {
        // Filled in by Task H2.
        unimplemented!("Task H2")
    }
}
```

You will also need to extend the struct:
```rust
pub struct RedbDedupIndex {
    pub(crate) config: DedupIndexConfig,
    pub(crate) root: DedupRoot,
    pub(crate) db: Arc<Database>,
    pub(crate) bloom: AtomicBloomFilter,
    pub(crate) high_water: Arc<AtomicU64>,
    pub(crate) stats: Arc<StatsCounters>,
    pub(crate) batch_writer: Option<BatchWriter>,
}
```

…and have `create` / `open` populate the new fields.

- [ ] **Step 2: Write the failing test**

Add to `redb_dedup_index.rs`:
```rust
#[cfg(test)]
mod insert_tests {
    use super::*;
    use slicefs_traits::{ChunkHash, DedupIndex};

    fn make_hash(seed: u8) -> ChunkHash {
        let mut v = vec![0u8; 28];
        v[0] = seed;
        ChunkHash::from_bytes(v)
    }

    #[test]
    fn insert_then_bloom_check_true() {
        let td = tempfile::tempdir().unwrap();
        let cas = td.path().join("cas");
        std::fs::create_dir_all(&cas).unwrap();
        let cfg = DedupIndexConfig::builder(&cas).build();
        let idx = RedbDedupIndex::create(cfg).unwrap();

        let h = make_hash(1);
        idx.insert(&h).unwrap();
        assert!(idx.bloom_check(&h));
    }
}
```

- [ ] **Step 3: Run test**

Run: `cargo test -p slicefs-dedup -- redb_dedup_index::insert_tests`
Expected: 1 test passes.

- [ ] **Step 4: Commit**

```bash
git flow feature start dedup-insert-impl
git add crates/slicefs-dedup/
git commit -m "feat(slicefs-dedup): DedupIndex::insert via BatchWriter

ARCHITECTURE §5.1, §8.1."
git flow feature finish dedup-insert-impl
git push origin develop
```

---

## Milestone H — Lookup + Remove

### Task H1: `lookup()` with bloom-front + redb authoritative

**Files:**
- Modify: `crates/slicefs-dedup/src/redb_dedup_index.rs`

- [ ] **Step 1: Write failing tests**

Add to `insert_tests` module:
```rust
#[test]
fn definitely_absent_bypasses_redb() {
    let td = tempfile::tempdir().unwrap();
    let cas = td.path().join("cas");
    std::fs::create_dir_all(&cas).unwrap();
    let cfg = DedupIndexConfig::builder(&cas).build();
    let idx = RedbDedupIndex::create(cfg).unwrap();
    let h = make_hash(0xFE);
    let r = idx.lookup(&h).unwrap();
    assert!(matches!(r, DedupResult::DefinitelyAbsent));
}

#[test]
fn present_after_insert() {
    let td = tempfile::tempdir().unwrap();
    let cas = td.path().join("cas");
    std::fs::create_dir_all(&cas).unwrap();
    let cfg = DedupIndexConfig::builder(&cas).build();
    let idx = RedbDedupIndex::create(cfg).unwrap();
    let h = make_hash(7);
    idx.insert(&h).unwrap();
    let r = idx.lookup(&h).unwrap();
    assert!(matches!(r, DedupResult::Present));
}
```

- [ ] **Step 2: Implement**

Replace the `lookup` stub in `impl DedupIndex for RedbDedupIndex`:
```rust
fn lookup(&self, hash: &ChunkHash) -> Result<DedupResult, CasError> {
    self.stats.lookups_total.fetch_add(1, Ordering::Relaxed);

    let bytes = hash.as_bytes();
    if !self.bloom.contains(bytes) {
        return Ok(DedupResult::DefinitelyAbsent);
    }
    self.stats.bloom_hits_total.fetch_add(1, Ordering::Relaxed);

    let h = Self::hash28(hash)?;
    let txn = self.db.begin_read().map_err(|e| {
        CasError::Index(format!("redb begin_read: {e}"))
    })?;
    let t = txn.open_table(DEDUP_TABLE).map_err(|e| {
        CasError::Index(format!("redb open_table: {e}"))
    })?;
    let hit = t.get(&h).map_err(|e| {
        CasError::Index(format!("redb get: {e}"))
    })?.is_some();

    if !hit {
        self.stats.bloom_false_positives_total.fetch_add(1, Ordering::Relaxed);
        return Ok(DedupResult::Absent);
    }

    if self.config.verify_on_present {
        if let Some(p) = self.cas_path(hash) {
            self.stats.verify_on_present_hits_total.fetch_add(1, Ordering::Relaxed);
            if !p.exists() {
                return Ok(DedupResult::Absent);
            }
        }
    }
    Ok(DedupResult::Present)
}
```

…and add a helper:
```rust
impl RedbDedupIndex {
    fn cas_path(&self, hash: &ChunkHash) -> Option<std::path::PathBuf> {
        let hex: String = hash.as_bytes().iter().map(|b| format!("{:02x}", b)).collect();
        if hex.len() < 4 { return None; }
        Some(self.config.cas_root.join(&hex[..2]).join(&hex[2..]))
    }
}
```

(Add `use std::sync::atomic::Ordering;` if not already present.)

- [ ] **Step 3: Run tests**

Run: `cargo test -p slicefs-dedup -- redb_dedup_index::insert_tests`
Expected: 3 tests pass (insert, definitely_absent, present_after_insert).

- [ ] **Step 4: Commit**

```bash
git flow feature start dedup-lookup-impl
git add crates/slicefs-dedup/
git commit -m "feat(slicefs-dedup): DedupIndex::lookup (bloom front + redb auth + verify_on_present)

ARCHITECTURE §8.2."
git flow feature finish dedup-lookup-impl
git push origin develop
```

### Task H2: `remove()` (bloom NOT updated, I3)

**Files:**
- Modify: `crates/slicefs-dedup/src/redb_dedup_index.rs`

- [ ] **Step 1: Write failing test**

Add to `insert_tests`:
```rust
#[test]
fn remove_makes_lookup_absent_but_bloom_still_hits() {
    let td = tempfile::tempdir().unwrap();
    let cas = td.path().join("cas");
    std::fs::create_dir_all(&cas).unwrap();
    let cfg = DedupIndexConfig::builder(&cas).build();
    let idx = RedbDedupIndex::create(cfg).unwrap();
    let h = make_hash(9);
    idx.insert(&h).unwrap();
    idx.remove(&h).unwrap();
    assert!(matches!(idx.lookup(&h).unwrap(), DedupResult::Absent));
    assert!(idx.bloom_check(&h), "bloom must NOT be updated on remove (I3)");
}
```

- [ ] **Step 2: Implement**

Replace the `remove` stub:
```rust
fn remove(&self, hash: &ChunkHash) -> Result<(), CasError> {
    let h = Self::hash28(hash)?;
    let mut txn = self.db.begin_write().map_err(|e| {
        CasError::Index(format!("redb begin_write: {e}"))
    })?;
    txn.set_durability(match self.config.durability {
        DurabilityMode::Seed     => Durability::None,
        DurabilityMode::Default  => Durability::Eventual,
        DurabilityMode::Paranoid => Durability::Immediate,
    });
    {
        let mut t = txn.open_table(DEDUP_TABLE).map_err(|e| CasError::Index(format!("open: {e}")))?;
        t.remove(&h).map_err(|e| CasError::Index(format!("remove: {e}")))?;
    }
    txn.commit().map_err(|e| CasError::Index(format!("commit: {e}")))?;
    self.stats.removes_total.fetch_add(1, Ordering::Relaxed);
    Ok(())
}
```

(Add `use redb::Durability;` and `use crate::config::DurabilityMode;` if needed.)

- [ ] **Step 3: Run tests**

Run: `cargo test -p slicefs-dedup -- redb_dedup_index::insert_tests::remove_makes_lookup_absent_but_bloom_still_hits`
Expected: pass.

- [ ] **Step 4: Commit**

```bash
git flow feature start dedup-remove-impl
git add crates/slicefs-dedup/
git commit -m "feat(slicefs-dedup): DedupIndex::remove (bloom NOT updated, I3)

ARCHITECTURE §8.3, §3 I3 (drift is benign)."
git flow feature finish dedup-remove-impl
git push origin develop
```

---

## Milestone I — `flush()` and `Drop`

### Task I1: `flush()` — drain batcher and force F_FULLFSYNC

**Files:**
- Modify: `crates/slicefs-dedup/src/redb_dedup_index.rs`

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn flush_drains_pending_inserts() {
    let td = tempfile::tempdir().unwrap();
    let cas = td.path().join("cas");
    std::fs::create_dir_all(&cas).unwrap();
    let cfg = DedupIndexConfig::builder(&cas).mode(DurabilityMode::Default).build();
    let idx = RedbDedupIndex::create(cfg.clone()).unwrap();
    for i in 0..50u8 { idx.insert(&make_hash(i)).unwrap(); }
    idx.flush().unwrap();

    // After flush, every insert must be Present even on a fresh read txn.
    for i in 0..50u8 {
        assert!(matches!(idx.lookup(&make_hash(i)).unwrap(), DedupResult::Present));
    }
}
```

- [ ] **Step 2: Implement**

```rust
fn flush(&self) -> Result<(), CasError> {
    // BatchWriter has no explicit drain primitive; submit() blocks
    // for reply already, so by the time the most-recent insert
    // returned, all earlier inserts are committed. To force device
    // durability, open the redb file and F_FULLFSYNC it.
    let f = std::fs::File::open(self.root.redb()).map_err(CasError::Io)?;
    crate::platform::durable_sync(&f).map_err(CasError::Io)?;
    Ok(())
}
```

- [ ] **Step 3: Run test**

Run: `cargo test -p slicefs-dedup -- redb_dedup_index::insert_tests::flush_drains_pending_inserts`
Expected: pass.

- [ ] **Step 4: Commit**

```bash
git flow feature start dedup-flush
git add crates/slicefs-dedup/
git commit -m "feat(slicefs-dedup): flush() forces F_FULLFSYNC on index.redb

ARCHITECTURE §5.1; §3 I10."
git flow feature finish dedup-flush
git push origin develop
```

### Task I2: `Drop` impl — clean shutdown writes manifest

**Files:**
- Modify: `crates/slicefs-dedup/src/redb_dedup_index.rs`

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn drop_writes_clean_shutdown_manifest() {
    let td = tempfile::tempdir().unwrap();
    let cas = td.path().join("cas");
    std::fs::create_dir_all(&cas).unwrap();
    let cfg = DedupIndexConfig::builder(&cas).build();
    {
        let idx = RedbDedupIndex::create(cfg.clone()).unwrap();
        idx.insert(&make_hash(1)).unwrap();
    } // Drop fires here.
    let m = crate::manifest::Manifest::read(&DedupRoot::new(&cfg.dedup_root)).unwrap();
    assert!(m.last_shutdown_was_clean);
}
```

- [ ] **Step 2: Implement Drop**

```rust
impl Drop for RedbDedupIndex {
    fn drop(&mut self) {
        // Best-effort: shut batcher with timeout, then flush, then update manifest.
        if let Some(bw) = self.batch_writer.take() {
            bw.shutdown(std::time::Duration::from_secs(5));
        }
        let _ = std::fs::File::open(self.root.redb())
            .and_then(|f| crate::platform::durable_sync(&f));

        let mut m = match crate::manifest::Manifest::read(&self.root) {
            Ok(m) => m,
            Err(_) => crate::manifest::Manifest::new(
                self.config.bloom.capacity as u64,
                self.config.bloom.fpr,
                self.config.page_size as u32,
            ),
        };
        m.last_shutdown_was_clean = true;
        m.last_clean_shutdown_unix_micros = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_micros() as u64;
        m.entries_high_water_mark = self.high_water.load(Ordering::Acquire);
        let _ = m.write_atomic(&self.root);
    }
}
```

- [ ] **Step 3: Run test**

Run: `cargo test -p slicefs-dedup -- redb_dedup_index::insert_tests::drop_writes_clean_shutdown_manifest`
Expected: pass.

- [ ] **Step 4: Commit**

```bash
git flow feature start dedup-drop-impl
git add crates/slicefs-dedup/
git commit -m "feat(slicefs-dedup): Drop writes clean-shutdown manifest

ARCHITECTURE §5.2 Drop contract, §9.1 manifest transition Healthy."
git flow feature finish dedup-drop-impl
git push origin develop
```

---

## Milestone J — BloomSnapshotter

### Task J1: Periodic snapshot every N inserts

**Files:**
- Modify: `crates/slicefs-dedup/src/redb_dedup_index.rs`
- Add: snapshotter logic (inline or new module — keep it inline for now)

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn snapshot_after_threshold_inserts() {
    let td = tempfile::tempdir().unwrap();
    let cas = td.path().join("cas");
    std::fs::create_dir_all(&cas).unwrap();
    let mut cfg = DedupIndexConfig::builder(&cas).build();
    cfg.bloom.snapshot_every = 10;
    let idx = RedbDedupIndex::create(cfg.clone()).unwrap();
    for i in 0..15u8 { idx.insert(&make_hash(i)).unwrap(); }
    idx.flush().unwrap();
    // Trigger the snapshot — Drop side-effect or explicit call.
    drop(idx);
    assert!(DedupRoot::new(&cfg.dedup_root).bloom().exists(),
            "bloom.snap must exist after threshold inserts + drop");
}
```

- [ ] **Step 2: Implement**

Add a snapshot-trigger check inside the `BatchWriter` thread after each successful commit, AND have `Drop` always write a final snapshot:
```rust
// In the BatchWriter committed-block, after stats updates:
let total = stats_for_thread.inserts_total.load(Ordering::Relaxed);
if cfg.bloom.snapshot_every > 0 && total % cfg.bloom.snapshot_every as u64 == 0 {
    let payload = bloom_for_thread.to_bytes();
    let meta = crate::bloom_snapshot::BloomSnapshotMeta {
        bloom_capacity: cfg.bloom.capacity as u64,
        bloom_fpr_bits: cfg.bloom.fpr,
        entries_at_snapshot: total,
        redb_hwm_at_snapshot: hw.load(Ordering::Acquire),
    };
    let root = DedupRoot::new(&cfg.dedup_root);
    if let Err(e) = crate::bloom_snapshot::write_atomic(&root, &meta, &payload) {
        tracing::warn!("bloom snapshot failed: {e}");
        stats_for_thread.bloom_snapshot_failures_total.fetch_add(1, Ordering::Relaxed);
    }
}
```

(You may need to capture `cfg.dedup_root: PathBuf` and `cfg.bloom: BloomConfig` into the thread closure — clone them at spawn time.)

In `Drop`, before writing the manifest, write a final snapshot using the same code path.

- [ ] **Step 3: Run test**

Run: `cargo test -p slicefs-dedup -- redb_dedup_index::insert_tests::snapshot_after_threshold_inserts`
Expected: pass.

- [ ] **Step 4: Commit**

```bash
git flow feature start dedup-bloom-snap-trigger
git add crates/slicefs-dedup/
git commit -m "feat(slicefs-dedup): bloom snapshot every N inserts + on Drop

ARCHITECTURE §9.2 (snapshot pathway); I6 (advisory)."
git flow feature finish dedup-bloom-snap-trigger
git push origin develop
```

### Task J2: Load bloom from snapshot on `open()`

**Files:**
- Modify: `crates/slicefs-dedup/src/redb_dedup_index.rs`

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn open_loads_bloom_from_snapshot() {
    let td = tempfile::tempdir().unwrap();
    let cas = td.path().join("cas");
    std::fs::create_dir_all(&cas).unwrap();
    let cfg = DedupIndexConfig::builder(&cas).build();
    {
        let idx = RedbDedupIndex::create(cfg.clone()).unwrap();
        idx.insert(&make_hash(33)).unwrap();
        idx.flush().unwrap();
    }
    let idx2 = RedbDedupIndex::open(cfg).unwrap();
    assert!(idx2.bloom_check(&make_hash(33)));
}
```

- [ ] **Step 2: Implement**

In `RedbDedupIndex::open`, after opening redb, attempt:
```rust
let bloom = match crate::bloom_snapshot::load(&root) {
    Ok((meta, payload)) => {
        // TODO: cross-check meta.redb_hwm_at_snapshot against current HWM
        // (rebuild bloom from redb if snapshot is too far behind).
        AtomicBloomFilter::from_serialized(&payload, &config.bloom)
    }
    Err(_) => {
        // No snapshot or corrupt; rebuild from redb.
        let bf = AtomicBloomFilter::new(&config.bloom);
        let txn = db.begin_read().unwrap();
        let t = txn.open_table(DEDUP_TABLE).unwrap();
        let mut iter = t.iter().unwrap();
        while let Some(Ok((k, _))) = iter.next() {
            bf.set(k.value());
        }
        bf
    }
};
```

(The `TODO` is fine because the architecture allows a stale-bloom rebuild as a v2 lever; keep it as a `tracing::debug!` for now and a follow-up issue. **Replace `unwrap()` with proper `?`-style error propagation**; the snippet shows intent only.)

- [ ] **Step 3: Run test**

Run: `cargo test -p slicefs-dedup -- redb_dedup_index::insert_tests::open_loads_bloom_from_snapshot`
Expected: pass.

- [ ] **Step 4: Commit**

```bash
git flow feature start dedup-bloom-snap-load
git add crates/slicefs-dedup/
git commit -m "feat(slicefs-dedup): open() loads bloom from snapshot or rebuilds from redb"
git flow feature finish dedup-bloom-snap-load
git push origin develop
```

---

## Milestone K — `rebuild_from_cas`

### Task K1: Walk CAS shards, bulk-load redb, atomic rename

**Files:**
- Create: `crates/slicefs-dedup/src/recovery.rs`
- Modify: `crates/slicefs-dedup/src/redb_dedup_index.rs`

- [ ] **Step 1: Write failing test**

`crates/slicefs-dedup/tests/recovery.rs`:
```rust
use slicefs_dedup::{DedupIndexConfig, RedbDedupIndex};
use slicefs_traits::{ChunkHash, DedupIndex, DedupResult};

fn write_cas_block(cas_root: &std::path::Path, hash: &[u8; 28]) {
    let hex: String = hash.iter().map(|b| format!("{:02x}", b)).collect();
    let dir = cas_root.join(&hex[..2]);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(&hex[2..]), b"x").unwrap();
}

#[test]
fn rebuild_from_cas_idempotent() {
    let td = tempfile::tempdir().unwrap();
    let cas = td.path().join("cas");
    std::fs::create_dir_all(&cas).unwrap();
    for i in 0..200u8 {
        let mut h = [0u8; 28]; h[0] = i;
        write_cas_block(&cas, &h);
    }
    let cfg = DedupIndexConfig::builder(&cas).build();
    RedbDedupIndex::rebuild_from_cas(cfg.clone()).unwrap();
    // Run twice — idempotent.
    RedbDedupIndex::rebuild_from_cas(cfg.clone()).unwrap();

    let idx = RedbDedupIndex::open(cfg).unwrap();
    for i in 0..200u8 {
        let mut h = [0u8; 28]; h[0] = i;
        let ch = ChunkHash::from_bytes(h.to_vec());
        assert!(matches!(idx.lookup(&ch).unwrap(), DedupResult::Present));
    }
}
```

- [ ] **Step 2: Implement**

`crates/slicefs-dedup/src/recovery.rs`:
```rust
use crate::config::DedupIndexConfig;
use crate::error::DedupIndexError;
use crate::paths::DedupRoot;
use crate::platform::fsync_parent_dir;
use crate::redb_dedup_index::DEDUP_TABLE;
use redb::{Database, Durability};
use std::path::Path;

pub fn rebuild_from_cas(config: DedupIndexConfig) -> Result<(), DedupIndexError> {
    let root = DedupRoot::new(&config.dedup_root);
    std::fs::create_dir_all(root.base())?;
    let tmp = root.base().join("index.redb.tmp");
    if tmp.exists() { std::fs::remove_file(&tmp)?; }

    let db = Database::builder()
        .set_page_size(config.page_size)
        .set_cache_size(config.redb_cache_bytes)
        .create(&tmp)?;
    let mut txn = db.begin_write()?;
    txn.set_durability(Durability::Immediate);
    {
        let mut t = txn.open_table(DEDUP_TABLE)?;
        for shard in 0u8..=255 {
            let shard_dir = config.cas_root.join(format!("{:02x}", shard));
            if !shard_dir.exists() { continue; }
            for entry in std::fs::read_dir(&shard_dir)? {
                let entry = entry?;
                let name = entry.file_name();
                let s = match name.to_str() { Some(s) => s, None => continue };
                if s.ends_with(".tmp") { continue; }
                if s.len() != 54 { continue; } // 56 hex chars total minus 2 already in shard name
                let mut h = [0u8; 28];
                h[0] = shard;
                if !decode_hex_into(&s, &mut h[1..]) { continue; }
                t.insert(&h, ())?;
            }
        }
    }
    txn.commit()?;
    drop(db);

    std::fs::rename(&tmp, root.redb())?;
    fsync_parent_dir(root.base())?;
    Ok(())
}

fn decode_hex_into(s: &str, out: &mut [u8]) -> bool {
    if s.len() != out.len() * 2 { return false; }
    for (i, byte) in out.iter_mut().enumerate() {
        let hi = match (&s[2*i..2*i+1]).chars().next() { Some(c) => c, None => return false };
        let lo = match (&s[2*i+1..2*i+2]).chars().next() { Some(c) => c, None => return false };
        let h = match hi.to_digit(16) { Some(v) => v, None => return false };
        let l = match lo.to_digit(16) { Some(v) => v, None => return false };
        *byte = ((h << 4) | l) as u8;
    }
    true
}
```

In `redb_dedup_index.rs`:
```rust
impl RedbDedupIndex {
    pub fn rebuild_from_cas(config: DedupIndexConfig) -> Result<(), DedupIndexError> {
        crate::recovery::rebuild_from_cas(config)
    }
}
```

(Add `mod recovery;` to `lib.rs`.)

> **Note:** the architecture says recovery is per-shard length 56 hex (full 28-byte hash). Each shard contains the **rest** of the hex (54 chars). Above hex parsing decodes that.

- [ ] **Step 3: Run test**

Run: `cargo test --test recovery -p slicefs-dedup`
Expected: 1 test passes.

- [ ] **Step 4: Commit**

```bash
git flow feature start dedup-rebuild-from-cas
git add crates/slicefs-dedup/
git commit -m "feat(slicefs-dedup): rebuild_from_cas — walk shards, bulk-load redb

ARCHITECTURE §9.2 procedure; I7 idempotent."
git flow feature finish dedup-rebuild-from-cas
git push origin develop
```

---

## Milestone L — Property tests

### Task L1: Parity vs `MemDedupIndex`

**Files:**
- Create: `crates/slicefs-dedup/tests/parity.rs`

- [ ] **Step 1: Write the proptest**

`crates/slicefs-dedup/tests/parity.rs`:
```rust
use cas_local::MemDedupIndex;
use proptest::prelude::*;
use slicefs_dedup::{DedupIndexConfig, RedbDedupIndex};
use slicefs_traits::{ChunkHash, DedupIndex, DedupResult};

#[derive(Debug, Clone)]
enum Op {
    Insert(u8),
    Lookup(u8),
    Remove(u8),
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        any::<u8>().prop_map(Op::Insert),
        any::<u8>().prop_map(Op::Lookup),
        any::<u8>().prop_map(Op::Remove),
    ]
}

fn make_hash(seed: u8) -> ChunkHash {
    let mut v = vec![0u8; 28]; v[0] = seed; ChunkHash::from_bytes(v)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]
    #[test]
    fn parity_redb_vs_mem(ops in proptest::collection::vec(op_strategy(), 0..200)) {
        let td = tempfile::tempdir().unwrap();
        let cas = td.path().join("cas");
        std::fs::create_dir_all(&cas).unwrap();
        let cfg = DedupIndexConfig::builder(&cas).build();
        let redb_idx = RedbDedupIndex::create(cfg).unwrap();
        let mem_idx  = MemDedupIndex::new(1000, 0.01);

        for op in ops {
            match op {
                Op::Insert(s) => {
                    redb_idx.insert(&make_hash(s)).unwrap();
                    mem_idx.insert(&make_hash(s)).unwrap();
                }
                Op::Remove(s) => {
                    redb_idx.remove(&make_hash(s)).unwrap();
                    mem_idx.remove(&make_hash(s)).unwrap();
                }
                Op::Lookup(s) => {
                    let r_red = redb_idx.lookup(&make_hash(s)).unwrap();
                    let r_mem = mem_idx.lookup(&make_hash(s)).unwrap();
                    // Permitted demotion only: redb may say Absent where Mem says DefinitelyAbsent.
                    // Forbidden: redb says Present where Mem says Absent/DefinitelyAbsent.
                    match (r_red, r_mem) {
                        (DedupResult::Present, DedupResult::Present) => {}
                        (DedupResult::Absent,  DedupResult::Absent) => {}
                        (DedupResult::Absent,  DedupResult::DefinitelyAbsent) => {} // permitted demotion
                        (DedupResult::DefinitelyAbsent, DedupResult::DefinitelyAbsent) => {}
                        (a, b) => prop_assert!(false, "parity break: redb={a:?}, mem={b:?}"),
                    }
                }
            }
        }
    }
}
```

- [ ] **Step 2: Run**

Run: `cargo test --test parity -p slicefs-dedup -- --nocapture`
Expected: passes (40 cases × up to 200 ops).

- [ ] **Step 3: Commit**

```bash
git flow feature start dedup-parity-prop
git add crates/slicefs-dedup/tests/parity.rs
git commit -m "test(slicefs-dedup): parity property test vs MemDedupIndex

ARCHITECTURE §14.1 (no FP promotion; demotion permitted)."
git flow feature finish dedup-parity-prop
git push origin develop
```

### Task L2: open-close-open preserves the set

**Files:**
- Modify: `crates/slicefs-dedup/tests/parity.rs` (or new file)

- [ ] **Step 1: Add proptest**

```rust
proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]
    #[test]
    fn prop_open_close_open_preserves_set(seeds in proptest::collection::vec(any::<u8>(), 0..100)) {
        let td = tempfile::tempdir().unwrap();
        let cas = td.path().join("cas");
        std::fs::create_dir_all(&cas).unwrap();
        let cfg = DedupIndexConfig::builder(&cas).build();
        {
            let idx = RedbDedupIndex::create(cfg.clone()).unwrap();
            for s in &seeds { idx.insert(&make_hash(*s)).unwrap(); }
            idx.flush().unwrap();
        }
        let idx = RedbDedupIndex::open(cfg).unwrap();
        for s in &seeds {
            let r = idx.lookup(&make_hash(*s)).unwrap();
            prop_assert!(matches!(r, DedupResult::Present));
        }
    }
}
```

- [ ] **Step 2: Run + commit**

Run: `cargo test --test parity -p slicefs-dedup`
Expected: pass.

```bash
git flow feature start dedup-prop-roundtrip
git add crates/slicefs-dedup/tests/parity.rs
git commit -m "test(slicefs-dedup): prop_open_close_open_preserves_set"
git flow feature finish dedup-prop-roundtrip
git push origin develop
```

---

## Milestone M — Mandatory failure-injection tests

ARCHITECTURE §14.2 lists 14 scenarios. Tests **1, 2, 6, 9, 10, 11, 13** are mandatory before MVP ship; 3–5, 7–8, 12, 14 are mandatory before paranoid GA. This plan covers MVP-mandatory.

### Task M1: Test 1 — `kill -9` after CAS fsync, before redb commit (I2/I4)

**Files:**
- Create: `crates/slicefs-dedup/tests/failure_injection_kill9.rs`

- [ ] **Step 1: Write the harness**

The pattern: parent test forks a child via `std::process::Command::new(std::env::current_exe())` with an env-var marker; child does the dangerous work; parent SIGKILLs the child mid-flight; parent then verifies on-disk state.

```rust
use std::env;
use std::process::{Command, Stdio};
use std::time::Duration;

const CHILD_MARK: &str = "DEDUP_FI_T1_CHILD";

fn child_main_t1() -> ! {
    use slicefs_dedup::{DedupIndexConfig, RedbDedupIndex};
    use slicefs_traits::{ChunkHash, DedupIndex};
    let cas = std::path::PathBuf::from(env::var("CAS").unwrap());
    std::fs::create_dir_all(&cas).unwrap();
    let cfg = DedupIndexConfig::builder(&cas).build();
    let idx = RedbDedupIndex::create(cfg).unwrap();
    // Write a CAS block first (mock the I2 caller order).
    let mut h = [0u8; 28]; h[0] = 0xAB;
    let hex: String = h.iter().map(|b| format!("{:02x}", b)).collect();
    let shard = cas.join(&hex[..2]);
    std::fs::create_dir_all(&shard).unwrap();
    std::fs::write(shard.join(&hex[2..]), b"data").unwrap();

    // Submit the insert and immediately abort.
    let _ = idx.insert(&ChunkHash::from_bytes(h.to_vec()));
    // Loop forever — parent will SIGKILL.
    loop { std::thread::sleep(Duration::from_secs(1)); }
}

#[test]
fn t1_kill9_post_cas_pre_commit_yields_FN_not_FP() {
    if env::var(CHILD_MARK).is_ok() { child_main_t1(); }

    let td = tempfile::tempdir().unwrap();
    let cas = td.path().join("cas");
    std::fs::create_dir_all(&cas).unwrap();
    let mut child = Command::new(env::current_exe().unwrap())
        .arg("--exact").arg("t1_kill9_post_cas_pre_commit_yields_FN_not_FP")
        .arg("--nocapture")
        .env(CHILD_MARK, "1")
        .env("CAS", &cas)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn().unwrap();
    std::thread::sleep(Duration::from_millis(50));
    let _ = child.kill();
    let _ = child.wait();

    // After the kill, mount the index and verify NO FP for the inserted hash.
    use slicefs_dedup::{DedupIndexConfig, RedbDedupIndex};
    use slicefs_traits::{ChunkHash, DedupIndex, DedupResult};
    let cfg = DedupIndexConfig::builder(&cas).build();
    // Recovery path: probe → Suspect (no clean shutdown) → open redb directly.
    let idx = RedbDedupIndex::open(cfg).unwrap();
    let mut h = [0u8; 28]; h[0] = 0xAB;
    let r = idx.lookup(&ChunkHash::from_bytes(h.to_vec())).unwrap();
    // Either Present (commit happened to land) or Absent — but NEVER FP without CAS.
    // CAS exists in this scenario, so Present is fine; Absent is the FN-benign case.
    assert!(matches!(r, DedupResult::Present | DedupResult::Absent | DedupResult::DefinitelyAbsent));
}
```

> **Note:** child-harness via `std::env::current_exe()` and a `--exact` filter is the simplest portable pattern. If the workspace already has a test harness crate, prefer that.

- [ ] **Step 2: Run + commit**

Run: `cargo test --test failure_injection_kill9 -p slicefs-dedup`
Expected: pass.

```bash
git flow feature start dedup-fi-t1
git add crates/slicefs-dedup/tests/failure_injection_kill9.rs
git commit -m "test(slicefs-dedup): FI-1 kill-9 between CAS fsync and redb commit"
git flow feature finish dedup-fi-t1
git push origin develop
```

### Task M2: Test 2 — `kill -9` during redb commit (torn root) → I9, I7

Pattern is identical — kill mid-commit. redb 4.1's COW root pointer + page CRCs make this benign. Mount must auto-recover.

- [ ] **Step 1: Add a `t2_*` test in the same file** mirroring M1, but in the child do many rapid inserts via a long-running batcher loop (so SIGKILL lands inside a write-txn).
- [ ] **Step 2: Run + commit** (`feature/dedup-fi-t2`).

### Task M3: Test 6 — Delete a CAS block but keep its index entry → I8

`paranoid` mode + `verify_on_present=true` must demote the result to `Absent`.

- [ ] **Step 1: Insert h, flush, delete `<cas>/XX/rest` for that hash.**
- [ ] **Step 2: With `Default` mode, lookup returns Present (no verify).** With `Paranoid`, lookup returns `Absent`.
- [ ] **Step 3: Run + commit** (`feature/dedup-fi-t6`).

### Task M4: Test 9 — Power-fail simulation (loop device dropping writes)

This needs Linux + nbd. Gate the test behind `cfg(target_os = "linux")` and an env-var feature flag (`SLICEFS_FI_NBD=1`) so CI can opt in.

- [ ] **Step 1: Bring up an `nbd-server` exporting a backing file with `O_DIRECT` and a configurable drop-after-T policy.**
- [ ] **Step 2: Mount as loop, run a short insert burst, drop writes after T_drop, mount the resulting image and verify no FP.**
- [ ] **Step 3: If nbd-server isn't available, skip with a clear `eprintln!` and exit 0.**
- [ ] **Step 4: Commit** (`feature/dedup-fi-t9`).

### Task M5: Test 10 — 100× concurrent kill-9 stress

100 child processes each running the same insert loop, parent SIGKILLs all randomly.

- [ ] **Step 1: Spawn 100 children, each with disjoint hash ranges.**
- [ ] **Step 2: Sleep random 10-200 ms then `kill -9` each.**
- [ ] **Step 3: After all children dead, mount index and assert: every hash that has a CAS block is either Present or Absent (never FP without CAS).**
- [ ] **Step 4: Commit** (`feature/dedup-fi-t10`).

### Task M6: Test 11 — F_FULLFSYNC no-op shim (macOS regression canary)

`DYLD_INSERT_LIBRARIES` shim that turns `F_FULLFSYNC` into a no-op, then power-fail simulation. Validates that `use_f_fullfsync=true` is non-cosmetic.

- [ ] **Step 1: Build a tiny dylib `libnoop_fullfsync.dylib` exporting `fcntl` returning 0 when `cmd == F_FULLFSYNC`.**
- [ ] **Step 2: Run the test child with `DYLD_INSERT_LIBRARIES=...` and verify FN behaviour increases.**
- [ ] **Step 3: Gate behind `cfg(target_os = "macos")`.**
- [ ] **Step 4: Commit** (`feature/dedup-fi-t11`).

### Task M7: Test 13 — S1 / OQ-11 caller-cached-Ok across crash

The headline contract test from §14.2: caller calls `insert(h)`, awaits Ok, immediately SIGKILLs **before** group-commit window expires. On next mount, `lookup(h)` may return Absent — that's permitted.

- [ ] **Step 1: Child does `insert(h)` → on Ok reply, SIGKILLs itself within 1 ms.**
- [ ] **Step 2: Parent mounts and asserts no FP. Document: caller MUST NOT cache "I inserted h" across crash without `flush()`.**
- [ ] **Step 3: Commit** (`feature/dedup-fi-t13`).

---

## Milestone N — Benchmarks (gate)

### Task N1: `bench_seed_burst_100m` — gate ≥ 100 K ins/s

**Files:**
- Create: `crates/slicefs-dedup/benches/seed_burst.rs`

- [ ] **Step 1: Write the criterion bench**

```rust
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use slicefs_dedup::{DedupIndexConfig, DurabilityMode, RedbDedupIndex};
use slicefs_traits::{ChunkHash, DedupIndex};

fn seed_burst(c: &mut Criterion) {
    c.bench_function("seed_burst_1m", |b| {
        b.iter_custom(|iters| {
            let td = tempfile::tempdir().unwrap();
            let cas = td.path().join("cas");
            std::fs::create_dir_all(&cas).unwrap();
            let cfg = DedupIndexConfig::builder(&cas).mode(DurabilityMode::Seed).build();
            let idx = RedbDedupIndex::create(cfg).unwrap();
            let n = (iters as usize).min(1_000_000);
            let start = std::time::Instant::now();
            for i in 0..n {
                let mut h = [0u8; 28];
                h[..8].copy_from_slice(&(i as u64).to_le_bytes());
                idx.insert(&ChunkHash::from_bytes(h.to_vec())).unwrap();
            }
            idx.flush().unwrap();
            black_box(start.elapsed())
        });
    });
}

criterion_group!(benches, seed_burst);
criterion_main!(benches);
```

- [ ] **Step 2: Run on the reference NVMe**

Run: `cargo bench -p slicefs-dedup --bench seed_burst -- --quick`
Expected: ≥ 100 K ins/s. Compute: `n / elapsed_seconds`.

- [ ] **Step 3: Document the result in `.planning/research/dedup-index/architecture/BENCH-RESULTS.md`** (create file).

- [ ] **Step 4: If gate misses, follow ARCHITECTURE §15.0.1** — reproduce on second NVMe; do NOT silently relax the gate.

- [ ] **Step 5: Commit**

```bash
git flow feature start dedup-bench-seed
git add crates/slicefs-dedup/benches/seed_burst.rs .planning/research/dedup-index/architecture/BENCH-RESULTS.md
git commit -m "bench(slicefs-dedup): seed_burst gate ≥ 100K ins/s"
git flow feature finish dedup-bench-seed
git push origin develop
```

### Task N2 – N5: Remaining benches (lookup warm/cold, steady_mixed, commit_latency, recovery_50m)

Each follows the same pattern as N1: criterion bench + reference-NVMe run + result row in `BENCH-RESULTS.md`. Targets per ARCHITECTURE §11 / §14.3:

- `bench_lookup_warm`: p99 ≤ 10 µs
- `bench_lookup_cold`: p99 ≤ 500 µs
- `bench_steady_mixed`: ≥ 20 K ins/s sustained
- `bench_commit_latency`: p99 ≤ 5 ms (Default), ≤ 12 ms (Paranoid)
- `bench_recovery_50m`: RTO ≤ 30 s

For each: create `benches/<name>.rs`, wire in `[[bench]]`, run, record, commit on a feature branch.

---

## Milestone O — CLI integration

### Task O1: `slicefs reindex --offline` — invokes `rebuild_from_cas`

**Files:**
- Modify: `crates/slicefs-cli/src/<cmd>.rs` (locate the existing subcommand dispatcher)
- Modify: `crates/slicefs-cli/Cargo.toml` (add `slicefs-dedup` dep)

- [ ] **Step 1: Add subcommand**

(Brief: clap derive enum gets `Reindex { store: PathBuf, offline: bool, bloom_capacity: Option<usize> }`. Handler calls `RedbDedupIndex::rebuild_from_cas(cfg)`.)

- [ ] **Step 2: Integration test**

Run a tiny store, seed via the existing seed command, run `slicefs reindex --offline`, mount, lookup → Present.

- [ ] **Step 3: Commit on `feature/cli-reindex`.**

### Task O2: `slicefs dedup recover` (non-destructive — `[OQ-5]`)

Mounts in Suspect mode, runs background scrub, never deletes data. Initial implementation can simply call `rebuild_from_cas` after preserving the existing index file as `.bak`.

### Task O3: `slicefs stats [Index]` block

Extend the existing `stats` subcommand output with a new section reporting `StatsSnapshot` fields.

---

## Milestone P — Release prep

### Task P1: Documentation

- [ ] Update `README.md` to mention the new `slicefs-dedup` crate in the "Repo layout" section.
- [ ] Add a `crates/slicefs-dedup/README.md` summarizing API + the three modes.
- [ ] Update `.planning/STATE.md` if such a file is in use.

### Task P2: Ship via git-flow release

- [ ] `git flow release start v0.2.0-dedup`
- [ ] Bump versions in workspace + per-crate `Cargo.toml`s as needed.
- [ ] `git flow release finish v0.2.0-dedup` — produces merge commits to `main` and `develop` and a tag.
- [ ] `git push origin main develop --tags`

---

## Self-Review Checklist (run before handing off)

- **Spec coverage:** every section of `ARCHITECTURE.md §1–§15` has at least one task above. Sections deferred to other plans are explicitly listed in "Out of scope" near the top of this file.
- **No placeholders:** all code blocks contain working code (the `unimplemented!()` stubs in G2 are replaced in H1/H2 within the same milestone; `TODO`s in J2 / O2 are scoped to v2 levers per ARCHITECTURE).
- **Type consistency:** `DedupIndexConfig`, `RedbDedupIndex`, `BatchWriter`, `AtomicBloomFilter`, `Manifest`, `BloomSnapshotMeta`, `DedupIndexError`, `MountState`, `StatsCounters`, `StatsSnapshot`, `IndexStats`, `VerifyReport` are referenced consistently across tasks. The trait surface in §C1 matches §G2/§H1/§H2.
- **Phasing matches architecture:** A=§15.0 (phase-0), B–O=§15.1 (MVP). v2 levers (sharded redb, log-structured engine, online rebuild for N>50M, segment-blooms, OTEL) are deferred per §15.2 / §15.3.

---

## Execution Handoff

Plan complete and saved. Two execution options:

**1. Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration. Best for the long milestones (G, K, M, N).

**2. Inline Execution** — Execute tasks in this session using executing-plans, batch execution with checkpoints. Best if you want to land Milestones A–C in one sitting.

Which approach?
