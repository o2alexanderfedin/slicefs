# Phase 19 deferred items

## From 19-09

- **Size change of a closed file and unlink of it at the same time.** `setattr_size_closed` splices in the transient cell without a stripe, then sets the manifest and stamps the size. If an unlink reclaims the inode meanwhile, the manifest entry and one refcount of the deleted inode remain, and the setattr returns EIO. This race is older than Phase 19. A fix must re-check the inode under its stripe before the install; that cannot happen while the transient cell (rank 4) is held, so the install step must move out of the cell hold.
- **Cross-directory rename ancestor check** (CONTEXT Deferred Ideas): not present; the rename mutex is where it belongs.

## From 19-10

- **mkdir: the new directory can be reached by its name before its `Mkdir` record is appended.** mkdir holds only the parent's stripe, and the store call takes the number and adds the entry in one step. In that window another thread can find the new name and create in it, or setattr it, under the new directory's stripe. That record can come before the `Mkdir` record, and replay then stops (a create in a missing directory) or skips it (a setattr). This window existed before 19-10, and was larger, because the record was appended after the apply. 19-10 closed it for create, mknod and symlink: `new_entry` takes the number first and locks `[parent, ino]`. mkdir needs a store call that gives the number before it adds the entry, or a `create_directory_at` with a number taken first.
- **A size change of a closed file against a setattr of other fields of the same inode.** The size change holds only the transient cell. If the fields call reads the cell map before the transient cell is in it, the two share no lock. Then the two inode writes (size change: mtime and ctime now; fields: explicit mtime, ctime now) and their two records can be in opposite orders. Replay then gives another mtime. This has the same root as the 19-09 item above (no stripe for a closed-file size change).

## From 19-11

- **The store lock is still held across disk I/O in two places (LOCK-01 not met).** The serialize-off harness with `lock-stats` shows store-lock holds of 21-91 ms (p99 22-237 us), and requests that waited up to 86 ms for the lock. A backtrace probe (one run, not committed) named two paths:
  - `ChunkStore::sync`, second phase: `sync_active` (the fsync of the bytes put since the first, unlocked phase) and `sync_dir` run with the store lock held. The commit calls it before the gate (`timed_commit`) and as `pre_root_sync`. CONTEXT says an fsync must leave the lock; 19-02 kept this "rest" under the lock (the #108 pattern).
  - `ChunkStore::put`, the record `append` (a page-cache pwrite, kept under the lock by D-14): on macOS it stalled 36 ms, most likely behind a concurrent fsync of the same pack file.
  - The WAL writer lock has the same effect: `PerOpWal::write_out` holds it for the record pwrite. With 4 threads its hold p99 is about 20 ms (one thread: max 0.3 ms), most likely behind another thread's fsync of the same segment.
  - A `file_cell` or `stripe` hold of about 20 ms in the harness matches the store hold max of the same run: the holder most likely took the store lock (rank 6) inside the cell or stripe hold and the pwrite stalled.
  A fix changes the sync protocol of the chunk store (for example: seal or switch the active pack under the lock and sync with no lock, or move the append out of the lock). That is an owner decision; Phase 20 or a follow-up plan.
