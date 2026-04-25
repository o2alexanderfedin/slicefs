# DedupIndex Crash Safety & Durability

**Domain:** SliceFS persistent DedupIndex (CAS-07)
**Researched:** 2026-04-23
**Confidence:** HIGH (Linux/macOS fsync(2) man pages, NVM Express NVM Command Set 1.0a, RocksDB/redb/LMDB design docs, InnoDB 8.4 reference, BLAKE3/xxHash benchmarks)

---

## 0. The Asymmetry — Why This Document Exists

Restating the central rule, because every design choice below collapses to it:

| Outcome | Symptom | Cost |
|---|---|---|
| **False POSITIVE** (`lookup → Present`, but CAS lacks the block) | Caller skips writing → block never lands on disk | **Catastrophic, irrecoverable data loss** |
| **False NEGATIVE** (`lookup → Absent`, but CAS has the block) | Caller re-writes; CAS write is idempotent (same hash → same path) | Wasted I/O; self-healing |

Index durability must be designed to make false positives **impossible by construction**, even at the cost of accepting false negatives during a recovery window.

---

## 1. Formal Durability Invariant

Define an `insert(h)` as **durable at time t** iff every byte required to reconstruct the assertion `h ∈ Index` has been transferred from any volatile cache (process heap, OS page cache, drive DRAM) to non-volatile storage **before** time t, and the transfer is verifiable on next open.

Concretely, `insert(h)` is durable when **all five** of the following hold:

1. **(a) Process kill** — bytes are out of the SliceFS process address space. Satisfied by `write(2)`.
2. **(b) OS crash** — bytes are out of the kernel page cache. Satisfied by `fsync(2)` (Linux) or `fcntl(F_FULLFSYNC)` (macOS — plain `fsync(2)` does **not** flush the drive cache on Darwin; see [Apple `fsync(2)` manpage](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/fsync.2.html), [Tsai 2022](https://mjtsai.com/blog/2022/02/17/apple-ssd-benchmarks-and-f_fullsync/), [transactional.blog 2022](https://transactional.blog/blog/2022-darwins-deceptive-durability)).
3. **(c) Power loss** — bytes are out of the drive's volatile DRAM. Satisfied by either (i) the drive having Power Loss Protection capacitors, or (ii) the FLUSH CACHE command issued by `F_FULLFSYNC` / `fsync` returning **after** the drive has migrated DRAM → NAND ([Small Datum 2026](http://smalldatum.blogspot.com/2026/01/ssds-power-loss-protection-and-fsync.html)).
4. **(d) Torn write** — the stored bytes are individually atomic at the granularity SliceFS depends on. NVMe's `AWUPF` (Atomic Write Unit Power Fail) is the per-device guarantee ([NVM Express NVM Command Set 1.0a §4.2.2](https://nvmexpress.org/wp-content/uploads/NVMe-NVM-Command-Set-Specification-1.0a-2021.07.26-Ratified.pdf), [Oracle Linux blog 2024](https://blogs.oracle.com/linux/nvme-atomic-write-with-qemu)). Consumer SSDs guarantee 512 B – 4 KiB; enterprise drives advertise up to 16–64 KiB. SliceFS MUST NOT assume >4 KiB atomicity.
5. **(e) Bit rot** — the stored bytes are recoverable as written, not as silently flipped by NAND wear. Requires application-level checksum (the storage stack itself does not detect post-write corruption on most consumer SSDs).

The directory containing the index file must also be `fsync`'d after creation/rename, or the index file may not be present after a crash even if its bytes are ([fsync(2) Linux manpage](https://man7.org/linux/man-pages/man2/fsync.2.html)).

---

## 2. The False-Positive Crash Window

A false positive becomes possible if and only if **the index entry for `h` reaches durable storage before the CAS block keyed by `h` does.** The window is exactly:

```
T0: CAS write begins             (block bytes in OS page cache)
T1: index insert begins          (bloom + on-disk insert)
T2: index insert fsync returns   ← if crash here, index says Present
T3: CAS fsync returns            ← block is durable from here onwards
```

Crash in `[T2, T3)` produces the catastrophic state.

**Eliminating the window — the ordering rule:**

> **CAS-store fsync MUST happen-before any persistent index insert.**
> The in-memory bloom and on-disk index for hash `h` are updated **only after** `fsync(cas_block_fd)` (Linux) or `fcntl(F_FULLFSYNC, cas_block_fd)` (macOS) **returns successfully**, AND the parent directory of the CAS block has also been `fsync`'d.

This is the single most important rule in the durability story. It cannot be relaxed for performance. If it is, the design is broken.

A complementary safety net (see §3) is **on-read verification**: before returning `Present`, the caller may verify the CAS path exists. This is cheap (`stat(2)` on a known path) and turns any residual false positive into a self-healing false negative. We should ship both: ordering as the primary defence, on-read `stat` as belt-and-braces.

---

## 3. CAS-Store-as-Truth — Index Is a Cache

**Argument FOR derivability:** The CAS store directory listing already contains every durable hash (each block's filename is its hash). The index is therefore a **performance-only artifact**: a sorted/bloom-front-loaded view of `ls cas/`. Treating it as such has profound consequences:

- We never need to recover the index from a WAL — we can rebuild it from `cas/`.
- We can ship with a simpler storage engine (no MVCC required, no MANIFEST surgery).
- We can run `fsck-style` rebuild as a routine recovery, not an emergency.

**Rebuild cost (estimate):**

For N blocks with 28-byte ChunkHash keys:
- Directory walk: O(N) `readdir` entries. On ext4/XFS/APFS with `getdents64`, ~1 M entries/sec on warm cache, ~100 K/sec cold.
- Hash decoding: parse hex/base32 filename → 28 B key. Free.
- Bloom rebuild: N `bloom.insert()` calls — fastbloom is ~50 ns each → 50 ms per million.
- On-disk index rebuild: bulk-load sorted (sort N keys, sequential write). For N=10 M, ~5 s on NVMe.

**Bottom line:** A 10 M-block store rebuilds in ~10–30 s cold, ~5 s warm. **Online rebuild is feasible** by serving `lookup` directly from the directory walk in progress (sorted-merge or scan-and-mark), but the simpler pattern is **block on rebuild at mount**, with progress reporting. Online rebuild becomes worthwhile only above ~50 M blocks.

**Recommendation:** Adopt CAS-as-truth. The index is rebuildable. This single decision removes most of the WAL complexity below.

---

## 4. WAL — Separate vs Piggybacked

| Option | Pro | Con |
|---|---|---|
| **No WAL** (rebuild from CAS on every restart) | Zero write amplification on insert path; trivial code | Cold-start cost on every mount |
| **Piggyback storage-engine WAL** (RocksDB / redb) | Engine handles fsync, group-commit, checksums; battle-tested ([RocksDB WAL wiki](https://github.com/facebook/rocksdb/wiki/Write-Ahead-Log-(WAL))) | Forces the engine choice; double durability cost when paired with §2 ordering rule |
| **Custom append-only WAL** | Trivially crash-safe (log-structured naturally tolerates torn writes — torn record is detected by checksum and truncated; see [LSM/SSTable design](https://en.wikipedia.org/wiki/Log-structured_merge-tree)) | Yet another moving part to test |

**Recommendation:** Use the chosen storage engine's WAL (likely **redb** for pure-Rust + COW B-tree — see [redb design.md](https://github.com/cberner/redb/blob/master/docs/design.md)). redb's COW B-tree gives torn-write resistance for free (LMDB-style shadow paging — new pages written before old pages are reused, so a crash mid-write leaves the previous valid root). No double-write buffer is needed, unlike InnoDB ([MySQL 8.4 §17.6.4](https://dev.mysql.com/doc/refman/8.4/en/innodb-doublewrite-buffer.html)). Disabling the WAL on writes (`WriteOptions::sync = false` equivalent) is **acceptable** when paired with §3 — a crash loses recent inserts, which the next mount rebuilds from CAS.

If we instead pick RocksDB, set `WriteOptions::sync = true` only on a periodic checkpoint, not per-insert; rely on rebuild for the gap.

---

## 5. Checksums

- **Per-page (4 KiB) CRC32C** at the storage-engine level. redb already does this; if we hand-roll, use `crc32c` (SSE4.2 CRC32 instruction) — ~20 GB/s.
- **No per-entry CRC needed** at the index level. The whole-block hash *is* the value, and the block's filename in CAS already attests its identity.
- **Algorithm choice for any application-level checksumming**: **xxh3** for incidental integrity (e.g., bloom file checksum on load). xxh3 reaches **~4–6 GB/s single-threaded** vs BLAKE3 at ~1 GB/s single-threaded; BLAKE3 only catches up with multi-threaded SIMD ([jolynch.github.io](https://jolynch.github.io/posts/use_fast_data_algorithms/), [mojoauth comparison](https://mojoauth.com/compare-hashing-algorithms/xxhash-vs-blake3)). We do not need cryptographic strength for index integrity — a malicious actor who can flip bits on the local SSD has already won.
- **Verification cadence:**
  - On every read of an index page → free (CPU is the bottleneck on cached reads, but CRC32C is hardware-accelerated and effectively free).
  - On startup → mandatory full scrub of the **bloom file header** only (it's tiny — ~MB).
  - **Background scrubber thread** → optional, walks the on-disk index pages once per day. Surfaces bit rot before it bites.

---

## 6. Torn Writes

NVMe consumer SSDs guarantee atomicity at the **logical block size**, typically 512 B or 4 KiB; `AWUPF` reports the power-fail-atomic unit ([NVMe NVM Command Set 1.0a](https://nvmexpress.org/wp-content/uploads/NVMe-NVM-Command-Set-Specification-1.0a-2021.07.26-Ratified.pdf)). Enterprise drives advertise 16–64 KiB but SliceFS cannot rely on that.

- **B-tree (e.g., redb / LMDB)**: tolerates torn writes via COW shadow paging — the previous root remains valid until the new root is atomically (single 4 KiB write) installed. No double-write buffer needed.
- **B-tree (e.g., InnoDB)**: needs a doublewrite buffer because pages are 16 KiB and updated in place ([MySQL 8.4 §17.6.4](https://dev.mysql.com/doc/refman/8.4/en/innodb-doublewrite-buffer.html), [Percona blog](https://www.percona.com/blog/innodb-double-write/)).
- **Log-structured (LSM)**: naturally torn-write-resistant — a torn final record is detected by checksum and truncated.

**Recommendation: redb (COW)**. We get torn-write safety for free.

---

## 7. Bloom-Filter Durability

The bloom filter's role is **performance only**: a `false` answer is an authoritative cache hit (definitely-absent), but the *correctness* of `false` depends only on the filter never having missed an insert that was applied to the on-disk index. Since the on-disk index is itself derivable from CAS (§3), the bloom is **derivable squared**.

**Decision:** **Do not log bloom mutations.** Persist a snapshot opportunistically (e.g., on clean shutdown, every N inserts, or every M minutes). On startup, load the most recent snapshot if its checksum matches; otherwise rebuild from the on-disk index in O(N) ns. A 10 M-entry fastbloom rebuilds in ~500 ms.

False negatives from a stale bloom are impossible — `bloom_check → true` falls through to authoritative `lookup` anyway. The only failure mode is a slightly elevated false-positive rate immediately after restart, which costs lookups, not correctness.

---

## 8. Recovery Procedure

Concrete startup flow for SliceFS mount:

```
1. Open CAS root directory; fsync if first-mount.
2. Open the on-disk index file (redb).
   - redb internally validates the latest committed root via page checksums.
   - If both roots are corrupt → mark index "stale", continue to step 5.
3. Open the bloom filter snapshot file.
   - Verify xxh3 header checksum.
   - On mismatch → mark bloom "stale", continue.
4. If both index and bloom are valid → mount READ-WRITE, schedule background scrubber. DONE.
5. Recovery path (any of: stale index, stale bloom, --force-rebuild):
   a. Walk cas/ with getdents64 in a worker thread.
   b. Bulk-load keys into a fresh redb table (sorted, sequential).
   c. Build bloom from the key stream.
   d. Atomically rename new index/bloom files into place; fsync directory.
   e. Mount READ-WRITE.
6. Background scrubber (always on after mount):
   - Walk index pages once / 24 h, verify CRC32C.
   - Walk a sample of CAS blocks, recompute hash, verify filename match.
   - Surface mismatches via metric/log; do not auto-repair the CAS (may indicate an attack).
```

Two user-visible knobs:
- `--mount-mode={fast|paranoid}`: `fast` skips the optional CAS sample-rehash on mount; `paranoid` walks the entire CAS and re-attests every hash before accepting writes (slow on large stores but the only true guarantee against bit rot of CAS bodies).
- `--force-rebuild`: discard index and rebuild from CAS unconditionally.

---

## Formal Spec Block

```
insert(h):
  PRE  : h ∈ ChunkHash; CAS(h) exists and is durable
         (i.e. fsync(cas_block) has returned, and parent dir is fsync'd)
  POST : Index ∋ h is durable per §1, OR a future call returns Absent
         (caller will retry; CAS write is idempotent)
  INV  : Index ⊆ CAS  (no false positives, ever)
  ORDER: cas_fsync ▸ bloom.insert ▸ ondisk.insert  (must hold)

lookup(h):
  PRE  : h ∈ ChunkHash
  POST : returns Present     ⇒ CAS(h) exists       (no false positive)
         returns Absent      ⇒ caller re-writes    (idempotent)
         returns DefAbsent   ⇒ bloom said no       (authoritative)
  INV  : a Present return is verifiable: CAS path stat(2) succeeds.

remove(h):
  PRE  : caller holds GC lock; CAS(h) has been deleted (or is being deleted in same txn)
  POST : Index does not contain h after fsync; bloom NOT updated
         (false-positive lookups acceptable; ordering: index.remove ▸ cas.unlink
          — opposite of insert, to avoid orphan index entries pointing to live CAS)
  INV  : Index ⊆ CAS continues to hold.
```

---

## Recommendation

**One-sentence recovery story:** Treat the index as a derivable cache of the CAS directory; persist it via redb (COW B-tree, free torn-write resistance) with the strict ordering rule `cas_fsync ▸ index_insert`, snapshot the bloom opportunistically, and rebuild from CAS on any integrity-check failure.

**Required user-visible config:**
- `mount_mode = fast | paranoid` (controls whether CAS-body rehash runs at mount).
- `force_rebuild` (one-shot flag).
- macOS-specific: `use_f_fullfsync = true` (default on; the only correct setting on Apple Silicon — plain `fsync` is **not durable** on Darwin).

---

## Sources

- [fsync(2) — Linux manual page](https://man7.org/linux/man-pages/man2/fsync.2.html)
- [Apple fsync(2) manpage](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/fsync.2.html)
- [Darwin's Deceptive Durability — transactional.blog 2022](https://transactional.blog/blog/2022-darwins-deceptive-durability)
- [Apple SSD Benchmarks and F_FULLSYNC — Michael Tsai 2022](https://mjtsai.com/blog/2022/02/17/apple-ssd-benchmarks-and-f_fullsync/)
- [NVM Express NVM Command Set Specification 1.0a (PDF)](https://nvmexpress.org/wp-content/uploads/NVMe-NVM-Command-Set-Specification-1.0a-2021.07.26-Ratified.pdf)
- [Introducing NVMe Atomic Write Support with QEMU 9.2 — Oracle Linux blog 2024](https://blogs.oracle.com/linux/nvme-atomic-write-with-qemu)
- [SSDs, power loss protection and fsync latency — Small Datum 2026](http://smalldatum.blogspot.com/2026/01/ssds-power-loss-protection-and-fsync.html)
- [RocksDB Write-Ahead Log wiki](https://github.com/facebook/rocksdb/wiki/Write-Ahead-Log-(WAL))
- [redb 1.0 release notes](https://www.redb.org/post/2023/06/16/1-0-stable-release/)
- [redb design.md](https://github.com/cberner/redb/blob/master/docs/design.md)
- [LMDB — Wikipedia](https://en.wikipedia.org/wiki/Lightning_Memory-Mapped_Database)
- [libmdbx — README](https://github.com/erthink/libmdbx)
- [MySQL 8.4 Reference Manual §17.6.4 Doublewrite Buffer](https://dev.mysql.com/doc/refman/8.4/en/innodb-doublewrite-buffer.html)
- [Percona — InnoDB Double Write](https://www.percona.com/blog/innodb-double-write/)
- [Log-structured merge-tree — Wikipedia](https://en.wikipedia.org/wiki/Log-structured_merge-tree)
- [Use Fast Data Algorithms — jolynch.github.io](https://jolynch.github.io/posts/use_fast_data_algorithms/)
- [xxHash vs BLAKE3 — mojoauth](https://mojoauth.com/compare-hashing-algorithms/xxhash-vs-blake3)
- [Durability: Linux File APIs — evanjones.ca](https://www.evanjones.ca/durability-filesystem.html)
