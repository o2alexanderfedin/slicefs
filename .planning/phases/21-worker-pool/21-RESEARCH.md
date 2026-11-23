# Phase 21: Worker pool - Research

**Researched:** 2026-11-23
**Domain:** FUSE request dispatch on a thread pool (Rust, vendored fuser 0.18, Linux kernel FUSE and FUSE-T 1.2.x on macOS)
**Confidence:** HIGH for the code facts and the design; MEDIUM for the kernel and FUSE-T ordering facts (training knowledge, marked `[ASSUMED]`); LOW for the POOL-05 result (S3 did not scale past 2 threads).

<user_constraints>
## User Constraints (from CONTEXT.md)

### Locked Decisions

#### Worker count option (user answer, 2026-11-23)
- Mount option `--workers N`. Linux default: the CPU count / 2, at least 1, at most 8 (`clamp(available_parallelism / 2, 1, 8)`; the cap of 8 is an owner answer of 2026-11-23). An explicit `--workers N` may be higher (reject 0; no upper bound on the explicit value). Reason for the cap: the Phase 20 memory cap (CAP_MIB 320) was measured with 4 writers; each busy writer can add one own step and a glibc arena.
- macOS default: 1, until a measured throughput gain on FSKit (roadmap criterion 1; research S1 result "Decides" line). An explicit `--workers N` on macOS is allowed.
- `--workers 1` runs the handler inline on the reader thread, as today (no queue, no extra thread).
- **Reversibility:** costly — a public CLI option and a default; later releases must keep it or deprecate it.

#### Decided autonomously
- [Pool] Q: "fuser `n_threads` or own pool?" → Selected: "One fuser reader thread plus a SliceFS pool. The vendored fuser refuses `n_threads != 1` on non-Linux (`vendor/fuser/src/session.rs`), and POOL-01 says one reader" — reversible internal structure. The research checks how to move a request plus its `Reply*` object to a worker with the vendored fuser (owned request data, `Send` replies); a small vendored-fuser change is allowed if needed.
- [Pool] Q: "Queue shape?" → Selected: "A bounded queue (for example 2 x workers or a fixed 64); the reader blocks when it is full, so memory stays bounded" — internal; never hold whole files in RAM.
- [Order] Q: "Which requests need order?" → Selected: "No global order (S1: FUSE-T 1.2.9 and Linux accept replies out of order). The research lists requests that need special care: FORGET/BATCH_FORGET, INTERRUPT, INIT, DESTROY, and any request the kernel expects in order for one inode; those run inline on the reader or keep per-inode order if research shows a need" — research item.
- [Shutdown] Q: "Shutdown order?" → Selected: "Unmount/SIGTERM: stop taking requests, drain the queue, join the workers (each wait has a time limit), then `destroy()` (which runs the Phase 20 `shutdown`: flusher stop, then the gate)" — ROADMAP criterion 2.
- [Panic] Q: "Worker panic?" → Selected: "A panic in a worker aborts the process (`std::process::abort` from a catch or a panic hook for worker threads); no reply is sent for the half-done request; the next mount replays the write log. A test shows a remount after an injected worker panic gives the replayed state" — ROADMAP criterion 3.
- [Locks] Q: "Reply after locks?" → Selected: "Each handler returns its reply data; the worker sends the reply after the handler returns and all guards are dropped. In debug builds, assert no ranked lock is held at the reply (Phase 19 rank checker)" — criterion 2.
- [Tests] Q: "How do tests run with 4 workers?" → Selected: "Linux Docker CI runs the FUSE smoke, live checks and pjdfstest with `--workers 4`; macOS FSKit CI runs with the default (1)" — criterion 4.
- [Bench] Q: "How to show scaling?" → Selected: "A Docker Linux run of parallel I/O on different files at 1, 2, 4, 8 and 16 workers, with throughput AND peak memory (VmHWM) per worker count; if 16 scales and memory stays bounded, a later change can raise the default cap of 8, recorded in `docs/research/multithreaded-request-handling.md`; also one macOS CI run at 1 and 2 workers, recorded, to decide whether the macOS default can be raised later" — criterion 5; reversible.
- [Throttle] Q: "Is the 5 s throttle wait limit right?" → Selected (owner concern, 2026-11-23): "Lower `THROTTLE_WAIT` from 5 s to 200 ms, the per-call pause cap of Linux `balance_dirty_pages` (`MAX_PAUSE` = HZ/5); ZFS caps one write delay at 100 ms (`zfs_delay_max_ns`). After the limit the writer pays with a step of its own file, as today, so the memory bound does not change. A plan of this phase makes the change, checks that the flusher tests still pass (they set their own limit), and re-checks the Docker S7 run under CAP_MIB" — reversible internal constant.
- [Waits] Every new wait (queue push, drain, join) has a time limit or ends at unmount; no wait while a ranked lock is held (Phase 19/20 rules).

### Claude's Discretion
- Queue depth, worker thread names, the exact handoff type, how the CPU count is read.

### Deferred Ideas (OUT OF SCOPE)
- Raising the macOS default above 1: only after a measured FSKit throughput gain (a later decision on the Phase 21 numbers).
- WAL group commit; Phase 19 and 20 deferred items stay open.

Out of scope (Phase Boundary): WAL group commit (deferred), chunk profiles (Phase 22), the Phase 19/20 deferred items.

Specific ideas: keep tests short; never hold whole files in RAM; do not run macOS mounts locally (macOS mounts only in CI).
</user_constraints>

**Research note (orchestrator update, 2026-11-23, not part of CONTEXT.md):** the [Throttle] change is already merged into `develop` (PR #151: commit 997c8f5, merge 9861aa5), so it is not phase work any more. This branch (`feature/phase-21-worker-pool`) does not contain it yet; see Q9.

<phase_requirements>
## Phase Requirements

| ID | Description | Research Support |
|----|-------------|------------------|
| POOL-01 | One FUSE reader hands each request to a SliceFS worker pool. The number of workers is a mount option; with one worker the handler runs inline as today. | Q1 (no fuser change; replies are `Send`), Q2 (which ops stay on the reader), Q6 (`MountedFs` is `Send + Sync`, `Arc` share), Q8 (`--workers` parser, defaults, CI), Pattern 1 and 2, queue bound in bytes |
| POOL-02 | Unmount drains the queue and joins the workers before `destroy()`. Replies are sent after all locks are released. | Q3 (drain at the start of `MountedFs::destroy`, before `shutdown()`), Q5 (`serve` helper, debug assert, source-scan test), Pattern 3 |
| POOL-03 | A worker panic aborts the daemon; the write log replays at the next mount (no request is served after a half-done update). | Q4 (panic hook that aborts before the unwind; the inline path has the same gap today; child-process test with `test_set_mkdir_hook`) |
| POOL-04 | All tests and pjdfstest pass with 4 workers on Linux (Docker CI). On macOS FSKit CI, tests pass with the macOS default from SPIKE-03. | Q8 (`SLICEFS_WORKERS` through `run.sh` and the scripts), Validation Architecture, Pitfall 6 (Phase 19 windows) |
| POOL-05 | Parallel I/O on different files scales with the number of workers, measured on Linux, and the result is recorded. | Q7 (bench script at 1/2/4/8/16 workers with 16 clients, MB/s and VmHWM, trace lines on and off, rule for "scales", the per-worker memory term of the CAP_MIB rule) |
</phase_requirements>

## Summary

The vendored fuser already gives what the pool needs. Its `Filesystem` trait takes `&self` for every request method and is `Send + Sync + 'static`. Every reply type that SliceFS uses is `Send + 'static` (checked with a compile test, with a negative control). The reply sender clones an `Arc` of the channel, and a reply on the FUSE-T stream socket takes a global send lock. So one reader thread can move a request (its arguments copied) and its `Reply*` object to a worker, as the Phase 18 S1 probe did. No vendored-fuser change is needed. fuser calls `destroy(&mut self)` only after the reader thread has ended, so the drain and join go at the start of `MountedFs::destroy`, before `shutdown()`.

Two current behaviours must change in this phase, and both are also true with one thread. First, every callback sends its reply while the commit-gate guard `_op` is still alive, so "reply after all locks" is a restructure of all callbacks. Second, a panic on the fuser reader thread today ends the session with an error, fuser drops its `FilesystemHolder`, and the drop runs `destroy()`, which commits the half-done state. A `catch_unwind` cannot fix this, because the unwind first drops the `parking_lot` guards (no poison) and the `Reply` (whose drop sends EIO). The correct mechanism is a panic hook that aborts on request threads before the unwind starts.

The throttle limit is already 200 ms on `develop` (PR #151), so the phase branch must take `develop` first. The S7 maximum waits were 282-1015 ms with 0 timeouts at 5 s, so with 200 ms timeouts will happen. The dirty bound then rests on `pay_budget` (the writer frees about what it added, from its own file). With N workers, a throttled writer blocks only its own worker, and the flusher and the commit timer are separate threads, so no deadlock is possible. The worst effect is head-of-line blocking of up to 200 ms plus one own-file step when all workers are throttled.

Memory is the real limit on the worker count. The Phase 20 cap rule has a per-writer term of only one write (4 x 1 MiB). The own-step copies and the allocator arenas of the writers are inside the measured overhead, which was measured at 4 writers. Each busy worker can hold a request payload, one own-step copy (1 MiB for a rebase step, up to 8 MiB for a release step, plus the splice output) and a glibc arena. So `CAP_MIB` = 320 is valid only for 4 writers. The default cap of 8 workers and the POOL-05 VmHWM runs at 1, 2, 4, 8 and 16 workers measure this term.

**Primary recommendation:** First merge `develop` (PR #151) into the branch. Then add a fuser-free `pool.rs` (jobs are boxed closures, a queue bounded in slots and in bytes, std or `chunk_store::sync` primitives, no new crates). Route every overridden callback except `init` and `destroy` through one `run` helper that runs inline for N = 1. Take the gate only in a `serve` helper that returns the handler result and asserts that no ranked lock is held. Install an abort-on-panic hook for threads named `fuser-*` and `slicefs-worker-*`. Drain and join (limit 150 s, then abort) at the start of `destroy`. Default `--workers` is `clamp(available_parallelism / 2, 1, 8)` on Linux and 1 on macOS. The bench uses 16 client processes at every worker count and records MB/s and the daemon VmHWM.

## Architectural Responsibility Map

| Capability | Primary Tier | Secondary Tier | Rationale |
|------------|-------------|----------------|-----------|
| Read FUSE frames, parse, build `Reply*` | fuser reader thread (`vendor/fuser`) | — | One reader on both platforms (POOL-01); fuser owns the buffer and the channel |
| Copy arguments, hand off the job | `fuse_callbacks.rs` (`MountedFs`), on the reader thread | `pool.rs` | The borrowed `&[u8]`, `&OsStr` and `&Request` die when the callback returns |
| Bounded queue, workers, drain, join | `pool.rs` (new) | — | Must be testable in process, so no fuser types in it |
| Gate, handler, log record, reply data | Worker thread (`serve` + `handlers.rs`) | `filesystem.rs` | A std `RwLock` read guard cannot cross threads; the worker takes it |
| Reply send | Worker thread, after `serve` returns | fuser `ChannelSender` | Reply after all guards drop (POOL-02) |
| Throttle admission | Worker thread, before the gate | `filesystem.rs::throttle_write` | Phase 20 rule: no lock held while waiting |
| Abort on panic | Process-wide panic hook (installed by `run_mount`) | — | Must run before the unwind drops guards and replies |
| Worker count, defaults | `cli.rs` (clap) and `mount.rs` | CI scripts (`SLICEFS_WORKERS`) | Public option; scripts pass it into Docker |
| Dirty memory | Flusher thread (`flusher.rs`) | Writers (`pay_budget`) | Unchanged from Phase 20; not a worker |

## Project Constraints (from CLAUDE.md)

The repository has no `CLAUDE.md` of its own. These rules come from the user's global instructions and the project memory notes:

- Avoid work with little or no value (keep the pool small; no async runtime; no new crates).
- Per-file daemon memory must be bounded, never O(file size) (#98). The queue must be bounded in bytes, because a WRITE payload is copied.
- Test time budget: short waits in tests. Long mount runs (Boost) only by hand, on `main` or on `ci/**`.
- Linux FUSE tests run in OrbStack Docker (`scripts/docker-linux-test/run.sh`). FUSE-T cannot run there.
- macOS backend: FSKit only. Do not run macOS mounts locally; macOS mounts only in CI.
- No store backward compatibility is needed (no deployments).
- Reversible technical choices: decide and report; ask only for one-way doors. `--workers` is a public option (costly to reverse; the name and defaults are locked).
- Never mention licensing in public GitHub text.

## Standard Stack

### Core
| Library | Version | Purpose | Why Standard |
|---------|---------|---------|--------------|
| `std::thread`, `std::sync::{Mutex, Condvar}` or `chunk_store::sync::{Mutex, Condvar}` | Rust std (rustc 1.95 nightly local, 1.100 in Docker) / workspace `parking_lot 0.12` | Worker threads, the bounded queue, the drain wait | Already in the tree; the Phase 19 shim gives a rank and timing [VERIFIED: Cargo.toml:36-38 `parking_lot = { version = "0.12", features = ["arc_lock"] }`] |
| `std::panic::{set_hook, take_hook}`, `std::process::abort` | Rust std | Abort before the unwind | No library needed |
| `std::thread::available_parallelism` | Rust std | The Linux default `clamp(n / 2, 1, 8)` | Reads affinity and cgroup quotas on Linux [CITED: doc.rust-lang.org/std/thread/fn.available_parallelism.html] |
| clap | 4.6.0 (Cargo.lock), features `["derive"]` only | `--workers` parser | Existing CLI [VERIFIED: Cargo.toml:25 `clap       = { version = "4", features = ["derive"] }`] |
| fuser (vendored) | 0.18.0 with 3 local patches | Reader thread, `Reply*` | Existing; no change needed (see Q1) |

### Supporting
None. No new package.

### Alternatives Considered
| Instead of | Could Use | Tradeoff |
|------------|-----------|----------|
| Own bounded queue (about 80 lines) | `crossbeam-channel` | Not in `Cargo.lock`; it bounds by count only, and the queue must also be bounded in bytes. Would need the package audit. |
| One reader + pool | fuser `n_threads` with `clone_fd` | Refused on non-Linux [VERIFIED: vendor/fuser/src/session.rs:259-264 `if !cfg!(target_os = "linux") && n_threads != 1 {` ... `"n_threads != 1 is only supported on Linux"`]; two code paths; each fuser thread holds a 16 MiB + 4 KiB read buffer. Keep as a later Linux option only if the reader is the bottleneck. |
| Panic hook | `catch_unwind` around each job | Runs after the unwind: guards are released (no poison) and `ReplyRaw::drop` sends EIO. See Q4. |
| Panic hook | `panic = "abort"` in `[profile.release]` | Global (also the flusher and GC threads); not testable in process; the workspace has no `[profile]` section now (unwind default). |

**Installation:** none.

## Package Legitimacy Audit

This phase installs no external package. `crossbeam-channel` was considered and not chosen, so it was not checked.

| Package | Registry | Age | Downloads | Source Repo | Verdict | Disposition |
|---------|----------|-----|-----------|-------------|---------|-------------|
| (none) | — | — | — | — | — | — |

**Packages removed due to [SLOP] verdict:** none
**Packages flagged as suspicious [SUS]:** none

## Answers to the research questions

### Q1. Request and `Reply*` flow in the vendored fuser; the S1 probe; fuser change

Flow on the reader thread (`fuser-0`):

1. `Session::new` mounts, then runs `handshake()`: it reads INIT and calls `init(&mut self)` on the calling thread, before `run()`.
2. `Session::run` spawns one thread per channel, named `fuser-{i}` [VERIFIED: vendor/fuser/src/session.rs:293-294 `for (i, ch) in channels.into_iter().enumerate() {` / `let thread_name = format!("fuser-{i}");`]. With `n_threads` unset there is one: `fuser-0`.
3. `event_loop` reads one frame into its one buffer, builds `RequestWithSender { ch: ChannelSender, request: AnyRequest<'a> }` (borrowed from the buffer), and calls `dispatch`. DESTROY is answered here and ends the loop [VERIFIED: vendor/fuser/src/session.rs:536-538 `if let Ok(Operation::Destroy(_)) = req.request.operation() {` / `req.reply::<ReplyEmpty>().ok();` / `return Ok(());`]. ENODEV ends the loop with `Ok` (session.rs:551).
4. `dispatch_req` calls the `Filesystem` method with borrowed arguments (`&Request`, `&OsStr`, `data: &[u8]`) and a new reply object `Reply::new(unique, ReplySender::Channel(self.ch.clone()))`.
5. After the reader thread ends, `run()` joins it and calls `destroy()` [VERIFIED: vendor/fuser/src/session.rs:324-330 `let Some(filesystem) = Arc::get_mut(&mut filesystem) else {` ... `filesystem.destroy();`].

Can a request plus reply move to another thread? Yes:
- The trait: [VERIFIED: vendor/fuser/src/lib.rs:400 `pub trait Filesystem: Send + Sync + 'static {`]; request methods take `&self`; only `init` and `destroy` take `&mut self` (lib.rs:404, 410).
- Every reply type implements the crate trait `pub(crate) trait Reply: Send + 'static {` [VERIFIED: vendor/fuser/src/reply.rs:104]. A scratch crate outside the repo compiled `fn s<T: Send + 'static>()` for `ReplyAttr`, `ReplyEntry`, `ReplyData`, `ReplyDirectory`, `ReplyEmpty`, `ReplyOpen`, `ReplyWrite`, `ReplyCreate`, `ReplyStatfs`, `ReplyXattr` and `ReplyLock` with no error; the same file with `s::<std::rc::Rc<u8>>()` failed with "`Rc<u8>` cannot be sent between threads safely" [VERIFIED: cargo check, this session]. `ReplyDirectory` does not implement `Reply` but is `Send` by its fields.
- The sender: [VERIFIED: vendor/fuser/src/channel.rs:138-142 `#[derive(Clone, Debug)]` / `pub(crate) struct ChannelSender(Arc<DevFuse>, bool);` / `static STREAM_SEND_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());`]. On a device fd one `writev` sends a whole reply; on the FUSE-T socket `send_stream` takes `STREAM_SEND_LOCK` and loops (PATCHES.md item 2). So concurrent replies from workers do not interleave bytes.
- The request header: `Request` is `#[repr(transparent)]` over `fuse_in_header` and has no `Clone` (request_param.rs:8-12). Copy `uid()`, `gid()`, `pid()` as `u32` values before the hand-off.
- Borrowed data: `write` gets `data: &[u8]` (lib.rs:620-626) and `setxattr` gets `value: &[u8]`; names are `&OsStr`. The pool path copies them (`to_vec`, `to_os_string`). The inline path keeps the borrow (no copy, as today).

The S1 probe (`crates/slicefs-cli/examples/s1_probe.rs`, commits f0249f8, 93cf284, 5e737b0) is exactly this pattern: `Probe { tx: Mutex<Sender<Job>> }`, callbacks put `ReplyEntry`/`ReplyAttr`/`ReplyData` into a `Job` and send it to 4 workers that share `Arc<Mutex<Receiver<Job>>>`; workers reply. It ran on Linux (max_inflight 3, 16 inversions) and on FUSE-T 1.2.9 (max_inflight 2, 5 inversions), with no error.

**Fuser change: none.** One optional change is not needed: fuser could expose `Request` as `Clone`, but copying three `u32` values is simpler.

### Q2. FUSE ops that need care; which run inline on the reader

| Op | What happens today | Under the pool |
|----|-------------------|----------------|
| INIT | `handshake()` calls `init(&mut self)` on the mount thread, before `run()` | Stays inline (fuser does it). Spawn the workers in `init` or before `mount()`; see Open Question 5. |
| DESTROY | `event_loop` replies and returns; `run()` calls `destroy()` after the reader joins (session.rs:536-538, 324-330) | Not a pool job. `destroy` drains and joins first (Q3). On Linux, a normal (not `fuseblk`) mount likely gets no DESTROY: the reader ends on ENODEV [ASSUMED]. Both paths reach `destroy()`. |
| INTERRUPT | fuser answers ENOSYS itself [VERIFIED: vendor/fuser/src/request.rs:117-120 `ll::Operation::Interrupt(_) => {` / `// TODO: handle FUSE_INTERRUPT` / `return Err(Errno::ENOSYS);`] | Stays inline; no SliceFS code. The kernel then stops sending INTERRUPT for the connection [ASSUMED]. A queued request is not cancelled; this is the same as today. |
| FORGET / BATCH_FORGET | SliceFS does not override `forget`; the default is a no-op [VERIFIED: vendor/fuser/src/lib.rs:425 `fn forget(&self, _req: &Request, _ino: INodeNo, _nlookup: u64) {}`]; no reply | Stays inline (no work). If SliceFS adds lookup counts later, keep FORGET in order per inode. |
| Not-overridden ops (ioctl, poll, bmap, lseek, copy_file_range, readdirplus, ...) | fuser default replies ENOSYS | Stay inline; no gate. |
| getattr after write | A WRITE reply is sent after the apply and the log write-out | The handler returns after the apply, and the reply follows, so a GETATTR that the kernel sends after the WRITE reply sees the new size. A GETATTR that runs at the same time as a WRITE of another process can see the old size; the kernel guards its attribute cache against such a stale reply with an attribute version [ASSUMED]. TTL is 1 s [VERIFIED: crates/slicefs-cli/src/filesystem.rs:1910 `pub(crate) const TTL: Duration = Duration::from_secs(1);`]. |
| Per-inode order of WRITEs | Linux write-through sends WRITEs of one inode one at a time (S4: "write" pass `waiting` max 1) | No per-inode queue is needed on Linux. READs of one inode overlap (S4: up to 7 with writes); the cell lock serializes what must be serialized. |
| readdir handles | `opendir` uses an atomic counter [VERIFIED: crates/slicefs-cli/src/filesystem.rs:1925-1926 `pub(crate) fn alloc_dir_handle(&self) -> u64 {` / `self.next_fh.fetch_add(1, Ordering::Relaxed) + 1`]; `handle_readdir(fs, ino, offset)` is stateless by offset (handlers.rs:155-164) | Thread-safe. The kernel waits for the OPENDIR reply before it sends READDIR with that fh. |
| flush / release on one fh | `close(2)` sends FLUSH and waits for its reply; RELEASE is sent later, without a wait, when the last reference goes [ASSUMED] | FLUSH finishes before RELEASE is sent, so they cannot reorder. A RELEASE can run after a later OPEN of the same inode; Phase 19 cells are shared per inode and counted per handle, so this is safe (`multi_handle_posix.rs`). An UNLINK while a RELEASE is queued sees an open handle and keeps an orphan, which the RELEASE reclaims. |
| Release at unmount | RELEASE can be queued when the umount succeeds [ASSUMED] | This is why the drain must run queued jobs, not drop them (Q3). |
| FUSE-T (macOS) | One reader on a stream socket; S1 saw 2 in flight and out-of-order replies accepted | With the macOS default (1) nothing changes. With an explicit `--workers 2` the NFS client of FUSE-T may send WRITEs of one file in parallel [ASSUMED]; overlapping ones could reorder. Do not add a per-inode queue now; check content in the macOS bench run; list it as a risk before the macOS default is raised. |

**Recommendation:** Every callback that `MountedFs` overrides, except `init` and `destroy`, goes through the pool. One rule is simpler than a list of cheap ops; `access`, `opendir` and `releasedir` cost microseconds in a worker. Only fuser-internal ops (DESTROY, INTERRUPT, FORGET, not-overridden ops) stay on the reader, by construction. No per-inode ordering is needed on Linux.

### Q3. Shutdown: unmount, SIGTERM, the FUSE-T watcher, `destroy()`

The order of events today:

1. SIGTERM/SIGINT/SIGHUP: the handler writes one byte to a pipe; the `signals` thread calls `on_stop`, which is `try_soft_unmount` (mount.rs:448). It runs `fusermount3 -u`, `fusermount -u`, then `umount` [VERIFIED: crates/slicefs-cli/src/unmount.rs:63-67 `let programs: &[(&str, &[&str])] = &[` / `("fusermount3", &["-u"]),` / `("fusermount", &["-u"]),` / `("umount", &[]),`]. A busy mount refuses; a second signal exits at once with `128 + sig` and no final commit (signals.rs:85-88); the next mount replays the log.
2. Linux: after the unmount the reader gets ENODEV (or DESTROY) and ends. macOS: the unmount watcher sees the mount gone and ends the FUSE-T server; the socket EOF maps to ENODEV (PATCHES.md item 2), and the reader ends.
3. fuser joins the reader, then calls `destroy()` on the mount thread (session.rs:324-330).
4. `MountedFs::destroy` calls `shutdown()` [VERIFIED: crates/slicefs-cli/src/filesystem.rs:3406-3410 `pub fn shutdown(&self) -> bool {` / `self.shut_down.store(true, Ordering::SeqCst);` / `self.stop_flusher();` / `let _op = self.gate_shared();` / `self.test_destroy()`]. `test_destroy` assumes no request runs [VERIFIED: crates/slicefs-cli/src/filesystem.rs:5137 `// No request runs now: the steps keep the cell locked.`].
5. `run_mount` then joins the watcher, stops the flusher (a no-op now), the online GC, the commit timer and the old GC.

The new order in `MountedFs::destroy(&mut self)`:

1. `if let Some(pool) = self.pool.take() { pool.drain_and_join(DRAIN_WAIT) }`. The reader has ended, so no new job can come. `drain_and_join` closes the queue, lets the workers run every queued job, waits until all workers report exit, then joins them (each `join` returns at once, because the thread is at its end).
2. The flusher still runs during the drain. A queued write that is throttled still gets memory freed, and its wait ends at 200 ms anyway.
3. If `DRAIN_WAIT` passes: print which job kinds are still running, then `std::process::abort()`. `test_destroy` must not run beside a request, and abort leaves the crash-safe state (the write log replays at the next mount). `DRAIN_WAIT` must be longer than the longest bounded wait in one request: `STEP_WAIT_MAX` [VERIFIED: crates/slicefs-cli/src/filesystem.rs:258 `const STEP_WAIT_MAX: Duration = Duration::from_secs(120);`] plus a `STORE_WAIT` [VERIFIED: crates/chunk-store/src/lib.rs:310 `const STORE_WAIT: Duration = Duration::from_secs(10);`] plus the throttle. Recommend 150 s.
4. Then `self.shutdown()` as today (flusher stop and join, gate, `test_destroy`).

Replies of drained jobs after the connection is gone fail to send; fuser logs "Failed to send FUSE reply" and goes on (reply.rs `send_ll_mut`). That is harmless. Rust binaries ignore SIGPIPE by default, so a write to the closed FUSE-T socket gives EPIPE and does not kill the daemon [ASSUMED].

Error paths:
- If `Session::new` fails before the filesystem is inside the holder, the `MountedFs` value is dropped and `destroy()` does not run. So `Pool` needs a `Drop` that closes the queue and joins with the same limit (a safety net; an empty queue joins at once).
- If the reader panics: see Q4 (the hook aborts before fuser can call `destroy()`).

No vendored-fuser change is needed for the drain.

### Q4. Panic → abort, and the remount test

Why `catch_unwind` is wrong here:
- `ReplyRaw::drop` sends EIO for an unsent reply [VERIFIED: vendor/fuser/src/reply.rs:150-157 `impl Drop for ReplyRaw {` ... `"Reply not sent for operation {}, replying with I/O error",` ... `self.send_ll_mut(&ll::ResponseErrno(ll::Errno::EIO));`]. The unwind drops the reply before a catch sees the panic, so a reply is sent for the half-done request. The CONTEXT rule says no reply.
- The Phase 19 locks are `parking_lot` with no poison ("There is no poison: a lock is free again after a panic", chunk-store `sync.rs` header). The unwind releases the cell and stripe locks, and other workers then serve requests on the half-done state before the abort.

What happens today with one thread (also a POOL-03 gap): a panic in a callback ends `fuser-0`; `run()` returns `Err("event loop thread panicked")` [VERIFIED: vendor/fuser/src/session.rs:311-315 `let res = match thread.join() {` ... `return Err(io::Error::other("event loop thread panicked"));`]; the `Arc<FilesystemHolder>` drops, and `FilesystemHolder::drop` calls `destroy()` [VERIFIED: vendor/fuser/src/session.rs:96-99 `impl<FS: Filesystem> Drop for FilesystemHolder<FS> {` / `fn drop(&mut self) {` / `self.destroy();`]. So `shutdown()` commits the half-done update.

**Recommended mechanism:** one process-wide panic hook, installed in `run_mount` before `mount()`:

- It calls the previous hook first (the message and the backtrace go to stderr).
- Then, if the current thread is a request thread, it prints one line ("request thread panicked: abort; the write log replays at the next mount") and calls `std::process::abort()`. The hook runs at the panic point, before any drop, so the locks are still held, no reply is sent, and no other worker can take those locks.
- A request thread is one whose name starts with `fuser-` (the reader, also the inline N = 1 path) or with `slicefs-worker-` (the pool). The names come from fuser (session.rs:294) and from `pool.rs`. A name test needs no per-callback code.
- The flusher (`slicefs-flusher`), the GC and the commit timer keep the Phase 20 behaviour (a flusher panic wakes throttled writers). See Open Question 2.
- The main thread runs `destroy()`. A panic there already ends the process with no commit (the main thread panics out of `run_mount`), so no change.

**Test without a macOS mount (in process, a child process):** `crates/slicefs-cli/tests/pool_panic_tests.rs`.

- The test re-runs its own binary: `Command::new(std::env::current_exe()?)` with `--exact <child test name> --ignored --nocapture` and an env var such as `POOL_PANIC_CHILD=<store dir>`. The child test is `#[ignore]` and returns at once if the env var is missing.
- The child builds a store (as `crash_recovery_tests.rs::make_fs_per_op` does), creates file A, writes, fsyncs (the log is durable), installs the hook (the same `pool::install_abort_hook()` that `run_mount` calls), starts a `Pool` of 2, sets the existing hook [VERIFIED: crates/slicefs-cli/src/filesystem.rs:1278-1281 `/// Test hook: run \`hook\` in each mkdir, create and mknod after the new` / `/// entry is linked and before its record is appended, with its stripes` / `/// held (\`None\`: no hook).` / `pub fn test_set_mkdir_hook(&self, hook: Option<RebaseHook>) {`] to `panic!`, and submits a job that creates B and then writes the marker line `REPLY B` to stdout (a stand-in for the reply).
- The hook runs at the half-done point: B is linked, its record is not appended, the stripes are held (filesystem.rs:2875-2883).
- The parent asserts: the child ended by SIGABRT (`ExitStatusExt::signal() == Some(libc::SIGABRT)`); stdout has no `REPLY B`; then it loads the store and runs `oplog::replay_log` (as `run_mount` does) and checks that A has its content and B does not exist.
- A second child case does the same with the panic on a thread named `fuser-0` (the inline path).
- Keep each child under 5 s (test-time budget).

**Mount test (Docker Linux, `verify` mode, debug build):** a debug-only env var, for example `SLICEFS_TEST_PANIC_CREATE=2` read in `run_mount` under `#[cfg(debug_assertions)]`, sets the mkdir hook to panic on the second create. The script mounts with `--workers 4`, writes and fsyncs file A, creates B, and expects the daemon to exit with status 134 (SIGABRT). It runs `fusermount -uz`, remounts, and checks A and that B is absent. `verify-phases.sh` already has the kill -9 and remount pattern (Phase 05 blocks).

### Q5. "Reply after all locks released" with the rank checker

Today every callback sends its reply inside the gate hold, for example [VERIFIED: crates/slicefs-cli/src/fuse_callbacks.rs:71-74 `fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {` / `let _op = self.gate_shared();` / ... `match handle_getattr(self, ino.0) {`], and the `reply.attr(...)` inside the `match` runs while `_op` lives. All about 30 overridden callbacks have this shape.

The gate guard puts a rank entry on the thread-local stack [VERIFIED: crates/slicefs-cli/src/filesystem.rs:1741-1744 `pub fn gate_shared(&self) -> GateGuard<'_> {` / `let rank = sync::rank::enter(Rank::Gate);` / ...], and the checker exposes the stack [VERIFIED: crates/chunk-store/src/sync.rs:218 `pub fn held() -> Vec<(Rank, u64)> {`, empty in a release build per sync.rs:216-226]. The ranks are [VERIFIED: crates/chunk-store/src/sync.rs:37-53 `Gate = 1,` `Rename = 2,` `Stripe = 3,` `Cell = 4,` `Short = 5,` `Store = 6,`].

**Pattern:** one helper owns the gate.

```rust
/// Run one handler inside one shared gate hold; return its result with
/// every lock released. The caller sends the reply after this returns.
fn serve<T>(fs: &SliceFsFilesystem, f: impl FnOnce() -> T) -> T {
    let r = {
        let _op = fs.gate_shared();
        f()
    };
    debug_assert!(
        chunk_store::sync::rank::held().is_empty(),
        "reply with a lock held: {:?}",
        chunk_store::sync::rank::held()
    );
    r
}
```

`throttle_write` stays before `serve` for writes (it asserts the same empty stack, filesystem.rs:3288-3292).

Checks, in three parts:
1. **Dynamic (debug builds):** the assert above. It runs in every `cargo test` that calls `serve`, and in the Docker smoke and live checks, which use the debug binary (`run.sh smoke`/`verify` build `/target/debug/slicefs`). pjdfstest uses the release build, so it has no assert.
2. **Unit test:** `serve_panics_when_a_lock_is_held_at_the_reply` takes a cell or a `Rank::Short` shim lock inside `f` and leaks it (`std::mem::forget` of a guard is not possible for a marker; use a test-only `rank::enter(Rank::Short)` kept alive past `f`), then expects the panic with `catch_unwind`, as `a_throttle_with_a_lock_held_panics` does.
3. **Source scan (release too):** a test in the style of `no_take_out.rs` reads `include_str!("../src/fuse_callbacks.rs")` and asserts that `gate_shared(` appears only inside `fn serve` (and in `init`), so no callback holds the gate around its reply.

`fsync` and `fsyncdir` call `flush_wal()` inside the handler (handlers.rs:219-239), so the durability wait stays inside the gate hold, as today. That is the S5 rule's open part, and WAL group commit is deferred; do not move it in this phase.

### Q6. Thread-safety of `SliceFsFilesystem` callbacks; the 15 single-thread dependencies

- The trait methods take `&self` (Q1). `MountedFs` is a newtype over `Arc<SliceFsFilesystem>` [VERIFIED: crates/slicefs-cli/src/fuse_callbacks.rs:36 `pub struct MountedFs(pub std::sync::Arc<SliceFsFilesystem>);`], and it implements `Filesystem: Send + Sync`, so `SliceFsFilesystem` is `Send + Sync` (the code compiles). A job closure gets `Arc::clone(&self.0)`.
- Interior mutability is done (Phase 19 cells, stripes, atomics; Phase 20 flush control block). The Phase 19 harness already runs the handlers from free threads (`serialize_off_target`).
- Add a `pool: Option<Pool>` field to `MountedFs`; `destroy(&mut self)` can `take()` it.

The 15 items of the research doc table (plus 16), and their state:

| # | Dependency | State |
|---|-----------|-------|
| 1, 16 | `keep_budget` takes another file's state; budget undercount | Done (Phase 19 per-cell atomic cost; Phase 20 flusher, `pay_budget` own file only) |
| 2-7, 15 | Take-out pattern (EBADF, stale read, no-op fsync, duplicate alias state, skipped finalize, `expect` panic, unlogged write) | Done (Phase 19: one `FileCell` per inode, never taken out; `only_four_functions_remove_a_cell`) |
| 8 | Log order differs from apply order | Done (Phase 19: record appended in the lock hold of its apply; replay creates at the logged inode number) |
| 9-10 | Refcount and inode read-modify-write | Done (`modify_inode`, under shard locks) |
| 11 | Multi-step namespace operations | Done (stripes in rank order, rename mutex). Known windows stay open: size change of a closed file against unlink or setattr; no rename ancestor check (Phase 19 deferred items). With N ≥ 2 they become reachable; see Pitfall 6. |
| 12 | `getlk`/`setlk` no-ops | Not closed; wrong with one thread too; out of scope |
| 13 | Per-write test counters | Done: thread-local [VERIFIED: crates/slicefs-cli/src/filesystem.rs:285-295 `thread_local! {` ... `static LAST_REBASE_BYTES` ... `static WRITE_REBASE_BYTES` ... `static WRITE_BASE_BYTES` ... `static WRITE_MAX_COPY`] |
| 14 | `commit_cut` and GC invariant | Kept: workers take the gate shared for the whole request; the queue holds no gate |

The rows of `.planning/research/ARCHITECTURE.md` Part 1 that the table above does not name, checked in the code of this branch:

| ARCHITECTURE.md row | State in code |
|---|---|
| `write_alias`, `owner_fh`, the heir logic in `test_release` | Done: `grep -n 'write_alias\|owner_fh' crates/slicefs-cli/src/*.rs` finds nothing this session; handles share one cell per inode |
| `close_handle`: two separate `handles.lock()` calls | Done: no `close_handle` now; `take_handle` removes the fh in one lock (`self.handles.lock().by_fh.remove(&fh)`, filesystem.rs:1941-1942), and `uncount` decrements and checks the orphan in one lock under the inode stripe (filesystem.rs:1949-1960, read this session) |
| `read_root`: `readers` mutex held across `Reader::read` | Done: "The map lock is held only to clone or add the `Arc`; the per-inode reader is locked for the read only. If another read holds it, this read opens a private reader instead of waiting (D-07)." (filesystem.rs:2667-2670; `shared.try_lock()` at :2684) |
| `gc_start_mode`, `commit_cut_begin` iterate the cells under the exclusive gate | Kept, as designed: `let gate = self.gate_exclusive();` (filesystem.rs:2070) and "The exclusive gate is held: lock the cells one at a time." (filesystem.rs:3708). Safe with workers, because a worker holds the gate shared for its whole request |

Assumptions found in this research that the table does not list, all closed by this phase:
- The reply is sent inside the gate hold (Q5).
- A request panic leads to `destroy()` and a commit (Q4).
- `test_destroy` assumes no request runs (Q3: drain first).
- Per-request `eprintln!` ENTRY/EXIT lines take the global stderr lock. This is a cost, not a correctness problem (Q7).
- The `Logged` clock is thread-local (handlers.rs:105-122); it is correct, because one request runs wholly on one worker.

### Q7. Benchmark for POOL-05

**Linux (Docker, by hand):** a new script `scripts/docker-linux-test/pool-bench.sh <bin>`, started with `run.sh pool-bench` (release build, like `bigwrite`). Not in CI.

- Fixed client count: C = 16 processes, the same at every N, so only the worker count changes, and at N = 16 every worker can be busy at the same time (the memory term below needs that).
- For N in 1, 2, 4, 8, 16, and 3 runs each (a new store for each run):
  1. Before the mount, make 16 source files of 64 MiB each from `/dev/urandom` (distinct data, so dedup does not remove work; 1 GiB in total).
  2. Mount with `--workers N` (always explicit; the default in the container is 8). Wait for the mount.
  3. Write phase: 16 parallel `dd if=src_i of=$M/f_i bs=1M conv=fsync`. Aggregate MB/s = 1024 MiB / wall time.
  4. Unmount (the daemon must exit), remount with the same N (a cold page cache).
  5. Read phase: 16 parallel `dd if=$M/f_i of=/dev/null bs=1M`. Aggregate MB/s. Then `cmp` every file with its source (a correctness gate).
  6. Metadata phase: 16 processes, each creates 250 files of 4 KiB, then closes them. Files per second.
  7. A random-write phase that makes overlays (the case with own steps and release steps): 16 processes, each does 64 MiB of 1 MiB writes at random offsets into its own 64 MiB file, then closes it. Aggregate MB/s.
- Record per run: the commit, kernel, CPU count, rustc, load average before and after, the daemon VmHWM (`VmHWM` from `/proc/<pid>/status` just before each unmount, one value per mount: one after the write and random-write phases, one after the read phase), and the largest WRITE length the pool saw (a counter the pool prints at unmount). VmHWM is a required column, as for throughput (CONTEXT [Bench]).
- One extra run at N = 16 with `MALLOC_ARENA_MAX=2` in the daemon environment shows the allocator part (the same diagnostic as Phase 20).
- Trace lines: every request writes two `eprintln!` lines, and stderr takes one global lock. No switch exists today. Add a read-once env var (for example `SLICEFS_FUSE_TRACE=0`; default on, so the logs do not change) and run the N = 1 and N = 8 points with the lines on and off. Report both.
- Rule for "scales" (set before the runs; see Open Question 7): at N = 4, the median aggregate write MB/s or read MB/s is at least 1.5 times the N = 1 median, and every run has 0 content mismatches. Record N = 8 and N = 16 too, also if they are lower (S3 showed a fall at N = 8 for writes in process).
- Rule for a later rise of the default cap of 8 (record it; the change itself is a later decision): N = 16 gives a median MB/s at least 10 % above N = 8 in some phase, and the VmHWM at N = 16 is under the cap that the memory rule below gives for 16 writers.
- Time: about 1 GiB per phase at 100-200 MB/s is 5-10 s; 15 runs with remounts plus the arena run fit in about 20 minutes.
- Record the table (MB/s and VmHWM per N) in `docs/research/multithreaded-request-handling.md` (a "Phase 21 result" section), with the step-6 Decides line.

**Memory per worker and the CAP_MIB rule.** The Phase 20 rule is [VERIFIED: .planning/phases/20-dirty-budget-and-rebase-flusher/20-01-PLAN.md:95-100 `- B = 64: the dirty budget of the runs (\`DIRTY_BUDGET\`).` / `- N x W = 4 x 1 = 4: one write in flight per writer (the throttle bound).` / `- O = Pm - B - N x W: the measured fixed overhead (process, caches, kept pack copies, the writers' own step copies, allocator, model and check buffers).` / `- F = 8: one flusher step, its copy and its splice output (2 x \`FLUSH_STEP\`; ...` / `- CAP_MIB = B + N x W + O + F + R = Pm + F + R, rounded up to a multiple of 8.`], and the constant is `const CAP_MIB: u64 = 320;` with `Pm = 267` (crates/slicefs-cli/tests/stress_harness.rs:1984-1990). So the only explicit per-writer term is W = 1 MiB. "The writers' own step copies" and "allocator" are inside O, and O was measured at N = 4, so O also grows with N. Split per busy worker:

| Per busy worker | Size | Source |
|---|---|---|
| Request payload (the pool copy of a WRITE) | ≤ 1 MiB on Linux [ASSUMED: A4]; 0 extra inline | Pattern 1 |
| One own step: copy plus splice output (2 x the step, as F counts the flusher step) | rebase or pay step: about 2 x 1 MiB; release (finalize) or hard-cap step: up to 2 x 8 MiB | [VERIFIED: crates/slicefs-cli/src/filesystem.rs:128 `pub const REBASE_STEP: usize = 1 << 20;` and :133 `pub const FINAL_STEP: usize = 8 << 20;`] |
| A glibc arena per thread that keeps freed memory | not measured per thread; Phase 20: a P0 run with `MALLOC_ARENA_MAX=2` gave 230 MiB against 267 MiB, about 37 MiB at 4 writers | research doc Phase 20 result |

So with N busy workers the model is `VmHWM(N) ≈ fixed + B + Q + N x (W + S + A)`, where Q ≤ 8 MiB is the pool queue (fixed, not per worker). From the Phase 20 numbers, S + A + W is about 3-12 MiB per worker for writes with rebase or pay steps, and up to about 26 MiB per worker when all workers release large overlays at once (2 x 8 MiB + about 9 MiB of arena + 1 MiB). From P1 = 287 MiB at 4 writers, 8 busy workers give about 300-390 MiB, which can be above 320 [ASSUMED: extrapolation]. This is why the owner capped the default at 8, and why 320 must not be used as the cap for 8 or 16 workers.

Recommendation for the plan:
- Keep `CAP_MIB` = 320 as the 4-writer cap. Do not change it to fit a later run (the Phase 20 rule).
- Make the per-writer term explicit for N writers: `CAP(N) = Pm(N) + F + R`, rounded up to a multiple of 8, where `Pm(N)` is the largest VmHWM of 3 runs at N writers. Fix `CAP(8)` and `CAP(16)` from runs before any later change raises the default.
- Measure in two places. (1) The in-process S7 run with 4, 8 and 16 writers: `THREADS` is a fixed `const THREADS: usize = 4;` in `s7_budget_baseline` (stress_harness.rs, about line 1983), so add an `S7_THREADS` env value (default 4) and run `S7_SERIALIZE=0 S7_FLUSHER=1` at 4, 8 and 16. This also gives the S7 re-check under `CAP_MIB` at 4 writers with the 200 ms limit, which PR #151 did not do (it changed only the constant and the texts). (2) The mount bench above, VmHWM per N.
- If VmHWM at 8 writers is above a stated cap, the next lever is the budget or `FINAL_STEP`, not more workers; report it to the owner (the budget is an owner decision).

**macOS (CI, by hand):** a `workflow_dispatch` workflow `pool-bench-macos.yml`, also started on a push to `ci-pool/**` (not `ci/**`, which starts the long `macos-mounts.yml`), the same shape as `s1-overlap.yml`. Script `scripts/macos-mount-test/pool-bench.sh`: C = 4 copies of 64 MiB distinct files with fsync, remount, parallel read, `cmp`, at `--workers 1` and `--workers 2`, 3 runs each. Upload the logs. Record the table; the default stays 1 (deferred decision).

What limits the result (state it in the record): S3 in process gave Linux write 117.9 / 160.4 / 132.9 / 120.4 MB/s at N = 1/2/4/8, before the Phase 19 narrowing of the `readers` lock; Phase 19 lowered all global holds to p99 ≤ 48 us on Linux. The single pack appender remains. OrbStack's virtual disk does not flush a drive cache on fsync.

### Q8. `--workers` parsing, defaults, CI

Parser (checked in a scratch crate with clap 4.6.0 this session: `--workers 4` gives `Some(4)`, `--workers 0` gives `ValueValidation`, no flag gives `None`):

```rust
/// Request worker threads (default: half the CPUs, at least 1 and at most
/// 8, on Linux; 1 on macOS). 1 runs each request on the FUSE reader
/// thread. At least 1; an explicit value may be above 8.
#[arg(long, value_name = "N", value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..))]
workers: Option<usize>,
```

The existing pattern for `u64` options is [VERIFIED: crates/slicefs-cli/src/cli.rs:117 `#[arg(long, value_name = "SECS", default_value_t = 5, value_parser = clap::value_parser!(u64).range(1..=u64::MAX / 1000))]`]; the ranged parser above gives `usize` with no upper cap (CONTEXT: no cap).

Default:

```rust
pub fn default_workers() -> usize {
    if cfg!(target_os = "macos") {
        1
    } else {
        std::thread::available_parallelism().map_or(1, |n| (n.get() / 2).clamp(1, DEFAULT_WORKERS_MAX))
    }
}

/// Most workers by default on Linux (owner answer, 2026-11-23): CAP_MIB was
/// measured with 4 writers, and each busy worker adds memory (see the
/// Phase 21 result). An explicit --workers may be higher.
const DEFAULT_WORKERS_MAX: usize = 8;
```

`available_parallelism` reads the affinity mask and cgroup quotas on Linux, and can over-count when they cannot be read [CITED: doc.rust-lang.org/std/thread/fn.available_parallelism.html]. Call it once at mount. In the local OrbStack container (32 CPUs, no quota) the default is 8 (16 clamped); on a 4-vCPU GitHub runner it is 2 [ASSUMED: runner size]. Unit-test the pure part as a function of the CPU count (`workers_for_cpus(n)`: 1 → 1, 2 → 1, 4 → 2, 16 → 8, 32 → 8), so the test does not depend on the machine. The explicit value has no upper bound (CONTEXT).

Print one line at mount: `Workers: 4 (set)` or `Workers: 8 (default)`. The scripts check this line, so the logs prove the CI value.

CI: clap has no `env` feature, so pass the value through the scripts:
- `.github/workflows/ci.yml` Linux job: `env: SLICEFS_WORKERS: 4`.
- `run.sh`: add `-e SLICEFS_WORKERS` to the `docker run` arguments (it passes `-e BW_KEEP -e BW_QUIET_REF` the same way).
- `mt-linux.sh`, `verify-phases.sh` (`mnt` helper), `pjdfstest.sh`, `read-bench.sh`, `bigwrite.sh`: add `${SLICEFS_WORKERS:+--workers $SLICEFS_WORKERS}` to each `$B mount`, and `grep -q "Workers: $SLICEFS_WORKERS"` in the mount log when it is set.
- macOS: no change; the default 1 applies. One push to a `ci/**` branch runs `macos-mounts.yml` (about 1 hour) for POOL-04.

`run_mount` gets a new `workers: usize` argument (it has 15 arguments now). Keep the change small: add the field next to `dirty`, as the Phase 20 `DirtyConfig` did, or pass a plain `usize`.

### Q9. The Phase 20 throttle with N workers; the 200 ms limit

**Deadlock and starvation analysis.**

| Case | Who waits, holding what | Who must move | Can it move? |
|------|------------------------|---------------|--------------|
| All N workers throttled | Each worker, no ranked lock (asserted in `throttle_write`) | The flusher thread | Yes: it is not a worker; it takes the gate shared per step. Each wait also ends at 200 ms; the writer then pays with its own file. |
| All workers throttled, a commit waits for the exclusive gate | The commit timer thread | Shared holders | Throttled workers hold no gate; flusher holds end per step (`all_writers_throttled_while_a_commit_waits_*`). |
| Queue full, reader blocked in `submit` | The reader, no lock | Any worker | Yes: each running job ends within its own bounded waits; if none ends for `SUBMIT_WAIT` (150 s), abort (the reader cannot see the unmount while it waits). |
| Workers wait for a step token that the flusher holds | Workers, cell released (Phase 19) | The flusher step | Yes: the flusher never waits for a worker; `STEP_WAIT` slices with progress, at most 120 s. |
| Drain at unmount with throttled writers in the queue | `destroy` on the mount thread, no lock | The flusher (still running), or the 200 ms limit | Yes: the flusher stops only after the drain, inside `shutdown()`. |
| A slow `fsync` holds the gate shared | A commit waits; new requests wait behind the commit (writer-preferring std `RwLock`, S5) | The fsync | Yes, after the disk sync; latency only (WAL group commit is deferred). |

No deadlock is possible: no wait in a request holds a lock, and the two threads that free memory or cut a commit are not workers. The worst case is head-of-line blocking: with all N workers throttled, a queued READ waits up to 200 ms plus one own-file pay step. Do not reserve a worker for non-writes; record the largest queue wait in the bench instead.

**The 200 ms limit.**

**Status: done, not phase work.** The change is merged into `develop` in PR #151 (commit 997c8f5 "fix: lower the throttle wait limit from 5 s to 200 ms", merge 9861aa5). On `origin/develop` the constant is `const THROTTLE_WAIT: Duration = Duration::from_millis(200);` (filesystem.rs:163, read with `git show origin/develop:...` this session), and the PR also changed `README.md`, `cli.rs` and the research doc text. This branch still has the old value [VERIFIED: crates/slicefs-cli/src/filesystem.rs:157-159 `/// Most time a writer over the budget waits for the flusher (Phase 20` / `/// CONTEXT default); then it writes and pays with a step of its own file.` / `const THROTTLE_WAIT: Duration = Duration::from_secs(5);`], because it branched from 885015a. **The first task of the phase: bring `develop` into the branch** (merge or rebase), so the pool is built and measured with 200 ms. PR #151 did not re-run S7; the 4-writer re-check under `CAP_MIB` is part of the memory runs in Q7.

What the 200 ms limit means for the pool:

- **Flusher tests.** The tests that throttle with a running flusher set their own limit with `test_set_throttle_wait` (flusher_tests.rs lines 230, 331, 373, 462, 555, 610, 823, 1157, 1348). The tests that keep the default either do not throttle or call `throttle_write` with no flusher running; then the writer pays at once [VERIFIED: crates/slicefs-cli/src/filesystem.rs:3301-3302 `if !ctl.running.load(Ordering::Acquire) {` / `return Admit { pay: true };`]. So the pool work does not need test changes for the limit.
- **Head-of-line blocking.** With N workers, the longest time that queued requests wait behind N throttled writers is 200 ms plus one own-file step (with 5 s it would have been 5 s). This is also the bound that the drain at unmount sees for throttled jobs.
- **WR-01 early exit.** The code does not change: a pass that began in the wait and could not free still ends the wait (`no_room_exits`). With 200 ms the deadline often comes first (one `FLUSH_STEP` of 4 MiB can take longer than 200 ms; the S7 largest waits were 282-1015 ms), so `timeouts` becomes the usual exit. Both exits give `pay: true`. The WR-01 test sets 10 s, so it still tests the early exit.
- **S7 memory bound.** S7 P1 had 411-437 throttle waits per run and 0 timeouts at 5 s. At 200 ms the waits above 200 ms become timeouts, and the writer pays. `pay_budget` frees up to `min(total - target, max(added, REBASE_STEP))` from its own cell [VERIFIED: crates/slicefs-cli/src/filesystem.rs:3242-3253 `fn pay_budget(&self, g: &mut CellGuard<'_>, added: usize) -> Result<(), i32> {` ... `let target = budget.saturating_sub(REBASE_STEP);` ... `let cap = *cap.get_or_insert((total - target.min(total)).min(added.max(REBASE_STEP)));`]. The write just put `added` bytes into this cell, so the writer can free about what it added, and the total grows by about 0 per paid write. A write in stream mode adds no overlay cost, so it has nothing to pay and adds nothing. So the bound "budget + N x (write + overhead) + one step in flight" still holds, but now by the pay rule, not by "a writer passes only below the budget". The cost moves to the writer: more own-file steps on request threads, a longer slowest write. With N workers, N writers can pay at the same time, and each one holds its own step copy; this is the per-worker memory term of Q7.
- **Hot files.** "At the budget a hot file can be stepped, because its writers wait in the throttle." With 200 ms the writers return sooner and can drop a flusher step in flight (the generation changes). S7 P1 had 0 drops; record `flusher_drops` in the S7 runs of Q7.
- **Proportional pause.** Not now. The throttle is a condition wait: it ends as soon as the total is under the budget, so the 200 ms is a cap, not a fixed sleep. A `balance_dirty_pages`-style curve adds pauses between half the budget and the budget; that is the deferred Phase 20 item "A delay curve instead of one wait threshold". Do it only if the S7 runs of Q7 show one of these: VmHWM at 4 writers above `CAP_MIB`; a slowest write above 500 ms (the #108 bound) with timeouts in most throttle waits; or `flusher_drops` that grow with the writer count.
- **Text:** PR #151 already updated the comment, the `--dirty-budget` help, README.md and the research doc Phase 20 lines. The Phase 21 result section adds only the N-worker effect: the old limit "a throttled write also stops reads" is gone with N ≥ 2.

## Architecture Patterns

### System Architecture Diagram

```
 kernel /dev/fuse  or  FUSE-T socket
        |
        v
 [fuser-0 reader]  read frame -> parse -> fuser-internal ops answered here
        |                          (DESTROY, INTERRUPT=ENOSYS, FORGET no-op,
        |                           not-overridden ops = ENOSYS)
        |  overridden callback on MountedFs
        v
 [copy args]  WRITE/setxattr: payload to_vec; names to_os_string; uid/gid/pid
        |
        +-- workers == 1 --> run the job here (inline, borrowed data, no copy)
        |
        +-- workers >= 2 --> Pool::submit(weight, job)
                                |  blocks while slots or bytes are full
                                v
                     [bounded queue: <= 2N slots, <= 8 MiB payload]
                                |
                                v
                     [slicefs-worker-k]
                        write only: throttle_write (no lock; <= 200 ms)
                        serve: gate shared -> handler (stripes, cell,
                               apply + log append) -> log write-out
                               -> all guards dropped; debug: held() empty
                        reply.<kind>(...)  --> ChannelSender --> kernel / FUSE-T
                                |
 panic on fuser-* or slicefs-worker-*:  hook -> previous hook (message)
                                        -> abort (no unwind, no reply)
                                |
 unmount / SIGTERM -> reader ends (ENODEV or DESTROY)
        -> fuser joins reader -> MountedFs::destroy:
             pool.drain_and_join(150 s; at the limit: abort)
             -> shutdown(): stop flusher -> gate -> test_destroy (commit)

 Not workers: slicefs-flusher (frees dirty memory), commit timer (exclusive gate),
 online GC, signals thread, FUSE-T unmount watcher.
```

### Recommended Project Structure
```
crates/slicefs-cli/src/
├── pool.rs            # NEW: Pool (bounded queue, workers, drain_and_join, Drop),
│                      #      install_abort_hook(), default_workers(); no fuser types
├── fuse_callbacks.rs  # MountedFs { fs, pool: Option<Pool> }; run(); serve(); all callbacks restructured
├── cli.rs             # --workers
├── mount.rs           # resolve workers, print "Workers: N", install hook, pass into MountedFs
crates/slicefs-cli/tests/
├── pool_tests.rs          # NEW: queue bound, parallel run, drain, limits, destroy order
├── pool_panic_tests.rs    # NEW: child-process abort + replay
└── reply_after_locks.rs   # NEW: source scan of fuse_callbacks.rs
scripts/docker-linux-test/
├── pool-bench.sh          # NEW (by hand)
└── run.sh, mt-linux.sh, verify-phases.sh, pjdfstest.sh, ...  # SLICEFS_WORKERS
scripts/macos-mount-test/pool-bench.sh   # NEW (CI by hand)
.github/workflows/pool-bench-macos.yml   # NEW (workflow_dispatch, ci-pool/**)
```

### Pattern 1: A fuser-free pool with a queue bounded in slots and bytes

**What:** `Pool::new(n, name)` spawns `n` threads named `slicefs-worker-{k}`. `submit(weight, job)` pushes `(Box<dyn FnOnce() + Send>, weight)`. It waits (in 1 s slices, a warning after 10 s, no lock held, at most `SUBMIT_WAIT` = 150 s and then abort) while `len >= slots` or `bytes + weight > max_bytes`, except that a job is always admitted when the queue is empty (so one write larger than the byte cap gets through). A worker pops, notifies "not full", runs the job, and repeats until the queue is closed and empty. Each worker sets `rank::require_gate(true)` in debug builds, as the flusher does (flusher.rs:88), so a cell or stripe lock outside the gate panics.
**When to use:** workers ≥ 2.
**Memory:** queued payload ≤ 8 MiB, plus at most N payloads in workers, plus the existing reader buffer. `submit` waits in 1 s slices, with no lock held, up to `SUBMIT_WAIT` (150 s, then abort; Open Question 1). fuser asks for `max_write` 16 MiB with `FUSE_MAX_PAGES` (lib.rs:121-122, 232); the Linux kernel limits a request to its `max_pages` limit, by default 256 pages = 1 MiB [ASSUMED]. So at N = 4 on Linux the pool adds at most about 12 MiB, at N = 8 about 16 MiB, and at N = 16 about 24 MiB. The S7 margin under `CAP_MIB` was 33 MiB (P1 max 287 of 320), but only for 4 writers; above 4 the own-step and arena term of Q7 is larger than the pool term. The pool prints the largest WRITE it saw at unmount, so the plan can confirm the Linux value from the smoke log.

### Pattern 2: One `run` helper for inline and pool

```rust
impl MountedFs {
    /// Run a request job: on this (reader) thread with one worker, else in
    /// the pool. `weight` is the payload bytes the job owns (0 for most).
    fn run(&self, weight: usize, job: impl FnOnce() + Send + 'static) {
        match &self.pool {
            None => job(),
            Some(p) => p.submit(weight, Box::new(job)),
        }
    }
}

fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
    let fs = Arc::clone(&self.0);
    self.run(0, move || match serve(&fs, || handle_getattr(&fs, ino.0)) {
        AttrResult::Ok(attr) => reply.attr(&TTL, &attr),
        AttrResult::Error(e) => reply.error(Errno::from_i32(e)),
    });
}
```

`write` needs two branches, so the inline path keeps the borrowed `data` (no copy, "as today"): `None => write_job(&self.0, ..., data, reply)`, `Some(p) => { let d = data.to_vec(); let fs = Arc::clone(&self.0); p.submit(d.len(), Box::new(move || write_job(&fs, ..., &d, reply))) }`. `write_job` calls `throttle_write`, then `serve(... handle_write_admitted ...)`, then the reply.

### Pattern 3: Drain at the start of `destroy`

```rust
fn destroy(&mut self) {
    if let Some(pool) = self.pool.take() {
        if let Err(stuck) = pool.drain_and_join(DRAIN_WAIT) {
            eprintln!("[{}][FUSE] destroy: {stuck} worker(s) still busy after {:?}: abort; the write log replays at the next mount", ts(), DRAIN_WAIT);
            std::process::abort();
        }
    }
    let ok = self.shutdown();
    // ...
}
```

### Anti-Patterns to Avoid
- **`catch_unwind` around the job:** the unwind sends EIO through `ReplyRaw::drop` and releases the no-poison locks before the abort.
- **Taking the gate on the reader thread:** a std read guard cannot move to a worker, and a queued job would hold the gate (a commit would wait for the queue).
- **Waiting in `submit` with a lock held:** the reader holds nothing; keep it so.
- **A count-only queue:** with WRITE payload copies, 64 slots of up to 16 MiB break the memory rule.
- **Dropping queued jobs at unmount:** a queued RELEASE must still finalize before `test_destroy`.
- **fuser `n_threads` on Linux in this phase:** two code paths, and the drain would need a fuser change.

## Don't Hand-Roll

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|-------------|-----|
| Moving a request to a thread | A custom reply channel or a fuser fork | fuser `Reply*` objects (already `Send + 'static`) | Proven by the S1 probe on both platforms |
| Lock-order checking at the reply | A new checker | `chunk_store::sync::rank::held()` and `require_gate` | Phase 19 checker, used by the throttle already |
| Joining a thread with a time limit | Polling `JoinHandle::is_finished` in a sleep loop | A worker exit count under the queue mutex plus a `Condvar` wait with a deadline, then `join()` | `join()` has no limit; the count ends exactly when each worker leaves its loop |
| CLI range check | Manual validation after parse | `clap::builder::RangedU64ValueParser::<usize>::new().range(1..)` | Checked in a scratch crate with clap 4.6.0 |
| Abort on panic | `panic = "abort"` profile | A panic hook filtered by thread name | Testable; keeps the flusher behaviour |
| CPU count | Parsing `/proc/cpuinfo` or cgroup files | `std::thread::available_parallelism` | Handles affinity and cgroup quotas |

**Key insight:** fuser was built for this hand-off ("the filesystem methods may run concurrent by spawning threads", session.rs:241-243). The real work is in SliceFS: where the gate lives, when the reply is sent, and what a panic may leave behind.

## Common Pitfalls

### Pitfall 1: The reply is still inside the gate hold
**What goes wrong:** a callback is moved to the pool but keeps `let _op = self.gate_shared();` above its `match`, so the reply is sent with the gate held; a slow reply write (FUSE-T socket) holds up a commit.
**How to avoid:** take the gate only in `serve`; the source-scan test fails on any other `gate_shared(` in `fuse_callbacks.rs`.
**Warning signs:** the debug assert "reply with a lock held" in the Docker smoke log.

### Pitfall 2: A panic hook that also catches test panics
**What goes wrong:** the hook is installed in a test binary and aborts on a test thread (test threads are named after the test, not `fuser-`), or it aborts on the flusher.
**How to avoid:** match names `fuser-` and `slicefs-worker-` only; install the hook in `run_mount` and in the child-process test, never in a shared test helper.

### Pitfall 3: The drain runs after `stop_flusher`
**What goes wrong:** queued writes over the budget can no longer get memory freed; they wait to their limit and pay, so unmount is slower, and a step-token wait could meet a stopped flusher.
**How to avoid:** drain first, then `shutdown()` (which stops the flusher). A test checks the order.

### Pitfall 4: The write payload is copied on the inline path
**What goes wrong:** N = 1 is no longer "as today"; every write costs a copy.
**How to avoid:** the two-branch `write` (Pattern 2).

### Pitfall 5: The panic test hangs or takes long
**What goes wrong:** a child that does not abort waits forever; a debug build of a large store setup is slow.
**How to avoid:** the parent waits for the child with a limit (kill at 20 s); the child uses a tiny store; keep the whole test under 5 s.

### Pitfall 6: Phase 19 windows become reachable
**What goes wrong:** with one dispatch thread, a size change of a closed file could not run beside an unlink or a setattr of the same inode. With N ≥ 2 it can (Phase 19 deferred items 19-09 and 19-10). On Linux the kernel likely serializes these per inode (the VFS inode lock for truncate, unlink and setattr) [ASSUMED]; on FUSE-T it is not known.
**How to avoid:** keep them deferred (CONTEXT); run pjdfstest with 4 workers (it runs mostly one operation at a time); name the windows in the Phase 21 result as open risks.

### Pitfall 7: The default worker count differs per machine
**What goes wrong:** the local OrbStack container has 32 CPUs and no quota, so a mount without `--workers` runs 8 workers (16 clamped to 8); CI passes 4, and a 4-vCPU runner default is 2. Memory and lock-wait numbers then differ.
**How to avoid:** the bench always passes `--workers` explicitly, and the S7 runs set `S7_THREADS`; the mount prints the value.

## Code Examples

### Abort hook
```rust
// Source: this research; std::panic::set_hook / take_hook semantics (std docs).
pub fn install_abort_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        prev(info);
        let name = std::thread::current().name().unwrap_or("").to_owned();
        if name.starts_with("fuser-") || name.starts_with("slicefs-worker-") {
            eprintln!("request thread {name} panicked: abort; the write log replays at the next mount");
            std::process::abort();
        }
    }));
}
```

### Drain with a time limit
```rust
// Inside Pool (queue state under one mutex: jobs, bytes, closed, exited).
pub fn drain_and_join(mut self, limit: Duration) -> Result<(), usize> {
    let deadline = Instant::now() + limit;
    let mut q = self.shared.q.lock();
    q.closed = true;
    self.shared.not_empty.notify_all();
    while q.exited < self.workers.len() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(self.workers.len() - q.exited);
        }
        self.shared.done.wait_for(&mut q, left.min(Duration::from_secs(1)));
    }
    drop(q);
    for h in self.workers.drain(..) {
        let _ = h.join(); // each thread is at its end; a panic already aborted
    }
    Ok(())
}
```

### Child-process panic test (outline)
```rust
#[test]
fn a_worker_panic_aborts_and_the_remount_replays() {
    let dir = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "panic_child", "--ignored", "--nocapture"])
        .env("POOL_PANIC_CHILD", dir.path())
        .stdin(std::process::Stdio::null())
        .output().unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(out.status.signal(), Some(libc::SIGABRT));
    assert!(!String::from_utf8_lossy(&out.stdout).contains("REPLY B"));
    // load the store, run oplog::replay_log, check: A has its content, B is absent
}
```

## State of the Art

| Old Approach | Current Approach | When Changed | Impact |
|--------------|------------------|--------------|--------|
| One fuser dispatch thread; safety by single thread | Phase 19 locks + Phase 20 flusher; Phase 21 pool | 2026-11 | Requests on different files run in parallel |
| Throttle wait limit 5 s | 200 ms (Linux `MAX_PAUSE`) | PR #151 on `develop` (2026-11-23) | A writer over the budget pays sooner; the S7 4-writer re-check is in the Q7 memory runs |
| fuser `n_threads` (Linux only) | One reader + SliceFS pool | This phase | One code path on Linux and macOS |

**Deprecated/outdated:**
- The research doc text "a throttled write also stops reads and all other requests for up to the wait limit (5 s in Phase 20; 200 ms after it)" (Phase 20 result, Limits): true only with one worker after this phase.

## Assumptions Log

| # | Claim | Section | Risk if Wrong |
|---|-------|---------|---------------|
| A1 | A normal (not `fuseblk`) Linux FUSE mount gets no DESTROY; the reader ends on ENODEV | Q2, Q3 | Low: both paths reach `destroy()` |
| A2 | The kernel stops sending INTERRUPT after an ENOSYS reply | Q2 | Low: fuser answers INTERRUPT inline either way |
| A3 | `close(2)` waits for FLUSH; RELEASE is sent later, without a wait; a RELEASE can be queued when the umount succeeds | Q2, Q3 | Medium: if RELEASE were synchronous, the drain would have less to do (no harm) |
| A4 | The Linux kernel limits one WRITE to 1 MiB by default (`max_pages` limit 256) | Pattern 1 | Medium: a larger WRITE raises the pool memory; the unmount counter shows the real value |
| A5 | The kernel ignores a stale GETATTR reply that races a WRITE (attribute version) | Q2 | Low: TTL is 1 s; same as with one thread from two processes |
| A6 | FUSE-T's NFS client may send parallel WRITEs of one file | Q2 | Medium for a later macOS default above 1; none for this phase (default 1) |
| A7 | Rust binaries ignore SIGPIPE, so a reply to a closed FUSE-T socket gives EPIPE | Q3 | Medium: if wrong, a drained reply could kill the daemon at unmount; a smoke on macOS CI would show it |
| A8 | The Linux VFS serializes truncate, unlink and setattr of one inode, so the Phase 19 windows stay closed on Linux | Pitfall 6 | Medium: pjdfstest with 4 workers is the check |
| A9 | GitHub `ubuntu-latest` has 4 vCPUs (default 2 workers) | Q8 | Low: CI passes `--workers 4` |
| A10 | The 1.5x rule at N = 4 is the right bar for "scales" | Q7 | Medium: needs the owner's confirmation before the runs |
| A11 | 8 busy workers give a VmHWM of about 300-390 MiB (extrapolated from P1 = 287 MiB at 4 writers, the step sizes and the Phase 20 arena run) | Q7 | Medium: the runs replace this number; it only explains why 320 is a 4-writer cap |
| A12 | About 9 MiB of glibc arena per thread (37 MiB / 4 from the Phase 20 `MALLOC_ARENA_MAX=2` run) | Q7 | Low: the arena run at N = 16 measures it |

## Open Questions

1. **What does the reader do when the queue stays full?**
   - What we know: the reader holds no lock; each running job ends within its own bounded waits (throttle 200 ms, step waits ≤ 120 s, store waits 10 s).
   - What's unclear: whether the wait "ends at unmount" ([Waits]). It does not: while the reader is blocked in `submit`, it does not read the channel, so it never sees ENODEV or DESTROY, `destroy()` never runs, and the drain limit never starts. A worker that is stuck for ever with a full queue would hang the unmount for ever.
   - Recommendation (meets [Waits]): give `submit` the same limit and action as the drain. Wait in 1 s slices with no lock held, print a warning after 10 s, and at `SUBMIT_WAIT` = 150 s with no free slot, print which jobs run and `abort()`; the write log replays at the next mount. Each wake-up from a finished job restarts nothing: the limit counts from the start of this `submit`, and a slot frees only when a job ends, so 150 s with no slot means that every worker has been in one job for longer than any bounded wait (`STEP_WAIT_MAX` 120 s + `STORE_WAIT` 10 s + the throttle). Do not reply EIO to a valid request. A test with a short test-only limit shows the abort path in a child process, and a second test shows that `submit` returns as soon as a job ends.

2. **Should a flusher (or GC) panic also abort?**
   - What we know: Phase 20 made a flusher panic survivable (it wakes throttled writers; `a_flusher_panic_wakes_throttled_writers`). The flusher step holds the gate shared and a cell; its unwind releases them with no poison.
   - Recommendation: in this phase abort only request threads (`fuser-*`, `slicefs-worker-*`). Add a deferred item: "a flusher panic during a publish can leave a half-done cell; consider abort for `slicefs-flusher` too". Reversible.

3. **Drain limit value and action.**
   - Recommendation: 150 s (above `STEP_WAIT_MAX` 120 s + `STORE_WAIT` 10 s + the throttle), then abort with a message. A test with a short test-only limit and a parked job shows that `drain_and_join` returns `Err` and does not hang.

4. **Queue depth.**
   - Recommendation: `slots = 2 x N` (at least 4) and `max_bytes = 8 MiB`, admit any job when the queue is empty. The pool term (at most about 16 MiB at the default cap of 8) is small next to the per-worker step and arena term (Q7), so the queue is not the part that limits N.

5. **Where to start the workers.**
   - Recommendation: create the `Pool` in `run_mount` and pass it in `MountedFs`, with `Pool::drop` as the safety net when `mount()` fails before the session. Simpler than spawning in `init(&mut self)`, and the worker count is printed before the mount.

6. **A trace switch for the per-request log lines.**
   - What we know: two `eprintln!` lines per request take the global stderr lock; no switch exists; `pjdfstest.sh` filters these lines.
   - Recommendation: add `SLICEFS_FUSE_TRACE=0` (read once at mount; default on, so logs do not change) for the bench only. Do not change the default in this phase.

7. **The rule for "scales" (POOL-05).**
   - Recommendation: at N = 4 the median aggregate write or read MB/s is at least 1.5 times the N = 1 median, with 0 mismatches. If neither phase reaches it, record the numbers and a lock-stats run of the N = 4 point, and report POOL-05 as not met instead of changing the bar. The owner should confirm the 1.5x bar before the runs (A10).

8. **Memory cap for more than 4 writers.**
   - What we know: `CAP_MIB` = 320 was fixed from 4-writer runs; the rule's only explicit per-writer term is one 1 MiB write; own-step copies (up to 2 x 8 MiB at a release) and arenas grow with N.
   - What's unclear: the real `Pm(8)` and `Pm(16)`.
   - Recommendation: keep 320 for 4 writers; add `CAP(N) = Pm(N) + F + R` from 3 runs at N writers (S7 in process with `S7_THREADS`, and the mount bench); state `CAP(8)` in the Phase 21 result. If `Pm(8)` is more than one budget (64 MiB) above `Pm(4)`, stop and report to the owner before the default cap of 8 ships, as the Phase 20 rule did for P0.

9. **Phase 19 windows under N ≥ 2.**
   - Recommendation: keep them deferred (CONTEXT); add one line to the Phase 21 result that they are now reachable on FUSE-T with `--workers ≥ 2`, and that pjdfstest passed with 4 workers on Linux.

## Environment Availability

| Dependency | Required By | Available | Version | Fallback |
|------------|------------|-----------|---------|----------|
| cargo / rustc (local) | build, in-process tests | ✓ | cargo 1.95.0-nightly, rustc 1.95.0-nightly | — |
| Docker (OrbStack) | Linux mount tests, pjdfstest, bench, S7 runs at 4, 8, 16 writers | ✓ | Docker 29.8.2, kernel 7.2.12-orbstack, 32 CPUs, 15.68 GiB | — |
| `/dev/fuse` in the container | Linux mounts | ✓ (used by Phase 18-20 runs via `run.sh`) | — | — |
| macOS FUSE-T FSKit mount | POOL-04 macOS, macOS bench | ✗ locally (by rule) | CI `macos-26` runner | CI only (`macos-mounts.yml` on `ci/**`, `pool-bench-macos.yml` by dispatch) |
| GitHub Actions | CI | ✓ (existing workflows) | — | — |

**Missing dependencies with no fallback:** none.
**Missing dependencies with fallback:** macOS mounts run only in CI.

## Validation Architecture

### Test Framework
| Property | Value |
|----------|-------|
| Framework | Rust built-in test harness (`cargo test`), plus Docker shell checks |
| Config file | none (workspace `Cargo.toml`; Docker image `scripts/docker-linux-test/Dockerfile`) |
| Quick run command | `cargo test -p slicefs-cli --test pool_tests --test pool_panic_tests --test reply_after_locks --test flusher_tests` |
| Full suite command | `cargo test --workspace` locally; `SLICEFS_WORKERS=4 scripts/docker-linux-test/run.sh all && SLICEFS_WORKERS=4 scripts/docker-linux-test/run.sh verify && SLICEFS_WORKERS=4 scripts/docker-linux-test/run.sh pjdfstest` |

### Phase Requirements → Test Map
| Req ID | Behavior | Test Type | Automated Command | File Exists? |
|--------|----------|-----------|-------------------|-------------|
| POOL-01 | `--workers 0` is refused; `--workers 4` parses; no flag gives the default | unit | `cargo test -p slicefs-cli workers_` (in `cli.rs` tests) | ❌ Wave 0 |
| POOL-01 | Default: macOS 1; Linux `clamp(n/2, 1, 8)` (`workers_for_cpus`: 1→1, 2→1, 4→2, 16→8, 32→8); `--workers 16` is accepted | unit | `cargo test -p slicefs-cli workers_for_cpus` (in `pool.rs`) and `cargo test -p slicefs-cli workers_` (in `cli.rs`) | ❌ Wave 0 |
| POOL-01 | With N = 4, 4 jobs run at once (a barrier of 4 is reached) | unit | `cargo test -p slicefs-cli --test pool_tests jobs_run_in_parallel` | ❌ Wave 0 |
| POOL-01 | `submit` blocks at the slot bound and at the byte bound; an oversize job passes when the queue is empty | unit | `cargo test -p slicefs-cli --test pool_tests submit_` | ❌ Wave 0 |
| POOL-01 | Inline: with no pool, `run` executes on the caller thread | unit | `cargo test -p slicefs-cli --test pool_tests inline_runs_on_the_caller` | ❌ Wave 0 |
| POOL-01 | Mount prints `Workers: 4` | Docker smoke | `SLICEFS_WORKERS=4 scripts/docker-linux-test/run.sh smoke` | ❌ (script change) |
| POOL-02 | `drain_and_join` runs queued jobs, waits for a running one, joins all | unit | `cargo test -p slicefs-cli --test pool_tests drain_` | ❌ Wave 0 |
| POOL-02 | `drain_and_join` with a short limit and a parked job returns `Err`, no hang | unit | `cargo test -p slicefs-cli --test pool_tests drain_limit_reports_a_stuck_worker` | ❌ Wave 0 |
| POOL-02 | `submit` returns when a job ends; with a short test-only `SUBMIT_WAIT` and a stuck worker it aborts (child process) | unit + child process | `cargo test -p slicefs-cli --test pool_tests submit_` and `--test pool_panic_tests submit_limit_aborts` | ❌ Wave 0 |
| POOL-02 | `MountedFs::destroy` drains before `shutdown()` (the parked write is in the committed state; the flusher ran during the drain) | integration, in process (`Filesystem::destroy(&mut mfs)`) | `cargo test -p slicefs-cli --test pool_tests destroy_drains_before_shutdown` | ❌ Wave 0 |
| POOL-02 | `serve` panics in debug when a ranked lock is held at the reply point | unit | `cargo test -p slicefs-cli serve_panics_when_a_lock_is_held` | ❌ Wave 0 |
| POOL-02 | `gate_shared(` appears in `fuse_callbacks.rs` only in `serve` and `init` | source scan | `cargo test -p slicefs-cli --test reply_after_locks` | ❌ Wave 0 |
| POOL-02 | Unmount and SIGTERM with 4 workers: daemon exits, final commit, no `mount.lock` | Docker live checks | `SLICEFS_WORKERS=4 scripts/docker-linux-test/run.sh verify` (FUSET-05 block, mt-linux `daemon-exit`) | ✅ (needs the env pass-through) |
| POOL-03 | A worker panic at the half-done point aborts (SIGABRT), sends no reply, and the remount replays: A present, B absent | integration (child process) | `cargo test -p slicefs-cli --test pool_panic_tests` | ❌ Wave 0 |
| POOL-03 | Same on a thread named `fuser-0` (inline path) | integration (child process) | `cargo test -p slicefs-cli --test pool_panic_tests reader_thread_panic` | ❌ Wave 0 |
| POOL-03 | Mount: `SLICEFS_TEST_PANIC_CREATE=2`, daemon rc 134, remount shows the fsynced file and not the half-done one | Docker live check | `SLICEFS_WORKERS=4 scripts/docker-linux-test/run.sh verify` (new block) | ❌ (script change) |
| POOL-04 | All workspace tests; smoke, verify, pjdfstest PASS with 4 workers | CI | `ci.yml` Linux job with `SLICEFS_WORKERS: 4` | ❌ (workflow change) |
| POOL-04 | macOS FSKit mounts pass with the default | CI (by push to `ci/**`) | `macos-mounts.yml` | ✅ |
| POOL-05 | Aggregate MB/s and daemon VmHWM at N = 1/2/4/8/16 with 16 clients, 3 runs each, 0 mismatches, plus one `MALLOC_ARENA_MAX=2` run at 16; recorded | manual bench | `scripts/docker-linux-test/run.sh pool-bench` | ❌ (new script) |
| POOL-05 | In-process VmHWM at 4, 8 and 16 writers with the flusher (4 writers: under `CAP_MIB` 320 with the 200 ms limit; 8 and 16: give `CAP(N)`) | manual (Docker, release) | `S7_THREADS=8 S7_SERIALIZE=0 S7_FLUSHER=1 scripts/docker-linux-test/run.sh harness s7_budget_baseline` (also 4 and 16) | ❌ (`S7_THREADS` env is new) |
| POOL-05 | macOS at N = 1/2, recorded | manual CI | `pool-bench-macos.yml` (dispatch or `ci-pool/**`) | ❌ (new workflow) |
| (develop) | After `develop` is merged in: the flusher tests pass at `THROTTLE_WAIT` 200 ms (done in PR #151; a regression check only) | unit | `cargo test -p slicefs-cli --test flusher_tests` | ✅ |

### Sampling Rate
- **Per task commit:** the quick run command (under 30 s).
- **Per wave merge:** `cargo test --workspace`, plus `SLICEFS_WORKERS=4 run.sh smoke` after the callback restructure.
- **Phase gate:** full suite green (local workspace + Docker `all`, `verify`, `pjdfstest` with 4 workers), one `macos-mounts.yml` run on `ci/**`, the POOL-05 and S7 records written, before `/gsd-verify-work`.

### Wave 0 Gaps
- [ ] `crates/slicefs-cli/tests/pool_tests.rs` — POOL-01, POOL-02 (needs `pool` and `fuse_callbacks::MountedFs` reachable from tests: `pub mod pool` in `lib.rs`)
- [ ] `crates/slicefs-cli/tests/pool_panic_tests.rs` — POOL-03 (child-process pattern; new in this repo)
- [ ] `crates/slicefs-cli/tests/reply_after_locks.rs` — POOL-02 source scan
- [ ] `SLICEFS_WORKERS` pass-through in `run.sh` and the mount scripts — POOL-04
- [ ] `scripts/docker-linux-test/pool-bench.sh`, `scripts/macos-mount-test/pool-bench.sh`, `.github/workflows/pool-bench-macos.yml` — POOL-05
- [ ] `S7_THREADS` env value (default 4) in `s7_budget_baseline` — POOL-05 memory term
- [ ] Merge `develop` into the branch (PR #151, the 200 ms limit) before any measurement
- [ ] Framework install: none

## Security Domain

No `security_enforcement` key in `.planning/config.json`, so the section is included. The phase adds no network surface, no authentication and no new input format.

### Applicable ASVS Categories

| ASVS Category | Applies | Standard Control |
|---------------|---------|-----------------|
| V2 Authentication | no | — |
| V3 Session Management | no | — |
| V4 Access Control | no change | fuser `SessionACL` check runs on the reader before dispatch (request.rs:73-97), so it also guards pool jobs |
| V5 Input Validation | yes | `--workers` range parser (reject 0); FUSE arguments are parsed by fuser before the hand-off |
| V6 Cryptography | no | — |

### Known Threat Patterns for a multithreaded FUSE daemon

| Pattern | STRIDE | Standard Mitigation |
|---------|--------|---------------------|
| Memory exhaustion by many large queued WRITEs | Denial of service | Queue bounded in slots and bytes; the reader blocks (kernel back-pressure) |
| Requests served on half-done state after a panic | Tampering | Abort in the panic hook before the unwind; replay at the next mount |
| A request that never ends blocks unmount | Denial of service | Drain limit and `submit` limit (150 s each), then abort (crash-safe) |
| Reply sent while a lock is held (slow peer stalls commits) | Denial of service | `serve` helper, debug assert, source scan |
| A debug-only panic injection env var reaches a release build | Tampering | `#[cfg(debug_assertions)]` on the env read |

## Sources

### Primary (HIGH confidence)
- Repository code read this session: `vendor/fuser/src/{session.rs, request.rs, reply.rs, channel.rs, lib.rs, request_param.rs}`, `vendor/fuser/PATCHES.md`; `crates/slicefs-cli/src/{fuse_callbacks.rs, filesystem.rs, handlers.rs, mount.rs, signals.rs, unmount.rs, flusher.rs, cli.rs}`; `crates/chunk-store/src/{sync.rs, lib.rs}`; `crates/slicefs-cli/examples/s1_probe.rs`; `crates/slicefs-cli/tests/{flusher_tests.rs, no_take_out.rs, crash_recovery_tests.rs, signal_tests.rs}`; `scripts/docker-linux-test/*`; `.github/workflows/*`.
- Scratch compile checks (outside the repo): reply types `Send + 'static` with an `Rc` negative control; clap 4.6.0 `RangedU64ValueParser::<usize>::new().range(1..)`.
- Planning docs: `21-CONTEXT.md`, ROADMAP Phase 21, REQUIREMENTS POOL-01..05, `docs/research/multithreaded-request-handling.md` (S1, S3, S4, S5, S7, Phase 19 and 20 results), `.planning/research/ARCHITECTURE.md` Part 1, `20-VERIFICATION.md`, `20-REVIEW.md` (WR-01), Phase 19 and 20 `deferred-items.md`.

### Secondary (MEDIUM confidence)
- [CITED: doc.rust-lang.org/std/thread/fn.available_parallelism.html] — Linux affinity and cgroup behaviour, errors, not cached.
- [CITED: docs.kernel.org/filesystems/fuse/fuse.html] — INTERRUPT handling (the daemon may ignore INTERRUPT; EAGAIN race); background request limit.

### Tertiary (LOW confidence)
- Kernel and FUSE-T ordering facts in the Assumptions Log (training knowledge).

## Metadata

**Confidence breakdown:**
- Standard stack: HIGH — no new package; std and existing crates; parser checked by compile.
- Architecture: HIGH — every hand-off fact read in the vendored fuser; the drain point follows from `run()`; the panic gap traced in code.
- Pitfalls: MEDIUM — the kernel and FUSE-T ordering parts are assumptions.
- POOL-05 outcome: LOW — S3 in process did not scale past 2; the mount result is not known.

**Line numbers:** the `filesystem.rs`, `cli.rs` and `README.md` line numbers above are those of this branch (based on 885015a), before `develop` is merged. PR #151 adds lines near `THROTTLE_WAIT`, so `filesystem.rs` lines after about 159 move by a few lines after the merge (on `develop` the constant is at line 163).

**Research date:** 2026-11-23
**Valid until:** 2026-12-23 (stable code base; re-check if fuser is updated)
