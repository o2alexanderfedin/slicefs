# DedupIndex on-disk physical layout for SSD/NVMe

Scope: choose a physical layout for the SliceFS `DedupIndex` (28-byte ChunkHash, set semantics, no values) tuned for modern SSD/NVMe. Workload is a huge initial seed (millions to billions of inserts), then mixed read-heavy with occasional `remove`. Bloom filter sits in front; the on-disk store is authoritative. Failure asymmetry is critical: a *false-positive* "Present" answer for a hash we never durably stored corrupts the CAS (data loss); a *false-negative* "Absent" answer for a hash we did store merely costs us a duplicate write (benign). The index must therefore never lose a durably-acknowledged insert, and durability ordering between (CAS chunk lands on disk) and (index entry becomes visible) must be enforced.

## 1. Write amplification (WAF) — structure-level vs device-level

WAF is multiplicative: `WAF_total = WAF_engine × WAF_FTL`. We must minimize both.

- **In-place B+tree.** Every insert dirties a 4–16 KiB page; small updates rewrite the whole page. Reported WiredTiger WAF is ~268× at 8 KiB pages and ~530× at 16 KiB pages on YCSB-like loads, vs RocksDB's ~38× — a 7–14× gap (Qiao et al., FAST '22). Causes: page-granularity rewrites, leaf splits, WAL doubling every payload. For our 28-byte keys this is catastrophic — a 28-byte insert drags a full 16 KiB page rewrite (≈585× before FTL).
- **LSM-tree.** Sequential SST flush + leveled compaction; per-record WAF roughly `≈ T × L` where `T` is size ratio (default 10) and `L` is number of levels. RocksDB production WAF commonly 8–30 (Facebook reports `T=10` typical). Compaction is the dominant cost; tiering halves WAF vs leveling at the cost of read amp.
- **Append-only log + in-memory hash / on-disk hash.** Insert is a single record append. WAF_engine ≈ 1.0–1.2 (header overhead). But without compaction `remove` leaks space, and recovery cost grows with log size.
- **Device-level (FTL).** Garbage collection, wear-levelling, and erase-before-write all amplify host writes. Sequential, large, aligned writes hit FTL WAF ≈ 1.0–1.2; random 4 KiB writes on a near-full consumer drive can hit 3–6× (Wikipedia: Write amplification; flashdba: FTL).

Combined: a B+tree of 28-byte keys on a 90%-full consumer SSD can see total WAF in the high hundreds. An append-friendly layout keeps total WAF in single digits.

## 2. Block alignment

Modern TLC/QLC NAND is overwhelmingly **16 KiB program page**, ~4–8 MiB **erase block**, atop 4 KiB OS / NVMe LBA (AnandTech ISSCC 2021; digitalcitizen.life). FS block is 4 KiB on APFS/ext4. Recommended I/O size for our writes:

- Append unit: **64 KiB or 128 KiB** segment buffers — multiple of NAND page, large enough to amortize FTL metadata, small enough to keep p99 latency low.
- Compaction / segment seal: **erase-block-aligned** (4 MiB chunks). Helps the FTL group sequential erases and avoids cross-block GC.
- Avoid sub-page (<4 KiB) writes: those force a read-modify-write inside the FTL.

## 3. Batching, group commit, RPO

Group-commit is mandatory. fsync on a consumer NVMe without PLP costs 1–4 ms; on enterprise PLP-backed NVMe it can drop to ~50–200 µs (Percona fsync benchmarks). Even at the optimistic end, naïve per-insert fsync caps us at ~10 K inserts/s — a billion-row seed would take ~28 hours of wall-clock fsync time alone.

Sweet spot for *seed*: **fsync per 64 KiB–1 MiB segment** (≈2 K–32 K keys per fsync at 28+ bytes per record), giving 100 K–500 K inserts/s on commodity NVMe with RPO = one segment (≈1 MiB / current ingest rate, typically <1 s). Steady-state mixed: **commit_delay ≈ 200–500 µs** style group-commit (Postgres model) — coalesce concurrent inserts but bound caller latency to ~1 ms.

## 4. Sync semantics

- `fsync` — forces data + metadata. Use only at log-rotate / checkpoint boundaries (size, mtime change).
- `fdatasync` — data + size-relevant metadata. ~2× faster than fsync (Percona). **Default** for per-segment durability when we have already pre-allocated and the file size is unchanged.
- `O_DIRECT` — bypasses the page cache. Required to get deterministic write sizes and avoid double-buffering during the seed. Does *not* flush device cache; must still call fdatasync (or pair with O_DSYNC).
- `O_DSYNC` — every write durable on return; equivalent to fdatasync-after-write. Convenient for tiny WAL records but kills batching, so reserve for the manifest/superblock pointer flip.

Recommended: `O_DIRECT | O_APPEND` for segments + explicit `fdatasync` at segment boundaries; `O_DSYNC` for the superblock / commit-record file.

## 5. Ordered writes / barriers

Required ordering: **CAS chunk durable → index entry durable → index entry visible**. Violating this is exactly the FP-causes-data-loss failure mode.

Mechanism:
1. Write the CAS chunk; `fdatasync` it. Only then enqueue the index insert.
2. Append index record into segment buffer; flush segment with `fdatasync`.
3. Bump an atomic in-memory "high-water mark" (HWM); only entries below HWM are answered as `Present`. The HWM advance happens after step 2 returns.
4. Periodically write a commit record (segment manifest) and `fdatasync` + flip the superblock pointer with `O_DSYNC` write.

Linux/macOS provide no portable write-barrier other than fsync/fdatasync; ordering must be enforced by *waiting* for the prior fdatasync to return before issuing the dependent write.

## 6. NVMe atomic-write capabilities

The NVM Command Set defines `AWUN` (Atomic Write Unit Normal) and `AWUPF` (Atomic Write Unit Power Fail). A write `≤ AWUPF` is guaranteed all-or-nothing across power loss; minimum mandatory AWUPF is **1 logical block** (typically 4 KiB) (NVM Express NVM Command Set Spec 1.1, 2024-08; MS Learn `NVME_CDW11_FEATURE_WRITE_ATOMICITY_NORMAL`). Many enterprise drives advertise AWUPF of 8–32 LBAs (32–128 KiB); some Samsung/Intel datacenter parts go higher with `NAWUPF` namespace-scoped values. QEMU 9.2 (2024) added passthrough so host filesystems can opt in (Oracle Linux blog).

Practical use: keep the **commit record / superblock ≤ 4 KiB** (always atomic), and align it to LBA. Do not rely on multi-block atomicity unless we probe `AWUPF` at mount time. Treat anything above guaranteed AWUPF as torn-write-prone and protect with a CRC + replay.

## 7. Layout recommendation: log-structured + periodic compaction

Pick: **append-only segmented log of fixed-size index records, with an in-memory hash index, periodic compaction into immutable sorted segments, bloom-filter front, manifest+superblock for crash recovery.**

Justification:

- **WAF is near-optimal.** Engine WAF ≈ 1 during seed (the dominant phase). Compaction adds ~2–4× during steady state — still order-of-magnitude better than B+tree.
- **Failure asymmetry respected.** Visibility (HWM) only advances after fdatasync; a torn tail segment is replayed and re-checksummed at recovery. We can never expose an entry that wasn't durable, so FP-on-stored-set-membership is impossible.
- **SSD-friendly I/O profile.** 64 KiB–4 MiB sequential appends, erase-block-aligned compaction, no random in-place updates → low FTL GC pressure, lower device WAF, longer drive life.

A B+tree+WAL hybrid is rejected: the random-page rewrite pattern of 28-byte keys is a worst-case for both FTL and engine WAF; we'd get the WAL cost *plus* the page cost.

## 8. Endurance budget

- Consumer NVMe (1 DWPD, 1 TB, 5 yr): ~1.8 PBW (Kingston, ATP).
- Enterprise NVMe mixed-use (3 DWPD, e.g. Samsung PM1733 1.6 TB+ SKUs): ~8.7 PBW for a 1.6 TB drive.
- High-endurance enterprise (10 DWPD): ~29 PBW for a 1.6 TB drive.

A 5× total-WAF blow-up shrinks effective lifetime 5×: a 5-year 1 DWPD drive becomes a 1-year drive. This is why the engine WAF must stay close to 1.

### Write budget table (seed = 1 B keys, 32 B per record incl. overhead → 32 GB logical)

Steady-state inserts/day are application writes; multiply by total WAF (engine × FTL ≈ 1.5 for log-structured, ≈ 60 for B+tree on this workload) for *device* writes. 5-year horizon, drive size 1 TB.

| Scenario | App writes/day | Engine WAF | Total WAF | Device writes / 5 yr | Drive class needed |
|---|---|---|---|---|---|
| Seed only (one-shot 32 GB) | — | 1.0 | 1.2 | 38 GB | any |
| Light steady (10 M ins/day = 320 MB) | 320 MB | 1.2 | 1.5 | 0.88 TB | any consumer |
| Medium steady (100 M ins/day = 3.2 GB) | 3.2 GB | 2.0 | 3.0 | 17.5 TB | consumer 1 DWPD ok |
| Heavy steady (1 B ins/day = 32 GB) | 32 GB | 3.0 | 4.5 | 263 TB | consumer marginal; enterprise 1 DWPD ok |
| Heavy + B+tree alternative | 32 GB | 30 | 60 | 3.5 PB | enterprise 3+ DWPD required |

## Conclusion

**Recommended layout: append-only segmented log of 32-byte index records (28-byte hash + 4-byte CRC/flags) with in-memory open-addressing hash, periodic erase-block-aligned compaction into immutable sorted runs, bloom filter front, fdatasync-per-segment + O_DSYNC superblock pointer flip for crash-consistent visibility.**

Justification:
- Engine WAF ≈ 1.0 during the dominant seed phase and ≈ 2–3× steady-state — 10–30× better than a B+tree on 28-byte keys, keeping us inside a 1 DWPD consumer endurance budget for realistic workloads.
- Ordered visibility via post-fdatasync HWM advance makes it structurally impossible to answer `Present` for a hash whose record wasn't durable, satisfying the FP=data-loss invariant.
- Sequential 64 KiB–4 MiB I/O matches NAND page/erase-block geometry, minimizes FTL garbage-collection amplification, and stays within NVMe `AWUPF` atomicity for the only critical small write (the superblock/commit record).

## Sources

- [Closing the B+-tree vs. LSM-tree Write Amplification Gap on Modern Storage Hardware (FAST '22, Qiao et al.)](https://www.usenix.org/system/files/fast22-qiao.pdf)
- [Revisiting B+-tree vs. LSM-tree (USENIX ;login: online)](https://www.usenix.org/publications/loginonline/revisit-b-tree-vs-lsm-tree-upon-arrival-modern-storage-hardware-built)
- [Write amplification — Wikipedia](https://en.wikipedia.org/wiki/Write_amplification)
- [Understanding Flash: The Flash Translation Layer (flashdba)](https://flashdba.com/2014/09/17/understanding-flash-the-flash-translation-layer/)
- [Coding for SSDs Part 3: Pages, Blocks, and the FTL (Code Capsule)](https://codecapsule.com/2014/02/12/coding-for-ssds-part-3-pages-blocks-and-the-flash-translation-layer/)
- [NAND at ISSCC 2021: TLC/QLC page and block geometry (AnandTech)](https://www.anandtech.com/show/16491/flash-memory-at-isscc-2021)
- [Write Amplification in SSDs (digitalcitizen.life)](https://www.digitalcitizen.life/write-amplification-in-ssds-why-your-drive-wears-faster-than-you-think/)
- [NVM Express NVM Command Set Specification 1.1 (2024-08-05) — AWUN/AWUPF](https://nvmexpress.org/wp-content/uploads/NVM-Express-NVM-Command-Set-Specification-Revision-1.1-2024.08.05-Ratified.pdf)
- [Microsoft Learn: NVME_CDW11_FEATURE_WRITE_ATOMICITY_NORMAL](https://learn.microsoft.com/en-us/windows/win32/api/nvme/ns-nvme-nvme_cdw11_feature_write_atomicity_normal)
- [Introducing NVMe Atomic Write Support with QEMU 9.2 (Oracle Linux blog, 2024)](https://blogs.oracle.com/linux/nvme-atomic-write-with-qemu)
- [Fsync Performance on Storage Devices (Percona)](https://www.percona.com/blog/fsync-performance-storage-devices/)
- [PostgreSQL Asynchronous Commit & commit_delay docs](https://www.postgresql.org/docs/current/wal-async-commit.html)
- [InnoDB, fsync and fdatasync — reducing commit latency (Small Datum)](http://smalldatum.blogspot.com/2020/10/innodb-fsync-and-fdatasync-reducing.html)
- [RocksDB Tuning Guide (write amplification)](https://github.com/facebook/rocksdb/wiki/RocksDB-Tuning-Guide)
- [Optimizing Space Amplification in RocksDB (CIDR '17, Dong et al.)](https://www.cidrdb.org/cidr2017/papers/p82-dong-cidr17.pdf)
- [Optimizing RocksDB Write Amplification on FDP SSDs (Samsung)](https://semiconductor.samsung.com/news-events/tech-blog/optimizing-rocksdb-write-amplification-on-fdp-ssds/)
- [Constructing and Analyzing the LSM Compaction Design Space (VLDB '21)](http://vldb.org/pvldb/vol14/p2216-sarkar.pdf)
- [Understanding SSD Endurance: TBW and DWPD (Kingston)](https://www.kingston.com/en/blog/servers-and-data-centers/understanding-ssd-endurance-tbw-dwpd)
- [TBW and DWPD: How SSD Endurance Specs Can Mislead Buyers (ATP)](https://www.atpinc.com/blog/ssd-tbw-dwpd-endurance)
- [Samsung PM9A3 NVMe PCIe SSD product brief (1.3 DWPD)](https://download.semiconductor.samsung.com/resources/brochure/Samsung%20PM9A3%20NVMe%20PCIe%20SSD.pdf)
- [Samsung PM1733 NVMe SSD datasheet (1 DWPD / 3 DWPD configs)](https://download.semiconductor.samsung.com/resources/brochure/PM1733%20NVMe%20SSD.pdf)
- [Over-provisioning enhances NAND endurance (ATP)](https://www.atpinc.com/de/blog/over-provisioning-ssd-benefits-endurance-and-performance)
- [HaloDB — log-structured KV store (ordering & durability docs)](https://github.com/yahoo/HaloDB)
