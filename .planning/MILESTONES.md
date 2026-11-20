# Milestones

## v2.1 Store Index & Measurements (Shipped: 2026-11-20)

**Phases completed:** 5 phases (13-17), 27 plans
**Requirements:** 19/19 satisfied
**Audit:** `.planning/milestones/v2.1-MILESTONE-AUDIT.md` (status passed; all 5 phase verifications passed on a7b29a2)
**Release:** `v2.2.0` (git-flow; `v2.1.0` was already used by the FUSE-T 1.2 release)

**What shipped (state at close):**
- Write log close-out (#20): each acceptance item maps to a passing test or a follow-up issue; crash tests for every window of the generation switch, in all three WAL modes (PerOp, FlushOnFsync, Periodic).
- Bounded-memory random writes (#98, #93): no whole-file buffer; per-file memory has a fixed limit and a shared dirty budget; a commit logs only the changed bytes. Docker bigwrite: VmHWM 106 MiB (1 GiB file), 165 MiB (4 GiB file), max write 124 / 709 ms. macOS FSKit 1 GiB copy: peak RSS 65 MiB.
- Pack-file measurements (#8): seed throughput and store size before and after packs, in `docs/design/chunk-store.md`.
- Chunk index benchmark (#66): `benchmarks/index-bench`, baseline at 1M/10M/100M objects.
- On-disk chunk index (#85): sorted runs with a MANIFEST, Bloom filters, mapped page lookups, background size-tiered merge, GC by durable tombstones. 2.2-3.3 B of RAM per object (was 106-196 B); a 100M store opens in 0.49 s (was 141 s).

**Known tech debt:** see the audit file (commit sync under the gate, timing `eprintln` lines, Phase 16 baseline warm-cache only). Open follow-ups: #22/#127 (multithreaded requests, rebase off the FUSE thread), #141 (idle merge).

---

## v2.0 Streaming Writes & Hardening (Shipped: 2026-11-17)

**Phases completed:** 6 phases (07.1, 8-12), 13 plans, 21 tasks
**Requirements:** 21/21 accounted (9 complete, 12 superseded by later design changes)
**Audit:** `.planning/milestones/v2.0-MILESTONE-AUDIT.md` (status tech_debt, no open gaps)
**Tag:** `milestone-v2.0` (not `v2.0`: release tags `v2.0.0`, `v2.0.1` and `v2.1.0` already exist)

**What shipped (state at close):**
- Streaming writes: sequential writes go straight into content-defined chunks (`content::Stream`); reads during a write are served from the stream. Out-of-order writes fall back to a Buffered mode per handle.
- Correctness: real statfs counts, O(1) snapshot lookup (FIX-02..04). Refcounts were replaced later by reachability GC with online GC at mount.
- Compression: the whole-file compression layer was removed (phase 9); zstd per chunk-store object came back later (PR #53, `--compression`).
- macOS: FUSE-T 1.2+ with FSKit as the only backend (SMB and NFS removed in PR #42). SIGTERM, SIGINT and SIGHUP do a soft unmount, so `destroy()` makes the final commit (PR #59).
- Write log (#20): each request is logged and replayed after a crash, with a clean cut at each commit; fixes in PRs #79, #81, #83 and #94 (shared write handles, replayed mtime, sync of re-logged bytes, utimens at close).

**Known tech debt:** issue #93 (full re-log of large Buffered files at each commit); write_alias not safe with several FUSE threads (not reachable: 1 thread); see the audit file.

---
