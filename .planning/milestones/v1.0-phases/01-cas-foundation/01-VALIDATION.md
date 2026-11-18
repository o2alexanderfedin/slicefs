---
phase: 1
slug: cas-foundation
status: draft
nyquist_compliant: false
wave_0_complete: false
created: 2026-03-27
---

# Phase 1 — Validation Strategy

> Per-phase validation contract for feedback sampling during execution.

---

## Test Infrastructure

| Property | Value |
|----------|-------|
| **Framework** | Rust built-in (`cargo test`) + `proptest` 1.x |
| **Config file** | None needed — standard Rust test infrastructure |
| **Quick run command** | `cargo test -p slicefs-traits -p cas-local` |
| **Full suite command** | `cargo test --workspace` |
| **Estimated runtime** | ~5 seconds |

---

## Sampling Rate

- **After every task commit:** Run `cargo test -p slicefs-traits -p cas-local`
- **After every plan wave:** Run `cargo test --workspace`
- **Before `/gsd:verify-work`:** Full suite must be green
- **Max feedback latency:** 10 seconds

---

## Per-Task Verification Map

| Task ID | Plan | Wave | Requirement | Test Type | Automated Command | File Exists | Status |
|---------|------|------|-------------|-----------|-------------------|-------------|--------|
| 01-01-01 | 01 | 1 | CAS-01 | unit | `cargo test -p cas-local block_store::tests` | ❌ W0 | ⬜ pending |
| 01-01-02 | 01 | 1 | CAS-01 | unit | `cargo test -p cas-local hasher::tests::swap_hasher` | ❌ W0 | ⬜ pending |
| 01-01-03 | 01 | 1 | CAS-02 | unit | `cargo test -p cas-local chunker::tests::swap_chunker` | ❌ W0 | ⬜ pending |
| 01-02-01 | 02 | 1 | CAS-03 | unit | `cargo test -p cas-local disk_block_store::tests` | ❌ W0 | ⬜ pending |
| 01-02-02 | 02 | 1 | CAS-05 | unit | `cargo test -p cas-local disk_block_store::tests::corruption_detected` | ❌ W0 | ⬜ pending |
| 01-03-01 | 03 | 1 | CAS-07 | unit | `cargo test -p cas-local mem_dedup_index::tests::dedup_prevents_write` | ❌ W0 | ⬜ pending |
| 01-03-02 | 03 | 1 | CAS-07 | unit | `cargo test -p cas-local mem_dedup_index::tests::bloom_fast_path` | ❌ W0 | ⬜ pending |

*Status: ⬜ pending · ✅ green · ❌ red · ⚠️ flaky*

---

## Wave 0 Requirements

- [ ] `Cargo.toml` (workspace root) — workspace initialization
- [ ] `crates/slicefs-traits/Cargo.toml` — traits crate manifest
- [ ] `crates/slicefs-traits/src/` — trait definitions
- [ ] `crates/cas-local/Cargo.toml` — local implementations crate manifest
- [ ] `crates/cas-local/src/` — stub/test implementations
- [ ] All test files under `crates/cas-local/src/*/tests` — covers all REQ IDs above
- [ ] `proptest` dependency added to cas-local dev-dependencies

*Framework install: `cargo add` is built-in; no additional tooling install needed.*

---

## Manual-Only Verifications

*All phase behaviors have automated verification.*

---

## Validation Sign-Off

- [ ] All tasks have `<automated>` verify or Wave 0 dependencies
- [ ] Sampling continuity: no 3 consecutive tasks without automated verify
- [ ] Wave 0 covers all MISSING references
- [ ] No watch-mode flags
- [ ] Feedback latency < 10s
- [ ] `nyquist_compliant: true` set in frontmatter

**Approval:** pending
