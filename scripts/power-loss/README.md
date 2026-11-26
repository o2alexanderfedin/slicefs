# Power-loss test of `slicefs rehash`

This test cuts the power at each sync point of a rehash and checks the
store after the crash. It runs in a Linux Docker container. It answers
review item IN-04 of Phase 23: the unit tests stop the process, but they
do not drop the data that was written and not synced.

## What it proves

The store is on [LazyFS](https://github.com/dsrhaslab/lazyfs) (release
0.3.1, pinned by commit in the `Dockerfile`). LazyFS is a FUSE file system
that keeps written data in its own cache until `fsync`. Its `clear-cache`
command drops all data that is not synced, as a power loss does.

A small store is made first: a snapshot root and a current root, a value
shared by two files and by both roots, about 0.6 MB in total. A clean
rehash under the shim `plshim.c` counts the sync-type calls of the run
(`fsync`, `fdatasync`, `rename`, `renameat`, `renameat2`). With N calls
there are N + 1 crash points: just before each call, and after the end.

For each point the script:

1. Copies the store into the LazyFS root and mounts LazyFS.
2. Runs `slicefs rehash --to sha256` with the shim. The shim kills the
   process with SIGKILL just before its N-th sync-type call. A point where
   the kill does not fire is a failure.
3. Sends `lazyfs::clear-cache` (the power loss), then unmounts and mounts
   LazyFS again.
4. Checks the store: `slicefs scrub` is clean, and the roots are the old
   roots or the new roots of a clean run. The snapshot root and the count of
   live objects of the old hash (from `rehash --dry-run`) decide. A mix is
   a failure.
5. Mounts a copy of the store with slicefs and compares the current tree
   and snapshot 1 byte for byte with the source files.
6. Runs rehash again with no fault. It must exit 0. Then: scrub is clean,
   the roots are the new roots, no live object has the old hash, the GC
   keeps the same object count as a clean run, `FORMAT` records the target
   hash, and no `rehash-tmp` or segment temp file is left. The files are
   compared again.

Each point prints one line, and the run ends with
`POWERLOSS points=N ok=N fail=0 old_root=A new_root=B`. The exit code is
not 0 if a point fails.

The test can find a real defect: with the chunk-store sync before the
switch removed from `rehash.rs` (a test change, not committed), 6 of 46
points failed. They were the points after the switch rename and before the
next pack sync, and scrub found 4 errors at each of them.

## How to run

```sh
scripts/power-loss/run.sh
```

It needs Docker with `/dev/fuse` (OrbStack or Docker Desktop on macOS). All
mounts are in the container. The first run builds the image and slicefs
(a few minutes); then a run takes about 5 minutes for 49 points.

Variables: `PL_TO` (target hash, default `sha256`), `PL_ONLY=N` (one point),
`PL_DEADLINE` (time limit of the loop in seconds, default 900),
`PL_OUT=<dir>` (the crashed stores and the logs of failed points),
`SLICEFS_TARGET_VOL` (cargo target volume, default
`slicefs-powerloss-target`).

## What it does not cover

- Renames and other metadata changes (create, unlink, mkdir) are durable at
  once in LazyFS: it passes them to the disk below with no cache. Thus a
  rename that is lost because the directory was not synced is not tested,
  and a missing directory `fsync` cannot be found. (FUSE has no
  `fsyncdir` in LazyFS; the kernel then reports success.)
- Unsynced data is dropped whole. There are no torn writes (part of a write
  kept) and no reordered writes. LazyFS can inject these, but this test
  does not.
- The crash points are the sync-type calls of the process. A crash between
  two of them is the same as a crash at the next one, because the data in
  between is not synced; only the metadata changes in between differ (see
  the first item).
- One small store and one direction (`blake3-256` to `sha256`). No large
  store, no store with a mixed hash at the start, no crash during the
  rerun (one crash per point).
- `rename` calls that do not go through the C library (a raw system call)
  are not counted as points. The slicefs code uses the standard library,
  which calls the C library.
