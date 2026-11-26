# DedupFS

## What This Is

A general-purpose deduplicating filesystem built in Rust that uses content-addressable storage (CAS) with block-level deduplication to dramatically reduce storage consumption. It presents a full POSIX-compliant filesystem interface via FUSE (FUSE-T on macOS, libfuse on Linux; Windows deferred) and is designed with pluggable abstractions at every layer to support future distributed and decentralized topologies.

## Core Value

High-ratio data deduplication that works transparently as a real, daily-driver filesystem — not a backup tool or archive format, but a mountable POSIX filesystem you can use for actual work.

## Current State

**Shipped:** v2.2 Concurrency & Store Evolution (2026-11-26). It ships as release `v2.3.0`, because `v2.2.0` was already used by the v2.1 release. Earlier: v2.1 (2026-11-20, release `v2.2.0`), v2.0 (2026-11-17, tag `milestone-v2.0`). Public releases: v2.0.0, v2.0.1, v2.1.0, v2.2.0; next v2.3.0.

- Chunk store: FastCDC v2020 and BLAKE3-256 by default, append-only packs, zstd per object, on-disk chunk index (sorted runs, MANIFEST, Bloom filters, background merge): 2.2-3.3 B of RAM per object.
- Store header (format 7) with compat, ro_compat and incompat flags. The flags `chunk-profile` (ro_compat) and `hash-sha256` (incompat) carry the release word `v2.3`.
- Per-store FastCDC profile, set at `seed` (default 4/8/32 KiB). SHA-256 is a second production hash; `slicefs rehash --to <hash>` converts a store offline and crash-safe.
- Multithreaded requests: fine-grained locks with a rank checker, a dirty budget with a background flusher, and a worker pool (`--workers`; default 1 on macOS, `clamp(cpus/2, 1, 8)` on Linux). pjdfstest passes with 4 workers. Reads scale (2.69x at 4 workers); writes do not scale past 2 workers.
- Safety gates (clippy, Miri, TSan, loom, Kani, deadlock stress, coverage) run in the release gate, not on each PR.
- Writes: streaming for sequential writes; random writes use per-file overlays with a fixed dirty limit, so daemon memory does not grow with the file size. Write log with replay after a crash; reachability GC, online at mount.
- macOS: FUSE-T 1.2+ with FSKit only. Linux: libfuse.

## Next Milestone Goals

Not defined yet (run `/gsd-new-milestone`). Owner items to carry forward:
- Release publishing (#178, sub-issues #179-#195; review bugs #196-#199): binaries and reports for macOS and Linux.
- Refcount removal (#165).
- #207 remaining parts: whole `.idx` decode, Bloom filters at about 1.25 B per object.
- #210: scrub keeps all content keys in RAM.
- Prolly trees (#19): decision "later" (comment posted); no work planned now.
- v2.2 audit warnings: G1 (no end-to-end test with a real mount and N workers around a rehash), G2 (the rehash refcount sorter has no free-space check), G3 (extsort, hash, rehash and the gc-tmp mark have no Miri, loom or TSan target of their own).
- Open items from the Phase 19-21.2 deferred lists (see STATE.md Deferred Items), for example write scaling (the single pack appender), a macOS worker default above 1, and a public `--read-only` mount option.

Other candidates: idle-merge control (#141), several roots, commits and refs (#86, with #87-#92), index layout again (#110), GC tuning (#121), adaptive control (#152).

<details>
<summary>Earlier milestone goals (archived)</summary>

**v2.2:** Handle FUSE requests on more than one thread with fine-grained locks, add `slicefs rehash`, and measure two store-format options (prolly trees, larger FastCDC profiles) before any default changes.

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
- ✓ Refcount overflow — superseded: GC is reachability (mark-and-sweep) since v2.0; the in-memory reference-count table is still kept (read only by `stats` and `scrub`), its removal is #165

### Validated (v2.1)

- ✓ Write log close-out: every #20 acceptance item maps to a test or a follow-up issue — v2.1
- ✓ Bounded memory for random writes; a commit re-logs only changed bytes (#98, #93) — v2.1
- ✓ Pack-file measurements (#8) — v2.1
- ✓ Chunk index benchmark (#66) — v2.1
- ✓ On-disk chunk index (#85) — v2.1 (106-196 B -> 2.2-3.3 B of RAM per object)

### Validated (v2.2)

- ✓ Spikes S1-S7 and an in-process stress harness (#127) — v2.2
- ✓ Fine-grained locks, no global lock on the hot path, read while write (#21) — v2.2
- ✓ Dirty budget with a background flusher, safe with N writers — v2.2
- ✓ Worker pool with more than one worker thread, drain before `destroy()`, abort on panic (#22) — v2.2
- ✓ Store header with format number and feature flags, format policy doc (#154) — v2.2
- ✓ Thread-safety gates (clippy, Miri, TSan, loom, Kani, deadlock stress, coverage) and the two Phase 19 races fixed — v2.2
- ✓ Per-store FastCDC profile and a profile benchmark (#67) — v2.2 (default stays 4/8/32 KiB)
- ✓ SHA-256 write hash and offline, crash-safe `slicefs rehash` (#62) — v2.2
- ✓ Prolly-tree evaluation with a written decision (#19) — v2.2 (decision: later)

### Active

- [ ] Next milestone not defined yet; see Next Milestone Goals.

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
- Rust workspace with a vendored, patched fuser; CI runs Linux tests in Docker (with pjdfstest, 4 workers) and macOS tests with FSKit mounts
- About 82,000 lines of Rust in `crates/` after v2.2 (v2.2 added about 41,000 lines in `crates/` and `vendor/fuser`)
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
| Pluggable hash functions | Future flexibility, no premature lock-in | ✓ Good; SHA-256 is a second production hash and `rehash` converts a store (v2.2) |
| Pluggable storage backend | Enables future distributed backends without core changes | — Pending |
| Block-level CAS deduplication | Proven approach, works across file types and workloads | ✓ Good; content-defined chunks (FastCDC) with full BLAKE3-256 IDs |
| Defer distributed/decentralized to future milestone | Focus on solid local foundation first | — Pending |
| Custom sorted-run chunk index, not fjall (v2.1) | 1.37 vs 2.12 B/object, 9 ms vs 0.65 s open at 10M; fjall refuses a second-process open | ✓ Good |
| Never hold a whole file in RAM (v2.1) | 1 GiB files must not cost 1 GiB of daemon memory | ✓ Good (VmHWM 165 MiB for a 4 GiB file) |
| FastCDC stays 4/8/32 KiB (v2.1) | Size-class ideas parked until measured | ✓ Kept in v2.2; the profile is now per store; the benchmark recommends 8/32/128 KiB for a later default |
| macOS worker default may be more than 1 (v2.2, Phase 18) | Spike S1 on FSKit CI: 2 requests in flight, 5 reply-order inversions, no errors | ⚠️ Revisit: the default stays 1; the macOS bench showed no gain (Phase 21) |
| Keep `std::sync::RwLock` for the commit gate; release the gate before any wait (v2.2, Phase 18) | Spike S5: a new reader waits as long as the longest hold while a commit waits | ✓ Good; loom models of the gate pass (Phase 21.2) |
| Stress harness with model check is the "done" test for the lock phases (v2.2, Phase 18) | Today's code fails it with free threads on one shared file | ✓ Good |
| Fine-grained locks: parking_lot 0.12 shim with a debug rank checker; one `FileCell` per open inode; rebase and stream steps run unlocked under a per-file step token; log record appended under the apply locks (v2.2, Phase 19) | The harness passes with free threads on one file; six lock sites hold at most 262 us (macOS) and 1967 us (Linux); a step wait now has 10-15 s limits that give EIO | ✓ Good |
| Dirty budget and flusher: writers wait for budget before any lock; one background flusher steps idle, expired or quiet files, one step per shared gate hold; `--dirty-budget` 64M, `--dirty-limit` min(32M, budget), `--dirty-idle` 5 s, `--dirty-expire` 30 s (v2.2, Phase 20) | No take-out of another file's state; all writers throttled with a commit waiting ends; Docker S7 with the flusher peaks at 287 MiB VmHWM under the 320 MiB cap | ✓ Good |
| Worker pool: one FUSE reader, N workers by `--workers`; default 1 on macOS, `clamp(cpus/2, 1, 8)` on Linux; a worker panic aborts the daemon (v2.2, Phase 21) | 4 workers pass pjdfstest; reads 2.69x at 4 workers; writes flat past 2 workers (single pack appender) | ✓ Good; write scaling is open |
| Store header format 7 with compat, ro_compat and incompat flags; a new setting line always comes with a flag (v2.2, Phase 21.1) | Old releases must refuse or mount read-only what they do not understand | ✓ Good; the format freeze is an owner decision |
| Safety gates run in the release gate, not on each PR (v2.2, Phase 21.2) | Owner decision (2026-11-26); the release must not ship with a failed gate | ✓ Good |
| Chunk profile fixed per store at `seed`; default stays 4/8/32 KiB (v2.2, Phase 22) | A profile change is a store-format change; the owner decides the default | — Pending (the benchmark recommends 8/32/128 KiB) |
| SHA-256 as a second production hash; rehash is offline only (v2.2, Phase 23) | Owner decision; rehash refuses while the store is mounted | ✓ Good |
| Prolly trees: decision "later" (v2.2, Phase 24) | At the Boost scale the gain per commit is small (about 4-8 ms); it grows with the map (1.6M entries pass 100 ms flat); file manifests are already content-cut trees | — Pending (#19) |
| GSD milestone v2.2 ships as release `v2.3.0` | `v2.2.0` was already used by the v2.1 release | ✓ Done |

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

*Last updated: 2026-11-26 after v2.2 milestone*
