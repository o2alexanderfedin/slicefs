# Milestones

## v2.2 Concurrency & Store Evolution (Shipped: 2026-11-26)

**Phases completed:** 9 phases (18, 19, 20, 21, 21.1, 21.2, 22, 23, 24), 69 plans, 79 tasks
**Requirements:** 37/37 satisfied
**Audit:** `.planning/milestones/v2.2-MILESTONE-AUDIT.md` (status tech_debt; the debt is accepted and carried forward). The audit was written before the #19 post and before the flag word `v2.3`, so it says 36/37 and 8/9 phases; Phase 24 and PROLLY-03 were completed after it.
**Release:** `v2.3.0` (the GSD milestone id stays v2.2; `v2.2.0` was already used by the v2.1 release). The store flags `chunk-profile` and `hash-sha256` carry the release word `v2.3`.
**Git range:** 111c960 (first Phase 18 commit) to the close commit; 558 commits; 181 files changed in `crates/` and `vendor/fuser` (+41,449 / -4,685 lines).
**Closeout type:** override_closeout. Known verification overrides: 22 newly acknowledged, 0 carried forward from a prior close (see STATE.md Deferred Items). The first audit listed 26 items; 4 of them were the rows of one table in the Phase 21.2 deferred list, so the real count is 22.
**Verification note:** all 9 phase reports say `passed`. The GSD tool reads Phases 18-23 as `stale`, because later phases changed source files that the earlier reports cover. The reports were not run again.

**What shipped (state at close):**
- Spikes S1-S7 and an in-process stress harness with a data model (#127).
- Fine-grained locks (#21): parking_lot shim with a debug rank checker, one `FileCell` per open inode, 256 namespace stripes, I/O steps with no lock held, log records appended under the locks of their apply. Six lock sites hold at most 1776 us (macOS) and 1967 us (Linux).
- Dirty budget with a background flusher; writers wait for budget before they take a lock; options `--dirty-budget`, `--dirty-limit`, `--dirty-idle`, `--dirty-expire`.
- Worker pool (#22): `--workers N`, drain before `destroy()`, abort on a worker panic. pjdfstest passes with 4 workers. Reads 2.69x at 4 workers; writes do not scale past 2 workers.
- Store header format 7 with compat, ro_compat and incompat flags (#154); an unknown ro_compat flag mounts read-only; `docs/design/store-format.md`.
- Safety gates in the release gate: clippy, Miri, TSan, loom, Kani, deadlock stress, coverage (floor 90.27 %). The two Phase 19 races are fixed.
- Per-store FastCDC profile set at `seed` (#67); the benchmark recommends 8/32/128 KiB for a later default; the default stays 4/8/32 KiB.
- SHA-256 as a second production hash, and `slicefs rehash --to <hash>`: offline, crash-safe, bounded memory, `--dry-run` (#62).
- Prolly-tree prototype (`crates/prolly`) and benchmark on Boost; decision "later", posted on #19.

**Known tech debt:** audit warnings G1 (no end-to-end test with a real mount and N workers around a rehash), G2 (the rehash refcount sorter has no free-space check), G3 (extsort, hash, rehash and the gc-tmp mark have no Miri, loom or TSan target of their own). Open follow-ups: #207 (whole `.idx` decode, Bloom filters), #210 (scrub keeps all content keys in RAM), #165 (refcount removal), #178 (release publishing).

---

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
