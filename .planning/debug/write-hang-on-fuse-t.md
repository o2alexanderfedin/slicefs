---
status: awaiting_human_verify
trigger: "SliceFS FUSE-T mount on macOS: ALL write operations hang indefinitely"
created: 2026-03-30T12:00:00Z
updated: 2026-03-30T13:00:00Z
---

## Current Focus

hypothesis: FUSE-T NFS4 server uses MKNOD+OPEN for file creation instead of atomic CREATE. mknod() returned ENOSYS, causing NFS client stall on hard-mount.
test: Implemented mknod() for regular files. Also fixed release() manifest erasure for read-only opens.
expecting: File creation via FUSE-T will now succeed via MKNOD+OPEN path, unblocking all write operations.
next_action: User must test on live FUSE-T mount.

## Symptoms

expected: Write operations (create file, copy file, echo into file) should complete normally
actual: ALL write operations hang indefinitely. "Zero bytes of 429 bytes" in Finder copy dialog. cp command never returns even with timeout.
errors: No error messages - operations just block forever. No crash, no timeout, no stderr.
reproduction: |
  1. Build: cargo build --release -p slicefs-cli
  2. Mount: slicefs mount --store .slicefs-store /tmp/slicefs-mount --noatime
  3. Reading works: ls /tmp/slicefs-mount, cat works
  4. Writing hangs: cp, echo > file, Finder duplicate all hang
started: Ongoing issue. Previous debug sessions fixed flush ENOSYS, O_TRUNC, setxattr ENOSYS but writes still hang.

## Eliminated

- hypothesis: Mutex deadlock between open_files and io locks
  evidence: Prior audit found no deadlock cycle. fuser on macOS uses SINGLE THREAD (n_threads != 1 only supported on Linux), so no FUSE callback concurrency exists.
  timestamp: 2026-03-30T12:05:00Z

- hypothesis: Missing reply in FUSE callback causes kernel to wait forever
  evidence: fuser's ReplyRaw::Drop sends EIO if reply is not sent. All code paths in our callbacks call reply.ok()/reply.error()/reply.created()/etc. Even if reply is dropped, EIO is sent.
  timestamp: 2026-03-30T12:10:00Z

- hypothesis: NFS file locking (getlk/setlk) ENOSYS causes hang
  evidence: fuser defaults for getlk/setlk return ENOSYS. FUSE protocol states ENOSYS for setlk is treated as success. NFS client implements file locking automatically when fs doesn't support it.
  timestamp: 2026-03-30T12:12:00Z

- hypothesis: copy_file_range ENOSYS causes hang
  evidence: copy_file_range is Linux-specific (ABI 7.28). FUSE-T/NFS4 doesn't use it. Default ENOSYS causes kernel fallback to read+write.
  timestamp: 2026-03-30T12:13:00Z

- hypothesis: FUSE_ATOMIC_O_TRUNC not supported by FUSE-T causes setattr(size=0) hang
  evidence: Even without FUSE_ATOMIC_O_TRUNC, setattr(size=0) on a brand-new inode works: NotFound manifest -> empty content -> set empty manifest -> update inode. No error or hang.
  timestamp: 2026-03-30T12:15:00Z

## Evidence

- timestamp: 2026-03-30T12:05:00Z
  checked: fuser 0.17.0 session.rs line 256
  found: "n_threads != 1 is only supported on Linux" - on macOS, fuser uses exactly 1 thread for ALL FUSE operations
  implication: No FUSE callback concurrency on macOS. Mutex contention between FUSE callbacks is impossible. BUT: if the single thread panics, all FUSE operations hang.

- timestamp: 2026-03-30T12:08:00Z
  checked: FileStorageAdd::save_and_clean (file_storage.rs line 74) and FileStorageAdd::drop (line 80-93)
  found: self.io.write(&path, &v).unwrap() - panics on ANY I/O error during CAS storage write
  implication: If CAS write fails (disk full, permission denied, path issue), the FUSE thread panics. fuser does NOT use catch_unwind. Panic kills the event loop thread. FUSE-T NFS connection stays alive but gets no responses -> ALL operations hang.

- timestamp: 2026-03-30T12:10:00Z
  checked: fuser reply.rs ReplyRaw::Drop (line 139-149)
  found: Drop handler sends EIO if reply not sent. This fires during panic unwinding.
  implication: The current operation gets EIO, but all SUBSEQUENT operations hang because the event loop thread is dead.

- timestamp: 2026-03-30T12:12:00Z
  checked: fallocate() implementation (filesystem.rs line 1813-1824)
  found: Returns Errno::EROFS ("Read-only file system") instead of ENOSYS or EOPNOTSUPP
  implication: If NFS client or FUSE-T calls fallocate (e.g., for space preallocation), EROFS tells it the filesystem is read-only, which could prevent further writes.

- timestamp: 2026-03-30T12:18:00Z
  checked: fuser default init flags on macOS (lib.rs line 114-117)
  found: macOS defaults are FUSE_ASYNC_READ | FUSE_CASE_INSENSITIVE | FUSE_VOL_RENAME | FUSE_XTIMES. FUSE_BIG_WRITES is NOT included (unlike non-macOS).
  implication: Without FUSE_BIG_WRITES, write size is limited to single page. Not a hang cause but limits performance.

- timestamp: 2026-03-30T12:20:00Z
  checked: Complete FUSE callback coverage in filesystem.rs
  found: All write-related callbacks implemented: create, open, write, flush, fsync, release, setattr. No missing callbacks that would cause hang.
  implication: The hang is not caused by a missing callback returning ENOSYS.

- timestamp: 2026-03-30T12:25:00Z
  checked: FUSE-T wiki and GitHub issues
  found: FUSE-T wiki mentions "call sequence might look very different from the original osxfuse" and a workaround "Don't fail NFS Read/Write op on a closed file handle (ReOpen -> Read/Write -> Close)". Issue #61 reports NFS client "endless loop of opening and immediately closing" on Sonoma.
  implication: FUSE-T NFS layer has known behavioral differences that can cause hangs when filesystem doesn't handle edge cases gracefully.

## Resolution

root_cause: FUSE-T's NFS4 server uses FUSE_MKNOD + FUSE_OPEN for file creation instead of FUSE_CREATE. This is documented behavior for FUSE filesystems exported over NFS. SliceFS's mknod() returned ENOSYS for ALL file types (including regular files), causing the NFS4 file creation to fail. Since macOS NFS uses "hard" mount semantics by default, the NFS client retries indefinitely on failure -- producing the observed hang-forever behavior for ALL write operations (which all start with file creation).

Secondary issue: release() would set an empty manifest on close for any file opened with O_RDWR but never written to. FUSE-T is documented to potentially open read-only files as O_RDWR (issue #15), which would erase existing file content on close.

fix: Two changes applied:

1. **filesystem.rs mknod()**: Implemented mknod for regular files (S_IFREG). Creates the inode and links it to the parent directory, but does NOT allocate a file handle (the subsequent open() does that). Non-regular file types (device nodes, FIFOs, sockets) still return ENOSYS.

2. **filesystem.rs test_release()**: When byte_count==0 (no writes through this handle), check if the file already has a manifest before setting an empty one. Prevents FUSE-T's O_RDWR read-only opens from erasing file content on release.

verification: All 550 workspace tests pass (0 failures). New tests added for mknod regular file creation + write flow. Release binary builds. Awaiting human verification on live FUSE-T mount.
files_changed:
- crates/slicefs-cli/src/filesystem.rs (mknod implementation + release manifest safety check)
- crates/slicefs-cli/tests/write_path_tests.rs (3 new tests: mknod regular file, mknod+write flow, mknod zero-type-bits)
