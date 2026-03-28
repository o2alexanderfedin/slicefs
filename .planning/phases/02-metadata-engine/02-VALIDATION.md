---
phase: 2
slug: metadata-engine
status: draft
nyquist_compliant: false
wave_0_complete: false
created: 2026-03-28
---

# Phase 2 — Validation Strategy

> Per-phase validation contract for feedback sampling during execution.

---

## Test Infrastructure

| Property | Value |
|----------|-------|
| **Framework** | Rust built-in test (`cargo test`) |
| **Config file** | none — standard `#[test]` and `#[cfg(test)]` modules |
| **Quick run command** | `cargo test -p metadata` |
| **Full suite command** | `cargo test --workspace` |
| **Estimated runtime** | ~5 seconds |

---

## Sampling Rate

- **After every task commit:** Run `cargo test -p metadata`
- **After every plan wave:** Run `cargo test --workspace`
- **Before `/gsd:verify-work`:** Full suite must be green
- **Max feedback latency:** 10 seconds

---

## Per-Task Verification Map

| Task ID | Plan | Wave | Requirement | Test Type | Automated Command | File Exists | Status |
|---------|------|------|-------------|-----------|-------------------|-------------|--------|
| 02-01-01 | 01 | 1 | META-03 | unit | `cargo test -p metadata -- metadata_store` | ❌ W0 | ⬜ pending |
| 02-01-02 | 01 | 1 | POSIX-06 | unit | `cargo test -p metadata -- inode::tests` | ❌ W0 | ⬜ pending |
| 02-01-03 | 01 | 1 | POSIX-07 | unit | `cargo test -p metadata -- inode::tests::timestamps` | ❌ W0 | ⬜ pending |
| 02-02-01 | 02 | 1 | Phase SC 2 | unit | `cargo test -p metadata -- directory::tests` | ❌ W0 | ⬜ pending |
| 02-02-02 | 02 | 1 | Phase SC 3 | unit | `cargo test -p metadata -- manifest::tests` | ❌ W0 | ⬜ pending |
| 02-03-01 | 03 | 2 | POSIX-08 | unit | `cargo test -p metadata -- xattr::tests` | ❌ W0 | ⬜ pending |
| 02-03-02 | 03 | 2 | POSIX-10 | integration | `cargo test -p metadata -- inode_map::tests::stable_across_reload` | ❌ W0 | ⬜ pending |
| 02-03-03 | 03 | 2 | Phase SC 5 | integration | `cargo test -p metadata -- inode_map::tests::stable_across_reload` | ❌ W0 | ⬜ pending |

*Status: ⬜ pending · ✅ green · ❌ red · ⚠️ flaky*

---

## Wave 0 Requirements

- [ ] `crates/metadata/` — crate does not exist yet; Wave 0 creates skeleton
- [ ] `crates/metadata/src/store.rs` — covers META-03, Phase SC 1
- [ ] `crates/metadata/src/inode.rs` — covers POSIX-06, POSIX-07, Phase SC 1
- [ ] `crates/metadata/src/directory.rs` — covers Phase SC 2
- [ ] `crates/metadata/src/manifest.rs` — covers Phase SC 3
- [ ] `crates/metadata/src/xattr.rs` — covers POSIX-08, Phase SC 4
- [ ] `crates/metadata/src/inode_map.rs` — covers POSIX-10, Phase SC 5
- [ ] `git submodule add https://github.com/o2alexanderfedin/data-id.git crates/data-id` — data-id not yet cloned
- [ ] `crates/dedupfs-traits` redesign — Digest224/Digest256 replacing ChunkHash

*Wave 0 installs test infrastructure and creates all stub files with failing tests.*

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
