# Local patches to fuser 0.19.0

This is the fuser source tree at tag `v0.19.0` (upstream commit `893dfeb`), with the patches
below. SliceFS uses it through `[patch.crates-io]` in the workspace `Cargo.toml`.
Drop this copy only when a fuser release has all four fixes AND the FUSE-T smoke workflow
(`.github/workflows/fuse-t-smoke.yml`: a real FUSE-T mount with a 1 GiB copy, renames and a
remount) passes on that release. Unit tests are not enough: PR #789 as it is passes them and
still breaks `mv` on FUSE-T (patch 2).

Patches 1 to 3 are for FUSE-T on macOS. They were found with FUSE-T 1.2.x; see the
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

Upstream PR: https://github.com/cberner/fuser/pull/789 (open), same commit as patch 1, with a local
change (below) that the PR does not have.
Files: `src/ll/request.rs` (`FUSE_RENAME` parsing, unit tests), `src/ll/argument.rs`
(`ArgumentIterator::peek`).

fuser always parsed `fuse_rename_in` on macOS with macFUSE's extended layout, which carries
the `renamex_np` flags. macFUSE sends that layout only when FUSE_RENAME_SWAP was negotiated.
FUSE-T sends the plain 8-byte layout, so the names were read from the wrong offset and `mv`
failed.

PR #789 parses the extension only when FUSE_RENAME_SWAP was negotiated. That is not enough for
FUSE-T 1.2.9: it sets bits 25 and 26 in its INIT flags, which fuser reads on macOS as
FUSE_RENAME_SWAP and FUSE_RENAME_EXCL (and always requests), but it still sends the plain
layout. A real mount showed it: `mv` failed with ENOSYS or EINVAL. So the local change chooses
the layout by content, as the 0.18 patch did: the two names never contain NUL, so the plain
layout leaves exactly two NULs after `newdir`, while the extended layout adds at least four
zero bytes of padding. Test: `rename_plain_layout_with_rename_swap_negotiated`.

## 3. Read and write whole messages on a stream socket

Not yet sent upstream. Fork branch with all three patches on `v0.19.0`:
https://github.com/o2alexanderfedin/fuser/tree/slicefs-0.19
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
is written. Both loops retry on `EINTR` and `EAGAIN`, as the 0.18 patch did. Reads and writes each take their own lock, so concurrent readers or senders
cannot interleave bytes. Devices and seqpacket sockets are unchanged. This mirrors
`lib/fuse_kern_chan.c` in FUSE-T's libfuse.

## 4. Pass the getxattr position (macOS)

Not sent upstream (a local change; it changes the `Filesystem::getxattr` signature).
Files: `src/ll/request.rs` (`GetXAttr::position()`, unit test `getxattr_size_and_position`),
`src/request.rs` (the `GetXAttr` dispatch), `src/lib.rs` (`Filesystem::getxattr` takes
`position: u32`), `examples/simple.rs` (the new argument, unused).

On macOS, `fuse_getxattr_in` carries a `position`: the resource fork
(`com.apple.ResourceFork`) is read in parts from that offset. fuser parsed the field and
dropped it, so every part of a read of the resource fork got the bytes from offset 0.
`GetXAttr::position()` returns it on macOS and 0 on other systems (as `SetXAttr::position()`
does), and the dispatch passes it to `Filesystem::getxattr`.

If this copy is dropped for a release without this change, the position is 0 again: reads of
the resource fork then start at offset 0, as before the patch.
