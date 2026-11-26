---
phase: 22-chunk-profiles
verified: 2026-11-26T11:51:12Z
status: passed
score: 35/35 must-haves verified (4/4 roadmap criteria; 31/31 plan truths, 1 by override)
covered_files:
  - ".planning/phases/22-chunk-profiles/22-01-PLAN.md"
  - ".planning/phases/22-chunk-profiles/22-01-SUMMARY.md"
  - ".planning/phases/22-chunk-profiles/22-02-PLAN.md"
  - ".planning/phases/22-chunk-profiles/22-02-SUMMARY.md"
  - ".planning/phases/22-chunk-profiles/22-03-PLAN.md"
  - ".planning/phases/22-chunk-profiles/22-03-SUMMARY.md"
  - ".planning/phases/22-chunk-profiles/22-04-PLAN.md"
  - ".planning/phases/22-chunk-profiles/22-04-SUMMARY.md"
  - ".planning/phases/22-chunk-profiles/runs/chunk-profiles-edits.csv"
  - ".planning/phases/22-chunk-profiles/runs/chunk-profiles-machine.txt"
  - ".planning/phases/22-chunk-profiles/runs/chunk-profiles-results.csv"
  - "README.md"
  - "crates/chunk-store/examples/chunk_profile_bench.rs"
  - "crates/chunk-store/src/file.rs"
  - "crates/chunk-store/src/lib.rs"
  - "crates/chunk-store/tests/profile_tests.rs"
  - "crates/slicefs-cli/src/cli.rs"
  - "crates/slicefs-cli/src/content.rs"
  - "crates/slicefs-cli/src/header.rs"
  - "crates/slicefs-cli/src/main.rs"
  - "crates/slicefs-cli/src/mount.rs"
  - "crates/slicefs-cli/src/seed.rs"
  - "crates/slicefs-cli/src/stats.rs"
  - "crates/slicefs-cli/tests/chunk_profile_tests.rs"
  - "docs/design/chunk-store.md"
  - "docs/design/store-format.md"
  - "docs/research/chunk-profiles.md"
  - "scripts/chunk-profiles/README.md"
  - "scripts/chunk-profiles/bench.sh"
  - "scripts/docker-linux-test/run.sh"
covered_digest: "v4:sha256:3aeb1e40dd86932520b1dbe0111808df2cabb583eeee0f9dbb05ddf1a528d555"
behavior_unverified: 0
overrides_applied: 1
overrides:
  - must_have: "No cargo gate or other heavy job runs at the same time, and the load before and after is recorded"
    reason: "Byte and object counts are identical in all runs; times are marked indicative; the default does not change in v2.2. A quiet rerun was tried 2026-11-26: another project still used about 6 of 32 CPUs. Rerun on a quiet machine before any change of the default."
    accepted_by: "orchestrator, under the owner rule for reversible choices (decide and report)"
    accepted_at: "2026-11-26"
re_verification:
  previous_status: passed
  previous_score: 34/35 (1 uncertain, then accepted by override)
  previous_head: 1bdd735
  head: 37811d7
  gaps_closed: []
  gaps_remaining: []
  regressions: []
  rechecked_after_review_fixes:
    - "WR-01: seed checks the source before the header write (PROF-01 / SC1)"
    - "WR-02: profile max capped at 1 MiB; the five benchmarked profiles stay valid (SC1, SC3)"
    - "IN-02: refusal text names the profile once, in KiB"
    - "WR-05: stats prints the checked profile; JSON shape unchanged"
    - "WR-03, WR-04, IN-03, IN-04, IN-05: bench.sh, run.sh and report command line (SC3)"
human_verification:
  - test: "Confirm the two judgment-tier prohibitions: 22-02 'no Docker or macOS mount in this plan' and 22-04 'the executor runs no gh write command'."
    expected: "No mount ran for 22-02, and no gh issue or gh pr write command ran for 22-04."
    why_human: "unverified-prohibition, human review recommended. The files show no sign of either (no mount test added; no gh command in the summaries), but absence in files cannot prove that a command did not run. The orchestrator recorded on 2026-11-26 that neither ran (pushes and PRs are made by the orchestrator only); that is an agent statement, not an owner resolution, so the verifier does not close it."
---

# Phase 22: Chunk profiles Verification Report

**Phase Goal:** A store has one FastCDC profile, fixed when the store is seeded, and a benchmark shows what larger profiles cost and gain; the default stays 4/8/32 KiB (#67)
**Verified:** 2026-11-26T11:51:12Z
**Status:** human_needed (0 gaps; one judgment-tier prohibition item for the owner)
**Re-verification:** Yes. Previous run on `1bdd735` (status passed after the orchestrator override). This run is on `37811d7`, after the code review fixes `085dd3e..37811d7`.

Base of the phase: `1e7c158` (merge of #203). HEAD: `37811d7` (local, not pushed).

Scope of the re-check: `git diff 1bdd735 37811d7` changes `chunk-store/src/lib.rs` (`ChunkProfile::new`), `chunk_profile_bench.rs` (one error line), `slicefs-cli` `cli.rs` (help text), `content.rs`, `header.rs` (one test line), `seed.rs`, `stats.rs`, the two profile test files, `bench.sh`, `run.sh`, README.md and three docs. It does not change `chunk-store/src/file.rs`, `crates/metadata`, `mount.rs`, `main.rs`, any `Cargo.toml` or `Cargo.lock`, or the committed run data in `runs/`. Items that rest only on unchanged files are carried from the previous report.

Gate evidence:
- Previous gate, on `ed1694a` (code equal to `1bdd735`): 1605 passed, 0 failed (`runs/gate-22-04.log`).
- Gate at `37811d7`, reported by the fixer (no log in `runs/`): cargo test --workspace 1608 passed, 0 failed; clippy ok (`UNSAFE files=31 tokens=132`); bench.sh smoke ok on macOS and in Linux Docker.
- Run by me at `37811d7`: the targeted tests and the macOS smoke in "Behavioral Spot-Checks" below. I did not run the full workspace, Docker or a mount.

## Goal Achievement

### Roadmap success criteria

| # | Criterion | Status | Evidence |
|---|-----------|--------|----------|
| 1 | `seed` accepts a profile (min/avg/max, powers of two, inside the crate limits) and keeps it in FORMAT; no setting gives 4/8/32 KiB; a mount with another profile is refused | VERIFIED | `ChunkProfile::new` (crates/chunk-store/src/lib.rs) now checks, in this order: power of two, min < avg < max, max <= `MAX_LIMIT` (1 MiB, new in WR-02), the fastcdc v2020 limits (from the crate constants), the zero cut. The 1 MiB cap is stricter than the crate maximums, so every accepted profile is still inside the crate limits; with max <= 1 MiB and min < avg < max, min <= 256 KiB and avg <= 512 KiB, both under the crate maximums (1 MiB and 4 MiB). `header::parse_chunks` uses the same `new`, so a FORMAT `chunks` line with max above 1 MiB is M9 (`chunks 131072 524288 2097152` added to the bad-line test). The seed/mount flow is unchanged except WR-01: `run_seed_with` now does `create_dir_all`, `lock_offline`, then checks the source (exists, is a dir, can be listed) before `check_write_with` writes the header, so a bad source no longer fixes the profile. New test `a_failed_seed_does_not_fix_the_profile` (missing source and a file as source: no FORMAT, packs, index or segments; then a seed with 8/16/64 works and the store has that profile). The other SC1 tests run again and pass: `seed_with_a_profile_makes_the_store_with_it`, `a_default_store_keeps_the_header_of_today`, `seed_refuses_another_profile_and_changes_nothing`, `mount_checks_the_profile`, `a_read_only_mount_refuses_another_profile`, `bad_profiles_are_refused`, `the_max_is_at_most_1_mib`, `parse_chunk_profile_refuses_bad_values`, `a_refused_profile_is_named_once_in_kib`. |
| 2 | Two stores with different profiles work in one process (byte for byte, zero runs, spliced edit); a golden cut-point test fails if a cut point moves | VERIFIED | Not touched by the fixes (`file.rs` unchanged; `golden_cut_points` unchanged). Run again and green: `two_profiles_in_one_process_read_back_byte_for_byte`, `golden_cut_points`, `zero_chunks_of_each_store_need_no_store_read` (profile_tests, 7 passed) and `two_profiles_through_the_file_system` (chunk_profile_tests, 10 passed). The golden values are in the plan commit `b22e969`, before any code. |
| 3 | One reproducible script reports the seven metrics for the five profiles; it runs outside the test suite | VERIFIED | The five profiles (4/8/32, 8/16/64, 8/32/128, 16/64/256, 32/128/512 KiB; largest max 512 KiB) are all under the 1 MiB cap: `the_five_profiles_and_the_limits_are_accepted` passes, and `bench.sh` default `PROFILES` is the same list. Metric -> column mapping is unchanged (see the previous report; the example changed only one error line). bench.sh fixes checked: WR-03 store in `mktemp -d "$WORK/store.XXXXXX"` with an EXIT trap (I put a `WORK/store/sentinel` before the smoke run; it was still there after, and no `store.*` dir was left); WR-04 median substitution checked for failure and empty output, `python3` checked at start; IN-03 a missing data dir is refused with exit 2 and is not made (run by me), rand/sparse inputs go to `WORK/inputs` (`gen_file` makes the parent dir); IN-04 `timeout 1200` on the build and `timeout 600` on bench.sh in `run.sh chunk-profiles-smoke`, still no `--device`/`--cap-add`. My smoke run: exit 0 in 14 s, 30 run rows + 10 median rows, `objects`/`stored_bytes`/`apparent_bytes`/`edit_objects`/`edit_stored_bytes` equal across runs and equal to the committed `runs/smoke-22-03-macos.csv` for the 4 shared keys. |
| 4 | The report recommends a default; a store seeded without a setting is still 4/8/32 KiB after v2.2 | VERIFIED | `ChunkProfile::DEFAULT` is unchanged (4/8/32); the diff does not touch it or `profile_of`. `the_five_profiles_and_the_limits_are_accepted` asserts `new(4096, 8192, 32768) == DEFAULT`, and `golden_cut_points` pins the default cuts; both pass. `docs/research/chunk-profiles.md` still recommends 8/32/128 KiB for a later release (max 128 KiB, inside the cap) and keeps 4/8/32 in v2.2; the only change there is the IN-05 `mkdir -p "$M/results"` line. The report tables and `runs/chunk-profiles-results.csv` are unchanged since the previous check against each other. |

### Plan truths (merged)

| Plan | Truths | Status | Notes |
|------|--------|--------|-------|
| 22-01 | 10 | 10 VERIFIED | Carried; `file.rs`, `crates/metadata` and Cargo files unchanged since `1bdd735`. Profile validation changed only by the cap (see SC1); profile_tests 7/7 and `for_each_profile` lib tests 2/2 run again. |
| 22-02 | 9 | 9 VERIFIED | Header pairing, round trip and M9 run again (`header::tests` 20 passed). Stats line and JSON: WR-05 keeps the checked `ChunkProfile` in `StoreStats` and serializes it through `profile_json` as the same `{min, avg, max}` object; `stats_shows_the_chunk_profile` and `stats::tests` (24 passed) green. Kani byte core: `header.rs` changed only one test line, so `runs/gate-22-02.log` (`KANI ok harnesses=9`) still applies. |
| 22-03 | 6 | 6 VERIFIED | Example subcommands, driver columns, median rows, inputs, Linux smoke mode with no FUSE device. Input locations for rand/sparse moved to `WORK/inputs`; README and the bench.sh header comment say so. |
| 22-04 | 6 | 5 VERIFIED, 1 PASSED (override) | Docs, report content, phase gate (1608 reported). Truth 4 "no other heavy job runs at the same time": PASSED (override), see frontmatter. |

### ro_compat check (D-11)

Carried unchanged: `file.rs` did not change. Both write paths store the zero chunk, and an older reader reads a non-default zero chunk from the packs. The cap makes the largest zero chunk 1 MiB.

### Review fix check

| Fix | Change | Check | Result |
|-----|--------|-------|--------|
| WR-01 | Source check before the header write in `run_seed_with` | `a_failed_seed_does_not_fix_the_profile` run by me | holds |
| WR-02 | `ChunkProfile::MAX_LIMIT = 1 << 20`, checked before the fastcdc limits | `the_max_is_at_most_1_mib`, five-profile test, header bad-line case, `parse_chunk_profile` 64/256/1024 accepted and 128/512/2048 refused | holds; all five benchmarked profiles stay valid |
| IN-02 | `new` returns the reason only; CLI adds `chunk profile {s} KiB: `, bench adds `--profile {p} KiB: ` | `a_refused_profile_is_named_once_in_kib` | holds |
| WR-05 | `StoreStats::chunk_profile` is a `ChunkProfile`; `human_stats` prints it with no second `new` | `stats::tests` (24), `stats_shows_the_chunk_profile` | holds; JSON field unchanged |
| WR-03 | Store dir by `mktemp -d`, trap removes only it | smoke with a sentinel `WORK/store` | holds |
| WR-04 | Median step failure and empty row exit 1; `python3` check | read the code; no failure injected | holds by reading |
| IN-03 | Data dir not made outside smoke; generated inputs under `WORK/inputs` | missing data dir run: exit 2, `no data dir: ...`, dir not made | holds |
| IN-04 | `timeout 1200` / `timeout 600` in run.sh, `timeout` in README lines | read the code | holds by reading; Docker smoke reported ok by the fixer |
| IN-05 | `mkdir -p "$M/results"` in the report command line | read the doc | holds |

Deferred by the orchestrator, not gaps for the phase goal: IN-01 (zero premise repeats per header parse; bounded, and smaller now with the cap), IN-06 (macOS backend selection before the M16 refusal; the refusal still comes before any lock or load), IN-07 (`--json` error escaping, a pattern older than the phase), IN-08 (`StoreIo::open` has only test callers).

### Docs against code

- README.md, `cli.rs` help (seed and mount), `store-format.md` chunks line and `chunk-store.md` all say max at most 1 MiB (1024 KiB) and the fastcdc minimums; this matches `ChunkProfile::new`. `chunk-store.md` now gives the total stream memory outside `--dirty-budget` (N x under 2 MiB, under 4 MiB during a step), which the 1 MiB cap gives.
- No stale "16 MiB" or "4 MiB" profile limit text is left: `git grep -E "16 MiB|4 MiB"` over `crates/chunk-store/examples`, `file.rs`, `crates/slicefs-cli/src` and the docs finds only unrelated sizes (write log, packs, the sparse input, #98 numbers).
- M9 reason list in store-format.md (`bad chunks line`, `two chunks lines`, and the two pairing reasons) matches header.rs; header errors do not include the `ChunkProfile::new` reason, so IN-02 does not change them.
- scripts/chunk-profiles/README.md matches bench.sh for the store dir, the inputs dir, the data dir refusal, the python3 check, the median exit status and the time limits.

### Prohibitions (must NOT)

| Plan | Prohibition | Tier | Check | Result |
|------|-------------|------|-------|--------|
| 22-01 | file.rs changes are constant reads only; no pub item renamed, moved or re-typed | test | Carried; `file.rs` unchanged since `1bdd735` | held |
| 22-01 | No new dependency and no change in crates/metadata | test | `git diff --stat 1bdd735 HEAD -- Cargo.lock Cargo.toml '*/Cargo.toml' crates/metadata crates/chunk-store/src/file.rs` | empty: held |
| 22-01 | The default header and the default cut points do not change | test | `golden_cut_points`, `a_default_store_keeps_the_header_of_today` run by me | green: held |
| 22-02 | A refused `--chunk-profile` changes nothing in the store | test | `seed_refuses_another_profile_and_changes_nothing`, `mount_checks_the_profile`, `a_read_only_mount_refuses_another_profile` run by me | green: held (lock-file note of the previous report still applies) |
| 22-02 | The Kani byte core of the header parser does not change | test | `header.rs` diff since `1bdd735` is one test line; `runs/gate-22-02.log` | held |
| 22-02 | No Docker or macOS mount in this plan | judgment | No mount test added | held (LLM judgment, non-authoritative); flagged, see Human Verification |
| 22-03 | The benchmark never runs inside cargo test | test | No `#[test]` in the example (diff is one error line) | held |
| 22-03 | No new crate dependency and no unsafe | test | Cargo diff empty; fixer clippy `UNSAFE files=31 tokens=132 result=ok` | held |
| 22-03 | No FUSE mount in the Docker smoke mode | test | `run.sh chunk-profiles-smoke` diff adds only `timeout`; no `--device`, no `--cap-add` | held |
| 22-04 | No number in the report that is not in the committed CSV | test | Report tables and CSV unchanged since the previous script check | held |
| 22-04 | The code default does not change | test | Diff does not touch `ChunkProfile::DEFAULT`; `the_five_profiles_and_the_limits_are_accepted` passes | held |
| 22-04 | The executor runs no `gh` write command | judgment | No `gh issue`/`gh pr` command in 22-04-SUMMARY | held (LLM judgment, non-authoritative); flagged, see Human Verification |

### Required Artifacts

`verify.artifacts` at `37811d7`: 22-01 6/6, 22-02 7/7, 22-03 4/4, 22-04 5/5 (all passed).

### Key Link Verification

`verify.key-links` at `37811d7`: 3/3 for each of the four plans. The manual links of the previous report rest on `main.rs`, `mount.rs` and the `content::open`/`open_read_only` profile setting, which the fixes do not change.

### Data-Flow Trace (Level 4)

| Artifact | Data | Source | Real data | Status |
|----------|------|--------|-----------|--------|
| stats.rs `Chunk profile` line / `chunk_profile` JSON | profile | `header::check` -> `content::profile_of` (now kept as `ChunkProfile`, printed by `Display`, serialized by `profile_json`) | yes | FLOWING |
| content store `Options::profile` | profile | parsed FORMAT `chunks` line | yes | FLOWING |
| report tables | numbers | `runs/chunk-profiles-results.csv` median rows (unchanged) | yes | FLOWING |

### Behavioral Spot-Checks

| Behavior | Command | Result | Status |
|----------|---------|--------|--------|
| Profile limits incl. 1 MiB cap, golden cut points, per-store zero chunk, two stores, old reader | `cargo test -p chunk-store --test profile_tests` | 7 passed | PASS |
| Seed/mount set, check and refuse; failed seed leaves no profile; read-only mount; two file systems | `cargo test -p slicefs-cli --test chunk_profile_tests` | 10 passed | PASS |
| CLI parse and refusal text; profile lib tests | `cargo test -p slicefs-cli --lib -- chunk_profile` / `profile_` / `stats_shows` | 4 / 5 / 2 passed | PASS |
| Zero cut and splice key for each of five profiles | `cargo test -p chunk-store --lib -- for_each_profile` | 2 passed | PASS |
| Header parse, pairing, bad lines (incl. max above 1 MiB) | `cargo test -p slicefs-cli --lib -- header::tests` | 20 passed | PASS |
| Stats line and JSON | `cargo test -p slicefs-cli --lib -- stats::tests` | 24 passed | PASS |
| Seed unit tests | `cargo test -p slicefs-cli --lib -- seed` | 8 passed | PASS |
| Benchmark smoke, five profiles, existing `WORK/store` untouched | `SMOKE=1 WORK=<scratch>/w BIN=target/release/examples/chunk_profile_bench timeout 600 bash scripts/chunk-profiles/bench.sh` | exit 0, 14 s, 30 run + 10 median rows, counts equal across runs and to the committed smoke CSV, sentinel kept | PASS |
| Missing data dir refused, not made | `WORK=<scratch>/w2 ... bash scripts/chunk-profiles/bench.sh <scratch>/nodata 1` | exit 2, `no data dir: ...`, dir does not exist after | PASS |

### Probe Execution

No probes: no `scripts/*/tests/probe-*.sh` and no plan or summary names one.

### Requirements Coverage

| Requirement | Source Plan | Status | Evidence |
|-------------|-------------|--------|----------|
| PROF-01 | 22-01, 22-02, 22-04 | SATISFIED | SC1 evidence (now with the 1 MiB cap and the WR-01 order); docs in store-format.md. |
| PROF-02 | 22-01, 22-02 | SATISFIED | SC2 evidence; no profile reads changed (`file.rs` unchanged). |
| PROF-03 | 22-03, 22-04 | SATISFIED | SC3 and SC4 evidence; the five listed profiles are valid under the cap. |

No orphaned requirements: REQUIREMENTS.md maps only PROF-01..03 to Phase 22. In the working tree (not committed) the three checkboxes are `[x]` and the traceability rows say "Complete".

### Anti-Patterns Found

| File | Line | Pattern | Severity | Impact |
|------|------|---------|----------|--------|
| scripts/chunk-profiles/bench.sh | 45, 50 | `XXXXXX` | Info | mktemp templates (work dir and the new store dir), not debt markers |
| scripts/chunk-profiles/README.md, bench.sh header | - | `WORK/store.XXXXXX` | Info | Doc text for the mktemp dir, not a debt marker |
| crates/slicefs-cli/src/seed.rs | 51-60 | source check after `create_dir_all` and `lock_offline` | Info | A failed seed can leave an empty store dir with a lock file. The test shows no FORMAT, packs, index or segments, and a later seed with another profile works, so the WR-01 aim holds. |
| crates/slicefs-cli/src/filesystem.rs | 2578 | comment says "2 x CHUNK_MAX" | Info | Old comment, not in this phase's changes (carried). |

No TBD, FIXME, XXX (debt), TODO, HACK or PLACEHOLDER in lines added by the fixes. No home paths in the added lines.

### Advisory (New Scope, Unevidenced)

None.

### Human Verification Required

### 1. Two judgment-tier prohibitions

**Test:** Confirm that no mount ran for 22-02 and that no `gh` write command ran for 22-04.
**Expected:** Both hold.
**Why human:** These are judgment-tier prohibitions (unverified-prohibition, human review recommended). The files show no sign of either, but that does not prove a command did not run. The orchestrator recorded on 2026-11-26 that neither ran; that is an agent statement, not an owner resolution, so this report does not close the item. To close it, the owner confirms, and the status can then be `passed`.

The benchmark-times item of the previous report is closed by the override in the frontmatter.

### Gaps Summary

No gaps. All four roadmap criteria and PROF-01..03 hold at `37811d7`. The review fixes do not break any earlier evidence: the 1 MiB cap keeps all five benchmarked profiles and the recommended 8/32/128 valid, and the docs, CLI help and README match the new limit. The only open item is the owner confirmation of the two judgment-tier prohibitions.

---

_Verified: 2026-11-26T11:51:12Z_
_Verifier: Claude (gsd-verifier)_

## Results after the verification (orchestrator, 2026-11-26, updated for 37811d7)

- Code review 22-REVIEW.md: 0 critical, 5 warnings, 8 info. All 5 warnings and IN-02..IN-05 fixed (085dd3e..37811d7), each with a test where code changed; IN-01, IN-06, IN-07, IN-08 deferred. WR-02 decision: chunk profile max capped at 1 MiB (all five benchmarked profiles stay valid; checked in this re-verification).
- Gate at `37811d7` (fixer report): cargo test --workspace (4 abort tests skipped) 1608 passed, 0 failed; clippy ok (UNSAFE files=31 tokens=132); bench.sh smoke ok on macOS and in Linux Docker. This re-verification ran the targeted tests and the macOS smoke itself (all green).
- Owner item 1 (quiet machine): accepted with the override above. Owner item 2: the orchestrator records that no mount ran in 22-02 and no gh write ran in 22-04 (pushes and PRs are made by the orchestrator only); open for owner confirmation (see Human Verification).

## Orchestrator check of the two prohibitions (2026-11-26)

- No `gh` write in 22-04: the GitHub API shows no issue, comment or repository event on o2alexanderfedin/slicefs after 2026-11-26T09:30Z, which covers the whole 22-04 run (10:32Z to 10:38Z and the commits after it). Every push and PR of this phase was made by the orchestrator.
- No mount in 22-02: the executor reported no mount, Docker or `run.sh` command, and nothing in its plan needs one (`cargo test --workspace` mounts nothing on macOS). The only FUSE-T mounts on this machine that day were the orchestrator's own manual checks, which the owner asked for.
