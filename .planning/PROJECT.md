# DedupFS

## What This Is

A general-purpose deduplicating filesystem built in Rust that uses content-addressable storage (CAS) with block-level deduplication to dramatically reduce storage consumption. It presents a full POSIX-compliant filesystem interface via FUSE (FUSE-T on macOS, libfuse on Linux, WinFSP on Windows) and is designed with pluggable abstractions at every layer to support future distributed and decentralized topologies.

## Core Value

High-ratio data deduplication that works transparently as a real, daily-driver filesystem — not a backup tool or archive format, but a mountable POSIX filesystem you can use for actual work.

## Current Milestone: v2.0 — Streaming Writes & Hardening

**Goal:** Remove the file size limitation by streaming writes through data-id's incremental push API, remove write-path compression (dedup on raw content), and fix known v1.0 bugs.

**Target features:**
- Streaming writes via State::push_bytes() — O(log N) memory regardless of file size
- Remove compression from write/read path — raw bytes to Merkle tree, cross-compressor dedup works
- Fix refcount overflow (silent wrap to 0 = data loss risk)
- Fix statfs reporting (hardcoded f_files=1M, unrealistic bfree)
- Improve snapshot lookup (O(n) → indexed)

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
- ✓ CI pipeline (Linux + macOS, pjdfstest) — v1.0

### Active

- [ ] Streaming writes — no file size = RAM limitation
- [ ] Remove write-path compression — raw bytes to tree, dedup on original content
- [ ] Refcount overflow protection — saturating_add or checked_add
- [ ] Realistic statfs reporting — track actual inode count, physical usage
- [ ] Snapshot indexed lookup — O(1) by version, O(1) by name

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
| Rust as implementation language | Memory safety, perf, fuser crate ecosystem | — Pending |
| FUSE-T + fuser for filesystem frontend | Cross-platform FUSE support, kext-free on macOS | — Pending |
| Pluggable hash functions | Future flexibility, no premature lock-in | — Pending |
| Pluggable storage backend | Enables future distributed backends without core changes | — Pending |
| Block-level CAS deduplication | Proven approach, works across file types and workloads | — Pending |
| Defer distributed/decentralized to future milestone | Focus on solid local foundation first | — Pending |

---
*Last updated: 2026-03-29 after v2.0 milestone start*
