# DedupIndex Storage Engine Survey

**Workload:** SET-only (28-byte BLAKE3-224 keys, no values), insert-heavy initial seed (1B+ entries), mixed read/insert steady state, occasional remove during GC. SSD/NVMe single-host. The bloom filter front already exists; this layer is the **authoritative** on-disk store.

**Asymmetric correctness invariant:**
- False **negative** (forgetting an inserted key) → re-write of one block. Benign.
- False **positive** (claiming a never-inserted key is present) → CAS skips writing → **DATA LOSS**.
- Therefore: writes **must be durable before `insert()` returns** (or callers must be re-driven through a re-insertion log on recovery). No eventually-consistent flush windows that "remember" a key that was never persisted.

The space of viable engines is shaped by this asymmetry. An engine that loses *committed* data on crash is disqualified; an engine that loses *un-fsynced* data on crash is fine if we fsync per insert (or per batch with caller re-drive).

---

## 1. LSM-tree KV stores

### 1a. RocksDB (`rocksdb` 0.24 / `rust-rocksdb` 0.46.0, Feb 2026)

- **Cold lookup:** ~1 SSD read per level after bloom filtering; with `bits_per_key=10` (default ~1% FPR) typical NVMe latency ~50–150 µs cold, <5 µs in block-cache.
- **Insert throughput:** Write path is memtable + WAL fsync; sustained 100k–500k ops/s on NVMe with group commit. Bulk-load mode (SST ingest) bypasses WAL and reaches millions/s.
- **Crash recovery:** WAL replay; torn writes detected via per-record checksums. With `WAL=on, sync=true` no committed insert is ever lost. CRC32C on every block.
- **RAM/key:** ~10 bits for bloom + memtable headroom (typically 64–512 MB). At 1B keys: ~1.25 GB bloom + caches.
- **1B+ scale:** Proven (Meta uses RocksDB at this scale daily). Compaction overhead is the concern, not capacity.
- **Top concerns:**
  1. **C++ FFI**: ~30 MB linked binary, slower compile, harder to reason about than pure-Rust crates. Bindings lag upstream.
  2. **Write amplification**: leveled compaction is typically 10–30× — for SET-only with 28-byte keys, this means ~280–840 bytes written to SSD per logical 28-byte insert. SSD wear concern over years.

### 1b. fjall 3.1 (Mar 2026, pure Rust)

- **Cold lookup:** Comparable to RocksDB; partitioned bloom per segment, ~1 SSD seek + bloom check.
- **Insert throughput:** v3 announced major improvements; bulk-load benchmarks (100M u128 keys) competitive with RocksDB and ahead of redb in writes.
- **Crash recovery:** WAL with checksums; v3 introduced a longevity-stable disk format.
- **RAM/key:** Similar to RocksDB; partitioned indexes keep block-cache pressure modest.
- **1B+ scale:** Plausible but unproven at this scale publicly. **Speculation:** the design is sound but production deployments at 1B+ are not visible.
- **Top concerns:**
  1. **Maturity / deployment evidence**: small ecosystem vs RocksDB; fewer bug-years.
  2. **Crash-safety pedigree**: well-documented WAL but limited adversarial testing reports compared to RocksDB and LMDB.

### 1c. sled (0.34.7, last release 2021; "champagne of beta")

- **Status:** Still beta in 2026. README explicitly warns format will break before 1.0. Active rewrite ("marble") underway but unreleased.
- **Concerns:** Disqualified for production DedupIndex. History of crash-safety bugs and format churn. Useful only as a reference.

---

## 2. B+tree / COW stores

### 2a. redb 4.1 (Apr 2026, pure Rust, already in workspace)

- **Cold lookup:** B-tree descent ~3–4 levels for 1B keys; ~3–4 SSD reads worst case (~200–400 µs cold), <1 µs warm.
- **Insert throughput:** Single-writer MVCC. v4.1 reports 1.5× improvement on writes via dynamic cache partitioning. Individual-write benchmarks: ~227 ms vs LMDB 388 ms vs RocksDB 701 ms (lower is better, redb's repo benchmark). Batch writes: redb 2,346 ms vs LMDB 2,136 ms vs RocksDB 992 ms — RocksDB wins on batches, redb wins on individual commits.
- **Crash recovery:** COW + two-phase commit; no WAL needed — every commit either fully lands or is invisible. Torn-write safe by construction.
- **RAM/key:** Page cache only; no per-key in-memory index. Trivially scales beyond RAM.
- **1B+ scale:** Architecturally fine; B-tree depth grows logarithmically. Single-writer is the bottleneck on insert-heavy seed.
- **Top concerns:**
  1. **Single-writer serialization** — initial seed of millions/billions becomes commit-latency-bound unless batched aggressively.
  2. **Write amplification** from COW page rewrites (~2–5× typical) is lower than LSM but every insert dirties an internal page.

### 2b. LMDB / heed 0.22 (heed3 in beta with checksumming)

- **Cold lookup:** Memory-mapped B+tree; with mmap + warm OS cache often <10 µs. Cold ~1 SSD read.
- **Insert throughput:** Single-writer like redb; very fast for small commits, but write throughput collapses if commits are tiny and synchronous.
- **Crash recovery:** Robust — battle-tested for 15+ years, MDB_NOSYNC/MDB_NOMETASYNC are explicitly opt-in. **Default heed (LMDB 0.9.x) has no per-page checksums** (heed3 adds them).
- **RAM/key:** mmap-driven; address-space-bound on 32-bit, fine on 64-bit.
- **1B+ scale:** Proven (Meilisearch, OpenLDAP). DB size capped at map size — must pre-allocate.
- **Top concerns:**
  1. **C dependency** — same FFI tax as RocksDB (smaller).
  2. **Pre-sized map file** — must guess upper bound; resize requires reopen.

### 2c. sanakirja 1.x

- **Cold lookup:** B-tree, comparable to LMDB/redb.
- **Inserts:** Author claims 20–50% faster than LMDB on graph workloads (2021 data).
- **Crash recovery:** COW like redb, with O(log n) page-refcounted clones.
- **1B+ scale:** **Unproven at this scale** publicly. Small user base outside Pijul.
- **Top concerns:**
  1. **Ecosystem maturity** — ~1 production user (Pijul); little independent crash testing.
  2. **Stale public benchmarks** (2021).

---

## 3. Bitcask-style (append log + in-RAM hash index)

Crates: `bitask`, `cask`, `assemblage_kv`, `rustcask` — all hobby/learning projects, none production-grade.

- **Lookup:** O(1) — single hash probe + 1 SSD read.
- **Insert:** Append-only — fastest write throughput possible (sequential SSD writes ~1 GB/s).
- **Crash recovery:** Append log replay; commit boundary = fsync. Torn last record is detected and discarded.
- **RAM/key:** **Disqualifying.** Bitcask requires the *entire* key directory in RAM — for 1B × (28B key + 16B pointer) ≈ **44 GB RAM**. This is precisely the ZFS DDT failure mode SliceFS exists to avoid.
- **Top concerns:**
  1. RAM footprint scales linearly with key count — same trap as ZFS DDT.
  2. No production-grade Rust crate.

---

## 4. On-disk hash indices (Cuckoo / Robin Hood)

- `axiomhq/rust-cuckoofilter`, `probabilistic-collections::CuckooFilter`, `rust-cuckoomap`.
- Cuckoo filters are *probabilistic* — they have a tunable false-positive rate. **This makes them disqualified as the authoritative layer** because the failure mode for FP is data loss. They are usable only as a *replacement for the bloom front-end* (which we already have).
- Persistent open-addressing hash tables (e.g., custom Robin Hood on mmap) exist but no production Rust crate. Building one is a 6+ month project with subtle crash-recovery bugs (rehash mid-crash → torn slots).
- **Top concerns:**
  1. Probabilistic structures cannot be authoritative given our asymmetry.
  2. No production Rust deterministic on-disk hash table.

---

## 5. Specialized "set" structures

- **Roaring bitmaps** (`roaring` crate): excellent for *integer* sets, but our keys are 224-bit hashes uniformly distributed over a 2^224 universe. Bitmaps are useless here — no compression possible on uniform random data.
- **Succinct / FM-index / learned indices**: research-grade for static sets. Insert-heavy workloads (mutable, billions) are not the target.
- **Top concerns:** Wrong workload model — none of these handle insert-heavy uniform-random keys.

---

## Scored tradeoff matrix

Scale: 1 (poor) – 5 (excellent). Higher = better for our workload.

| Engine        | Write-amp | Read-lat | Crash-safe | Memory | Rust ecosystem | **Total** |
|---------------|:---------:|:--------:|:----------:|:------:|:--------------:|:---------:|
| RocksDB       | 2         | 5        | 5          | 4      | 4 (FFI)        | **20**    |
| fjall 3.1     | 3         | 4        | 4          | 4      | 3              | **18**    |
| sled          | 3         | 3        | 2          | 3      | 2              | **13**    |
| redb 4.1      | 4         | 4        | 5          | 5      | 5              | **23**    |
| LMDB/heed     | 4         | 5        | 5          | 4      | 3 (FFI)        | **21**    |
| sanakirja     | 4         | 4        | 4          | 5      | 2              | **19**    |
| Bitcask       | 5         | 5        | 4          | 1      | 1              | **16**    |
| Cuckoo on-disk| 5         | 5        | 1          | 4      | 1              | **16**    |
| Roaring/learned| n/a      | n/a      | n/a        | n/a    | n/a            | **DQ**    |

Crash-safety scoring weights: data-loss-on-crash → 1; un-fsynced loss only → 4–5.
Bitcask "memory=1" reflects 44 GB RAM at 1B keys, which violates the design goal.
Cuckoo-on-disk scored 1 on crash-safe because no shipped Rust impl handles atomic rehash.

---

## Top-3 ranked recommendation

### 1. **redb 4.1** (primary recommendation)

- Already a workspace dependency — zero new crates, no FFI, pure Rust.
- COW + two-phase commit eliminates WAL torn-write categories entirely.
- v4.1's 1.5× write speedup (Apr 2026) and dynamic cache partitioning materially help our workload.
- Page cache only — no per-key RAM cost; lives within OS page cache budget.
- **Mitigation for single-writer bottleneck on 1B-row seed**: batch inserts in 10k–100k key transactions during seed; the bloom front-end means the steady-state insert rate after seed is much lower (only novel chunks hit `insert`).
- **Risk:** redb 4.x is young (current as of Apr 2026); cberner's solo maintainership is a bus-factor concern. Mitigated by pure-Rust auditability.

### 2. **RocksDB (`rocksdb` 0.24 / rust-rocksdb 0.46)**

- The only engine with public 1B+-key production track record and well-understood failure modes.
- Choose if seed-rate testing shows redb's single-writer commit can't saturate SSD write bandwidth.
- Tune: `disable_wal=false`, `bits_per_key=10` bloom, `BlockBasedTableOptions` with `whole_key_filtering`, no value column (use empty value or column-family-as-set pattern). Consider `OptimisticTransactionDB` for idempotent inserts.
- **Cost:** C++ FFI in the build; +30 MB binary; slower CI compile.

### 3. **LMDB via heed3** (insurance pick)

- 15+ years of adversarial use; OpenLDAP/Meilisearch run it at scale.
- heed3 (currently beta in early 2026, depending on LMDB master3 release) adds page checksums — important for our asymmetry.
- Use only if redb shows unexpected production issues; otherwise the C FFI is a step backward from redb.

### Explicit rejections
- **sled**: still beta in 2026, format-unstable, disqualified.
- **bitcask**: 44 GB RAM at 1B keys violates the "avoid ZFS DDT" design goal.
- **cuckoo on-disk / probabilistic structures as authoritative layer**: false-positive failure mode is data-loss. They are valid only as the in-memory front-end (which `fastbloom` already covers).
- **roaring / learned / succinct**: workload mismatch.

---

## Speculation flags

- Specific microsecond latencies for cold reads on NVMe are **estimated from device class**, not measured on SliceFS hardware. Concrete numbers must come from a benchmark on the target SSD.
- fjall's 1B+ scaling is **plausible but unproven** in publicly visible deployments.
- sanakirja benchmarks are 5 years stale.
- The "44 GB RAM for bitcask at 1B keys" assumes a 16-byte file-offset record; the precise figure varies but the order of magnitude is firm.

## Sources

- [rust-rocksdb on crates.io](https://crates.io/crates/rust-rocksdb)
- [rust-rocksdb 0.46.0 docs](https://docs.rs/crate/rust-rocksdb/latest)
- [RocksDB Tuning Guide (bloom, write amp)](https://github.com/facebook/rocksdb/wiki/RocksDB-Tuning-Guide)
- [RocksDB Bloom Filter wiki](https://github.com/facebook/rocksdb/wiki/RocksDB-Bloom-Filter)
- [Fjall 3.0 release announcement](https://fjall-rs.github.io/post/fjall-3/)
- [Fjall 2.8 release (bulk loading)](https://fjall-rs.github.io/post/fjall-2-8/)
- [fjall-rs/lsm-tree (crate)](https://github.com/fjall-rs/lsm-tree)
- [rust-storage-bench](https://github.com/marvin-j97/rust-storage-bench)
- [sled GitHub (still beta)](https://github.com/spacejam/sled)
- [sled on docs.rs (0.34.7)](https://docs.rs/crate/sled/latest)
- [redb GitHub](https://github.com/cberner/redb)
- [redb 3.0.0 release (Aug 2025)](https://github.com/cberner/redb/releases/tag/v3.0.0)
- [redb 4.1 release coverage (Phoronix, Apr 2026)](https://www.phoronix.com/news/Redb-4.1-Released)
- [redb 4.1 writeup with benchmark numbers](https://www.webpronews.com/rusts-redb-hits-4-1-ai-agents-squash-bugs-deliver-1-5x-write-speedups-in-embedded-kv-store/)
- [heed (LMDB Rust)](https://github.com/meilisearch/heed)
- [heed3 docs](https://docs.rs/heed3/latest/heed3/)
- [Sanakirja crate](https://crates.io/crates/sanakirja)
- [Pijul: Rethinking Sanakirja](https://pijul.org/posts/2021-02-06-rethinking-sanakirja/)
- [bitask (Bitcask in Rust)](https://crates.io/crates/bitask)
- [rust-cuckoofilter](https://github.com/axiomhq/rust-cuckoofilter)
- [Cuckoo Filter paper (CMU)](https://www.cs.cmu.edu/~dga/papers/cuckoo-conext2014.pdf)
- [roaring-rs](https://github.com/RoaringBitmap/roaring-rs)
- [BloomStore: bloom-filter-based KV for dedup on flash](https://www.researchgate.net/publication/254043379_BloomStore_Bloom-Filter_based_memory-efficient_key-value_store_for_indexing_of_data_deduplication_on_flash)
- [OpenZFS dedup post-mortem (despairlabs)](https://despairlabs.com/blog/posts/2024-10-27-openzfs-dedup-is-good-dont-use-it/)
- [ZFS Fast Dedup overview (Klara)](https://klarasystems.com/articles/zfs-fast-dedup-for-proxmox-ve-9x/)
