# VERIFY-2: Second-pass audit of ARCHITECTURE.md

**Auditor:** Verifier Architect (round 2, light pass)
**Date:** 2026-04-23
**Subject:** `/Volumes/Unitek-B/Projects/file-systems/.planning/research/dedup-index/ARCHITECTURE.md` (revised after VERIFY-1)
**Inputs:** VERIFY-1.md (5 BLOCKER, 7 MAJOR, 6 MINOR, 2 NIT), SYNTH-1.md change log, the four `architecture/0X-*.md` source specs, `SYNTHESIS.md`.

## Verdict: APPROVED

The synthesizer closed every BLOCKER and MAJOR cleanly, fixed all six MINORs (one verified moot), and the one self-raised follow-up (F-021) is correctly scoped as a v2 dependency with an airtight MVP mitigation. Re-read against the four source specs found no regressions and no new BLOCKERs. The document is ready to drive implementation.

---

## Closed-finding audit

### BLOCKER F-001 (storage layout drift) — **CLOSED**
**SYNTH-1 claim:** resync §1 path, §4 mermaid, §7.1 tree, §7.2 table name, §7.3 (`manifest.json`), §7.4 byte format, §9.2 rebuild steps, §15.1 layout.
**ARCHITECTURE.md evidence:** §1 path `<store>/cas/.dedup-index/`; §4 `subgraph Disk` labels match `02 §1`; §7.1 tree shows `index.redb`+`index.redb.lock`+`bloom.snap`+`.tmp`+`manifest.json`+`.tmp` exactly per `02 §1`; §7.2 const renamed to `DEDUP_TABLE` with table `dedup_index_v1` (matches `02 §2.1`); §7.3 mermaid is the `02 §4` Manifest with all 12 fields; §9.2 steps now use `*.tmp + rename + fsync(parent)` ordering; §15.1 prose updated.
**Verifier judgment:** Faithful adoption of `02-storage-and-layout`. CLOSED.

### BLOCKER F-002 (redb 3.1 vs 4.1) — **CLOSED**
**SYNTH-1 claim:** new §15.0 phase-0 with audit checklist; §1 dependency callout; SLOs marked advisory until phase 0 lands.
**ARCHITECTURE.md evidence:** §1 contains the explicit "Dependency note" blockquote pinning the upgrade as phase-0; §15.0 enumerates the four 4.1 API surfaces (Durability variants, `Database::stats()` field names, `compact()`, `Builder::set_page_size`, `impl Value for ()`) and ties to `cargo test -p metadata` 1287-test green baseline; final sentence of §15.0: "Until phase 0 lands, all §11 SLOs are advisory."
**Verifier judgment:** Honest, falsifiable, and forces the upgrade onto the critical path. CLOSED.

### BLOCKER F-003 (mount.lock TOCTOU) — **CLOSED**
**SYNTH-1 claim:** new §13.4 documents dirty-canary semantics, delegates redb-file exclusion to `index.redb.lock` (flock), files OQ-15/OQ-16 for hardened multi-process bloom/manifest exclusion as v2.
**ARCHITECTURE.md evidence:** §13.4 cites `crates/metadata/src/mount_lock.rs:41`, has a 3-row "resource → mechanism" table, addresses S3 directly: "redb 4.1's `Database::open` reclaims it automatically — the OS releases the `flock` on fd-close." §7.1 mermaid annotates `mount.lock` as "NOT an exclusion lock". §16 OQ-4 reversed; OQ-15/16 added.
**Verifier judgment:** Honest about the limitation, delegates to redb where redb already protects, and scopes the residual hardening as v2 — defensible for MVP under the FUSE single-mount contract. CLOSED.

### BLOCKER F-004 (bloom checksum downgrade) — **CLOSED**
**SYNTH-1 claim:** restore xxh3-128 split (lo32 in header, hi64 in footer) with `BL01ENDX` end-magic; rework §14.2 #4 to flip mid-payload byte.
**ARCHITECTURE.md evidence:** §7.4 mermaid lists `payload_xxh3_lo32` at header offset 56–60 and `payload_xxh3_hi64` at footer offset 0–8 with `magic_end = "BL01ENDX"` — byte-for-byte the `02 §3.2` table. The accompanying prose explicitly contrasts CRC32C (~2⁻³² collision risk on 1 KB sidecar — acceptable) against xxh3-128 (mandatory at 1.2 GB). §14.2 test #4 reworded: "Flip a single byte in `bloom.snap` payload (4 KiB block at offset 600 MB)" — tests the case the verifier flagged.
**Verifier judgment:** Direct match to spec. CLOSED.

### BLOCKER F-005 (type-name drift) — **CLOSED**
**SYNTH-1 claim:** §1 reconciliation paragraph naming `RedbDedupIndex` as production binding; treat `PersistentDedupIndex` in `02/03/04` as identical.
**ARCHITECTURE.md evidence:** §1 paragraph "Type-naming reconciliation" explicitly states the rule and cites `[01 §Naming note]` + `[COORDINATOR-LOG §5]`.
**Verifier judgment:** Internally consistent; the cross-reference issue is now documented as a known stylistic divergence in the source specs. CLOSED.

### MAJOR F-006 (insert ordering / I5) — **CLOSED**
§8.1 mermaid retains `bloom → HWM` order; the surrounding prose now states the canonical contract: `redb.commit ▸ {bloom.set_all, HWM.fetch_add} ▸ caller-reply`. Mermaid gained the `▼ COMMIT BARRIER (I5) ▼` note. The `{…}` braces wording is the verifier's own recommendation, picked up verbatim. CLOSED.

### MAJOR F-007 (caller-cache after Ok / OQ-11) — **CLOSED**
§8.1 has the "Caller observability note (S1 / OQ-11)" blockquote — explicit warning that `Durability::Eventual` does not survive `kill -9` within the 200 ms group-commit window without a prior `flush()`. §14.2 added test #13 with the exact mechanism (SIGKILL ≤ 1 ms after Ok-reply, before any `flush()`); §16 OQ-11 marked Closed. CLOSED.

### MAJOR F-008 (insert/remove race / G2) — **CLOSED**
New §8.4 walks the CAS-write/CAS-unlink interleave around redb's serialized writer slot, lifts the G2 contract from COORDINATOR-LOG into the doc proper, names the per-hash lock + `verify_on_present=true` rescan as the resolution, and notes "MVP ships with GC disabled." This is the **prevent**, not just **describe**, contract the prompt asked for: the chunker is required to re-stat after `cas.put` when concurrent remove is possible, which closes the I1 hole. CLOSED.

### MAJOR F-009 (BloomConfig drift triggers) — **CLOSED**
§13 `BloomConfig` struct gained `effective_fpr_rebuild_multiplier: f64` (default 4.0) with `[04 §7]` cite; §13.1 mode preset table has rows for both `drift_rebuild_ratio` (Seed/Default 0.20, Paranoid 0.10) and `effective_fpr_rebuild_multiplier` (Seed/Default 4.0, Paranoid 2.0). Also documents OR semantics. CLOSED.

### MAJOR F-010 (MemDedupIndex migration) — **CLOSED**
§15.1 bullet now spells out: source change none; `flush()` no-ops on `MemDedupIndex` is correct (no on-disk state); `cas-local::mem_dedup_index` re-export remains; no production binary uses it. Matches verifier's recommendation. CLOSED.

### MAJOR F-011 (Seed → Fast rename) — **CLOSED**
§13 `enum DurabilityMode` reads `Seed / Default / Paranoid`; §13.1 table column header is "Seed | Default | Paranoid"; §13.1 epilogue calls out the spec restoration explicitly ("the earlier draft renamed `Seed → Fast` and silently switched its durability from `None` to `Eventual`; this revision restores the spec"). §17 glossary updated. **Spot-checked for dangling references:** §11 SLO row uses "seed mode"; §16 OQ-13 says "bench_seed_burst_100m"; §15.1 says "modes `seed`/`default`/`paranoid`". No `Fast` survivor anywhere. CLOSED.

### MAJOR F-012 (gate-failure escalation pathway) — **CLOSED**
New §15.0.1 "Gate-failure escalation" is a 4-row staged table: reproduce → shard → re-bench → swap-to-v2. Names the architecture coordinator as decision-maker, and the rule "shard first, swap second" replaces the contradictory §15.1/§15.2 wording. **Cross-checked against `04 §10`:** bench `bench_seed_burst_100m` exists at `04 §10` row 1 with the same 100 K ins/s SLO — the gate references real, runnable benchmarks. CLOSED.

### MINOR F-013 (DDT cite) — **CLOSED**
§17 `DDT` line: `[SYNTHESIS §1, 04-prior-art §1]`. CLOSED.

### MINOR F-014 (reply channel) — **CLOSED**
§10 batcher detail: "`crossbeam_channel::bounded(1)` (sync — MVP `slicefs-dedup` does not pull tokio; OQ-14a closed)". OQ-14a added in §16. CLOSED.

### MINOR F-015 (FUSE wiring in v2 list) — **CLOSED**
§15.1 has the explicit FUSE-wiring bullet ("behind `--features dedup-index` flag"); §15.2 now lists only "sharded redb if not already landed under §15.0.1." CLOSED.

### MINOR F-016 (proptest demote/promote wording) — **CLOSED**
§14.1 reworded with "Permitted demotion" / "Forbidden promotion" framing per recommendation. CLOSED.

### MINOR F-017 (RetryPolicy default) — **CLOSED**
§16 OQ-3 expanded with `dedup_index.transient_errors_total` counter + `tracing::warn!` until G1 ships; §10 cross-reference present. CLOSED.

### MINOR F-018 (recovery RTO contradiction) — **CLOSED**
§11 now has three rows for N=1 B (cold 1.2 h, warm 12 min, target 30 min v2 sharded) — fully reconciled with the §9.3 xychart's 4320 s data point. CLOSED.

### NIT F-019 (FN/FP wording) — **NOT FIXED (correctly)**
Verifier's own recommendation was "no action". Synthesizer correctly left it. CLOSED-by-verification.

### NIT F-020 (mermaid `<br>` consistency) — **CLOSED**
Synthesizer claims grep-verified all are `<br/>`. Spot-check of §3, §7.1, §7.4, §10 confirms `<br/>` form throughout. CLOSED.

---

## New issues introduced by patches

**None of severity ≥ MAJOR found.**

Sample-checked the patches that were thinnest:

- **§13.4 locking model.** Mentions `flock(LOCK_EX | LOCK_NB)` for redb but cites the existing `metadata/src/mount_lock.rs` correctly as the dirty-canary. The "redb 4.1's `Database::open` reclaims it automatically" claim depends on phase-0 audit landing before this is empirically verified — but §15.0's checklist is the right place for that, and the doc is consistent. No new finding.
- **§8.4 G2 contract.** The OQ-15 dependency is honest; F-021 (synthesizer-raised) flags this, and the MVP mitigation (GC disabled, `verify_on_present=true` in remove tests) is concrete and testable. No new finding.
- **§7.4 byte layout** — re-verified against `02 §3.2` table, byte-offset for byte-offset. Matches.
- **§15.0.1 escalation table** cross-checked against `04 §10`: bench `bench_seed_burst_100m` (row 1) is the gate; the 4-step staged response (sharded redb → log-structured) is consistent with `04 §10`'s own escalation language. No new finding.

---

## Adversarial sweep

**Risk area 1 — F_FULLFSYNC contract on Linux (test #14).** §14.2 #14 is symmetric to #11 (macOS shim). Walk: an LD_PRELOAD that no-ops `fdatasync` plus a power-cut should produce a torn redb root and the recovery state machine in §9.1 must transition Probing → Rebuilding. §9.1 covers this transition explicitly. SOUND.

**Risk area 2 — Phase-0 redb upgrade hidden footgun.** If 4.1 *removed* `Durability::None`, seed mode (§13 / §13.1) would lose its substrate. §15.0's audit checklist explicitly probes for "`Durability::None` and `Durability::Eventual` exist in 4.1?" — the synthesizer correctly hoisted this risk into the gate. If the audit fails, the architecture coordinator must re-pick durability mappings; the doc would need a §13 patch but no architectural rethink. ACCEPTABLE risk — appropriately gated.

**Risk area 3 — §8.4 G2 with `verify_on_present=false` (default).** §13.1 lists default mode at `verify_on_present=false`. §8.4 says the chunker `cas.put(h)` with `verify_on_present=true` re-stats — so the race-mitigation only fires in paranoid mode. MVP says "GC disabled," so the race cannot occur in MVP — the conjunction holds. But once GC lands, the default-mode `verify_on_present=false` will re-expose the race. §16 OQ-15 captures this as the "GC architect" dependency. ACCEPTABLE under MVP-gates-GC.

---

## Summary table

| Original ID | New status | Notes |
|---|---|---|
| F-001 | CLOSED | Storage layout fully resynced to `02 §1/§2/§3/§4`. |
| F-002 | CLOSED | Phase-0 §15.0 with API audit checklist; SLOs marked advisory. |
| F-003 | CLOSED | §13.4 locking model with explicit S3 walkthrough; OQ-4 reversed. |
| F-004 | CLOSED | xxh3-128 split restored byte-for-byte from `02 §3.2`. |
| F-005 | CLOSED | §1 type-naming reconciliation paragraph. |
| F-006 | CLOSED | `{bloom, HWM}` braces rule documented in §8.1 prose. |
| F-007 | CLOSED | §8.1 caller-observability note + §14.2 test #13. |
| F-008 | CLOSED | §8.4 race walk-through + G2 contract + MVP-disable-GC mitigation. |
| F-009 | CLOSED | `effective_fpr_rebuild_multiplier` in BloomConfig + mode table. |
| F-010 | CLOSED | §15.1 expanded migration paragraph. |
| F-011 | CLOSED | `Seed/Default/Paranoid` restored throughout — no `Fast` survivors. |
| F-012 | CLOSED | §15.0.1 staged escalation table; "shard first, swap second" rule. |
| F-013 | CLOSED | Cite updated. |
| F-014 | CLOSED | `crossbeam_channel::bounded(1)` locked in §10; OQ-14a closed. |
| F-015 | CLOSED | FUSE wiring under `--features dedup-index` in MVP. |
| F-016 | CLOSED | Permitted/Forbidden framing applied. |
| F-017 | CLOSED | OQ-3 expanded with counter + warn. |
| F-018 | CLOSED | §11 split into cold/warm/v2-target rows; xychart consistent. |
| F-019 | CLOSED-by-verification | NIT — no action recommended. |
| F-020 | CLOSED | Verified moot. |
| F-021 (new) | ACKNOWLEDGED | Owned in §16 OQ-15; MVP mitigation concrete. |

## Final counts

- **CLOSED:** 20 (all 5 BLOCKERs, all 7 MAJORs, all 6 MINORs, both NITs)
- **PARTIAL:** 0
- **NOT FIXED:** 0
- **REGRESSED:** 0
- **New BLOCKERs introduced:** 0
- **New MAJORs introduced:** 0
- **Synthesizer self-raised follow-ups:** 1 (F-021, MINOR, MVP-mitigated, v2-tracked)

---

## Final word

The architecture document is internally consistent, traceable to its source specs at every load-bearing claim, and honest about the residual unknowns (phase-0 redb upgrade, GC architect dependency, F_FULLFSYNC measured cost). The MVP envelope (N ≤ 50 M, GC disabled, FUSE-single-mount) is well-defined and the gates (bench #1 §15.0.1, phase-0 audit §15.0, telemetry G7 §12) for v2 escalation are concrete. **Ship it to implementation.**

*End of VERIFY-2.md.*
