---
phase: 19-fine-grained-locks
reviewed: 2026-11-22T23:46:26Z
depth: deep
files_reviewed: 37
files_reviewed_list:
  - crates/chunk-store/src/sync.rs
  - crates/chunk-store/src/lib.rs
  - crates/chunk-store/src/pack_io.rs
  - crates/chunk-store/src/retry.rs
  - crates/chunk-store/src/index.rs
  - crates/chunk-store/src/mem.rs
  - crates/chunk-store/src/lock_stats.rs
  - crates/chunk-store/src/locality.rs
  - crates/chunk-store/src/lock_tail_tests.rs
  - crates/chunk-store/src/race_tests.rs
  - crates/metadata/src/store.rs
  - crates/metadata/src/store_io.rs
  - crates/metadata/src/wal/mod.rs
  - crates/metadata/src/wal/per_op.rs
  - crates/metadata/src/wal/buffered.rs
  - crates/metadata/src/wal/periodic.rs
  - crates/metadata/src/wal/flush_on_fsync.rs
  - crates/metadata/src/wal/no_wal.rs
  - crates/metadata/src/segment/writer.rs
  - crates/slicefs-cli/src/filesystem.rs
  - crates/slicefs-cli/src/handlers.rs
  - crates/slicefs-cli/src/overlay.rs
  - crates/slicefs-cli/src/oplog.rs
  - crates/slicefs-cli/src/locks.rs
  - crates/slicefs-cli/src/fuse_callbacks.rs
  - crates/slicefs-cli/src/commit_timer.rs
  - crates/slicefs-cli/src/gc_pacing.rs
  - crates/slicefs-cli/src/lib.rs
  - crates/slicefs-traits/src/metadata.rs
  - crates/slicefs-cli/tests/lock_free_io.rs
  - crates/slicefs-cli/tests/multi_handle_posix.rs
  - crates/slicefs-cli/tests/lock_wait_rows.rs
  - crates/slicefs-cli/tests/rebase_step_tests.rs
  - crates/slicefs-cli/tests/write_log_replay_tests.rs
  - crates/slicefs-cli/tests/wal_durability_tests.rs
  - crates/slicefs-cli/tests/stress_harness.rs
  - crates/slicefs-cli/tests/common/mod.rs
findings:
  critical: 1
  warning: 7
  info: 2
  total: 10
status: issues_found
fix_status: all_fixed
fixed: 10
skipped: 0
---

# Phase 19: Code Review Report

**Reviewed:** 2026-11-22T23:46:26Z
**Depth:** deep
**Files Reviewed:** 37
**Status:** issues_found

## Summary

Diff base `a045f0c` to `HEAD` on `feature/phase-19-fine-grained-locks`. The review followed the call chains across the three hot crates. It checked these items: the rank checker (`sync.rs`), the cell protocol (copy, I/O with no lock, publish at the same generation), the step token, the hard cap, the WAL append/write-out split (PerOp and buffered), the one-write-in-flight rule of the chunk store and of the segment writer, the stripes and `lock_namespace`, replay by logged inode, and the commit and GC gate paths.

These parts hold up under tracing:
- The rank checker: it checks only the top of the stack, and that is enough because the stack stays sorted. Condvar suspend and resume work.
- The SyncWait latch and the step-token wake order: no lost wakeup.
- The PerOp one-write rule and the order of `done` and `failed`.
- The chunk-store `begin_write`/`publish`: every seal and `append_locked` caller waits for no write in flight first.
- Generation monotonicity across overlay replacement.
- `lock_namespace` revalidation.

The main defect is liveness on the overlay write path. When several threads write one file, the "At the cap no writer changes the overlay" premise is false. A writer at the hard cap then gets EIO. Several error paths also report EIO for a write that was already applied and logged, or drop unstored data when the last release fails. The O_TRUNC open path holds a namespace stripe across waits and store I/O. A size change takes no stripe, so its record can be logged before the Create record of the inode.

The necessity references could not be applied in this run: no `necessity:` classification is given.

## Fix Status

All 10 findings are fixed on `feature/phase-19-fine-grained-locks`, one commit per finding or small group. Each new test failed before its fix and passes after it. This section replaces a separate REVIEW-FIX.md file. "Human check" means a logic fix: the tests pass, but a person must check the logic.

| Finding | Status | Test (red before the fix) |
|---------|--------|---------------------------|
| CR-01 | fixed: human check | `lock_free_io::overwriters_do_not_starve_a_writer_at_the_cap` |
| WR-01 | fixed: human check | `lock_free_io::a_failed_step_after_the_apply_does_not_fail_the_write` |
| WR-02 | fixed: human check | `lock_free_io::an_o_trunc_open_waits_for_a_finalize_step` (chmod during the wait) |
| WR-03 | fixed: human check | `write_log_replay_tests::a_size_change_of_a_new_file_comes_after_its_create_record` |
| WR-04 | fixed: human check | `lock_free_io::a_failed_last_release_keeps_the_unstored_bytes` |
| WR-05 | fixed: human check | `lock_free_io::a_writer_waits_for_a_step_while_it_makes_progress` |
| WR-06 | fixed: human check | `lock_free_io::a_panic_in_a_step_clears_the_step_token` |
| WR-07 | fixed | the changed asserts in `lock_free_io.rs` |
| IN-01 | fixed: human check | `lock_free_io::an_unlink_releases_the_content_with_no_stripe_held` |
| IN-02 | fixed: human check | no new test (an inode that is not cached is hard to make in a test) |

Commits: `244eb80` (WR-07), `863be32` (CR-01, WR-01), `c7bdb36` (WR-02, WR-03), `41bb85b` (WR-04, WR-05, WR-06), and the IN-01 + IN-02 commit after it. `cargo test --workspace` passed on the tree of each code commit. For `244eb80` (one test file only) the `lock_free_io` target was run on its tree, and the rest of the workspace was unchanged from the green baseline.

Open points (also in `deferred-items.md`):
- CR-01: while a writer of a file is at the hard cap, the other writers of that file wait for the cap step (writers wait for one writer's I/O on one file; readers do not wait). The performance effect is not measured. Phase 20 (the flusher and the throttle) can change this.
- WR-04: a cell that a failed last release keeps is stored only by a later open and release, or by destroy. Until then each commit cut logs its extents again.
- WR-05: the progress count moves only with chunk-store work on the holder's thread. A step that does more than 15 s of CPU work with no store call gets EIO for its waiters. No step does that today (each chunk is put after it is hashed).
- The 19-09 and 19-10 deferred items (a closed-file size change against an unlink or a setattr of other fields) stay open: the WR-03 stripe barrier orders a size change only after a create.

## Critical Issues

### CR-01: A writer at the hard cap gets EIO when other threads write the same overlay

**Fix status:** fixed: human check. A writer at the cap is counted in the cell (`CellState::cap_writers`). While one is counted, every overlay writer waits for the step in flight before it applies, and no ordinary rebase step starts (`rebase_cell`). So only a forced drop or a retire can drop a step at the cap. Readers do not wait. The `STEP_TRIES` EIO stays, but only before the apply.

**File:** `crates/slicefs-cli/src/filesystem.rs:2326-2340` (cap gate), `:1817-1830` (`cap_step`), `:2297-2304` (`drops >= STEP_TRIES` -> EIO)

**Issue:** The cap gate sends a writer into `cap_step` only when *its* write would raise the cost above `hard_cap()` (`after > cap`). A write whose `cost_after` stays at or below the cap applies at once. This includes an overwrite inside an existing extent, or growth inside the spare capacity of an extent (`GROW_DIV`). It does not check `g.stepping` first. `Overlay::write` always moves the generation (`overlay.rs` `put`). So the unlocked rebase step of the `cap_step` writer is dropped in `step_publish` (`ov.generation() != plan.generation`), and `drops += 1`. When another thread's step is in flight, `cap_step` does not reset `drops` (it only waits). After `STEP_TRIES` (16) dropped steps in a row, the writer returns EIO. The comment at `:2337-2338`, "At the cap no writer changes the overlay, so a step publishes", is false.

**Failure scenario:** Two or more threads write one file in overlay mode, for example a parallel downloader that writes pieces at different offsets, or a database that writes random pages. Thread B's writes mostly extend B's own extent inside its spare capacity, or rewrite hot pages, so their `after <= cap`. The over-limit `rebase_cell` attempts of every writer are dropped too (any concurrent write moves the generation), so the cost climbs to the hard cap. Thread A's next write would grow an extent, so A enters `cap_step` and starts a step of several ms. B applies a write during the step, and the step is dropped. After 16 such steps A's `write(2)` returns EIO. This happens in normal use, with no fault injected. The default tests do not catch it: they stay below the dirty budget, and the cap tests use one writer.

**Fix:** When the cost is at or above the cap (or above the limit), an overlay writer must not change the overlay while a step is in flight. Gate every overlay apply on the token in that state:
```rust
if let WriteMode::Overlay(ov) = &g.file.write_mode {
    let (cap, after) = (self.hard_cap(), ov.cost_after(offset, data.len()));
    // At or above the limit, a step in flight must be able to publish:
    // wait for it (cell unlocked) before any change of the overlay.
    if g.stepping.is_some() && cost_before >= self.dirty_limit.load(Ordering::Relaxed) {
        g.wait_step()?;
        continue;
    }
    if cost_before > 0 && after > cap { self.cap_step(&mut g, after - cap, &mut drops)?; continue; }
    ...
}
```
Another fix is to make `step_publish` accept a publish when the only changes since Phase A are puts that do not touch the copied pieces (a small written-range list, RESEARCH Pattern 1 "cheap improvement"). Also count only forced drops toward the EIO limit. Add a test with 1 growing writer and 2 overwriting writers at the hard cap.

## Warnings

### WR-01: A write that was applied and logged can still return EIO (an O_APPEND retry then duplicates data)

**Fix status:** fixed: human check. After the apply and the `Write` record, a failed hard-cap step, rebase step or budget step is printed after the hold, and the write returns `Ok`. The next write, the release or a budget step does the work. New test hook: `test_fail_steps(n)`.

**File:** `crates/slicefs-cli/src/filesystem.rs:2443-2449`, `:2460`, `:2471`

**Issue:** After `overlay_write` (or the stream push) applied the bytes and `log(lsn, offset)` appended the `Write` record, the write can still fail with EIO from three places:
- the post-apply hard-cap loop (`drops >= STEP_TRIES`);
- `rebase_cell(...).map_err(|_| libc::EIO)`, after any Phase B store error, including a `STORE_WAIT` timeout;
- `keep_budget` -> `free_dirty` -> `to_overlay`/`rebase_cell` errors.

`handle_write_flags` replies `WriteResult::Error(EIO)`, but the record is written out and the bytes are in the file.

**Failure scenario:** An application gets EIO from an `O_APPEND` write and retries it, which is the normal reaction. The retry takes EOF again under the cell lock and appends the same bytes a second time. The replay of the log has both records. A plain write retried at the same offset is idempotent, but an `O_APPEND` write is not. The CR-01 starvation and a slow store make this path reachable.

**Fix:** After the apply, a step or budget failure must not change the result of the write. The bytes are safe in the overlay, under the hard cap, and in the log. Log the step error and return `Ok(n)`:
```rust
Ok(n) if over => { if let Err(e) = self.rebase_cell(&mut g, free) { g.notes.push(format!("rebase after write failed: {e}")); } Ok(n) }
...
Ok(n) => { let _ = self.keep_budget(&mut g, added); Ok((n, offset)) }
```
For the post-apply cap loop, stop stepping and return `Ok`. Its EIO is only valid before the apply.

### WR-02: An O_TRUNC open (and an open that meets a transient cell) holds the inode's stripe across a wait of up to 15 s and store I/O

**Fix status:** fixed: human check. `open_logged` holds `stripe(ino)` only for the existence check and the handle count, then does the cell work (also the truncate) with no stripe. An error undoes the count with `uncount`. `create_logged` runs no truncate for `O_TRUNC`: the new inode is empty.

**File:** `crates/slicefs-cli/src/filesystem.rs:3844-3858` (`open_logged` holds `stripe(ino)` across `open_write`), `:858-889` (`open_write` -> `setattr_size_open(&mut g, 0)`), `:2171` (create with `O_TRUNC` under the parent and child stripes)

**Issue:** `open_logged` holds `self.ns.lock(&[ino])` for the whole `open_write` call. With `O_TRUNC`, `setattr_size_open` runs these steps:
- `wait_step()` (up to `STEP_WAIT` 15 s for each step in flight, and it loops over new steps);
- `run_step(get_manifest + prepare_manifest)`: a possible cold node read and a node put that can wait `STORE_WAIT` 10 s.

Without `O_TRUNC`, `cells.get_or_insert` can return the *transient* cell of a closed-file size change. `lock_cell` then waits for the whole splice (content I/O), with the stripe held. `create_logged` calls `test_setattr_size(ino, .., 0)` with both the parent's and the new inode's stripes held. The rule in `locks.rs:11-14` and RESEARCH Pattern 4 rule 1 says "a stripe is never held across content I/O, hashing or a splice". This path is not one of the accepted exceptions. The rank checker cannot see a step-token wait, so the dependency stripe -> token is not checked either.

**Failure scenario:** Process A writes a log file (stream steps). Process B runs `: > file` (`open(O_TRUNC)`). B holds `stripe(ino)` until A's step ends, and B can lose the cell to other stream writers more than once. Unlink, link, rename, create, setattr and xattr calls on every inode that hashes to the same stripe (1 in 256, including parent directories) stall for that time. With a stalled store they stall for 10-15 s, and then B gets EIO.

**Fix:** Count the handle under the stripe, which is the only part that needs it (existence check and `open_count`). Then release the stripe, and do the cell work (`write_handles += 1`, the truncate) with the stripe released. If the truncate fails, undo it with `close_handle` (which takes the stripe again). Unlink is still safe: the handle is already counted. In `create_logged`, apply `O_TRUNC` after `_ns` is dropped (a new inode is empty anyway, so the truncate can be skipped).

### WR-03: A size change takes no stripe, so its `Setattr` record can come before the `Create`/`Mknod` record and replay drops it

**Fix status:** fixed: human check. `setattr_size_logged` first takes `stripe(ino)` and releases it at once. The caller holds no stripe. The test hook `test_set_mkdir_hook` now also runs in create and mknod (in `new_entry`).

**File:** `crates/slicefs-cli/src/filesystem.rs:2999-3046` (`setattr_size_logged`), `:2186-2194` (`new_entry` links the name before the record), `:2167-2172`, `:3320`; `crates/slicefs-cli/src/oplog.rs:302`

**Issue:** `new_entry` makes the name visible (`meta.link`) and keeps the stripes until the `Create` record is appended. D-08 relies on this: "a request that finds the new name and works on the inode by its number ... waits for the inode's stripe". `setattr_size_logged` takes no stripe. Case A locks the create's new cell (made by `open_write` before the record). Case B makes a transient cell. Either case appends its size-only `Setattr` record at once.

**Failure scenario:** Thread 1 runs `create("f")` and is between `new_entry` and `log(Create)`. Thread 2 looks up "f" (lookups take no stripe) and calls `truncate("f", 4096)`, which logs `Setattr{ino, size: 4096}`. Thread 1 then logs `Create{ino}`. After a crash, replay reaches the `Setattr` first. The inode does not exist yet, so the new skip rule at `oplog.rs:302` skips it, and the file comes back with size 0. The live mount showed 4096. The same happens with `mknod` followed by a truncate.

**Fix:** Before the size change takes a cell, wait for any create of the inode in progress. Take `stripe(ino)` and release it at once (rank 3 before 4, legal). Hold no stripe across the size change itself:
```rust
fn setattr_size_logged(&self, ino: u64, new_size: u64, log: LogOp<'_>) -> Result<(), i32> {
    drop(self.ns.lock(&[ino])); // a create that linked `ino` has logged its record
    ...
}
```
Do this together with the WR-02 fix. `create_logged` (`:2171`) calls `test_setattr_size` with `stripe(ino)` held. A stripe barrier there would take the same stripe twice (a rank panic in debug, a self-deadlock in release). So the O_TRUNC of a create must move out of the stripe hold first.

### WR-04: A failed last release retires the cell and discards its unstored data; a step-token timeout is now enough to trigger it

**Fix status:** fixed: human check. When `finalize_steps` fails and the state is not clean, the cell stays in the map with no write handle. Reads see the bytes, a commit cut flushes them or logs them again, and the next open and release, or destroy, store them. `reclaim_inode` drops such a cell when the inode goes. Open point: nothing retries the store before a new open or destroy.

**File:** `crates/slicefs-cli/src/filesystem.rs:925-934` (`finalize`), `:972-976` (`wait_step()?` in `finalize_steps`)

**Issue:** `finalize` sets `g.retired = true` and removes the cell even when `finalize_steps` returned Err. This drops a dirty overlay's extents, a stream's unflushed tail, or pending writes. Before this phase, only a store I/O error lost the state. Now `wait_step()` can fail the finalize too: it returns EIO after `STEP_WAIT` for a step of another request on the cell, for example a size change from `truncate(path)`. So a lock-wait timeout now discards data.

**Failure scenario:** A file is closed while another process truncates it by path and the store is slow (a seal or pack create near `STORE_WAIT`). The release waits 15 s, gets EIO, and retires the cell. Reads now show the old manifest content. The `Write` records are still in the WAL, but the next commit cuts the log with a root that does not have them. After that, the data is lost for good.

**Fix:** On error, do not retire a cell whose state is not clean. Leave it in the map with `write_handles == 0`. The commit cut (`flush_cell(Locked)` plus relog), a later open, or `test_destroy` then stores it. Retire only when `finalize_steps` returned Ok, or when the state is clean.

### WR-05: `STEP_WAIT` (15 s) is shorter than one step can legitimately take

**Fix status:** fixed: human check. Rule: a waiter waits for one step in slices of `STEP_WAIT` (15 s). It waits another slice only if the step moved its progress count in the last slice. Each chunk-store put or read on the holder's thread moves the count, and so does each store wait that ends in time (`chunk_store::with_progress`). After a slice with no progress, or `STEP_WAIT_MAX` (120 s) for that step, it gives EIO. Every store wait ends within `STORE_WAIT` (10 s), so a slow step that is not stuck moves its count in each slice. New test hook: `test_set_step_wait`.

**File:** `crates/slicefs-cli/src/filesystem.rs:142-145`, `:414-421`

**Issue:** The comment says that 15 s is "above the 10 s STORE_WAIT ... because the step can itself wait that long in a store put". But one step makes many puts: a rebase step of `final_step` (8 MiB) is about 1000 chunk puts plus nodes, and a stream push puts many chunks. Each put can wait up to `STORE_WAIT` for a pack create, a seal, a record write in flight or `KEEP_CAP` (`lib.rs:1519-1527`). So the time of one step has no bound of 15 s. The measured slowest single write was already 7.8 s (DIRTY_LIMIT notes).

**Failure scenario:** Under a heavily loaded machine or a slow volume (macOS F_FULLFSYNC stalls), a step takes more than 15 s. Every stream writer, size change and finalize that waits for it gets EIO. Through WR-04 the finalize also discards data. The step itself succeeds.

**Fix:** Make the wait depend on progress. For example, the token carries a progress counter that each put moves, and `wait_step` returns EIO only after 15 s with no progress. Or apply `STEP_WAIT` only when the holder is stuck in one store wait. Another way is to give up on the wait without EIO, and only for the paths that can retry.

### WR-06: A panic in a step's I/O phase leaves the step token set forever

**Fix status:** fixed: human check. `run_step` holds a drop guard (`StepEnd`). It ends the token on every path. On a panic it also clears the token from the cell, so waiters wake and later steps can start. The cap count of a guard is also cleared on a panic.

**File:** `crates/slicefs-cli/src/filesystem.rs:401-410` (`run_step`), `:388-394` (`unlocked`)

**Issue:** `run_step` sets `stepping = Some(token)`, unlocks, and calls `io`. If `io` panics (an `expect`/`unwrap` in the splice, chunk store or manifest code, or a test hook), the unwind drops the `CellGuard` with `state == None`. Nothing clears `stepping` or calls `token.done()`. `parking_lot` has no poison, so other threads go on with the cell and see a token that never ends.

**Failure scenario:** After one panic in a stream push of inode X, every later stream write to X waits 15 s and gets EIO, for good. `setattr_size_open` gets EIO too. The last release of X gets EIO in `finalize_steps` and (WR-04) discards X's data. In tests, a panicking hook inside `catch_unwind` leaves a wedged cell for the rest of that test.

**Fix:** Clear the token in a drop guard that runs on unwind as well:
```rust
fn run_step<R>(&mut self, io: impl FnOnce() -> R) -> R {
    let token = SyncWait::default();
    self.stepping = Some(token.clone());
    struct Done<'a>(&'a SyncWait);
    impl Drop for Done<'_> { fn drop(&mut self) { self.0.done(); } }
    let _done = Done(&token);
    let r = self.unlocked(io);   // on unwind: waiters wake; the token is stale
    self.stepping = None;
    r
}
```
Also make `wait_step` treat a token whose `is_done()` is already true as cleared: it may take the lock and set `stepping = None` itself.

### WR-07: Tests assert that work finishes within 300 ms (flaky on a loaded machine)

**Fix status:** fixed. The positive receives use `WAIT` (5 s) while the step is still parked (the hook parks for up to 10 s). The step is let go only after the receive. The mixed check of the cap test also checks that the write it sees waiting is at the cap.

**File:** `crates/slicefs-cli/tests/lock_free_io.rs:238`, `:301`, `:665`, `:895`, `:946`, `:1073`, `:1106`, `:1138`

**Issue:** These tests spawn a thread for a read or write and then call `recv_timeout(Duration::from_millis(300)).expect(...)`. That is a *positive* deadline: the test fails if a debug-build read (Reader open, node loads) or a 128 KiB write takes more than 300 ms. The 19-14/19-15 runs were at load average 30-40. The negative checks ("it waited": `is_err()`) are safe. The positive ones are not.

**Failure scenario:** CI or a local run under parallel `cargo test` load fails at random with "the read waited for the retry: Timeout" even though no lock is held.

**Fix:** Prove "did not wait for the parked step" without a short deadline. Use a long deadline (`WAIT`, several seconds) for the positive receive while the step stays parked, and release the parked step only after the receive. A read that waits for the step then hangs until `WAIT` and fails clearly. A read that does not wait passes, however slow the machine is.

## Info

### IN-01: Safe fix shape for the deferred unlink reclaim under the stripes

**Fix status:** fixed: human check. Under the stripes, `reclaim_inode` takes the manifest digest from memory and deletes the inode. After the stripes, inside the shared gate, `release_content` reads the manifest node and drops the refcounts. This is the same in unlink, in a rename that replaces a file, at the last close of an orphan, and at unmount. New store calls: `manifest_digest`, `manifest_entries`. New test hook: `test_set_reclaim_hook`. The 19-16 deferred item is closed.

**File:** `crates/slicefs-cli/src/filesystem.rs:1318-1326` (`reclaim_inode`), `:1331-1345`; `crates/metadata/src/store.rs:1497-1501`

**Issue:** The deferred 20 ms hold is `get_manifest` -> `load_manifest` (a node read). `manifest_data` holds the digest in memory.

**Fix:** Under the stripes, read the manifest *digest* (memory, `manifest_data`), then `delete_inode`. This removes the entry, so no request can reach the inode. After the `NsGuard` drops, and still inside the request's shared gate (so no commit cut falls in between), call `load_manifest(digest)` and `decrement_refcount` for each entry. Refcount decrements commute. Replay of the `Unlink` from the old root runs the full reclaim again, so the counts stay right. This needs a small store API, for example `take_manifest_digest(ino) -> Option<ObjectKey>` and `manifest_entries(digest)`.

### IN-02: `append_eof` can read the store with the cell locked

**Fix status:** fixed: human check. For a fresh stream, the committed size of an append is read as a step with the cell unlocked, and is used only if the stream generation did not change. No new test.

**File:** `crates/slicefs-cli/src/filesystem.rs:2479-2484`

**Issue:** For a fresh stream cell (`byte_count == 0`), the EOF of an append comes from `self.meta.get_inode(ino)` inside the write's cell hold. If the inode is not cached, `cached_inode` reads it from the store (a node read) with the cell locked. This happens on the request path, so readers of the file wait for it.

**Fix:** Take the committed size when the cell is made (in `open_write`, which already reads the inode under the stripe), or take it in an unlocked step and check the generation, as the `has_content` check does.

---

_Reviewed: 2026-11-22T23:46:26Z_
_Reviewer: Claude (gsd-code-reviewer)_
_Depth: deep_
