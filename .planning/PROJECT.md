# DedupFS

## What This Is

A general-purpose deduplicating filesystem built in Rust that uses content-addressable storage (CAS) with block-level deduplication to dramatically reduce storage consumption. It presents a full POSIX-compliant filesystem interface via FUSE (FUSE-T on macOS, libfuse on Linux; Windows deferred) and is designed with pluggable abstractions at every layer to support future distributed and decentralized topologies.

## Core Value

High-ratio data deduplication that works transparently as a real, daily-driver filesystem — not a backup tool or archive format, but a mountable POSIX filesystem you can use for actual work.

## Current State

**Shipped:** v2.1 Store Index & Measurements (2026-11-20, release `v2.2.0`). Earlier: v2.0 (2026-11-17, tag `milestone-v2.0`). Public releases: v2.0.0, v2.0.1, v2.1.0, v2.2.0.

- Chunk store: FastCDC v2020 (4/8/32 KiB) and BLAKE3-256, append-only packs, zstd per object, on-disk chunk index (sorted runs, MANIFEST, Bloom filters, background merge): 2.2-3.3 B of RAM per object, a 100M-object store opens in 0.49 s.
- Writes: streaming for sequential writes; random writes use per-file overlays with a fixed dirty limit and a shared dirty budget, so daemon memory does not grow with the file size.
- Write log (#20) with replay after a crash, three modes (per-op, flush on fsync, periodic) and a clean generation switch at each commit; reachability GC, online at mount.
- macOS: FUSE-T 1.2+ with FSKit only. Linux: libfuse. FUSE requests run on one thread.

## Next Milestone

Not defined yet. Candidates: multithreaded request handling (#22, #127; research note on file), idle-merge control (#141), several roots, commits and refs (#86, with #87-#92), prolly trees (#19, #110), `slicefs rehash` (#62), GC tuning (#121).

<details>
<summary>Earlier milestone goals (archived)</summary>

**v2.1:** Record the missing measurements, close the finished issues, and replace the in-memory chunk index with an on-disk index that does not need much RAM per object.

**v2.0:** Remove the file size limitation by streaming writes through data-id's incremental push API, remove write-path compression (dedup on raw content), and fix known v1.0 bugs.

</details>

## Requirements

### Validated (v1.0)

- ✓ Block-level CAS-based deduplication with pluggable hash functions — v1.0
- ✓ Pluggable chunking/block-splitting strategy — v1.0
- ✓ Pluggable storage backend for CAS blocks — v1.0
- ✓ Full POSIX filesystem semantics — v1.0
- ✓ FUSE frontend via fuser (FUSE-T on macOS, libfuse on Linux) — v1.0
- ✓ macOS + Linux support — v1.0
- ✓ Crash safety (WAL, GC, refcounts) — v1.0
- ✓ Block compression (Zstd/LZ4/None, pluggable) — v1.0
- ✓ Point-in-time snapshots (create/list/switch) — v1.0
- ✓ CLI completeness (mount/unmount/seed/gc/snapshot/stats/scrub/--json) — v1.0
- ✓ CI pipeline (Linux: cargo test + pjdfstest; no macOS job) — v1.0

### Validated (v2.0)

- ✓ Streaming writes, no file size = RAM limit for sequential writes — v2.0
- ✓ Dedup on raw content (compression is per stored object, after dedup) — v2.0
- ✓ Realistic statfs reporting — v2.0
- ✓ O(1) snapshot lookup by version and by name — v2.0
- ✓ Clean shutdown on SIGTERM/SIGINT/SIGHUP (final commit) — v2.0
- ✓ Refcount overflow — superseded: refcounts replaced by reachability GC — v2.0

### Validated (v2.1)

- ✓ Write log close-out: every #20 acceptance item maps to a test or a follow-up issue — v2.1
- ✓ Bounded memory for random writes; a commit re-logs only changed bytes (#98, #93) — v2.1
- ✓ Pack-file measurements (#8) — v2.1
- ✓ Chunk index benchmark (#66) — v2.1
- ✓ On-disk chunk index (#85) — v2.1 (106-196 B -> 2.2-3.3 B of RAM per object)

### Active

(None: next milestone not defined yet; see Next Milestone)

### Out of Scope

- Distributed/decentralized topology — deferred to future milestone
- Specific chunking algorithm selection — owner will provide existing technology
- Network protocol design — depends on distributed topology decisions
- GUI or management UI — CLI-first
- Windows support — winfsp-rs GPL-3 license issue, deferred
- Segment-level compression (Option C) — not needed: zstd per stored object covers it

## Context

- The owner has an existing technology repository with chunking/block-splitting work that will be integrated later
- FUSE-T is the macOS FUSE implementation (kext-free, uses NFS under the hood)
- The `fuser` Rust crate provides cross-platform FUSE bindings
- Rust workspace with a vendored, patched fuser; CI runs Linux tests in Docker (with pjdfstest) and macOS tests with FSKit mounts
- The architecture should anticipate distributed use but not build for it yet — clean interfaces at storage and metadata boundaries are the preparation

## Constraints

- **Language**: Rust — for memory safety, performance, and fuser crate compatibility
- **FUSE frontend**: FUSE-T + fuser — non-negotiable starting point
- **Abstraction**: All core components (hash, chunking, block storage, metadata storage) must be pluggable via traits
- **Quality**: Must be reliable enough for daily use with real data — not a prototype

## Key Decisions

| Decision | Rationale | Outcome |
|----------|-----------|---------|
| Rust as implementation language | Memory safety, perf, fuser crate ecosystem | ✓ Good |
| FUSE-T + fuser for filesystem frontend | Cross-platform FUSE support, kext-free on macOS | ✓ Good, with a vendored patched fuser; FSKit is the only macOS backend |
| Pluggable hash functions | Future flexibility, no premature lock-in | — Pending |
| Pluggable storage backend | Enables future distributed backends without core changes | — Pending |
| Block-level CAS deduplication | Proven approach, works across file types and workloads | ✓ Good; content-defined chunks (FastCDC) with full BLAKE3-256 IDs |
| Defer distributed/decentralized to future milestone | Focus on solid local foundation first | — Pending |
| Custom sorted-run chunk index, not fjall (v2.1) | 1.37 vs 2.12 B/object, 9 ms vs 0.65 s open at 10M; fjall refuses a second-process open | ✓ Good |
| Never hold a whole file in RAM (v2.1) | 1 GiB files must not cost 1 GiB of daemon memory | ✓ Good (VmHWM 165 MiB for a 4 GiB file) |
| FastCDC stays 4/8/32 KiB (v2.1) | Size-class ideas parked until measured | — Pending |

## Evolution

This document evolves at phase transitions and milestone boundaries.

**After each phase transition** (via `/gsd-transition`):
1. Requirements invalidated? → Move to Out of Scope with reason
2. Requirements validated? → Move to Validated with phase reference
3. New requirements emerged? → Add to Active
4. Decisions to log? → Add to Key Decisions
5. "What This Is" still accurate? → Update if drifted

**After each milestone** (via `/gsd-complete-milestone`):
1. Full review of all sections
2. Core Value check — still the right priority?
3. Audit Out of Scope — reasons still valid?
4. Update Context with current state

---

*Last updated: 2026-11-20 after v2.1 milestone*
