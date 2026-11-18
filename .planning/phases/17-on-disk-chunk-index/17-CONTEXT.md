# Phase 17: On-disk chunk index - Context

**Gathered:** 2026-11-18
**Status:** Ready for planning

<domain>
## Phase Boundary

Replace `Inner::index: HashMap<ObjectId, Loc>` in crates/chunk-store/src/lib.rs (line 401) with an LSM-style index: sorted immutable runs of (ObjectId, Loc) on disk, each with a sparse key table and a filter kept in RAM, a memtable for entries since the last flush, and merges in the background. Cold open reads only run footers and filters. Pack records stay the source of truth, and online GC (gc_begin / gc_finish) must never leave an index entry that points to a removed object. The phase ends with the Phase 16 benchmark run against the new index.

</domain>

<decisions>
## Implementation Decisions

### On-disk index layout (approved by the user: "Approve as revised")
- Packs and the per-pack `.idx` files stay unchanged and are the source of truth. `.idx` is not in the lookup path; it is used for rebuild and by `gc_finish`. — **Reversibility:** one-way — on-disk format of the store
- Global immutable sorted runs plus a MANIFEST (list of live runs and the pack high-water mark) in `<store>/index/`. — **Reversibility:** one-way — on-disk format
- Each run entry holds the full 33-byte ObjectId and the full Loc (about 58 B per entry). One 8-byte fence key per 4 KiB block. — **Reversibility:** one-way — on-disk format
- Sort order is (tag, digest). Bloom filters, fences and interpolation use only the digest bytes. — **Reversibility:** one-way — on-disk format
- One blocked Bloom filter per run, about 10 bits per key (about 1% false-positive rate), about 1.4 B per object of RAM. — **Reversibility:** costly — filter format inside run files
- Cold open reads the MANIFEST, the run footers and the filters, then replays only the packs after the high-water mark. — **Reversibility:** costly
### Concurrency and maintenance
- Memtable is sharded by key prefix with one lock per shard. It flushes to a new run only at a size limit. — **Reversibility:** costly
- Readers take an Arc snapshot of the run set. Tiered merges run in the background. Old run files are deleted only after the new MANIFEST is durable. — **Reversibility:** costly
- Online GC writes a tombstone run before it punches or deletes a pack, so no lookup (get or put dedup check) returns a removed object. `gc_finish` works through the per-pack `.idx` files, not an O(N) by_pack map. — **Reversibility:** costly
### Plan order
- The first plan is a spike: fjall versus the custom run design, measured with the Phase 16 benchmark. The result chooses the implementation; the on-disk rules above stay.
- The locality cache is deferred to a follow-up issue.

### Decided autonomously
- [Filter type / dependency] Q: "Ribbon/xor crate (new dep), fastbloom (already in workspace), or hand-written Bloom?" → Selected: "A hand-written blocked Bloom filter, about 10 bits per key (about 1% false positives, 1.25 B/object), saved in the run file. The keys are already uniform BLAKE3 digests, so the probe positions come straight from digest bytes (for example h1 = digest[0..8], h2 = digest[8..16], Kirsch-Mitzenmacher) with no hashing. No new crate. fastbloom (workspace dep used only by crates/cas-local/src/mem_dedup_index.rs) is not used for this: its bit layout and hashing belong to the crate and can change between versions, so its bits are not safe to persist. Ribbon would save about 30% of the filter RAM but needs a new crate or a lot of hand-written code. Bloom already meets the 'few bytes per object' target." — The filter sits behind one small trait and has a type byte in the run footer. With no store backward compatibility, a later change to ribbon is a version bump.
- [Sparse key table] Q: "Size of the sparse key table and how a lookup searches" → Selected: "Entries go in 4 KiB pages, and the table keeps the first key of each page in RAM (about 33 B per 70 entries, about 0.5 B/object). A lookup does a binary search of the table (an interpolation start is optional, because the keys are uniform), then one pread of a 4 KiB page. With about 1.25 B for the filter, the total is about 2 B/object." — Built at open from the persisted table block; a tuning constant.
- [Disk I/O path] Q: "mmap or pread for run lookups?" → Selected: "pread through the PackIo strategy (crates/chunk-store/src/pack_io.rs). Add a new file kind (for example Kind::Run, extension `.run`) or a small run-file API to PackIo, FileIo and mem::MemIo, so that the crash and fault tests (MemIo::crash, fail_next_syncs, flip) also cover the index files." — Internal trait. The module comment of pack_io.rs already plans other read paths such as mmap.
- [Memtable / flush trigger] Q: "When is the memtable flushed?" → Selected: "At pack seal (ChunkStore::seal, lib.rs:898) and when the memtable reaches a size limit. The memtable holds the Active::entries of the unsealed pack plus any GC tombstones not yet flushed. Crash recovery stays exactly as now: the active pack is scanned at open (scan_pack). The rule is 'packs above the run high-water mark get scanned', with the high-water pack number stored in the run footer or the index state file." — A policy constant and an internal ordering.
- [Merge policy] Q: "Leveled or tiered merges, and on what thread?" → Selected: "Size-tiered: merge when about 4 runs of a similar size exist (target: fewer than about 10 + log4(N) runs). Merges run on one background thread owned by ChunkStore. A merge reads its inputs outside the Mutex<Inner> and takes the lock only to install the result. A merge writes its output through write_durable (tmp, fsync, rename, dir sync) before it deletes the input runs. If a crash leaves both, newest-wins plus identical Locs make the duplicates harmless. A merge drops entries that have a matching tombstone, and drops the tombstones when it merges into the oldest run." — No persisted format depends on it.
- [GC interaction] Q: "How do GC removals reach entries in older runs?" → Selected: "gc_finish (lib.rs:628) and punch_pack (lib.rs:719) record each removed object as a tombstone (id, pack), so a tombstone kills only the entry of that pack. A compacted copy is re-added in a newer pack at a new Loc and must survive. The tombstones go in the memtable and are flushed durably into a run before any pack is deleted or punched, the same order as the current 'index first' rule in punch_pack. A lookup takes the newest run first, and a tombstone stops the search. The gc_finish loop at lib.rs:642 must not load every entry into the by_pack HashMap. It works one pack at a time, from the per-pack .idx or from a merge iterator." — This is in the run format, but it follows from correctness, and no other option meets IDX-04 with exact put dedup.
- [Stats at cold open] Q: "How does Inner::stats (objects, raw_bytes, stored_bytes) stay exact without reading every entry at open?" → Selected: "Keep the counters in a small durable index state file (or the newest run footer) that each flush, merge and GC writes. At open, add the scan of the active pack. Tests assert exact counts (gc_tests.rs:41,65,97,136; store_tests.rs:52-66; slicefs-cli gc_sweep_tests.rs:29)." — Internal; the stats file can be rebuilt by a full scan.
- [Read-only open] Q: "How does Options::read_only (slicefs-cli content.rs:184 open_read_only; used by stats, scrub and the format check) open the store when a mount may merge runs at the same time?" → Selected: "Read-only open writes nothing. It lists the runs once, ignores a run file that disappears (retries the listing once), and puts entries of uncovered packs in an in-RAM memtable." — Behavior only.
- [Locking] Q: "Disk reads under Mutex<Inner> in put/read/contains?" → Selected: "Check the memtable and the filters under the lock. Do the run-page pread outside the lock, holding a snapshot (Arc) of the run set, like the reader handles today (Inner::reader, lib.rs:985). Before put appends (lib.rs:561-569), recheck the memtable under the lock." — Internal concurrency detail.
- [Rehash #62 / tag byte] Q: "Key layout with the ObjectId tag" → Selected: "The key is the full 33-byte ObjectId (tag first), compared bytewise as the current Ord does. The Bloom probes use only digest bytes. An interpolation search, if used, works inside each tag range." — Matches the existing ObjectId Ord.
- [Tuning surface] Q: "New Options fields?" → Selected: "Add only internal or test knobs (memtable limit, bits per key, merge fan-in) to chunk_store::Options with defaults. No CLI flags in this phase." — A Rust struct inside the workspace, not a public CLI contract.

### Claude's Discretion
- Test names, helper layout and plan split are at Claude's discretion.

</decisions>

<code_context>
## Existing Code Insights

### Reusable Assets
- crates/chunk-store/src/lib.rs encode_index/decode_index (lines 337-387): already a sorted (ObjectId, offset, stored, raw, flags) run with a CRC. Base for the L0 run format.
- crates/chunk-store/src/lib.rs scan_pack / resync_scan (247-335): recovery of entries not covered by a run
- crates/chunk-store/src/pack_io.rs write_durable (256-263) and PackIo::write_durable: atomic tmp+fsync+rename+dir sync for run and state files
- crates/chunk-store/src/pack_io.rs Kind enum + pack_name (13-33): extend with a Run kind
- crates/chunk-store/src/mem.rs MemIo (crash(), fail_next_syncs, flip, set_fail_reads): fault injection for the crash tests during flush and merge
- crates/chunk-store/tests/gc_fault_tests.rs, gc_tests.rs, punch_tests.rs: GC test patterns to extend for 'no lookup returns a GC-removed object'
- Inner::reader LRU (lib.rs:985-1002) and MAX_OPEN_PACKS: pattern for an LRU of run file handles
- crc32c (existing dependency) for run page and footer checksums
- Phase 16 benchmark (not written yet: .planning/phases/16-chunk-index-benchmark is empty, and crates/chunk-store has no benches dir). IDX-05 reuses it.

### Established Patterns
- Index-before-bytes ordering: make the index change durable before a pack is deleted or punched (punch_pack comment, lib.rs:712-718; gc_finish syncs the copies before it deletes, lib.rs:699-708)
- A sync failure is final: Inner::failed + check() (lib.rs:938-960). Run flush and merge failures in the foreground path follow it. Background merge failures can only log and retry, because the runs are derived.
- Untrusted counts on decode: checked_mul size validation (decode_index, lib.rs:367-373; test an_index_with_an_overflowing_count_is_refused). Apply to run footers and sparse tables.
- Format version check with refusal on mismatch (check_version, lib.rs:260-272). FORMAT_VERSION changes; no migration.
- All storage goes through the PackIo strategy so that MemIo can inject faults
- Plain-English doc comments; the design note docs/design/chunk-store.md ('Index in memory' section, around line 158) must be updated

### Integration Points
- ChunkStore::open_with (lib.rs:456-532): today it decodes every .idx into the HashMap. It must become: load the run footers, filters and sparse tables, then scan only the packs above the high-water mark or without a valid .idx.
- ChunkStore::put (lib.rs:536-579): two dedup checks under the lock (543, 563). A stale positive here skips the write, so a GC-removed object would be lost silently. This is the critical correctness path.
- ChunkStore::read / contains / stored_len (lib.rs:807-848): lookups. contains is also used by slicefs-cli content.rs:141 on a read-only store.
- ChunkStore::append / seal (583-603, 898-908): memtable insert and flush trigger
- ChunkStore::gc_begin / gc_finish / punch_pack (608-796): gc_finish loops over the whole of inner.index (642). Must become a per-pack or iterator scan plus tombstones. The touched set (Inner::touched) stays as now.
- Inner::add (977-983) and stats accounting; GcStats counts
- slicefs-cli filesystem.rs gc_start (around 490-540) calls content.gc_begin under the commit gate. No API change is expected.
- slicefs-cli content.rs open / open_read_only (around 150-188): read-only open while a mount merges
- pack_io.rs PackIo trait + FileIo + mem.rs MemIo: new file kind for runs and the index state

### Files likely modified
- crates/chunk-store/src/lib.rs
- crates/chunk-store/src/pack_io.rs
- crates/chunk-store/src/mem.rs
- crates/chunk-store/src/index.rs (new: memtable, run set, lookup, merge)
- crates/chunk-store/src/run.rs (new: run file encode/decode, sparse table, footer)
- crates/chunk-store/src/bloom.rs (new: blocked Bloom filter over digest bytes)
- crates/chunk-store/tests/index_tests.rs (new: lookup, crash during flush or merge, GC visibility)
- crates/chunk-store/tests/gc_tests.rs
- crates/chunk-store/tests/gc_fault_tests.rs
- crates/chunk-store/benches/ or the Phase 16 benchmark binary (IDX-05 comparison)
- docs/design/chunk-store.md

### Risks
- Phase 16 (benchmark, #66) has no artifacts yet: .planning/phases/16-chunk-index-benchmark is empty and crates/chunk-store has no benches. IDX-05 depends on it, and the baseline must be recorded before the HashMap is removed.
- A stale index positive in put (lib.rs:543/563) makes put skip the write, so a GC-removed or punched object is lost silently. Tombstone durability and visibility must come before the pack delete or punch, and the tests must cover put after GC (not only get).
- gc_finish builds by_pack from every index entry (lib.rs:641-646), which costs O(N) RAM. If it is not changed, GC brings back the RAM problem.
- The exact Stats counts are asserted by tests. Duplicate IDs across runs after compaction make the counts at cold open hard to get right without a persisted counter.
- A read-only open (slicefs stats/scrub/format check) while a mount merges in the background: run files can disappear between list and open.
- There is no background thread in ChunkStore today. A merge thread needs shared ownership (Arc of the index state) and a clean shutdown in close() and Drop. Mount/unmount tests must not hang.
- The Docker/libfuse test path and the no-macOS-mount rule: the index crash tests must stay in-process on MemIo and need no mount.

</code_context>

<specifics>
## Specific Ideas

The current state: Loc (lib.rs:168-175) is {pack u64, offset u64, stored u32, raw u32, flags u8}. A per-pack .idx entry is 50 B (IDX_ENTRY = ID_LEN+17; the pack number is implied by the file name). A merged-run entry must carry the pack number: 58 B with u64 pack and u64 offset (u32 for both would fit the 64 MiB pack_limit, but keep u64 for safety). The HashMap costs about 85-170 B/object (issue estimate). The target is about 2 B/object: a Bloom at about 10 bits/key, about 1.25 B, plus a sparse table of first keys per 4 KiB page, about 0.5 B. Hit: filter check, table search, one 4 KiB pread. Miss: filter check only, about 1% false positives per run, so with R runs the miss cost is about R x 1% preads. Keep R small. Do the memtable and filter checks under Mutex<Inner>, and the pread outside it. Correctness: a lookup must never return a Loc for an object that GC removed. This also protects put, because a stale hit makes put skip the write, which loses data silently. Use tombstones (id, pack), made durable before the pack delete or punch. Newest-first lookup lets a compacted copy (a newer pack) shadow the older entry. Recovery: packs without a valid .idx (or above the high-water mark) are scanned as now. Merged runs and the index state file are written with write_durable, and inputs are deleted only after the output is durable. Crash tests use MemIo::crash() at each step of flush and merge, and check that no object is lost and no Loc is wrong. GC tests: gc_begin, mark, gc_finish, then contains/get/put for removed IDs must report missing (and put must write the object again), across reopen and across a merge done during the GC. Note on fastbloom: it is already in [workspace.dependencies] and used by crates/cas-local, so it adds no new crate. But its bit layout is crate-internal, so it is not used for persisted filters.

Project rules: no macOS mounts in automated tests; use targeted cargo commands in worktrees and one full test run per merge; no store backward compatibility; FSKit is the only macOS backend.

</specifics>

<deferred>
## Deferred Ideas

The RAM of the GC mark set (the `live` HashSet passed to gc_finish) and of Inner::touched still grows with the number of live objects. At 100M objects that is several GiB during a GC. Out of scope for #85; suggest a follow-up issue (for example a disk-backed or Bloom-assisted mark). A ribbon/xor filter as a later change to save RAM. Tuning for HDD and a measured SSD vs HDD comparison beyond the benchmark. mmap read path. CLI knobs for the index.

</deferred>
