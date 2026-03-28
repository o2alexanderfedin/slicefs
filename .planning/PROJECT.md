# DedupFS

## What This Is

A general-purpose deduplicating filesystem built in Rust that uses content-addressable storage (CAS) with block-level deduplication to dramatically reduce storage consumption. It presents a full POSIX-compliant filesystem interface via FUSE (FUSE-T on macOS, libfuse on Linux, WinFSP on Windows) and is designed with pluggable abstractions at every layer to support future distributed and decentralized topologies.

## Core Value

High-ratio data deduplication that works transparently as a real, daily-driver filesystem — not a backup tool or archive format, but a mountable POSIX filesystem you can use for actual work.

## Requirements

### Validated

(None yet — ship to validate)

### Active

- [ ] Block-level CAS-based deduplication with pluggable hash functions
- [ ] Pluggable chunking/block-splitting strategy (owner has existing technology)
- [ ] Pluggable storage backend for CAS blocks
- [ ] Full POSIX filesystem semantics (read, write, create, delete, rename, symlinks, permissions, xattrs)
- [ ] FUSE frontend via fuser crate (FUSE-T on macOS, libfuse on Linux, WinFSP on Windows)
- [ ] Cross-platform support: macOS, Linux, Windows
- [ ] Production-quality reliability for daily use as a real filesystem
- [ ] Clean abstraction layers ready for future distributed/decentralized extension

### Out of Scope

- Distributed/decentralized topology — deferred to future milestone
- Specific chunking algorithm selection — owner will provide existing technology
- Network protocol design — depends on distributed topology decisions
- GUI or management UI — CLI-first

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
*Last updated: 2026-03-27 after initialization*
