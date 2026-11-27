---
phase: "24"
slug: "prolly-evaluation"
# status lifecycle: draft (seeded by plan-phase) → validated (set by validate-phase §6)
status: draft
nyquist_compliant: false
wave_0_complete: false
created: "2026-11-26"
---

# Phase 24 — Validation Strategy

> Per-phase validation contract for feedback sampling during execution. Source: 24-RESEARCH.md "Validation Architecture".

---

## Test Infrastructure

| Property | Value |
|----------|-------|
| **Framework** | Rust built-in test harness; `proptest` (workspace dep) for one property test |
| **Config file** | none (crate `Cargo.toml` only) |
| **Quick run command** | `export PATH="$HOME/.cargo/bin:$PATH"; timeout 900 cargo test -p prolly </dev/null` |
| **Full suite command** | `export PATH="$HOME/.cargo/bin:$PATH"; timeout 900 cargo test -p prolly -p prolly-bench </dev/null` |
| **Estimated runtime** | a few seconds of test time (plus build) |

---

## Sampling Rate

- **After every task commit:** quick run command (or `-p prolly-bench` for bench tasks)
- **After every plan wave:** full suite command, `scripts/safety/unsafe-count.sh`, and the "not wired into the product" greps (D-01, D-19)
- **Before `/gsd-verify-work`:** full suite green, `SMOKE=1 RUNS=1 timeout 600 scripts/prolly-bench/bench.sh` done, full Boost run done or recorded as human-needed (D-29)
- **Max feedback latency:** 900 seconds (build-bound)

---

## Per-Task Verification Map

Filled per plan by the planner (task IDs `24-NN-MM`). Requirement-level map:

| Requirement | Behavior | Test Type | Automated Command | File Exists | Status |
|-------------|----------|-----------|-------------------|-------------|--------|
| PROLLY-01 | Order independence: bulk, shuffled single `apply`, batches in two orders give one root | unit | `cargo test -p prolly order_independence` | ❌ W0 | ⬜ pending |
| PROLLY-01 | Same-length value bytes do not move boundaries | unit | `cargo test -p prolly boundaries_ignore_value_bytes` | ❌ W0 | ⬜ pending |
| PROLLY-01 | Node size limits (max; min except last of a level) | unit | `cargo test -p prolly node_size_limits` | ❌ W0 | ⬜ pending |
| PROLLY-01 | Variable-length keys/values equal a `BTreeMap` model | proptest | `cargo test -p prolly variable_length_model` | ❌ W0 | ⬜ pending |
| PROLLY-01 | One same-width change writes one path (`depth` objects) | unit | `cargo test -p prolly one_change_writes_one_path` | ❌ W0 | ⬜ pending |
| PROLLY-01 | Bounded memory (high-water independent of entry count) | unit | `cargo test -p prolly memory_is_bounded` | ❌ W0 | ⬜ pending |
| PROLLY-01 | Golden root and cut-probability table | unit | `cargo test -p prolly golden_root` | ❌ W0 | ⬜ pending |
| PROLLY-01 | Canonical root (empty, one key, delete back) | unit | `cargo test -p prolly canonical_root` | ❌ W0 | ⬜ pending |
| PROLLY-01 | Malformed node is an error, not a panic | unit | `cargo test -p prolly malformed_node_is_an_error` | ❌ W0 | ⬜ pending |
| PROLLY-01 | Not wired into the product | grep | `grep -n prolly crates/metadata/Cargo.toml crates/slicefs-cli/Cargo.toml crates/chunk-store/Cargo.toml` finds nothing | n/a | ⬜ pending |
| PROLLY-02 | Bench parses args; tiny end-to-end in a tempdir | unit + smoke | `cargo test -p prolly-bench` | ❌ W0 | ⬜ pending |
| PROLLY-02 | Driver script end to end on generated data | by hand | `SMOKE=1 RUNS=1 timeout 600 scripts/prolly-bench/bench.sh` | ❌ W0 | ⬜ pending |
| PROLLY-02 | Full Boost run, 3 runs | by hand | `timeout 5400 scripts/prolly-bench/bench.sh DATA 3` | Plan 05 | ⬜ pending |
| PROLLY-03 | Report, design section, #19 text present; `store-format.md` unchanged | doc check | greps listed in 24-RESEARCH.md | Plan 05 | ⬜ pending |

*Status: ⬜ pending · ✅ green · ❌ red · ⚠️ flaky*

### Per-task map (planner)

| Task | Plan | Wave | Requirement | Threat Ref | Automated command (filter) | Status |
|------|------|------|-------------|------------|----------------------------|--------|
| 24-01-01 (tracer) | 01 | 1 | PROLLY-01 | T-24-SC, T-24-03 | `cargo test -p prolly --lib -- tests::build_then_get_roundtrip tests::golden_root`; Cargo.toml/Cargo.lock greps | ⬜ pending |
| 24-01-02 | 01 | 1 | PROLLY-01 | — | `cargo test -p prolly --lib` (cursor_iterates_in_order, node_size_limits, boundaries_ignore_value_bytes, levels_use_their_own_salt, same_set_twice_writes_nothing, memory_is_bounded) | ⬜ pending |
| 24-01-03 | 01 | 1 | PROLLY-01 | T-24-01, T-24-02 | `cargo test -p prolly` (malformed_node_is_an_error, entry_too_large_is_an_error, unsorted_input_is_an_error); clippy; unsafe-count; `cargo tree -i prolly`; `git diff --quiet a882251` | ⬜ pending |
| 24-02-01 (tracer) | 02 | 2 | PROLLY-01 | — | `cargo test -p prolly --lib -- tests::apply_one_put_then_get tests::one_change_writes_one_path tests::apply_inserts_equal_bulk tests::golden_root` | ⬜ pending |
| 24-02-02 | 02 | 2 | PROLLY-01 | T-24-05 | `cargo test -p prolly --lib` (canonical_root, order_independence, same_batch_twice_writes_nothing, apply_rejects_unsorted_batch) | ⬜ pending |
| 24-02-03 | 02 | 2 | PROLLY-01 | T-24-04 | `cargo test -p prolly` (variable_length_model, apply_memory_is_bounded, malformed_node_is_an_error_in_apply); clippy; unsafe-count; no-cache grep; branch file list | ⬜ pending |
| 24-03-01 (tracer) | 03 | 3 | PROLLY-02 | T-24-SC, T-24-07 | `cargo test -p prolly-bench` (smoke_end_to_end, bad_arguments_exit_2); unsafe-count; members/lock greps | ⬜ pending |
| 24-03-02 | 03 | 3 | PROLLY-02 | T-24-06 | `cargo test -p prolly-bench` (full edit script + version step); no Boost in tests | ⬜ pending |
| 24-03-03 | 03 | 3 | PROLLY-02 | T-24-08 | `cargo test -p prolly-bench`; clippy; unsafe-count; `cargo tree -i prolly`; `git diff --quiet a882251` | ⬜ pending |
| 24-04-01 (tracer) | 04 | 4 | PROLLY-02 | T-24-09 | `SMOKE=1 RUNS=1 SCALE_NS= ... bench.sh` -> rows=30 medians=18 | ⬜ pending |
| 24-04-02 | 04 | 4 | PROLLY-02 | T-24-09, T-24-10 | SMOKE outputs (scale=16 disk=12 edits=10 batches=9); full mode on gen data leaves the data dir unchanged; refusals exit 2; interrupted run leaves no store | ⬜ pending |
| 24-04-03 | 04 | 4 | PROLLY-02 | T-24-11 | `scripts/docker-linux-test/run.sh prolly-bench-smoke` (prolly tests on Linux + smoke); `scripts/safety/clippy.sh` | ⬜ pending |
| 24-05-01 (tracer) | 05 | 5 | PROLLY-02 | T-24-13 | phase gate; quick Boost pass; `report-tables.py --check` | ⬜ pending |
| 24-05-02 | 05 | 5 | PROLLY-02 | T-24-15 | full run CSV shape (runs=738 medians=30, or a marked fallback); digest checks ok | ⬜ pending |
| 24-05-03 | 05 | 5 | PROLLY-03 | T-24-12, T-24-14 | `report-tables.py --check docs/research/prolly-trees.md`; section and fact greps; bench_data branch gate; content-cut greps; no-change diff | ⬜ pending |

Note: the phase has five plans (the suggested three were split for context size: the crate core and `apply`, then the bench binary and its driver). The run plan of CONTEXT D-29 is 24-05.

---

## Wave 0 Requirements

- [ ] `crates/prolly` (new crate with its tests) — PROLLY-01
- [ ] `benchmarks/prolly-bench`, `scripts/prolly-bench/bench.sh`, `scripts/prolly-bench/README.md` — PROLLY-02
- [ ] `docs/design/unsafe-review.md` entry for the bench's `getrusage` helper (unsafe-count gate)
- Framework install: none.

---

## Manual-Only Verifications

| Behavior | Requirement | Why Manual | Test Instructions |
|----------|-------------|------------|-------------------|
| Full benchmark on Boost 1_84_0 / 1_85_0 | PROLLY-02 | Needs the Boost data and takes long; never in `cargo test` (D-06) | `scripts/macos-mount-test/fetch-boost.sh DATA 1_84_0 1_85_0`, then `timeout 5400 scripts/prolly-bench/bench.sh DATA 3` |
| Posting the decision on #19 | PROLLY-03 | Executors run no `gh` write command (D-27) | Owner or orchestrator posts `24-ISSUE-19-COMMENT.md` |

---

## Validation Sign-Off

- [ ] All tasks have `<automated>` verify or Wave 0 dependencies
- [ ] Sampling continuity: no 3 consecutive tasks without automated verify
- [ ] Wave 0 covers all MISSING references
- [ ] No watch-mode flags
- [ ] Feedback latency < 900s
- [ ] `nyquist_compliant: true` set in frontmatter

**Approval:** pending
