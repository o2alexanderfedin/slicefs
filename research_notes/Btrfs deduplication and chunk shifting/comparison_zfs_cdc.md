# Dedup in other storage systems (ZFS, VDO, Windows Server, CDC backup tools, research FUSE filesystems), compared with Btrfs and SliceFS

Scope: for each system: in-band vs out-of-band, chunking (fixed vs CDC), granularity, whether dedup survives a byte insertion or shift, and memory/IO costs. Researched 2026-11-12. Items marked "(search snippet, page not opened)" come from search-result summaries only and need verification.

## ZFS dedup: classic DDT and OpenZFS 2.3 "fast dedup". Does it survive a 1-byte insertion?

### Takeaway
ZFS dedup is inline, block-level and keyed on whole fixed-size records (recordsize or volblocksize). The DDT entry costs about 320 bytes of RAM when cached. OpenZFS 2.3 "fast dedup" changes how the DDT is updated and bounded (log, prefetch, quota, prune). It does not change chunking. Records stay fixed-offset, so an insertion that shifts file content defeats dedup for all following records of that file.

### Cited Findings
- Each pool has one global DDT, shared by all dedup-enabled datasets and zvols. It is an on-disk hash table that uses extensible hashing from the ZAP, and it is cached in ARC. — [OpenZFS Workload Tuning](https://openzfs.github.io/openzfs-docs/Performance%20and%20Tuning/Workload%20Tuning.html)
- "Each cached entry uses slightly more than 320 bytes of memory." To size it, multiply the entry count from `zdb -S` (a simulated DDT) by about 320 bytes. — [OpenZFS Workload Tuning](https://openzfs.github.io/openzfs-docs/Performance%20and%20Tuning/Workload%20Tuning.html)
- Each DDT entry is one unique block. The block size is recordsize for datasets (default 128 KiB, any power of 2 from 512 B to 1 MiB, up to 16 MiB with large_blocks) or volblocksize for zvols. Upper-bound estimate: storage / recordsize x 320 B. "Partial records each count as a full recordsize for the purposes of deduplication." — [OpenZFS Workload Tuning](https://openzfs.github.io/openzfs-docs/Performance%20and%20Tuning/Workload%20Tuning.html)
- The DDT is consulted for every dedup-able block written or freed. If the DDT does not fit in RAM, each miss is a random read. The doc's example: one 7200 RPM disk at about 100 IOPS limits writes to about 100 blocks/s (400 KB/s with 4 KB blocks). — [OpenZFS Workload Tuning](https://openzfs.github.io/openzfs-docs/Performance%20and%20Tuning/Workload%20Tuning.html)
- TrueNAS docs: DDT entries are "about 300-900 bytes in size". Pools with ratios of 3x or more "might only need 1-3 GB of RAM per 1 TB of data". — [TrueNAS ZFS Deduplication](https://truenas.com/docs/references/zfsdeduplication)
- OpenZFS 2.3.0 release notes call Fast Dedup "a major performance upgrade to the original OpenZFS deduplication functionality". They add `zfs_dedup_log_*` module parameters (flush and memory limits). Published 2025-01-14 (GitHub API `published_at: 2025-01-14T00:35:09Z`). — [OpenZFS 2.3.0 release](https://github.com/openzfs/zfs/releases/tag/zfs-2.3.0)
- `zfs_dedup_log_*` parameters include `zfs_dedup_log_cap`, `_flush_entries_max/min`, `_flush_txgs`, `_mem_max`, `_mem_max_percent`, `_txg_max`, `_hard_cap`. — [OpenZFS Module Parameters](https://openzfs.github.io/openzfs-docs/Performance%20and%20Tuning/Module%20Parameters.html)
- Fast dedup "temporarily writes [entries] into a log, flushing it into actual DDT ZAP only after sorting". Batching lets several entry updates share one ZAP leaf write. Prefetch loads the DDT into ARC; without it, loading a large DDT after import "can take days". Over quota, "the entry is not created and the corresponding write is converted to a regular non-dedup write." — [TrueNAS ZFS Deduplication](https://truenas.com/docs/references/zfsdeduplication)
- `dedup_table_quota` "sets a limit on the on-disk size of the pool's dedup table". In the default auto mode, the size of a dedicated dedup vdev is the limit. It "works for both legacy and fast dedup tables". — [zpoolprops(7)](https://openzfs.github.io/openzfs-docs/man/master/7/zpoolprops.7.html)
- `zpool ddtprune` "prunes older unique entries from the dedup table" to make room for newer duplicate entries: `-d days` (by age) or `-p percentage`. Unique entries written with copies > 1 are not pruned. — [zpool-ddtprune(8)](https://openzfs.github.io/openzfs-docs/man/master/8/zpool-ddtprune.8.html)
- Fast dedup was developed by Klara (led by Allan Jude) with iXsystems. It was donated to OpenZFS in February 2024 and shipped in 2.3 (search snippet, page not opened). — [TrueNAS blog](https://www.truenas.com/blog/fast-dedup-is-a-valentines-gift-to-the-openzfs-and-truenas-communities); [Klara article](https://klarasystems.com/articles/how-klara-and-truenas-collaborated-to-fix-one-of-zfss-longest-standing-limitations/)
- Vendor claim: TrueNAS quotes "5X the usable capacity and 20X the performance" (search snippet, page not opened; vendor source). — [TrueNAS blog](https://www.truenas.com/blog/fast-dedup-is-a-valentines-gift-to-the-openzfs-and-truenas-communities)
- General boundary-shift statement: with fixed-size chunking, "if one or several bytes are inserted at the beginning of a file, all current chunk cut-points ... will be shifted and no duplicate chunks will be detected." — [FastCDC, USENIX ATC'16, §2](https://www.usenix.org/conference/atc16/technical-sessions/presentation/xia)

### Inferences
- ZFS keys dedup on whole records at fixed file offsets (k x recordsize), so a 1-byte insertion at file start changes every following record. None of them will match the DDT. Only the records before the insertion point still dedup. This follows from the per-record DDT design plus the FastCDC FSC statement. I found no ZFS doc that states the 1-byte case directly.
- Fast dedup lowers the IO and RAM cost of the DDT (sorted, batched log; bounded size; pruning of singletons). It does not improve dedup ratio on shifted data. The quota and prune features even trade ratio for bounded cost.
- Compared with SliceFS: SliceFS dedups in-band, the same placement as ZFS. CORRECTION (verified in code after this note was written): the README line saying the default `Chunker` is "fixed-size" was stale and has been fixed. The live write path (`crates/slicefs-cli/src/filesystem.rs`, `seed.rs`) uses `blockset::State::push_all`, which is content-defined, so SliceFS does not have ZFS's 1-byte-insertion weakness by design. `FixedChunker` in `cas-local` is unused test code.

### Gaps
- Whether the fast-dedup on-disk entry format is smaller than the classic 320 B in-core entry: no primary source found.
- Whether ZFS 2.4 changed dedup further: Klara notes mention "Fast Dedup Log pacing" (search snippet only).
- Independent (non-vendor) benchmarks of fast dedup vs legacy dedup: none verified.

## Windows Server Data Deduplication: an example of filesystem-level CDC

### Takeaway
Windows Server Data Deduplication on NTFS (and ReFS in later versions, not verified here) is post-process. Files are written normally. Then a scheduled Optimization job splits them into variable-size, content-defined chunks, stores unique chunks in a chunk store, and replaces the file stream with a reparse point that Dedup.sys resolves on read. The design paper gives 32-128 KB chunks, a Rabin-based chunker with "regression chunking", and about 6 bytes of RAM per chunk for the index.

### Cited Findings
- Design principle 1: "Optimization should not get in the way of writes to the disk". Data Deduplication "optimizes data by using a post-processing model". — [MS Learn: Understanding Data Deduplication](https://learn.microsoft.com/en-us/windows-server/storage/data-deduplication/understand)
- Steps: scan for files meeting the policy, "Break files into variable-size chunks", identify unique chunks, place them in the chunk store (optionally compressed), and replace the file stream with a reparse point. Reads go through the Dedup.sys filter. Modifications are "written unoptimized" and re-optimized at the next job run. — [MS Learn](https://learn.microsoft.com/en-us/windows-server/storage/data-deduplication/understand)
- Jobs: Optimization (hourly by default), Garbage Collection (weekly), Integrity Scrubbing (weekly; keeps extra copies of chunks referenced more than 100 times in a "hotspot"), Unoptimization (manual). Default policy: minimum file age 3 days for the general-purpose usage type. — [MS Learn](https://learn.microsoft.com/en-us/windows-server/storage/data-deduplication/understand)
- The chunk store is "an organized series of container files in the System Volume Information folder". — [MS Learn](https://learn.microsoft.com/en-us/windows-server/storage/data-deduplication/understand)
- Design paper (El-Shimi et al., "Primary Data Deduplication - Large Scale Study and System Design", USENIX ATC'12): "an average chunk size of about 80KB (and size range 32-128KB) with compression yields savings comparable to 4KB average chunk size with compression." — [USENIX ATC'12 paper PDF](https://www.usenix.org/system/files/conference/atc12/atc12-final293.pdf)
- The chunker uses a Rabin-fingerprint sliding-window hash with a 16-bit pattern, "giving an average chunk size of Savg = 64KB". It adds "regression chunking" to avoid forced cuts at the 128 KB maximum. The resulting average is 80 KB on GFS-US. Regression chunking adds about 3% savings overall and about 27% more for PDF files. — [ATC'12 paper](https://www.usenix.org/system/files/conference/atc12/atc12-final293.pdf)
- Fixed vs variable, from the paper: "We also found reduced space savings for chunking with 64KB fixed size blocks when comparing it with ~64KB variable size chunking ... because chunking with fixed size blocks does not support the identification of duplicate data when its duplicate occurrence is not aligned along block boundaries." Also, "whole file and sub-file fixed-size chunk deduplication were significantly inferior to sub-file variable size chunk deduplication." — [ATC'12 paper](https://www.usenix.org/system/files/conference/atc12/atc12-final293.pdf)
- Rationale for post-process over inline: inline "introduces additional write latencies and reduces write throughput". On file servers, most files are not accessed after 24 hours. — [ATC'12 paper](https://www.usenix.org/system/files/conference/atc12/atc12-final293.pdf)
- Index: a "low RAM footprint" chunk hash index "that uses about 6 bytes of RAM for every" chunk. — [ATC'12 paper](https://www.usenix.org/system/files/conference/atc12/atc12-final293.pdf)
- Background: it replaced Single Instance Store, which only deduplicated whole identical files. SIS was deprecated in 2012 R2 and removed in Windows Server 2016. — [MS Learn](https://learn.microsoft.com/en-us/windows-server/storage/data-deduplication/understand)

### Inferences
- Windows Dedup is the closest shipped production analogue to SliceFS's CDC approach, but it is out-of-band. Recently written or modified data stays undeduplicated until the next job. SliceFS dedups on write (in-band, per its [README](../../README.md)), so it pays chunking and index cost on the write path. That is exactly the cost Microsoft avoided by choosing post-processing.
- The paper's finding that 80 KB chunks with compression roughly match 4 KB chunks with compression is relevant to SliceFS chunk-size tuning. Larger CDC chunks plus compression can keep savings while cutting index size by about 20x.

### Gaps
- The paper is from 2012. I did not check whether later Windows Server versions (2019, 2022, 2025) changed chunk parameters. MS Learn docs do not state the chunk size range.

## Linux VDO (dm-vdo) and other fixed-block inline systems (HAMMER2, lessfs, SDFS)

### Takeaway
dm-vdo was merged in Linux 6.9 (released 12 May 2024). It is an inline, 4 KiB fixed-block dedup and compression device-mapper target with a UDS hash index. Like ZFS, it cannot survive byte shifts that are not multiples of 4 KiB. HAMMER2 reportedly has only "live" (write-path, cache-based) dedup. lessfs is a fixed-block inline FUSE filesystem and appears dormant. SDFS/OpenDedup supports both fixed and variable blocks; its GitHub repo was last pushed in July 2023. bcachefs has no dedup.

### Cited Findings
- dm-vdo "provides inline deduplication, compression, zero-block elimination, and thin provisioning". It has "a maximum block size of 4K" and accepts writes as small as 512 B. — [kernel docs: vdo-design](https://docs.kernel.org/admin-guide/device-mapper/vdo-design.html)
- Each block is hashed to a 16-byte name, and VDO "does not use a cryptographic hash". The UDS index default "can hold 64 million records, corresponding to about 256GB of data" (the dedup window). The volume index is held entirely in memory. Doubling the index doubles window, storage and memory. A sparse index gives 10x the window at the same memory and "will detect 97-99% of the deduplication that a standard index will detect". — [kernel docs: vdo-design](https://docs.kernel.org/admin-guide/device-mapper/vdo-design.html)
- First released in 2013, open-sourced in 2017. — [kernel docs: vdo-design](https://docs.kernel.org/admin-guide/device-mapper/vdo-design.html)
- Merged for Linux 6.9 via the `for-6.9/dm-vdo` tag. — [Phoronix](https://www.phoronix.com/news/DM-VDO-Merged-Linux-6.9); [KernelNewbies Linux 6.9](https://kernelnewbies.org/Linux_6.9) (released 12 May 2024)
- (search snippet, page not opened; the DragonFly source browser returned an anti-bot block) HAMMER2 "currently implements only live de-duplications". The design doc lists deduper status as "live operational, batch specced". The 2015 commit caches {data_off, crc} of recently accessed blocks. On a write cache hit, the disk block is compared and reused if it matches. — [HAMMER2 DESIGN](https://grok.dragonflybsd.org/xref/dragonfly/sys/vfs/hammer2/DESIGN?r=f984587a); [commit "hammer2 - live dedup"](https://gitweb.dragonflybsd.org/dragonfly.git/commit/3f01ebaaf512c09685f0f7b8bfec3b47bbd1e72a)
- HAMMER2 is the default DragonFly filesystem (search snippet). — [DragonFly HAMMER wiki](https://wiki.dragonflybsd.org/hammer/)
- lessfs: "a high-performance inline data deduplication filesystem written for Linux" (FUSE, GPLv3). It uses a 192-bit Tiger hash per block (search snippet, page not opened). — [Linux Journal: Data Deduplication with Linux](https://www.linuxjournal.com/content/data-deduplication-linux); [SourceForge lessfs](https://sourceforge.net/p/lessfs/)
- One forum comment: with block-level dedup like lessfs, a small change at the start of a large file gives no benefit, because blocks no longer align (search snippet; anecdotal). — [unRAID forum thread](https://forums.unraid.net/topic/5147-lessfs-de-duplication/)
- SDFS (OpenDedup): "SDFS will work either with fixed or variable block sizes" (verified on page). The article's example volume uses 4 KB blocks. The claims that it runs on 64-bit x86 Linux, is GPLv2, and enables variable blocks via hash type `VARIABLE_MURMUR3` come from search snippets and were NOT found in the opened article. — [ADMIN Magazine 2015, SDFS](https://www.admin-magazine.com/Articles/The-SDFS-deduplicating-filesystem)
- SDFS memory (search snippet, page not opened), quoted from OpenDedup docs on a 2012 mailing list: about 2 GB for 200 GB at 4 KB chunks, and 8 GB RAM for 32 TB at 128 KB. — [CentOS list 2012](https://lists.centos.org/pipermail/centos/2012-August/128706.html)

- SDFS repo status: `opendedup/sdfs` last pushed 2023-07-05, not archived (GitHub API, queried 2026-11-12). — [github.com/opendedup/sdfs](https://github.com/opendedup/sdfs)
- bcachefs: official FAQ: "Bcachefs currently does not support data deduplication however compression is supported" (search snippet). One news site claims inline small-file dedup, but it gives no primary source; treat as unreliable. — [bcachefs FAQ](https://bcachefs.org/FAQ/); [conflicting claim: news.lavx.hu](https://news.lavx.hu/article/bcachefs-accelerates-linux-storage-new-features-performance-boosts-and-community-momentum)

### Inferences
- dm-vdo, ZFS, HAMMER2 and lessfs all match on aligned fixed blocks. This works well for VM images and identical copies. It fails on byte-shifted content, the same weakness Btrfs's extent/block-based dedup has.
- lessfs status: I found no activity after roughly 2012-2013 and no official discontinuation notice. Treat it as **dormant/unmaintained** (inference). SDFS/OpenDedup: the repo is not archived, but it has had no pushes since July 2023, so treat it as **likely inactive** (inference).

### Gaps
- No primary source on dm-vdo RAM per TB in the kernel doc. The Red Hat VDO docs (not fetched) have sizing tables.
- Whether bcachefs has concrete dedup plans: only user wish-lists found.
- lessfs last release date: SourceForge page not opened.
- HAMMER2 claims rest on search snippets (the source browser blocked fetching); verify against the DragonFly `sys/vfs/hammer2/DESIGN` file.

## Backup and distribution tools with CDC: restic, borg, casync, Kopia, Duplicacy

### Takeaway
All mainstream modern backup tools use content-defined chunking with large (MiB-scale) chunks. restic uses Rabin, 512 KiB-8 MiB. Kopia uses buzhash, about 4 MiB by default. borg uses buzhash, 512 KiB-8 MiB, about 2 MiB target, with a fixed mode as an option. casync uses buzhash, 64 KiB average. Duplicacy uses 1-16 MiB, 4 MiB average. They are not filesystems, but they show that CDC is the accepted answer to boundary shift.

### Cited Findings
- restic: "uses Rabin Fingerprints for implementing this Content Defined Chunking (CDC)". 64-byte window. Random irreducible polynomial per repo (`chunker_polynomial`). Chunks are 512 KiB-8 MiB, targeting about 1 MiB, identified by SHA-256. Files under 512 KiB are not split. — [restic references](https://restic.readthedocs.io/en/latest/100_references.html)
- borg buzhash defaults: CHUNK_MIN_EXP=19 (512 KiB), CHUNK_MAX_EXP=23 (8 MiB), HASH_MASK_BITS=21 (about 2 MiB target), window 4095 B. The buzhash table is XORed with a per-repo random seed to prevent chunk-size fingerprinting. A `fixed,BLOCK_SIZE[,HEADER_SIZE]` chunker is also available. — [borg internals: data structures](https://borgbackup.readthedocs.io/en/stable/internals/data-structures.html)
- borg memory: about chunk_count x 164 + file_count x 240 bytes. Example: 1 TiB in 1 Mi files needs about 0.31 GiB with defaults vs about 2.8 GiB with a 64 KiB target (`buzhash,10,23,16,4095`). — [borg internals](https://borgbackup.readthedocs.io/en/stable/internals/data-structures.html)
- casync: chunking "is based on ... the buzhash rolling hash function". "By default casync will use a 64K average chunk size". Chunks are named by SHA-256 and compressed with xz. The catar serialization is chunked as one stream, so file boundaries do not force chunk boundaries. — [casync announcement (Lennart Poettering)](https://0pointer.net/blog/casync-a-tool-for-distributing-file-system-images.html)
- Duplicacy: variable chunking with a 4 MiB average, 16 MiB max (4 x average), 1 MiB min (average / 4). Set min = avg = max to get fixed-size chunks. Chunk size cannot be changed after init. The wiki recommends fixed chunks for files updated in place (VM disks, databases, encrypted containers). — [Duplicacy wiki: Chunk Size](https://github.com/gilbertchen/duplicacy/wiki/Chunk-Size) (verified); [Duplicacy forum](https://forum.duplicacy.com/t/chunk-size-details/1082)
- Kopia: `DYNAMIC-4M-BUZHASH` (buzhash CDC, about 4 MiB) is "the splitter used by default for new repositories". It is fixed at repo creation. Fixed (`FIXED-*`) and other splitters exist (search snippet, page not opened). Forum users report mixed results for 1M vs 4M (4 GiB test: 1M better; 775 GB test: 4M gave 90.9 GB vs 94.2 GB). — [kopia splitter Go doc](https://pkg.go.dev/github.com/kopia/kopia@v0.11.3/repo/splitter); [Kopia forum: dedup efficiency](https://kopia.discourse.group/t/deduplication-efficiency/1298)

### Inferences
- These tools pick MiB-scale chunks to keep index and metadata small for whole-repo backups. A filesystem like SliceFS that serves random reads and writes probably needs smaller chunks (Windows used 32-128 KiB, casync 64 KiB), which raises index cost.

### Gaps
- The Kopia splitter docs page returned 404. The default rests on the Go package doc (search snippet).

## Quantitative evidence: fixed-size vs CDC dedup ratio (FastCDC and others)

### Takeaway
FastCDC (Xia et al., USENIX ATC'16) chunks about 10x faster than Rabin CDC and about 3x faster than Gear/AE, with nearly the same dedup ratio. In its comparison, fixed-size chunking (10 KB) has the lowest ratio, and can be catastrophically worse on some backup datasets (for example, about 95% vs about 10% on one dataset). Microsoft's primary-storage study also found 64 KB fixed blocks give clearly lower savings than ~64 KB CDC.

### Cited Findings
- FastCDC abstract: "about 10x faster than the best of open-source Rabin-based CDC, and about 3x faster than the state-of-the-art Gear- and AE-based CDC" while keeping a dedup ratio close to Rabin. Its three techniques are a simplified hash judgment, skipping sub-minimum cut-points, and normalized chunking. — [FastCDC, USENIX ATC'16](https://www.usenix.org/conference/atc16/technical-sessions/presentation/xia); [PDF](https://www.usenix.org/system/files/conference/atc16/atc16-paper-xia.pdf)
- FastCDC on FSC: "if one or several bytes are inserted at the beginning of a file, all current chunk cut-points ... declared by FSC will be shifted and no duplicate chunks will be detected." CDC was "proposed to solve the boundary-shift problem". — [FastCDC PDF §2](https://www.usenix.org/system/files/conference/atc16/atc16-paper-xia.pdf)
- FastCDC Table 4 compared Rabin (RC, min 2 KB), FastCDC without and with normalization (FC, FC-NC min 4/8 KB) and fixed-size XC (10 KB) on 7 datasets: TAR 19 GB, LNX 105 GB (260 Linux source versions), WEB 36 GB, VMA 117 GB (75 VM images), VMB 1.9 TB (125 Ubuntu VM backups), RDB 1.1 TB (200 Redis backups), SYN 1.4 TB (synthetic backups). Text: "FC-NC with Min4KB achieves the highest deduplication ratio among the five approaches while Fixed-Size Chunking (XC) has the lowest". XC "works well on the LNX, WEB, VMB datasets" (small files, structured VM backups). — [FastCDC PDF §4.5](https://www.usenix.org/system/files/conference/atc16/atc16-paper-xia.pdf)
- Table 4 values (dedup ratio = fraction of redundant data removed; RC / FC / FC-NC4 / FC-NC8 / XC). The row-to-dataset mapping is my reconstruction from PDF text extraction (cross-checked with Table 5). It is consistent with the paper's statement that XC does well on LNX, WEB and VMB.
  - TAR: 47.58 / 47.64 / 50.19 / 47.18 / **12.21%**
  - LNX: 97.25 / 97.26 / 97.35 / 97.10 / 96.51%
  - WEB: 95.09 / 94.02 / 95.47 / 94.44 / 93.19%
  - VMA: 38.23 / 37.96 / 40.31 / 38.15 / **18.26%**
  - VMB: 96.13 / 96.09 / 96.24 / 96.11 / 95.68%
  - RDB: 95.53 / 95.50 / 96.71 / 95.70 / **9.80%**
  - SYN: 93.64 / 93.67 / 94.09 / 92.62 / **75.06%**
  — [FastCDC PDF Table 4](https://www.usenix.org/system/files/conference/atc16/atc16-paper-xia.pdf)
- Microsoft primary-data study: 64 KB fixed blocks gave reduced savings vs ~64 KB variable chunking because fixed blocks miss duplicates "not aligned along block boundaries". On GFS-US, chunk-level dedup improved on file-level dedup by figures shown as 2.3x-14.1x in Table 2 (exact mapping not extracted). — [ATC'12 paper](https://www.usenix.org/system/files/conference/atc12/atc12-final293.pdf)
- A 2024 independent survey ("A Thorough Investigation of Content-Defined Chunking Algorithms for Data Deduplication") compares leading CDC algorithms on four datasets (throughput, dedup ratio, chunk-size variance). The abstract gives no numbers. — [arXiv 2409.06066](https://arxiv.org/abs/2409.06066)

### Inferences
- Fixed-size chunking does about as well as CDC on aligned data (VM images, many small files where each file is one chunk). It collapses on streams where content shifts, such as tar archives, database dumps and synthetic edits: TAR 12% vs 47-50%, RDB 10% vs 95-97%. That is where ZFS, VDO and Btrfs block-level dedup (and SliceFS with its default fixed-size chunker) fail, and where a CDC chunker pays off.
- Normalized chunking (FC-NC Min4KB) slightly beats Rabin on ratio and is far faster, so it is a strong default chunker for an in-band FUSE filesystem where CPU is on the write path.

### Gaps
- I found no published head-to-head of ZFS dedup vs a CDC filesystem on the same shifted dataset. The evidence is indirect (FSC vs CDC chunker studies).
- The FastCDC Table 4 row mapping was reconstructed from flattened PDF text. It was cross-checked against Table 5's layout (XC average chunk size: TAR = 10240 B; LNX = 6477 B, which fits the small-file explanation), but not against the rendered PDF.
