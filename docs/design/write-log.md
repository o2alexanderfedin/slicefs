# Write log, part 1: durable fsync and replay (#20)

Status: design for part 1. Part 2 (overlays, re-chunk at commit, no Buffered
mode) comes with the new chunk store (#15) and #21.

## Goal

`fsync` returns only when the change is durable. A crash (including power
loss) after `fsync` returns does not lose it. A crash never makes the store
unmountable.

Today `fsync` syncs a WAL that gets records only at commit, so the metadata
of a file reaches disk only at the next timer commit (30 s by default).

## Model

```
mutating request ──► apply in memory ──► append a log record ──► reply
fsync            ──► sync the log up to now ──► reply
commit           ──► (gate held) write open handles into manifests, write
                     objects, fsync the objects, append RootUpdate, sync
mount            ──► load the last root ──► re-run the records after the
                     last RootUpdate, in order
```

One global log: the existing segment files. A `RootUpdate` record is the
cut. Every record before the last `RootUpdate` is in that root, because a
commit holds the gate exclusively (no request is in flight) and first writes
the content of each open handle into its manifest.

## Records

Frame (new segment version 2):

| Field | Size |
|---|---|
| record type | u8 |
| payload length | u32 LE |
| LSN | u64 LE |
| payload | n bytes |
| CRC32C over type, length, LSN, payload | u32 LE |

Replay stops at the first record with a bad CRC, a short read, or an LSN
that is not the previous LSN + 1 (a torn tail). The segment is cut there, and
new records go to a new segment.

Record types (one for each mutating request, with the arguments that the
request had, after name and permission checks passed):

create, mknod, mkdir, symlink, link, unlink, rmdir, rename, setattr (all
fields), setxattr, removexattr, write (ino, offset, bytes), and truncate as
part of setattr. Plus the existing RootUpdate and Snapshot.

Open and release are not logged: replay keeps one internal write handle per
inode while it re-runs writes, and finalizes all of them at the end.

## Replay

1. Load the last root.
2. Read the records after the last RootUpdate.
3. Re-run each one through the same code as the FUSE request. Inode numbers
   come out the same: allocation is deterministic from `next_ino` in the
   root plus the same sequence of requests.
4. Finalize the replay handles, commit, and start the mount.

A record that fails on replay (it cannot fail if the order is the same) stops
the mount with an error that names the LSN, so that nothing is lost silently.

## fsync and group sync

- `fsync` waits until `durable_lsn >= current_lsn` at the time of the call.
- Version 1: the caller syncs. Version 2: one flusher thread; concurrent
  `fsync` calls share a sync (needs more than one FUSE thread, #22).
- macOS: `F_FULLFSYNC` (plain `fsync` does not flush the drive cache). This
  is configurable; measure both on the Boost tree first.

## Commit changes

1. Under the gate: write the content of each open handle into its manifest
   (Streaming: an ended clone, as fsync does now; Buffered: hash the buffer).
2. Write all objects, then fsync them (or the pack file, after #8).
3. Append RootUpdate and sync the log. Only then may GC delete objects that
   the old root used.
4. Start a new segment; delete the segments before the cut.

## Cost

Each write is written twice: to the log, and at commit into the store. This
is the price of a durable fsync without a commit per fsync. Part 2 removes
the store write of data that is overwritten before a commit.

## Tests

1. Write, fsync, `kill -9`, remount with `--commit-interval 0`: content is
   the same. Also for create, rename, unlink, setattr, xattr. Fails today.
2. Unit test: `fsync` syncs the log (count real syncs).
3. A torn last record (cut the file in the middle of a record, and also
   flip a byte): the mount works and has every record before it.
4. Replay twice gives the same state (no record is applied twice after the
   replay commit).
5. Boost round trip on NFS: speed against today, with plain fsync and with
   `F_FULLFSYNC`.

## Open points from review (to resolve before code)

1. The cut: one cut function on `SliceFsFilesystem` for every commit (timer,
   GC start, unmount, snapshot). Handles with pending (RAM-only) bytes or in
   Buffered mode are not flushed under the gate: the cut records, per such
   inode, the LSN from which its write/truncate records must be replayed
   (idempotent in order), and segments are kept back to the lowest such LSN.
2. Determinism: log the clock times the code took (create, `touch_dir`) and
   the inode number each create returned; replay with the logged time and
   assert the inode. `open` with `O_TRUNC` is a truncate record. Unlink of an
   open file: later writes to it are skipped on replay. Append a record only
   after the apply succeeded; no logging during replay.
3. Existing code that loses records: `compact_segment` keeps only RootUpdate
   and Snapshot (offline `slicefs gc` runs it on every segment); every caller
   of `load_store_from_segments` (gc, snapshot, stats, scrub) must replay
   first or refuse.
4. Tests to add: differential (live state against replayed state, with times,
   over random sequences incl. two handles on one inode, O_TRUNC, unlink while
   open, rename over, out-of-order and sparse writes); sync order (cut the
   active segment to its last synced length, then replay); a log-size commit
   trigger (also with `--commit-interval 0`); measure the cost of fsyncing
   each object before pack files exist.
