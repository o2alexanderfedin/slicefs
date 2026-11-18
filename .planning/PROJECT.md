# DedupFS

## What This Is

A general-purpose deduplicating filesystem built in Rust that uses content-addressable storage (CAS) with block-level deduplication to dramatically reduce storage consumption. It presents a full POSIX-compliant filesystem interface via FUSE (FUSE-T on macOS, libfuse on Linux; Windows deferred) and is designed with pluggable abstractions at every layer to support future distributed and decentralized topologies.

## Core Value

High-ratio data deduplication that works transparently as a real, daily-driver filesystem — not a backup tool or archive format, but a mountable POSIX filesystem you can use for actual work.

## Current State

**Shipped:** v2.0 Streaming Writes & Hardening (2026-11-17, tag `milestone-v2.0`). Public releases so far: v2.0.0, v2.0.1, v2.1.0.

- Chunk store: FastCDC v2020 (4/8/32 KiB) and BLAKE3-256, append-only packs, zstd per object, in-memory index.
- Streaming writes for sequential writes; Buffered fallback for out-of-order writes.
- Write log (#20) with replay after a crash and a clean cut at each commit; reachability GC, online at mount.
- macOS: FUSE-T 1.2+ with FSKit only. Linux: libfuse.

## Next Milestone Goals (candidates)

- Several roots, commits and refs (branches, temporary branches, tags, pins as GC roots): #86, with #87-#91 and several mounts of one store (#92).
- On-disk chunk index instead of the in-memory hash table: #85.
- Cheaper commit with large open Buffered files: #93.

<details>
<summary>v2.0 milestone goal (archived)</summary>

**Goal:** Remove the file size limitation by streaming writes through data-id's incremental push API, remove write-path compression (dedup on raw content), and fix known v1.0 bugs.

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

### Active

- [ ] Several roots, commits and refs (#86)
- [ ] On-disk chunk index (#85)

### Out of Scope

- Distributed/decentralized topology — deferred to future milestone
- Specific chunking algorithm selection — owner will provide existing technology
- Network protocol design — depends on distributed topology decisions
- GUI or management UI — CLI-first
- Windows support — winfsp-rs GPL-3 license issue, deferred
- Segment-level compression (Option C) — deferred to v2.1 after streaming writes land

## Context

- The owner has an existing technology repository with chunking/block-splitting work that will be integrated later
- FUSE-T is the macOS FUSE implementation (kext-free, uses NFS under the hood)
- The `fuser` Rust crate provides cross-platform FUSE bindings
- This is a greenfield Rust project
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

---
*Last updated: 2026-11-17 after v2.0 milestone close*
