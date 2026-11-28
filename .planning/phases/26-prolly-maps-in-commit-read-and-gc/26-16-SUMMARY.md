---
phase: 26-prolly-maps-in-commit-read-and-gc
plan: 16
subsystem: slicefs-cli, vendored fuser
tags: [prolly, xattrs, bounded-memory, fuser-patch, docker-mount, pmap-03]
requires: ["26-15"]
provides:
  - "SliceFsFilesystem::getxattr_reply(ino, name, size, position) and listxattr_reply(ino, size) -> Result<XattrReply, i32>"
  - "filesystem::XattrReply { Size(u32), Data(Vec<u8>) }, filesystem::RESOURCE_FORK"
  - "handle_getxattr(fs, ino, name, size, position); handle_listxattr on the bounded replies"
  - "fuser patch 4: GetXAttr::position(), Filesystem::getxattr(.., size, position, reply)"
  - "mt-linux.sh steps xattr-big, xattr-many, xattr-remove"
affects: [26-09]
tech-stack:
  added: []
  patterns:
    - "A FUSE reply filled from a streamed store scan (ControlFlow::Break when the buffer is full)"
key-files:
  created:
    - .planning/phases/26-prolly-maps-in-commit-read-and-gc/runs/gate-26-16.log
  modified:
    - crates/slicefs-cli/src/filesystem.rs
    - crates/slicefs-cli/src/handlers.rs
    - crates/slicefs-cli/src/fuse_callbacks.rs
    - vendor/fuser/src/ll/request.rs
    - vendor/fuser/src/request.rs
    - vendor/fuser/src/lib.rs
    - vendor/fuser/examples/simple.rs
    - vendor/fuser/PATCHES.md
    - scripts/docker-linux-test/mt-linux.sh
key-decisions:
  - "Positioned getxattr reads only for com.apple.ResourceFork; a position above 0 on another name is EINVAL"
  - "getxattr: a name above 255 bytes is ERANGE (Linux errno); an empty name EINVAL; an absent name ENODATA"
  - "getxattr reads at most size + 1 bytes after the length check, so a value that grew between the two lookups is ERANGE, not a cut value"
  - "Size queries above u32::MAX are E2BIG (getxattr length, listxattr sum)"
  - "The getxattr position is a local fuser patch (patch 4), not sent upstream"
status: complete
metrics:
  completed: 2026-11-28
actuals:
  tokens: 9800
  tasks: 3
  commits: 3
plan_head_before: ef4fb9ed4b8b55488d81bf734165cf935da80be4
plan_head_after: 12e6e75904e5fb6c2d54e78f1260d05d16a2e34f
---

# Phase 26 Plan 16: xattrs in the fs layer Summary

getxattr and listxattr now keep the memory bound of the 26-15 xattr tree up to the FUSE reply. Size queries read no value byte and build no list. A buffer that is too small gives ERANGE before a value read. The macOS resource fork is read by position. The name list streams from the store scan into the reply buffer. The vendored fuser passes the getxattr position (patch 4). A Docker mount sends a spilled 60,000-byte value, 300 names and a remove through the Linux kernel.

## What was built

- `filesystem.rs`:
  - `XattrReply`, `RESOURCE_FORK` and a private `getxattr_errno` (NotFound to ENODATA, NameTooLong to ERANGE, the rest through `meta_error_to_errno`).
  - `getxattr_reply(ino, name, size, position)` works as follows:
    - A position above 0 on a name other than the fork gives EINVAL.
    - Size 0 gives `Size(xattr_len)`. A length above `u32::MAX` gives E2BIG.
    - For the fork, it returns `read_xattr(position, size)`. This works at position 0 too, with no ERANGE.
    - For any other name, `xattr_len > size` gives ERANGE with no value read. Otherwise it returns `read_xattr(0, size + 1)`, and a result above `size` also gives ERANGE.
  - `listxattr_reply(ino, size)` scans `for_each_xattr_name`:
    - Size 0 gives the sum of (name length + 1). The scan stops with E2BIG above `u32::MAX`.
    - Otherwise a growing buffer is filled. The scan stops at the first name that does not fit and gives ERANGE. Nothing is allocated up front from `size`.
  - `test_getxattr` and `test_listxattr` stay as they are for the tests that use them.
- `handlers.rs`: `handle_getxattr(fs, ino, name, size, position)` and `handle_listxattr` map the replies to `XattrResult`. The doc comments are updated.
- `fuse_callbacks.rs`:
  - `getxattr` takes `position` and passes it on. The trace line shows it.
  - It replies `NO_XATTR` only for ENODATA. Other errors (ERANGE, EINVAL, E2BIG, EIO) go out as `Errno::from_i32`.
- Vendored fuser, patch 4:
  - `GetXAttr::position()`: macOS `arg.position`, 0 on other systems.
  - The `GetXAttr` dispatch passes `x.position()`.
  - `Filesystem::getxattr` takes `position: u32`, and its doc names patch 4. `examples/simple.rs` takes the new argument and does not use it.
  - Parse test `getxattr_size_and_position`: the macOS 16-byte layout with position 4096, and the 8-byte layout elsewhere with position 0.
  - `PATCHES.md` has a "## 4." section. The drop rule says "all four fixes".
- `mt-linux.sh`, after the `xattr` step:
  - `xattr-big`: a 60,000-byte random value set as `0s<base64>`, then read back with `getfattr --only-values` and compared with `cmp`.
  - `xattr-many`: 300 `user.nN` names, then `getfattr -d` must list 300.
  - `xattr-remove`: `setfattr -x`, then `getfattr` must fail.

## Tests

Handler unit tests:

- `getxattr_size_query_gives_the_length`: 60,000 bytes. Size 0, a 65,536-byte buffer and an exact 60,000-byte buffer.
- `getxattr_too_small_buffer_is_erange`: sizes 3 and 4 against 5 bytes, an absent name, and a spilled 10,000-byte value against 9,999.
- `getxattr_name_errors`: a 305-byte name gives ERANGE, an empty name EINVAL.
- `resource_fork_reads_by_position`: 1 MiB value. Reads a middle slice, the first 4 KiB, a short tail, empty data past the end, and the size query.
- `a_position_on_another_name_is_einval`.
- `listxattr_size_query_sums_the_names` and `listxattr_too_small_buffer_is_erange`: 300 names. The size query, size - 1, size 1, and the exact size in byte order.

All existing `test_handle_*xattr*` tests and `test_xattr_set_get_list_remove` pass.

## Decisions

- Positioned reads apply only to `com.apple.ResourceFork`, as on macOS. Another name with a position above 0 gives EINVAL. This is a discretion choice of the plan.
- For getxattr, a name above 255 bytes gives ERANGE, the Linux errno. On Linux the VFS refuses such a name before FUSE, so this matters only for direct callers.
- An empty name gives EINVAL.
- A non-UTF-8 name still becomes `""` in the callback, so getxattr now replies EINVAL where it replied NO_XATTR before. Linux xattr names are bytes, but the store takes `&str`. This is recorded here, not changed.
- setxattr and removexattr keep `meta_error_to_errno`, so a long name there is still ENAMETOOLONG. The plan did not ask for a change. Linux refuses such names in the VFS first.
- After the length check, getxattr reads `size + 1` bytes at most. The read allocates at most the value length (`FileReader::read` caps it to the value). Then a replace between `xattr_len` and `read_xattr` gives ERANGE or the whole new value, never a cut value. This keeps the plan's rule that ERANGE comes before any value read.
- listxattr of a missing inode is an empty list, as before: `for_each_xattr_name` does not check the inode.
- Size queries above `u32::MAX` give E2BIG.
- Patch 4 is local and was not sent upstream.

## Deviations from Plan

1. **Workspace gate reused.** The `cargo test --workspace` section of the gate log is the Task 2 run on commit `d52598d`. Task 3 changes only `mt-linux.sh` and the log, with no Rust code, so the Rust tree is the same. Docker `run.sh all` ran the workspace again on Linux on the final tree.
2. **Fuser host tests need the bundled pkg-config path.** In a copy under the scratch folder, the fuser build script does not see the workspace `.cargo/config.toml`, so it cannot find `fuse.pc`. The gate sets `PKG_CONFIG_PATH=<repo>/.pkgconfig`. With that, 100 lib tests pass on macOS, including the new macOS-layout test.
3. **Linux branch of the parse test.** The Linux branch of `getxattr_size_and_position` ran in Docker `run.sh all` (fuser `cargo test --lib`, which passed). The gate log tail does not show that line. The little-endian guard leaves out big-endian hosts, as the other request tests do with their own constants.

No file outside `files_modified` was changed.

## Verification

`runs/gate-26-16.log` has 7 sections and 7 `rc=0` lines, with no `rc=[1-9]`:

- slicefs-cli lib: 426 tests;
- the CLI suites (dir_link, coverage_boost, e2e_filesystem, integration, posix_compliance);
- the fuser lib tests and examples on the host;
- clippy with `-D warnings`;
- `unsafe-count.sh` (files=34, tokens=144, ok);
- `cargo test --workspace`;
- Docker `run.sh all`.

Every mount step is `ok`: read, echo-write, cp-3MB, cp-again, readback, mkdir-mv, chmod, truncate, xattr, xattr-big, xattr-many, xattr-remove, rm, unmount, daemon-exit.

`retry_tests` did not fail. No lock changed, so `lock_stats_report` is not needed (rule 12).

The acceptance greps give the expected counts:

| Grep | Count |
|------|-------|
| `xattr_len` in filesystem.rs | 3 |
| `for_each_xattr_name` | 3 |
| `ERANGE` | 10 |
| `XattrResult::Error(_) =>` | 0 |
| the three tests | 3 |
| `x.position()` | 2 |
| `^## 4\.` | 1 |
| `fn position` | 2 |
| `step xattr-` | 3 |
| the three Docker ok lines | 3 |

## Commits

- `b396cc6` feat(26-16): bounded getxattr and listxattr in the fs layer (Task 1)
- `d52598d` feat(26-16): fuser patch 4, pass the getxattr position (Task 2)
- `12e6e75` test(26-16): Docker xattr steps through the kernel; gate log (Task 3)

Branch `exec/26-16`, as rule 1 says. STATE.md and ROADMAP.md are not updated by this plan (26-15 precedent; 26-08 runs in parallel).

## Open items

- **setxattr with a position** (writing the resource fork in parts) stays ignored, as before. The owner decision covers reads only.
- **XATTR_CREATE / XATTR_REPLACE** are not done. `setxattr` ignores `flags`, and `set_xattr` is a blind put. Atomic create-only or replace-only needs `MapTree::modify` in `crates/metadata` (26-08 is editing it). Nothing was changed there.
- `docs/design/store-format.md` still does not describe the xattr tree. It was not in this plan's files (26-15 noted the same).

## Notes for 26-09

- 26-16 did not touch `crates/metadata`. The xattr fs paths use only `xattr_len`, `read_xattr` and `for_each_xattr_name`. `get_xattr` and `list_xattrs` are now used only by tests and by `tests/common/mod.rs`.
- Pattern for a streamed reply: fill the reply buffer from a store scan (`ControlFlow::Break` when the next entry does not fit, then an error). This fits a readdir reply that a directory tree fills from `MapTree::scan_keys`.
- The Docker mount smoke now has the xattr steps after `truncate` and before `rm`. A step added after them sees `e.txt` with 301 user xattrs (`user.k` plus 300 names).

## Known Stubs

None.

## Threat Flags

None beyond the plan's register.

- T-26-46: the `getxattr_too_small_buffer_is_erange`, `listxattr_too_small_buffer_is_erange` and `resource_fork_reads_by_position` tests; no allocation of `size` up front.
- T-26-47: patch 4 and `resource_fork_reads_by_position`.
- T-26-48: accepted and recorded in PATCHES.md.
- T-26-SC: no package was added.

## Self-Check: PASSED

The gate log and this SUMMARY exist. Commits `b396cc6`, `d52598d` and `12e6e75` are ancestors of HEAD. The gate log has 7 `rc=0` lines, no `rc=[1-9]` line, and no home or user paths.
