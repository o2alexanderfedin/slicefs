---
phase: 19-fine-grained-locks
plan: 09
subsystem: concurrency
status: complete
tags: [lock-03, lock-05, d-04, d-06, d-15, d-16, d-18, namespace, stripes, commit-gate]

requires:
  - phase: 19-fine-grained-locks
    provides: "19-01 sync shim (Rank::Stripe/Rename/Gate, new_keyed, rank::enter, require_gate); 19-03 modify_inode and change_xattrs; 19-05 handle table with open counts and orphans; 19-07 transient cell for a closed-file size change; 19-08 logging in the apply hold"
provides:
  - "crates/slicefs-cli/src/locks.rs: NamespaceLocks (256 stripes, Rank::Stripe keyed by index, site \"stripe\"), stripe(ino), NsGuard, rename mutex (Rank::Rename)"
  - "SliceFsFilesystem::lock_namespace(cross_dir, known, lookups) -> (NsGuard, lookups under the lock), with lookup-then-lock revalidation"
  - "open checks the inode under stripe(ino) (ENOENT); uncount / drop_last_link decide under the same stripe; uncount_held and close_handle_held for callers that hold it"
  - "create/mkdir/mknod/symlink: parent stripe plus live-directory check; link/unlink/rmdir/rename through lock_namespace; rename mutex first for a cross-directory rename"
  - "setattr fields, setxattr, removexattr: stripe then cell (with_inode_locked); xattr changes check that the inode exists"
  - "gate_shared / gate_exclusive -> GateGuard (std RwLock guard plus a Rank::Gate entry); used in all 33 FUSE callbacks, gc_start_mode, the harness gated(); timed_commit holds a Gate entry"
  - "Harness worker and commit threads run with rank::require_gate(true); serialize_off_target is in the default suite (2 s per mode)"
  - "test_orphan_count accessor; Site::Stripe"
  - "Replay skips a Setxattr/Removexattr record of an inode it already reclaimed"
affects: [19-10, 19-11, 20, 21]

estimate:
  tokens: 140000
  tasks: 3
actuals:
  tokens: 16665
  tasks: 3
  commits: 5

plan_head_before: c10866671b1e38746b0abaade9f54ed1e989dce0
plan_head_after: 37b1119191e6609d8be2e43a7ce20b35f863e127
commits: 5

tech-stack:
  added: []
  patterns:
    - "Namespace op: `let (_ns, found) = self.lock_namespace(cross_dir, &[parents..], &[(parent, name)..]);` then use `found` (stable while `_ns` lives)"
    - "Functions that run under a stripe their caller holds are named `*_held` (or documented: `drop_last_link`); a function that takes a stripe itself must not be called under that stripe"
    - "Gate: `let _op = fs.gate_shared();` (never `commit_gate().read()` in request code)"

key-files:
  created:
    - crates/slicefs-cli/src/locks.rs
  modified:
    - crates/slicefs-cli/src/lib.rs
    - crates/slicefs-cli/src/filesystem.rs
    - crates/slicefs-cli/src/fuse_callbacks.rs
    - crates/slicefs-cli/src/commit_timer.rs
    - crates/chunk-store/src/lock_stats.rs
    - crates/slicefs-cli/tests/lock_stats_report.rs
    - crates/slicefs-cli/tests/dir_link_tests.rs
    - crates/slicefs-cli/tests/stress_harness.rs
    - crates/slicefs-cli/src/oplog.rs
    - crates/slicefs-cli/tests/write_log_replay_tests.rs
    - .planning/REQUIREMENTS.md
    - .planning/ROADMAP.md

key-decisions:
  - "Rank 2 is shared by the commit lock (19-01) and the rename mutex. Rule: no thread holds both. The checker enforces it, because two Rank::Rename locks at once panic. The commit lock is taken by the commit and GC threads (and wait_for_commit before a gate), the rename mutex only by a cross-directory rename, which never commits. The six-rank order of CONTEXT.md is unchanged."
  - "lock_namespace reads the children with no stripe, locks the sorted set, reads again, and retries on a change. This is the plan's revalidation with one unlocked read in place of a first lock of the known stripes."
  - "The size part of setattr stays under the cell lock only (the open cell, or the transient cell of a closed file), not under the stripe, because it can run stream steps or a splice (rule 1). The other fields then run under stripe, then cell."
  - "create counts the new handle before it releases the parent stripe, so an unlink of the new name makes an orphan instead of reclaiming an inode that is about to be opened."
  - "symlink stores its target content before it takes the parent stripe."
  - "link refuses an orphan (nlinks 0) with ENOENT."
  - "256 stripes, each on its own cache line; stripe(ino) = (ino * 0x9E3779B97F4A7C15, wrapping) >> 56."

patterns-established:
  - "Every namespace operation locks its stripes in ascending stripe order, cross-directory rename after the rename mutex (D-04, LOCK-03)"
  - "Every request holds the commit gate through gate_shared, so the rank stack has a Gate entry and the strict-gate rule can run in the harness (D-16)"

requirements-completed: [LOCK-03]

duration: 75min
completed: 2026-11-22
---

# Phase 19 Plan 09: Namespace stripes, atomic open/close/unlink, gate at rank 1 Summary

**Namespace operations lock a sorted set of 256 stripes in ascending order, with the rename mutex first for a cross-directory rename. Open, last close and last unlink decide under one stripe. Every request takes the commit gate through `gate_shared`, which puts a rank-1 entry on the rank stack, and the harness runs with the strict-gate rule. `serialize_off_target` is in the default suite.**

## Performance

- **Duration:** about 75 min (with the replay fix after the review)
- **Started:** 2026-11-22T07:56Z (after `c108666`)
- **Completed:** 2026-11-22T09:10Z
- **Tasks:** 3
- **Files modified:** 13 (1 created)

## Accomplishments

- `locks.rs`: `NamespaceLocks`, 256 stripes at `Rank::Stripe` keyed by index and timed as `stripe`. `lock(&[ino])` sorts and deduplicates, so two inodes in one stripe lock it once. There is a rename mutex at `Rank::Rename`.
- `lock_namespace` is used by unlink, rmdir, link and rename (5 uses in `filesystem.rs`).
- `open` returns ENOENT for an inode that is gone. The handle is counted under `stripe(ino)`, and last close (`uncount`) and last unlink (`drop_last_link`) decide under the same stripe.
- Creates check under the parent stripe that the parent is still a live directory (PITFALLS 6c).
- setattr fields, setxattr and removexattr take the stripe, then the cell. xattr changes check that the inode exists. This closes the 19-03 race between `set_xattr` and `delete_inode`.
- Gate helpers: in all 33 callbacks (`gate.read()` count in `fuse_callbacks.rs` is 0), in `gc_start_mode`, in the harness `gated()`, and as a Gate entry in `timed_commit`.
- `serialize_off_target` is un-ignored (2 s per mode, watchdog 30 s). The ignore count in `stress_harness.rs` is 5.
- LOCK-03 and ROADMAP criterion 4 now say "in ascending stripe order (inode number hashed to a stripe)".

## Task Commits

1. **Task 1: NamespaceLocks and atomic open, last close and unlink** (tracer): `3909e6e`
2. **Task 2: Every namespace operation under its stripes; rename mutex; wording** (TDD): `dbd8166`
3. **Task 3: Gate helpers, strict-gate rule, serialize_off_target in the suite** (TDD): `c9664c8`
4. **Fix: replay skips an xattr record of a reclaimed inode** (deviation 4): `37b1119`
- Plan metadata: `4b494f5` (first SUMMARY), then the docs commit with this update

## Verification

- Task 1: `dir_link_tests` 28 passed (with `unlink_and_last_close_never_leak_an_orphan`). `lock_stats_report --features lock-stats` passed with `stripe` in the list. The tracer gate re-ran both after the commit: green.
- Task 2: `dir_link_tests` 31, `posix_compliance_tests` 40 and `e2e_filesystem_tests` 78 passed. `dir_link_tests` ran 5 more times, green each time (about 1.04 s).
- Task 3: `stress_harness`: 5 passed, 5 ignored, 6.23 s warm (6.44 s on the first run). The limit is 10 s; before this plan it was 11.4 s with the target included at 5 s per mode. `--features deadlock-detect ... serialize_off_target`: passed in 4.72 s, no "STRESS deadlock detected", `max_inflight=4` in both modes.
- Before each task commit: `cargo test --workspace` exit 0, 66 `test result: ok` lines, no `FAILED`, no `lock rank violation`.
- After the plan: the full suite command of 19-VALIDATION.md plus `cargo check -p slicefs-cli --all-targets --features deadlock-detect` exit 0. `lock_stats_report` passes (0.37 s).
- Acceptance greps: `pub struct NamespaceLocks` 1, `pub mod locks` 1, `"stripe"` in the report 1, orphan test 1, `lock_namespace` 5, old wording 0/0, new wording 1/1, ignore before `serialize_off_target` 0, `#[ignore` 5, `fn gate_shared` 1, `require_gate(true)` 3.

## TDD Evidence

- **Task 2 (red before the fix, at `3909e6e` plus the new tests):**
  - `create_in_a_removed_directory_fails` failed: create returned ENOTDIR (20), not ENOENT.
  - `set_xattr_and_unlink_leave_no_xattr_behind` failed in round 0: "xattr of deleted inode 2 left behind".
  - `namespace_ops_in_parallel_keep_the_tree_consistent` failed: "entry n5 points to deleted inode 126".
  - All three pass after the change.
- **Task 3:**
  - `strict_gate_catches_a_request_without_the_gate` already passed at HEAD, because the checker and `require_gate` exist since 19-01.
  - The red step was the harness change by itself: `require_gate(true)` on the workers while `gated()` still used `gate.read()`. Then `short_one_file`, `short_many_files`, `model_detects_planted_bug` and `serialize_off_target` failed with "lock rank violation: Stripe/60 taken without the commit gate".
  - They pass with the gate helpers.
  - `serialize_off_target` was already green at 5 s per mode before the plan, and it is green at 2 s.
- The test and the fix were committed together (no red commit).

## Decisions Made

See `key-decisions`. All of them can be changed later.

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 2 - Missing critical] xattr changes need an inode-exists check**
- **Found during:** Task 2
- **Issue:** `change_xattrs` does not check that the inode exists. With the set and the delete serialized by the stripe, a set that runs after the delete would still add an entry for the deleted inode.
- **Fix:** `test_setxattr` and `test_removexattr` call `get_inode` under the stripe (ENOENT if gone). This closes the race that 19-03 left open. Test: `set_xattr_and_unlink_leave_no_xattr_behind` (200 rounds).
- **Commit:** `dbd8166`

**2. [Rule 1 - Bug] A link could give a name to an orphan**
- **Found during:** Task 2
- **Issue:** `simulate_link` did not check `nlinks`. A link to an unlinked inode that is still open would give it a name, and its last close would then reclaim an inode that has a directory entry.
- **Fix:** ENOENT for `nlinks == 0`, under the stripes.
- **Commit:** `dbd8166`

**3. [Rule 1 - Bug] create: unlink of the new name before the handle was counted**
- **Found during:** Task 2 design
- **Issue:** `test_create` links the name and then counts the handle. An unlink in between finds no open handle and reclaims the inode, and then the handle and cell are made for a deleted inode.
- **Fix:** the parent stripe is held from the live-directory check until the handle is counted and the cell exists.
- **Commit:** `dbd8166`

### Plan facts

- **Parallel test names:** in `namespace_ops_in_parallel_keep_the_tree_consistent`, the threads share one pool of 6 names instead of one prefix per thread. With own prefixes, no lookup ever races with a change of the same name, so the revalidation of `lock_namespace` (T-19-31) would not be exercised. The final checks are the plan's (live entries, nlinks equals the number of entries, no inode without entries, no orphan). With shared names the test was red before the change.
- **setattr size outside the stripe:** see key-decisions. The must-have says "setattr ... take the inode's stripe and then its cell". This holds for every field except the size. The size change holds the cell lock (open) or the transient cell (closed), as rule 1 and the plan action require.
- **Test helpers `test_setattr_mode`, `test_setattr_uid_gid`, `test_setattr_mtime`:** they take the stripe only, not the cell. No handler calls them: handlers use `test_setattr_full`, which takes stripe then cell.
- **`create_in_a_removed_directory_fails`** also covers mkdir, mknod and symlink in the removed directory.
- **Extra test** `two_inodes_in_one_stripe_are_locked_once` (Task 1 behavior 3): it finds a file in the root's stripe and unlinks it while open, then closes it. There are also unit tests in `locks.rs` (range and spread of the hash, dedup, ascending keys on the rank stack).
- **S3 spike site list:** `"open_files"` changed to `"file_cell"` (the stale label that 19-05 noted). This is an ignored spike, so it is outside the suite.
- **Gate entry order:** `gate_shared` and `gate_exclusive` push the Gate entry before they take the std lock. A second gate hold on one thread therefore panics in debug builds instead of deadlocking behind a queued commit. `GateGuard` releases the std guard first, then the entry.

**4. [Rule 1 - Bug] Replay aborted on an xattr record of an orphan**
- **Found during:** review after Task 3
- **Issue:** with deviation 1, the xattr calls refuse a missing inode. Take an orphan that was open in the live run through a plain open, which is not in the log. Its Unlink reclaims it in replay, so a later `Setxattr` or `Removexattr` record of it failed and aborted the whole replay. Replay already skips a `Write` or `Truncate` of a gone inode.
- **Fix:** `oplog.rs` skips such an xattr record (`stats.skipped`).
- **Test:** `an_xattr_change_of_an_open_orphan_replays`. It was red before the fix: "write log record 1 (setxattr) did not replay".
- **Files modified:** `crates/slicefs-cli/src/oplog.rs`, `crates/slicefs-cli/tests/write_log_replay_tests.rs` (not in the plan file list)
- **Commit:** `37b1119`

**Total deviations:** 4 auto-fixes (3 Rule 1, 1 Rule 2) and 7 notes. **Impact:** all plan truths hold. The setattr truth holds with the size exception that rule 1 requires.

## Known open issue (deferred)

- **Size change of a closed file and unlink of it at the same time:** `setattr_size_closed` runs its splice in the transient cell without a stripe. Then it sets the manifest and stamps the size. If an unlink reclaims the inode meanwhile, the manifest entry (and one refcount) of the deleted inode remains, and the setattr returns EIO. This race existed before this plan. A fix needs the install step to re-check the inode under its stripe after the splice. That is not possible while the transient cell (rank 4) is held, so the code has to be restructured. It is recorded in `deferred-items.md`.
- **Cross-directory rename ancestor check:** still not present (CONTEXT Deferred Ideas). The rename mutex is where it would go.

## For later plans

- Plan 10 (logging of namespace records in the apply hold): the stripe hold of each namespace op (`_ns` in `simulate_*`, `test_create`, `test_mknod_rdev`, `with_inode_locked`) is the apply hold. Append the record before `_ns` drops.
- With a stripe held, do not call `uncount`, `test_setxattr`, `test_removexattr`, `test_setattr_full`, `test_create`, `test_mknod_rdev`, `open_logged` or any `simulate_*`. Each of them locks a stripe, and a stripe with the same or a lower key than the held one panics in debug builds. Under a held stripe, use `uncount_held`, `close_handle_held` or `drop_last_link`.
- A stripe must not be taken with a cell held (rank 3 after 4 panics). `test_release` therefore runs `release_write` (cell) before `uncount` (stripe).
- New request code takes the gate only through `gate_shared`. A thread that runs request code in a test can turn on `rank::require_gate(true)` to check this.
- Replay (Plan 10): `open_logged` now fails with ENOENT for an inode that does not exist, so replay must create the inode first. It does this today. A `Write` or `Truncate` of a gone inode is skipped, as before. Xattr records of a gone inode are now skipped too (deviation 4).
- Replay (Plan 10), older than this plan: a `Setattr` record of an orphan whose open is not in the log still aborts replay. Replay reclaims the orphan at its Unlink, and then `modify_inode` gives NotFound. Plan 10 can skip it in the same way.

## Issues Encountered

- A full `cargo test --workspace` takes about 10 minutes here (mostly the build). I ran each one in the background and read the exit code from its log.

## Known Stubs

None.

## Threat Flags

None. There is no new endpoint, file kind or trust boundary.
- T-19-SC: no new crate.
- T-19-31: `lock_namespace` revalidation; `namespace_ops_in_parallel_keep_the_tree_consistent` with shared names.
- T-19-32: sorted, deduplicated stripes; rename mutex first; Gate entry on the rank stack; strict-gate rule in the harness; `deadlock-detect` run of `serialize_off_target` passed.
- T-19-33: `unlink_and_last_close_never_leak_an_orphan`.
- T-19-34: `create_in_a_removed_directory_fails`.
- T-19-35: the REQUIREMENTS and ROADMAP edits change only the order wording.

## Self-Check: PASSED

- `crates/slicefs-cli/src/locks.rs`, this SUMMARY and `deferred-items.md`: FOUND
- Commits `3909e6e`, `dbd8166`, `c9664c8`, `4b494f5` and `37b1119`: FOUND on HEAD (`git rev-list --count c108666..37b1119` prints 5)
- After the fix: `cargo test --workspace` exit 0 (66 ok, no FAILED, no rank violation); the `deadlock-detect` run of `serialize_off_target` passed in 4.99 s; the four Task 1 acceptance greps print 1, 1, 1, 1
