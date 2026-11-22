# Phase 19 deferred items

## Open

### From 19-09

- **Size change of a closed file and unlink of it at the same time.** `setattr_size_closed` splices in the transient cell without a stripe, then sets the manifest and stamps the size. If an unlink reclaims the inode meanwhile, the manifest entry and one refcount of the deleted inode remain, and the setattr returns EIO. This race is older than Phase 19. A fix must re-check the inode under its stripe before the install; that cannot happen while the transient cell (rank 4) is held, so the install step must move out of the cell hold. Owner: a follow-up plan.
- **Cross-directory rename ancestor check** (CONTEXT Deferred Ideas): not present; the rename mutex is where it belongs. Owner: a follow-up plan.

### From 19-10

- **A size change of a closed file against a setattr of other fields of the same inode.** The size change holds only the transient cell. If the fields call reads the cell map before the transient cell is in it, the two share no lock. Then the two inode writes (size change: mtime and ctime now; fields: explicit mtime, ctime now) and their two records can be in opposite orders. Replay then gives another mtime. This has the same root as the 19-09 item above (no stripe for a closed-file size change). Owner: a follow-up plan.

### From 19-16

- **Long cell and stripe holds in the harness, cause not measured.** In the serialize-off lock-stats runs, `file_cell` had single holds of up to 7.9 ms (macOS) and 13 ms (Linux), and `stripe` one hold of 21.5 ms (macOS, many-files mode). These are not the global locks of LOCK-01, and the parked tests show that no writer I/O is in a cell hold. Possible causes: the copy of a large step in a debug build, the commit cut (a locked flush under the exclusive gate), or metadata I/O under a stripe that has no trace span (spike 19g saw `unlink_logged`). Owner: Phase 20 (a probe run like spike 19g).
- **WAL group commit** (REQUIREMENTS future item; spike S6 decides). Owner: a later phase.
- **The Phase 20 flusher and throttle:** the background rebase flusher, writers throttled before they take a lock, and the cross-file dirty budget (LOCK-02, LOCK-06). Owner: Phase 20.

## Closed by 19-12..19-15

- **The store lock held across disk I/O (from 19-11).** Fixed in 19-12: the phase-2 fsync of `ChunkStore::sync`, the fsync of the index flush and the directory syncs run with no store lock, and a put writes its record with no lock (one write in flight at the end of the active pack). 19-16 measured store hold max 61-262 us (macOS) and 88-1967 us (Linux).
- **The WAL writer lock held for the record pwrite (from 19-11).** Fixed in 19-13: every strategy writes its records with a positional write and no WAL lock, one write in flight per segment; a cut creates its next segment before it takes the writer lock. 19-16 measured `wal_writer` hold max 38-1776 us (macOS) and 101-1019 us (Linux).
- **Locked fallbacks with writer I/O in the cell hold (from 19-11).** Fixed in 19-14 (step token, unlocked retries of dropped steps, the hard-cap gate, reads that compose pending writes from memory) and 19-15 (the finalize and the size change of an open file run only unlocked steps; a hard-cap gate step copies at most the final step). The locked step forms are used only by destroy and the commit cut (exclusive gate).
- **mkdir: the new directory could be reached by its name before its `Mkdir` record (from 19-10).** Fixed in 19-13: mkdir reserves the number first, locks the stripes of the parent and of the new directory, then links it and appends the record in that hold.
