# DedupIndex Architecture — Coordinator Log

**Status:** WORKING NOTES — input to the synthesize/verify pair.
**Date:** 2026-04-23.
**Inputs reconciled:** `SYNTHESIS.md`, `01-api-and-types.md`, `02-storage-and-layout.md`, `03-durability-and-recovery.md`, `04-performance-and-operations.md`.
**Companion:** first-draft `ARCHITECTURE.md` written alongside this log.

This document is a coordinator's working log: it surfaces unanimities, names disagreements, flags gaps, consolidates the open questions raised across the four specialist documents, and proposes defaults the unified draft inherits. It introduces no new architectural decisions; where a contradiction had to be resolved to write the draft, the resolution is recorded here as "Recommended decision" with one-sentence rationale.

---

## 1. Cross-cutting decisions reached unanimously

The four specialists agree on the following points; the unified draft binds them as ratified design:

1. **Engine = redb 4.1, fastbloom front, CAS-as-truth recovery.** [SYNTHESIS §5], [01 §1.1], [02 §2], [03 §1, §2], [04 §1].
2. **`&self` interior mutability throughout the trait.** [01 §7], [04 §3], matches existing `MemDedupIndex` and trait surface in `slicefs-traits/src/dedup_index.rs`.
3. **Single redb table, fixed 28-byte key, unit value, SET semantics.** [01 §2.1] (`HASHES = TableDefinition<&[u8;28], ()>`), [02 §2] (`hashes_v1`).
4. **Three durability modes (fast/seed | default | paranoid) selected at mount.** [SYNTHESIS §6.3], [01 §2.2 `DurabilityMode`], [03 §7], [04 §1].
5. **macOS uses `F_FULLFSYNC`; Linux uses `fdatasync`.** Mandatory in all three modes. [SYNTHESIS §N7], [03 §9, I10], [04 §1].
6. **Bloom is opportunistic, never authoritative.** Snapshots accelerate warm-start only; loss → rebuild from redb in ~500 ms / 10 M. [02 §3], [03 §8 I6, I12], [04 §4].
7. **Insert ordering is `cas_block.fsync ▸ cas_dir.fsync ▸ index.commit ▸ bloom.set ▸ HWM++`.** [SYNTHESIS §3 I2], [02 §5.1], [03 §2].
8. **Remove ordering is `index.delete ▸ index.fsync ▸ cas.unlink`; bloom is never updated on remove.** [SYNTHESIS §3 I3], [02 §8.2], [03 §2 invariants table], [04 §7].
9. **4 KiB redb page size, AWUPF-safe, 4 KiB superblock atomic.** [02 §6], [03 §9 I9], [04 §1 cold lookup justification].
10. **Atomic-rename for all advisory files** (bloom snapshot, manifest/header). [02 §3.2, §4], [03 §6 step 6–8 and §8].
11. **CAS-as-truth recovery walker iterates `cas/<XX>/<rest>`, parses 28-byte hex hashes, skips `*.tmp`.** [SYNTHESIS §6.4 step 6], [03 §6].
12. **Online background compactor; not per-commit, not scheduled.** [02 §8.1], [04 §8].
13. **Single OS-thread batching writer behind a bounded MPSC queue.** [SYNTHESIS §6.6], [01 §7.1, §7.2], [04 §2].
14. **Telemetry-gated v2 escalation to log-structured engine.** Trigger = `device_writes_per_day_bytes` exceeds the published threshold. [SYNTHESIS §5 pt 5, §8], [04 §6 G7, §1 commit-throughput SLO].

These items appear in the unified draft as ratified Decisions, not Open Questions.

---

## 2. Disagreements surfaced

### D1 — Crate placement: new `slicefs-dedup` crate vs `cas-local::dedup`

- **Specialists:** [01 §1.1] explicitly overrules [SYNTHESIS §6.1].
- **[01]'s position:** New crate `slicefs-dedup`. Reasons: `cas-local` is a stub/test crate per its own `lib.rs` doc-comment; pulling redb + xxhash + tracing + libc + crossbeam into a "stub" worsens compile times for every consumer; AGPL-vs-commercial seam is naturally crate-shaped.
- **[SYNTHESIS §6.1]'s position:** `cas-local::persistent_dedup_index.rs` — "don't bloat the workspace with a new crate before we know we need separation."
- **[02], [03], [04]:** Silent on the question. [02 §2] writes `crates/slicefs-dedup/src/schema.rs` in a code comment, implicitly adopting [01]'s placement. [03] uses `PersistentDedupIndex` (the SYNTHESIS name); [04] does the same.
- **Recommended resolution:** Adopt **[01]**'s position (new crate `slicefs-dedup`). Three of four arguments [01] cites are concrete and verifiable in the repo today (`cas-local/src/lib.rs` is indeed labelled stub/test; its current dep set is small; the licensing seam is real). Migration cost from `MemDedupIndex` is zero — repo grep confirms no production wiring uses it (see Gap G3).
- **Naming:** Adopt **`RedbDedupIndex`** (per [01]'s renaming) over `PersistentDedupIndex`. Engine-explicit names compose better with the planned `LogDedupIndex` v2 sibling.

### D2 — Trait extension: add `flush()`, `verify()`, `stats()`?

- **Specialists:** [01 §4] proposes adding three default-method extensions to the `DedupIndex` trait. [03], [04] use `flush` semantics implicitly (e.g. [03 §8] talks about clean shutdown writing `last_clean_shutdown=true`). [SYNTHESIS] does not propose trait changes.
- **`MemDedupIndex` impact check:** [01 §4]'s explicit defaults (`flush → Ok(())`, `verify → Ok(VerifyReport::default())`, `stats → IndexStats::default()`) make `MemDedupIndex` compile unchanged. No regression.
- **Disagreement is therefore "minimal trait" vs "extended trait with safe defaults."**
- **Recommended resolution:** **Adopt [01]'s extension.** Three default methods, no signature changes to existing four. The risk profile is asymmetric: the on-disk impl genuinely needs `flush()` (called from `Drop` and `umount`), and grafting it on later as a downcast or a free function is uglier than a one-time trait widening with no-op defaults.
- **`flush()` fallibility:** keep [01]'s **fallible** signature — `Result<(), CasError>`. Users that want to ignore can `.ok()`; users that want to know an unflushed shutdown happened need the error path.

### D3 — `verify_on_present` (I5/I8) default in `default` mode

- **Specialists:** [SYNTHESIS §6.1] sets default `false`; [03 §3, §7 table] sets `false` in `default`, `true` in `paranoid`, `false` in `fast`; [04] is silent. [01 §2.2] documents `verify_on_present: bool` with default `false`.
- **No real disagreement** — all three converge on `false` in `default`, but [03 Open Q3] flags that the cost may be cheap enough (1–5 µs warm `stat`) to flip on by default.
- **Recommended resolution:** Keep `false` in `default` for the MVP (matches three of three). Promote to a measured decision once `bench_lookup_warm` (planned in [04 §10]) reports the warm-cache `stat(2)` cost on the reference NVMe.

### D4 — Group-commit window: 200 ms vs 2 ms

- **Specialists:** [SYNTHESIS §6.3] says "200 ms group-commit"; [03 §2] inherits this; [04 §2] writes "**2 ms** in default mode, 20 ms in seed mode." This is a concrete numeric contradiction.
- **Reading both more carefully:** [SYNTHESIS] writes "Eventual + 200 ms group-commit"; [04] aims to keep "p99 caller-visible insert latency under the 5 ms commit budget." The 200 ms figure was a placeholder commit batching window; the 2 ms figure is the **batcher coalescing window** (how long the batcher waits to collect more inserts before issuing the commit). They are not the same knob. [04] added a finer-grained timing knob the SYNTHESIS lumped into one figure.
- **Recommended resolution:** Two distinct knobs, both in `DedupIndexConfig`:
  - `batcher_coalesce_window_us` = **2 000 µs (2 ms)** in default, **20 000 µs (20 ms)** in seed, **0 µs** in paranoid (per-call commit).
  - `redb_group_commit_window_ms` = **200 ms** in default — tracks redb's `Durability::Eventual` internal flush cadence and bounds the worst-case fsync stall.
  Document both in the unified `DedupIndexConfig`. The unified draft uses [04]'s 2 ms as the batcher window because it is the value derived from a stated SLO (commit p99 ≤ 5 ms).

### D5 — Bloom snapshot durability classification

- **Specialists:** [02 §3] says the bloom file is "non-durable in the sense that we never need to fsync it to call an `insert` durable" and refers to it as "opportunistic." [03 §8, I6, I12] formally invariants the bloom as advisory only and rebuilds from redb on header xxh3 mismatch. [04 §6 H/G metric tree] tracks `bloom_snapshot_failures` and treats them as warn-level. [01 §8 error tier table] makes bloom-snapshot write failure non-fatal with a `tracing::warn!`.
- **No disagreement** — all four agree.
- **Recommended resolution:** Adopt unanimously. The unified draft pulls [03]'s I6 + I12 as the canonical statement.

### D6 — Paranoid commit p99 (04 § flagged, vs 03's mode table)

- **Specialists:** [04 §1] sets paranoid commit p99 ≤ **12 ms**; [03 §7]'s mode table lists paranoid per-insert latency floor at "1–4 ms (F_FULLFSYNC)" and crash window of `0`.
- **Reading carefully:** [03] talks about the lower bound (best case = one F_FULLFSYNC); [04] talks about the p99 ceiling (commit + COW + fsync + drive cache flush worst case on a busy device). They are consistent — best case 1 ms, p99 12 ms. [04] explicitly self-identifies this as "pinged for coordinator review" because the contrast with 03's narrower number could read as conflict.
- **Recommended resolution:** No conflict. The unified draft documents both: floor 1–4 ms (typical insert latency in paranoid), p99 ≤ 12 ms (worst-case commit). Cite both source docs.

### D7 — Manifest naming and contents

- **Specialists:** [SYNTHESIS §6.7] uses `manifest` (JSON) at `cas/.dedup-index/manifest`. [02 §1, §4] uses `header.json` at `<store_root>/dedup/header.json`. [03 §8, I11] uses `manifest` and references `last_clean_shutdown`.
- **Reading carefully:** Same file, two names. [02] also moves the directory from `cas/.dedup-index/` to `<store_root>/dedup/` — a separate decision (location of the dedup subdir).
- **Recommended resolution:**
  - **File name:** `header.json` (matches [02]'s self-identification and is more discoverable to operators than `manifest`).
  - **Location:** `<store_root>/dedup/` (matches [02 §1]; preserves submodule symmetry with `metadata/` and decouples from any future relocation of the CAS shards). Update the unified draft accordingly. [SYNTHESIS]'s `cas/.dedup-index/` was a sketch; [02] is the spec-binding choice.
  - **Schema:** Union of [SYNTHESIS §6.7] and [02 §4] fields (magic, format_version, created_at_unix, page_size_bytes, bloom_capacity, bloom_fpr, hash_algorithm, host_uuid, last_clean_shutdown, last_cardinality, hwm, crc32c).

---

## 3. Gaps identified

The four specialists collectively did not address:

### G1 — FUSE-layer integration

No specialist documents *where* `RedbDedupIndex::open` is called inside `slicefs-cli` or where `Drop` is guaranteed to run before unmount. The current `slicefs-cli` mount path constructs a CAS store and a metadata redb but no DedupIndex. The chunker that calls `bloom_check` / `lookup` / `insert` needs an injection point.

- **Recommended action (deferred):** dedicated `05-fuse-integration.md` doc out of scope for this coordinator pass. The unified draft notes the gap in §15 (Phasing) and §16 (Open Questions).

### G2 — GC subsystem coordination

[SYNTHESIS §F4] and [02 §8.2], [04 §7] reference "the GC engine" calling `remove`. No specialist documents the contract between GC and `insert`: can a hash be inserted while GC is removing it? What's the synchronization? redb's single-writer mutex serializes the txns, but the *decision* to GC a hash that was just inserted is racy.

- **Recommended action:** The CAS layer's segment-immutability story makes this less urgent than it sounds — GC operates on whole segments, and `remove(h)` is invoked only after the segment is already unreachable. Document the assumption in the draft and flag for the GC architect.

### G3 — Migration from `MemDedupIndex`

Repo grep confirms `MemDedupIndex` is referenced by:
- `crates/cas-local/src/mem_dedup_index.rs` (definition + tests)
- `crates/cas-local/src/lib.rs` (re-export)
- `crates/slicefs-traits/src/dedup_index.rs` (trait + tests via parity)
- `crates/slicefs-traits/src/lib.rs` (trait re-export)

**No production binary instantiates `MemDedupIndex`.** It is purely a test/dev impl. No data migration is needed. The unified draft documents this in §15 (Phasing) under "MVP migration plan."

### G4 — Telemetry export format

[04 §6] proposes `tracing` spans + `metrics` crate (Prometheus exporter behind a feature flag). No specialist nailed down: do we expose a stable JSON-over-stdout for `slicefs stats`? Do we export OTEL? Is Prometheus the canonical export?

- **Recommended action:** The unified draft adopts:
  - `tracing` spans with structured fields = canonical in-process telemetry.
  - `slicefs stats --json` = canonical CLI export ([04 §9] already designs this).
  - Prometheus exporter = optional cargo feature `prometheus`, off by default (keeps build small for the typical user).
  - OTEL = explicit non-goal for MVP (out of scope, §15).

### G5 — Test strategy beyond [03 §10] failure injection

[03 §10] lists 12 fault-injection scenarios but does not cover:
- Property-based tests (parity with `MemDedupIndex` for `bloom_check` / `lookup` / `insert` / `remove` under randomized sequences).
- Cross-mode invariant tests (does running `paranoid` then mounting as `default` preserve I1?).
- Bloom snapshot version-skew tests (open a v1 snap with a v2 expectation).
- TempDir-based integration tests for `open` → `insert` × N → `Drop` → `open` again.

- **Recommended action:** Unified draft §14 (Testing strategy) adds a **property-test plan** alongside the failure-injection list. Two parts: (a) trait-parity property tests reusing the existing `MemDedupIndex` proptest suite (proves `RedbDedupIndex` answers the same way as `MemDedupIndex` for any sequence of operations modulo `Absent`/`DefinitelyAbsent` distinction); (b) crash-injection tests per [03 §10].

### G6 — Workspace dependency declarations

No specialist lists the exact `Cargo.toml` lines to add (redb 4.1, fastbloom 0.14, xxhash-rust, tracing, metrics, libc, crossbeam-channel, tempfile for tests). Implementation gap, not architecture gap. Flagged for the implementer.

---

## 4. Open questions consolidated

All "Open Question" items raised across the four specialists, numbered and given a proposed default:

| # | Question | Source | Proposed default |
|---|---|---|---|
| OQ-1 | New crate `slicefs-dedup` vs `cas-local::dedup` module | [01 O-1] / [SYNTHESIS §6.1] | **New crate.** See D1. |
| OQ-2 | `flush()` fallible vs infallible | [01 O-2] | **Fallible** (`Result<(), CasError>`). |
| OQ-3 | `RetryPolicy` config knob for transient redb errors | [01 O-3] | **No.** Fail fast; FUSE-layer middleware (when added) handles retries. |
| OQ-4 | `dedup.lock` separate from `mount.lock` or folded? | [02 OQ-S1] | **Folded into `mount.lock`** — operator UX wins; lifetimes are already coupled (you can't mount one without the other). |
| OQ-5 | Offer `slicefs dedup recover` (rebuild bloom + verify redb) as non-destructive op? | [02 OQ-S2] | **Yes.** Non-destructive recovery (rebuild bloom + verify redb pages) is cheap and operator-friendly; full `reindex` is a fallback. |
| OQ-6 | Is `dedup/` layout part of stable on-disk format? | [02 OQ-S3] | **Stable across minor, breakable across major,** versioned via `header.format_version`. Matches [02 §2.3]. |
| OQ-7 | Group-commit window 200 ms tuning on Apple Silicon | [03 OQ 1] | **Bench before locking; default to 200 ms (redb internal) and 2 ms (batcher coalesce) until measured.** See D4. |
| OQ-8 | Online rebuild for N > 50 M acceptable for MVP? | [03 OQ 2] | **No — offline-only in MVP** matching [SYNTHESIS §8]. "Suspect mode partial reads" is a v2 feature; MVP refuses to mount RW until rebuild is complete. |
| OQ-9 | `verify_on_present` default in `default` mode | [03 OQ 3] / [SYNTHESIS §7 Q5] | **`false` in MVP**, revisit after `bench_lookup_warm` reports cost. See D3. |
| OQ-10 | Bloom-sizing under multi-year growth without segments | [SYNTHESIS §7 Q1] | **Manual `slicefs reindex --bloom-capacity 2x` ladder for MVP**; segment-blooms in v2.1. |
| OQ-11 | `Durability::None` + caller-redrive contract — does any layer above DedupIndex cache "I just inserted h" state? | [SYNTHESIS §7 Q2] | **Audit `cas-local::insert_block` and FUSE write handlers; write a `kill -9` test** (added to G5/§14). Provisional answer: no, but verify. |
| OQ-12 | F_FULLFSYNC cost on Apple Silicon — bench needed | [SYNTHESIS §7 Q3] | **Add `bench_commit_latency` ([04 §10 #5]) on M2 reference**; budget 1–4 ms per F_FULLFSYNC pending data. |
| OQ-13 | redb single-writer ceiling on 100M-hash seed burst | [SYNTHESIS §7 Q4] | **Run `bench_seed_burst_100m` ([04 §10 #1]) on reference NVMe**; if < 100K ins/s → escalate to log-structured before MVP ship. |
| OQ-14 | Verify-on-present cost (I5) in warm-cache p99 | [SYNTHESIS §7 Q5] | Same as OQ-9. |

---

## 5. Recommended decisions (one-liner each)

| Topic | Decision | Rationale |
|---|---|---|
| Crate placement | **New crate `slicefs-dedup`** | `cas-local` is a stub crate; dep hygiene + license seam justify split. |
| Type name | **`RedbDedupIndex`** | engine-explicit; symmetry with future `LogDedupIndex`. |
| Trait extension | **Add `flush`, `verify`, `stats` with default impls** | needed by on-disk impl, no breakage for `MemDedupIndex`. |
| `flush()` signature | **Fallible** | consumers that don't care can `.ok()`. |
| `verify_on_present` MVP default | **`false` in `default`, `true` in `paranoid`, `false` in `fast`** | matches three of three sources; revisit after benchmarks. |
| Group-commit window | **batcher coalesce 2 ms / 20 ms / 0 ms; redb `Eventual` ≈ 200 ms** | resolves D4 — two knobs, both real. |
| Manifest file | **`header.json` at `<store_root>/dedup/header.json`** | union of fields; [02]'s naming wins. |
| Dedup directory | **`<store_root>/dedup/`** | per [02 §1]; submodule symmetry with `metadata/`. |
| Migration plan | **None — no production callers of `MemDedupIndex`** | confirmed by repo grep. |
| Telemetry | **`tracing` + `slicefs stats --json` canonical; Prometheus optional feature** | minimal default footprint; OTEL deferred. |
| Test strategy | **Property tests (parity with `MemDedupIndex`) + 12 [03 §10] crash-injection scenarios** | covers G5. |
| Lock file | **Folded into `mount.lock`** | OQ-4 default. |
| Online rebuild | **Offline-only in MVP, online in v2** | OQ-8 default. |
| Bloom rebuild ladder | **Manual `slicefs reindex --bloom-capacity 2x` until v2.1 segment-blooms** | OQ-10 default. |

**Disagreements resolved:** 5 (D1, D2, D3, D4, D7). **Disagreements escalated:** 2 (D5 unanimously agreed → no escalation; D6 = false alarm, no escalation). Net escalations to verify pass: **0** new disagreements; the 14 open questions are pre-existing and tracked in the draft.

---

*End of COORDINATOR-LOG.md. The unified `ARCHITECTURE.md` first draft accompanies this log.*
