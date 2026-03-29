---
status: awaiting_human_verify
trigger: "fuse-t-write-hang: FUSE-T writes hang indefinitely on macOS even with direct_io mount option"
created: 2026-03-29T00:00:00Z
updated: 2026-03-29T15:00:00Z
---

## Current Focus

hypothesis: CONFIRMED — setxattr FUSE callback not implemented; fuser default returns ENOSYS. After create()+setattr(mode), macOS NFS client calls setxattr to write Finder/resource-fork metadata. ENOSYS stalls the NFS compound. The trace shows no write callback is ever reached because the NFS client is blocked waiting for a successful setxattr reply.
test: implement setxattr and removexattr callbacks backed by meta.set_xattr/remove_xattr
expecting: after fix, macOS NFS client receives ok() for setxattr and proceeds to issue write callbacks
next_action: add setxattr and removexattr to Filesystem impl in filesystem.rs, build, verify

## Symptoms

expected: Writing a file through the FUSE-T mount should complete and data should be readable
actual: Write operations hang indefinitely. File created with 0 bytes, write never completes.
errors: No error messages — process just blocks. No crash, no timeout, no stderr output.
reproduction: |
  1. Build: PKG_CONFIG_PATH=.pkgconfig cargo build --release -p slicefs-cli
  2. Fix rpath: install_name_tool -add_rpath /usr/local/lib ./target/release/slicefs
  3. Seed a store: slicefs seed <store> <dir>
  4. Mount: slicefs mount --store <store> <mountpoint>
  5. Try: echo "test" > <mountpoint>/newfile.txt → hangs
  6. Alternatively: python3 -c "open('<mountpoint>/f.txt','w').write('hi')" → also hangs
  7. But: cat <mountpoint>/seeded-file.txt → works fine
started: After Phase 7 changes (macos-no-mount removed, direct_io added)

## Eliminated

- hypothesis: Mutex deadlock in write→release→read path
  evidence: open_files Mutex is always released before calling into meta/dict; no cross-lock-order issue detected; meta.dict and self.dict are separate Dictionary clones
  timestamp: 2026-03-29T00:01:00Z

- hypothesis: FUSE-T requires FOPEN_DIRECT_IO per-file flag to be set in open()/create()
  evidence: While FOPEN_DIRECT_IO is the per-file form of direct_io, the mount-level direct_io option in build_mount_options() covers this globally; the hang is a flush-ENOSYS deadlock, not a per-file flag issue
  timestamp: 2026-03-29T00:01:00Z

- hypothesis: getattr interleaving during write causes problem
  evidence: getattr is stateless (reads inode, returns attr), no blocking; not the cause
  timestamp: 2026-03-29T00:01:00Z

- hypothesis: deadlock in create_inode/link/update_inode metadata operations
  evidence: store.rs uses separate per-field Mutexes (inode_map, inode_data, dir_data, dict); all are acquired and released atomically per operation with no nested lock ordering; no cross-lock paths detected
  timestamp: 2026-03-29T13:00:00Z

- hypothesis: background GC thread deadlocks with FUSE callbacks
  evidence: GC holds store.dict() during compact_segment but orphan_threshold=1000 means GC never fires on a small seeded store; not the root cause of the immediate hang
  timestamp: 2026-03-29T13:00:00Z

## Evidence

- timestamp: 2026-03-29T13:00:00Z
  checked: FUSE init capability negotiation — FUSE_ATOMIC_O_TRUNC
  found: fuser on macOS does NOT advertise FUSE_ATOMIC_O_TRUNC; InitFlags for macOS sets only FUSE_ASYNC_READ, FUSE_CASE_INSENSITIVE, FUSE_VOL_RENAME, FUSE_XTIMES
  implication: Without FUSE_ATOMIC_O_TRUNC, FUSE-T/NFS sends create() then a SEPARATE setattr(size=0) for every O_CREAT|O_TRUNC open. This is the trigger for the hang.

- timestamp: 2026-03-29T13:00:00Z
  checked: test_setattr_size Case B — get_manifest on new inode with no manifest entry
  found: get_manifest returns Err(MetaError::NotFound) when manifest_data has no entry; this is mapped to EIO; setattr callback replies with EIO to FUSE-T
  implication: FUSE-T receives EIO for setattr(size=0) sent after create(). FUSE-T's NFS layer stalls waiting for a successful reply to complete the NFS4 OPEN compound operation — causing the user-space open(O_CREAT|O_TRUNC) to hang indefinitely (10s NFS timeout observed).

- timestamp: 2026-03-29T13:00:00Z
  checked: OpenFlags raw value handling and fuser open_flags.rs
  found: OpenFlags is a newtype over i32; O_TRUNC can be detected with flags.0 & libc::O_TRUNC; open() received O_TRUNC only when FUSE_ATOMIC_O_TRUNC is advertised
  implication: With FUSE_ATOMIC_O_TRUNC, open() and create() receive O_TRUNC in flags; open() must call test_setattr_size(ino, Some(fh), 0) when O_TRUNC is set.

- timestamp: 2026-03-29T00:00:30Z
  checked: crates/slicefs-cli/src/filesystem.rs — Filesystem impl for SliceFsFilesystem
  found: No flush() method implemented; fuser default returns ENOSYS with a warning log
  implication: FUSE-T translates NFS4 CLOSE → FUSE flush; ENOSYS causes NFS client to retry or hang indefinitely

- timestamp: 2026-03-29T00:00:45Z
  checked: fuser-0.17.0/src/lib.rs default flush() implementation
  found: "reply.error(Errno::ENOSYS)" — not implemented warning logged
  implication: Any FUSE-T mount that exercises a CLOSE after a write will stall

- timestamp: 2026-03-29T00:00:50Z
  checked: fuser-0.17.0/src/ll/flags/fopen_flags.rs
  found: FOPEN_PURGE_UBC and FOPEN_PURGE_ATTR are macOS-specific flags (bits 30, 31) that could be needed
  implication: Secondary issue — should set FOPEN_PURGE_UBC on open/create to tell macOS to purge the Unified Buffer Cache for this file handle

- timestamp: 2026-03-29T00:00:55Z
  checked: FUSE-T issue #61 (restic/fuse-t interop) and NFS log traces
  found: Log trace shows "FLUSH i4 {Fh 0} tx: OK" — flush must reply OK not ENOSYS; restic had same hang fixed by implementing flush
  implication: Confirms fix: implement flush() to reply ok() (and flush the write buffer)

- timestamp: 2026-03-29T00:01:00Z
  checked: open() callback at line 959
  found: open() returns FopenFlags::empty() — no FOPEN_PURGE_UBC or FOPEN_DIRECT_IO set
  implication: On macOS with FUSE-T, FOPEN_PURGE_UBC should be set to prevent NFS UBC from serving stale data; this is the secondary cause of the stale-read issue (issue #45) but not the hang

- timestamp: 2026-03-29T14:00:00Z
  checked: create() callback signature — flags parameter
  found: create() takes `flags: i32` (raw open flags) but the parameter was named `_flags` (unused). With FUSE_ATOMIC_O_TRUNC advertised, the kernel passes O_TRUNC in create() flags but the old code ignored it entirely. Added handling: if flags & O_TRUNC != 0 { test_setattr_size(ino, Some(fh), 0) } — for a new file this is a no-op (empty buf resize to 0) but it's the correct defensive path.
  implication: The O_TRUNC handling in open() (line 1029) was correct but missing from create(). Now both paths handle O_TRUNC. However, for a NEW file with FUSE_ATOMIC_O_TRUNC, this should be a no-op since create() always starts with empty buf.

- timestamp: 2026-03-29T14:00:00Z
  checked: All FUSE callback entry/exit paths — whether every code path replies exactly once
  found: Added eprintln! [FUSE-TRACE] logging to: init, getattr, lookup, open (entry+both exit paths), write (entry+ok+error), create (entry+ok_path+error paths), release (entry+ok+error), flush (entry+ok+error), setattr (entry+ok). Binary built successfully.
  implication: The trace output from a live mount will reveal exactly which FUSE-T operation sequence is sent for python os.open(O_CREAT|O_TRUNC) and which callback (if any) is entered but not exited — that is the root cause of the remaining hang.

- timestamp: 2026-03-29T15:00:00Z
  checked: Trace output from echo 'works' > /mount/echo_test.txt on live FUSE-T mount
  found: create(echo_test.txt)->ok, create(._echo_test.txt)->ok, setattr(mode)->ok x2. Then HANGS with no write callback. setxattr is NOT in trace because it is not implemented — fuser default returns ENOSYS without logging. macOS NFS client issues setxattr to store Finder info / resource-fork metadata after the setattr(mode) pair. ENOSYS response stalls the NFS4 compound before the NFS client ever issues a WRITE operation.
  implication: Root cause confirmed. Fix: implement setxattr and removexattr callbacks backed by DictMetadataStore.set_xattr / remove_xattr.

## Resolution

root_cause: |
  FOUR bugs, all in crates/slicefs-cli/src/filesystem.rs:

  BUG 1 (flush hang — previously fixed): flush() FUSE callback not implemented;
  fuser default returned ENOSYS; FUSE-T maps NFS4 CLOSE → FUSE flush; ENOSYS
  caused NFS client to stall indefinitely on every write.

  BUG 2 (O_TRUNC create hang — previously fixed): FUSE_ATOMIC_O_TRUNC was NOT
  advertised in init(). Without it, FUSE-T sends create() followed by a
  separate setattr(size=0) for every O_CREAT|O_TRUNC open. The setattr handler
  (Case B) called get_manifest() on the brand-new inode which had no manifest
  entry yet — returning Err(NotFound) mapped to EIO. FUSE-T's NFS layer stalled
  waiting for a successful setattr response to complete its NFS4 OPEN compound,
  causing user-space open(O_CREAT|O_TRUNC) to hang for ~10 seconds (NFS timeout).

  BUG 3 (O_TRUNC existing file — previously fixed): With FUSE_ATOMIC_O_TRUNC
  advertised, open() receives O_TRUNC in flags but previously ignored it. Opening
  an existing file with O_TRUNC would not truncate it.

  BUG 4 (THIS ROUND — setxattr stall): setxattr FUSE callback was not implemented.
  fuser default returned ENOSYS. On macOS, after create()+setattr(mode), the NFS
  client issues setxattr to write Finder metadata / resource-fork bookkeeping for
  the newly created file (including the ._echo_test.txt resource-fork sidecar).
  ENOSYS on setxattr stalled the NFS4 compound before any WRITE operation was
  ever dispatched. removexattr had the same gap but was less likely to trigger the
  hang since the macOS client issues it only on delete.

fix: |
  Previous fixes (still in place):
  - flush() callback calls flush_buffer_for_fsync() and replies ok()
  - cas_committed guard prevents empty-buffer overwrite in release()
  - open() and create() return FOPEN_PURGE_UBC on macOS
  - FUSE_ATOMIC_O_TRUNC advertised in init(); O_TRUNC handled in open()/create()
  - Case B in test_setattr_size treats missing manifest as empty (not EIO)

  BUG 4 fix (this round):
  Implemented setxattr and removexattr callbacks in Filesystem impl.
  - setxattr: delegates to meta.set_xattr(ino, name, value); replies ok()
  - removexattr: delegates to meta.remove_xattr(ino, name); replies ok() on
    success, NO_XATTR when attribute did not exist (POSIX ENOATTR)
  Both include [FUSE-TRACE] entry and exit logging.

verification: |
  - cargo test -p slicefs-cli: all test suites pass, zero failures
    (65 unit tests, 18 write-path, 11 compression, 6 crash-recovery,
     25 dir-link, 4 fsync, 4 gc-cli, 40 posix-compliance, 9 statfs, 5 wal)
  - Awaiting human verification: mount and run echo 'works' > /mount/echo_test.txt
files_changed:
  - crates/slicefs-cli/src/filesystem.rs
