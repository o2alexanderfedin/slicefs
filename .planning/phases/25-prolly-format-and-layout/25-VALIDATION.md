---
phase: "25"
slug: "prolly-format-and-layout"
# status lifecycle: draft (seeded by plan-phase) → validated (set by validate-phase §6)
status: draft
nyquist_compliant: false
wave_0_complete: false
created: "2026-11-27"
---

# Phase 25 — Validation Strategy

> Per-phase validation contract for feedback sampling during execution.

---

## Test Infrastructure

| Property | Value |
|----------|-------|
| **Framework** | Rust built-in test harness; `proptest` (dev-dependency of `prolly`); Docker Linux runner for mount tests |
| **Config file** | none (workspace `Cargo.toml`) |
| **Quick run command** | `export PATH="$HOME/.cargo/bin:$PATH"; timeout 900 cargo test -p <crate of the task> </dev/null` |
| **Full suite command** | `export PATH="$HOME/.cargo/bin:$PATH"; timeout 2400 cargo test --workspace </dev/null`; mounts: `scripts/docker-linux-test/run.sh test` |
| **Estimated runtime** | ~600 seconds (workspace); Docker run longer |

---

## Sampling Rate

- **After every task commit:** the crate test of the task (`-p prolly`, `-p metadata`, `-p slicefs-cli --lib header`, `-p prolly-bench`)
- **After every plan wave:** `cargo test --workspace` on the host (no mounts) plus `scripts/docker-linux-test/run.sh test`
- **Before `/gsd-verify-work`:** Full suite must be green on the host and in Docker
- **Max feedback latency:** 900 seconds

---

## Per-Task Verification Map

Filled from the PLAN files; the requirement-to-test map is in `25-RESEARCH.md` § Validation Architecture.

| Req | Behavior | Test Type | Automated Command | File Exists |
|-----|----------|-----------|-------------------|-------------|
| PMAP-04 | fixed-point table pinned, goldens, version byte, salt | unit | `cargo test -p prolly` | ❌ W0 |
| PMAP-02 | root record without `inode_map`, round trip, GC walk, rehash | unit + integration | `cargo test -p metadata`; `cargo test -p slicefs-cli --test hash_change_tests` | ✅ update |
| PMAP-05 | format 8 header, format 7 refused with upgrade-doc text, salt line | unit + integration | `cargo test -p slicefs-cli --lib header`; `--test store_header_tests` | ❌ W0 |
| PMAP-01 | bench layouts run; decision written in store-format.md | smoke + doc check | `cargo test -p prolly-bench`; grep of `docs/design/store-format.md` | ✅ update |

*Status: ⬜ pending · ✅ green · ❌ red · ⚠️ flaky*

---

## Wave 0 Requirements

- [ ] `crates/prolly/src/tests.rs` — goldens, fixed-point error/agreement, rule version, salt tests
- [ ] `crates/slicefs-cli/tests/store_header_tests.rs` — format 7 refusal, salt round trip and refusals
- [ ] `crates/metadata` — root record size and offset test
- [ ] `benchmarks/prolly-bench/tests/smoke.rs` — all maps and the layout mode on a small tree

---

## Manual-Only Verifications

| Behavior | Requirement | Why Manual | Test Instructions |
|----------|-------------|------------|-------------------|
| `v2.3.0` refuses a format 8 store | PMAP-05 | needs a build of the old tag | build tag `v2.3.0` in a separate worktree, run `slicefs stats <format-8 store>`, record the log |
| Layout bench at 1M and 10M inodes | PMAP-01 | long run (minutes to hours) | run the bench by hand, keep the CSV files, cite them in store-format.md |

---

## Validation Sign-Off

- [ ] All tasks have `<automated>` verify or Wave 0 dependencies
- [ ] Sampling continuity: no 3 consecutive tasks without automated verify
- [ ] Wave 0 covers all MISSING references
- [ ] No watch-mode flags
- [ ] Feedback latency < 900s
- [ ] `nyquist_compliant: true` set in frontmatter

**Approval:** pending
