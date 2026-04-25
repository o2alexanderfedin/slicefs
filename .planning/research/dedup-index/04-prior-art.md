# Prior Art: On-Disk Dedup Index Implementations

**Domain:** SliceFS persistent on-disk DedupIndex (28-byte ChunkHash, SET semantics, bloom-front + on-disk authoritative, billions of entries, single-host SSD)
**Researched:** 2026-04-23
**Confidence:** MEDIUM-HIGH (sources cited inline; vendor architecture docs marked SPECULATIVE where they hide internals)

---

## Scope

We are looking for systems that store a content-addressed key set on disk, must answer "have I seen this hash?" billions of times, and must NEVER produce a false positive (FP → restore returns wrong bytes → catastrophic). False negatives (FN → re-store the chunk) are benign. Each entry below covers: **on-disk layout**, **persistence model**, **scale + war stories**, **transferability to SliceFS**.

---

## 1. ZFS DDT — the cautionary tale

**Layout.** DDT is a ZAP (ZFS Attribute Processor) object — a generic name→value store using `microzap` (small, packed) or `fatzap` (extensible-hash, leaf+pointer blocks). Each cached DDT entry is ~320 B in legacy dedup; in-memory was an AVL tree keyed by checksum. Sources: [DeepWiki — DDT and ZAP](https://deepwiki.com/truenas/zfs/3.5-deduplication-(ddt)-and-zap-attribute-processor), [Matt Ahrens dedup paper](https://openzfs.org/w/images/8/8d/ZFS_dedup.pdf).

**Persistence.** Every block write/free mutates the ZAP in the same TXG. ZAP leaf blocks become hot; COW amplifies; cold lookups page random leaves into ARC.

**Scale & war stories.** Rule of thumb 1–5 GB ARC per TB; once DDT spills out of ARC, throughput collapses to seek rate ([Oracle sizing](https://www.oracle.com/technical-resources/articles/it-infrastructure/admin-o11-113-size-zfs-dedup.html), [cr0x.net war story](https://cr0x.net/en/zfs-dedup-eats-ram/), [despairlabs — "good now and you shouldn't use it"](https://despairlabs.com/blog/posts/2024-10-27-openzfs-dedup-is-good-dont-use-it/)). OpenZFS [#16713](https://github.com/openzfs/zfs/issues/16713): `ddtprune -p 100` + `dedup_table_quota=1` produced unbootable pools with scrub checksum failures — DDT mutation that was not crash-safe.

**OpenZFS 2.3 "Fast Dedup" (Feb 2024, Klara/iXsystems).** Adds in-memory AVL + an **append-only on-disk log (FDT-Log)** that buffers DDT mutations and flushes batches into the ZAP. Adds dedup-quota and `ddtprune`. AVL ordering changed to match ZAP key order so flush is sequential. Corrupted FDT block becomes immutable but pool still mounts. ([Klara intro](https://klarasystems.com/articles/introducing-openzfs-fast-dedup/), [Allan Jude AsiaBSDCon 2024](https://2024.asiabsdcon.org/program/_p01a/paper.pdf)).

**Transferable:** the FDT-Log pattern (RAM mutable + on-disk append log + batched merge into authoritative store) is exactly the bloom-front + on-disk-authoritative shape SliceFS wants.

---

## 2. Btrfs duperemove — out-of-band, SQLite hashfile

**Layout.** SQLite `.hashfile` storing per-extent SHA-256 + extent metadata. Out-of-band only; in-band never landed ([LWN](https://lwn.net/Articles/679031/)). Source: [duperemove](https://markfasheh.github.io/duperemove/duperemove.html).

**Persistence.** SQLite WAL. The kernel does the share via `ioctl(FIDEDUPERANGE)` which **byte-compares before sharing** — the hash table is advisory; the kernel refuses to share unequal extents. **FP-safe by construction.**

**Scale.** Tens of millions of extents on multi-TB FSes. SQLite becomes the bottleneck above ~10⁸ rows.

**Transferable:** "hash table is advisory, verify before commit" is the only foolproof FP defense. SliceFS's collision-time content-equality check is validated by duperemove's production track record.

---

## 3. Borg Backup — HashIndex (open-addressing, on-disk == in-memory)

**Layout.** `HashIndex` (C, Cython-wrapped) is flat open-addressing with **linear probing**. **On-disk format is byte-identical to the mmap'd in-memory layout**: header + N buckets of `key_size + value_size`. Resize at 75%/25% load; tombstones up to ~93%. Sources: [data-structures docs](https://borgbackup.readthedocs.io/en/stable/internals/data-structures.html), [#1985 hashtable findings](https://github.com/borgbackup/borg/issues/1985), [#3868 — RAM limits](https://github.com/borgbackup/borg/issues/3868).

**Persistence.** No WAL. Repository index is `index.<TRANSACTION_ID>`, **rewritten atomically per transaction** (rename-over). Crash recovery rebuilds via segment scan if missing/stale.

**Scale.** Issue #3868 makes RAM the explicit limit — OOM at hundreds of millions of chunks because the whole index is mmap-loaded. Rewrite-on-commit cost grows with index size.

**Transferable:** binary-identical disk/memory layout is elegant for SSDs; rewrite-per-transaction does NOT scale to billions and is what SliceFS must avoid.

---

## 4. Restic — JSON index files, master-index, supersede-linked

**Layout.** Many small JSON index files, each ≤ 8 MiB, listing pack→blob. `MasterIndex` is the in-memory union. Index files have a `supersedes` field listing IDs of older indexes they replace. Sources: [design.rst](https://github.com/restic/restic/blob/master/doc/design.rst), [terminology](https://restic.readthedocs.io/en/stable/design.html).

**Persistence.** Append-only. New indexes written; `prune`/`rebuild-index` writes new files with `supersedes`-pointers and deletes old. **Crash-safe by virtue of immutability** — partial writes ignored on next list.

**Scale.** Users report `prune`/`rebuild-index` taking hours at multi-TB; master-index must fit in RAM. JSON parsing dominates load — binary v2 index added for this reason.

**Transferable:** **immutable, append-only, supersede-pointer pattern** is highly relevant. "Many small files unioned in RAM" trades disk reads for crash simplicity.

---

## 5. casync — chunk store + .caibx/.caidx index

**Layout.** Chunks live as compressed files in `default.castr/<2-hex>/<full-hex>.cacnk`, sharded by 2 hex digits of SHA-512/256. Chunk-index files (`.caibx` / `.caidx`) are flat arrays of `(hash, size)` in stream order. Sources: [casync GitHub](https://github.com/systemd/casync), [Poettering blog](https://0pointer.net/blog/casync-a-tool-for-distributing-file-system-images.html).

**Persistence.** No central dedup index — existence is `stat()` on `castr/XX/HASH.cacnk`. **The filesystem is the index.**

**Scale.** Inode/dirent overhead dominates above ~10⁸ chunks. Works because casync targets OS-image distribution (10⁵–10⁶ chunks).

**Transferable:** confirms the **anti-pattern** of "FS is the index" — seductively simple, doesn't scale.

---

## 6. Proxmox Backup Server — chunkstore + .didx/.fidx

**Layout.** `<datastore>/.chunks/0000…ffff/` (65,536 preallocated dirs sharded by 2-byte hash prefix). Per-snapshot index files: `.fidx` (fixed 4 MiB chunks for VMs) or `.didx` (rolling-hash dynamic chunks for `pxar` archives). Index files are flat hash arrays. Source: [PBS technical overview](https://pbs.proxmox.com/docs/technical-overview.html).

**Persistence.** Same FS-as-index model as casync. GC walks all index files, marks via mtime touch, sweeps unreferenced.

**Scale.** Multi-PB deployments documented. The 65,536-shard scheme keeps per-dir entries reasonable up to ~10⁹ chunks (~15k entries/dir worst case). GC walks become the pain point.

**Transferable:** the **2-byte prefix shard with preallocation** is a proven directory layout for any file-per-chunk fallback. Existence-via-stat still doesn't scale lookups.

---

## 7. Tarsnap — append-only log of HMAC-keyed blocks

**Layout.** Client-side dedup against an HMAC-SHA256 keyspace; server is a dumb blob store. Server uses a **log-structured store** with periodic compaction; client maintains a local cache to short-circuit network round-trips. Sources: [Wikipedia](https://en.wikipedia.org/wiki/Tarsnap), [Magical's reverse-engineered architecture](http://tilde.town/~magical/tarsnap.html), [Percival EuroBSDCon 2013](http://www.daemonology.net/papers/EuroBSDCon13.pdf).

**Persistence.** Transactions: `start, write*, commit | cancel`. The log is the truth; the index is rebuildable.

**Scale.** SPECULATIVE on absolute numbers (Tarsnap doesn't publish), but continuous operation since 2008 with no published index-corruption postmortems.

**Transferable:** **log-as-truth + rebuildable-index** is the cleanest crash model. If the index is lost/torn, scan the log and rebuild.

---

## 8. IPFS — pluggable KV (badger / leveldb / pebble)

**Layout.** Kubo's blockstore wraps a pluggable KV: LevelDB (default small), Badger v1 (deprecated — corruption under power loss), Pebble (recommended for large repos). Since [go-ipfs 0.12](https://github.com/ipfs/kubo/releases/tag/v0.12.0), keys are the **multihash** (codec-stripped) so the same block under different CID codecs dedupes. Source: [Kubo datastores doc](https://github.com/ipfs/kubo/blob/master/docs/datastores.md).

**Persistence.** LSM-tree (LevelDB/Pebble) or LSM-with-value-log (Badger). WAL + SSTables. Crash-safe by construction.

**Scale.** Pebble nodes documented at 100M+ blocks; Badger v1 known to corrupt under power loss (deprecation rationale).

**Transferable:** **don't write your own LSM** — proven embedded KVs (rocksdb, sled, redb, fjall) buy correctness and tooling. Strong vote for using a battle-tested KV as SliceFS's authoritative on-disk layer.

---

## 9. VAST / Pure / Dell PowerProtect — published architecture

**VAST.** Hash tables and reduction metadata live in **shared Storage Class Memory (SCM)** across the cluster, not controller DRAM. Single-host SliceFS can't replicate SCM, but the principle "metadata in fast persistent tier, RAM only as cache" maps directly to the bloom-front design. ([VAST whitepaper](https://www.vastdata.com/whitepaper)).

**Pure FlashArray.** Variable 4 KiB–32 KiB chunks; **hashes are NEVER trusted alone** — match candidates undergo full byte-compare before share. ([Pure 101 blog](https://blog.purestorage.com/pure-storage-101-adaptive-data-reduction/)).

**Dell PowerProtect (Data Domain SISL).** [Zhu et al., FAST 2008](https://www.usenix.org/legacy/events/fast08/tech/full_papers/zhu/zhu.pdf): (1) **Bloom filter** ("Summary Vector") to short-circuit lookups for unseen fingerprints, (2) **Stream-Informed Segment Layout** packs related segments together so referencing one prefetches its neighbors' fingerprints, (3) **Locality-Preserved Caching** keeps neighbors hot. Together: ~99% of disk accesses for dedup lookup eliminated.

**Transferable:** SISL Summary Vector = SliceFS's planned bloom-front. Stream-locality clustering is a future scaling lever if the authoritative tier outgrows in-page cache.

---

## 10. RocksDB-backed systems (CockroachDB / TiKV) — tuning lessons

Less directly relevant to dedup, but: (a) per-SSTable bloom filters + hash-table-friendly key encoding kill 99% of negative lookups (already in SliceFS plan), (b) write amplification under high-churn keysets is the dominant cost — for an insert-heavy SET this is fine; deletes (chunk reclamation) need careful tombstone handling.

---

## Top 3 patterns to steal

1. **OpenZFS Fast Dedup FDT-Log: in-RAM mutable + append-only on-disk log + batched merge into authoritative store.** Maps directly onto bloom-front + on-disk-authoritative. ([Klara](https://klarasystems.com/articles/introducing-openzfs-fast-dedup/), [Allan Jude paper](https://2024.asiabsdcon.org/program/_p01a/paper.pdf))
2. **Restic's immutable, append-only, supersede-linked index files.** Crash safety falls out of immutability; partial writes ignored. Compaction = write new file with `supersedes`, delete old. ([restic design.rst](https://github.com/restic/restic/blob/master/doc/design.rst))
3. **Data Domain SISL bloom-front + locality-preserved cache** — the original 2008 design that made dedup tractable; the bloom alone removes 99% of negative-lookup disk I/O. ([Zhu et al. FAST 2008](https://www.usenix.org/legacy/events/fast08/tech/full_papers/zhu/zhu.pdf))

## Top 3 anti-patterns to avoid

1. **DDT-in-ARC (legacy ZFS dedup).** Index sized to RAM, not disk. Throughput cliff on spillover. War stories: [cr0x.net](https://cr0x.net/en/zfs-dedup-eats-ram/), [despairlabs](https://despairlabs.com/blog/posts/2024-10-27-openzfs-dedup-is-good-dont-use-it/), [Oracle sizing](https://www.oracle.com/technical-resources/articles/it-infrastructure/admin-o11-113-size-zfs-dedup.html).
2. **Filesystem as the dedup index (casync, PBS).** `stat()` per lookup, dirent overhead, GC walks O(n_files). Works at 10⁶, breaks at 10⁹.
3. **Trusting hash equality alone (legacy ZFS).** Pure and duperemove both byte-compare before sharing. SHA-256/Blake3 collisions are astronomically improbable, but a flipped bit on disk that fakes a hash hit is not — verify on collision. Bonus anti-pattern: **rewrite-the-whole-index-per-transaction (Borg [#3868](https://github.com/borgbackup/borg/issues/3868))** — atomic rename gives crash safety but commit cost is O(index size).

## Open question prior art doesn't answer

**How do you size the bloom filter for a SET that grows from 0 to 10⁹ entries over a multi-year lifetime, on a single-host SSD, when you cannot afford to rebuild it?** All surveyed systems either (a) re-tune bloom on compaction/segment turnover (DD, ZFS), (b) sidestep with per-segment bloom sets that grow incrementally (Borg's segments, restic's pack-indexes), or (c) accept rising FP rate and rely on the authoritative tier (RocksDB per-SSTable bloom). **For a single growing on-disk authoritative index without natural segment boundaries, the bloom-resize problem is genuinely unsolved in published prior art.** SliceFS likely needs to introduce its own segment boundaries (e.g., periodic "epoch" rollover) to make bloom growth tractable — borrowing the LSM playbook even if the authoritative store isn't itself an LSM.
