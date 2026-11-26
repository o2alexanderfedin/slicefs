# Review of the unsafe code

## What this is

This document is the written review of the `unsafe` code of SliceFS that
Miri does not execute. It covers every crate of the build: chunk-store,
metadata, slicefs-cli, the benchmark tools and the vendored fuser.

Miri runs the lib tests of chunk-store and metadata (Safety job `miri`,
script `scripts/safety/miri.sh`). The Safety workflow runs when a `v*` tag
is pushed, or when a person starts it by hand. A developer can run each
script locally. Miri cannot run a foreign
function that it does not model, a file-backed `mmap` or a FUSE mount. For
that code, this review gives the safety argument in words.

Other Safety jobs check the same code from other sides: `clippy`
(rows `clippy-linux`, `clippy-macos`), `model`, `tsan` and `deadlock`. The
CI workflow (pushes and PRs to `main` and `develop`) runs the tests on
Linux (job `linux`, in Docker, with a FUSE
smoke test and pjdfstest) and on macOS (job `macos`, `cargo test` and the
FUSE-T build).

## How to read an entry

Each entry has four fields:

- **File and item**: the file and the function or type. Entries name the
  function, not a line number, because line numbers change.
- **What it does**: the operation in one or two sentences.
- **Safety argument**: the invariant that makes the operation sound, and
  why that invariant holds.
- **How it is tested**: a test name and the job that runs it, or why no
  test can reach it.

A **Note** is a weak point that is not a memory-safety bug. A note that
needs a code change is a follow-up. This review changes no code.

## How the counts are kept

The section "Counts" has one line per `.rs` file that has the word
`unsafe` at least once. The number is the count of `unsafe` word tokens in
the file (`grep -ow unsafe`), comments included. The script
`scripts/safety/unsafe-count.sh` counts the tokens again and compares them
with these lines. A file whose count differs, or a file that is on one side
only, fails the script. `scripts/safety/clippy.sh` runs it. Thus the Safety
job `clippy` (on a `v*` tag or a manual run) fails on Linux and macOS when
the code has an `unsafe` token that this review does not count. Add or change a review entry, then change
the count line.

## chunk-store

All `unsafe` of chunk-store `src` is in `crates/chunk-store/src/pack_io.rs`.

### Executed by Miri

| Item | What Miri runs | Test |
|------|----------------|------|
| `unsafe impl Send for RunMap` | a move of a map into another thread | `pack_io::tests::a_run_map_reads_back_moves_across_threads_and_unmaps` |
| `unsafe impl Sync for RunMap` | a read of a shared map from another thread | the same test |
| `RunMap::bytes` (`from_raw_parts`) | the view of the mapped bytes | the same test |
| `Drop for RunMap` (`munmap`) | the unmap, in the thread that got the map | the same test |

The test makes its map with `RunMap::anon_for_test`, a private anonymous
mapping (test code, two `unsafe` blocks). Miri maps whole pages. Thus Miri
does not see a `len` that is one byte too long. The `len` argument of the
next entry is the check for that.

### pack_io.rs: the `len` of a `RunMap` (applies to `bytes` and `Drop`)

- **File and item**: `pack_io.rs`, the fields `ptr` and `len` of `RunMap`.
- **What it does**: `bytes` makes `&[u8]` of `len` bytes at `ptr`. `Drop`
  unmaps `len` bytes at `ptr`.
- **Safety argument**: There are three places that make a `RunMap`. Each
  stores the exact length that it gave to `mmap`. `new` stores `len = 0`
  with a null pointer, or the `len` of the successful `mmap`.
  `anon_for_test` (test only) does the same. The fields are private, no code
  changes them after construction, and `RunMap` is not `Clone` or `Copy`.
  Thus `munmap` runs once, on the mapping that `mmap` made. `bytes` and
  `Drop` skip the pointer when `len` is 0. A successful `mmap` of `len`
  bytes is never larger than `isize::MAX`, so the slice rule of
  `from_raw_parts` holds. `bytes` borrows `&self`, so the slice cannot live
  after the unmap.
- **How it is tested**: the Miri test above, and the file tests of the next
  entry. A wrong `len` that stays inside the last page is not visible to a
  test. This argument is the check. chunk-store has no other `unsafe` that
  reads the mapping, so every read of the slice is bounds-checked safe
  Rust.

### pack_io.rs: `RunMap::new` (`mmap` of a run file)

- **File and item**: `pack_io.rs`, `RunMap::new`.
- **What it does**: maps a whole index run file read-only
  (`PROT_READ`, `MAP_SHARED`, offset 0, length from `file.metadata()`).
- **Safety argument**: The fd is open for the call. A `MAP_FAILED` result
  is an error and is never used. The length is the file size at the call.
  Only index run files are mapped (`map_run`, `Kind::Run`); pack files are
  never mapped. A run file is sealed before it is mapped: `RunWriter::finish`
  takes the writer by value, so no writer is alive when `map_run` runs, and
  no code calls `write_at`, `set_len` or `punch` on a run file (`punch` opens
  only `Kind::Pack`). Thus the mapped bytes do not change while a `&[u8]`
  of them is alive. `Run::open` refuses a map that is shorter than the
  length in the MANIFEST, so a lookup does not read past the end of the
  file. A merge or GC that removes a run file does not take the bytes away:
  readers hold the run through an `Arc`, and the file system keeps a
  removed file while a mapping of it exists.
- **Accepted risk** (from the `RunMap` doc): a disk read error inside the
  mapping, or a cut of the file by a program outside SliceFS, raises SIGBUS
  and stops the process. SliceFS never cuts a sealed run file. Scrub, merge
  and rebuild read runs with `read_at`, where a read error is a normal
  error.
- **How it is tested**: `run::tests::a_run_round_trips_through_each_io`,
  `mapped_run_tests` (`lookups_on_files_stay_right_while_merges_and_gc_replace_the_runs`,
  `a_run_file_cut_before_the_open_is_rebuilt_not_mapped_past_its_end`), in
  `cargo test` of the CI jobs `linux` and `macos`. Miri does not support a
  file-backed `mmap`.

### pack_io.rs: `madvise` in `RunMap::new`

- **File and item**: `pack_io.rs`, `RunMap::new`, the `madvise` call.
- **What it does**: gives `MADV_RANDOM` for a run of at least
  `MADV_RANDOM_MIN` (256 MiB).
- **Safety argument**: The address and the length are those of the mapping
  that `mmap` made one line before. `madvise` is only advice. It does not
  change the bytes, and its result is ignored because a failure changes
  nothing that is correct.
- **How it is tested**: no test makes a run of 256 MiB (too large for a
  unit test). Miri does not support `madvise`.

### pack_io.rs: `punch_hole` on macOS (`fcntl F_PUNCHHOLE`)

- **File and item**: `pack_io.rs`, `punch_hole` with
  `cfg(target_os = "macos")`.
- **What it does**: frees a byte range of a pack file.
- **Safety argument**: The fd is open for the call. The argument is a
  `fpunchhole_t` on the stack that lives for the call; the kernel only reads
  it. A bad range (not aligned, or a negative `off_t` after the cast) gives
  an error code, not memory damage. `punch` calls it only on `Kind::Pack`
  files, which are never mapped.
- **How it is tested**: `punch_tests` (for example
  `a_mostly_live_pack_is_punched_and_the_live_objects_stay`) and the punch
  probe of `FileIo`, in `cargo test` of the CI job `macos`. Miri does not
  support this `fcntl` command.

### pack_io.rs: `punch_hole` on Linux (`fallocate`)

- **File and item**: `pack_io.rs`, `punch_hole` with
  `cfg(target_os = "linux")`. This is Linux-only code.
- **What it does**: `fallocate` with `FALLOC_FL_PUNCH_HOLE |
  FALLOC_FL_KEEP_SIZE` frees a byte range of a pack file.
- **Safety argument**: The fd is open for the call, and all arguments are
  integers. A bad range gives an error code. `EOPNOTSUPP` and `ENOTSUP`
  become `Unsupported` (on Linux the two values are equal). Same
  `Kind::Pack` rule as on macOS.
- **How it is tested**: `punch_tests` in `cargo test` of the CI job
  `linux` (Docker). Miri does not support `fallocate`.

### pack_io.rs: `full_sync` (`fcntl F_FULLFSYNC`)

- **File and item**: `pack_io.rs`, `full_sync`, the macOS block.
- **What it does**: flushes the file and the drive cache. On failure it
  falls back to `sync_all`.
- **Safety argument**: The fd is open for the call (`f` is borrowed), and
  the call has no pointer argument.
- **How it is tested**: every `FileIo::new` store test with sync on (for
  example `punch_tests`) in `cargo test` of the CI job `macos`. No Miri-run
  test reaches it.

## metadata

### store.rs: `getuid` and `getgid` in `DictMetadataStore::new`

- **File and item**: `crates/metadata/src/store.rs`,
  `DictMetadataStore::new`, one line with two `unsafe` blocks.
- **What it does**: reads the real user and group ID of the process for the
  owner of the root inode.
- **Safety argument**: The calls take no argument and touch no memory of
  the caller. POSIX says that they always succeed. The `cfg(not(unix))`
  branch uses `(0, 0)` and has no `unsafe`.
- **How it is tested**: every test that makes a new metadata store, in
  `cargo test` of the CI jobs `linux` and `macos`. Miri does not support
  `getgid`.

## slicefs-cli

### alloc_tune.rs: `mallopt` in `tune_at_start`

- **File and item**: `crates/slicefs-cli/src/alloc_tune.rs`,
  `tune_at_start`, `cfg(all(target_os = "linux", target_env = "gnu"))`.
  This is Linux-only code.
- **What it does**: sets the glibc mmap threshold to 128 KiB.
- **Safety argument**: `mallopt` takes two integers. glibc owns the
  allocator state and locks it. The result is checked (`1` is success).
- **How it is tested**: `alloc_tune_tests::tune_at_start_fixes_the_mmap_threshold`
  in the CI job `linux`.

### alloc_tune.rs: `mallinfo2` in `stats`

- **File and item**: `alloc_tune.rs`, `stats`, the glibc block. This is
  Linux-only code.
- **What it does**: reads the glibc allocator numbers.
- **Safety argument**: `mallinfo2` takes no pointer and returns a struct by
  value.
- **How it is tested**: `alloc_tune_tests::stats_show_a_live_heap` in the CI
  job `linux`.

### alloc_tune.rs: `zeroed` and `malloc_zone_statistics` in `stats`

- **File and item**: `alloc_tune.rs`, `stats`, the macOS block (two
  `unsafe` blocks).
- **What it does**: reads the numbers of all malloc zones into a
  `malloc_statistics_t`.
- **Safety argument**: All-zero is a valid value of this C struct (plain
  integers). A null zone means all zones. `&mut s` is a valid out pointer
  for the call.
- **How it is tested**: `alloc_tune_tests::stats_show_a_live_heap` in the CI
  job `macos`.
- **Note**: the `SAFETY` comment is above the `zeroed` line, not above the
  call. A comment-only follow-up.

### fuset_watch.rs: `getfsstat` in `is_mounted`

- **File and item**: `crates/slicefs-cli/src/fuset_watch.rs`,
  `is_mounted`. The module is `cfg(target_os = "macos")`.
- **What it does**: reads the kernel mount table and compares each mount
  point with the path.
- **Safety argument**: The first call has a null buffer and returns only
  the count. The buffer is a `Vec` of `n + 8` zeroed `statfs` (all-zero is
  valid for this C struct). The size given to the kernel is the size of
  that `Vec` in bytes, so the kernel writes at most that many entries. The
  `Vec` is cut to the count that the second call returns. `f_mntonname` is
  a fixed array that the kernel ends with a NUL, so `CStr::from_ptr` stays
  inside the array.
- **How it is tested**: `fuset_watch::tests::the_root_is_mounted_and_a_plain_directory_is_not`
  in the CI job `macos`.

### fuset_watch.rs: `kill(pid, SIGKILL)` in `spawn_unmount_watcher`

- **File and item**: `fuset_watch.rs`, `spawn_unmount_watcher`.
- **What it does**: ends the `go-nfsv4` children of this process after the
  mount is gone.
- **Safety argument**: `kill` takes two integers and touches no memory.
  The pids come from `pgrep -P <own pid> go-nfsv4`, so only children of
  this process are signalled.
- **How it is tested**: no automated test. It needs a FUSE-T mount, and
  this project does not mount on macOS CI. It runs in the by-hand FUSE-T
  runs.
- **Note**: as in `unmount.rs` (below), the pid is parsed from tool text and
  is not checked for `pid > 1`. The risk is lower here: `pgrep -P` prints
  only the pids of real children.

### gc_pacing.rs: `statvfs` in `free_ratio`

- **File and item**: `crates/slicefs-cli/src/gc_pacing.rs`, `free_ratio`
  (two `unsafe` blocks).
- **What it does**: reads the free and total blocks of the file system
  under the store.
- **Safety argument**: All-zero is a valid value of `statvfs`. `c` is a
  `CString`, so it ends with a NUL and lives for the call. `&mut sv` is a
  valid out buffer. An error gives `1.0` and the struct is not read.
- **How it is tested**: `gc_pacing::tests::the_free_ratio_of_a_real_path_is_a_ratio_and_an_error_is_plenty`
  in the CI jobs `linux` and `macos`.

### signals.rs: `write` in the handler `on_signal`

- **File and item**: `crates/slicefs-cli/src/signals.rs`, `on_signal`.
- **What it does**: writes the signal number as one byte to the pipe.
- **Safety argument**: The handler does only two things: an atomic load of
  the fd and `write(2)`. Both are async-signal-safe. It does not allocate,
  lock or print. The byte is on the handler stack and lives for the call.
  The fd is `-1` (not used) until `install` stores it, and it is never
  closed.
- **How it is tested**: `signal_tests::a_signal_calls_the_stop_callback_and_ignored_signals_stay_ignored`
  in the CI jobs `linux` and `macos`.
- **Note**: the handler does not save and restore `errno`. `write` sets
  `errno` only when it fails, for example on a closed fd. The fd is never
  closed, so this does not occur. Saving `errno` is a small follow-up.
  The pipe is blocking. A full pipe (64 KiB of signal bytes that the
  thread did not read) would block the handler; it would not fail. That
  many signals do not occur in practice.

### signals.rs: `pipe` in `install`

- **File and item**: `signals.rs`, `install`.
- **What it does**: opens the pipe for the handler.
- **Safety argument**: `fds` is an array of two `c_int`, which is the room
  that `pipe(2)` needs. The result is checked.
- **How it is tested**: the signal test above.

### signals.rs: `fcntl(F_SETFD, FD_CLOEXEC)` in `install`

- **File and item**: `signals.rs`, `install`, the loop over the two fds.
- **What it does**: sets close-on-exec on each pipe fd.
- **Safety argument**: Each fd is one that `pipe(2)` just opened. The call
  has no pointer argument.
- **How it is tested**: the signal test above (it does not check the flag).
- **Note**: the flag is set after `pipe`, not in the same call, and the
  result is not checked. A child that another thread starts between the
  two calls gets the pipe fds. That is an fd leak in the child, not memory
  damage. `pipe2(O_CLOEXEC)` exists on Linux but not on macOS. No
  follow-up is proposed.

### signals.rs: the `sigaction` block in `install`

- **File and item**: `signals.rs`, `install`, one `unsafe` block per signal.
- **What it does**: reads the old action. If the signal is not ignored, it
  sets `on_signal` with `SA_RESTART` and an empty mask.
- **Safety argument**: All-zero is a valid `sigaction`. The pointers are
  to locals that live for each call. `SA_SIGINFO` is not set, so
  `sa_sigaction` holds a plain one-argument handler, which is the type of
  `on_signal` (`extern "C" fn(c_int)`). Every result is checked. A signal
  that was ignored at start stays ignored.
- **How it is tested**: the signal test above (it checks both a handled and
  an ignored signal).

### signals.rs: `read` in the signal thread

- **File and item**: `signals.rs`, `install`, the thread loop.
- **What it does**: reads one byte from the pipe.
- **Safety argument**: The buffer is one local byte and the length is 1.
  `EINTR` is retried; end of file or an error ends the thread.
- **How it is tested**: the signal test above.

### unmount.rs: `kill(pid, SIGTERM)` in `kill_processes_at_mountpoint`

- **File and item**: `crates/slicefs-cli/src/unmount.rs`,
  `kill_processes_at_mountpoint`, the `lsof +D` loop.
- **What it does**: sends SIGTERM to each process that has a file open under
  the mount point.
- **Safety argument**: `kill` takes two integers and touches no memory. The
  pid is column 2 of the `lsof` output, parsed with `parse::<i32>()`.
- **How it is tested**: `unmount::tests::test_kill_processes_at_mountpoint_temp_dir_does_not_panic`
  and `..._nonexistent_path_does_not_panic` in the CI jobs `linux` and
  `macos` (no process is listed there, so `kill` is not called).
- **Note (hardening, follow-up)**: the pid is not checked for `pid > 1`. A
  pid of 0 would signal the process group of the caller, and -1 would
  signal every process of the user. The risk is low because `lsof` and `ps`
  print no such pid. The guard is one line (skip a pid that is not
  `pid > 1`) plus a unit test on `fuse_t_servers_of`. It is a follow-up, not
  done in this phase, because decision D-33 allows no behavior change
  outside the two race fixes.

### unmount.rs: `kill(pid, SIGKILL)` in `kill_processes_at_mountpoint`

- **File and item**: `unmount.rs`, `kill_processes_at_mountpoint`, the
  FUSE-T server loop.
- **What it does**: sends SIGKILL to the `go-nfsv4` server of this mount
  point only.
- **Safety argument**: as above. The pids come from `fuse_t_servers_of`,
  which parses `ps -axww -o pid=,command=` and keeps only `go-nfsv4` lines
  whose last argument is this mount point.
- **How it is tested**: `unmount::tests::only_the_server_of_the_mount_point_is_selected`
  tests the parser in the CI jobs `linux` and `macos`. The `kill` itself
  runs only in the by-hand FUSE-T runs.
- **Note**: the same `pid > 1` follow-up as above.

### filesystem.rs: `statvfs` in `compute_statfs`

- **File and item**: `crates/slicefs-cli/src/filesystem.rs`,
  `compute_statfs` (two `unsafe` blocks).
- **What it does**: reads the block numbers of the file system under the
  store for the FUSE `statfs` reply.
- **Safety argument**: All-zero is a valid `statvfs`. The path is a
  `CString` that lives for the call. `&mut sv` is a valid out buffer. The
  struct is read only when the call returns 0.
- **How it is tested**: `test_statfs_with_store_path`,
  `test_statfs_no_store_path` and `test_statfs_invalid_path_cstring` in the
  CI jobs `linux` and `macos`.
- **Note**: the blocks have no `SAFETY` comment. A comment-only follow-up.

### filesystem.rs: `statfs` in `compute_statfs`

- **File and item**: `filesystem.rs`, `compute_statfs`, the fallback after
  `statvfs` (two `unsafe` blocks).
- **What it does**: reads `statfs(2)` when `statvfs` fails or reports no
  blocks (for example exFAT on macOS).
- **Safety argument**: as for `statvfs`. The `statfs` struct of `libc` has
  the layout of the target (the Linux and macOS structs differ; the code
  reads only fields that both have).
- **How it is tested**: no test reaches the fallback. It needs a file system
  that reports 0 blocks through `statvfs`.
- **Note**: no `SAFETY` comment, as above.

## Test code

The `unsafe` in test code is listed here with one line per file.

- `crates/chunk-store/src/pack_io.rs` (test part): `RunMap::anon_for_test`,
  an anonymous `mmap` and a copy into it; one more token is in a doc
  comment of the Miri test.
- `crates/chunk-store/tests/gc_full_memory_tests.rs`: a counting
  `GlobalAlloc` that passes each call to `System`.
- `crates/slicefs-cli/tests/dirty_budget_tests.rs`: the same counting
  allocator.
- `crates/slicefs-cli/tests/memory_bound_tests.rs`: the same counting
  allocator.
- `crates/slicefs-cli/tests/replay_memory_tests.rs`: the same counting
  allocator.
- `crates/slicefs-cli/tests/signal_tests.rs`: `signal(SIGHUP, SIG_IGN)` and
  two `raise` calls.
- `crates/slicefs-cli/tests/stress_harness.rs`: a zeroed `rusage` and
  `getrusage`.
- `crates/slicefs-cli/tests/no_take_out.rs`: the word in a parser of
  function headers and its test strings; no `unsafe` code.
- `crates/slicefs-cli/tests/reply_after_locks.rs`: the word in the same
  kind of parser; no `unsafe` code.

## vendor/fuser

`vendor/fuser` is outside the workspace, so clippy does not check it
(decision D-11). This review covers it.

**What SliceFS compiles.** `slicefs-cli` uses `fuser` with no features
(`default = []`). `vendor/fuser/build.rs` sets the cfg `fuser_mount_impl`:

- Linux (no `libfuse` feature): `pure-rust`. `src/mnt/mod.rs` then compiles
  `mnt/fuse_pure.rs`.
- macOS (no `macos-no-mount` feature): a pkg-config probe of `fuse` (the
  libfuse2 library of FUSE-T), then `libfuse2`. `src/mnt/mod.rs` then
  compiles `mnt/fuse2.rs` and `mnt/fuse2_sys.rs`.
- `libfuse3` is set only for other targets or with a `libfuse` feature.
  Thus SliceFS compiles `mnt/fuse3.rs` and `mnt/fuse3_sys.rs` on no target.

**How it is tested.** The CI job `linux` runs the vendored fuser lib tests
(`scripts/docker-linux-test/run.sh all`, a copy of `vendor/fuser`,
`cargo test --lib`), the Linux FUSE smoke test and pjdfstest. These run as
root in Docker, so the mount is the direct `mount(2)` path, not the
`fusermount` path. The CI job `macos` builds and links the libfuse2 path
but does not mount. The FUSE-T mount runs only in the by-hand runs.

**Code that SliceFS does not use.** The passthrough items (`BackingId`,
`wrap_backing`, `open_backing`) and `clone_fd` are compiled, but SliceFS
calls none of them: it makes no `BackingId` and does not set `clone_fd`.

### channel.rs: `Channel::clone_fd`

- **File and item**: `vendor/fuser/src/channel.rs`, `Channel::clone_fd`, the
  `FUSE_DEV_IOC_CLONE` block.
- **Compiled for SliceFS**: Linux yes (`cfg(target_os = "linux")`), macOS
  no.
- **What it does**: opens a new `/dev/fuse` fd and attaches it to the
  session of this channel.
- **Safety argument**: The ioctl gets an fd that `DevFuse::open` just
  opened and a pointer to a local `u32` that lives for the call. The `nix`
  ioctl macro checks the result.
- **How it is tested**: not reached. Only the session option `clone_fd`
  calls it, and SliceFS does not set it.

### channel.rs: `ChannelSender::wrap_backing`

- **File and item**: `channel.rs`, `unsafe fn ChannelSender::wrap_backing`
  and the block in it.
- **Compiled for SliceFS**: Linux yes, macOS yes.
- **What it does**: makes a `BackingId` from a raw backing ID. There is no
  system call.
- **Safety argument**: The function passes its contract (the ID is open,
  belongs to this session and is not closed while the `BackingId` lives)
  to `BackingId::wrap_raw`. Its callers are the `wrap_backing` functions of
  `reply.rs`, which have the same contract.
- **How it is tested**: not reached. SliceFS calls no `wrap_backing`.

### reply.rs: `ReplySender::wrap_backing`

- **File and item**: `vendor/fuser/src/reply.rs`, `unsafe fn
  ReplySender::wrap_backing` and the block in it.
- **Compiled for SliceFS**: Linux yes, macOS yes.
- **What it does**: passes the call to `ChannelSender::wrap_backing`.
- **Safety argument**: the same contract, passed on unchanged.
- **How it is tested**: not reached.

### reply.rs: `ReplyOpen::wrap_backing`

- **File and item**: `reply.rs`, `pub unsafe fn ReplyOpen::wrap_backing`
  and the block in it.
- **Compiled for SliceFS**: Linux yes, macOS yes.
- **What it does**: the public entry for a file system that makes a
  `BackingId` from a raw ID.
- **Safety argument**: the `# Safety` doc of the function states the
  contract; the caller must keep it. SliceFS does not call it.
- **How it is tested**: not reached.

### reply.rs: `ReplyCreate::wrap_backing`

- **File and item**: `reply.rs`, `pub unsafe fn ReplyCreate::wrap_backing`
  and the block in it.
- **Compiled for SliceFS**: Linux yes, macOS yes.
- **What it does**: as `ReplyOpen::wrap_backing`.
- **Safety argument**: as `ReplyOpen::wrap_backing`.
- **How it is tested**: not reached.

### passthrough.rs: `BackingId::create_raw`

- **File and item**: `vendor/fuser/src/passthrough.rs`,
  `BackingId::create_raw`, the `FUSE_DEV_IOC_BACKING_OPEN` call.
- **Compiled for SliceFS**: Linux yes, macOS yes (on macOS it returns an
  error before the call).
- **What it does**: registers an open file as a backing file of the
  session and returns its ID.
- **Safety argument**: Both fds are borrowed for the call. The argument is
  a local `fuse_backing_map` that lives for the call. The `nix` ioctl macro
  checks the result.
- **How it is tested**: not reached. Only `open_backing` calls it.

### passthrough.rs: `BackingId::wrap_raw`

- **File and item**: `passthrough.rs`, `unsafe fn BackingId::wrap_raw`.
- **Compiled for SliceFS**: Linux yes, macOS yes.
- **What it does**: stores the ID and a weak reference to the channel.
  There is no operation that needs `unsafe` in it.
- **Safety argument**: The `unsafe` marks the contract: `Drop` closes the
  ID, so the ID must be open and owned by this value.
- **How it is tested**: not reached.

### passthrough.rs: `Drop for BackingId`

- **File and item**: `passthrough.rs`, `Drop for BackingId`, the
  `FUSE_DEV_IOC_BACKING_CLOSE` call.
- **Compiled for SliceFS**: Linux yes, macOS yes.
- **What it does**: closes the backing ID when the value is dropped.
- **Safety argument**: The call runs only if the channel still exists (the
  weak reference upgrades), so the fd is open. The argument points to a
  field of `self`, which lives for the call. An error is logged.
- **How it is tested**: not reached. SliceFS makes no `BackingId`.

### forget_one.rs: `ForgetOne::slice_from_inner`

- **File and item**: `vendor/fuser/src/forget_one.rs`,
  `ForgetOne::slice_from_inner` (`transmute`).
- **Compiled for SliceFS**: Linux yes, macOS yes.
- **What it does**: shows the `fuse_forget_one` array of a `BATCH_FORGET`
  request as `&[ForgetOne]`.
- **Safety argument**: `ForgetOne` is `repr(transparent)` over
  `fuse_forget_one`, so both have the same size and alignment. The
  `transmute` of `&[A]` to `&[B]` keeps the length and the lifetime. The
  input slice comes from `ArgumentIterator::fetch_slice`, which uses
  `zerocopy` and checks the size and the alignment.
- **How it is tested**: it runs for each `BATCH_FORGET` that the kernel
  sends. The Linux FUSE smoke test and pjdfstest of the CI job `linux` can
  reach it; no test checks that they do.

### mnt/fuse_pure.rs: `File::from_raw_fd` in `receive_fusermount_message`

- **File and item**: `vendor/fuser/src/mnt/fuse_pure.rs`,
  `receive_fusermount_message`.
- **Compiled for SliceFS**: Linux yes, macOS no.
- **What it does**: takes ownership of the `/dev/fuse` fd that
  `fusermount` sends over a socket (`SCM_RIGHTS`).
- **Safety argument**: An fd received with `SCM_RIGHTS` is new in this
  process, and no other value owns it. A negative fd is refused.
- **How it is tested**: reached only when the direct mount fails (a user
  that is not root: `fuse_mount_sys` returns `None` only on `EPERM`). The
  CI Docker image has no `USER` line and the scripts do not change the
  user, so CI mounts as root and no CI job reaches it.
- **Note**: only the first fd of the message is taken. An extra fd would
  stay open (a leak, not memory damage). `fusermount` sends one.

### mnt/fuse_pure.rs: `clear_cloexec_in_pre_exec`

- **File and item**: `fuse_pure.rs`, `unsafe fn clear_cloexec_in_pre_exec`
  and the `pre_exec` block in it.
- **Compiled for SliceFS**: Linux yes, macOS no.
- **What it does**: in the child, after `fork` and before `exec`, it clears
  `FD_CLOEXEC` on one fd, so that `fusermount` gets that fd.
- **Safety argument**: The `pre_exec` closure runs in the child of a
  `fork`, where only async-signal-safe calls are allowed. It calls only
  `fcntl`, and it does not allocate (a `nix` error becomes an `io::Error`
  from the raw OS code). The `unsafe fn` contract is that the fd stays open
  until the command is spawned; both callers keep it.
- **How it is tested**: reached only on the `fusermount` path (see above).

### mnt/fuse_pure.rs: the call in `fuse_mount_fusermount`

- **File and item**: `fuse_pure.rs`, `fuse_mount_fusermount`.
- **Compiled for SliceFS**: Linux yes, macOS no.
- **What it does**: lets the `fusermount` child keep its end of the socket
  pair.
- **Safety argument**: `child_socket` is dropped only after `spawn`, so
  the fd is open when the child runs the closure.
- **How it is tested**: reached only on the `fusermount` path.

### mnt/fuse_pure.rs: the call in `fuse_mount_mount_fusefs`

- **File and item**: `fuse_pure.rs`, `fuse_mount_mount_fusefs`.
- **Compiled for SliceFS**: Linux yes, macOS no.
- **What it does**: lets `mount_fusefs` keep the `/dev/fuse` fd.
- **Safety argument**: `fuse_device` lives until `output()` returns, so
  the fd is open when the child runs the closure.
- **How it is tested**: not reached on Linux. It runs only when the found
  helper is `mount_fusefs` (the BSDs).

### mnt/fuse2.rs: `fuse_mount_compat25` in `MountImpl::new`

- **File and item**: `vendor/fuser/src/mnt/fuse2.rs`, `MountImpl::new`, the
  `fuse_mount_compat25` call.
- **Compiled for SliceFS**: Linux no, macOS yes (the FUSE-T path).
- **What it does**: asks libfuse2 to mount and returns the session fd.
- **Safety argument**: The mount point is a `CString` that lives for the
  call. The argument list comes from `with_fuse_args`, which keeps its
  strings alive for the closure. A negative result is an error.
- **How it is tested**: the CI job `macos` builds and links it. It runs in
  the by-hand FUSE-T runs only.

### mnt/fuse2.rs: `File::from_raw_fd` in `MountImpl::new`

- **File and item**: `fuse2.rs`, `MountImpl::new`.
- **Compiled for SliceFS**: Linux no, macOS yes.
- **What it does**: takes ownership of the fd that `fuse_mount_compat25`
  returned.
- **Safety argument**: In the libfuse2 API the caller owns the returned fd
  and closes it. The fd is checked (`fd >= 0`) before the call.
- **How it is tested**: as above.

### mnt/fuse2_sys.rs: the `unsafe extern "C"` block

- **File and item**: `vendor/fuser/src/mnt/fuse2_sys.rs`, the block with
  `fuse_mount_compat25` (and `fuse_unmount_compat22` for other targets).
- **Compiled for SliceFS**: Linux no (the module is compiled for the
  vendored tests, but this block is `cfg(fuser_mount_impl = "libfuse2")`),
  macOS yes.
- **What it does**: declares the libfuse2 functions.
- **Safety argument**: The declarations must match the C functions. The
  `_compat25` names exist since libfuse 2.6, and `build.rs` asks pkg-config
  for at least 2.6.0. `fuse_args` is `repr(C)` with the C fields.
- **How it is tested**: the CI job `macos` links them against the FUSE-T
  library. A wrong signature is not visible at link time; the by-hand
  FUSE-T runs call it.

### Not compiled for SliceFS

- `mnt/fuse2.rs`, `MountImpl::umount_impl`, the `fuse_unmount_compat22`
  block: not compiled (its cfg leaves out macOS, and Linux does not build
  `fuse2.rs`).
- `mnt/fuse3.rs`, 6 tokens (`fuse_session_new`, `fuse_session_mount`,
  `fuse_session_fd`, `BorrowedFd::borrow_raw`, the unmount block,
  `unsafe impl Send for MountImpl`): not compiled (`libfuse3` only).
- `mnt/fuse3_sys.rs`, the `unsafe extern "C"` block: not compiled
  (`libfuse3` only).
- `mnt/mod.rs`, the `CStr::from_ptr` of a test of `with_fuse_args`: test
  code; it runs in the vendored lib tests of the CI job `linux`.
- `examples/passthrough_fork.rs` (`wrap_backing`, `fork`,
  `OwnedFd::from_raw_fd`) and `examples/simple.rs` (3 tokens): fuser
  examples. SliceFS does not build them.

## Benchmark tools

These tools run by hand only. They are not part of the daemon.

- `benchmarks/chunk-bench/src/main.rs`: a zeroed `rusage` and `getrusage`.
- `benchmarks/index-bench/src/main.rs`: zeroed `rusage` and `getrusage`
  (three places), `task_info` for the RSS on macOS (two places, with a
  zeroed `mach_task_basic_info`), `sysconf(_SC_PAGESIZE)` and `sync`.

## Follow-ups

None of these is a memory-safety bug. This review changes no code.

1. `unmount.rs` and `fuset_watch.rs`: skip a pid that is not `pid > 1`
   before `kill`, with a unit test on `fuse_t_servers_of`.
2. `signals.rs`: save and restore `errno` in `on_signal`.
3. Comment-only: a `SAFETY` comment on the four `unsafe` blocks of
   `filesystem.rs` `compute_statfs`, and the `alloc_tune.rs` macOS comment
   on the call line.

## Counts

Taken with `grep -ow unsafe` per `.rs` file under `crates`, `benchmarks`
and `vendor/fuser`, sorted by path. `scripts/safety/unsafe-count.sh`
compares these lines with the code.

- count: benchmarks/chunk-bench/src/main.rs 2
- count: benchmarks/index-bench/src/main.rs 11
- count: crates/chunk-store/src/pack_io.rs 12
- count: crates/chunk-store/tests/gc_full_memory_tests.rs 9
- count: crates/metadata/src/store.rs 2
- count: crates/slicefs-cli/src/alloc_tune.rs 4
- count: crates/slicefs-cli/src/filesystem.rs 4
- count: crates/slicefs-cli/src/fuset_watch.rs 2
- count: crates/slicefs-cli/src/gc_pacing.rs 2
- count: crates/slicefs-cli/src/signals.rs 5
- count: crates/slicefs-cli/src/unmount.rs 2
- count: crates/slicefs-cli/tests/dirty_budget_tests.rs 9
- count: crates/slicefs-cli/tests/memory_bound_tests.rs 9
- count: crates/slicefs-cli/tests/no_take_out.rs 3
- count: crates/slicefs-cli/tests/replay_memory_tests.rs 9
- count: crates/slicefs-cli/tests/reply_after_locks.rs 2
- count: crates/slicefs-cli/tests/signal_tests.rs 3
- count: crates/slicefs-cli/tests/stress_harness.rs 2
- count: vendor/fuser/examples/passthrough_fork.rs 3
- count: vendor/fuser/examples/simple.rs 3
- count: vendor/fuser/src/channel.rs 3
- count: vendor/fuser/src/forget_one.rs 1
- count: vendor/fuser/src/mnt/fuse2.rs 3
- count: vendor/fuser/src/mnt/fuse2_sys.rs 1
- count: vendor/fuser/src/mnt/fuse3.rs 6
- count: vendor/fuser/src/mnt/fuse3_sys.rs 1
- count: vendor/fuser/src/mnt/fuse_pure.rs 5
- count: vendor/fuser/src/mnt/mod.rs 1
- count: vendor/fuser/src/passthrough.rs 3
- count: vendor/fuser/src/reply.rs 6
