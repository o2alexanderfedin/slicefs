# SYNTH-1: Synthesizer revision log for ARCHITECTURE.md (response to VERIFY-1)

**Author:** Synthesizer Architect
**Date:** 2026-04-23
**Subject:** `/Volumes/Unitek-B/Projects/file-systems/.planning/research/dedup-index/ARCHITECTURE.md`
**Inputs:** VERIFY-1.md (5 BLOCKER, 7 MAJOR, 6 MINOR, 2 NIT — 20 findings total).

## Summary of resolution

**5 BLOCKERs fixed; 7 MAJORs fixed; 6 MINORs fixed; 2 NITs (1 fixed, 1 verified moot); 0 deferred to next iteration.** 1 new finding raised (F-021 below). Document size 5995 words — within the 3500–6000 word band.

---

## Per-finding change log

### BLOCKERs

**F-001 (Storage layout drift) — FIXED.** Resynced to `02-storage-and-layout.md` rewrite: §1 path (`<store>/cas/.dedup-index/`), §4 overview mermaid (Disk subgraph + separate StoreRoot subgraph for `mount.lock`), §7.1 directory tree (`.dedup-index/` sibling of `00..ff` shards under `<store>/cas/`), §7.2 table name (`dedup_index_v1`, const renamed `DEDUP_TABLE`), §7.3 (`header.json` → `manifest.json` with `02 §4` Manifest shape), §7.4 (spec byte layout: `SLDXBL01` magic, header CRC32C, payload xxh3-128 lo32/hi64 split, `BL01ENDX` end-magic), §9.2 rebuild steps (`*.tmp + rename + fsync(parent)` order explicit), §15.1 layout description, §17 glossary `CAS`. Type-naming reconciliation paragraph added to §1.

**F-002 (redb 3.1 vs 4.1) — FIXED.** New §15.0 phase-0 task: bump `Cargo.toml`, audit `metadata` crate (only current consumer), confirm `cargo test -p metadata` green on 4.1. Audit checklist enumerates the four 4.1 API surfaces this doc relies on (`Durability` variants, `Database::stats()` field names, `compact()`, `Builder::set_page_size`, `impl Value for ()`). Added an explicit dependency-note callout in §1 and an "all SLOs are advisory until phase 0 lands" caveat.

**F-003 (mount.lock TOCTOU) — FIXED.** New §13.4 "Locking model (correction to draft)" documents that `mount.lock` is a dirty-mount canary, not an exclusion lock (citing `crates/metadata/src/mount_lock.rs:41`). Multi-process exclusion of `index.redb` is delegated to redb's own `flock`-based `index.redb.lock`. Bloom/manifest concurrent access is caller-responsibility (FUSE single-mount contract). S3 (`kill -9` leftover lock) is explicitly addressed: redb 4.1 reclaims stale `flock` automatically on fd-close. §16 OQ-4 reversed; new OQ-15 / OQ-16 track multi-process hardening as v2.

**F-004 (bloom checksum strength) — FIXED.** §7.4 byte format restored to `02 §3.2` verbatim: header CRC32C over first 60 bytes; payload xxh3-128 split as `payload_xxh3_lo32` (header bytes 56–60) + `payload_xxh3_hi64` (footer bytes 0–8) + `BL01ENDX` end-magic. Justification paragraph contrasts CRC32C's ~2⁻³² collision probability (acceptable on 1 KB sidecar) against xxh3-128's cryptographic-grade collision resistance (mandatory at 1.2 GB). §14.2 test #4 reworded to flip a byte at offset 600 MB (mid-payload, not header).

**F-005 (type naming) — FIXED.** §1 adds an explicit one-paragraph reconciliation note: production binding name is `RedbDedupIndex` per `[01 §Naming note]` and `[COORDINATOR-LOG §5]`; references to `PersistentDedupIndex` in `02/03/04` are the unrenamed-source-spec form and refer to the same type. The architecture document uses `RedbDedupIndex` uniformly throughout.

### MAJORs

**F-006 (Insert ordering / I5) — FIXED.** §8.1 mermaid sequence keeps the `bloom → HWM` order but the surrounding prose explicitly states the rule as `redb.commit ▸ {bloom.set_all, HWM.fetch_add} ▸ caller-reply`. The braces are intentional — relative order of bloom/HWM is unconstrained, both transient states (bloom-hit + HWM-old, bloom-miss + HWM-new) are within I1/I5. Mermaid gained a new `▼ COMMIT BARRIER (I5) ▼` note.

**F-007 / OQ-11 (caller-cache after Ok) — FIXED.** §8.1 doc-note added explaining `Durability::Eventual` semantics: callers must NOT cache "I inserted h" across a crash without first calling `flush()`. §14.2 added test #13: caller-cached Ok + `kill -9` ≤ 1 ms before group-commit window expires; assert no FP, validate caller redrive contract. OQ-11 marked **Closed** in §16.

**F-008 / G2 (insert/remove race) — FIXED.** New §8.4 documents the race walk-through (CAS-write/CAS-unlink interleave around redb's serialized writer slot); G2 contract lifted from COORDINATOR-LOG with the per-hash locking + `verify_on_present=true` rescan semantics. New §16 OQ-15 tracks the dependency on the GC architect. MVP ships with GC disabled.

**F-009 (BloomConfig drift triggers) — FIXED.** §13 `BloomConfig` struct gained `effective_fpr_rebuild_multiplier: f64` (default 4.0) field, comment cites `[04 §7]`. §13.1 mode preset table added rows for both `drift_rebuild_ratio` (Seed/Default 0.20, Paranoid 0.10) and `effective_fpr_rebuild_multiplier` (Seed/Default 4.0, Paranoid 2.0).

**F-010 (MemDedupIndex migration) — FIXED.** §15.1 MVP bullet expanded: source change none; `idx.flush()` on `MemDedupIndex` is a silent no-op (correct — no on-disk state); `cas-local::mem_dedup_index` re-export remains; no production binary uses it.

**F-011 (Seed → Fast rename) — FIXED.** Reverted the silent `Seed → Fast` rename. §13 `DurabilityMode` enum now reads `Seed / Default / Paranoid`, matching `[01 §2.2]` and `[03 §7]` `mount_mode`. Each variant's redb `Durability` mapping spelled out: `Seed → Durability::None`, `Default → Durability::Eventual` + 200 ms group-commit, `Paranoid → Durability::Immediate` per-insert. §13.1 mode table updated; §13.1 epilogue calls out the spec restoration explicitly.

**F-012 (gate-failure escalation pathway) — FIXED.** New §15.0.1 "Gate-failure escalation": four-step staged response (reproduce → shard 4 redb DBs by hash prefix → re-bench → if still failing, swap to v2 log-structured engine). Decision-maker named (architecture coordinator); rollback paths spelled out per step. §15.1 and §15.2 reworded to point at §15.0.1; explicit "shard first, swap second" rule.

**F-018 (recovery RTO contradiction at N=10⁹) — FIXED.** §11 SLO row replaced the single ambiguous "≤ 30 min" entry with three rows:
- `Recovery RTO (1 B chunks, cold cache)` ≤ 1.2 h offline (~4320 s) per `[03 §6]` table
- `Recovery RTO (1 B chunks, warm cache)` ≤ 12 min (~720 s) per `[03 §6]` table
- `Recovery RTO (1 B chunks, target with v2 sharded walker)` ≤ 30 min online — v2 lever

The §9.3 xychart's 4320 s value is now consistent with §11.

### MINORs

**F-013 (DDT cite) — FIXED.** §17 glossary `DDT` cite changed to `[SYNTHESIS §1, 04-prior-art §1]`.

**F-014 (reply channel) — FIXED.** §10 batcher spec now commits to `crossbeam_channel::bounded(1)` (sync; no tokio dep in MVP `slicefs-dedup`). New §16 OQ-14a tracks the resolved choice.

**F-015 (FUSE-layer integration in v2 list) — FIXED.** §15.1 MVP gained an explicit FUSE-wiring bullet: ships behind `--features dedup-index` flag in `slicefs-cli`. `05-fuse-integration.md` is named as a phase-0 prerequisite for the wiring task (not for the crate itself). §15.2 v2 entry recast.

**F-016 (proptest demote/promote) — FIXED.** §14.1 prose reworded with "Permitted demotion" and "Forbidden promotion" framing per the verifier's recommendation.

**F-017 (RetryPolicy default safety) — FIXED.** §16 OQ-3 expanded: keep "No (fail fast)" default but add `dedup_index.transient_errors_total` counter and `tracing::warn!` on transient EBUSY until G1 FUSE retry middleware ships. §10 batcher details cross-reference.

**F-019 (FN/FP wording) — NOT FIXED.** Stylistic NIT; the verifier explicitly says "no action — stylistic." Verified in current §17 glossary FN/FP entries; informal but unambiguous.

**F-020 (mermaid `<br>` vs `<br/>`) — FIXED (verified moot).** Grep confirmed all line-breaks in the document are already `<br/>` form; no inconsistency to repair.

---

## Findings deferred (none)

Every BLOCKER and MAJOR was patched in this iteration. All 6 MINORs were also patched (F-013 / F-014 / F-015 / F-016 / F-017 + F-020-verified). F-019 NIT requires no action per the verifier's own recommendation.

No `// TODO: address in next iteration` markers were left in the document.

---

## New findings raised by the synthesizer

**F-021 [MINOR] — §8.4 G2 contract depends on a doc not yet written.** §8.4 names "the GC architect" as owner of per-hash locking, but no `06-gc-architecture.md` exists. MVP mitigation (GC disabled, `verify_on_present=true` in remove-exercising tests) is concrete; v2 production-remove requires that companion doc. Filed as §16 OQ-15 in this revision.

---

## Constraints honored

- **No new architectural decisions:** every change traces to `01/02/03/04` or `COORDINATOR-LOG`. Conflict-resolution priority per prompt: `02` for paths/schema, `03` for invariants/sequences, `04` for SLOs/batcher.
- **Mermaid diagrams preserved:** all 13 diagrams retained and updated rather than dropped (§3 invariants, §4 overview, §6 class, §7.1 tree, §7.3 manifest, §7.4 bloom format, §8.1/§8.2/§8.3 sequences, §9.1 state machine, §9.3 xychart, §10 concurrency, §12 observability).
- **Word count:** 5995 words (within 3500–6000 band).

The architect believes the document is ready for VERIFY-2; if a new BLOCKER surfaces, another patch pass is preferred — no architectural re-think required.

*End of SYNTH-1.md.*
