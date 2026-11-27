---
phase: 25-prolly-format-and-layout
verified: 2026-11-27T12:00:00Z
status: passed
score: 4/4 must-haves verified
covered_files:
  - ".planning/phases/25-prolly-format-and-layout/25-01-PLAN.md"
  - ".planning/phases/25-prolly-format-and-layout/25-01-SUMMARY.md"
  - ".planning/phases/25-prolly-format-and-layout/25-02-PLAN.md"
  - ".planning/phases/25-prolly-format-and-layout/25-02-SUMMARY.md"
  - ".planning/phases/25-prolly-format-and-layout/25-03-PLAN.md"
  - ".planning/phases/25-prolly-format-and-layout/25-03-SUMMARY.md"
  - ".planning/phases/25-prolly-format-and-layout/25-04-PLAN.md"
  - ".planning/phases/25-prolly-format-and-layout/25-04-SUMMARY.md"
  - ".planning/phases/25-prolly-format-and-layout/25-05-PLAN.md"
  - ".planning/phases/25-prolly-format-and-layout/25-05-SUMMARY.md"
  - ".planning/phases/25-prolly-format-and-layout/25-06-PLAN.md"
  - ".planning/phases/25-prolly-format-and-layout/25-06-SUMMARY.md"
  - ".planning/phases/25-prolly-format-and-layout/25-07-PLAN.md"
  - ".planning/phases/25-prolly-format-and-layout/25-07-SUMMARY.md"
  - ".planning/phases/25-prolly-format-and-layout/25-08-PLAN.md"
  - ".planning/phases/25-prolly-format-and-layout/25-08-SUMMARY.md"
  - "crates/metadata/src/inode_alloc.rs"
  - "crates/metadata/src/store.rs"
  - "crates/prolly/src/lib.rs"
  - "crates/prolly/src/node.rs"
  - "crates/prolly/src/rule.rs"
  - "crates/prolly/src/rule_table.rs"
  - "crates/prolly/src/tests.rs"
  - "crates/slicefs-cli/src/content.rs"
  - "crates/slicefs-cli/src/header.rs"
  - "crates/slicefs-cli/src/seed.rs"
  - "docs/design/store-format.md"
  - "docs/user/upgrade.md"
covered_digest: "v4:sha256:460157d7d01790f914e2ab03c1ab995e0703988937a5d3375d3b7993481f74da"
behavior_unverified: 0
overrides_applied: 0
---

# Phase 25: Prolly format and layout Verification Report

**Phase Goal:** The format of the prolly metadata maps is decided and written down, the duplicate inode map is gone, and the new format is store format 8 (#215)
**Verified:** 2026-11-27
**Status:** passed
**Re-verification:** No, initial verification

## Goal Achievement

### Observable Truths (ROADMAP Phase 25 success criteria)

| # | Truth | Status | Evidence |
|---|-------|--------|----------|
| 1 | Layout decided and recorded in `docs/design/store-format.md`, with the reason and the effect on the root record | VERIFIED | Section "Metadata maps (store format 8)" has a Layout subsection: layout a, five prolly trees, a key grammar table, the rule table for a/b/c with numbers, the reason (b and c fail the "not more nodes at 1B" rule: 48.9M and 48.4M against 47.3M), the effect on the GC mark (five walks), and the effect on the root record (same nine fields, 197 bytes; b would be 65 bytes, c 98). The numbers match `runs/layout/decision.txt` (last line `verdict layout=a l=8192 node_max=16384`; 23,479,915 nodes at 1B). The rules were pre-registered (commit 0e4036f precedes the run commits). |
| 2 | Root record holds one of `inode_map` / `inode_data`; a store from the new build mounts; existing metadata and mount tests pass | VERIFIED | `grep inode_map\|InodeMap` over `crates` and `benchmarks` finds no Rust code use; `inode_map.rs` is gone. `root_rec` in `crates/metadata/src/store.rs` has 9 fields with `SIZE` = 197 (tests `root_record_layout`, `a_230_byte_root_record_is_refused`). `cargo test -p metadata` run in this verification: 191 passed plus all integration binaries, 0 failed. Docker gate `runs/gate-25-08.log` (`run.sh all`) ends `content ok`, `rc=0`, and `runs/gate-25-02.log` ends `content ok`, `rc=0`. The Docker logs are recorded evidence; I did not mount on the host. |
| 3 | Fixed-point boundary rule (no float), a version byte, the rule, constants and node format in the doc, a golden test with the same cut points on Linux and macOS | VERIFIED | No `f64`/`f32` in `rule.rs`, `node.rs`, `tree.rs`, `apply.rs`, `lib.rs`. `rule.rs` has `exp_neg_fp` and `threshold` in u128 integer math over `rule_table.rs`. `python3 scripts/prolly-table/gen_table.py --check crates/prolly/src/rule_table.rs` exits 0. `RULE_VERSION` = 1 is written in each node header (`node.rs:73`) and checked on decode (`node.rs:156`, `Error::RuleVersion`). `cargo test -p prolly` in this verification: 33 passed, 0 failed (includes `golden_root`). `runs/golden-macos.log` and `runs/golden-linux.log` (Docker) both show `golden_root ... ok` against the same pinned values, at code commit 4a4e14d. The doc sections "Node format", "Boundary rule", "Constants" and "Boundary salt" give the header table, the formula, the table hash, the error bound (2^-22) and the constants. |
| 4 | Store format 8; v2.3.0 refuses it; the new build refuses format 7 with a message that points to the upgrade doc | VERIFIED | `header.rs`: `FORMAT_NUMBER = 8`, M17 text "...store format 7, flat metadata maps. This SliceFS reads store format 8 only. Copy the files into a new store with the steps in docs/user/upgrade.md." `docs/user/upgrade.md` exists. `runs/v230-format-check.log` used a real v2.3.0 binary (582a41d): v2.3.0 `stats` and `mount` of a format 8 store print "made by a newer SliceFS (store format 8). This SliceFS reads store format 7 only." and exit 1 with no mount; this build's `stats` of a v2.3.0 store7 prints M17 and exits 1, and the FORMAT file digest is unchanged. `cargo test -p slicefs-cli --test store_header_tests --test boundary_salt_tests` run in this verification: 7 and 10 passed, 0 failed. The format policy (D-13) is updated in the doc. |

**Score:** 4/4 truths verified (0 present, behavior-unverified)

### Required Artifacts

| Artifact | Expected | Status | Details |
|----------|----------|--------|---------|
| `crates/prolly/src/rule.rs`, `rule_table.rs` | integer rule and generated table | VERIFIED | Substantive, used by `tree.rs`/`apply.rs`; table generator check passes |
| `crates/prolly/src/node.rs` | 10-byte header with rule version | VERIFIED | Encode and decode both use `RULE_VERSION` |
| `crates/metadata/src/store.rs`, `inode_alloc.rs` | one inode map, 197-byte record, allocator | VERIFIED | Offsets 0/8/16/49/82/115/148/181/189, size 197 |
| `crates/slicefs-cli/src/header.rs`, `content.rs`, `seed.rs` | format 8, M17, salt line and flag, `seed --boundary-salt` | VERIFIED | Tests pass |
| `docs/design/store-format.md` | metadata map section, format policy | VERIFIED | Sections at lines 441-740; policy at 740 |
| `docs/user/upgrade.md` | upgrade page named by M17 | VERIFIED | Exists; copy steps are deferred to #155 (Phase 27) as the plan says |
| `benchmarks/prolly-bench`, `scripts/prolly-bench/{layout.sh,decide.py}` | layout engine and pre-registered rules | VERIFIED | Layout run output (27 rows, all checks ok) is in `runs/layout/` |

### Key Link Verification

| From | To | Via | Status |
|------|----|-----|--------|
| `header.rs` M17 | `docs/user/upgrade.md` | message text names the file | WIRED |
| `store.rs` root record | allocator `InodeAlloc` | `next_ino` read/written under `inode_alloc` lock | WIRED |
| bench `main.rs` root-record copy | `store.rs` layout | same 197-byte offsets (review confirmed) | WIRED |
| `rule.rs` `threshold` | `tree.rs` / `apply.rs` cut decision | called per entry; `forced_cuts` counted | WIRED |

### Requirements Coverage

| Requirement | Source Plans | Description | Status | Evidence |
|-------------|--------------|-------------|--------|----------|
| PMAP-01 | 25-05, 25-06, 25-07, 25-08 | Map layout decided and written down | SATISFIED | Truth 1 |
| PMAP-02 | 25-02, 25-08 | One inode map; root record holds one | SATISFIED | Truth 2 |
| PMAP-04 | 25-01, 25-04, 25-06, 25-07, 25-08 | Fixed-point rule, version byte, doc | SATISFIED | Truth 3 |
| PMAP-05 | 25-03, 25-04, 25-08 | Store format 8; old release refuses; new build refuses 7 with pointer | SATISFIED | Truth 4 |

All four IDs named in the phase appear in PLAN frontmatter and in REQUIREMENTS.md. No ORPHANED requirements: REQUIREMENTS.md maps PMAP-03, 06, 07, 08 to Phases 26, not 25.

### Anti-Patterns Found

None blocking. A grep for TBD, FIXME and XXX over the Rust, shell and Python files changed in the phase finds none. `25-REVIEW.md` found 0 Critical, 0 Warning, 6 Info. `25-REVIEW-DISPOSITION.md` records the Info items: IN-01 and IN-02 deferred to Phase 26, IN-03 to IN-06 open (message wording, `Debug` on `Config`, bench default `--l`, dead `Clone` derive).

### Behavioral Spot-Checks

| Behavior | Command | Result | Status |
|----------|---------|--------|--------|
| Prolly crate (rule, node, golden) | `cargo test -p prolly` | 33 passed, 0 failed | PASS |
| Metadata crate (root record, allocator, store) | `cargo test -p metadata` | 191 unit tests and all integration binaries passed | PASS |
| Header refusals and salt | `cargo test -p slicefs-cli --test store_header_tests --test boundary_salt_tests` | 7 and 10 passed | PASS |
| Table matches generator | `python3 scripts/prolly-table/gen_table.py --check ...rule_table.rs` | exit 0 | PASS |

Code under `crates`, `benchmarks` and `scripts` is unchanged since commit 4a4e14d, the commit that the gate logs name; later commits change docs only.

### Notes for the owner (not gaps)

- IN-02 (deferred to Phase 26): format 8 says "prolly metadata maps" in `FORMAT`, but this build still writes flat maps. This is the accepted interim state in `25-CONTEXT.md` (no release between Phases 25 and 26). Phase 26 must refuse stores made by this interim build with a clear message, or must not leave such stores around.
- The `REQUIREMENTS.md` traceability table still shows PMAP-01, 02, 04 and 05 as `Pending`, and the ROADMAP Phase 25 checkboxes are unticked. The orchestrator should update these when it closes the phase.
- `docs/user/upgrade.md` has no copy steps yet. The roadmap assigns them to Phase 27 (#155), and criterion 4 only asks that the message points to the doc.
- Mount behavior on a format 8 store rests on the recorded Docker logs; I did not mount on the host.

---

_Verified: 2026-11-27_
_Verifier: Claude (gsd-verifier)_
