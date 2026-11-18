# Write log, part 1: durable fsync and replay (#20)

Status: part 1 is done. Part 2 is done for the write path by #98: overlays
replace the full-file buffer (there is no Buffered mode). The re-chunk is a
rebase at the dirty limit and at close, not at commit. See "Overlays (#98,
part 2)".

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

1. Under the gate: write the content of each open stream into its manifest
   (an ended clone, as fsync does). An overlay is not hashed: its base is
   already in the manifest, and its extents are logged again after the cut
   (see "Overlays").
2. Write all objects, then fsync them (or the pack file, after #8).
3. Append RootUpdate and sync the log. Only then may GC delete objects that
   the old root used.
4. Start a new segment; delete the segments before the cut.

## Overlays (#98, part 2)

A write that does not continue a stream (an out-of-order write past the
reorder window, a write to a file with content, a truncate that is not a
pure extension) switches the handle to an overlay. No code reads the whole
file into RAM.

- The base is a content root that the manifest of the inode names. At the
  switch, the base is installed in the manifest at once, and the inode size
  is stamped. So the base is a GC root through the manifest, and online GC
  also adds the base of each open overlay to its roots.
- Extents hold the bytes written since the base: a sparse map from offset to
  bytes. A read merges the base, zeros and the extents. A truncate or an
  extend changes only the sizes and the extents. An extent grows by at most
  1/8 of its length, so its capacity stays near the written bytes.
- Dirty limit (`DIRTY_LIMIT`, per open file): when the RAM of the extents
  passes it, the write that passed it runs a rebase before it returns. A
  rebase also runs at release and at unmount. A size change of a file with
  no write handle uses the same splice. A rebase stores base plus extents
  as a new root through the splice (see the chunk-store note): it cuts again
  only the chunks near the extents and reuses the other chunks and whole
  old nodes by ID. The new
  root is installed in the manifest, and a clean overlay starts on it. The
  test hook `test_last_rebase_bytes()` gives the bytes that the last splice
  cut again, not the file size.
- Commit (D-05): the cut takes descriptors only under the gate. After the
  commit, for each overlay it logs a `Setattr` to `visible` if the base was
  cut off (shrunk), a `Setattr` to the size if the size changed, then the
  extents in `Write` records of at most 1 MiB each. Each piece is copied,
  logged and dropped before the next. So a commit logs again only the bytes
  changed since the last rebase: at most the dirty limit plus one write
  (BUFR-01). This also fixes #93: the old Buffered mode logged the full
  file again at each commit.
- No rebase at fsync (D-09). An fsync syncs the log; the extents are
  durable through their `Write` records. A commit does not rebase either.
- Replay needs no new record. A `Write` on a file with content switches the
  replay handle to an overlay on the root in the manifest. The `Setattr`
  records put back the zeros past `visible`.
- GC: a rebase runs inside a FUSE callback (a write at the limit, release,
  unmount; a size change with no write handle splices in its setattr). That
  callback holds the commit gate shared, as every callback does, and
  `gc_start` takes the gate exclusively. So online GC
  cannot start while a rebase is in flight, and its mark sees the new root.
- BUFR-03 reading: the commit gate is never held exclusively (`commit_cut`,
  `gc_start`) for a hash of file content. A rebase holds the gate shared. On
  random data the splice cuts again only near the change, so the hold is
  short. On long repeating non-zero data, canonical chunking (no forced cut,
  D-08) can make the splice cut again up to the end of the file. Then a
  timed commit or a GC start waits until that callback ends. Memory stays
  fixed in that case.

The Docker numbers (1 GiB and 4 GiB random writes under one memory cap) are
in the chunk-store note, under "Splice (#98)".

Follow-up issues (not in #98):
- One memory limit for all open files. The dirty limit is per open file.
- The RAM of the flush-on-fsync and periodic log modes, which keep records
  in RAM until a sync, and the RAM of the pending records that replay
  loads.

## Cost

Each write is written twice: to the log, and at commit into the store. This
is the price of a durable fsync without a commit per fsync. With overlays,
bytes that are overwritten before a rebase are stored once, at the rebase.

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

Overlay tests (#98):
- `overlay::tests`: `overlay_reads_match_a_vec_model`,
  `a_growing_extent_does_not_double_its_capacity`.
- Lib tests in `filesystem.rs`: `an_overlay_rebases_when_it_crosses_the_dirty_limit`,
  `a_commit_relogs_an_overlay_in_pieces_of_at_most_1_mib`,
  `a_switch_that_moves_pending_writes_over_the_limit_rebases_at_once`.
- `memory_bound_tests.rs`: `an_open_file_uses_memory_bounded_by_the_dirty_limit_not_the_file_size`
  (a counting allocator, 16 MiB and 48 MiB files).
- `write_path_tests.rs`: `a_rebase_rechunks_only_near_the_change`,
  `a_small_change_adds_only_the_changed_chunks`,
  `an_extend_without_a_handle_rechunks_little`.
- `gc_online_tests.rs`: `online_gc_during_a_rebase_keeps_every_chunk`,
  `gc_start_waits_for_a_rebase_in_a_callback`.
- `write_log_replay_tests.rs`: `a_commit_relogs_only_the_bytes_changed_since_the_last_rebase`,
  `an_overlay_replays_after_a_crash_with_each_log_mode`,
  `an_overlay_with_more_than_64_mib_of_relog_replays`.
- `fsync_tests.rs`: `commit_and_fsync_do_not_rebase_an_open_overlay`.
- Docker, by hand: `scripts/docker-linux-test/run.sh bigwrite 1024 4096`
  (memory cap); `run.sh verify` checks that a large copy does not switch to
  an overlay.

## Open points from review (to resolve before code)

1. The cut: one cut function on `SliceFsFilesystem` for every commit (timer,
   GC start, unmount, snapshot). Handles with pending (RAM-only) bytes or
   with an overlay are not flushed under the gate: the cut records, per such
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

## Implementation plan (part 1)

Done on `feat/write-log`:
- Segment frame v2: seq + CRC32C per record; the reader stops at a torn tail.
- `WalEntry::Op` / `SegmentEntry::Op` (opaque bytes, type 0x10);
  `load_store_with_log` returns the op records after the last RootUpdate;
  `DictMetadataStore::log_op`; PerOp syncs only non-Op records;
  `compact_segment` keeps Op records.

Next:
1. `slicefs-cli/src/oplog.rs`: `Op` enum (create, mkdir, mknod, symlink,
   link, unlink, rmdir, rename, setattr, setxattr, removexattr, write,
   truncate), binary encoding, a time per record. A thread-local clock freeze
   in `now_pair()` for the duration of each logged request (all times of one
   request are equal and replayable). Log in `handlers.rs` after success; no
   logging while replaying.
2. Replay at mount: one replay handle per inode; a write to a reclaimed inode
   is skipped; at the end release all replay handles and commit.
3. The cut: before every commit (timer, GC start, unmount) flush each open
   handle into its manifest (settle pending, Streaming: ended clone; an
   overlay keeps its base and logs its extents again, see "Overlays"). The
   commit timer then needs `Weak<SliceFsFilesystem>`.
4. Rotation at commit: RootUpdate + all snapshot records into a new segment,
   sync, delete older segments (the log is one segment: root, snapshots,
   ops). Then `compact_segment` never sees dead op records.
5. Tests: differential (live state against replayed state), kill -9 with
   `--commit-interval 0` in the Linux harness, sync order, torn tail.
