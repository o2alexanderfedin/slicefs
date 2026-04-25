# DedupIndex — Synthesis & Decision Document

**Status:** DECISION — supersedes the four input research reports. **Date:** 2026-04-23.
**Inputs:** [01:storage-engines], [02:ssd-friendliness], [03:crash-safety], [04:prior-art].

## 1. Problem statement

SliceFS is a content-addressed deduplicating POSIX filesystem (FUSE-T on macOS, libfuse on Linux) targeting single-host SSD/NVMe with billions of distinct chunks over a multi-year lifetime. The `DedupIndex` answers "have I durably stored this 28-byte ChunkHash?" — gating whether the chunker writes a new CAS block or short-circuits as a duplicate. The current implementation is in-memory only (`MemDedupIndex`), which is the same RAM-bound failure mode that broke legacy ZFS DDT [04:§1]. We need a persistent, bounded-RAM, crash-safe authoritative store that sits behind the existing fastbloom front-end without ever causing a false positive (FP). The on-disk index is, by design, a **derivable cache of the CAS store directory** — never the source of truth — and must be cheap to rebuild, fast to query, and friendly to consumer-grade NVMe endurance budgets.

## 2. Requirements

### Functional (from `dedup_index.rs` + project context)
- **F1.** `bloom_check` false ⇒ definitely absent (no FN at bloom layer).
- **F2.** `lookup` is authoritative: `Present` ⇒ CAS block durable.
- **F3.** `insert` is durable on return (or satisfies §3 ordering such that no FP can result from a crash).
- **F4.** `remove` is GC-only; never updates bloom (FP after remove is benign).
- **F5.** `&self` interior mutability.
- **F6.** SET semantics — no values, no versioning, no MVCC reader snapshots.

### Non-functional
- **N1. Scale:** 0 to 10⁹ entries, single host.
- **N2. RAM:** bloom O(N) acceptable (~1.2 GB at 1B keys / 1% FPR); authoritative tier O(N) RAM is **not** — that's the ZFS DDT antipattern [04:§1].
- **N3. Latency:** cold `lookup` ≤ 500 µs at 1B keys; warm ≤ 10 µs; insert ≤ 1 ms p99 steady-state.
- **N4. Endurance:** total WAF inside 1 DWPD consumer budget at "medium steady" (~100 M ins/day) [02:§8]. Must not require enterprise NVMe.
- **N5. Recovery:** mount-time rebuild from CAS up to ~50 M chunks (~30 s) [03:§3]; online for larger.
- **N6. Maintainability:** small team. Prefer 1 line of `Cargo.toml` over 5,000 lines of unsafe storage code.
- **N7. macOS:** `F_FULLFSYNC` mandatory on Darwin; plain `fsync` is not durable [03:§1].
- **N8.** Coexists with CAS BlockStore; BlockStore directory listing is ground truth.

## 3. Invariants

### I1 — The Asymmetry (formal)
```
Let CAS = { h : block(h) is durable on disk }
Let IDX = the set the DedupIndex answers Present for

INVARIANT:  IDX ⊆ CAS    (always, including across crashes)
```
A Present answer for h ∉ CAS causes the chunker to skip the write → **catastrophic data loss**. An Absent answer for h ∈ CAS causes a redundant idempotent write → **benign**. Every design choice must respect this asymmetry [03:§0].

### I2 — Ordering rule (Insert)
```
cas_block.fdatasync ▸ cas_dir.fdatasync ▸ index.insert(h) ▸ index.fdatasync ▸ bloom.insert(h) ▸ HWM++
```
Visibility (the "high-water mark" past which `lookup` may answer Present) advances **only after** the index record is durable [02:§5][03:§2].

### I3 — Ordering rule (Remove)
```
index.remove(h) ▸ index.fdatasync ▸ cas_block.unlink
```
Reverse direction — we never want a Present answer pointing to a unlink'd CAS block [03:§Formal-Spec].

### I4 — CAS-as-truth
The CAS directory is the canonical set. Index can always be rebuilt from `walk(cas/)`. Therefore the index is permitted to lose recent inserts on crash — those reduce to FN, which is benign — provided I1 holds [03:§3].

### I5 — Verify-on-collision (defense in depth)
On `Present`, optionally `stat(2)` the CAS path before returning. This converts any residual I1 violation into a self-healing FN. Cheap on warm cache; configurable for paranoid mode [03:§2].

## 4. Tradeoff matrix

Consolidated from the four reports. Scale 1 (poor) – 5 (excellent). Higher = better for SliceFS workload.

| Design | Engine WAF [02] | Cold lookup p99 [01] | Crash-safe [03] | RAM/key [01] | Build cost [01] | Endurance @ heavy [02:§8] | Maintainability [01,03] | 1B+ proven [01,04] | **Total** |
|---|:---:|:---:|:---:|:---:|:---:|:---:|:---:|:---:|:---:|
| **redb 4.1 (COW B+tree)** | 3 | 4 | 5 | 5 | 5 | 2 (~3.5 PB / 5 yr) | 5 | 3 | **32** |
| **Append-only log + bloom + manifest** | 5 | 4 | 5 | 5 | 1 | 5 (~263 TB / 5 yr) | 1 | 4 (FDT-Log shape) | **30** |
| **Hybrid: redb now, log v2** | 3 → 5 | 4 | 5 | 5 | 5 | 2 → 5 | 4 | 3 | **31** |
| RocksDB | 2 | 5 | 5 | 4 | 3 (FFI) | 1 | 3 | 5 | 28 |
| LMDB / heed3 | 4 | 5 | 5 | 4 | 3 (FFI) | 3 | 4 | 5 | 33* |
| fjall 3.1 | 3 | 4 | 4 | 4 | 4 | 2 | 3 | 2 | 26 |
| sled | 3 | 3 | 2 | 3 | 4 | 2 | 2 | 1 | 20 |
| Bitcask | 5 | 5 | 4 | **1** | 2 | 5 | 2 | 1 | DQ (RAM) |
| Cuckoo on-disk | 5 | 5 | 1 | 4 | 1 | 5 | 1 | 1 | DQ (FP risk) |
| FS-as-index (casync) | 5 | 1 | 5 | 5 | 5 | n/a | 5 | 1 | DQ (lookup latency) |

\* LMDB's nominal score win is offset by N6/N7: C dep, pre-sized mapfile, heed3 page-checksums still beta, no workspace dep.

Headline conflict: **redb** wins on Build cost + Maintainability; **append-only log** wins on Engine WAF + Endurance. Bitcask, Cuckoo, FS-as-index, and probabilistic structures are DQ'd by §3 invariants or N2.

## 5. Recommended design

**Pick: redb 4.1 as the authoritative on-disk store + fastbloom front + CAS-as-truth recovery, with the log-structured layout filed as a v2 lever gated on telemetry.**

This is a *hybrid in time* — start at row 1 of the matrix, migrate to row 2 only if measured endurance pressure justifies it.

### Why this resolves the redb-vs-log disagreement

[02:§7]'s 13× endurance gap (263 TB vs 3.5 PB / 5 yr at heavy steady-state) is real, not academic. But [01:§Top-3] and [03:§Recommendation] both pick redb on build cost + COW torn-write resistance, and **CAS-as-truth recovery [03:§3] neutralizes the durability concern** that would otherwise force a custom engine. [04:§1] notes OpenZFS Fast Dedup *kept its authoritative tier as a B-tree (ZAP)* and added the FDT-Log only as a batching front-end — exactly the hybrid shape we adopt.

Decisive points:

1. **Heavy steady-state is not our workload.** The 13× gap requires ~1B inserts/day sustained. After seed, the bloom front eats >99% of `lookup` [04:§9] — only novel chunks reach `insert`. For loads <100 M inserts/day, redb is at 17.5 TB / 5 yr [02:§8], comfortably inside 1 DWPD consumer.
2. **redb gives torn-write safety for free.** COW shadow paging keeps the previous root valid until a single 4 KiB superblock atomic write installs the new one [03:§6]. NVMe AWUPF ≥ 4 KiB is mandatory [02:§6]. Log-structured matches it via checksummed-tail truncation but only with code we'd write.
3. **CAS-as-truth makes WAF mostly moot for correctness.** [03:§3] is load-bearing: rebuilds in O(N) from `walk(cas/)` mean we can run redb at `Durability::None` on the hot path. Recent inserts lost on crash → FN → benign. Per-insert fsync cost — the real killer — collapses without changing engines.
4. **Build cost is asymmetric.** redb = one `Cargo.toml` line, already depended on. A custom log engine (manifest, compaction, segment recovery, AWUPF probing, HWM) is 6 months with subtle crash bugs [01:§4] — hours not spent on streaming writes (the actual v2.0 goal).
5. **Telemetry is the gate.** Instrument device-write rate; >5% DWPD/day on the index alone triggers v2 = log-structured. Until then, premature.
6. **Maintainability over peak perf.** cberner maintains redb (bus factor [01:§2a], mitigated by pure-Rust auditability and fork-ability).
7. **Failure-mode coverage is identical** for I1–I5. Endurance-WAF is the only divergence, deferred not ignored.

If telemetry later shows pressure, the [02:§7] design is queued. Migration = offline rebuild from CAS, the primitive we already need.

## 6. Implementation sketch

### 6.1 Module layout

New module **inside `cas-local`** (don't bloat the workspace with a new crate before we know we need separation):

```
crates/cas-local/src/
├── mem_dedup_index.rs         (existing, kept for tests)
├── persistent_dedup_index.rs  (NEW — redb-backed impl)
├── bloom_persistence.rs       (NEW — fastbloom snapshot + xxh3 header)
└── recovery.rs                (NEW — walk(cas/) → bulk-load index + bloom)
```

Trait impl shape:

```rust
pub struct PersistentDedupIndex {
    bloom: AtomicBloomFilter,                 // existing fastbloom
    db: redb::Database,                       // COW B+tree
    table_def: redb::TableDefinition<'static, [u8; 28], ()>,
    high_water: AtomicU64,                    // visibility barrier (I2)
    cas_root: PathBuf,                        // for verify-on-collision (I5)
    config: PersistentDedupIndexConfig,
}

pub struct PersistentDedupIndexConfig {
    pub bloom_capacity: usize,
    pub bloom_fpr: f64,
    pub durability: Durability,               // None = rebuild-on-crash; Eventual = group-commit
    pub verify_on_present: bool,              // I5 belt-and-braces
    pub bloom_snapshot_every: usize,          // N inserts between bloom snapshots
    pub use_f_fullfsync: bool,                // macOS only; default true
}
```

### 6.2 Trait impl outline

- `bloom_check` → `self.bloom.contains(h.as_bytes())`. Unchanged.
- `lookup` → bloom miss ⇒ `DefinitelyAbsent`. Bloom hit ⇒ redb read txn `table.get(h)`; Some ⇒ optional `stat(cas_path(h))` if `verify_on_present` ⇒ `Present`; None ⇒ `Absent`.
- `insert`: caller MUST have already fsync'd the CAS block (I2; enforced by layer-above contract). Then open redb write txn, `table.insert(h, ())`, commit (`Eventual` group-commit by default, or `None` in seed mode). After commit returns: insert bloom, bump HWM. Every N inserts, fork a background bloom snapshot with xxh3 header.
- `remove` → redb txn delete, do NOT touch bloom (I3 ordering enforced by GC layer).

Error handling: `CasError::Index(String)` for redb, `CasError::Io` for FS-level. All redb panics caught and converted.

### 6.3 Sync model

Three configurable modes:

| Mode | redb durability | macOS sync | Use case |
|---|---|---|---|
| `paranoid` | `Immediate` per-insert | F_FULLFSYNC | Compliance / unattended |
| `default` | `Eventual` + 200 ms group-commit | F_FULLFSYNC at boundary | Daily driver |
| `seed` | `None` (rebuild from CAS on crash) | n/a | One-shot bulk load |

Default = `Eventual`. CAS-as-truth ensures correctness even at `None`.

### 6.4 Startup / recovery flow

1. Open `cas/`; fsync directory on first mount.
2. Open `cas/.dedup-index/`; create if absent.
3. Open `index.redb` — redb validates latest committed root via page CRCs. On corruption: mark stale, goto 6.
4. Open `bloom.snap` — verify xxh3 header. On mismatch: mark stale, goto 6.
5. Both valid: mount RW; HWM = redb.last_committed_id; spawn scrubber. DONE.
6. RECOVERY (rebuild from CAS): walk `cas/` with `getdents64` → bounded channel → bulk-load into fresh redb table (one txn per 100k keys) + build bloom in same pass → fsync, atomic-rename into place, fsync parent dir → mount RW.
7. Scrubber: once / 24 h, walk index pages and verify CRC32C; in `paranoid` mount, also periodic CAS sample-rehash.

### 6.5 Bloom filter sizing strategy

Open question [04:§Open-question] — sizing a 0→10⁹ bloom — gets a concrete answer:

- **Static sizing at mount.** `bloom_capacity = max(current_chunk_count × 4, 1M)`. Fastbloom at 1B / 1% FPR ≈ 1.2 GB. Accepted as the price of bounded-memory dedup at scale.
- **Rebuild-on-crossing.** If actual entries exceed 90% of capacity, mark stale; emit recommendation; manual `slicefs reindex --bloom-capacity 2x` rebuilds (no online resize — too complex for v2.0).
- **v2 lever:** per-segment blooms [01:§1a, 04:§10] when we move to log-structured.
- **Snapshot policy:** persist every 100k inserts and on clean shutdown. Rebuild from redb is ~500 ms / 10 M entries [03:§7].

### 6.6 Concurrency model

- redb is single-writer MVCC: many readers, one writer — matches our `&self` trait.
- Bloom (`AtomicBloomFilter`) and HWM (`AtomicU64`) are lock-free.
- **Batching writer thread (default in `seed` mode):** MPSC → one drainer thread → one redb txn per 10k–100k inserts → oneshot reply. Caller awaits to satisfy I2. Mitigates the [01:§2a] single-writer ceiling without breaking durability.

### 6.7 File layout on disk

```
cas/
├── 00/  01/  ...  ff/             ← existing CAS shards (sharded by 1st byte of hash)
└── .dedup-index/
    ├── index.redb                  ← authoritative on-disk index (redb COW B+tree)
    ├── index.redb.lock             ← redb's own lock file
    ├── bloom.snap                  ← fastbloom serialized (with xxh3 header)
    ├── bloom.snap.tmp              ← atomic-rename staging file
    └── manifest                    ← JSON: {version, bloom_capacity, bloom_fpr, hwm, last_clean_shutdown}
```

`manifest` is rewritten with `O_DSYNC` after every clean shutdown and on `bloom_capacity` change. Header format for `bloom.snap`:

```
[ 8B magic "SLDXBL01" ][ 8B xxh3 of payload ][ 8B u64 expected_items ]
[ 8B f64 fpr ][ 8B u64 entries_at_snapshot ][ payload : fastbloom serde ]
```

## 7. Open questions

1. **Bloom sizing under multi-year growth without segments** [04:§Open-question]. Static sizing forces a manual `slicefs reindex` to grow capacity. **Action:** prototype additive segment-blooms in v2.1; document the manual ladder until then.
2. **`Durability::None` + caller-redrive contract.** Hot-path `Durability::None` loses recent inserts on crash; CAS-as-truth reduces this to FN. **Question:** do layers above DedupIndex (chunker, FUSE write path) cache "I just inserted h" state that would itself be corrupted? **Action:** audit `cas-local::insert_block` + FUSE write handlers; deliver a `kill -9` crash test between CAS write and index commit.
3. **F_FULLFSYNC cost on Apple Silicon.** [03:§1] cites 1–4 ms on consumer NVMe; M-series behavior is poorly documented. **Action:** benchmark `fdatasync` vs `F_FULLFSYNC` per-insert on M2; tune macOS `commit_delay` from the result.
4. **redb single-writer ceiling on seed burst** [01:§2a]. Mitigation (batching writer thread) is proposed but unmeasured. **Action:** 100M-hash seed benchmark in `seed` mode on NVMe; require ≥100 K inserts/s or escalate to log-structured now.
5. **Verify-on-present cost (I5) in warm-cache p99.** `stat(2)` is 1–5 µs warm but 100 µs+ cold. **Action:** measure under contended page cache; default `verify_on_present=false` in `default`, `true` only in `paranoid` if measurement confirms cost.

## 8. Phasing

### MVP (this synthesis → first ship)
- `PersistentDedupIndex` over redb 4.1, default `Durability::Eventual`, CAS-as-truth recovery, static bloom sizing.
- Three sync modes: `seed` / `default` / `paranoid`.
- macOS `F_FULLFSYNC` on. Linux `fdatasync`.
- Recovery walks `cas/` and rebuilds redb + bloom — single-threaded streaming ingest.
- Snapshot bloom every 100k inserts and on clean shutdown.
- Trait API unchanged; `MemDedupIndex` retained as a test/dev impl.
- Property tests (parity with `mem_dedup_index.rs`) + crash-injection tests using `kill -9`.
- Telemetry: `slicefs stats` reports device-write rate per day; `slicefs reindex` exists.

### v2 (gated on telemetry / feedback)
- **Endurance metric exceeds threshold:** swap redb for [02:§7] segmented log; same trait, new module. Migration = offline rebuild. Per-segment blooms.
- **Seed-bench misses ≥100 K ins/s:** add batching writer thread, or escalate to log-structured early.
- **Bloom capacity ceiling hit:** segment-bloom incremental growth; auto-reindex on shutdown above 90%.
- **Online rebuild** for stores >50 M chunks.
- **Background CAS scrubber** with sample-rehash.
- **Per-shard concurrency** — split redb by hash prefix to bypass single-writer ceiling.

### Out of scope
- Distributed/multi-host replication (per `PROJECT.md` v2.0).
- Block compression (removed in v2.0 streaming milestone).
- Crypto integrity beyond CRC32C — physical-disk attackers win earlier.
- Cuckoo / learned-index replacement of bloom front — fastbloom is fine.
- F_BARRIERFSYNC investigation — F_FULLFSYNC is correct.
- Runtime AWUPF probing — hardcode 4 KiB minimum [02:§6, 03:§6].

---

*All citations [0N:§M] reference the four input research reports in `.planning/research/dedup-index/`.*
