# Phase 19 deferred items

## Open

### From 19-09

- **Size change of a closed file and unlink of it at the same time.** `setattr_size_closed` splices in the transient cell without a stripe, then sets the manifest and stamps the size. If an unlink reclaims the inode meanwhile, the manifest entry and one refcount of the deleted inode remain, and the setattr returns EIO. This race is older than Phase 19. A fix must re-check the inode under its stripe before the install; that cannot happen while the transient cell (rank 4) is held, so the install step must move out of the cell hold. Owner: a follow-up plan.
- **Cross-directory rename ancestor check** (CONTEXT Deferred Ideas): not present; the rename mutex is where it belongs. Owner: a follow-up plan.

### From 19-10

- **A size change of a closed file against a setattr of other fields of the same inode.** The size change holds only the transient cell. If the fields call reads the cell map before the transient cell is in it, the two share no lock. Then the two inode writes (size change: mtime and ctime now; fields: explicit mtime, ctime now) and their two records can be in opposite orders. Replay then gives another mtime. This has the same root as the 19-09 item above (no stripe for a closed-file size change). Owner: a follow-up plan.

### From 19-16

- **unlink reclaims the inode with the namespace stripes held.** On macOS, `unlink_logged` held its two stripes for about 20 ms in each of the 3 many-files runs (21.5, 20.2 and 20.2 ms). A probe (`runs/g6-macos-probe.log`) shows that the first store read of the manifest in `reclaim_inode` takes the 20 ms. No global lock is held during it. Why the read takes 20 ms is not measured. On Linux the stripe max was 1.1-2.6 ms. A fix (reclaim after the stripes are released, or read the manifest before the stripes are taken) is an owner decision. Owner: Phase 20 or a follow-up plan.
- **file_cell holds of 3.6-13 ms, span not measured.** On macOS the one-file mode had `file_cell` holds of 3.6-7.9 ms in every run, on Linux one of 13 ms. The probe caught only 3.2-4.8 ms holds, released in `write_in_cell`. The parked tests show no writer I/O in a cell hold. Owner: Phase 20 (a span probe).
- **WAL group commit** (REQUIREMENTS future item; spike S6 decides). Owner: a later phase.
- **The Phase 20 flusher and throttle:** the background rebase flusher, writers throttled before they take a lock, and the cross-file dirty budget (LOCK-02, LOCK-06). Owner: Phase 20.

## Closed by 19-12..19-15

- **The store lock held across disk I/O (from 19-11).** Fixed in 19-12: the phase-2 fsync of `ChunkStore::sync`, the fsync of the index flush and the directory syncs run with no store lock, and a put writes its record with no lock (one write in flight at the end of the active pack). 19-16 measured store hold max 61-262 us (macOS) and 88-1967 us (Linux).
- **The WAL writer lock held for the record pwrite (from 19-11).** Fixed in 19-13: every strategy writes its records with a positional write and no WAL lock, one write in flight per segment; a cut creates its next segment before it takes the writer lock. 19-16 measured `wal_writer` hold max 38-1776 us (macOS) and 101-1019 us (Linux).
- **Locked fallbacks with writer I/O in the cell hold (from 19-11).** Fixed in 19-14 (step token, unlocked retries of dropped steps, the hard-cap gate, reads that compose pending writes from memory) and 19-15 (the finalize and the size change of an open file run only unlocked steps; a hard-cap gate step copies at most the final step). The locked step forms are used only by destroy and the commit cut (exclusive gate).
- **mkdir: the new directory could be reached by its name before its `Mkdir` record (from 19-10).** Fixed in 19-13: mkdir reserves the number first, locks the stripes of the parent and of the new directory, then links it and appends the record in that hold.
