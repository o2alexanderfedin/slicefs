# Local patches to fuser 0.19.0

This is the fuser source tree at tag `v0.19.0` (upstream commit `893dfeb`), with the patches
below. SliceFS uses it through `[patch.crates-io]` in the workspace `Cargo.toml`.
Drop this copy when upstream ships the fixes.

All three patches are for FUSE-T on macOS. They were found with FUSE-T 1.2.x; see the
upstream issue https://github.com/cberner/fuser/issues/761.

## 1. Size the INIT reply for the negotiated protocol minor version

Upstream PR: https://github.com/cberner/fuser/pull/789 (open).
Files: `src/ll/fuse_abi.rs`, `src/ll/request.rs` (`Init::reply()`, unit tests).

fuser declares protocol 7.19 on macOS but picked the `fuse_init_out` size from the peer's
minor version only. FUSE-T offers 7.23, so fuser sent the 64-byte 7.23 reply while claiming
7.19. FUSE-T uses the INIT reply to choose a protocol profile: a 64-byte reply makes it pick
its libfuse3 profile with Linux struct layouts, but fuser then sends macOS layouts, so FUSE-T
closes the connection after the first GETATTR. With the reply sized for
`min(peer minor, our minor)` (24 bytes for 7.19), FUSE-T picks its libfuse2-compatible profile
with macOS layouts.

## 2. Parse the `fuse_rename_in` layout that the peer sends on macOS

Upstream PR: https://github.com/cberner/fuser/pull/789 (open), same commit as patch 1.
File: `src/ll/request.rs` (`FUSE_RENAME` parsing, unit tests).

fuser always parsed `fuse_rename_in` on macOS with macFUSE's extended layout, which carries
the `renamex_np` flags. macFUSE sends that layout only when FUSE_RENAME_SWAP was negotiated.
FUSE-T sends the plain 8-byte layout, so the names were read from the wrong offset and `mv`
failed. fuser now parses the extension only when FUSE_RENAME_SWAP was negotiated, as macFUSE's
own libfuse does.

## 3. Read and write whole messages on a stream socket

Not yet sent upstream.
Files: `src/channel.rs` (`StreamSocket`, unit tests), `src/dev_fuse.rs` (`DevFuse` keeps the
detected socket state; `DevFuse::new`), and the places that build a `DevFuse`
(`src/session.rs`, `src/notify.rs` test, `src/mnt/fuse2.rs`, `src/mnt/fuse3.rs`,
`src/mnt/fuse_pure.rs`).

FUSE-T hands libfuse a `SOCK_STREAM` socket instead of a character device. A device returns
one whole request per `read()` and takes one whole reply per `writev()`; a stream socket can
split either. A large WRITE then arrived in parts ("Short read of FUSE request"), was dropped,
never answered, and `cp` into the mount hung.

When the fd is a stream socket (checked once with `getsockopt(SO_TYPE) == SOCK_STREAM`),
`receive()` reads the 40-byte header, then exactly the rest of the request, and maps EOF
before a request to `ENODEV` (clean exit on unmount). `send()` loops until the whole reply
is written. Reads and writes each take their own lock, so concurrent readers or senders
cannot interleave bytes. Devices and seqpacket sockets are unchanged. This mirrors
`lib/fuse_kern_chan.c` in FUSE-T's libfuse.
