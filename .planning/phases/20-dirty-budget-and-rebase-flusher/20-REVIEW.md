---
phase: 20-dirty-budget-and-rebase-flusher
reviewed: 2026-11-23T12:50:14Z
depth: deep
files_reviewed: 14
files_reviewed_list:
  - crates/slicefs-cli/src/cli.rs
  - crates/slicefs-cli/src/commit_timer.rs
  - crates/slicefs-cli/src/content.rs
  - crates/slicefs-cli/src/filesystem.rs
  - crates/slicefs-cli/src/flusher.rs
  - crates/slicefs-cli/src/fuse_callbacks.rs
  - crates/slicefs-cli/src/handlers.rs
  - crates/slicefs-cli/src/lib.rs
  - crates/slicefs-cli/src/main.rs
  - crates/slicefs-cli/src/mount.rs
  - crates/slicefs-cli/tests/flusher_tests.rs
  - crates/slicefs-cli/tests/no_take_out.rs
  - crates/slicefs-cli/tests/stress_harness.rs
  - scripts/docker-linux-test/run.sh
findings:
  critical: 0
  warning: 4
  info: 6
  total: 10
  necessity: 1
status: issues_found
fix_status: WR-01..WR-04, IN-01, IN-04..IN-06 fixed; IN-02, IN-03 accepted
---

# Phase 20: Code Review Report

**Reviewed:** 2026-11-23T12:50:14Z
**Depth:** deep
**Files Reviewed:** 14
**Status:** issues_found

## Summary

Diff base `9091b7f` (Phase 19 merge) to `HEAD` on `feature/phase-20-dirty-budget-flusher`. The review followed the call chains from the FUSE `write` and `destroy` callbacks, from `run_mount`, and from the flusher thread into `flush_step`, `free_dirty`, `rebase_cell`, `step_io`, `step_publish`, `reclaim_inode`, `commit_cut_begin`, `test_destroy` and the metadata store commit (`begin_commit`, `finish_commit`).

Reachability: `build_mount_options` (`mount.rs`) does not set `n_threads`, and the vendored fuser uses `config.n_threads.unwrap_or(1)` (`vendor/fuser/src/session.rs:257`). So a mount has one FUSE dispatch thread. Two FUSE requests never run at the same time in a mount. The flusher, the commit timer and the online GC are background threads. A throttle wait stops the one dispatch thread, so it stops every FUSE request for that time.

These parts hold up under the trace:
- Lock order. `throttle_write` runs before `gate_shared()` in `fuse_callbacks.rs:437`, and a debug assert checks that no ranked lock is held. The flusher takes the gate shared for one `flush_step` only, then the cell. It skips a cell with a step token, a writer at the hard cap, a retired cell or a transient cell, so it never waits for a token with the gate held. It holds no lock across its sleep. `shutdown()` stops and joins the flusher before it takes the gate. The flags lock (`Rank::Short`) is never held across another lock.
- Wake-ups. The kick sets `kicked` under the flags lock, and the flusher tests `kicked` and `stop` under the same lock before it waits. The throttled writer tests the total, `stop` and `running` under the flags lock, and `notify_freed` takes that lock before `notify_all`. The `Running` drop guard clears `running` and wakes the writers also on a panic. No lost wake-up was found. The two `empty_pass` fixes (`4d8ac3d`, `e44d57d`) are correct for the case that they fix: below the budget, only time or the full budget can make a new candidate, and the timed pass (100 ms) covers time.
- Time limits. The throttle wait has a deadline and leaves at `stop` and when the flusher ends. The flusher sleep has the pass interval and wakes at `stop`. The join in `stop_flusher` waits for at most one `flush_step`, which has its own timed waits.
- Dirty counts. `dirty_total` and `FileCell::dirty` change only in `sync_dirty`, under the cell lock, in every path (write, flusher step, release, truncate, unlink, error). A flusher step on a kept cell (no write handle) that races with an unlink is safe: `reclaim_inode` sets `retired` under the cell lock, `rebase_unlocked` then returns before `step_publish`, and Phase B changes no refcount.
- `--dirty-expire` and hot files: the idle and expire loop skips a hot cell (`flusher.rs:184`). This differs from the plain user rule 2 (maximum age), but decision A10 accepts it and the `--dirty-expire` help text tells the user. It is not a finding.
- Option parsing: K, M, G (and KiB, MiB, GiB, any case), no sign, no fraction, overflow check, 4 MiB minimum, `--dirty-limit` not above `--dirty-budget` (checked before any side effect), times 1 to `u64::MAX / 1000` s. One gap: `--dirty-budget` below 32M alone is refused (WR-04).

The main problems are on the error path, in one mount option and in one test. When the flusher cannot free memory (a store error), each write over the budget waits the full 5 s and stops all FUSE requests. A failed flusher step has no back-off, and it keeps the kick on. One test counts stop exits and can fail under load.

Deferred item (commit timer during unmount): not a real race. See IN-01 for the evidence.

No recorded decision in the 20-01 to 20-08 summaries, `20-FIX-empty-pass.md` or `deferred-items.md` covers WR-01 to WR-04.

## Fix status

| Finding | Status |
|---|---|
| WR-01 | fixed in 79f84e5 |
| WR-02 | fixed in c0d10d0 |
| WR-03 | fixed in c0d10d0 |
| WR-04 | fixed in 7ff8c3c |
| IN-01 | fixed in b912aed (timer-loop test in db225bb) |
| IN-02 | accepted, no change |
| IN-03 | accepted, no change |
| IN-04 | fixed in 7ff8c3c |
| IN-05 | fixed in b912aed (doc corrected, check kept) |
| IN-06 | fixed in b912aed |

Each fix commit passed `cargo test --workspace`, `cargo check -p slicefs-cli --features deadlock-detect` and `cargo test -p slicefs-cli --features deadlock-detect --test flusher_tests` in the main checkout before it was made.

## Narrative Findings (AI reviewer)

## Warnings

### WR-01: A writer waits the full 5 s when the flusher cannot free memory, and the wait stops every FUSE request

**Status:** Fixed in 79f84e5. A pass that began in the wait and could not free memory (no step published, and a step failed or the last pick found no candidate at the budget) ends the wait; the writer pays with its own step. The 5 s limit stays the upper bound. New counter `no_room_exits`. Test `a_throttled_writer_does_not_wait_when_the_flusher_cannot_free`: red 10.02 s (timeouts 1), green 125 ms.
**File:** `crates/slicefs-cli/src/filesystem.rs:3219-3257` (`throttle_write`), `crates/slicefs-cli/src/fuse_callbacks.rs:437`
**Issue:** The throttle wait ends only when the total is below the budget, at the deadline (`THROTTLE_WAIT`, 5 s), or at a stop. It does not end when the flusher has no step that can free memory. Example: a store error (disk full, a bad pack). Each flusher step then fails (`FlushStep::Failed`), the total stays at or above the budget, and each write waits the full 5 s. After the wait, `pay_budget` fails too, and the write returns `Ok`. A mount has one FUSE dispatch thread (see Summary), so during each wait no read, release, fsync or getattr runs. The mount is then almost stopped: 5 s for each write, and the same stop for all other requests. Before Phase 20, `pay_budget` failed at once and the write returned at once. The memory bound does not need this wait, because the flusher cannot lower the total in this state.
**Fix:** End the wait when a flusher pass that started after the wait began ended with no freed step. For example, add a pass sequence number and a "last pass freed nothing" flag to `FlushCtl`. Set both at the end of `flush_pass`, under the flags lock, and call `notify_freed()`:
```rust
// in throttle_write, in the wait loop, under the flags lock:
if flags.pass_seq > seq_at_start && flags.last_pass_freed == 0 {
    ctl.no_room_exits.fetch_add(1, Ordering::Relaxed);
    break true; // pay with own steps now; waiting cannot help
}
```
Add a test: the flusher runs, every step fails (`test_fail_steps`), and a write over the budget returns in much less than the wait limit.

### WR-02: A failed flusher step has no back-off, and `empty_pass` counts tried steps, not freed steps

**Status:** Fixed in c0d10d0. Back-off per cell after a failed step (100 ms, doubled for each failure in a row, at most 30 s; a freed step ends it), in both loops of `flush_pass`. The log line gives the back-off, so it comes at most once per cell in each back-off. `empty_pass` is now `published == 0 && last_full`. Tests `a_failed_step_backs_off` and `a_write_does_not_wait_for_a_cell_in_its_back_off`.
**File:** `crates/slicefs-cli/src/flusher.rs:194`, `:184-191`, `:220-228`
**Issue:** The `tried` set lives for one pass only. A cell whose step fails is tried again in each pass. A quiet cell is also a candidate of the idle loop after 5 s. Thus the flusher tries it every `FLUSH_PASS` (100 ms), with one full step of I/O and one `eprintln!` line each time, with no end. The cell that a failed last release keeps (deferred item "Finalize retry of a kept cell") is such a cell when the cause of the failure stays. Also, `empty_pass` is `st.steps == 0 && last_full`. A pass that tried only a failed or skipped step has `steps > 0`, so `empty_pass` stays false, and each write at or above half the budget kicks one more pass. This is the empty-pass problem of `20-FIX-empty-pass.md` again, but now each extra pass also does a failed step and writes a log line. At a high write rate this gives a log line and a step try for each write.
**Fix:** Store `empty_pass` from freed steps, not from tried steps: `ctl.empty_pass.store(st.published == 0 && last_full, ...)`. The reason of the fix note still holds: a cell that was skipped or dropped was busy (it is hot now), and a failed cell gets its next try from the timed pass. Also add a back-off per cell after a failure. For example, `FileCell::flush_retry_at: AtomicU64` (flusher clock), set to `now + min(2^n x 100 ms, 30 s)` after each failed step, and filter it out in both loops of `flush_pass`. Print the error line only when the back-off grows.

### WR-03: `stop_wakes_throttled_writers` can count 3 stop exits instead of 4

**Status:** Fixed in c0d10d0. The test asserts 4 waits, no timeout, no no-room exit, and 1 to 4 stop exits.
**File:** `crates/slicefs-cli/tests/flusher_tests.rs:522`; `crates/slicefs-cli/src/filesystem.rs:3245-3248`
**Issue:** The wait loop tests `total < budget` before it tests `stop`. After `stop_flusher`, the first writer that wakes leaves with `pay: true`. It writes 32 KiB and then pays with `pay_budget`. Its own file has about 288 KiB dirty, and its pay can free up to 1 MiB, so it makes its file clean. The total then falls about 256 KiB below the budget, which the test set to the total. A writer that runs only after that sees `total < budget` and leaves with `pay: false`. That exit does not count in `stop_exits`. A woken thread must wait for the CPU, and the deferred items record host loads of 19 to 75. So `stop_exits == 3` can occur, and the test then fails. The product code is correct. Only the count is not fixed.
**Fix:** Assert what the test is for: every writer left the wait before the limit, and none waited for the limit.
```rust
assert_eq!(st.timeouts, 0, "no throttle wait reached the limit: {st:?}");
assert!(st.stop_exits >= 1, "at least the first wait left at the stop: {st:?}");
```
Another way: test `flags.stop` before `total < budget` in the loop. Then all four writers leave as stop exits. In that case, a writer at the stop pays also when there is room. That is acceptable, because a stop comes only at unmount.

### WR-04: `--dirty-budget` below 32M with no `--dirty-limit` stops the mount

**Status:** Fixed in 7ff8c3c. With no `--dirty-limit`, the limit is 32M, or the budget if that is less (`Cmd::dirty_config` in the lib). An explicit limit above the budget is still refused. Help text and README updated. Test `a_small_budget_alone_gives_a_limit_of_the_budget`: red limit 33554432.
**File:** `crates/slicefs-cli/src/cli.rs:97-110`, `crates/slicefs-cli/src/filesystem.rs:216-221` (`DirtyConfig::check`), `crates/slicefs-cli/src/mount.rs:269`
**Issue:** `--dirty-limit` has the fixed default `"32M"`, and `check()` refuses a limit above the budget. So `slicefs mount --dirty-budget 16M ...` (with no `--dirty-limit`) fails with "--dirty-limit 33554432 is above --dirty-budget 16777216". The `--dirty-budget` help text says "Default 64M, at least 4M". It does not tell the user that each value from 4M to below 32M also needs `--dirty-limit`. The mount options are public (CONTEXT: "costly" to change), so the default rule must be right before a release.
**Fix:** Make the limit default follow the budget when the user does not give it. Use `Option<usize>` for `dirty_limit` (no `default_value`), and in `main.rs`:
```rust
let limit = dirty_limit.unwrap_or(DIRTY_LIMIT.min(dirty_budget));
```
Then write in the help text "Default 32M, or --dirty-budget if that is less". Add a cli test: `--dirty-budget 16M` alone parses and gives a limit of 16M. If the refusal is wanted, write in the `--dirty-budget` help text that a value below 32M also needs `--dirty-limit`.

## Info

### IN-01: Deferred item: the commit timer during unmount is not a real race

**Status:** Fixed in b912aed. `shutdown()` marks the file system as shut down first; the timer calls `commit_unless_shut_down`, which checks the mark before the sync and again with the exclusive gate held, and then does no commit. `timed_commit` keeps its behavior. Test `no_timed_commit_after_shutdown`: red, the timer call made a full commit after `shutdown()`. Timer-loop test `the_timer_does_not_commit_after_shutdown` in db225bb: red with the old call in the loop (the timer did not end within 10 s).
**File:** `crates/slicefs-cli/src/mount.rs:483-496`, `crates/slicefs-cli/src/commit_timer.rs:85-92`, `crates/slicefs-cli/src/filesystem.rs:5042-5070`, `crates/metadata/src/store.rs:1049-1050`, `:1099`
**Issue:** The commit timer stops only after `mount()` returns (`mount.rs:494`). Two cases were traced.
1. A timed commit is in `commit_cut_finish` (no gate) when `destroy` starts. `begin_commit` takes `commit_lock`, and the `PreparedCommit` keeps it until `finish_commit` ends (`store.rs:1050`, `:1099`). The `meta.commit()` of `destroy` waits for it. So the two commits do not interleave, and the root of `destroy` comes last.
2. The timer tested `has_uncommitted_changes()` and then waits for the exclusive gate while `shutdown()` holds it shared. When `destroy` ends, the timer cuts and commits again, after `shutdown_wal()` and `content.close()`. It does not test for changes again. The state is the same, so the root record is the same. `shutdown_wal()` only flushes and syncs (`per_op.rs:285`, `flush_on_fsync.rs:53`; `periodic.rs:102` also stops its timer), so `cut` and `install_root` still work. The chunk store has no closed flag (`chunk-store/src/lib.rs:3128`). If this commit puts a new node, the node goes into a new pack that is not sealed. The next open then scans that pack (the comment at `filesystem.rs:5063` says that a sealed pack avoids a scan).

Verdict:
- Data loss: no. The root of `destroy` holds all data, and a later root has the same state.
- Double commit: yes, one extra commit can occur, but `commit_lock` serializes it, and it gives the same root.
- Panic: no. No assert in the commit path tests the unmount state.
- Hang: no. The gate wait of the timer ends when `shutdown()` returns. On a mount that succeeds, the timer is joined before the process exits (`mount.rs:496`). On a mount that fails, `mounted?` (`mount.rs:491`) returns before the join. This is older than Phase 20, and no `destroy` runs in that case.

**Fix:** None is necessary. To remove the extra commit and the possible unsealed pack, test `has_uncommitted_changes()` again in `timed_commit` after the exclusive gate is held, or stop and join the commit timer in `shutdown()` before `stop_flusher()`.

### IN-02: No test can tell `pay: false` from `pay: true` on the admitted path

**Status:** Accepted, no change (low value).
**File:** `crates/slicefs-cli/tests/flusher_tests.rs:218-251` (assert at `:247`)
**Issue:** In `a_writer_over_the_budget_waits_for_the_flusher`, the total is already below the budget when the write runs. So `pay_budget` does nothing also with `pay: true`, and `rebase == 0` holds in both cases. The 20-02 summary also records this. If `write_in_cell` ignores `admit.pay` and always pays, no test fails.
**Fix:** Add a test hook that keeps the total above the budget after the admission (for example, lower the budget in the gate hold before the handler, as `throttled_write_sampled` permits). Then assert `rebase == 0` for `Admit { pay: false }` and `rebase > 0` for `Admit { pay: true }`.

### IN-03: The take-out check finds only the literal text `self.cells.remove(`

**Status:** Accepted, no change (low value).
**File:** `crates/slicefs-cli/tests/no_take_out.rs:58`
**Issue:** `removal_sites` matches only the text `self.cells.remove(`. A removal through another binding, for example `let cells = &self.cells; cells.remove(&c)`, or a new `CellMap` method that removes, passes the check. The debug rank rule catches a second cell lock, but not a removal with one cell lock.
**Fix:** Match `.remove(` on any receiver of type `CellMap`: make `CellMap::remove` the only removal API (it is now), and check that its callers are in the allow-list, for example with a match on `cells.remove(` and on `CellMap` methods that call `HashMap::remove`.

### IN-04: The `DIRTY_BUDGET` doc is stale, and two messages have a fixed "4M"

**Status:** Fixed in 7ff8c3c. New `DIRTY_BUDGET` doc; the `--dirty-budget` help text and the README say that below 64M a stream with out-of-order writes can become an overlay at the budget; both messages take the value from `MIN_DIRTY_BYTES`.
**File:** `crates/slicefs-cli/src/filesystem.rs:134-140`, `:213`; `crates/slicefs-cli/src/content.rs:240`
**Issue:** The doc of `DIRTY_BUDGET` says "when a write makes the sum pass it, the file with the most dirty bytes stores a step". Phase 20 changed this: the writer waits in the throttle, the flusher steps other files, and a writer steps only its own file. The doc also says "The same value as `REORDER_WINDOW`, so one stream can still use its whole window". With `--dirty-budget` below 64 MiB this is not true: at the budget the flusher can change a stream with pending writes into an overlay (one way). The `--dirty-budget` help text does not say this; only the `--dirty-limit` help text names the window. The two error messages write "4M" as text, but `MIN_DIRTY_BYTES` is `4 * REBASE_STEP`, so a change of `REBASE_STEP` makes the text wrong.
**Fix:** Write the doc again for the Phase 20 path. Add to the `--dirty-budget` help text that a value below 64M can change a stream with out-of-order writes into an overlay. Make the message from `MIN_DIRTY_BYTES >> 20`, for example `format!("below the minimum of {}M ({MIN_DIRTY_BYTES} bytes)", MIN_DIRTY_BYTES >> 20)`.

### IN-05: Impossible-state guard: the self-join check in `stop_flusher`

**Status:** Fixed in b912aed. The check stays; the doc now gives the true reason (a test hook in a flusher step could call `stop_flusher`, and a join there would wait for itself).
**File:** `crates/slicefs-cli/src/filesystem.rs:3288`, `:3311`
**Category:** necessity
**Issue:** The doc says that `stop_flusher` does not join on the flusher thread because "the last `Arc` can drop there after a pass". But a drop of the last `Arc` does not call `stop_flusher`: there is no `impl Drop for SliceFsFilesystem`. The callers are `shutdown()` (the FUSE `destroy` thread, `fuse_callbacks.rs:65`), `run_mount` (the main thread, `mount.rs:483`) and tests. None of them runs on the flusher thread. A test hook in a flusher step could call it, so this rests on judgment.
**Fix:** Keep the check if you want it as cheap safety, but correct the doc: the check is for a caller on the flusher thread (for example a hook), not for the drop of the last `Arc`. Or remove the check and the doc sentence.

### IN-06: `a_configured_idle_time_is_used` has only 1 s of real-time margin

**Status:** Fixed in b912aed. Idle time 60 s, passes at 50 s and 70 s (10 s of real-time margin).
**File:** `crates/slicefs-cli/tests/flusher_tests.rs:1015-1034` (assert at `:1030`)
**Issue:** `FlushCtl::now_ms()` is the real clock plus the test offset. The test moves the clock by 6 s with an idle time of 7 s, and asserts that the pass leaves F. If more than 1 s of real time goes by between the last fill write of F and that pass, F is idle and the pass stores it, and the assert fails. Between them there is only one `test_dirty_cost` call and one `test_advance_clock` call, so the risk is low. But the deferred items record host loads up to about 75, and this is the Phase 19 WR-07 class of test. The other policy tests have a margin of 20 s or more, or a hot window of 60 s or 1 h.
**Fix:** Use a larger idle time with the same check, for example `idle_secs: 60`, advance 50 s (assert F stays), then 20 s more (assert F's cost is 0).

---

_Reviewed: 2026-11-23T12:50:14Z_
_Reviewer: Claude (gsd-code-reviewer)_
_Depth: deep_
