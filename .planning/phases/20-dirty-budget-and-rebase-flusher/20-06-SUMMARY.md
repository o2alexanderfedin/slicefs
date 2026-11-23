---
phase: 20-dirty-budget-and-rebase-flusher
plan: 06
subsystem: cli
status: complete
tags: [lock-06, mount-options, dirty-budget, flusher]

requires:
  - phase: 20-dirty-budget-and-rebase-flusher
    plan: 05
    provides: "the idle and maximum-age atomics of FlushCtl, read by flush_pass; test_advance_clock"
provides:
  - "slicefs mount --dirty-budget SIZE (default 64M), --dirty-limit SIZE (default 32M), --dirty-idle SECS (default 5), --dirty-expire SECS (default 30)"
  - "content::parse_dirty_bytes: bytes or a whole number with K, M or G (also KiB, MiB, GiB; any case; 1024-based), overflow checked, at least 4M"
  - "filesystem::MIN_DIRTY_BYTES (4 x REBASE_STEP)"
  - "filesystem::DirtyConfig (Default = the constants) with check(); SliceFsFilesystem::set_dirty_config; test hook test_dirty_config"
  - "run_mount(.., dirty: DirtyConfig): the check is its first statement; set_dirty_config runs before the write-log replay"
affects: [20-07, 20-08]

estimate:
  tokens: 80000
  tasks: 2
actuals:
  tokens: 3770      # chars/4 over the added lines of the diff b53999f..2493fea
  tasks: 2
  commits: 2

plan_head_before: b53999f97303087e475a22b667ca6e2f88d15acf
plan_head_after: 2493fea2586ade59798bc9d4a2169b1a0822e638
commits: 2

tech-stack:
  added: []
  patterns:
    - "A cross-field check of mount options runs in run_mount before the first side effect"
    - "A cli test ties each option default to its constant, so the two cannot drift apart"

key-files:
  created: []
  modified:
    - crates/slicefs-cli/src/cli.rs
    - crates/slicefs-cli/src/content.rs
    - crates/slicefs-cli/src/main.rs
    - crates/slicefs-cli/src/mount.rs
    - crates/slicefs-cli/src/filesystem.rs
    - crates/slicefs-cli/tests/flusher_tests.rs

decisions:
  - "MIN_DIRTY_BYTES is a public constant in filesystem.rs (next to REBASE_STEP); parse_dirty_bytes and DirtyConfig::check both use it"
  - "DirtyConfig::check also refuses a time above u64::MAX / 1000 s, the same range as the clap parser"
  - "set_dirty_config does not call check; it is a plain store (seconds to ms with saturating_mul)"

metrics:
  duration: "about 36 min (09:20 to 09:56 UTC; two workspace runs of 846 s and 870 s)"
  completed: 2026-11-23
---

# Phase 20 Plan 06: Dirty-data mount options Summary

`slicefs mount` has four new options: `--dirty-budget` and `--dirty-limit` (bytes, with K, M or G, at least 4M), and `--dirty-idle` and `--dirty-expire` (seconds, at least 1). A limit above the budget stops the mount before the mount point is made. The values reach the file system before the write-log replay. The throttle and the flusher read them. With no new option, the values are the old constants.

## Tasks

| Task | Name | Commit | Files |
|---|---|---|---|
| 1 | `--dirty-budget` and `--dirty-limit` end to end, with the minimum and the order check | 3f285d8 | cli.rs, content.rs, main.rs, mount.rs, filesystem.rs |
| 2 | `--dirty-idle` and `--dirty-expire` to the flusher | 2493fea | cli.rs, main.rs, filesystem.rs, tests/flusher_tests.rs |

## What was built

- **`parse_dirty_bytes`** (content.rs). The number part must be ASCII digits only, so a sign (`+4M`, `-4M`) and a fraction (`1.5M`) are refused. The suffix, in lower case, must be one of: none, `k`, `kib`, `m`, `mib`, `g`, `gib`. So `64MB` and `64X` are refused. The scale uses `checked_mul`. A value below 4 MiB after scaling is refused with a message that names `4M` and `4194304`. This also refuses 0.
- **Options** (cli.rs). `dirty_budget` and `dirty_limit` (`value_name = "SIZE"`, defaults `"64M"` and `"32M"`, parser `parse_dirty_bytes`). `dirty_idle` and `dirty_expire` (`value_name = "SECS"`, defaults 5 and 30, `value_parser!(u64).range(1..=u64::MAX / 1000)`). The help texts give the unit, the default and the minimum. The `--dirty-budget` text says that a writer over it waits up to 5 s for the flusher, then stores part of its own file. The `--dirty-expire` text says that a file with no write pause of 300 ms or more is kept under its limit by its own writer (the hot rule of 20-05).
- **`DirtyConfig`** (filesystem.rs). Four public fields; `Default` gives `DIRTY_BUDGET`, `DIRTY_LIMIT`, 5 s and 30 s. `check()` refuses a byte value below `MIN_DIRTY_BYTES`, a limit above the budget (the message has both values in bytes), and a time of 0 s or above `u64::MAX / 1000` s.
- **`set_dirty_config`** stores the budget and the limit (Task 1) and the idle and expire times in ms (Task 2). The `test_set_*` hooks stay. `test_dirty_config()` reads the four values back.
- **Wiring.** `main.rs` builds the `DirtyConfig` from the four options. `run_mount` takes it as a new last argument. Its first statement is the check, before the mount point directory is made. `fs.set_dirty_config(&dirty)` runs before the write-log replay. `run_mount` has no other caller.

## Red and green runs

Each new test failed before its code, at an assert or at the parse. No red tree was committed (project rule). All runs used `timeout`.

Task 1:

| Run | Tree | Test | Failed at |
|---|---|---|---|
| A | b53999f (old tree) plus a parse-only test | `cli::tests::dirty_size_options_parse` | cli.rs:270, `error: unexpected argument '--dirty-budget' found` |
| B | stubs: `parse_dirty_bytes` takes plain bytes only, `check` takes all, `set_dirty_config` stores nothing | `content::tests::dirty_bytes_parse_with_binary_suffixes` | content.rs:346, `left: Err("invalid digit found in string") right: Ok(67108864)` |
| B | same | `content::tests::dirty_bytes_refuse_bad_values` | content.rs:361, `3M: the message names the minimum: invalid digit found in string` |
| B | same | `cli::tests::dirty_size_defaults_are_the_constants` | cli.rs:290, `invalid value '64M' for '--dirty-budget <SIZE>'` |
| B | same | `cli::tests::dirty_size_options_parse` | cli.rs:298, `invalid value '8M' for '--dirty-budget <SIZE>'` |
| B | same | `mount::tests::dirty_config_check_refuses_a_limit_above_the_budget` | mount.rs:546, `a limit above the budget is refused: ()` |
| B | same | `mount::tests::run_mount_refuses_a_dirty_limit_above_the_budget_first` | mount.rs:568, `the mount point was not made (store not found at ...: no segments/ directory)`. The mount point was made; the missing store stopped the call before any mount. |
| B | same | `filesystem::tests::set_dirty_config_stores_the_budget_and_limit` | filesystem.rs:6530, `the dirty budget and limit are set`, left `(67108864, 33554432)` |

Task 2, on the Task 1 tree (3f285d8):

| Test | Failed at |
|---|---|
| `cli::tests::dirty_time_options_parse` (parse-only form) | cli.rs:308, `error: unexpected argument '--dirty-idle' found` |
| `a_configured_idle_time_is_used` | flusher_tests.rs:942, `F's cost is 0 at 8 s (idle time 7 s)`, left 525312, `PassStats` all 0 (`set_dirty_config` did not store the times) |

Green:
- Task 1 verify: `cargo test -p slicefs-cli --lib -- cli:: mount:: content:: filesystem:: --nocapture`: 188 passed, 0 failed. All seven new test lines say `ok`, and each has `dirty` in its name. `slicefs mount --help` shows `--dirty-budget` and `--dirty-limit`.
- `cargo test --workspace` exited 0 before the 3f285d8 commit (69 `test result: ok` lines, 0 failed, 846 s; `stress_harness` 10.06 s).
- Tracer gate: both Task 1 verify commands ran again on the committed tree: 188 passed; 3 help lines match.
- Task 2 verify: `cargo test -p slicefs-cli --lib -- cli::`: 21 passed. `cargo test -p slicefs-cli --test flusher_tests a_configured_idle_time_is_used`: 1 passed. `slicefs mount --help`: 5 lines match `--dirty-(budget|limit|idle|expire)` (the fifth is the `--dirty-limit` help text, which names `--dirty-budget`).
- `cargo test --workspace` exited 0 before the 2493fea commit (69 `test result: ok` lines, 0 failed, 870 s; `stress_harness` 9.45 s; `flusher_tests` 21 passed).

## Acceptance checks

- `grep -c 'pub fn parse_dirty_bytes' crates/slicefs-cli/src/content.rs` = 1
- `grep -c 'dirty_budget' crates/slicefs-cli/src/cli.rs` >= 1
- `grep -c 'pub struct DirtyConfig' crates/slicefs-cli/src/filesystem.rs` = 1
- `grep -c 'dirty.check()' crates/slicefs-cli/src/mount.rs` = 1
- `grep -c 'dirty_expire' crates/slicefs-cli/src/cli.rs` = 2
- `grep -ciE 'licens|/Users/|/home/' crates/slicefs-cli/src/cli.rs` = 0
- `stress_harness.rs` did not change.

## Deviations from Plan

### Auto-fixed Issues

None.

### Other deviations

**1. No separate RED commit.** The project rule is to commit only when `cargo test --workspace` passes. Each task has one commit with its tests and its code. The red runs are recorded above.

**2. The cli red runs used a parse-only form of the test.** A test that reads the new fields does not compile on a tree without them. So each cli red run used a test that only parses the arguments; it failed at the unknown argument, as the plan says. The final tests then read the values.

**3. Task 1 `set_dirty_config` stored only the budget and the limit.** The Task 1 text says "stores the four atomics", but the Task 2 red run needs a `set_dirty_config` that ignores the times. The times were added in Task 2.

**4. `a_configured_idle_time_is_used` sets idle and expire to 1 h before the config.** With the default idle time of 5 s, the red run would fail at the 6 s pass, not at "cost is 0". The 1 h values make the red run fail where the plan says, and show that the config replaces values that are already set.

**5. More refused values in the tests than the plan lists.** `+4M`, `-4M`, ` 4M`, `M`, a digit count that overflows `usize` before the scale, `--dirty-budget=-5`, `--dirty-limit 0` and `--dirty-limit 3M`. In `DirtyConfig::check`: 0 bytes, a limit 1 byte below the minimum, 0 s for each time, and an expire time above `u64::MAX / 1000`.

## Open points

- **Requirements.** LOCK-06 is in the plan frontmatter, but it is not marked complete (orchestrator rule). REQUIREMENTS.md did not change.
- **Upper bound of the byte options.** There is no upper bound: `--dirty-budget 1000G` is accepted. The hard cap uses `saturating_add`, so a very large limit does not overflow. The plan asks for no upper bound.
- **`stress_harness` time.** 10.06 s and 9.45 s in the two workspace runs (load average about 15). This plan did not change the write path; the item stays in `deferred-items.md`.
- **Deferred items** (commit timer at unmount, empty passes, harness time) are not in the scope of this plan and did not change.

## Threat model check

- T-20-SC: no new crate. The size parser is hand-written.
- T-20-18: `parse_dirty_bytes` scales with `checked_mul`; `99999999999G` and a 23-digit number are refused (tests). The seconds range `1..=u64::MAX / 1000` keeps the ms count in u64; `u64::MAX / 1000 + 1` is refused (test), and `set_dirty_config` uses `saturating_mul`.
- T-20-19: the 4 MiB minimum in the parser and in `DirtyConfig::check`; the check runs before the mount point is made (`run_mount_refuses_a_dirty_limit_above_the_budget_first`).
- T-20-20: accepted. The accepted forms (bytes; K, M, G and KiB, MiB, GiB, any case, 1024-based) are tested here.

## Known Stubs

None.

## Self-Check: PASSED

- FOUND: crates/slicefs-cli/src/content.rs (`pub fn parse_dirty_bytes`), crates/slicefs-cli/src/filesystem.rs (`pub struct DirtyConfig`, `pub fn set_dirty_config`, `pub const MIN_DIRTY_BYTES`), crates/slicefs-cli/src/mount.rs (the check in `run_mount`), crates/slicefs-cli/tests/flusher_tests.rs (`a_configured_idle_time_is_used`)
- FOUND: 3f285d8 and 2493fea in HEAD
- `commits: 2` is measured with `git rev-list --count` from `plan_head_before`. The docs commit (this file, STATE.md, ROADMAP.md) comes after it.
