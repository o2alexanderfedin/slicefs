---
phase: 20-dirty-budget-and-rebase-flusher
plan: 01
subsystem: concurrency
status: complete
tags: [lock-02, lock-06, measurement, wave-0, s7, rss-cap, take-out-guard]

requires:
  - phase: 19-fine-grained-locks
    provides: "per-cell locks, no cross-file take-out, debug rank rule (second Rank::Cell lock panics)"
provides:
  - "S7_SERIALIZE knob and the serialize= and vmhwm_mib= fields of the S7 line"
  - "CAP_MIB = 320 in s7_budget_baseline, fixed from P_old and P0 before the flusher exists"
  - "runs/s7-wave0.log (6 runs), runs/s7-wave0-arena.log (1 run), runs/s7-wave0-NOTES.md (rule, inputs, IMAGE_ID, RUSTC, CAP_MIB)"
  - "tests/no_take_out.rs: source allow-list for cell removal and the budget function name"
  - "the writer-side budget function is pay_budget (own cell only)"
affects: [20-02, 20-03, 20-07, 20-08]

estimate:
  tokens: 70000
  tasks: 3
actuals:
  tokens: 186500    # chars/4 over the added lines of the diff; the two logs are about 182400 of it, code and text about 4100
  tasks: 3
  commits: 3

plan_head_before: 5c8aac7cc8d45a56c8f31a774cba8eaf401a6b94
plan_head_after: e4bc8d3b9ac1a85ec2d9455d1f7eefc0bdf0c034
commits: 3

tech-stack:
  added: []
  patterns:
    - "Source guard test: include_str! of the source, walk back from each removal line to the nearest fn header, compare with an allow-list"
    - "Measurement logs go to a scratch file and are copied into runs/ after the run, so the tar stream reads a clean tree"

key-files:
  created:
    - crates/slicefs-cli/tests/no_take_out.rs
    - .planning/phases/20-dirty-budget-and-rebase-flusher/runs/s7-wave0.log
    - .planning/phases/20-dirty-budget-and-rebase-flusher/runs/s7-wave0-arena.log
    - .planning/phases/20-dirty-budget-and-rebase-flusher/runs/s7-wave0-NOTES.md
  modified:
    - crates/slicefs-cli/tests/stress_harness.rs
    - scripts/docker-linux-test/run.sh
    - crates/slicefs-cli/src/filesystem.rs

decisions:
  - "CAP_MIB = 320, from the plan's rule: Pm 267 + F 8 + R 40 = 315, rounded up to 320 (above the Phase 18 cap of 256; owner to note)"
  - "The arena branch applied: one MALLOC_ARENA_MAX=2 serialize-off run gave VmHWM 230 MiB; it is not a rule input"

metrics:
  duration: "about 60 min"
  completed: 2026-11-22
---

# Phase 20 Plan 01: Wave 0 baseline Summary

S7 got a serialize knob and a VmHWM field. Six Docker Linux runs on the Phase 19 code gave P_old max 260 MiB and P0 max 267 MiB. The stated rule gave CAP_MIB = 320, which is now committed before any flusher code. A source test now makes sure that only four functions remove a cell. The writer-side budget function is now `pay_budget`.

**20-01 Task 3 sha: `e4bc8d3b9ac1a85ec2d9455d1f7eefc0bdf0c034`** (short `e4bc8d3`). Plans 20-07 and 20-08 check that `runs/s7-wave0.log` and `runs/s7-wave0-NOTES.md` did not change after this commit.

## Tasks

| Task | Name | Commit | Files |
|---|---|---|---|
| 1 | S7 serialize knob and VmHWM line | f0a7dd9 | stress_harness.rs, run.sh |
| 2 | Source allow-list test; the budget function becomes `pay_budget` | 1bfc290 | tests/no_take_out.rs, src/filesystem.rs |
| 3 | Docker P_old and P0 runs; CAP_MIB by the stated rule | e4bc8d3 | runs/s7-wave0.log, runs/s7-wave0-arena.log, runs/s7-wave0-NOTES.md, stress_harness.rs |

## Task 1

- `S7_SERIALIZE` (default 1, range 0..=1) goes to `spike_ctx`. Without the variable, the run is the same as the Phase 18 baseline.
- New helper `vmhwm_mib()`: on Linux it reads `VmHWM:` from `/proc/self/status`; on other systems it gives `None`, and the line shows `vmhwm_mib=na`.
- The S7 line now has `serialize=true|false` and `vmhwm_mib=`. `peak_rss_mib=` stays.
- `run.sh` `harness` mode passes `-e S7_SERIALIZE`.
- Local check (macOS, debug, small sizes, serialize off): `serialize=false ... vmhwm_mib=na`, passed. A small Docker run of the same form gave `vmhwm_mib=56`, so the Linux path gives a number.
- The default harness run took 8.85 s and 9.13 s (warm, host load about 28).

## Task 2

- `tests/no_take_out.rs` has three tests: `only_four_functions_remove_a_cell`, `the_budget_step_is_own_file_only`, and `header_parser_finds_names` (a small check of the header parser, so the allow-list cannot pass because the parser finds no name).
- Red run on the old tree: `the_budget_step_is_own_file_only` failed with `the old cross-file budget function is back at lines [2704]`.
- After the rename: all three tests pass. `grep -c 'fn pay_budget('` gives 1, and the old name has 0 hits in filesystem.rs. There are still 4 lines with `self.cells.remove(`.
- Guard mutation (not committed): a fifth `self.cells.remove(&cell)` in `free_dirty` (written in a form that compiles). `only_four_functions_remove_a_cell` failed with `cell removal outside ["finalize", "finalize_locked", "reclaim_inode", "setattr_size_logged"]: ["free_dirty:2740"] (all sites: ["finalize:1072", "finalize_locked:1201", "reclaim_inode:1470", "free_dirty:2740", "setattr_size_logged:3262"])`. The file was then restored from a copy of the renamed version.
- `dirty_budget_tests`, `rebase_step_tests` and `write_log_replay_tests` pass.

## Task 3

Docker: image `slicefs-fusetest`, rustc 1.100.0 (0ef1bc386 2026-11-09), kernel 7.2.12-orbstack x86_64, 32 CPUs. Tar stream of commit `1bfc290` (clean tree) into a fresh target volume `slicefs-target-p20`.

| Mode | Run | vmhwm_mib | peak_rss_mib | Host load at start (1/5/15 min) |
|---|---|---|---|---|
| P_old (serialize on) | 1 / 2 / 3 | 248 / 260 / 254 | 244 / 255 / 249 | 32.63 / 31.91 / 30.52 (start of the six runs) |
| P0 (serialize off) | 1 / 2 / 3 | 267 / 247 / 263 | 267 / 241 / 257 | 18.43 / 23.28 / 26.85 (end of the six runs) |
| P0, MALLOC_ARENA_MAX=2 | 1 | 230 | 230 | 19.44 / 23.08 / 26.64 |

The OrbStack VM load before each run is in the log (1-minute value 2.9 to 4.2).

Rule (from the plan, written before the runs):
- P_old = 260, P0 = 267, Pm = 267, Spread = max(12, 20) = 20.
- B 64 + N x W 4 + O 199 + F 8 + R 40, with R = max(16, 27, 40) = 40.
- Pm + F + R = 315, rounded up to a multiple of 8: **CAP_MIB = 320**.
- Branches: no failed run (no mismatch, no errno, no panic). P0 max 267 is not more than P_old max + 64 = 324. CAP_MIB is above 256, so the arena branch applied. The arena run gave VmHWM 230 MiB, so glibc arenas of the parallel threads are a part of the overhead O.
- The plan's awk on the committed log prints `runs=6 modes=2 pm=267 spread=20 r=40 cap=320`. This is the same as the constant.

For the owner: CAP_MIB = 320 is above the Phase 18 S7 cap of 256 MiB. The rule gives this value from the measured runs. The P_old runs alone are already 248 to 260 MiB VmHWM (Phase 18 had 234 MiB getrusage on the old code). A lower cap needs a lower dirty budget or a smaller allocator overhead (for example a smaller arena count); both are owner decisions.

## Deviations from Plan

### Auto-fixed Issues

None. The plan code steps were done as written.

### Other deviations

**1. No separate RED commit for the TDD task.** The project rule says to commit only when `cargo test --workspace` passes. A commit with only the red test would fail that rule. So the test and the rename are in one commit (1bfc290). The red run is recorded above.

**2. Extra test `header_parser_finds_names`.** This is a small test of the header parser of `no_take_out.rs`. It does not change the two tests of the plan.

**3. Extra Docker probe run.** Before the measurement, one small serialize-off run (`S7_FILE_MIB=4`) went through the same tar stream into a scratch log. It confirmed that `vmhwm_mib=` is a number on Linux. It is not in `runs/` and not in the rule. It also built the release binary in the fresh volume.

**4. The run script built the image on each call.** The plan says to build once and record the ID. My scratch script ran `docker build -q` on each of its three calls (probe, measurement, arena). Docker here uses the containerd image store, so each build gave a new image ID, also with all steps from the cache. The six runs used `sha256:56b5f12a...`; the arena run and the tag now use `sha256:71e0cab4...`, which is the `IMAGE_ID` in the notes. I think the content is the same, because each build came fully from the cache and the image config date is 2026-11-13. I could not compare the two images, because the measurement image is gone. The rule inputs do not change because of this.

**5. Host load per run.** The project rule asks for `uptime` next to each measurement. The log has the host `uptime` only at the start and at the end of the six runs, and before and after the arena run. Before each run it has only the load of the OrbStack VM (`/proc/loadavg` in the container).

**6. Requirements not marked.** The state step marked LOCK-02 and LOCK-06 complete. I reverted that, because plans 20-02 to 20-08 also list them (see Open points).

## Open points

- **Image ID.** `IMAGE_ID` in the notes is the ID of the tag `slicefs-fusetest` now (`sha256:71e0cab4...`, checked after the commit). It is not the ID of the build that the six measurement runs used (`sha256:56b5f12a...`), see deviation 4. Plans 20-03, 20-07 and 20-08 must not run `docker build`: with the containerd image store, each build gives a new ID, and the ID check then fails.
- `within_cap` in the S7 line still uses the getrusage peak, not VmHWM. Plan 20-07 adds the assert in the flusher mode; it should decide which of the two values it asserts. The notes give both.
- LOCK-02 and LOCK-06 are not marked complete in REQUIREMENTS.md. Plans 20-02 to 20-08 also list them, and this plan gives only the Wave 0 parts (the source guard and the cap).
- `docs/research/multithreaded-request-handling.md` still uses the old budget function name in the Phase 18 text. That text is a record of the old code, so this plan did not change it.

## Threat model check

- T-20-01: the rule is in the plan; the awk on the committed log gives the constant; the notes have `CAP_MIB = 320` for plan 20-07.
- T-20-02: the tar stream of a clean committed tree, a fresh target volume, and the system line are in the log.
- T-20-03: the allow-list test is red on a planted fifth site.
- T-20-SC: no new crate.

## Known Stubs

None.

## Self-Check: PASSED

All four created files exist; commits f0a7dd9, 1bfc290 and e4bc8d3 are in HEAD.
