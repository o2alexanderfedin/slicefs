# Feature Research

**Domain:** Deduplicating POSIX FUSE Filesystem (DedupFS)
**Researched:** 2026-03-27
**Confidence:** MEDIUM-HIGH (core POSIX requirements HIGH; dedup nuances MEDIUM based on ZFS/Btrfs documentation and academic research)

---

## Feature Landscape

### Table Stakes (Users Expect These)

Features users assume exist in any daily-driver POSIX filesystem. Missing any of these means the filesystem cannot be used for real work — tools will break, data will be lost, or users will hit walls immediately.

| Feature | Why Expected | Complexity | Notes |
|---------|--------------|------------|-------|
| read/write/create/delete files | Fundamental POSIX | LOW | fuser trait implementation; baseline for everything else |
| Directory create/delete/list (readdir) | Fundamental POSIX | LOW | Must include `.` and `..` entries correctly |
| Atomic rename (rename(2)) | POSIX requires rename atomicity; mv, editors, package managers depend on it | MEDIUM | Critical: editors do rename-on-save; must be truly atomic across the virtual namespace; COW makes this non-trivial |
| Symbolic links (symlink/readlink) | Universally expected; package managers, toolchains, dotfiles use them | MEDIUM | Must preserve target path verbatim; no content-dedup of link targets |
| Hard links (link(2)) | Expected by build systems (make, ninja), inode-based tools | MEDIUM | Challenging with CAS: hard links share one inode but two filesystem paths — reference counting must reflect this |
| File permissions (chmod/chown, uid/gid) | Any multi-user or permission-sensitive workload | MEDIUM | Must store independently from block content in metadata layer |
| Timestamps (atime, mtime, ctime) | POSIX-mandated; make, rsync, and many tools depend on mtime | MEDIUM | atime updates on read cause dedup inefficiency; support `noatime` mount option |
| Extended attributes (xattr) | macOS Finder metadata, SELinux labels, ACLs stored here | MEDIUM | Not in POSIX.1 but universally required; needed for macOS compatibility |
| Truncate (truncate/ftruncate) | Editors, databases, log rotation depend on it | MEDIUM | Must update metadata and handle partial block reference counting |
| stat/fstat/lstat | Every tool uses these to check file existence, size, type | LOW | Must return consistent values; inode numbers must be stable |
| Stable inode numbers | Tools cache inode numbers; stale inodes break NFS, watch APIs, build tools | HIGH | CAS blocks have content-addresses, but inodes are metadata-layer identifiers — they must be stable across mount cycles |
| fsync/fdatasync | Databases, editors, package managers call fsync to guarantee durability | HIGH | Must guarantee data is committed to backing store before returning; FUSE writeback cache makes this subtle |
| POSIX locking (fcntl locks, flock) | Databases, editors (vim .swp files), package managers use locking | HIGH | FUSE passes these through but the filesystem must handle them; incorrect behavior corrupts data |
| Correct error codes (errno) | POSIX specifies which errors mean what; tools parse errno | MEDIUM | Wrong errno causes silent failures; e.g. ENOSPC vs EIO vs EROFS vs ENOENT |
| Space reporting (statfs) | df, du, editors checking free space | MEDIUM | Dedup complicates this: logical size vs physical size must be reported correctly and consistently |
| Mount/unmount cleanly | Crash recovery, data integrity on unexpected unmount | HIGH | Must handle SIGTERM gracefully; flush all pending writes; block count consistency |
| Block-level CAS deduplication | Core value proposition — the reason DedupFS exists | HIGH | Hash-based dedup; reference counting per block; pluggable hash function |
| Integrity verification on read | Silent data corruption is worse than visible errors; CAS makes this natural | MEDIUM | Re-hash block on read, compare to stored hash; configurable (on/off for performance) |
| Garbage collection of orphan blocks | Blocks with zero references must be reclaimed | HIGH | Two-phase mark-and-sweep or reference-count-based; must be crash-safe; see pitfalls |
| CLI mount/unmount tool | Users need a way to mount and unmount; no GUI planned | LOW | Thin wrapper around FUSE mount; accepts options for backing store path, hash algorithm |
| Basic stats CLI (dedup ratio, physical vs logical size) | Users need to see dedup is working | MEDIUM | Reports: logical bytes written, physical bytes stored, dedup ratio, block count, reference distribution |

---

### Differentiators (Competitive Advantage)

Features that go beyond what every filesystem provides. These are where DedupFS competes and justifies its existence over simpler solutions.

| Feature | Value Proposition | Complexity | Notes |
|---------|-------------------|------------|-------|
| Pluggable hash function | Future-proof against hash deprecation (SHA-256 today, BLAKE3 tomorrow); enables owner's existing tech integration | MEDIUM | Trait abstraction: `trait ContentHash { fn hash(data: &[u8]) -> Hash; }` — selected at pool creation time, stored in pool metadata |
| Pluggable chunking/block-splitting strategy | Owner has existing algorithm; pluggability enables content-defined chunking (CDC) for higher dedup ratios on variable-content files | HIGH | CDC (e.g. Rabin, FastCDC) dramatically outperforms fixed-size chunking on real-world data |
| Pluggable storage backend | Enables future distributed/decentralized backends (S3, IPFS, custom P2P) without rewriting core | HIGH | Trait abstraction: `trait BlockStore { fn get(hash) -> Block; fn put(hash, block); fn delete(hash); }` |
| Cross-dedup across all files in the pool | Unlike file-level tools, every file in the pool shares the block namespace — a block stored once by any file is deduplicated for all files | MEDIUM | Natural consequence of pool-wide CAS; must be preserved in architecture |
| Transparent operation (no workflow change) | Users treat it like any other filesystem; no special commands to trigger dedup | LOW | FUSE transparency is the key — dedup happens invisibly in the write path |
| Dedup-aware space reporting | Users see both logical (what they wrote) and physical (what's stored) sizes | MEDIUM | `statfs` returns physical; CLI tool reports logical+physical+ratio; crucial for user trust |
| Content-defined chunking (CDC) for binary files | Variable-size chunks following content boundaries survive insertions/deletions — much higher dedup ratios for real data | HIGH | Requires pluggable chunker; FastCDC is a strong starting algorithm |
| Snapshots (read-only point-in-time views) | Safe backups without copying data; rollback capability; space-efficient due to shared blocks | HIGH | Requires metadata versioning: snapshot = frozen pointer to root metadata tree; new writes go to new metadata without touching snapshot |
| Compression of stored blocks | Stacks with dedup: compress after dedup; further reduces physical storage | MEDIUM | Best as a pluggable wrapper around the block store; LZ4 for speed, Zstd for ratio; must apply before hashing or after (design decision — see pitfalls) |
| Encryption at rest | Stores sensitive data securely; relevant for cloud/remote backends later | HIGH | AES-256-GCM per-block; key derived from passphrase via Argon2/scrypt; must encrypt before storing, decrypt on read; note: encrypt-then-dedup or dedup-then-encrypt are different designs — see anti-features |
| Mount options for performance tuning | Power users need noatime, writeback cache, read-ahead tuning | MEDIUM | `noatime` (skip atime updates), `sync` vs `async` write modes, cache size controls |
| Integrity scrub command | Proactive corruption detection: walk all blocks, re-verify hashes | MEDIUM | CLI command: `dedupfs scrub <mountpoint>` — reports corrupted blocks; doesn't repair (no redundancy in v1) |

---

### Anti-Features (Commonly Requested, Often Problematic)

| Feature | Why Requested | Why Problematic | Alternative |
|---------|---------------|-----------------|-------------|
| Encrypt-before-dedup | Security-conscious users want everything encrypted, including on write path | Encryption destroys block content patterns — encrypted blocks of identical data produce different ciphertext; dedup ratio drops to zero. You get the cost of both with the benefit of neither. | Dedup-then-encrypt: deduplicate blocks first (in memory or on a temporary local store), then encrypt the deduplicated block store at rest. This preserves dedup ratio while securing stored blocks. |
| Inline ZFS-style dedup with DDT in memory | ZFS does this; seems natural | ZFS requires ~320 bytes RAM per unique block. A 4 TB pool at 4 KB blocks = 1 billion potential blocks = 320 GB DDT. This is why ZFS dedup is infamously memory-hungry. | Keep the DDT on disk with an in-memory cache (LRU/ARC). OpenZFS Fast Dedup (2.3.0) solved this same problem the same way. Accept slightly higher lookup latency for dramatically lower RAM requirements. |
| Online defragmentation | Users expect it from HDD-era wisdom | CAS-based storage with content-addressable blocks has no concept of physical adjacency meaningful to defrag. Blocks are stored by hash; adjacency is irrelevant. Defrag would be a no-op or counterproductive. | Compact/repack command: coalesce small blocks, rewrite pack files for sequential access. Different from defrag — this is about pack file layout, not block adjacency. |
| Per-file dedup ratio tracking | Users want to know "how much space did THIS file save?" | Blocks are shared across files. A block deduplicated because File A already stored it doesn't "belong" to File A or File B — it's a pool resource. Per-file attribution is misleading and requires expensive reverse-mapping. | Report pool-level dedup ratio (total logical / total physical). Optionally report per-file logical size vs "estimated physical contribution" with a clear disclaimer that it's approximate. |
| Distributed filesystem in v1 | Natural extension of the architecture | Adding distribution before the local filesystem is solid guarantees you'll be debugging distributed consistency bugs on top of unfinished local semantics. The pluggable backend architecture defers this correctly. | Implement clean `BlockStore` and `MetadataStore` traits now. Distributed backends plug in later without touching filesystem core. |
| Journaling/WAL like ext4 | Crash safety is expected | FUSE filesystems can't journal at the kernel level. Attempting to replicate ext4's journal in userspace creates complexity without the kernel's atomicity guarantees. | Use atomic metadata updates: write new metadata version, fsync, then atomically update the root pointer (e.g., rename-based commit). CAS blocks are inherently safe to re-read; only metadata root needs atomic commit. |
| POSIX atime updates | POSIX requires atime; some tools depend on it | atime requires a metadata write on every read. For a dedup filesystem, this means every read potentially triggers a block reference or metadata update, serializing reads. | Default to `noatime` (like most modern Linux deployments). Expose `atime`/`relatime` as mount options for users who need them. Document the default clearly. |
| GUI management interface | Nice for non-technical users | Out of scope per PROJECT.md; adds platform-specific complexity; CLI-first is the right call for a filesystem tool | Provide structured JSON output from CLI stats commands so third-party GUIs can be built without first-party involvement |

---

## Feature Dependencies

```
[POSIX read/write/create/delete]
    └──requires──> [Stable inode numbers]
    └──requires──> [Metadata layer (inodes, directory entries)]

[Block-level CAS deduplication]
    └──requires──> [Pluggable hash function]
    └──requires──> [Pluggable chunking strategy]
    └──requires──> [Block store (local)]
    └──requires──> [Reference counting per block]
        └──requires──> [Garbage collection]

[Atomic rename]
    └──requires──> [Metadata layer atomic commits]

[Hard links]
    └──requires──> [Reference counting at inode level (separate from block refcounts)]

[Snapshots]
    └──requires──> [COW metadata layer]
    └──requires──> [Block-level CAS deduplication] (blocks are shared across snapshots naturally)
    └──enhances──> [Garbage collection] (snapshots pin blocks; GC must respect snapshot refs)

[Compression]
    └──enhances──> [Block-level CAS deduplication] (stack: dedup first, then compress)
    └──conflicts──> [Encrypt-before-dedup] (cannot dedup after encryption; design choice must be made early)

[Encryption at rest]
    └──requires──> [Block-level CAS deduplication] (must dedup before encrypting for ratio preservation)
    └──conflicts──> [Encrypt-before-dedup anti-feature]

[Integrity verification on read]
    └──requires──> [Block-level CAS deduplication] (hashes are already computed; re-verification is free)

[Integrity scrub command]
    └──requires──> [Integrity verification on read] (same mechanism, applied proactively to all blocks)

[Pluggable storage backend]
    └──enhances──> [Block store (local)] (local is the first concrete implementation)
    └──enables──> [Distributed backends (future)]

[Basic stats CLI]
    └──requires──> [Dedup-aware space reporting (statfs)]
    └──requires──> [Block reference count metadata]

[Snapshots]
    └──enhances──> [Basic stats CLI] (snapshots consume physical space; stats must account for them)

[fsync/fdatasync]
    └──requires──> [Metadata layer atomic commits]
    └──conflicts──> [FUSE writeback cache] (writeback cache delays flushes; fsync must force flush through cache)
```

### Dependency Notes

- **CAS deduplication requires reference counting:** Every block needs a refcount that's atomically updated on write (increment) and unlink/truncate/overwrite (decrement). Zero-refcount blocks are garbage.
- **GC requires snapshot awareness:** If snapshots are implemented, GC must treat snapshot root pointers as GC roots — blocks reachable from any snapshot are live, not orphaned.
- **Compression-before-hash vs hash-before-compression is a one-time design decision:** If you compress then hash, the hash identifies the compressed form. If you hash then compress, the hash identifies the original. For dedup integrity, hash the original; store compressed. This means: hash(original_block) → store compress(original_block). Re-verification decompresses then rehashes.
- **Encryption placement:** Must be dedup-first, then encrypt stored blocks. Encrypting inputs before hashing kills dedup ratio entirely.
- **Pluggable chunking owns block boundaries:** The chunker determines what a "block" is before hashing. Fixed-size is simplest; CDC (content-defined chunking) gives much higher real-world dedup ratios but is more complex.

---

## MVP Definition

### Launch With (v1)

Minimum viable for a daily-driver deduplicating filesystem. Everything here is load-bearing.

- [ ] Full POSIX read/write/create/delete/stat/rename/symlink — without these, real tools break immediately
- [ ] Stable inode numbers across mount cycles — build systems and editors depend on this
- [ ] Hard links — required by package managers (Homebrew, apt) and build tools
- [ ] Extended attributes (xattr) — required on macOS (Finder metadata); needed for basic usability
- [ ] Block-level CAS deduplication with pluggable hash function (default: SHA-256 or BLAKE3) — core value proposition
- [ ] Pluggable chunking interface (fixed-size as first implementation, owner's algorithm as second) — required by architecture
- [ ] Local block store (file-based or RocksDB-backed) — pluggable via trait; first concrete implementation
- [ ] Reference counting per block with crash-safe GC — without this, blocks leak and the filesystem fills up
- [ ] Atomic metadata commits (rename-based or WAL-lite) — required for fsync correctness and crash safety
- [ ] fsync/fdatasync correctness — databases and editors depend on this; getting it wrong silently corrupts data
- [ ] Correct statfs (physical vs logical size) — users need to see how full the disk is
- [ ] CLI: mount, unmount, stats (dedup ratio, logical/physical bytes, block count)
- [ ] pjdfstest pass rate target: >95% — measures POSIX correctness objectively
- [ ] Integrity verification on read (configurable on/off) — natural with CAS; builds user trust

### Add After Validation (v1.x)

Add once the core is proven correct and stable under real workloads.

- [ ] Content-defined chunking (CDC, e.g. FastCDC) — trigger: users report poor dedup ratio on their actual data
- [ ] Compression of stored blocks (LZ4 or Zstd) — trigger: users want further space reduction beyond dedup
- [ ] noatime mount option — trigger: performance profiling shows atime updates are a bottleneck
- [ ] Integrity scrub command — trigger: first report of silent corruption concern or storage media failure
- [ ] Snapshot support (read-only point-in-time) — trigger: users ask for backup-safe snapshots; requires COW metadata first
- [ ] Structured JSON output from CLI stats — trigger: third-party tooling interest

### Future Consideration (v2+)

Defer until product-market fit is established and local foundation is solid.

- [ ] Encryption at rest — deferred because it requires finalizing dedup-then-encrypt architecture; adds key management complexity
- [ ] Pluggable remote/distributed storage backend — deferred per PROJECT.md; requires clean backend trait established in v1
- [ ] Cross-machine dedup — requires distributed block store; natural extension once remote backend exists
- [ ] Snapshot writeable clones (branch-on-write) — complex metadata management; v1 read-only snapshots are sufficient
- [ ] Quota management (per-directory or per-user space limits) — complex with shared blocks; dedup complicates attribution
- [ ] Online compaction/repack — useful for long-running pools; not needed until pool fragmentation is observed

---

## Feature Prioritization Matrix

| Feature | User Value | Implementation Cost | Priority |
|---------|------------|---------------------|----------|
| POSIX read/write/create/delete/stat | HIGH | LOW | P1 |
| Atomic rename | HIGH | MEDIUM | P1 |
| Stable inode numbers | HIGH | MEDIUM | P1 |
| Hard links | HIGH | MEDIUM | P1 |
| Extended attributes (xattr) | HIGH | MEDIUM | P1 |
| fsync/fdatasync correctness | HIGH | HIGH | P1 |
| Block-level CAS deduplication | HIGH | HIGH | P1 |
| Pluggable hash function | HIGH | MEDIUM | P1 |
| Pluggable chunking interface | HIGH | MEDIUM | P1 |
| Local block store (pluggable trait) | HIGH | MEDIUM | P1 |
| Reference counting + crash-safe GC | HIGH | HIGH | P1 |
| Atomic metadata commits | HIGH | HIGH | P1 |
| Correct statfs reporting | HIGH | MEDIUM | P1 |
| Integrity verification on read | HIGH | LOW | P1 |
| CLI: mount/unmount/stats | HIGH | LOW | P1 |
| Symbolic links | MEDIUM | MEDIUM | P1 |
| Content-defined chunking (CDC) | HIGH | HIGH | P2 |
| Compression (LZ4/Zstd) | MEDIUM | MEDIUM | P2 |
| noatime mount option | MEDIUM | LOW | P2 |
| Integrity scrub command | MEDIUM | MEDIUM | P2 |
| Snapshots (read-only) | HIGH | HIGH | P2 |
| Encryption at rest | MEDIUM | HIGH | P3 |
| Distributed storage backend | HIGH | HIGH | P3 |
| Snapshot writeable clones | MEDIUM | HIGH | P3 |
| Quota management | LOW | HIGH | P3 |

**Priority key:**
- P1: Must have for launch (v1)
- P2: Should have, add after core validation (v1.x)
- P3: Nice to have, future milestone (v2+)

---

## Competitor Feature Analysis

| Feature | ZFS | Btrfs | Borg/Restic | DedupFS Approach |
|---------|-----|-------|-------------|-----------------|
| Deduplication | Inline (block-level); memory-hungry DDT; Fast Dedup in 2.3.0 improves RAM | Offline only (ioctl_fideduperange); no inline | Chunk-based; post-process; repository-scoped | Inline, block-level CAS; on-disk DDT with LRU cache; pool-scoped |
| Chunking | Fixed-size blocks | Fixed-size blocks | Content-defined (Rabin/Buzhash) | Pluggable; fixed-size first, CDC second |
| Snapshots | COW, instant, space-efficient | COW, instant, space-efficient | Not a filesystem; repository snapshots | v1.x roadmap; COW metadata required first |
| Compression | LZ4/Zstd/etc per dataset | LZ4/Zstd/etc per subvolume | LZ4/Zstd/etc per repo | v1.x; dedup-first then compress stored blocks |
| Encryption | Native (AES-256-GCM, ZFS 0.8+) | Not native (use dm-crypt/LUKS) | AES-256-CTR+Poly1305 (Borg); AES-GCM (Restic) | v2+; dedup-then-encrypt; pluggable cipher |
| POSIX compliance | Full (native kernel filesystem) | Full (native kernel filesystem) | Not POSIX (archive/backup tool) | Target: >95% pjdfstest; FUSE means some syscalls unavailable |
| Cross-file dedup | Yes (pool-wide) | Yes (with manual ioctl) | Yes (repo-wide) | Yes (pool-wide CAS namespace) |
| Storage backend | ZPool (local block devices) | Local block devices | Local, SSH | Pluggable trait; local first; remote later |
| RAM requirements | High (DDT in memory by default) | Low | Low | Low (on-disk DDT; tunable cache size) |
| Daily-driver use | Yes | Yes | No (backup tool) | Yes (core design goal) |
| FUSE overhead | No (kernel native) | No (kernel native) | N/A | Yes; ~5-15% overhead vs kernel FS; accept the tradeoff for cross-platform and rapid development |

---

## Sources

- [Deduplication — Btrfs Wiki](https://btrfs.wiki.kernel.org/index.php/Deduplication)
- [Btrfs vs. ZFS Comparison 2026 — Wundertech](https://www.wundertech.net/btrfs-vs-zfs-comparison/)
- [Introducing OpenZFS Fast Dedup — Klara Systems](https://klarasystems.com/articles/introducing-openzfs-fast-dedup/)
- [OpenZFS dedup is good now and you shouldn't use it — despairlabs](https://despairlabs.com/blog/posts/2024-10-27-openzfs-dedup-is-good-dont-use-it/)
- [ZFS Deduplication — TrueNAS Documentation Hub](https://www.truenas.com/docs/references/zfsdeduplication/)
- [The Logic of Physical Garbage Collection in Deduplicating Storage — USENIX FAST 2017](https://www.usenix.org/system/files/conference/fast17/fast17-douglis.pdf)
- [Performance and Resource Utilization of FUSE User-Space File Systems — ACM](https://dl.acm.org/doi/fullHtml/10.1145/3310148)
- [To FUSE or Not to FUSE: Performance of User-Space File Systems — USENIX FAST 2017](https://www.usenix.org/system/files/conference/fast17/fast17-vangoor.pdf)
- [DFUSE: Strongly Consistent Write-Back Kernel Caching — arXiv 2025](https://arxiv.org/html/2503.18191v1)
- [Inline deduplication vs post-processing — TechTarget](https://www.techtarget.com/searchdatabackup/tutorial/Inline-deduplication-vs-post-processing-Data-dedupe-best-practices)
- [pjdfstest — POSIX filesystem test suite](https://github.com/saidsay-so/pjdfstest)
- [POSIX Compatibility comparison — JuiceFS Blog](https://juicefs.com/en/blog/engineering/posix-compatibility-comparison-among-four-file-system-on-the-cloud)
- [gocryptfs FUSE encryption — ArchWiki](https://wiki.archlinux.org/title/Gocryptfs)
- [Copy-on-write — Wikipedia](https://en.wikipedia.org/wiki/Copy-on-write)
- [Deduplication Garbage Collection Overview — Microsoft TechNet](https://social.technet.microsoft.com/wiki/contents/articles/31178.deduplication-garbage-collection-overview.aspx)
- [What is a POSIX File System? — Quobyte](https://www.quobyte.com/storage-explained/posix-filesystem/)
- [Extended file attributes — Wikipedia](https://en.wikipedia.org/wiki/Extended_file_attributes)

---
*Feature research for: Deduplicating POSIX FUSE Filesystem (DedupFS)*
*Researched: 2026-03-27*
