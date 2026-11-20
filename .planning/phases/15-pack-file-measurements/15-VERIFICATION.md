---
phase: 15-pack-file-measurements
verified: 2026-11-20T09:44:48Z
status: passed
score: 3/3 roadmap truths verified (plan truths 13/14; 1 setup truth not checkable from the repo, not needed for the goal)
covered_files: [".planning/phases/15-pack-file-measurements/15-01-PLAN.md", ".planning/phases/15-pack-file-measurements/15-01-SUMMARY.md", ".planning/phases/15-pack-file-measurements/15-02-PLAN.md", ".planning/phases/15-pack-file-measurements/15-02-SUMMARY.md", ".planning/phases/15-pack-file-measurements/15-03-PLAN.md", ".planning/phases/15-pack-file-measurements/15-03-SUMMARY.md", "crates/slicefs-cli/src/seed.rs", "docs/design/chunk-store.md", "scripts/macos-mount-test/boost-test.sh", "scripts/pack-measure/README.md", "scripts/pack-measure/seed-measure.sh"]
covered_digest: "v4:sha256:0b7a9b5ef8d3a539f2923b5444de4c821fad432bfb5053660c7d00e7b1bd8ca8"
behavior_unverified: 0
overrides_applied: 0
---

# Phase 15: Pack-file measurements Verification Report

**Phase Goal:** The #8 numbers are recorded in `docs/design/chunk-store.md` and #8 is closed
**Verified:** 2026-11-20T09:44:48Z, on branch docs/v2.1-close-items (develop a7b29a2 plus uncommitted planning, doc and comment edits)
**Status:** passed
**Re-verification:** No. The 2026-11-19 report (b4b5494) had no `gaps:` section. It was stale because the covered files changed. A first run of this report earlier today found one wording gap (a stale header pattern in 15-01 and a stale FSKit truth in 15-02). The plan wording was then updated, and this report checks the tree after that update.

## What changed since the 2026-11-19 report

- `scripts/pack-measure/seed-measure.sh` and `README.md`: commit 1b74ba8 added an `index_files` column. Before, the `index/` files of the Phase 17 index were counted as "other" (audit warning W1).
- `docs/design/chunk-store.md`: in `### Results (2026-11-18)`, only the FSKit paragraph changed. The "manual run, add its numbers later" text is replaced by the FSKit store file count (48) and a table of the time per step. The three seed tables did not change (section diff between b4b5494 and HEAD).
- Uncommitted edits (2026-11-20):
  - `15-01-PLAN.md` `contains:` now has the 16-column header with `index_files`.
  - The 15-02 truth now reads "The subsection names the FSKit Boost round trip (boost-test.sh) and gives its store file count; this phase mounted nothing".
  - `chunk-store.md:1564-1565` now says that only the `fsync` and `write` time per call on FSKit is not measured.
  - The `seed.rs` doc comment now lists `packs/` once, plus `index/`. This is a comment-only diff.

## Goal Achievement

### Observable Truths (ROADMAP success criteria)

| # | Truth | Status | Evidence |
|---|-------|--------|----------|
| 1 | One script measures seed throughput and store size for the Boost tree, a tar file and random data | VERIFIED | `scripts/pack-measure/seed-measure.sh` (`bash -n` OK). `src_for()` has `tree`, `tar` and `rand`. Each run records wall seconds, MB/s and peak RSS (`/usr/bin/time -l`), apparent bytes, allocated bytes (`du -sk`), and file counts for `vt0/`, `packs/`, `segments/`, `index/` and other. The columns agree: the header has 16 fields, each row has 16 fields, and `median()` takes `range(3, 16)` = 13 numeric columns after 3 key fields. The script calls only `"$B" seed "$store" "$src"`. That matches `Cmd::Seed { store, source_dir, .. }` in `crates/slicefs-cli/src/cli.rs:109` (`test_seed` parses `slicefs seed /data /src`). |
| 2 | The numbers for a build from before #56 and for the current build are in the design note, with the machine and the commits | VERIFIED | `docs/design/chunk-store.md:1506` `### Results (2026-11-18)` under `## Measurements to take` (line 1497). The note gives the machine (Xeon W-3245, 192 GiB, macOS 26.2, APFS SSD) and three builds. Git confirms them: c2f82fa = merge of #44; 7016bb6 = first parent of e038e9f (merge of #56); c447551 = merge of #97. The three tables (tree, tar, random) are byte-identical to the version checked on 2026-11-19 against the CSV copy in 15-02-SUMMARY.md, so that cell-by-cell check still holds. |
| 3 | The store file count after a Boost copy is recorded | VERIFIED | `chunk-store.md:1561-1565`: "Store files after the FSKit Boost round trip ... macOS CI run 44484947311, 2026-11-19, commit c921a11 ...: 48 (packs/ 41, index/ 4, segments/ 1)". `boost-test.sh:101-103` prints this count. The log of run 44484947311 has the same line. That run is in the "macOS mounts" workflow, it succeeded, and its head c921a11 is an ancestor of HEAD. The log also has `gc-freed: ok` and `RESULT fskit: 0 failure(s)`. The seed-based count (10 files at c447551, against 426,739 before packs) is also in the doc. Both meet the target "tens, not hundreds of thousands". |

**Score:** 3/3 roadmap truths verified (0 present, behavior-unverified). Plan truths: 13/14 verified. 1 setup truth is not checkable from the repo and is not needed for the goal.

### Plan must-haves

| Plan | Truth (short) | Status | Evidence |
|------|---------------|--------|----------|
| 15-01 | Script seeds tree, tar and random into a new store per run; one CSV row per run plus a median row | VERIFIED | `rm -rf store-$i` before each run, `row` per run, `median` row per input |
| 15-01 | Same script runs unchanged on the three builds | VERIFIED | Only positional `seed "$store" "$src"`; key link found |
| 15-01 | c2f82fa store has vt0/ only; 7016bb6 has both; current has packs/ only | VERIFIED | Doc tables: c2f82fa `vt0/ 426,738`; 7016bb6 `vt0/ 170,366, packs/ 8`; c447551 `packs/ 8`, no vt0/ |
| 15-01 | A failed seed or a missing input exits non-zero, with no median row | VERIFIED (static) | `exit 1` on `rc != 0` before `median`; `nodata()` exits 2; `set -euo pipefail` |
| 15-01 | Three release binaries and Boost data outside the repo; no worktree left registered | PARTIAL (not needed for the goal) | The binaries and data are outside the repo, so this report cannot check them (SUMMARY claim only). This truth is a setup step for the measurement run, not a roadmap criterion |
| 15-02 | seed.csv has 36 data rows (3 builds x 3 inputs x (3 runs + 1 median)) | VERIFIED (SUMMARY copy) | The CSV copy in 15-02-SUMMARY.md has 36 rows that start with c2f82fa, 7016bb6 or c447551 |
| 15-02 | One `### Results (DATE)` subsection under "Measurements to take", with the machine, the commits and one table per input | VERIFIED | Line 1506; three tables |
| 15-02 | Table cells equal the CSV median rows | VERIFIED (unchanged) | Tables unchanged since the 2026-11-19 check (section diff) |
| 15-02 | File count after a seed of the Boost tree stated for the three builds against the target | VERIFIED | "Store files after a seed of the Boost tree: c2f82fa 426,739, 7016bb6 170,376, c447551 10" |
| 15-02 | The subsection names the FSKit Boost round trip (boost-test.sh) and gives its store file count; this phase mounted nothing | VERIFIED | Lines 1561-1565 name `scripts/macos-mount-test/boost-test.sh` and give 48 store files from CI run 44484947311. The seed tables come from `slicefs seed` only, and the FSKit count comes from CI, not from this phase |
| 15-02 | No home path, host name, user name or licensing text; no existing doc line changed | VERIFIED | grep of the section for `/Users/`, `~/`, `GPL`, `licen`: 0 hits. "No existing line changed" was true for the phase commit 68b4277 (checked 2026-11-19). Later commits edit the doc on purpose |
| 15-03 | #8 closed only after the merge and a green CI on develop's head | VERIFIED (timing carried) | `gh issue view 8`: CLOSED 2026-11-18T20:44:24Z. The CI timing check is carried from the 2026-11-19 report: develop head 6296eb6 was green at 20:43:47Z |
| 15-03 | Close comment names the script, c2f82fa, 7016bb6 and the doc section | VERIFIED | I re-read the full comment of 2026-11-18T20:44:22Z. It names `seed-measure.sh`, c2f82fa, 7016bb6 and "Measurements to take" |
| 15-03 | Close comment has no licensing text, no home path, no number that is not in the doc | VERIFIED | grep of the full comment for `GPL`, `licen`, `/Users/`, `~/`: 0 hits. Its numbers are issue refs (#56, #82, #85) or doc values: 77,655, 426,739 / 170,376 / 10, and 635 / 189 / 27 s (634.64 / 189.32 / 26.74 rounded) |

### Required Artifacts

`query verify.artifacts`: 15-01 2/2, 15-02 1/1 (both exit 0); 15-03 has no artifacts (exit 66). `query verify.key-links`: 15-01 2/2, 15-02 2/2; 15-03 has no key links (exit 66).

| Artifact | Expected | Status | Details |
|----------|----------|--------|---------|
| `scripts/pack-measure/seed-measure.sh` | Repeatable seed throughput and store-size measurement | VERIFIED | The 16-column header pattern is found. The script is substantive and wired (truth 1); no debt marker |
| `scripts/pack-measure/README.md` | Usage, inputs, CSV columns | VERIFIED | Documents `index_files` ("`index/` exists since #85") |
| `docs/design/chunk-store.md` `### Results (2026-11-18)` | Numbers, machine, commits, file count | VERIFIED | Lines 1506-1586 |
| Issue #8 | Closed with the numbers | VERIFIED | CLOSED 2026-11-18T20:44:24Z |

### Key Link Verification

| From | To | Via | Status | Details |
|------|----|----|--------|---------|
| `seed-measure.sh` | `fetch-boost.sh` | data dir layout `x/boost_` | WIRED | verify.key-links |
| `seed-measure.sh` | `crates/slicefs-cli/src/cli.rs` | `seed "$store" "$src"` | WIRED | verify.key-links; `Cmd::Seed` is still positional |
| `chunk-store.md` | `seed-measure.sh` | "To repeat" line | WIRED | verify.key-links |
| `chunk-store.md` | `boost-test.sh` | FSKit paragraph | WIRED | verify.key-links; `boost-test.sh:103` prints the store file count |

### Data-Flow Trace (Level 4)

| Artifact | Data | Source | Real data | Status |
|----------|------|--------|-----------|--------|
| FSKit file count (doc) | 48 (packs 41, index 4, segments 1) | `boost-test.sh:103` in CI run 44484947311 | Yes: the same line is in the run log | FLOWING |
| FSKit time-per-step table (doc) | 10/6/16, 2,183, 495, 73/383, 17, 2,332 s; GC 0.2 s, 81,290 objects, 199,859,608 bytes | the same run log | Yes: every cell matches the `: ok (Ns)` lines and the GC line | FLOWING |
| Seed tables (doc) | medians | 15-02-SUMMARY CSV copy | Yes (checked 2026-11-19; tables unchanged) | FLOWING |

### Behavioral Spot-Checks

| Behavior | Command | Result | Status |
|----------|---------|--------|--------|
| Script parses | `bash -n scripts/pack-measure/seed-measure.sh` | exit 0 | PASS |
| Commit labels are right | `git log -1` on c2f82fa, 7016bb6, c447551, e038e9f | #44 merge; #55 merge (= first parent of #56 merge e038e9f); #97 merge | PASS |
| FSKit run and count | `gh run view 44484947311 --log` | success; `store files: 48 ...`; `RESULT fskit: 0 failure(s)` | PASS |
| Develop CI on a7b29a2 | `gh run list --commit a7b29a2` | CI: success | PASS |
| Seed run | not run (a 2.5 h job on macOS; static evidence is enough) | - | SKIP |

The `seed.rs` edit changes only a doc comment, so a new CI run is not needed for it.

### Probe Execution

None. The phase declares no probes, and `scripts/*/tests/probe-*.sh` is out of scope for this phase.

### Requirements Coverage

| Requirement | Source Plan | Description | Status | Evidence |
|-------------|-------------|-------------|--------|----------|
| PACK-01 | 15-01 | A repeatable script measures `slicefs seed` throughput and store size for the Boost tree, a tar file and random data | SATISFIED | Truth 1 |
| PACK-02 | 15-02 | Runs on a pre-pack build and the current build; numbers in `chunk-store.md` with the machine and the commits | SATISFIED | Truth 2 |
| PACK-03 | 15-02, 15-03 | Store file count after a Boost copy recorded (target: tens); #8 closed; FSKit round trip run by the user | SATISFIED | 48 files after an FSKit copy (CI run 44484947311) and 10 after a seed; #8 closed |

No orphaned requirements: REQUIREMENTS.md maps only PACK-01..03 to Phase 15, and the plans claim all three.

### Anti-Patterns Found

None. The earlier info items are fixed:
- `chunk-store.md:1564-1565` now says only that the `fsync` and `write` time per call is not measured.
- `seed.rs:30-31` now lists `packs/` once, plus `index/`.

Debt-marker gate: there is no TBD, FIXME or XXX in `docs/design/chunk-store.md`, `crates/slicefs-cli/src/seed.rs`, `scripts/macos-mount-test/boost-test.sh` or `scripts/pack-measure/*`.

### Human Verification Required

None. The plans have no `<human-check>` block and no `prohibitions:` block. The FSKit round trip that PACK-03 gives to the user ran in macOS CI (run 44484947311), and its numbers are in the doc.

### Non-critical tech debt

1. **The `fsync` and `write` time per call on FSKit is not measured.** "Measurements to take" asks for it, and the doc says that it is still open.
2. **The seed timings were taken under load** (load average 52-137; runs differ by up to 32 %). The doc labels them as an indication only. Sizes and file counts are exact.
3. **Only three builds were measured.** The doc asks for numbers after each of steps 2 to 4. 15-CONTEXT defers this.
4. **The raw `seed.csv` is outside the repo.** The full CSV is copied in 15-02-SUMMARY.md.
5. **macOS only** (`stat -f`, `/usr/bin/time -l`), by design.
6. **The seed tables were made with the 15-column script.** A run of the current script gives 16 columns. The README documents the new column.
7. **The 2026-11-20 edits are not committed.** These are the plan wording, the doc sentence and the `seed.rs` comment. This report's fingerprint covers the edited files, so it goes stale if those files change again.

### Gaps Summary

There are no gaps. The phase goal is achieved: the #8 numbers are in `chunk-store.md`, and #8 is closed. All 3 roadmap criteria and 13 of 14 plan truths hold on the current tree. The one plan truth left (the release binaries and data outside the repo) cannot be checked from the repo, and the goal does not need it. Both earlier wording gaps are closed by the plan updates, so no override is needed. Develop CI on a7b29a2 is green.

---

_Verified: 2026-11-20T09:44:48Z_
_Verifier: Claude (gsd-verifier)_
