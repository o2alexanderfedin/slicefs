---
phase: 23-hashes-and-rehash
plan: 04
status: complete
subsystem: slicefs-cli (rehash command)
tags: [rehash, cli, offline, gc, dry-run, HASH-02, HASH-03, HASH-05]
requires:
  - 23-01 (Sha256, ensure_hash_flag, check_write with the flag)
  - 23-02 (write_switch_segment, remove_switch_tmps, gc_on_open_store, last record per snapshot version)
  - 23-03 (metadata::rehash::rehash_roots, Mode, RootSet, Stats)
provides:
  - slicefs_cli::rehash (run_rehash, rehash_report_with, RehashArgs, Step, PROGRESS_EVERY, msg_log_pending, msg_stale_mount_lock)
  - content::REHASH_TMP_DIR, content::remove_rehash_tmp, content::open_with_hasher; every writable open removes rehash-tmp
  - Cmd::Rehash (--to NAME, --dry-run, STORE) and its dispatch
  - lock refusal texts that name rehash
affects: [23-05 (crash tests use the Step hooks; docs must quote the new M13-M15 texts)]
tech-stack:
  added: []
  patterns: [one offline run under one held lock; GC on the already open store; dry-run as a read-only branch after the shared checks]
key-files:
  created:
    - crates/slicefs-cli/src/rehash.rs
    - crates/slicefs-cli/tests/rehash_tests.rs
    - .planning/phases/23-hashes-and-rehash/runs/red-23-04.log
    - .planning/phases/23-hashes-and-rehash/runs/gate-23-04.log
  modified:
    - crates/slicefs-cli/src/content.rs
    - crates/slicefs-cli/src/lib.rs
    - crates/slicefs-cli/src/cli.rs
    - crates/slicefs-cli/src/main.rs
    - crates/slicefs-cli/src/header.rs
    - crates/slicefs-cli/tests/store_header_tests.rs
decisions:
  - "Header check of rehash: check(Want::Read), then decide(.., Want::Write) on the parsed header, in both modes: a real run never writes a new header and the dry-run refuses what a real run refuses"
  - "The unknown-target check comes before the lock (no wait for the lock on a typo)"
  - "No switch segment when the new RootSet equals the old one; the header line is still set and the full GC still runs (R-08)"
  - "A small pub helper content::remove_rehash_tmp is shared by the writable open and the end of a run"
  - "On an engine error or a test hook stop, the open store is dropped with no close and rehash-tmp stays; the next writable open removes it"
  - "PROGRESS_EVERY = 5 s; the rewrite progress line is sent from the engine callback; step lines at each step"
metrics:
  duration: about 95 min
  completed: 2026-11-26
actuals:
  tokens: 9500
  tasks: 3
  commits: 3
plan_head_before: 1aa0c71
plan_head_after: 9542d9e
---

# Phase 23 Plan 04: slicefs rehash command Summary

`slicefs rehash --to <hash> [--dry-run] <store>` converts an offline store to another hash: checks under the held exclusive store lock, the hash flag, the rewrite into `<store>/rehash-tmp/`, a sync, one switch segment, the header hash line, a full GC on the open store, and a report with bytes rewritten and bytes freed. The dry-run reads and verifies the tree through a read-only open and writes nothing.

BASE = `1aa0c71` (`git merge-base HEAD feature/phase-23-hashes-rehash`), recorded before the first edit. Branch `exec/23-04`, not pushed.

## Commits

| Task | Commit | Subject |
|------|--------|---------|
| 1 (tracer) | e2982ef | feat(23-04): slicefs rehash converts an offline store end to end (tracer) |
| 2 | 6b1b49c | feat(23-04): slicefs rehash subcommand and its refusals |
| 3 | 9542d9e | feat(23-04): rehash report, progress lines and dry-run estimate |

## What was built

- **Run order** (`rehash_report_with`):
  1. `require_store`, then the target from `hasher_by_name` (else the `parse_hash` text).
  2. `lock_offline`, held to the end, GC included.
  3. `check(Want::Read)`, then `decide(.., Want::Write)`.
  4. A `mount.lock` refusal, then `scan_store_with_log`. A log that is not empty is refused, and so is no root.
  5. The dry-run branch.
  6. `ensure_hash_flag`.
  7. `remove_switch_tmps`, `open_with_hasher`, `rehash_roots` with `Mode::Rewrite { rehash-tmp }`, then `cs.sync()`.
  8. `write_switch_segment`, only when a root changed.
  9. `check_write(target)` for the header hash line.
  10. `gc_on_open_store(&cs, new roots, true)`.
  11. `cs.close()`, remove `rehash-tmp`, then the report.
- **Hooks.** `stop_after: Option<Step>` returns right after each step, and `stop_after_values` passes through to the engine. Both are `#[doc(hidden)]`. Plan 23-05 uses them in the crash tests.
- **Report.** These lines, in order:
  - `Rehash to H: N roots (S snapshots).`
  - `Read: ...; each object read was verified.`
  - `Rewritten: ...; new objects: ... (... bytes stored).`
  - `Kept as is: ... Stale refcount rows dropped: ...`
  - `Every live object already has hash H; no root changed.` (only when no root changed)
  - `GC (full): <stats_line>`
  - `Bytes freed: <removed_bytes>.`
  - the `store disk usage` line
- **Dry-run report.** Three lines: the dry-run header, `Read and verified: ...`, and `Objects of another hash: N, about B bytes. This is an estimate ...`.
- **Progress lines.**
  - Step lines: `rehash: checks passed`, `flag written` (only when the flag is written), `synced`, `switch` or `no switch needed`, `header`, `gc`.
  - The line `rehash: rewrite: root i of n: ...` comes from the engine callback at `progress_every`.
- **CLI.**
  - `Cmd::Rehash { to, dry_run, store }`. `--to` goes through `parse_hash`.
  - The `main.rs` arm prints `slicefs rehash error: ...` (or JSON with `--json`) and exits 1.
- **Lock texts.** The three store-lock messages now read "gc, seed, rehash, snapshot create or snapshot switch".

## Tests

- `rehash_tests.rs` has 14 tests:
  - the tracer `a_blake3_store_rehashes_to_sha256`;
  - `a_writable_open_removes_a_stale_rehash_tmp`;
  - six `refuse_*` tests (lock exclusive, lock shared, `mount.lock`, write log, unknown target, no store). Each checks that the store tree is unchanged;
  - `dry_run_changes_nothing_and_reports_an_estimate` and `dry_run_refuses_like_a_real_run`;
  - `report_has_the_counts_and_bytes_freed`;
  - `progress_lines_come_at_each_step`;
  - `a_second_run_finds_nothing_to_rewrite`;
  - `a_store_with_only_target_objects_gets_the_header_line`.
- `cli::tests::rehash_parses_to_and_dry_run` and `rehash_refuses_an_unknown_hash`.
- `store_header_tests` has rehash rows in the write-command table and in the no-store table.
- The tracer checks these results:
  - the header is `hash sha256` plus the flag;
  - the snapshot has the same version, name and `created_at`, and a new root;
  - every object of every root has tag 0x12;
  - the dumps of the current tree and the snapshot tree are equal to the dumps before;
  - scrub passes;
  - no `rehash-tmp` is left;
  - `Bytes freed` is above 0;
  - the dry-run before the real run changes no file.

## TDD record (runs/red-23-04.log)

- **Task 1.** The tests did not compile (no `rehash` module, no `REHASH_TMP_DIR`).
- **Task 2.**
  - The CLI parse tests did not compile (no `Cmd::Rehash`).
  - The six refusal tests are guards, because the refusals came with the Task 1 run order. One run with five guard mutations (no `require_store`, a fallback to the default hash for an unknown target, no lock, no `mount.lock` check, no log check) turns all six red.
- **Task 3.**
  - Five of the six tests were red on the Task 1 report.
  - `dry_run_refuses_like_a_real_run` is a guard. Skipping the `mount.lock` and log checks for the dry-run turns it red.

## Gates

See `runs/gate-23-04.log`.
- **Local gate** (4 standard skips), exit 0 at every commit:
  - Task 1: 1645 passed;
  - Task 2: 1653 passed;
  - Task 3: 1659 passed, 0 failed, 13 ignored, 81 result lines.
- **`RUST_STABLE=stable scripts/safety/clippy.sh`** at every commit: `CLIPPY ... result=ok` and `UNSAFE files=31 tokens=132 result=ok`.
- **Negative grep.** `rehash.rs` has 0 code lines with `gc_report`, `GarbageCollector`, `run_gc_mode` or `store_io(`.

## Deviations from Plan

1. **[Rule 3 - Blocking] Branch name and start state.** The plan names `feature/phase-23-hashes-and-rehash`; the real branch is `feature/phase-23-hashes-rehash`. As the orchestrator said, `exec/23-04` was made with `git checkout -B exec/23-04 feature/phase-23-hashes-rehash` at `1aa0c71` before any edit.
2. **[Rule 1 - Bug, test helper] `open_fs` lost the snapshot.**
   - The first form of the copied helper did not call `set_snapshots`. Its commit then wrote a root segment with no snapshot record.
   - The helper now calls `set_snapshots`, as a mount does. This is test code only.
3. **[Rule 1 - Bug, test helper] The write-log setup made no record.**
   - `simulate_mkdir` writes no log record. The test made sure of its own setup (it asserts that `scan_store_with_log` gives a non-empty log), so the run failed there.
   - The setup now uses the logged request `handlers::handle_mkdir`.
4. **Assertion changed in `a_store_with_only_target_objects_gets_the_header_line`.**
   - The plan says such a run "rewrites no value". The engine of 23-03 always translates the key-carrying nodes and counts them in `values_rewritten`: 8 on that store. The translation gives the same keys.
   - The test therefore asserts `new objects: 0 (0 bytes stored)`, the `Every live object already has hash sha256` line, no new segment file and the `hash sha256` header line. It does not assert `Rewritten: 0 values`.
5. **Small addition.** `content::remove_rehash_tmp(store)` (pub) is shared by the writable open and the end of a run. It is reversible.
6. **Process.** Clippy ran before each of the three commits, not only in Task 3, as the orchestrator asked.

STATE.md, ROADMAP.md and REQUIREMENTS.md were not touched (orchestrator rule).

## Known Stubs

None.

## Threat Flags

None. The run adds no network, auth or format surface. `rehash-tmp` is a per-run temp directory made under the exclusive lock (T-23-14).

## For the owner

- **The `Rewritten:` count.** It counts translated key-carrying nodes also when they give the same key. On a store that has only target objects, the report says `Rewritten: 8 values; new objects: 0`. The engine could count only values with a new key, but that is a change to 23-03 code, which this plan did not do. Reversible; it is your choice.
- **Docs.** `docs/design/store-format.md` M13-M15 still quote the old lock texts, which have no `rehash`. Plan 23-05 owns that file and updates it. Until then the docs and `header.rs` differ by one word.
- **Unchanged from wave 1.**
  - A refcount row whose object is missing is dropped and counted. The report shows the count.
  - `HASH_SHA256_RELEASE` is still the placeholder `v2.2`.

## Self-Check: PASSED

- Files: `crates/slicefs-cli/src/rehash.rs`, `crates/slicefs-cli/tests/rehash_tests.rs`, `runs/red-23-04.log`, `runs/gate-23-04.log` exist.
- Commits e2982ef, 6b1b49c and 9542d9e are ancestors of HEAD on `exec/23-04`. `git rev-list --count 1aa0c71..9542d9e` is 3.
