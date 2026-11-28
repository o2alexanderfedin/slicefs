---
phase: "26"
slug: "prolly-maps-in-commit-read-and-gc"
status: draft
nyquist_compliant: false
wave_0_complete: false
created: "2026-11-27"
---

# Phase 26 — Validation Strategy

> Per-phase validation contract for feedback sampling during execution.

---

## Test Infrastructure

| Property | Value |
|----------|-------|
| **Framework** | Rust `cargo test`, `proptest`, counting `GlobalAlloc` test binaries, Docker scripts for mounts |
| **Config file** | none (workspace `Cargo.toml`) |
| **Quick run command** | `export PATH="$HOME/.cargo/bin:$PATH"; timeout 900 cargo test -p prolly </dev/null` and `cargo test -p metadata --lib` |
| **Full suite command** | `timeout 3000 cargo test --workspace </dev/null`; Docker: `scripts/docker-linux-test/run.sh all`, `SLICEFS_WORKERS=4 scripts/docker-linux-test/run.sh pjdfstest`, `scripts/docker-linux-test/run.sh verify` |
| **Estimated runtime** | ~600 s workspace; Docker longer |

---

## Sampling Rate

- **After every task commit:** `cargo test -p prolly` and the touched crate's `--lib`
- **After every plan wave:** `cargo test --workspace`
- **Before `/gsd-verify-work`:** workspace, clippy script, Docker `all`, `pjdfstest` with 4 workers, `verify`; then the by-hand PMAP-07 runs
- **Max feedback latency:** 900 seconds

---

## Per-Task Verification Map

The requirement-to-test map is in `26-RESEARCH.md` § Validation Architecture; the PLAN files carry the per-task commands.

| Req | Behavior | Test Type | Automated Command | File Exists |
|-----|----------|-----------|-------------------|-------------|
| PMAP-03 | round trip of files, dirs, hard links, xattrs, snapshots; pjdfstest 4 workers | integration + Docker | `run.sh all`; `SLICEFS_WORKERS=4 run.sh pjdfstest` | ✅ re-run |
| PMAP-03 | one-change commit grows with tree height only | integration | `cargo test -p metadata --test commit_counter_tests` | ❌ W0 |
| PMAP-06 | mark reaches every node; peak heap flat in map size; online GC | integration + alloc binary | `cargo test -p metadata --test gc_prolly_tests --test gc_prolly_memory_tests` | ❌ W0 |
| PMAP-08 | cut at each apply, root put, sync, install; replay consistent | integration | `cargo test -p metadata --test commit_crash_tests`; `-p slicefs-cli --test write_log_replay_tests` | ❌ W0 / extend |
| PMAP-07 | 1/5 of flat at 1M inodes; Boost not `drop`; insdel recorded | by-hand bench | `cargo test --release -p metadata --test commit_bench -- --ignored` in both trees | ✅ extend |

---

## Wave 0 Requirements

- [ ] `crates/prolly` — `NodeCache`, `range`, `count`, `visit` and their tests
- [ ] `crates/metadata/tests/gc_prolly_memory_tests.rs`, `commit_counter_tests.rs`, `commit_crash_tests.rs`
- [ ] test-only bulk loader for N inodes
- [ ] pre-registered PMAP-07 rule file before any measurement

---

## Manual-Only Verifications

| Behavior | Requirement | Why Manual | Test Instructions |
|----------|-------------|------------|-------------------|
| One-change commit at 1M inodes vs `v2.3.0` flat | PMAP-07 | long bench, two builds | flat baseline in a `v2.3.0` worktree, branch run in Docker; apply the pre-registered rule |

---

## Validation Sign-Off

- [ ] All tasks have `<automated>` verify or Wave 0 dependencies
- [ ] Sampling continuity: no 3 consecutive tasks without automated verify
- [ ] Wave 0 covers all MISSING references
- [ ] No watch-mode flags
- [ ] Feedback latency < 900s
- [ ] `nyquist_compliant: true` set in frontmatter

**Approval:** pending
