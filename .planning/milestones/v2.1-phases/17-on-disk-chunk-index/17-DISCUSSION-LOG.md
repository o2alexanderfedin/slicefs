# Phase 17: On-disk chunk index - Discussion Log

> **Audit trail only.** Do not use as input to planning, research, or execution agents.
> Decisions are captured in CONTEXT.md — this log preserves the alternatives considered.

**Date:** 2026-11-18
**Phase:** 17-On-disk chunk index
**Areas discussed:** On-disk index layout (user approved the revised design in an earlier session turn)

## On-disk index layout

| Option | Description | Selected |
|--------|-------------|----------|
| Keep .idx as L0, merged runs derived | Scout proposal | |
| Global sorted runs + MANIFEST, .idx unchanged as source of truth | Revised design after review of #85 | ✓ |

**User's choice:** Approve as revised

## Decided autonomously

[auto] [Filter type / dependency] — Q: "Ribbon/xor crate (new dep), fastbloom (already in workspace), or hand-written Bloom?" → Selected: "A hand-written blocked Bloom filter, about 10 bits per key (about 1% false positives, 1.25 B/object), saved in the run file. The keys are already uniform BLAKE3 digests, so the probe positions come straight from digest bytes (for example h1 = digest[0..8], h2 = digest[8..16], Kirsch-Mitzenmacher) wi" (reversible technical — decided autonomously: The filter sits behind one small trait and has a type byte in the run footer. With no store backward compatibility, a later change to ribbon is a version bump.)
[auto] [Sparse key table] — Q: "Size of the sparse key table and how a lookup searches" → Selected: "Entries go in 4 KiB pages, and the table keeps the first key of each page in RAM (about 33 B per 70 entries, about 0.5 B/object). A lookup does a binary search of the table (an interpolation start is optional, because the keys are uniform), then one pread of a 4 KiB page. With about 1.25 B for the f" (reversible technical — decided autonomously: Built at open from the persisted table block; a tuning constant.)
[auto] [Disk I/O path] — Q: "mmap or pread for run lookups?" → Selected: "pread through the PackIo strategy (crates/chunk-store/src/pack_io.rs). Add a new file kind (for example Kind::Run, extension `.run`) or a small run-file API to PackIo, FileIo and mem::MemIo, so that the crash and fault tests (MemIo::crash, fail_next_syncs, flip) also cover the index files." (reversible technical — decided autonomously: Internal trait. The module comment of pack_io.rs already plans other read paths such as mmap.)
[auto] [Memtable / flush trigger] — Q: "When is the memtable flushed?" → Selected: "At pack seal (ChunkStore::seal, lib.rs:898) and when the memtable reaches a size limit. The memtable holds the Active::entries of the unsealed pack plus any GC tombstones not yet flushed. Crash recovery stays exactly as now: the active pack is scanned at open (scan_pack). The rule is 'packs above th" (reversible technical — decided autonomously: A policy constant and an internal ordering.)
[auto] [Merge policy] — Q: "Leveled or tiered merges, and on what thread?" → Selected: "Size-tiered: merge when about 4 runs of a similar size exist (target: fewer than about 10 + log4(N) runs). Merges run on one background thread owned by ChunkStore. A merge reads its inputs outside the Mutex<Inner> and takes the lock only to install the result. A merge writes its output through write" (reversible technical — decided autonomously: No persisted format depends on it.)
[auto] [GC interaction] — Q: "How do GC removals reach entries in older runs?" → Selected: "gc_finish (lib.rs:628) and punch_pack (lib.rs:719) record each removed object as a tombstone (id, pack), so a tombstone kills only the entry of that pack. A compacted copy is re-added in a newer pack at a new Loc and must survive. The tombstones go in the memtable and are flushed durably into a run " (reversible technical — decided autonomously: This is in the run format, but it follows from correctness, and no other option meets IDX-04 with exact put dedup.)
[auto] [Stats at cold open] — Q: "How does Inner::stats (objects, raw_bytes, stored_bytes) stay exact without reading every entry at open?" → Selected: "Keep the counters in a small durable index state file (or the newest run footer) that each flush, merge and GC writes. At open, add the scan of the active pack. Tests assert exact counts (gc_tests.rs:41,65,97,136; store_tests.rs:52-66; slicefs-cli gc_sweep_tests.rs:29)." (reversible technical — decided autonomously: Internal; the stats file can be rebuilt by a full scan.)
[auto] [Read-only open] — Q: "How does Options::read_only (slicefs-cli content.rs:184 open_read_only; used by stats, scrub and the format check) open the store when a mount may merge runs at the same time?" → Selected: "Read-only open writes nothing. It lists the runs once, ignores a run file that disappears (retries the listing once), and puts entries of uncovered packs in an in-RAM memtable." (reversible technical — decided autonomously: Behavior only.)
[auto] [Locking] — Q: "Disk reads under Mutex<Inner> in put/read/contains?" → Selected: "Check the memtable and the filters under the lock. Do the run-page pread outside the lock, holding a snapshot (Arc) of the run set, like the reader handles today (Inner::reader, lib.rs:985). Before put appends (lib.rs:561-569), recheck the memtable under the lock." (reversible technical — decided autonomously: Internal concurrency detail.)
[auto] [Rehash #62 / tag byte] — Q: "Key layout with the ObjectId tag" → Selected: "The key is the full 33-byte ObjectId (tag first), compared bytewise as the current Ord does. The Bloom probes use only digest bytes. An interpolation search, if used, works inside each tag range." (reversible technical — decided autonomously: Matches the existing ObjectId Ord.)
[auto] [Tuning surface] — Q: "New Options fields?" → Selected: "Add only internal or test knobs (memtable limit, bits per key, merge fan-in) to chunk_store::Options with defaults. No CLI flags in this phase." (reversible technical — decided autonomously: A Rust struct inside the workspace, not a public CLI contract.)
