# Local patches to fuser 0.18.0

This is fuser 0.18.0 from crates.io (upstream commit `9c957f74efe715112049298cdf1d601781829c8d`)
with the patches below. SliceFS uses it through `[patch.crates-io]` in the workspace `Cargo.toml`.
Drop this copy when upstream ships the fix.

## 1. Size the INIT reply for the negotiated protocol minor version

File: `src/ll/request.rs`, `Init::reply()`.

fuser declares protocol 7.19 on macOS (`FUSE_KERNEL_MINOR_VERSION = 19`) but picked the
`fuse_init_out` size from the *kernel's* minor version only. When the peer announces minor >= 23,
fuser sent the full 64-byte struct while claiming 7.19.

FUSE-T 1.2.x uses the INIT reply to choose a protocol profile. A 64-byte reply makes it pick the
"v3" (libfuse3, Linux struct layout) profile, but fuser then sends macOS layouts (for example
`fuse_attr` with `crtime`/`flags`), so FUSE-T closes the connection after the first GETATTR and
fuser reports "Invalid request" (a 0-byte read). With the reply sized for
`min(kernel minor, our minor)` (24 bytes for 7.19), FUSE-T picks the "v2" (libfuse2-compatible,
macOS layout) profile and everything works.


## 2. Framed I/O when the channel fd is a stream socket (FUSE-T)

File: `src/channel.rs`.

FUSE-T hands libfuse a `SOCK_STREAM` socket instead of a character device. fuser read one
request per `read()` and wrote one reply per `writev()`, which is only correct for a device.
On the socket, a 512 KiB WRITE arrived in pieces ("Short read of FUSE request (80 < 524368)"),
the request was dropped, never answered, and `cp` into the mount hung (on every backend).

When the fd is a socket (checked once with `fstat`), `receive()` now reads the 40-byte header,
then exactly the rest of the request, and maps EOF to `ENODEV` (clean exit on unmount).
`send()` takes a lock and loops until the whole reply is written. Device fds are unchanged.
This mirrors `lib/fuse_kern_chan.c` in FUSE-T's libfuse.

## 3. Parse both `fuse_rename_in` layouts on macOS

Files: `src/ll/request.rs` (`FUSE_RENAME` parsing, `Rename` struct, 2 new unit tests),
`src/ll/argument.rs` (`ArgumentIterator::peek`).

With `macfuse-4-compat`, fuser always parsed `fuse_rename_in` as 16 bytes (`newdir`, `flags`,
`padding`). The peer uses that layout only when FUSE_RENAME_SWAP was negotiated; otherwise it
sends the 8-byte compat layout (`newdir` only). FUSE-T sends the 8-byte layout, so the old
names were misparsed, fuser answered ENOSYS, and `mv` failed with "Permission denied".

fuser now reads `newdir`, and on macOS skips `flags`/`padding` only if the remaining bytes
contain more than two NULs. The two names never contain NUL, so the compat layout leaves
exactly two, while the extended layout adds at least four zero padding bytes. The rename
`flags` were never used by fuser (it always passes `RenameFlags::empty()`), so nothing is lost.

## Upstream

Reported upstream: https://github.com/cberner/fuser/issues/761 — drop this copy once fuser ships the fixes.
