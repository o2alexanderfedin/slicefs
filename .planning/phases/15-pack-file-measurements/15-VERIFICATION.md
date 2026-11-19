---
phase: 15-pack-file-measurements
verified: 2026-11-19T07:22:22Z
status: passed
score: 3/3 must-haves verified
covered_files: [".planning/phases/15-pack-file-measurements/15-01-PLAN.md", ".planning/phases/15-pack-file-measurements/15-01-SUMMARY.md", ".planning/phases/15-pack-file-measurements/15-02-PLAN.md", ".planning/phases/15-pack-file-measurements/15-02-SUMMARY.md", ".planning/phases/15-pack-file-measurements/15-03-PLAN.md", ".planning/phases/15-pack-file-measurements/15-03-SUMMARY.md", "docs/design/chunk-store.md", "scripts/pack-measure/README.md", "scripts/pack-measure/seed-measure.sh"]
covered_digest: "v4:sha256:9c3ee515639d9c77a0c4d085d9b7a652baf7acb30ad6c5c96f6a133e881dac3e"
behavior_unverified: 0
overrides_applied: 0
---

# Phase 15: Pack-file measurements Verification Report

**Phase Goal:** The #8 numbers are recorded in `docs/design/chunk-store.md` and #8 is closed
**Verified:** 2026-11-19T07:22:22Z (retroactive, at develop b4b5494)
**Status:** passed
**Re-verification:** No (initial verification)

## Goal Achievement

### Observable Truths (ROADMAP success criteria)

| # | Truth | Status | Evidence |
|---|-------|--------|----------|
| 1 | One script measures seed throughput and store size for the Boost tree, a tar file and random data | VERIFIED | `scripts/pack-measure/seed-measure.sh` (104 lines, mode 100755, `bash -n` OK). The inputs `tree`, `tar` and `rand` are in `src_for()`. Per run it records wall secs, MB/s and peak RSS (`/usr/bin/time -l`), apparent bytes (`stat -f %z`), allocated bytes (`du -sk`), and the file count split by `vt0/`, `packs/`, `segments/` and other. It adds one median row per input (`median_low`). It calls only `slicefs seed "$store" "$src"`, so it runs the same way on old and new builds. A README is at `scripts/pack-measure/README.md`. |
| 2 | The numbers for a build from before #56 and for the current build are in the design note, with the machine and the commits | VERIFIED | `docs/design/chunk-store.md` line 918 has `### Results (2026-11-18)` under `## Measurements to take`. Machine: Xeon W-3245, 16 cores, 192 GiB, macOS 26.2, APFS on an SSD. Commits: c2f82fa (before packs), 7016bb6 (before #56) and c447551 (develop). Git check: c2f82fa is the merge of #44; 7016bb6 is the first parent of the #56 merge e038e9f; c447551 is the merge of #97. There are three tables (tree, tar, random). Every seconds, allocated MB and file-count cell in them equals the median row of the CSV that is copied in 15-02-SUMMARY.md (checked against that copy; the raw `seed.csv` is outside the repo). |
| 3 | The store file count after a Boost copy is recorded | VERIFIED (by an autonomous decision) | The doc records 426,739 / 170,376 / 10 store files after a `seed` of the Boost 1_85_0 tree, against the target "tens, not hundreds of thousands". This count is after a `slicefs seed` of the tree, not after a copy through an FSKit mount. That proxy is the decision [Store file count after Boost copy (PACK-03)] in 15-CONTEXT.md line 26, under "Decided autonomously" (no user decision). PACK-03 says that the user runs the FSKit round trip. |

**Score:** 3/3 roadmap truths verified (0 present, behavior-unverified). Plan truths: 13/14 verified, 1 not checkable from the repo (see below; it is not needed for the goal).

### Plan must-haves

`query verify.artifacts`: 15-01 2/2, 15-02 1/1. `query verify.key-links`: 15-01 2/2, 15-02 2/2. 15-03 has no artifacts and no key links.

| Plan | Truth (short) | Status | Evidence |
|------|---------------|--------|----------|
| 15-01 | Script seeds tree, tar and random into a new store per run; CSV row per run plus median row | VERIFIED | Loop over `$INPUTS`; `rm -rf store-$i` before each run; `row` per run; `median` row per input |
| 15-01 | Same script runs unchanged on the three builds | VERIFIED | Only `"$B" seed "$store" "$src"` is called (key link to cli.rs found) |
| 15-01 | c2f82fa store has vt0/ only; 7016bb6 has both; current has packs/ only | VERIFIED | Doc tables: c2f82fa `vt0/ 426,738`, 7016bb6 `vt0/ 170,366, packs/ 8`, c447551 `packs/ 8`, no vt0/ |
| 15-01 | A failed seed or a missing input exits non-zero, with no median row | VERIFIED (static) | `exit 1` on `rc != 0` before the median line; `nodata()` exits 2; `set -euo pipefail` |
| 15-01 | Three release binaries and the Boost data exist outside the repo; no worktree left registered | PARTIAL (not needed for the goal) | `git worktree list` has no `slicefs-measure` entry. The binaries and data are outside the worktree and were not checked (SUMMARY claim only) |
| 15-02 | seed.csv has 36 data rows | VERIFIED (SUMMARY copy) | The CSV copy in 15-02-SUMMARY has 27 run rows plus 9 median rows = 36 |
| 15-02 | One Results subsection under "Measurements to take", with the machine, the commits and one table per input | VERIFIED | Line 918; three tables |
| 15-02 | Table cells equal the CSV median rows | VERIFIED | All 9 rows checked against the SUMMARY CSV copy |
| 15-02 | File count after a seed of the Boost tree is stated for the three builds against the target | VERIFIED | "Store files after a seed of the Boost tree: ... c447551 10" |
| 15-02 | Subsection says that the FSKit round trip is manual and that nothing was mounted | VERIFIED | Paragraph that names `boost-test.sh` |
| 15-02 | No home path, host name, user name or licensing text | VERIFIED | grep for `/Users/`, `~/`, `GPL`, `licen`: 0 hits |
| 15-03 | #8 closed only after the merge and a green CI on develop's head | VERIFIED | PR #101 merged into develop 2026-11-18T12:11:05Z; CI on 2f127da succeeded 12:18:33Z. The #8 close comment is at 20:44:22Z; develop's head was then 6296eb6, whose CI succeeded at 20:43:47Z |
| 15-03 | Close comment names the script, c2f82fa, 7016bb6 and the doc section | VERIFIED | `gh issue view 8 --comments` |
| 15-03 | Close comment has no licensing text, no home path, no number that is not in the doc | VERIFIED | Comment text read; its numbers (426,739 / 170,376 / 10; 635 / 189 / 27 s) are in the doc |

### Required Artifacts

| Artifact | Expected | Status | Details |
|----------|----------|--------|---------|
| `scripts/pack-measure/seed-measure.sh` | Repeatable seed measurement | VERIFIED | 104 lines (min 60); CSV header present; no debt marker |
| `scripts/pack-measure/README.md` | How to run it | VERIFIED | 99 lines: inputs, run, output columns |
| `docs/design/chunk-store.md` `### Results (2026-11-18)` | Numbers, machine, commits | VERIFIED | Lines 918-984 |
| Issue #8 | Closed with the numbers | VERIFIED | CLOSED 2026-11-18T20:44:24Z. The comment points to the doc and says that the FSKit round trip is a manual run to add later |

### Key Link Verification

| From | To | Via | Status | Details |
|------|----|----|--------|---------|
| `seed-measure.sh` | `fetch-boost.sh` | data dir layout `x/boost_` | WIRED | gsd verify.key-links |
| `seed-measure.sh` | `crates/slicefs-cli/src/cli.rs` | `seed "$store" "$src"` | WIRED | gsd verify.key-links |
| design note | `seed-measure.sh`, `boost-test.sh` | the "To repeat" line and the FSKit line | WIRED | gsd verify.key-links |
| 15-01 and 15-02 commits | develop | ancestry | WIRED | ea91f2b, 85efd81, 68b4277 are ancestors of b4b5494 |

### Behavioral Spot-Checks

| Behavior | Command | Result | Status |
|----------|---------|--------|--------|
| Script parses | `bash -n scripts/pack-measure/seed-measure.sh` | exit 0 | PASS |
| Commit labels are right | `git log -1` on c2f82fa, e038e9f, c447551 | #44 merge; parents 7016bb6 323c09a; #97 merge | PASS |
| Seed runs | not run again (a 2.5 h job; static evidence is enough for this audit) | - | SKIP |

### FSKit Boost round trip (PACK-03 note): evidence

The `macos-mounts` workflow (`.github/workflows/macos-mounts.yml`) runs `scripts/macos-mount-test/boost-test.sh fskit 1_85_0` on a new macOS VM. It runs only on `main`, on `ci/**` branches, or by hand. It does not run on develop or on PRs.

- The last green run is 44168697882 (2026-11-17, a push to main, head b1316f0 = merge of PR #84). Its log has `RESULT fskit: 0 failure(s)` and `gc-freed: ok`. tar-to took 10 s and tree-to took 2020 s. CAS bytes after the tree were 451.97 MiB in `packs/`. Thus the FSKit round trip passed with the pack store.
- This evidence has limits:
  - b1316f0 is 12 develop commits before c447551, the build that was measured. No macos-mounts run exists after it.
  - `boost-test.sh` prints CAS bytes, dedup ratios and the time of each step (`perf.csv`). It prints no store file count. The uploaded artifacts (`mount.log`, `perf.csv`, `top.log`) have no file count either.
  - The doc and the #8 comment both say that the FSKit numbers are still to be added: the time per step, the `fsync` and `write` time per call, and the store file count after that copy.

Conclusion: CI shows that the FSKit Boost round trip works with packs. CI does not record the FSKit numbers. PACK-03 gives that run to the user, so this is not a gap for this phase.

### Requirements Coverage

| Requirement | Source Plan | Description | Status | Evidence |
|-------------|-------------|-------------|--------|----------|
| PACK-01 | 15-01 | A repeatable script measures `slicefs seed` throughput and on-disk store size for the Boost tree, a tar file and random data | SATISFIED | `scripts/pack-measure/seed-measure.sh` and its README (truth 1) |
| PACK-02 | 15-02 | The script runs on a pre-pack build and on the current build; the numbers go into `docs/design/chunk-store.md` with the machine and the commits | SATISFIED | `### Results (2026-11-18)` with c2f82fa, 7016bb6, c447551 and the machine line (truth 2) |
| PACK-03 | 15-02, 15-03 | The store file count after a Boost copy is recorded (target: tens); #8 closed; the FSKit round trip is run by the user | SATISFIED | 10 files after a seed of the Boost tree (c447551), against 426,739 before packs (seed proxy, an autonomous CONTEXT decision). #8 CLOSED 2026-11-18. The FSKit round trip is green in macos-mounts run 44168697882 (b1316f0), but its numbers are not in the doc (see tech debt) |

No orphaned requirements. REQUIREMENTS.md maps only PACK-01..03 to Phase 15, and plans claim all three.

### Anti-Patterns Found

None. There is no TBD, FIXME, XXX, TODO or HACK in `scripts/pack-measure/*`, and no debt marker in the doc subsection.

### Human Verification Required

None for the phase goal. The plans have no `<human-check>` block. The FSKit numbers are a planned manual follow-up (tech debt item 1), not a check of what this phase delivered.

### Non-critical gaps / tech debt

1. **The FSKit Boost round-trip numbers are not in the doc.** The doc says "Add its numbers here when that run is done". The missing numbers are the time per step, the `fsync` and `write` time per call, and the store file count after an FSKit copy. CI does not fill this in: `boost-test.sh` prints no file count and no time per call. To close it, add a `find "$R/store" -type f | wc -l` line to `boost-test.sh` (or run it once by hand) and paste the numbers into the doc.
2. **The FSKit round trip has not run on the current head.** The last macos-mounts run is at b1316f0 (2026-11-17). The next run is on a push to main or to `ci/**`.
3. **The timings were taken under load.** The 1-minute load average was 52-137 during the run, and the runs differ by up to 32 %. The doc labels seconds, MB/s and peak RSS as an indication only. 15-02-SUMMARY has an open item to measure the timings again on a quiet machine. The sizes and file counts are exact.
4. **Only three builds were measured.** The doc's "Measurements to take" asks for numbers "before (blockset) and after each of steps 2 to 4". Phase 15 measured c2f82fa, 7016bb6 and c447551. 15-CONTEXT.md line 95 defers the measurements after each separate step.
5. **The raw data is outside the repo** (per SUMMARY; not checked). `seed.csv`, `machine.txt` and `load.txt` are said to be in the measure root. The full CSV is copied in 15-02-SUMMARY.md, so the numbers can be traced, but the raw files are not versioned.
6. **macOS only.** The script needs `stat -f` and `/usr/bin/time -l`. A Linux run was out of scope (15-CONTEXT).
7. **Judgement call to note:** two verdicts rest on CONTEXT decisions marked "Decided autonomously". In this phase, a seed counts as the "Boost copy" (PACK-03). The user can reverse this if an FSKit copy count is required.

### Gaps Summary

There are no blocking gaps. All three success criteria hold in the code and the doc, and #8 is closed after the merge and green CI. PACK-03 is met through the seed-based file count, which 15-CONTEXT chose on purpose. The FSKit numbers stay open as tech debt. Develop CI for b4b5494 (run 44478418274) completed with success.

---

_Verified: 2026-11-19T07:22:22Z_
_Verifier: Claude (gsd-verifier)_
