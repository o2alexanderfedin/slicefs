# Phase 20: Dirty budget and rebase flusher - Research

**Researched:** 2026-11-22
**Domain:** Rust daemon concurrency: a background rebase flusher, writer admission (throttle), the commit-gate deadlock rule, bounded memory
**Confidence:** HIGH for the code facts (read this session). MEDIUM for the design choices (reversible, internal). LOW for the RSS cap number (not measured yet).

<user_constraints>
## User Constraints (from CONTEXT.md)

### Locked Decisions

#### When dirty data is written (user answer, 2026-11-23)
- The flusher writes (rebase steps to the chunk store) a file's dirty data when one of these is true:
  1. the file was idle (no write) for some time — default `--dirty-idle` 5 s;
  2. its oldest dirty data reached a maximum age — default `--dirty-expire` 30 s (the same as the default `--commit-interval`);
  3. the buffer is full — the shared dirty total is at or above the background threshold (half of `--dirty-budget`), or one file is at its own limit. The flusher then steps the largest dirty file first.
- A writer over the budget sleeps (with no lock held) until the flusher frees memory. After the wait limit (default 5 s), the writer does not fail: it writes, and pays for it by one unlocked step of its own file (the Phase 19 path). No new error for applications. Memory stays bounded by the per-file hard cap and the budget plus one step per writer.
- **Reversibility:** reversible — internal policy; the defaults can change.

#### Mount options (user answer, 2026-11-23)
- Add mount options with default values: `--dirty-budget` (bytes, default 64 MiB = today's `DIRTY_BUDGET`), `--dirty-limit` (bytes per file, default 32 MiB = today's `DIRTY_LIMIT`), `--dirty-idle` (seconds, default 5), `--dirty-expire` (seconds, default 30). Accept size suffixes (K, M, G) for the byte options if a parser exists; otherwise plain bytes. Validate: `--dirty-limit` <= `--dirty-budget`; both at least a few rebase steps; reject 0.
- **Reversibility:** costly — public CLI options; later releases must keep them or deprecate them.

#### Decided autonomously
- [Flusher] Q: "How many flusher threads?" → Selected: "One thread, spawned with the mount like the commit timer; it takes the commit gate shared for each step (never across a sleep) and steps one file per pass under the Phase 19 step token" — `.planning/research/ARCHITECTURE.md` A5; reversible.
- [Flusher] Q: "How does it pick a file?" → Selected: "Budget full: the largest dirty cell (scan of the cell map shards, or a small max-heap rebuilt per pass); idle/expire: cells whose last-write / first-dirty time passed the limit. Per-cell `last_write` and `dirty_since` timestamps (atomics)" — internal.
- [Throttle] Q: "Where does a writer wait?" → Selected: "In the write entry, before the commit gate, stripes and cell: if the shared total is over the budget, wait on a condvar that the flusher signals after each step, in slices, up to the wait limit; then write and step own file" — the research deadlock rule: never wait for budget while holding the gate.
- [Deadlock] Q: "Commit vs throttled writers?" → Selected: "A commit never waits for budget. The flusher releases the gate between steps, so an exclusive gate waiter (commit cut) gets it. Test: all writers throttled, a commit waiting; it ends within a time limit" — ROADMAP criterion 3.
- [Take-out] Q: "How is 'no take-out' checked?" → Selected: "Delete the cross-file path (the remains of `keep_budget` that step other files, if any) and add a test that greps the source for `open_files` removal outside release/destroy, or a structural test that no function takes another inode's cell while holding a cell" — criterion 1.
- [RSS] Q: "RSS cap value?" → Selected: "Fixed cap = budget + per-writer step + a measured fixed overhead (the planner sets it from a Phase 18/19 baseline run); the Docker test reads VmHWM from /proc/self/status" — criterion 4.
- [Waits] All new waits have a time limit and exit when the mount shuts down (unmount wakes the flusher and the throttled writers).

### Claude's Discretion
- Flusher pass interval (for example 100-500 ms when idle-scanning), background threshold ratio, exact condvar shape, option value parser.

### Deferred Ideas (OUT OF SCOPE)
- Phase 19 deferred items stay open (see `.planning/phases/19-fine-grained-locks/deferred-items.md`): the locked re-lookup in chunk-store `put`, WAL group commit, closed-file size change races, cross-directory rename ancestor check, a kept cell after a failed release (the flusher may retry it in this phase if it is cheap).

Also out of scope (CONTEXT `<domain>`): the worker pool (Phase 21), WAL group commit (deferred), chunk profiles (Phase 22).

Specific ideas (CONTEXT `<specifics>`): keep tests short; never hold whole files in RAM; do not grow `stress_harness.rs` without need, prefer a focused throttle test.
</user_constraints>

<phase_requirements>
## Phase Requirements

| ID | Description | Research Support |
|----|-------------|------------------|
| LOCK-02 | Per-file state stays in its table behind its own lock; no request takes another file's state out of `open_files` (the take-out pattern is gone, including `keep_budget`). | Q1: the take-out pattern is already gone; `keep_budget` steps only its own cell. Phase 20 renames it into the "pay" path, adds the source allow-list test and relies on the Cell-after-Cell rank panic. |
| LOCK-06 | The shared dirty budget is safe with N writers: a background flusher lowers it, a writer that is over the limit sleeps before it takes any lock, and every wait has a time limit. A test with all workers throttled while a commit waits does not deadlock, and peak RSS stays under the set limit. | Q2 flusher design, Q3 throttle and deadlock test, Q4 memory bound and the S7 flusher run, Q5 mount options, Q6 pitfalls. |
</phase_requirements>

## Summary

Phase 19 already removed the take-out pattern. Each open inode has one `FileCell` in a sharded map. No code takes a file's state out of a map. `keep_budget` steps only the writer's own locked cell [VERIFIED: crates/slicefs-cli/src/filesystem.rs:2695-2733]. So LOCK-02 needs no large code change. It needs a rename of the writer-side budget path, a source allow-list test for the four `cells.remove` sites, and the existing debug rank rule that panics on a second Cell lock [VERIFIED: crates/chunk-store/src/sync.rs:82, test `two_cells_panic_even_in_key_order` at sync.rs:552].

The real work is LOCK-06. Today nothing lowers the dirty bytes of other files. Add one flusher thread (new `flusher.rs`), per-cell `last_write` and `dirty_since` atomics, and a throttle that the FUSE `write` callback calls **before** `gate_shared()` [VERIFIED: crates/slicefs-cli/src/fuse_callbacks.rs:436]. The throttle must be a separate call, because both callers of the write handler (the FUSE callback and the harness `gated()` at tests/stress_harness.rs:472) take the gate before the handler runs. The throttle waits only while a flusher runs. Replay and the in-process tests have no flusher, so they keep the Phase 19 behavior (own-file step at once). The flusher takes the gate shared for one step at a time and never across a sleep. It must stop and join in the `destroy` callback **before** that callback takes the gate.

One fact shapes the flusher policy. Every overlay write moves the overlay generation [VERIFIED: crates/slicefs-cli/src/overlay.rs:179-185], and a step publishes only if the generation did not move [VERIFIED: filesystem.rs:2126]. So a flusher step on a file that a writer changes in the step's I/O phase is dropped. The dropped result is garbage until a GC. So, below the budget, the flusher must not step a file that was written in the last few hundred ms. Above the budget the writers are throttled, so their files are quiet and the steps publish.

**Primary recommendation:** Build `flusher.rs` with one thread (Weak handle, per-step gate, timed condvars), a `throttle_write()` admission call before the gate in the FUSE `write` callback and in a new harness `gated_write`, mount options with a small hand-written size parser, and a new focused test file `tests/flusher_tests.rs` with the deadlock test guarded by an abort watchdog. Measure the RSS baseline first (in-process S7 with serialize off, in Docker), then fix the cap.

## Architectural Responsibility Map

The usual web tiers do not apply. The tiers here are the threads and layers of the daemon.

| Capability | Primary Tier | Secondary Tier | Rationale |
|------------|-------------|----------------|-----------|
| Admission (throttle) | FUSE callback layer (`fuse_callbacks.rs::write`), before the gate | `SliceFsFilesystem::throttle_write` | It must run before any lock. The callback is the only place before `gate_shared()`. |
| Pay step after the wait | `write_in_cell` (cell held, I/O unlocked) | — | The Phase 19 own-file path; the cell is the writer's own. |
| Lowering the shared total | Flusher thread (`flusher.rs`) | `SliceFsFilesystem` step functions (`free_dirty`, `rebase_cell`) | Only a thread that is not a request can do cross-file work with no request waiting for it. |
| Candidate selection | Flusher, from per-cell atomics | `CellMap::all` | Reads `FileCell::dirty`, `last_write`, `dirty_since` with no cell lock. |
| Thread lifecycle | `mount.rs` (spawn after replay) | `fuse_callbacks.rs::destroy` (stop and join before the gate) | Same pattern as the commit timer, but the stop moves into `destroy`. |
| Option parsing and validation | `cli.rs` (clap `value_parser`) | `main.rs`/`mount.rs` (cross-field check, wiring to atomics) | Clap does single values; the `limit <= budget` check needs both values. |
| Memory bound check | In-process S7 flusher test in Docker | `bigwrite.sh` parallel mode (optional, by hand) | The daemon has one dispatch thread until Phase 21, so only the in-process run has parallel writers. |

## Project Constraints (from CLAUDE.md)

There is no `./CLAUDE.md` in the repository. The user's global instructions and memory give these rules:

- Avoid work with little or no value. Keep tests short (memory "Test time budget": short waits; Boost mount runs only by hand, on main or `ci/**`).
- Per-file daemon memory must be bounded, never O(file size) (memory "No whole file in RAM", #98). Tests must not hold whole files in RAM.
- Run Linux FUSE tests in OrbStack Docker (memory "Docker Linux FUSE testing"). No macOS mounts in this research.
- No store backward compatibility is needed (memory "No store backward compat").
- Never mention licensing in public GitHub text.
- Reversible technical choices: decide and report (memory "Delegates reversible choices"). The CLI options are a one-way door; CONTEXT already has the user's answer.
- Task rules for this research: no `until pgrep -f` loops; every wait has a time limit; no command that reads stdin (use `</dev/null`); no gh command that changes GitHub.

## Standard Stack

No new crate. Everything uses primitives that exist in the repository.

### Core
| Item | Where | Purpose | Why |
|------|-------|---------|-----|
| `chunk_store::sync::Mutex` + `Condvar` (parking_lot, ranked) | crates/chunk-store/src/sync.rs:286-489 | Flusher wake and writer wait | `Condvar::wait_for` takes the guard and suspends the rank entry while it waits [VERIFIED: sync.rs:475-480]. No poison. |
| `chunk_store::sync::rank::held()` | sync.rs:218 | Debug proof that the throttle holds no lock | The gate pushes a `Rank::Gate` entry [VERIFIED: filesystem.rs:1233-1237], so an empty stack proves no gate, stripe, cell, short or store lock. Empty in release builds. |
| `std::sync::atomic::{AtomicU64, AtomicUsize, AtomicBool}` | std | Per-cell `last_write`, `dirty_since`; config values; flags | The pattern of `FileCell::dirty` [VERIFIED: filesystem.rs:327-328]. |
| `std::thread::Builder` + `Weak<SliceFsFilesystem>` | std; commit_timer.rs:66-96 | Flusher thread | Same lifecycle as the commit timer. |
| clap `#[arg(long, value_name, default_value_t, value_parser)]` | crates/slicefs-cli/src/cli.rs:77-90 | Mount options | Existing style; `parse_compression` is the model for a value parser [VERIFIED: content.rs:196-212]. |

### Supporting
| Item | Purpose | When |
|------|---------|------|
| `libc::getrusage` / `/proc/self/status` | Peak RSS in tests | `peak_rss_mib()` exists [VERIFIED: tests/stress_harness.rs:1546-1557]; add a VmHWM reader for Linux. |
| `hdrhistogram` (already a dev dependency of the harness) | Throttle wait histogram in S7 | Only in the S7 flusher run. |

### Alternatives Considered
| Instead of | Could Use | Tradeoff |
|------------|-----------|----------|
| Hand-written size parser | `bytesize` / `parse-size` crate | Not worth a new dependency for 15 lines; no package audit needed. |
| Polling flusher with slices | Pure event-driven flusher | Idle/expire triggers need a timer anyway; a 100 ms pass with a kick condvar is simpler. |
| One global wait threshold | ZFS-style delay curve between 60% and 100% | CONTEXT locks "sleep when over the budget". A curve can come later (reversible). |

**Installation:** none.

## Package Legitimacy Audit

This phase installs no external package.

| Package | Registry | Age | Downloads | Source Repo | Verdict | Disposition |
|---------|----------|-----|-----------|-------------|---------|-------------|
| (none) | — | — | — | — | — | — |

**Packages removed due to [SLOP] verdict:** none
**Packages flagged as suspicious [SUS]:** none

## Answers to the six questions

### Q1. What remains of the take-out pattern and of cross-file work

**Take-out pattern: gone.** `open_files` has no hit in `crates/slicefs-cli/src` (grep this session). State lives in `FileCell { ino, transient, state: sync::Mutex<CellState>, dirty: AtomicUsize }` [VERIFIED: filesystem.rs:319-329], and the doc says "Requests lock it in place; no request takes the state out." [VERIFIED: filesystem.rs:317-318].

**`keep_budget` today** (filesystem.rs:2695-2733) is own-file only. Its doc says: "If it is above the budget, this file (and only this one, D-13: no other cell is locked) stores steps until the cost `max(added, REBASE_STEP)` is freed, the total is `REBASE_STEP` below the budget, or the file is clean. ... Moving work to other files is Phase 20 (flusher)." [VERIFIED: filesystem.rs:2697-2703]. `free_dirty` (2737-2745) steps the same locked cell `g`. The call site is the end of `write_in_cell` [VERIFIED: filesystem.rs:2689].

**Sites that remove a cell from the map** (all `self.cells.remove(`; each one locks only the cell it removes):

| Line | Enclosing function | Why it is allowed |
|------|-------------------|-------------------|
| 1072 | `finalize` (1065) | Last write-handle release of the same inode. |
| 1201 | `finalize_locked` (1198) | `test_destroy` only, no request runs. |
| 1470 | `reclaim_inode` (1465) | Unlink of the last link of this inode; drops a WR-04 kept cell. |
| 3259 | `setattr_size_logged` (3208) | The transient cell of a closed-file size change. |

[VERIFIED: grep this session plus Read of filesystem.rs:1040-1215, 1450-1480, 3240-3265.] `CellMap::remove` (filesystem.rs:586-591) is the only code that removes from the shard map.

**Code that locks many cells, one at a time:** `gc_start_mode` (filesystem.rs:1568-1577, exclusive gate), `commit_cut_begin` (2965-3010, exclusive gate), `test_destroy` (4387-4400). None is a request path. These hold one cell at a time.

**Stale text to fix:** tests/stress_harness.rs:1959-1973 says S7 is serialized "because `keep_budget` takes another file's state out". That is no longer true after Phase 19. ARCHITECTURE.md line 22 and 108 also describe the old state.

**How to check "no take-out site" robustly** (three layers):

1. **Dynamic, structural (exists):** the debug rank checker panics when a thread takes a second `Rank::Cell` lock: `Rank::Gate | Rank::Rename | Rank::Cell => true` [VERIFIED: sync.rs:82]. Every debug test that runs a write path, and the flusher thread, prove that no code locks another inode's cell while it holds a cell. Turn on `rank::require_gate(true)` on the flusher thread (as the harness does at tests/stress_harness.rs:1154-1161), so the flusher also proves that it takes cells only inside a gate hold.
2. **Source allow-list test (new, `tests/no_take_out.rs`):** `include_str!("../src/filesystem.rs")`. For each line with `self.cells.remove(`, find the nearest previous line that matches `^\s*(pub(\(crate\))? )?fn (\w+)` and assert the name is in `{finalize, finalize_locked, reclaim_inode, setattr_size_logged}` and that there are exactly 4 sites. Also assert: no `fn keep_budget` (renamed, see below), no `open_files` token, and in `flusher.rs` no `cells.remove(` except the kept-cell retire helper if the planner adds one (then add it to the allow-list with a comment). A grep for `open_files` alone proves nothing, because it has zero hits now.
3. **Rename:** rename `keep_budget` to `pay_budget` (or `own_budget_step`) and change its doc to "the writer pays for its own file after a throttle timeout, or when no flusher runs". The ROADMAP text "including `keep_budget`" is then checkable: the name is gone and the path is own-file by the rank rule.

### Q2. Flusher design

**Thread lifecycle.**

- New module `crates/slicefs-cli/src/flusher.rs`. Two entry points:
  - `pub fn run_flusher(fs: &SliceFsFilesystem)`: the loop. It returns when the stop flag is set. Tests run it in `std::thread::scope`, because the harness `Ctx` owns the file system by value [VERIFIED: tests/stress_harness.rs:379-380] and `fresh_fs()` helpers return `SliceFsFilesystem`, not `Arc`.
  - `pub fn spawn_flusher(fs: Weak<SliceFsFilesystem>) -> JoinHandle<()>`: for the mount. It upgrades the `Weak` once per pass, as `spawn_commit_timer` does [VERIFIED: commit_timer.rs:66-96], and ends when the upgrade fails.
- Spawn point in `mount.rs`: after the replay and `Arc::new(fs)` [VERIFIED: mount.rs:359-372], next to the commit timer (mount.rs:401-410). Not on a snapshot mount (read-only, no writes). Store the `JoinHandle` in the file system (`flusher_thread: sync::Mutex<Option<JoinHandle<()>>>`, `Rank::Short`), so `destroy` can join it.
- **Stop in `destroy`, before the gate.** The `destroy` callback takes the gate shared and then runs `test_destroy` [VERIFIED: fuse_callbacks.rs:62-67]. `test_destroy` runs `finalize_locked`, whose steps keep the cell locked and do not check the step token [VERIFIED: filesystem.rs:1194-1222, 4387-4400]. A flusher holds the gate shared too, so the gate does not keep it out. Order in the callback:
  1. `self.stop_flusher()`: set `stop`, `notify_all` on both condvars, take the handle, `join()`.
  2. `let _op = self.gate_shared();`
  3. `self.test_destroy()`.
  Do not join while you hold the gate: if the commit timer (still running until mount.rs:470) is queued for the exclusive gate, a flusher that waits for `gate.read()` waits behind it, and the commit waits for the destroy hold. That is a deadlock. Also call `stop_flusher()` after `mount()` returns (idempotent; covers a failed mount). `test_destroy` should `debug_assert!` that no flusher runs.
- A `running: AtomicBool` is set at loop entry and cleared by a drop guard at loop exit (also on a panic). The guard also wakes all throttled writers. The throttle reads `running` to decide whether to wait.
- The flusher thread calls `chunk_store::sync::rank::require_gate(true)` at entry.

**Shared control block** (new fields of `SliceFsFilesystem`):

```rust
// [ASSUMED] shape; names are the planner's choice
struct FlushCtl {
    state: sync::Mutex<FlushFlags>,   // Rank::Short; FlushFlags { stop: bool, kicked: bool }
    wake: sync::Condvar,              // the flusher waits here between passes
    freed: sync::Condvar,             // throttled writers wait here
    running: AtomicBool,
}
// config atomics (mount options; test hooks set them lower)
dirty_idle_ms: AtomicU64, dirty_expire_ms: AtomicU64,
throttle_wait_ms: AtomicU64, flush_pass_ms: AtomicU64,
```

`dirty_budget` and `dirty_limit` atomics exist already [VERIFIED: filesystem.rs:667-670, 745-746].

**Gate handling per step.** One step = `gate_shared()` → `lock_cell()` → one step → drop cell guard → drop gate → `freed.notify_all()`. Never hold the gate across the pass sleep, across a condvar wait, or across two files. Holding the gate for the whole step keeps the GC rule "a rebase never straddles `gc_begin`" (ARCHITECTURE.md line 219; `gc_start` takes the gate exclusive at filesystem.rs:1560). Check `stop` before each `gate_shared()`.

**Finding candidates cheaply.** Per step, call `CellMap::all()` [VERIFIED: filesystem.rs:595-597: one shard lock at a time, it clones the `Arc`s] and read atomics only:

- `FileCell::dirty` (exists).
- New `last_write: AtomicU64`: ms since a process epoch (`OnceLock<Instant>`). `write_in_cell` stores it after the apply (Relaxed).
- New `dirty_since: AtomicU64`: set in `CellGuard::sync_dirty` [VERIFIED: filesystem.rs:509-519] when `old == 0 && cost > 0` (store now), cleared when `cost == 0`. This is the Linux `dirtied_when` rule: the age resets only when the file becomes clean. Skip transient cells (`FileCell::transient`).

The open-for-write set is small, so a linear scan per step is enough. No heap.

**Pass logic** (pass interval 100 ms, discretion; the `kicked` flag ends the sleep early):

```
loop {
  wait on `wake` for flush_pass_ms, or until kicked/stop
  if stop: return
  budget-full mode: while total >= budget/2 or some cell.dirty >= limit:
      pick = largest dirty cell not skipped in this pass, with the hot rule below
      step_one(pick); notify writers; check stop
  idle/expire mode: for each cell with dirty > 0 and
      (now - last_write >= idle) or (dirty_since != 0 and now - dirty_since >= expire):
      step_one(cell)   // one step per cell per pass, until clean over later passes
  kept cells (WR-04): see below
}
```

**Hot rule (because of the generation drop).** A cell is hot if its `last_write` is inside the hot window (2 pass intervals). Below the budget, the flusher **never** steps a hot cell, in every mode (budget-full, idle, expire). If no quiet candidate is left, the flusher sleeps until the next pass. At or above the budget, hot cells are allowed: their writers are throttled, so the step publishes. After a step that published nothing (dropped or skipped), skip that cell for the rest of the pass. Count `flusher_steps` and `flusher_drops` (test hooks and a debug line).

Why "never" and not "prefer quiet": at the defaults, `pub const DIRTY_LIMIT: usize = 32 << 20;` (filesystem.rs:123) is exactly half of `pub const DIRTY_BUDGET: usize = 64 << 20;` (filesystem.rs:140), which is the background threshold. One busy file holds about its limit (doc at filesystem.rs:105-109; own step at 2677). So with a single writer (the bigwrite workload) trigger 3 stays true and there is no quiet cell. A fallback that steps the hot cell then gives a dropped step on every pass (filesystem.rs:2126). Worse, while the flusher holds the token, the writer's own limit step is skipped (`g.stepping.is_some()` in `rebase_cell`, filesystem.rs:1972), so the file grows toward the hard cap. The hot file's own writer already pays its limit steps; the flusher adds nothing there.

This narrows the locked wording "steps the largest dirty file first" to "the largest quiet dirty file first, below the budget" (Assumption A10, confirm in plan review). It also means a file that is written without pause is flushed by its own limit steps, not by the expire rule; expire acts in the quiet gaps of a file that is written slowly.

**`step_one(cell)`:**

```rust
// [ASSUMED] skeleton; the functions it calls are VERIFIED (filesystem.rs lines below)
let _gate = fs.gate_shared();                 // 1233
let mut g = fs.lock_cell(&cell);              // 968
if g.retired || cell.transient { return; }
if g.stepping.is_some() || g.cap_writers > 0 { return; }   // skip, do not wait
let r = fs.free_dirty(&mut g, FLUSH_STEP);    // 2737 -> rebase_cell 1970 / to_overlay 1716
// kept cell: retire it when clean (see WR-04 below)
drop(g); drop(_gate);
fs.flush_ctl.freed.notify_all();
```

`rebase_cell` already skips when `g.stepping.is_some() || g.cap_writers > 0` [VERIFIED: filesystem.rs:1970-1976], and the step runs its I/O with the cell unlocked under the token (`run_step`, filesystem.rs:457-465). The flusher must not call `wait_step` (filesystem.rs:472-492) while it holds the gate; skip instead.

**Step size.** `FLUSH_STEP` = 4 MiB [ASSUMED, discretion]: larger than `REBASE_STEP` (`pub const REBASE_STEP: usize = 1 << 20;`, filesystem.rs:128) to cut fixed cost per install, smaller than `FINAL_STEP` (`pub const FINAL_STEP: usize = 8 << 20;`, filesystem.rs:133) because a commit cut waits for the longest shared gate hold. Measure the commit `gate_wait` in the S7 flusher run and lower it if needed.

**Streams.** For a stream, `dirty_cost` counts only pending out-of-order writes [VERIFIED: filesystem.rs:172-177]. `free_dirty` turns such a stream into an overlay (`to_overlay`, filesystem.rs:1716). For the flusher, convert a stream only in budget-full mode when no overlay candidate is left, or in idle/expire mode. A stream with no pending bytes has cost 0 and is never a candidate (the commit cut flushes it).

**Interaction with the step token and CR-01.** The flusher uses the same token as writers. If it holds the token on cell X and a writer of X reaches the hard cap, that writer counts itself in `cap_writers` and waits for the flusher's step [VERIFIED: filesystem.rs:2533-2541, `cap_step` 1990-2001]. While `cap_writers > 0`, other overlay writers also wait for the step in flight, so the flusher's step publishes. The waiter's progress count moves with the flusher's store work, because `run_step` wraps the I/O in `chunk_store::with_progress` on the thread that runs it (filesystem.rs:461). The flusher never starts a step on a cell with `cap_writers > 0`: the writer at the cap does its own step first.

**WR-04 kept cell.** `finalize` leaves a dirty cell in the map with no write handle after a failed last release [VERIFIED: filesystem.rs:1065-1074]. Its cost stays in `dirty_total`. Recommendation (cheap, in scope by CONTEXT):
- Add `kept: AtomicBool` to `FileCell`; `finalize` sets it when it leaves the cell; `open_write` clears it.
- The flusher retries a kept cell at most once per `dirty_expire`: under the gate, lock it, and if `write_handles == 0 && !retired`, call `finalize(&cell, &mut g)`. It retires and removes the cell when clean. `finalize` is bounded (the cell is under the hard cap; steps of `final_step`).
- Do not append a `Release` record: there is no handle. Replay releases every open handle at the end of the log [VERIFIED: crates/slicefs-cli/src/oplog.rs:367-369].
- `finalize` can call `wait_step` with the gate held (as a release does). That is acceptable for this rare path; add `finalize` to the `cells.remove` allow-list only through its existing site.

### Q3. Throttle

**Exact place.** In `fuse_callbacks.rs::write`, before `let _op = self.gate_shared();` [VERIFIED: fuse_callbacks.rs:423-437]. Only `write` adds dirty bytes: `fallocate` returns ENOSYS [VERIFIED: fuse_callbacks.rs:809-826], and `setattr_size_open` changes size, not data (filesystem.rs:3332+). `fsync`, `flush` and `release` are never throttled; they free memory.

```rust
// fuse_callbacks.rs::write  [ASSUMED] shape
let admit = self.throttle_write();          // no lock held
let t_gate = std::time::Instant::now();
let _op = self.gate_shared();
...
let handled = handle_write_admitted(self, fh.0, offset, data, _flags.0, admit);
```

**API.** `handle_write_flags` (handlers.rs:331-348) and `handle_write` keep today's behavior: the writer pays the budget step at once when the total is over the budget. Replay (oplog.rs:326-336), all in-process tests and `dirty_budget_tests.rs` / `rebase_step_tests.rs:226` / `write_log_replay_tests.rs:686-822` use that path and must not change. Add `handle_write_admitted(..., admit: Admit)` and pass `pay: bool` down to `write_logged` → `write_in_cell`, where the renamed `pay_budget` runs only if `pay` (or the old path). The harness gets `Ctx::gated_write`, which calls `fs.throttle_write()` before `gated()`.

**Wait shape.**

```rust
// [ASSUMED] skeleton
pub fn throttle_write(&self) -> Admit {
    // First line: every debug write checks it, also on the fast paths.
    debug_assert!(chunk_store::sync::rank::held().is_empty(), "throttle with a lock held");
    let budget = self.dirty_budget.load(Relaxed);
    let total = self.dirty_total.load(Relaxed);
    if total >= budget / 2 { self.kick_flusher(); }
    if total < budget { return Admit { pay: false } }
    if !self.flush_ctl.running.load(Acquire) { return Admit { pay: true } } // Phase 19 path
    let deadline = Instant::now() + Duration::from_millis(self.throttle_wait_ms.load(Relaxed));
    self.throttled_now.fetch_add(1, Relaxed);           // test hook
    let mut st = self.flush_ctl.state.lock();
    let pay = loop {
        if self.dirty_total.load(Relaxed) < budget { break false; }
        if st.stop || !self.flush_ctl.running.load(Acquire) { break true; }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() { break true; }
        self.flush_ctl.freed.wait_for(&mut st, left.min(THROTTLE_SLICE)); // 50 ms slice
    };
    drop(st);
    self.throttled_now.fetch_sub(1, Relaxed);
    Admit { pay }
}
```

- Slices are needed. `dirty_total` also falls on any guard drop (a release, a writer's own step) with no notify [VERIFIED: filesystem.rs:509-519, 534-547]. The flusher's notify covers its own steps; the slice covers the rest.
- No lost wakeup for flusher steps: the flusher lowers the total (in the guard drop) before it takes `state` and notifies; the writer checks the total while it holds `state`.
- `SyncWait` (crates/chunk-store/src/pack_io.rs:114-154) is one-shot. Do not use it for this repeating signal.

**Shutdown wakeup.** `stop_flusher()` sets `stop` under `state` and calls `notify_all` on `freed` and `wake`. Throttled writers return with `pay = true`. They write and pay their own step. No EIO.

**"Write after the wait".** After the wait limit, `pay = true`. The write runs normally (gate, cell, apply, log), then `pay_budget` steps the writer's own file until it freed `max(added, REBASE_STEP)` or the total is `REBASE_STEP` below the budget (today's `keep_budget` bound, filesystem.rs:2704-2733). Each step does its I/O with the cell unlocked under the token. A failed step does not fail the write (WR-01 rule, filesystem.rs:2647-2653).

**Commit and GC.** A commit never waits for budget. `timed_commit` syncs the store, then takes the gate exclusive [VERIFIED: commit_timer.rs:44-63]. Throttled writers hold no gate. The flusher holds the gate shared for one step only. So the commit waits at most for the longest in-flight shared hold. With std `RwLock`, a new reader waits behind a queued writer on both platforms (S5 result, docs/research/multithreaded-request-handling.md line 209). So the flusher's next `gate_shared()` waits for the commit cut. That is correct and finite. GC start is the same (`gc_start_mode`, exclusive gate, filesystem.rs:1549-1560).

**Deadlock test shape** (criterion 3), new `tests/flusher_tests.rs`:

- Setup: logged fs (PerOp WAL so `timed_commit` works; `common::new_fs_with`), 4 seeded files of 4 MiB, committed, opened `O_RDWR`. `test_set_dirty_limit(2 MiB)`, throttle wait 30 s (so a writer that leaves early is a bug, not a timeout), flusher pass 20 ms. Start the flusher in the scope and pause it at once (`test_pause_flusher(true)`).
- Fill **with the default budget** (64 MiB): each file gets 64 KiB overlay writes until it has about 1.1 MiB dirty (4.4 MiB total). No file passes its 2 MiB limit, so no own step runs, and the total is far below the budget, so no pay step runs. Only then call `test_set_dirty_budget(4 MiB)`: the total is now above the budget and the next write of every writer throttles. If the budget is set first, the plain fill path pays at once (filesystem.rs:2704-2733) and no writer ever throttles. This is the pattern of rebase_step_tests.rs:226 (set the budget after the setup).
- Use the same order (fill, pause flusher, then lower the budget) in every throttle test of the Test Map.
- **Variant A (flusher paused):** `test_pause_flusher(true)`. Start 4 writer threads that call `gated_write`-style throttle + gate + handler. `wait_until(5 s, throttled_now == 4)`. Then run `timed_commit` on another thread and require it to end within 5 s while `throttled_now == 4`. This proves that throttled writers hold no gate. Then unpause; require all writers to end within 10 s, `dirty_total < budget`, and the content to match a block-version model after release.
- **Variant B (flusher parked inside a step, holding the gate shared):** use the rebase hook parker pattern of `lock_free_io.rs:78-104` so the flusher's first step parks in `step_io` (filesystem.rs:2104-2107) with the gate shared. Queue `timed_commit` (it waits for exclusive). Writers are throttled. Let the step go. Require: the commit ends, then the flusher's next pass frees budget, then all writers end. Everything inside one watchdog.
- **Watchdog:** a real deadlock cannot be unwound, and `deadlock-detect` does not see the std gate. Use the harness pattern: a thread with `recv_timeout(30 s)` that prints and calls `std::process::abort()` [VERIFIED: tests/stress_harness.rs:189-201].
- Use event waits (`wait_until(limit, cond)`, channels), not fixed sleeps. Assert order facts and generous upper limits only.

### Q4. Bound on peak memory

**Dirty bound with the throttle.** A writer passes the throttle only when `total < budget`. N writers can pass at the same moment, so `total <= budget + N × (write size + EXTENT_OVERHEAD)` while the flusher keeps up. `pub const EXTENT_OVERHEAD: usize = 64;` [VERIFIED: overlay.rs:21]. After a timeout, a paying writer frees at least what it added, so the total does not grow from that write. A per-file cost never passes the hard cap: `limit + limit / GROW_DIV + 2 * REBASE_STEP` [VERIFIED: filesystem.rs:1957-1960], with `pub const GROW_DIV: usize = 8;` [VERIFIED: overlay.rs:25]. At the default `pub const DIRTY_LIMIT: usize = 32 << 20;` (filesystem.rs:123) that is 32 + 4 + 2 = 38 MiB.

**RAM peak model** [ASSUMED: model, to be checked by the baseline run]:

```
peak ≈ base (process, caches, WAL queue)
     + budget + N × max write                    (dirty bytes)
     + (N + 1) × step copy                        (N writers' own steps + the flusher; <= FINAL_STEP each)
     + chunk store unsynced pack copies           (KEEP_CAP per pack in background sync)
     + allocator retention                        (glibc per-thread arenas)
```

`pub const KEEP_CAP: usize = 3 * KEEP_LIMIT;` with `pub const KEEP_LIMIT: usize = 8 << 20;` [VERIFIED: crates/chunk-store/src/retry.rs:44-48], so 24 MiB per pack. `pub const DIRTY_BUDGET: usize = 64 << 20;` [VERIFIED: filesystem.rs:140]. `pub const REORDER_WINDOW: usize = 64 << 20;` [VERIFIED: filesystem.rs:48]; pending stream bytes count in `dirty_cost`, so the budget covers them.

**Existing numbers.**
- S7 (in-process, serialized, old code d24da49): peak RSS 234 MiB of the whole test process, RSS start 42 MiB, after writes 214 MiB, cap 256 MiB not asserted (docs/research/multithreaded-request-handling.md lines 243-256). The sample was taken with getrusage `ru_maxrss`.
- bigwrite (daemon, one writer, release): VmHWM 114 MiB (1 GiB) and 161 MiB (4 GiB) (filesystem.rs:118-122 comment); P1 117 MiB, cap 256 MiB (scripts/docker-linux-test/bigwrite.sh:9-13).

**Where to measure (criterion 4).** The daemon has one dispatch thread until Phase 21. In a mount, parallel writer processes are still served one request at a time. So the primary run is the **in-process S7 run with serialize off and the flusher on**, in Docker:

- Add `s7_budget_flusher` next to `s7_budget_baseline` (tests/stress_harness.rs:1957-2113): same workload (4 threads, 1 GiB files, 1 MiB fill then the 1 MiB + 4 KiB random mix, fsync every 64 MiB, commit every 30 s), `serialize = false`, flusher running in the scope, writes through `gated_write`. Report VmHWM, throttle waits (count, max), timeouts (pay count), flusher steps and drops, max dirty, commit gate wait max, slowest write. Assert `VmHWM <= CAP_MIB` and the model check.
- Read VmHWM from `/proc/self/status` (`VmHWM:` line, kB) on Linux; keep `peak_rss_mib()` for macOS.
- Run: `S7_FILE_MIB=1024 S7_WRITE_MIB=128 scripts/docker-linux-test/run.sh harness s7_budget_flusher`. The `harness` mode builds in release and passes the `S7_*` variables [VERIFIED: scripts/docker-linux-test/run.sh, `harness)` case]. The script passes the test name as `"$@"` with `--exact`.
- Optional, by hand: a `BW_FILES=N` parallel mode in `bigwrite.sh` that runs N `rw.py` processes on N files and reads the daemon VmHWM with the existing `hwm()` (`/proc/$MP/status`, bigwrite.sh:35). It checks the daemon with the flusher, but not parallel dispatch.

**The numbers do not agree yet.** The model gives about 42 (base) + 64 (budget) + 4 × 1 (writes) + 5 × 8 (steps) + 24 (KEEP_CAP) ≈ 174 MiB. The only run with 4 writers (S7, old code) gave a peak of 234 MiB, and that run was serialized, so at most one step copy existed at a time. So about 60 MiB is not explained by the model, before parallel step copies exist. Candidates: allocator retention (glibc per-thread arenas), the check-phase read buffers and the model, more than one pack in background sync [all ASSUMED]. The S7 doc gives only 22 MiB of margin to 256 MiB (multithreaded-request-handling.md line 256). A cap cannot be fixed from the model.

**Proposed cap procedure.**
1. Wave 0, task 1 (no new code): rerun the existing `s7_budget_baseline` on the current code in Docker (`run.sh harness s7_budget_baseline`). It checks whether 234 MiB still holds after Phase 19. Record P_old.
2. Wave 0, task 2: run `s7_budget_flusher` in measure-only mode with the flusher off and serialize off. Record P0. A run with serialize off above the budget was never done on Phase 19 code (ROADMAP line 183 kept the Phase 19 tests below the budget), so treat it as a probe [ASSUMED safe: `keep_budget` is own-file only], not as a known-good baseline.
3. After the flusher lands, run `s7_budget_flusher` again: P1. Also run once with `MALLOC_ARENA_MAX=2` [ASSUMED knob] to see how much is allocator retention.
4. Fix `CAP_MIB` from P1, with a margin of at least 15% [ASSUMED]. Keep 256 MiB only if P1 <= 220 MiB. If P1 is higher, find the cause before the cap goes up. Write P_old, P0, P1 and the cap into the S7 section of docs/research/multithreaded-request-handling.md.

### Q5. Mount option parsing

- **No size parser exists.** The only value parsers are `parse_compression` and `parse_hash` in content.rs and `parse_backend_flag` in backend.rs (grep this session). Add `pub fn parse_size(s: &str) -> Result<usize, String>` next to `parse_compression` (content.rs:196). Accept a decimal integer with an optional binary suffix `K`, `M`, `G` (also `KiB`, `MiB`, `GiB`), case-insensitive; use `checked_mul`; reject an empty number, a fraction, and overflow. 15 lines, no crate.
- **Options** in `Cmd::Mount` (cli.rs:32-98), clap style of `commit_interval` (cli.rs:75-78):
  - `--dirty-budget <SIZE>`, `default_value = "64M"`, `value_parser = crate::content::parse_size`.
  - `--dirty-limit <SIZE>`, `default_value = "32M"`, same parser.
  - `--dirty-idle <SECS>`, `default_value_t = 5`, `value_parser = clap::value_parser!(u64).range(1..)`.
  - `--dirty-expire <SECS>`, `default_value_t = 30`, same range.
- **Validation:**
  - Each byte value `>= 4 * REBASE_STEP` (4 MiB) [ASSUMED: "a few rebase steps"]; this also rejects 0. Put this in `parse_size_min` or in the cross check.
  - `dirty_limit <= dirty_budget`: clap cannot check two fields in a value parser. Check in `main.rs` before `run_mount`, or at the top of `run_mount` (mount.rs:249-266), and return an error with both values.
  - Seconds: reject 0 (CONTEXT "reject 0"). No order rule between idle and expire.
- **Wiring:** `run_mount` gets a `DirtyConfig { budget, limit, idle, expire }` (it already has `#[allow(clippy::too_many_arguments)]`, mount.rs:248). Before `Arc::new(fs)`: `fs.set_dirty_config(cfg)`, which stores the four atomics (budget and limit exist at filesystem.rs:667-670). Keep `DIRTY_BUDGET` / `DIRTY_LIMIT` as the defaults and keep the `test_set_*` hooks.
- Tests: the CLI parse tests in cli.rs (pattern at cli.rs:196-280) for defaults, suffixes and rejects; a `run_mount`-level or `main`-level test for `limit > budget`.

### Q6. Risks and pitfalls

See "Common Pitfalls" below.

## Architecture Patterns

### System Architecture Diagram

```
 FUSE write request
        |
        v
 [throttle_write]  -- total < budget --------------------------+
   no lock held     \                                          |
   (rank stack empty)\- total >= budget, flusher running:      |
                       wait on `freed` (slices, <= wait limit) |
                       |-- total falls / stop / limit -------->|
        |                                                      v
        |                                         gate_shared (rank 1)
        |                                                      |
        |                                         cell lock -> apply -> log append
        |                                                      |
        |                                         own limit step (Phase 19, unlocked I/O)
        |                                         pay_budget step only if admit.pay
        |                                                      |
        |                                         drop cell, write out log, reply
        |
 kick (total >= budget/2)
        v
 [flusher thread] -- sleeps on `wake` (pass interval) -- checks stop
        |
        +-- pick from CellMap::all() + atomics (dirty, last_write, dirty_since)
        |      budget-full: largest (quiet first); idle; expired; kept (WR-04)
        v
   gate_shared -> lock cell -> skip if token/cap -> free_dirty (run_step: I/O unlocked)
        -> drop cell -> drop gate -> notify `freed`
        ^
        |  exclusive gate waiter (timed_commit / gc_start) gets the gate between steps
```

### Recommended Project Structure

```
crates/slicefs-cli/src/
├── flusher.rs          # NEW: run_flusher, spawn_flusher, pass logic, candidate pick
├── filesystem.rs       # FlushCtl fields, throttle_write, stop_flusher, per-cell atomics,
│                       # keep_budget -> pay_budget, kept-cell flag, test hooks
├── fuse_callbacks.rs   # write: throttle before gate; destroy: stop_flusher before gate
├── handlers.rs         # handle_write_admitted
├── cli.rs, content.rs  # options, parse_size
└── mount.rs            # DirtyConfig wiring, spawn after replay, stop after mount()
crates/slicefs-cli/tests/
├── flusher_tests.rs    # NEW: criteria 2 and 3
├── no_take_out.rs      # NEW: criterion 1 source allow-list
└── stress_harness.rs   # gated_write, s7_budget_flusher (ignored), fix stale S7 comment
```

### Pattern 1: Release every lock before any wait (deadlock rule)
**What:** A request never waits for budget, a commit or durability while it holds the gate, a stripe or a cell (ARCHITECTURE.md lines 372-374).
**When:** The throttle, the flusher between steps, the flusher stop.
**Check:** `debug_assert!(rank::held().is_empty())` at the throttle entry.

### Pattern 2: Background thread with a Weak handle and a stop flag
**What:** commit_timer.rs:66-96: loop, sleep in slices, check `shutdown`, upgrade `Weak` per pass.
**Change for the flusher:** a condvar instead of a 100 ms `sleep`, so a kick and a stop act at once; a drop guard clears `running`.

### Pattern 3: Copy, I/O, publish under the step token
**What:** Phase A copies under the cell lock, Phase B does the I/O unlocked, Phase C publishes if the generation did not move (filesystem.rs:2013-2040, 2118-2154). The flusher reuses it through `free_dirty`. It does not add a new step form.

### Anti-Patterns to Avoid
- **Throttle inside `handle_write_flags` or `write_logged`:** the gate is already held there.
- **Always-on throttle:** with no flusher (replay, in-process tests) every write over the budget would wait the full limit.
- **Joining the flusher under the gate:** deadlock with a queued commit.
- **Flusher waits for a step token with the gate held:** a commit cut then waits for a step of another thread. Skip the cell instead.
- **Waiting with no time limit:** every new wait uses `wait_for` with a deadline.
- **Using `SyncWait` for the budget signal:** it is one-shot.

## Don't Hand-Roll

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|-------------|-----|
| A new step form for the flusher | A flusher-only rebase | `free_dirty` → `rebase_cell` / `to_overlay` (filesystem.rs:2737, 1970, 1716) | Token, retire, drop and CR-01 rules are already in them. |
| Lock-order checks | Ad-hoc asserts | `chunk_store::sync` ranks and `require_gate` | The Cell-after-Cell panic is the LOCK-02 proof. |
| Condition waits | Spin loops with `sleep` | `sync::Condvar::wait_for` | Rank entry is suspended while waiting (sync.rs:475-480). |
| Peak memory in tests | A custom sampler | `/proc/self/status` VmHWM; counting allocator of `dirty_budget_tests.rs` for heap | Existing patterns. |
| Abort on deadlock | Join with no limit | Harness `watchdog()` (stress_harness.rs:189-201) | A real deadlock cannot unwind. |

**Key insight:** Phase 19 built every step primitive the flusher needs. Phase 20 adds a scheduler and an admission gate, not new I/O paths.

## Common Pitfalls

### Pitfall 1: Flusher steps dropped on a hot file
**What goes wrong:** the flusher copies, splices and stores a step, and the publish is dropped because a writer moved the generation. The work is garbage until a GC, and nothing is freed.
**Why:** every overlay `put` moves the generation (overlay.rs:179-185); `step_publish` checks it (filesystem.rs:2126).
**How to avoid:** the hot rule (below the budget, never step a cell written inside the hot window), skip a cell for the pass after a drop, count drops.
**Warning signs:** `flusher_drops` close to `flusher_steps` in the S7 run; GC reclaims much more than usual.

### Pitfall 2: Lost wakeup or long wakeup of throttled writers
**What goes wrong:** the total falls by a release or an own step, with no notify; writers sleep to the full limit.
**How to avoid:** waits in slices (50 ms) that re-check the total; the flusher checks under `state` and notifies after the decrease.
**Warning signs:** throttle waits of exactly the limit while `dirty_total < budget`.

### Pitfall 3: Flusher starves behind the exclusive gate
**What goes wrong:** with std `RwLock`, a queued commit blocks new readers; the flusher waits for the whole commit cut (gate hold max 171.8 ms in S7, docs line 250).
**How to avoid:** accept it (it is finite); keep steps short so the commit itself does not wait long; never hold the gate across a wait.
**Warning signs:** flusher lag spikes at commit times; commit `gate_wait` grows with `FLUSH_STEP`.

### Pitfall 4: Flusher and a writer at the hard cap (CR-01 counter)
**What goes wrong:** the flusher starts a step while a writer is at the cap, so the writer's step is dropped, or the flusher waits for the writer's step with the gate held.
**How to avoid:** skip a cell with `cap_writers > 0` or `stepping.is_some()` (rebase_cell does the first check). If a writer reaches the cap during a flusher step, it waits for that step, and the step publishes because overlay writers wait too (filesystem.rs:2533-2541).
**Warning signs:** `STEP_TRIES` EIO lines; writers at the cap that wait for many slices (`STEP_WAIT` 15 s, filesystem.rs:148).

### Pitfall 5: Destroy meets a flusher step
**What goes wrong:** `finalize_locked` runs locked steps with no token check while a flusher step is in its I/O phase; or the join deadlocks behind a queued commit.
**How to avoid:** stop and join in the `destroy` callback before `gate_shared()`.

### Pitfall 6: One dispatch thread until Phase 21
**What goes wrong:** in the daemon a throttled write stalls every FUSE request (reads too) for up to the wait limit.
**How to avoid:** start the flusher at half the budget, so writers rarely reach the budget; measure the slowest write with the optional bigwrite parallel run. Do not lower the wait limit to hide it.

### Pitfall 7: Timing-flaky tests on a loaded machine
**What goes wrong:** asserts like "the commit took less than 100 ms" fail at load average 15 (the Phase 19 runs had 15-17).
**How to avoid:** assert order and states (counters, `throttled_now == 4`) with `wait_until(5-10 s)`; only lower bounds on durations (`elapsed >= wait limit` for the timeout test); a watchdog abort for deadlocks. Test hooks set idle 200 ms, expire 500 ms, pass 20 ms, wait limit 300 ms.

### Pitfall 8: Stream conversion by the flusher
**What goes wrong:** `free_dirty` turns a stream with pending writes into an overlay. For an active sequential stream (reorder window) that ends streaming for the handle.
**How to avoid:** prefer overlay candidates; convert streams only when nothing else frees memory, or when idle/expired.

### Pitfall 9: Flusher panic leaves writers waiting
**What goes wrong:** writers wait the full limit for a flusher that is gone.
**How to avoid:** the drop guard clears `running` and wakes writers; then the throttle returns `pay = true` at once (Phase 19 path). `StepEnd` already clears the token on a panic (filesystem.rs:401-416).

### Pitfall 10: Option misuse
**What goes wrong:** `--dirty-limit` above `--dirty-budget`, or values below one rebase step, make every write a step.
**How to avoid:** the validation in Q5.

## Code Examples

The skeletons in Q2 and Q3 are `[ASSUMED]` shapes. The functions they call are verified at the cited lines: `gate_shared` (filesystem.rs:1233), `lock_cell` (968), `free_dirty` (2737), `rebase_cell` (1970), `run_step` (457), `wait_step` (472), `sync_dirty` (509), `CellMap::all` (595), `timed_commit` (commit_timer.rs:44), `spawn_commit_timer` (commit_timer.rs:66), `Condvar::wait_for` (sync.rs:475), `rank::held` (sync.rs:218), `rank::require_gate` (sync.rs:211).

Size parser:

```rust
// [ASSUMED] new code, next to content.rs:196
pub fn parse_size(s: &str) -> Result<usize, String> {
    let t = s.trim();
    let split = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let n: usize = num.parse().map_err(|_| format!("bad size {s:?}; use a number with K, M or G"))?;
    let mul: usize = match unit.to_ascii_uppercase().as_str() {
        "" => 1, "K" | "KIB" => 1 << 10, "M" | "MIB" => 1 << 20, "G" | "GIB" => 1 << 30,
        _ => return Err(format!("unknown size unit in {s:?}; use K, M or G")),
    };
    n.checked_mul(mul).ok_or_else(|| format!("size {s:?} is too large"))
}
```

## State of the Art

| Old Approach | Current Approach | When Changed | Impact |
|--------------|------------------|--------------|--------|
| Dirtier does writeback inline (Linux before 3.2) | Dirtier sleeps; flusher writes (Linux IO-less throttling; ZFS delay) | Linux 3.2 (2011) | The docs cite this (multithreaded-request-handling.md line 19). SliceFS moves from inline own-file steps to a flusher plus sleep. |
| `keep_budget` stepped another file (before Phase 19) | Own-file step only (Phase 19) | Phase 19 | Phase 20 adds cross-file work only on the flusher thread. |

**Deprecated/outdated:**
- tests/stress_harness.rs:1959-1973 "serialized until Phase 20 ... `keep_budget` takes another file's state out": outdated since Phase 19.
- ARCHITECTURE.md line 22 ("take-out pattern, keep_budget taking another file's state ... no flusher") and line 108: outdated text; update after Phase 20.

## Assumptions Log

| # | Claim | Section | Risk if Wrong |
|---|-------|---------|---------------|
| A1 | `FLUSH_STEP` 4 MiB is a good flusher step size | Q2 | Commit gate wait grows (too big) or install cost grows (too small). Reversible; measure. |
| A2 | Pass interval 100 ms, background threshold budget/2, throttle slice 50 ms, hot window 2 passes | Q2, Q3 | Lag or CPU use; reversible. |
| A3 | Minimum byte option 4 MiB (4 × REBASE_STEP) | Q5 | Costly to change later (public CLI); confirm in plan review. |
| A4 | RAM peak model, the 15% margin, and "keep 256 MiB only if P1 <= 220 MiB" | Q4 | The criterion-4 test fails or is too loose; the Wave 0 baseline settles it. |
| A5 | glibc per-thread arenas can raise VmHWM; `MALLOC_ARENA_MAX=2` as a diagnostic | Q4 | Only a diagnostic hint. |
| A6 | getrusage `ru_maxrss` on Linux equals VmHWM | Q4 | Small; read `/proc/self/status` directly anyway. |
| A7 | A WR-04 kept-cell retry through `finalize` is cheap enough (bounded by the hard cap) | Q2 | A slow store makes one flusher pass long; it still releases the gate between files. |
| A8 | "Every wait has a time limit" means the new waits (throttle, flusher sleep); gate waits stay bounded by the holders' own timed waits | Q3 | If the reviewer wants a timed gate acquire too, that needs a new gate API (std `RwLock` has no timed read). |
| A9 | The throttle-skipping `handle_write_flags` path keeps the Phase 19 behavior (own step when over budget) | Q3 | If instead tests should go through the throttle, many existing tests need a flusher. |
| A10 | Below the budget the flusher never steps a hot cell (narrows the locked "largest dirty file first" to "largest quiet dirty file first") | Q2 | If the user wants strict "largest first", the flusher wastes dropped steps on a hot file and blocks its own limit steps. Confirm in plan review. |
| A11 | A run with serialize off above the budget is safe on Phase 19 code | Q4 | Never run (ROADMAP line 183). If it fails, the P0 probe finds a Phase 19 bug before the flusher work. |

## Open Questions (RESOLVED)

- Q1 (flusher in the default harness runs): RESOLVED by 20-CONTEXT.md "Answers to the research open questions", [Harness]: the default `run()` does not start the flusher; a focused test does, and the harness gets only a small opt-in knob (plan 20-07 `S7_FLUSHER`).
- Q2 (throttle also counts the `Write` record copy): RESOLVED as the research recommends (no). CONTEXT has no separate answer; the copy is bounded by N x write size and freed before the reply, inside the bound of plan 20-07.
- Q3 (expire on a file that never becomes clean): RESOLVED by 20-CONTEXT.md A10: a file that is written all the time is not stepped by the flusher below the hard limits; its writer pays with its own steps (plan 20-05).

The original questions follow.

1. **Does the flusher run in the default harness runs (`serialize_off_target`)?**
   - What we know: those runs stay below the budget (tests/stress_harness.rs:1421-1437), so the flusher would only do idle/expire work. CONTEXT says "do not grow `stress_harness.rs` without need".
   - Recommendation: leave it open. The required harness changes are only `gated_write` and `s7_budget_flusher`. Add the flusher to `run()` only if the planner sees a gap that `flusher_tests.rs` does not cover.
2. **Should the throttle also count the `Write` record copy?**
   - What we know: `Op::Write { data: data.to_vec() }` copies each write (filesystem.rs:2461-2465) until the write-out.
   - Recommendation: no; it is bounded by N × write size and freed before the reply.
3. **Expire on a file that never becomes clean.**
   - What we know: with the `dirtied_when` rule, a busy file stays "expired". The hot rule applies to expire too, so the flusher steps it only in its quiet gaps (or at or above the budget).
   - Recommendation: accept it. A file written without pause is kept under its limit by its own steps.

## Environment Availability

| Dependency | Required By | Available | Version | Fallback |
|------------|------------|-----------|---------|----------|
| cargo / rustc | build, tests | ✓ (not on PATH; in `~/.cargo/bin`) | not probed | Use `~/.cargo/bin/cargo` or add it to PATH |
| Docker (OrbStack) | S7 flusher run, optional bigwrite | ✓ | server 29.8.2, 32 CPUs, 16 GiB | — |
| Linux `/proc` | VmHWM | in the container only | — | `peak_rss_mib()` on macOS (not the criterion) |

**Missing dependencies with no fallback:** none.

## Validation Architecture

### Test Framework
| Property | Value |
|----------|-------|
| Framework | Rust built-in test harness (`cargo test`), one binary per file in `crates/slicefs-cli/tests/` |
| Config file | none (features in crates/slicefs-cli/Cargo.toml:13-17: `lock-stats`, `deadlock-detect`) |
| Quick run command | `~/.cargo/bin/cargo test -p slicefs-cli --test flusher_tests --test no_take_out` |
| Full suite command | `~/.cargo/bin/cargo test --workspace` |
| Docker (criterion 4, by hand) | `S7_FILE_MIB=1024 S7_WRITE_MIB=128 scripts/docker-linux-test/run.sh harness s7_budget_flusher </dev/null` |

### Phase Requirements → Test Map
| Req / Criterion | Behavior | Test Type | Automated Command | File Exists? |
|-----------------|----------|-----------|-------------------|-------------|
| LOCK-02 / C1 | Only 4 allow-listed `cells.remove` sites; no `keep_budget`; no `open_files` | source test | `cargo test -p slicefs-cli --test no_take_out` | ❌ Wave 0 |
| LOCK-02 / C1 | No code takes a second cell lock | unit (exists) | `cargo test -p chunk-store two_cells_panic_even_in_key_order` | ✅ sync.rs:552 |
| LOCK-02 / C1 | Flusher takes cells only inside a gate hold, one at a time | integration (debug, `require_gate(true)` on the flusher thread) | every `flusher_tests` test | ❌ Wave 0 |
| LOCK-06 / C2 | Flusher lowers the total below budget/2 with no writer | integration | `cargo test -p slicefs-cli --test flusher_tests flusher_lowers_the_total` | ❌ |
| LOCK-06 / C2 | Idle file is flushed (cost reaches 0) | integration | `... an_idle_file_is_flushed` (idle 200 ms, wait_until 5 s) | ❌ |
| LOCK-06 / C2 | Expired file is flushed in the quiet gaps of a slow writer | integration | `... an_expired_file_is_flushed` (idle 10 s, expire 500 ms, pass 20 ms, hot window 40 ms; one write every 300 ms; `wait_until(5 s, published flusher steps on that cell >= 1)`) | ❌ |
| LOCK-06 / C2 | One hot writer below the budget: no flusher step on it, and its own limit steps still run | integration | `... a_hot_file_is_left_to_its_own_writer` (budget 8 MiB, limit 4 MiB, random writes for 1 s; assert flusher steps and drops on that cell == 0, writer rebase count > 0, cost <= hard cap) | ❌ |
| LOCK-06 / C2 | Flusher steps a quiet file, not a hot one, below the budget | integration | `... the_flusher_steps_the_quiet_file` | ❌ |
| LOCK-06 / C2 | A throttle with a lock held panics (debug) | integration | `... a_throttle_under_the_gate_panics` (`#[should_panic]` or `catch_unwind`) | ❌ |
| LOCK-06 / C2 | Writer over budget waits until the flusher frees, then writes | integration | `... a_writer_over_the_budget_waits_for_the_flusher` | ❌ |
| LOCK-06 / C2 | After the wait limit the writer writes and pays an own step (no error) | integration | `... a_throttled_writer_writes_after_the_limit` (limit 300 ms; assert `elapsed >= 300 ms`, timeout counter, own cost fell) | ❌ |
| LOCK-06 / C2 | Stop wakes throttled writers (wait limit 30 s, returns in < 5 s) | integration | `... stop_wakes_throttled_writers` | ❌ |
| LOCK-06 / C2 | No wait without a flusher (Phase 19 path unchanged) | integration (exists, must stay green) | `cargo test -p slicefs-cli --test dirty_budget_tests --test rebase_step_tests` | ✅ |
| LOCK-06 / C2 | Flusher thread starts, stops, joins; ends when the fs is dropped | unit in `flusher.rs` (pattern commit_timer.rs:99-213) | `cargo test -p slicefs-cli flusher::` | ❌ |
| LOCK-06 / C2 | Options: defaults, K/M/G, rejects (0, below 4 MiB, limit > budget) | unit (cli.rs, content.rs) | `cargo test -p slicefs-cli cli:: content::` | ❌ |
| LOCK-06 / C3 | All writers throttled while a commit waits: no deadlock (flusher paused; flusher parked in a step holding the gate) | integration + abort watchdog 30 s | `cargo test -p slicefs-cli --test flusher_tests all_writers_throttled_while_a_commit_waits` | ❌ |
| LOCK-06 / C4 | Bounded dirty total with 4 parallel writers and the flusher (short proxy) | integration | `... dirty_total_stays_bounded_with_parallel_writers` (assert sampled max <= budget + 4 × (write + 64) + REBASE_STEP; content check) | ❌ |
| LOCK-06 / C4 | VmHWM under the fixed cap with 4 parallel random writers on 1 GiB files | Docker, in-process, release, by hand | `scripts/docker-linux-test/run.sh harness s7_budget_flusher` | ❌ (ignored test) |
| Regression | Serialize-off harness (unchanged; flusher only if Open Question 1 says so) | integration (exists) | `cargo test -p slicefs-cli --test stress_harness serialize_off_target` | ✅ |

### Sampling Rate
- **Per task commit:** the quick run command (the two new binaries; each test under a few seconds).
- **Per wave merge:** `cargo test --workspace`.
- **Phase gate:** full suite green, plus one Docker `s7_budget_flusher` run with the result written into docs/research/multithreaded-request-handling.md (S7 Phase 20 result), before `/gsd-verify-work`.

### Wave 0 Gaps
- [ ] `crates/slicefs-cli/tests/flusher_tests.rs`: helpers (logged fs, seeded files, `wait_until`, watchdog, parker from lock_free_io.rs:78-104).
- [ ] `crates/slicefs-cli/tests/no_take_out.rs`.
- [ ] Test hooks on `SliceFsFilesystem`: `test_pause_flusher`, `test_set_throttle_wait`, `test_set_dirty_idle`, `test_set_dirty_expire`, `test_set_flush_pass`, `test_throttled_now`, `test_flusher_stats` (steps, drops, throttle waits, timeouts).
- [ ] Harness: `Ctx::gated_write`; `s7_budget_flusher` (ignored) with a VmHWM reader; fix the stale S7 comment.
- [ ] Baseline P_old: rerun the existing `s7_budget_baseline` in Docker on the current code (no new code).
- [ ] Probe P0: measure-only `s7_budget_flusher` with the flusher off and serialize off, before the flusher lands (Assumption A11).
- [ ] Throttle tests: fill with the default budget, pause the flusher, then lower the budget (see Q3).

## Security Domain

The daemon is local. The relevant concerns are availability and input validation.

### Applicable ASVS Categories

| ASVS Category | Applies | Standard Control |
|---------------|---------|-----------------|
| V2 Authentication | no | — |
| V3 Session Management | no | — |
| V4 Access Control | no (unchanged) | — |
| V5 Input Validation | yes | `parse_size` with `checked_mul`; clap ranges; the `limit <= budget` check |
| V6 Cryptography | no | — |

### Known Threat Patterns

| Pattern | STRIDE | Standard Mitigation |
|---------|--------|---------------------|
| A writer fills daemon memory (many files, random writes) | Denial of service | Budget + throttle + per-file hard cap; VmHWM cap test |
| A wait with no limit hangs the mount | Denial of service | Every new wait has a deadline; stop wakes waiters |
| Option overflow (`--dirty-budget 99999999G`) | Tampering | `checked_mul`, minimum and order checks |

## Sources

### Primary (HIGH confidence, read this session)
- crates/slicefs-cli/src/filesystem.rs: 40-200 (constants, `dirty_cost`), 300-600 (`FileCell`, `CellState`, step token, `CellGuard`, `CellMap`), 640-1000 (fields, test hooks, `lock_cell`), 1040-1300 (release, finalize, destroy steps, gate), 1450-1600 (reclaim, GC start), 1940-2160 (hard cap, `rebase_cell`, `cap_step`, `rebase_unlocked`, `step_publish`), 2400-2780 (write path, `keep_budget`, `free_dirty`), 2960-3010 (commit cut), 3240-3265, 3332-3380, 4380-4400.
- crates/slicefs-cli/src/overlay.rs:19-25, 170-186; commit_timer.rs:1-120; mount.rs:245-275, 330-485; fuse_callbacks.rs:1-70, 395-460, 805-827; handlers.rs:319-348; cli.rs:25-130; content.rs:196-212; oplog.rs:290-370; locks.rs.
- crates/chunk-store/src/sync.rs:1-130, 200-220, 455-495 (ranks, checker, Condvar); retry.rs:30-50; pack_io.rs:114-154; lib.rs:310.
- crates/slicefs-cli/tests/stress_harness.rs (gated, commit loop, watchdog, S7 baseline, serialize-off target, peak RSS); dirty_budget_tests.rs:104-175; rebase_step_tests.rs:1-40, 215-260; lock_free_io.rs helper list; tests/common/mod.rs.
- scripts/docker-linux-test/run.sh, bigwrite.sh, mt-linux.sh.
- .planning: 20-CONTEXT.md, ROADMAP.md (Phase 20), REQUIREMENTS.md (LOCK-02, LOCK-06), research/ARCHITECTURE.md, 19-REVIEW.md (fix status), deferred-items.md; docs/research/multithreaded-request-handling.md (S5, S7, Phase 19 result).

### Secondary (MEDIUM)
- Linux IO-less dirty throttling and ZFS dirty data delay, as summarized and linked in docs/research/multithreaded-request-handling.md line 19 (not fetched again this session).

### Tertiary (LOW)
- RSS cap number and the allocator notes (Assumptions A4-A6).

## Metadata

**Confidence breakdown:**
- Standard stack: HIGH. No new dependency; all primitives read this session.
- Architecture: MEDIUM-HIGH. The placement facts (gate before handler, destroy order, generation drop) are verified; the policy values are discretion.
- Pitfalls: HIGH for the code-derived ones (1, 3, 4, 5, 6, 9); MEDIUM for tuning.
- Memory cap: LOW until the Wave 0 baseline run.

**Research date:** 2026-11-22
**Valid until:** 2026-12-22 (code moves fast in this area; re-check line numbers after any change to filesystem.rs)
