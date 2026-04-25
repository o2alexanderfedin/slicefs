# VERIFY-1: First-pass audit of ARCHITECTURE.md

**Auditor:** Verifier Architect (adversarial pass)
**Date:** 2026-04-23
**Subject:** `/Volumes/Unitek-B/Projects/file-systems/.planning/research/dedup-index/ARCHITECTURE.md` (first draft, 2026-04-23)
**Cross-checked against:** `SYNTHESIS.md`, `architecture/01-api-and-types.md`, `architecture/02-storage-and-layout.md` (rewritten AFTER ARCHITECTURE.md), `architecture/03-durability-and-recovery.md`, `architecture/04-performance-and-operations.md`, `architecture/COORDINATOR-LOG.md`, the live `slicefs-traits::dedup_index`, `cas-local::mem_dedup_index`, `metadata::mount_lock`, the workspace `Cargo.toml`, and `03-crash-safety.md`.

## Verdict: NEEDS-REVISION

5 BLOCKERs were identified. They cluster around the (acknowledged) post-coordinator rewrite of `02-storage-and-layout.md` and one independent dependency-version bug. Per the prompt's heuristic ("more than 5 BLOCKERs ⇒ re-think, not patch"), we are right at the threshold; however, four of the five BLOCKERs collapse into one root cause (storage-layout drift), so the architecture itself is sound. Patch, do not re-think.

## Severity legend
- **BLOCKER** — must fix before MVP; correctness, dependency, or contract bug.
- **MAJOR**   — should fix before MVP; defer only with explicit risk acceptance.
- **MINOR**   — cleanup, fix in v2.
- **NIT**     — stylistic.

---

## Findings

### F-001 [BLOCKER] Storage layout / paths divergent from current `02-storage-and-layout.md`

**Where:** ARCHITECTURE.md §1 (intro), §4 (overview diagram, "<store_root>/dedup/" subgraph), §7.1 (directory tree mermaid), §7.3 (`header.json`), §7.4 (`bloom.snap` shown as `bloom.current → bloom.snapshot.<gen>`), §9.2 step 6, §15.1 ("`<store_root>/dedup/` layout").

**Issue:** The current `02-storage-and-layout.md` (the one labelled rewritten *after* ARCHITECTURE.md was drafted) picks the path **`<store>/cas/.dedup-index/`**, the redb table name **`dedup_index_v1`**, and a single fixed-name **`bloom.snap`** file (rename-replaced atomically). ARCHITECTURE.md *still* reflects the placeholder layout: `<store_root>/dedup/`, table `hashes_v1`, plus a non-existent `bloom.current → bloom.snapshot.<gen>` symlink scheme. Every diagram, prose paragraph, and rebuild-step reference is wrong.

**Evidence:**
- ARCHITECTURE.md §7.2: `TableDefinition::new("hashes_v1")`. `02-storage-and-layout.md` §2.1: `TableDefinition::new("dedup_index_v1")`. **Direct contradiction.**
- ARCHITECTURE.md §7.1 mermaid shows `Dedup --> Header["header.json"]`, sibling to `Index["index.redb"]`, parented at `<store_root>/dedup/`. `02-storage-and-layout.md` §1 mermaid puts the same files at `<store>/cas/.dedup-index/` and §4 names the manifest `manifest.json` (not `header.json`).
- ARCHITECTURE.md §7.4 invents a "`bloom.current` → `bloom.snapshot.<gen>` symlink" scheme that has no source in any specialist doc. `02-storage-and-layout.md` §3.3 specifies a single fixed-name `bloom.snap` with `bloom.snap.tmp` staging — no symlinks, no generation suffix.
- ARCHITECTURE.md §9.2 step 6 uses generation suffix; step 7 uses `tmp/index.redb.new`; the current spec (§3.3, §5.1) uses `bloom.snap.tmp` and `index.redb.tmp`.
- `COORDINATOR-LOG.md §2 D7` documents the *coordinator's* resolution: *file name `header.json`, location `<store_root>/dedup/`*. That resolution is stale — the rewritten `02-storage-and-layout.md` overrides it to `manifest.json` at `<store>/cas/.dedup-index/`.

**Recommendation:** Reconcile by **adopting the rewritten `02-storage-and-layout.md` verbatim** (it is the current spec; it considered submodule symmetry, the CAS-as-derivable-cache property, and backups). Update ARCHITECTURE.md §1 ("`<store_root>/dedup/`" → "`<store>/cas/.dedup-index/`"), §4 graph (rename `Dedup` subgraph), §7.1 mermaid, §7.2 (`hashes_v1` → `dedup_index_v1`), §7.3 (rename `header.json` to `manifest.json`, retain the field union per coordinator D7), §7.4 (drop the symlink scheme; document the rename-flip from §3.3 of the source), §9.2 (rename `tmp/*.new` to `*.tmp`), §15.1 ("layout" sentence), §17 glossary (no path strings used, but verify). If the coordinator wants to *override* the rewritten `02-storage-and-layout.md` (e.g., keep `header.json` per D7), it MUST also revert `02-storage-and-layout.md` to match — having two living source-of-truth docs is a process bug.

### F-002 [BLOCKER] redb dependency version mismatch with workspace

**Where:** ARCHITECTURE.md §1 ("substrate is **redb 4.1**"), §2.G7, §15.1 (`RedbDedupIndex` over redb 4.1), §17 glossary "redb".

**Issue:** The workspace `Cargo.toml` declares `redb = "3.1"`. ARCHITECTURE.md (and SYNTHESIS.md, and the four specialists) all uniformly cite redb 4.1, including specific 4.1 features and capabilities (e.g., "redb 4.1 exposes `Database::compact()` and free-page metrics" — `04-performance-and-operations.md §8`). Either:
- The workspace will be upgraded to 4.x as part of this work (not stated anywhere; needs confirmation and a phasing entry); or
- The architecture is built against a non-existent dependency version in this repo.

This is *not* a documentation typo — `Database::compact()` API and the `Builder::set_page_size` method are 2.x/3.x features, but the `Durability::Eventual` enum variant and group-commit semantics referenced throughout (`03-durability-and-recovery.md §2`, `04-performance-and-operations.md §1`) need version verification against whatever `redb` major version actually ships. Per redb's CHANGELOG, the pre-`v2.0` `Durability::None` was deprecated/renamed; the `compact()` API stabilized at 1.0; `Database::stats()` exists across 1.x–4.x but field names (e.g. `free_pages`, `tree_height`) have shifted.

The mention of `Value for ()` in §7.2 fallback is also version-conditional: it has been stable since redb 1.0, but the specific signature differs between 3.x and 4.x.

**Evidence:**
- `/Volumes/Unitek-B/Projects/file-systems/Cargo.toml`: line `redb = "3.1"`.
- ARCHITECTURE.md §1 line: "The design substrate is **redb 4.1** (single-file COW B+tree)".
- No phase or task in §15 (Phasing) lists "bump redb to 4.x" as a prerequisite.

**Recommendation:** Pick one of three:
1. Bump workspace `redb = "4.1"` and add a phase-0 task "verify all existing `metadata` redb call sites compile against 4.x" — ARCHITECTURE.md §15.1 should call this out.
2. Re-target architecture to redb 3.1 (the actually-shipping version) and audit every API claim in §7.2, §9.2, §13, §15.2 against 3.1 docs.
3. Pin a version in `[workspace.dependencies]` and document the rationale in §1.

Until resolved, **all benchmark SLOs in §11 are based on a version that is not in the repo.** This is a stability/correctness issue for the contract.

### F-003 [BLOCKER] `mount.lock` "folded" decision contradicts existing `mount.lock` semantics

**Where:** ARCHITECTURE.md §7.1 (`Lk["mount.lock<br/>folded"]`), §16 OQ-4, §17 glossary (implicit).

**Issue:** `COORDINATOR-LOG §4 OQ-4` and ARCHITECTURE.md §16 OQ-4 both record "Folded into `mount.lock`" as the proposed default. But the existing `crates/metadata/src/mount_lock.rs` is **not a multi-process exclusion lock** — it is a *dirty-mount canary*: a regular file whose presence on next mount means "previous mount exited uncleanly" (returns `DirtyMount` error). It is removed via `Drop`. Two SliceFS processes racing for the same store will both see the file absent at start, both call `std::fs::write(b"")`, and both proceed. This is documented at `mount_lock.rs:41` (`acquire_mount_lock`).

If ARCHITECTURE.md is going to inherit this for DedupIndex coordination — which the doc implies under "open()" semantics in §5.2 ("`open()` … attaches to an existing one") — then **two-process races for the same `<store>/cas/.dedup-index/` are unguarded**. Two `RedbDedupIndex::open` calls on the same path will fail in unspecified ways (redb's own `index.redb.lock` advisory lock would catch it for the redb file *if* present, but the bloom snapshot path and `manifest.json` path are not flock-protected).

**Evidence:**
- `crates/metadata/src/mount_lock.rs:41` — `if lock_path.exists() { return Err(DirtyMount); }` — TOCTOU race; not an exclusion lock. There is **no `flock` / `fcntl(F_SETLK)`**.
- ARCHITECTURE.md §16 OQ-4: "Folded into `mount.lock`" — does not address the race.
- `01-api-and-types.md §5.1 + §6` documents an "Opening" state but does not specify what happens on concurrent `open()`.

**Recommendation:** Either:
1. Strengthen `mount_lock.rs` to use `flock(LOCK_EX | LOCK_NB)` (Linux) or `fcntl(F_SETLK)` (cross-platform) and amend ARCHITECTURE.md §16 OQ-4 to document the exclusion semantics. Add a failure-injection test in §14.2 (a 13th scenario).
2. If keeping the current dirty-canary semantics, document explicitly in ARCHITECTURE.md §5 / §9 that `RedbDedupIndex::open` relies on **redb's own `.lock` file** for multi-process exclusion *of the redb file* and that bloom/manifest concurrent access is **caller's responsibility** (i.e. the FUSE layer must single-mount the store).
3. Add a phase-0 task to introduce a real `flock`-based store-wide lock that *both* the metadata WAL recovery and the new DedupIndex use. This is the only choice consistent with G2's "Bounded RAM" claim: a races-on-bloom case will silently double the snapshot file and the recovery story is undefined.

S3 (the prompt's adversarial scenario "redb's lock file is left behind after `kill -9`") is left **unanswered** by the doc — this is the symptom of the same bug.

### F-004 [BLOCKER] Adversarial scenario S6 — torn 1.2 GB bloom snapshot rename — is silently incomplete

**Where:** ARCHITECTURE.md §7.4, §9.2 step 6, §14.2 scenario #4 (single-byte flip), §14.2 scenario #7 (rename-then-fsync crash).

**Issue:** The rewritten `02-storage-and-layout.md §Top three risks` (item 2) explicitly flags: *"Bloom snapshot torn write at 1.2 GB. A 1.2 GB rename is not atomic at the device level beyond AWUPF (4 KiB)."* and routes mitigation through "xxh3-128 footer + recovery-from-redb fallback." ARCHITECTURE.md §7.4 mentions an `index_root_hash: [u8;32]` cross-check and a header CRC32C, but **does not** explicitly walk the case where the *tmp file* is partially written, fsync'd, renamed, and then the post-rename `fsync(parent)` is interrupted.

The drafted §14.2 covers (a) single-byte payload flip → header xxh3 detects (#4) and (b) crash between rename and parent fsync → mount may not see new file (#7). It does NOT cover: a torn write *within* the 1.2 GB payload that the header CRC32C does not see (header is computed over the first 60 bytes only per §7.4). The footer xxh3-128 in `02-storage-and-layout.md §3.2` covers that case, but `ARCHITECTURE.md §7.4` describes the file format using a `BloomSnapshotFile` mermaid that lists `footer_crc32c: u32` — which is **the wrong checksum**: a 32-bit CRC over a 1.2 GB payload has a non-trivial collision probability against random NAND bit-flip, and is not the xxh3-128 specified in `02-storage-and-layout.md §3.2`.

In short: **payload integrity in the architecture diagram is downgraded** from xxh3-128 (specified) to CRC32C (drafted). At 1.2 GB, this matters.

**Evidence:**
- ARCHITECTURE.md §7.4 mermaid lines: `header_crc32c: u32` ... `bitmap_payload: bytes` ... `footer_crc32c: u32`.
- `02-storage-and-layout.md §3.2` header bytes 56–60: "payload_xxh3_lo32 — low 32 bits of payload xxh3-128"; footer bytes 0–8: "payload_xxh3_hi64 — high 64 bits of payload xxh3-128"; footer 8–16: "magic_end — ASCII `BL01ENDX`". *No `footer_crc32c` field exists in the spec.*

**Recommendation:** Replace ARCHITECTURE.md §7.4's `BloomSnapshotFile` mermaid with the byte layout from `02-storage-and-layout.md §3.2` verbatim. Add a §14.2 scenario "#13: Torn write in middle of 1.2 GB bloom payload (4 KiB block flipped at offset 600 MB) — xxh3-128 catches → rebuild from redb." Also amend the §9.2 rebuild's step 6 to write the snapshot via *temp + rename + parent-dir-fsync* in that order (currently merged).

### F-005 [BLOCKER] Public type/identifier inconsistency: `RedbDedupIndex` (preferred) vs `PersistentDedupIndex` (live in source docs)

**Where:** ARCHITECTURE.md uses `RedbDedupIndex` consistently. `03-durability-and-recovery.md §1, §2` and `04-performance-and-operations.md §1, §2, §6, §9` use `PersistentDedupIndex`. `02-storage-and-layout.md §3.3 sequence` uses `PersistentDedupIndex`. `01-api-and-types.md §1.1` is the *only* source spec that renames to `RedbDedupIndex` (with explicit rationale at line ~22).

**Issue:** `COORDINATOR-LOG §5` ratifies `RedbDedupIndex`, but the three other specialist docs were not updated in tow. ARCHITECTURE.md is correct; the *source specs* are inconsistent with each other. From an audit standpoint of "is the unified document internally consistent," this is fine *for the unified doc* — but it is a bug in the *system of documents* that cite each other. The verifier flags it because:
- A reader cross-referencing `[03 §2]` from ARCHITECTURE.md §8.1 will see the term `PersistentDedupIndex` in the source and infer a different type.
- The `class` definitions in mermaid blocks of §6 will not match doc-strings if the implementer copies them.

**Evidence:**
- `04-performance-and-operations.md §6`: "`PersistentDedupIndex::stats()`".
- `03-durability-and-recovery.md §1` mermaid: `IDX as PersistentDedupIndex`.
- `02-storage-and-layout.md §3.3 sequence`: `participant App as PersistentDedupIndex`.
- `01-api-and-types.md §1.1`: `RedbDedupIndex` (the rename).
- ARCHITECTURE.md throughout: `RedbDedupIndex`.

**Recommendation:** ARCHITECTURE.md should add an explicit one-paragraph reconciliation note in §5.2 ("the source specs use `PersistentDedupIndex`; this document renames to `RedbDedupIndex` per `[01 §Naming note]` and `[COORDINATOR-LOG §5]`. References to `PersistentDedupIndex` in 02/03/04 are stylistic; the binding name is `RedbDedupIndex`."). Or — better — issue a coordinated rename pass on 02/03/04 and bump their dates. Either resolves the audit concern.

---

### F-006 [MAJOR] Insert sequence step 13 (HWM after bloom) violates I5 wording

**Where:** ARCHITECTURE.md §3 Invariant `I5` ("HWM advances only after index commit returns"), §8.1 sequence diagram steps 12–14 (`bloom.set_all` then `HWM.fetch_add`).

**Issue:** Invariant I5 says "Visibility barrier: HWM advances only after index commit returns." The §8.1 mermaid sequence shows: redb commit → bloom.set_all → HWM.fetch_add → reply. This *technically* satisfies I5 (HWM advances after commit), but the diagram's ordering of bloom-then-HWM creates a window where another thread can: (a) read bloom hit, (b) read redb (sees the entry), (c) yield, (d) read HWM (still old). If any caller uses HWM as a visibility barrier (which is the *purpose* of HWM per `01 §2.1`), they may incorrectly conclude "my insert hasn't landed" while in fact it has. This is benign for correctness (still no FP) but breaks the documented "visibility-after-HWM" contract.

The §10 lock map also documents this ordering: `commit + F_FULLFSYNC → bloom.insert_all → HWM.store`.

**Evidence:**
- §3 I5 wording: "HWM advances only after index commit returns."
- §8.1 step 13: `IDX->>IDX: bloom.set_all(hashes)`. Step 14: `IDX->>IDX: HWM.fetch_add(N, AcqRel)`.

**Recommendation:** Either flip the order (HWM first, bloom second — both happen-after commit) and document the chosen ordering rule, or add a sentence to §3 I5 clarifying that `HWM` is "first-monotonic" (advances no earlier than commit) but not necessarily strictly tied to bloom visibility. The simpler fix: state in §8.1 that "bloom + HWM are observably equivalent for I1 — the strict-ordering contract is `commit ▸ {bloom, HWM} ▸ caller-reply`; the `{bloom, HWM}` braces are unordered."

### F-007 [MAJOR] Adversarial scenario S1 not addressed: post-Ok caller observability

**Where:** Trait contract `crates/slicefs-traits/src/dedup_index.rs:60` (`insert` doc-comment: "Idempotent: inserting a hash that already exists is a no-op."). ARCHITECTURE.md §8.1 ("oneshot reply Ok(())"), §13 (`DurabilityMode::Default` uses `Eventual`), §16 OQ-11.

**Issue:** S1 from the audit prompt: `insert(h)` returns Ok via the batcher; crash 1 ns later before the redb group-commit window of 200 ms expires. On next mount, redb's last-committed root may be *earlier* than the txn the caller saw. The caller's "Ok" is **lying about durability** — but this is correct under §3 I4 (CAS-as-truth) since the rebuild walker will catch it.

The risk is callers above the trait that **cache the Ok reply locally** and short-circuit a future `lookup`. The COORDINATOR-LOG OQ-11 explicitly lists this audit as not-yet-done. ARCHITECTURE.md §16 OQ-11 says "Audit + write `kill -9` test (added to §14.2)" but §14.2 has no such test (test #1 is "before redb commit," not "after Ok-reply, before group-commit fsync").

**Evidence:**
- ARCHITECTURE.md §8.1 step 16: `IDX-->>Caller: oneshot reply Ok(())` — issued *after* `redb.commit(durability=Eventual)` returns. Per redb 3.x docs, `Durability::Eventual` does NOT guarantee post-power-loss durability; it returns when the txn is staged (in OS page cache or redb's own buffer), not when the device has flushed.
- `03-crash-safety.md §0` table: false positive = catastrophic.
- `03-durability-and-recovery.md §2 P5/P6`: documents the case as benign — but only because the *next mount* does not promise `Present`. There is no guarantee that *callers* don't promise it.

**Recommendation:** Add to §14.2 the actual test: "#13. Caller calls `insert(h)`, awaits `Ok`, immediately `kill -9` before group-commit window expires. On next mount, `lookup(h)` may return `Absent` — verified non-FP. (Validates: caller cannot assume Ok ⇒ Present after mount.)" Plus, add a one-paragraph §5.1 doc-note: "*A successful `insert` reply does NOT promise post-crash visibility on its own; callers needing that contract must also call `flush()` (which forces F_FULLFSYNC and waits for ack)*." This is an OQ-11 audit gap that the verifier exposes; the trait contract on line 60 is silent on this and would be a correctness bug if a caller assumed otherwise.

### F-008 [MAJOR] GC subsystem: race between concurrent `remove(h)` and `insert(h)` not specified

**Where:** ARCHITECTURE.md §8.3 (Remove sequence), §16 (no OQ for this), §3 I3 (Remove ordering).

**Issue:** `COORDINATOR-LOG §3 G2` flags this gap: "can a hash be inserted while GC is removing it?" The log's recommended action is "Document the assumption in the draft and flag for the GC architect." ARCHITECTURE.md does **not** document the assumption. If the chunker calls `insert(h)` while GC is in the middle of `remove(h)` for the same `h`:
- Insert path: CAS write → CAS fsync → redb txn(insert) → bloom set.
- Remove path: redb txn(remove) → redb fsync → CAS unlink.
- redb's single-writer mutex serializes the two redb txns, but the CAS-write and CAS-unlink can interleave: insert writes the CAS file, remove unlinks it, insert's redb txn lands → I1 violation (redb says Present, CAS file gone). I1 is *not* protected by I2 ordering alone in this case.

**Evidence:**
- §3 I2: insert ordering. §3 I3: remove ordering. Neither addresses `insert(h)` and `remove(h)` *concurrent* for the same `h`.
- ARCHITECTURE.md §8.3 step 7: "GC->>CAS: unlink(cas_path(h))  /* AFTER index commit returns */" — but it is silent on concurrency with `insert(h)`.

**Recommendation:** Add a short §8.4 ("Insert/remove race") stating that the GC layer must hold an exclusive lock on each chunk hash for the duration of the (remove-from-redb, unlink-CAS) sequence — typically by checking refcount-zero under a CAS-block-level lock — and that the chunker's `insert(h)` *must* re-`stat(cas_path(h))` after `cas.put` if `verify_on_present=true`. Add a §16 OQ for this. Until the GC architect specifies the locking, the existing G2 acknowledgment in COORDINATOR-LOG should be lifted into ARCHITECTURE.md §16.

### F-009 [MAJOR] Bloom rebuild trigger threshold inconsistency

**Where:** ARCHITECTURE.md §8.3 (`drift past effective_fpr > 4 × design_fpr OR drift_ratio > 0.20`); §13 BloomConfig (`drift_rebuild_ratio: f64, // 0.20`).

**Issue:** Cross-checking against `04-performance-and-operations.md §7`:

> *"Rebuild trigger: `effective_fpr > 4 × design_fpr` OR `drift_ratio = removed_since_rebuild / capacity > 0.20`. Either condition schedules a background bloom rebuild from the live redb table — same code path as recovery, runs without taking the mount offline."*

ARCHITECTURE.md §8.3 quotes the same threshold. So far consistent. But:
- §13 BloomConfig has *only* `drift_rebuild_ratio: f64` (0.20). The `4 × design_fpr` threshold is **not represented in the config struct**. Implementer reading just §13 won't know it exists.
- `13.1 Mode preset summary` does not vary `drift_rebuild_ratio` across fast/default/paranoid; if paranoid wants tighter drift handling (it should), the config should expose this.

**Evidence:**
- §13 BloomConfig field list: `drift_rebuild_ratio: f64, // 0.20 — trigger background rebuild` — only one knob.
- §8.3 mentions both knobs.
- §13.1 mode preset table — no row for either.

**Recommendation:** Add to §13 BloomConfig a `effective_fpr_rebuild_multiplier: f64` (default 4.0) field. Add row in §13.1 mode preset for both. Or document in §13 that the multiplier is hardcoded at 4.0 and won't be configurable in MVP; flag for v2.

### F-010 [MAJOR] Upgrade path from `MemDedupIndex` not documented in body

**Where:** ARCHITECTURE.md §15.1 (one-line: "Migration: **none required** — no production binary uses `MemDedupIndex`"). `cas-local::lib.rs` re-exports `MemDedupIndex`; the existing `cas-local` consumers may rely on this re-export.

**Issue:** The "no production caller" claim from `COORDINATOR-LOG G3` is correct as of repo grep: `MemDedupIndex` is exported but no FUSE-side wiring exists yet. However: (a) the trait extension in §5.1 adds three new methods (`flush`/`verify`/`stats`) with default no-op impls — the trait change is a backwards-compat win, but the doc should explicitly state that **`MemDedupIndex` requires no source change**, and (b) any downstream tests that use `MemDedupIndex` continue to work — but the impl deliberately *does not* gain `flush()` semantics, so a test that calls `idx.flush()` and expects work to land on disk will silently no-op. This is correct, but should be doc'd.

**Evidence:**
- ARCHITECTURE.md §5.1: trait additions with default impls.
- ARCHITECTURE.md §15.1: one-line migration claim.
- `cas-local::mem_dedup_index.rs`: no `flush` impl; will use the default no-op.

**Recommendation:** Replace the §15.1 one-liner with a small subsection "Migration of `MemDedupIndex` consumers" stating: (i) Source change: none. (ii) Test-side caveat: tests calling `idx.flush()` on `MemDedupIndex` get a no-op; this is correct. (iii) The `cas-local` re-export remains. No additional file movement.

### F-011 [MAJOR] SLO numbers vs batcher window are arithmetically consistent but unjustified at the boundaries

**Where:** ARCHITECTURE.md §11 (SLO table, "Insert throughput, seed mode ≥ 100 K ins/s"), §10 batcher (batch_size=10 000, coalesce_window=2 ms default / 20 ms seed), §13 (matching defaults).

**Issue:** Quick math: at 100 K ins/s, with batch_size 100 000 in seed mode and 20 ms coalesce window, the batcher needs to run 1 commit/s with batches of 100 000 — meaning each commit must complete in < 1 s including fsync. That is plausible *if* `commit p99 ≤ 5 ms` (default mode SLO) holds in seed mode. **The doc does not explicitly tie these together** — there is no SLO row "seed-mode commit p99". The batcher in seed mode uses `Durability::None` per §13.1, but the doc later says "redb `Eventual`" in §13.1 for `fast` (not seed) — there's an ambiguity over whether `Seed` mode (per `01 §2.2`) is the same as `Fast` mode (per ARCHITECTURE.md §13.1). The 4 specialists used `Seed`, ARCHITECTURE.md collapsed it to `Fast`. The reader needs to know whether seed throughput is realized via `Durability::None` (no fsync, naïve > 100 K easy) or `Durability::Eventual` (group-commit, depends on redb 4.x behavior).

**Evidence:**
- `01-api-and-types.md §2.2`: `enum DurabilityMode { Paranoid, Default, Seed }` — three modes.
- ARCHITECTURE.md §13: `enum DurabilityMode { Fast, Default, Paranoid }` — three modes, but **renamed** `Seed → Fast`.
- ARCHITECTURE.md §13.1: `Fast | redb Eventual`. `04-performance-and-operations.md §1`: "seed throughput" with separate seed-mode batch params. The renaming silently changes the durability of "seed" from `Durability::None` (per `01 §2.2`) to `Durability::Eventual` (per ARCHITECTURE.md §13.1). This is a semantic change.

**Recommendation:** Reconcile the names. Either restore `DurabilityMode::Seed` (per `01`) and add a fourth `Fast` mode, or merge `Seed` into `Fast` and explicitly document that "seed-mode" is `Durability::Eventual + 20 ms coalesce` (loses recent inserts on crash, recovers from CAS). If the latter, update §11 SLO row to add "seed throughput justification: `Durability::Eventual` + 20 ms coalesce + 100 000 batch / commit." If the former, add a column in §13.1.

### F-012 [MAJOR] `bench_seed_burst_100m` gate is documented but its falsification → v2-escalation pathway is unclear

**Where:** ARCHITECTURE.md §11 (gate row), §15.1 ("clears 100 K ins/s on reference NVMe (gate; if not, escalate to v2)"), §15.2 ("Seed-bench misses ≥ 100 K ins/s: add a per-shard concurrency split..."), §14.3 bench #1.

**Issue:** ARCHITECTURE.md says if the bench misses, **escalate**. But:
- §15.1 says "escalate to v2".
- §15.2 says "add a per-shard concurrency split (4 redb DBs sharded by hash prefix), or escalate to log-structured early."
- These are different escalation pathways. Sharding is a *patch*, not a v2 swap.

The implementer/operator reading §15.1 will see "v2" as binding; the architect reading §15.2 will see "shard or v2" — different decisions. The decision criterion is missing.

**Evidence:**
- ARCHITECTURE.md §15.1: "escalate to v2".
- ARCHITECTURE.md §15.2: "add a per-shard concurrency split, or escalate to log-structured early."
- `04-performance-and-operations.md §10`: "Bench 1 doubles as the v2-escalation gate."

**Recommendation:** Pick: shard first (cheaper), or skip directly to v2 (cleaner)? The benchmark numbers in `01 §10 #1` and `04 §10 #1` would let an architect pick — but the doc punts. Add a §15.1.1 "Gate failure response": if `bench_seed_burst_100m < 100 K ins/s`, then (a) try sharded redb (4 DBs), (b) re-run; (c) if still failing, escalate to v2 log-structured. Make the criterion explicit.

---

### F-013 [MINOR] Glossary term `DDT` references `[SYNTHESIS §1]` — section not present

**Where:** ARCHITECTURE.md §17 glossary `DDT — … [SYNTHESIS §1]`.

**Issue:** `SYNTHESIS.md §1` is "Problem statement," which references the ZFS DDT antipattern but does not cite §1 of `04-prior-art.md`. The original cite path is `04-prior-art §1`, which is what `SYNTHESIS §1` indirectly cites. Audit-pedantic.

**Recommendation:** Change to `[SYNTHESIS §1, 04-prior-art §1]`.

### F-014 [MINOR] §10 batcher MPSC: `tokio::sync::oneshot` vs `crossbeam_channel::bounded(1)` — implementation choice unspecified

**Where:** ARCHITECTURE.md §10 ("Reply: per-request oneshot channel"). `04-performance-and-operations.md §2`: "per-request `tokio::sync::oneshot` (or `crossbeam_channel::bounded(1)` for sync callers)".

**Issue:** ARCHITECTURE.md hides the choice. If the `slicefs-dedup` crate is sync-only (no `tokio` dep), then `crossbeam` is the right call; if it pulls tokio, the reply could be async. Implementer needs to know.

**Recommendation:** Specify: "per-request `crossbeam_channel::bounded(1)` (sync, no tokio dep)" or commit to tokio. If MVP is sync, lock crossbeam.

### F-015 [MINOR] §15.2 v2 entry "FUSE-layer integration doc (`05-fuse-integration.md`) — covers G1" lists an integration deliverable as a *v2* item

**Where:** ARCHITECTURE.md §15.2.

**Issue:** §1 says "FUSE-layer wiring inside `slicefs-cli` (deferred to `05-fuse-integration.md`)" is **out of scope** for *this* document but does not say it's deferred to v2 — only to a sibling doc. Putting the FUSE wiring under §15.2 v2 implies the production binary in MVP will not actually mount the new dedup index. That cannot be right; a `RedbDedupIndex` that is not wired to the chunker's `bloom_check` / `lookup` / `insert` paths is a no-op subsystem.

**Recommendation:** Either (a) move "FUSE-layer integration" to MVP (§15.1) and mark `05-fuse-integration.md` as a phase-0 prerequisite, or (b) clarify that the MVP ships a usable `RedbDedupIndex` *behind a feature flag*, with the FUSE wiring landing as a fast-follow.

### F-016 [MINOR] §14.1 property-test plan permits `RedbDedupIndex` to "demote `DefinitelyAbsent` to `Absent`" — but never *promote* `Absent` to `DefinitelyAbsent`

**Where:** ARCHITECTURE.md §14.1 ("the on-disk impl can demote `DefinitelyAbsent` to `Absent` if a snapshot rebuild caused the bloom to be repopulated from redb").

**Issue:** Demotion is fine. But the wording is confusing: a fresh-rebuilt bloom would be *more* aggressive (more bits set, more FPs), so it could go *Definitely-Absent → Absent* (extra work) but never go *Absent → Definitely-Absent* on the same hash without remove. Should be stated more clearly.

**Recommendation:** Reword: "...the on-disk impl may answer `Absent` (extra `lookup` cost) where `MemDedupIndex` answered `DefinitelyAbsent`; both are within the trait contract. The reverse is not permitted: an `Absent` from one impl must not become `DefinitelyAbsent` in the other for the same hash without an intervening `remove`."

### F-017 [MINOR] §16 open question table: OQ-3 ("RetryPolicy config knob") proposes "No (fail fast)" — verify this is the safe default

**Where:** ARCHITECTURE.md §16 OQ-3.

**Issue:** "No retry" is the safe default *if* the FUSE layer above has a retry middleware. The `01 §8 OQ O-3` explicitly notes this: "let the FUSE layer's retry middleware deal with it — but this couples us to a FUSE layer that doesn't have such middleware yet." Without the upstream middleware, transient redb errors (e.g. EBUSY) will surface as `CasError` to user-space write paths and likely manifest as FUSE I/O errors. Per the developer-profile directive ("frustrations: regression"), this could be a regression vector.

**Recommendation:** Keep "No (fail fast)" but add a sentence: "*Until FUSE-layer retry middleware exists (G1), transient `EBUSY` from redb will surface as user-visible I/O errors. Mitigation: redb's own internal retry on lock-file contention plus a `tracing::warn!` and a `dedup_index.transient_errors_total` counter to alert if observed in the wild.*" Or, lock OQ-3 to "Yes, with single-attempt 50 ms retry" until G1 lands.

### F-018 [MINOR] Recovery N=10⁹ time of "1.2 h" / "12 min" appears nowhere in ARCHITECTURE.md but is in `03 §6`

**Where:** ARCHITECTURE.md §9.3 xychart (uses 4320 s = 1.2 h for x=1000 M).

**Issue:** The MVP cuts off at N≤50 M (§15.1). At N=10⁹ the rebuild takes 1.2 h offline — that's outside the MVP envelope, and §11 SLO row says "1 B chunks: 30 min offline / online above 50 M (v2)". 30 min ≠ 1.2 h. The xychart shows ~1.2 h (4320 s); §11 says 30 min (1800 s). One is wrong.

**Evidence:**
- §9.3 xychart: y-value 4320 at x=1000.
- §11 SLO row: "≤ 30 min offline / online above 50 M (v2)" (1800 s).

**Recommendation:** Reconcile. Either the SLO is the ceiling (30 min) and the xychart is wrong (drop the 4320 point), or the xychart is the cold-cache reality and the SLO is unrealistic. Per `03 §6`, cold rebuild at N=10⁹ is ~1.2 h. Recommend: change §11 to read "≤ 1.2 h offline (cold) / ≤ 12 min warm / 30 min target requires per-shard parallel walker (v2)".

### F-019 [NIT] §17 glossary entry "FN" / "FP" — wording is fine but capital `Index says ... for a hash` is informal

**Where:** ARCHITECTURE.md §17 ("FN — False Negative. Index says Absent for a hash that is in CAS.").

**Recommendation:** No action. Stylistic.

### F-020 [NIT] Mermaid diagrams: a few use `<br/>` inline, others use `<br>`. Renderer-tolerant but inconsistent.

**Where:** §3, §4, §6, §7.1, §7.3, §7.4, §8.x, §9.x, §10, §12.

**Recommendation:** Pick one (`<br/>` is more correct XML); pass through the doc with a single search/replace.

---

## Approved sections (sign-off)

The following sections are internally consistent, traceable to their cited sources, and require no fix:

- **§2 (Goals & Non-goals)** — all goals trace to invariants or specialist sections; non-goals consistent with `SYNTHESIS §8` and `COORDINATOR-LOG §1`. Reasoning: cross-checked every G/NG against source citations; no drift.
- **§3 (Invariants)** mermaid + table — invariants I1–I12 match `03-durability-and-recovery.md §1` verbatim. The hard/soft classification holds. No issues.
- **§5.3 (Errors)** — `DedupIndexError` enum matches `01-api-and-types.md §2.3` line-for-line. `From<DedupIndexError> for CasError` mapping is correct against the live `slicefs-traits` `CasError::Index/Io` variants.
- **§8.2 (Lookup sequence)** — diagram is correct; the bloom-miss / bloom-hit / redb-miss / redb-hit / verify-on-present branches are exhaustive and faithful to `03 §3`.
- **§9.1 (Recovery state machine)** — states match `03 §5` exactly.
- **§12 (Observability)** — counter/gauge/histogram tree matches `04 §6` G1–G7. Stack choice (`tracing` + `metrics` + optional `prometheus`) consistent with `COORDINATOR-LOG G4`.
- **§15.3 (Out of scope)** — correct, mirrors `SYNTHESIS §8 Out of scope` and `01 §1`.

---

## Summary table

| ID | Severity | Section | One-line |
|---|---|---|---|
| F-001 | BLOCKER | §1, §4, §7.1–§7.4, §9.2, §15.1 | Storage layout / paths / table name / file names diverge from rewritten `02-storage-and-layout.md` (`<store>/cas/.dedup-index/`, `dedup_index_v1`, `manifest.json`, `bloom.snap`). |
| F-002 | BLOCKER | §1, §15.1 | redb 4.1 cited but workspace `Cargo.toml` declares redb 3.1; no upgrade phase listed. |
| F-003 | BLOCKER | §7.1, §16 OQ-4 | "Folded into mount.lock" — but existing `mount.lock` is a dirty-canary, not an exclusion lock; race unguarded. |
| F-004 | BLOCKER | §7.4, §14.2 | Bloom snapshot footer downgraded from xxh3-128 (in spec) to CRC32C (in draft); 1.2 GB payload integrity weakened. |
| F-005 | BLOCKER | throughout | Type name drift — `RedbDedupIndex` (here) vs `PersistentDedupIndex` (02/03/04 source specs). |
| F-006 | MAJOR | §3 I5, §8.1, §10 | Insert step ordering (bloom-then-HWM) creates HWM-visibility window; either flip or document `{bloom, HWM}` braces. |
| F-007 | MAJOR | §8.1, §14.2, §16 OQ-11 | S1 (caller-cache after Ok-reply, before group-commit fsync) not tested; caller-redrive contract underdocumented. |
| F-008 | MAJOR | §8.3, §16 | Race between concurrent `insert(h)` and `remove(h)` not specified; G2 not lifted from COORDINATOR-LOG into doc. |
| F-009 | MAJOR | §13, §13.1 | `effective_fpr_rebuild_multiplier` (4×) not in BloomConfig; only one of two drift triggers exposed. |
| F-010 | MAJOR | §15.1 | `MemDedupIndex` migration story is one line; should specify trait-default no-op behavior. |
| F-011 | MAJOR | §13, §13.1, §11 | `Seed` mode (specs) silently renamed to `Fast` (draft) with implicit `Durability::Eventual` change. |
| F-012 | MAJOR | §15.1, §15.2 | Gate-failure escalation pathway ambiguous: shard or v2-skip? |
| F-013 | MINOR | §17 | Glossary cite `[SYNTHESIS §1]` better as `[SYNTHESIS §1, 04-prior-art §1]`. |
| F-014 | MINOR | §10 | Reply channel: tokio::oneshot vs crossbeam — pick one. |
| F-015 | MINOR | §15.2 | "FUSE-layer integration" in v2 list looks like the MVP ships a non-wired DedupIndex. |
| F-016 | MINOR | §14.1 | Property-test demote/promote rule: clarify direction. |
| F-017 | MINOR | §16 OQ-3 | "No retry" default may regress until G1 ships; add transient-error counter and tracing warn. |
| F-018 | MINOR | §9.3, §11 | N=10⁹ rebuild time inconsistent: xychart says 4320 s, SLO says 1800 s. |
| F-019 | NIT | §17 | Glossary wording — informal but acceptable. |
| F-020 | NIT | mermaid blocks | `<br/>` vs `<br>` mixed. |

---

## Adversarial scenario audit (S1–S8)

| # | Scenario | Forbidden? | Verdict |
|---|---|---|---|
| S1 | Caller cached `Ok` 1 ns before crash; later `lookup → Present` post-recovery? | Should be: NO (Absent acceptable). | **Gap** — see F-007. Doc does not forbid the *caller* assumption; only the *index* assumption. |
| S2 | F_FULLFSYNC silently dropped on Linux mount that lies | Detection? | **Partially addressed** — §14.2 #11 is macOS-only; equivalent Linux test for `fdatasync` lying virtualization is missing. MAJOR. |
| S3 | redb lock file left after `kill -9` | `open()` recovers? | **Not addressed**. See F-003. Doc says nothing about cleanup; redb 3.x does support `Database::open` after kill, but the bloom/manifest paths are undefined. BLOCKER. |
| S4 | Bloom `redb_hwm_at_snapshot` ahead of redb commit log (impossible per §3.3 sequence) | Sequence forbids? | **Addressed correctly** — `02 §3.3, §5.1` enforces "bloom-after-redb-commit"; ARCHITECTURE.md §8.1 step 12 (`bloom.set_all` after commit) inherits this. A torn snapshot with a forward HWM is detected via `index_root_hash` cross-check (§7.4). NO action needed; sound. |
| S5 | `slicefs reindex` while volume mounted; backups (rsync) during runtime | Lock semantics? | **Not addressed**. F-003-adjacent. The doc does not specify whether `reindex` requires umount or runs concurrently. MAJOR — recommend §16 OQ-15. |
| S6 | 1.2 GB bloom snapshot rename not atomic past 4 KiB AWUPF; spec flags it | Detected? | **Partially addressed but downgraded** — see F-004. The spec's xxh3-128 catches it; the draft uses CRC32C. BLOCKER. |
| S7 | `Durability::None` loses last 200 ms; do callers rely on durability? | Caller-redrive? | **Same as F-007.** OQ-11 records this; §14.2 lacks the test. MAJOR. |
| S8 | Insert idempotent + bloom-after-commit: retry sees `bloom_check=false`, but caller already saw `Present`? Walk through. | I5/I6 hold? | **Sound.** If a caller saw `Present` once, the only way `bloom_check` later returns `false` is recovery from a stale snapshot whose `redb_hwm_at_snapshot < hwm`; in that case `lookup` falls through to redb anyway and returns `Present`. The bloom is non-authoritative (I6); `bloom_check=false → DefinitelyAbsent` is only valid if no recent snapshot rebuild rolled it back. The retry-after-Ok semantics flow correctly: a retry of `insert(h)` when `h` is already there lands on the redb idempotent insert (no-op), and bloom is set again (no-op). NO action needed; sound. |

---

## Performance claims audit

| Claim | §11 row | Source | Confidence |
|---|---|---|---|
| Cold lookup p50 ≤ 80 µs | r1 | `04 §1` cites `02-ssd-friendliness §1` (NVMe random-4K = 50–80 µs). **ESTIMATED** from device-spec arithmetic, not measured. AT-RISK if redb's read-path adds >20 µs of CPU. |
| Cold lookup p99 ≤ 500 µs | r2 | Same source. **ESTIMATED**. Bench plan #3 (`bench_lookup_cold`) would falsify. Plan exists. |
| Warm lookup p50 ≤ 5 µs | r3 | `03-crash-safety §3` (50 ns/probe). **MEASURED** (fastbloom benchmarks public). |
| Warm lookup p99 ≤ 10 µs | r4 | `04 §1` extrapolation. **ESTIMATED**. |
| Insert seed mode ≥ 100 K ins/s | r5 | Open question; §15.1 makes it the **gate**. **AT-RISK** — F-012 ambiguity; bench plan #1 is the falsifier. |
| Insert steady-state ≥ 20 K ins/s | r6 | `04 §1` (`commit_delay≈200 µs`). **ESTIMATED**. |
| Commit p99 default ≤ 5 ms | r7 | `04 §1` arithmetic from F_FULLFSYNC 1–4 ms. **AT-RISK** — Apple Silicon F_FULLFSYNC unmeasured per OQ-12. Bench #5 plans to falsify. |
| Commit p99 paranoid ≤ 12 ms | r8 | `04 §1` worst-case. **ESTIMATED**. |
| Recovery RTO ≤30 s @50 M | r9 | `04 §1` from `03 §3` (1 M entries/s warm). **ESTIMATED**. Bench #6 plans to falsify. |
| Recovery RTO ≤30 min @1 B | r10 | `03 §6`. **AT-RISK** — actual cold rebuild is 1.2 h per `03 §6` table. See F-018. |
| Total RAM ≤ 1.5 GB @1 B | r11 | `04 §5`. **MEASURED** for fastbloom (1.2 GB at 1% FPR is exact); redb cache 256 MB is configurable. SOUND. |

Conclusion: 8 of 11 SLOs are **ESTIMATED** with bench plans to falsify. 1 (F-018) is **AT-RISK** for a doc-internal contradiction. 1 (F-007 caller assumption) is **AT-RISK** for testing gap. 1 is **MEASURED**. The bench plan in §14.3 covers 7 of 11; recovery RTO at 1 B is the gap.

---

## Open Question audit (OQ-1 .. OQ-14)

Each `OQ-N` is checked: is the proposed default the *safe* choice (will not box us in)?

- **OQ-1** New crate. Safe. Splitting later is harder than merging.
- **OQ-2** Fallible flush. Safe. Honest signature.
- **OQ-3** No retry. **At-risk** — see F-017.
- **OQ-4** Folded into `mount.lock`. **Unsafe** — see F-003. Recommend reversing to "separate `dedup.lock` with `flock(LOCK_EX | LOCK_NB)`".
- **OQ-5** Yes to non-destructive `slicefs dedup recover`. Safe.
- **OQ-6** Stable across minor, breakable across major. Safe.
- **OQ-7** Bench before locking. Safe.
- **OQ-8** No online rebuild for N>50 M in MVP. Safe but limits operator ergonomics; flag for v2.
- **OQ-9** `verify_on_present=false` in default. Safe — F8 covers self-heal-on-Present.
- **OQ-10** Manual `slicefs reindex --bloom-capacity 2x`. Acceptable for MVP.
- **OQ-11** Audit + test. **At-risk** — see F-007. Test not yet in §14.2.
- **OQ-12** Bench. Safe.
- **OQ-13** Bench. Safe.
- **OQ-14** Same as OQ-9. Safe.

Recommended override: **OQ-4 (mount.lock)** should NOT default to "folded" — see F-003.

---

## Mermaid diagram audit

| Diagram | Renders? | Matches prose? | Arrow direction OK? | Notes |
|---|:---:|:---:|:---:|---|
| §3 invariants flowchart | yes | yes | n/a | classDef styles fine |
| §4 architecture overview graph | yes | partial | yes | F-001: subgraph `Disk` labels stale paths. |
| §6 RedbDedupIndex classDiagram | yes | yes | yes | composition vs aggregation legend correct |
| §7.1 directory tree | yes | NO | yes | F-001: paths wrong |
| §7.3 HeaderJson classDiagram | yes | NO | n/a | F-001: should be `manifest.json` per current spec |
| §7.4 BloomSnapshotFile classDiagram | yes | NO | n/a | F-004: footer field is wrong checksum |
| §8.1 Insert sequence | yes | yes (modulo F-006) | yes | autonumber 1–17; clear |
| §8.2 Lookup sequence | yes | yes | yes | branching alt/opt structure correct |
| §8.3 Remove sequence | yes | yes (modulo F-008) | yes | brief but clear |
| §9.1 state machine | yes | yes | yes | matches `03 §5` |
| §9.3 xychart | yes | partial | n/a | F-018: 4320 s vs SLO 1800 s |
| §10 concurrency flowchart | yes | yes (modulo F-006) | yes | wait-free / lock-free annotation clear |
| §12 observability flowchart | yes | yes | yes | clean tree |

---

## Summary

The architecture has **5 BLOCKERs**, **7 MAJORs**, **6 MINORs**, **2 NITs** — a total of 20 findings. The 5 BLOCKERs are concentrated in two root causes: (a) the post-coordinator rewrite of `02-storage-and-layout.md` (F-001, F-004 — and partially F-003, F-005), and (b) one independent dependency-version bug (F-002).

Patch path is well-scoped: rewrite §1, §4 graph, §7.1, §7.2, §7.3, §7.4, §9.2 in line with the rewritten `02-storage-and-layout.md`; sync the type name across 02/03/04 specialist docs; resolve the `redb 3.1` vs `4.1` mismatch (most likely by upgrading `Cargo.toml`); strengthen `mount.lock` semantics or split a real exclusion lock. Architecture itself is **sound** — no design re-think needed.

**Verdict: NEEDS-REVISION.** Re-run verifier after fixes.

*End of VERIFY-1.md.*
