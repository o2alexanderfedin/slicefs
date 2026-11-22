## Problem

The current code serializes almost all work:
- One global `Mutex` around `StoreIo` (the store I/O). The code holds it during hash calculation and disk I/O.
- One global `Mutex` around `open_files`.
- Thus a read of one file waits for a write of another file, and a read of a file that is open for write clones the writer state and rebuilds the full file under the global lock (#1).

## Design

Rules:
- No global lock on the hot path.
- Hold a lock only for an in-memory change (nanoseconds to microseconds). Never hold a lock during disk I/O, hash calculation, or a wait.
- The FUSE callbacks of `fuser` are synchronous. Thus use synchronous locks (`parking_lot`), not async locks. Use threads for CPU work (`rayon`) and for the log flusher. Async (for example `tokio`) is only useful later, for network parts.

| Structure | Access | Mechanism |
|---|---|---|
| Committed root and tree | read always, replaced one time per commit | `ArcSwap` (reads without a lock, atomic publish) |
| Inode table (ino → per-inode state) | many lookups, few inserts | sharded map (`DashMap`, or N shards of `RwLock<HashMap>`) |
| Per-inode state (extent overlays, size, attributes) | many readers, short writes | one `parking_lot::Mutex` per open inode (the file cell); readers hold it only to copy |
| Log append | each write, very short | append the record in the lock hold of its apply: a memory copy plus an LSN under a short queue lock; write-out and sync after the apply locks are released |
| `durable_lsn` | read by each `fsync` | `AtomicU64` plus a condition variable |
| Log segments | parallel reads | immutable when sealed; active segment append-only; a record write runs with no log lock, one write in flight at the end of the segment; `pread` needs no lock |
| Chunk store (content-addressed) | written by rebase steps and at commit, read always | objects do not change, writes are idempotent; a short store lock covers memory only; a record write runs with no lock, one write in flight at the end of the active pack; fsync runs with no lock |
| Commit and GC | rare | one `Mutex` for the two, not on the hot path |

## Read while write

Log order rule: each log record is appended under the same locks as its apply. The append is a memory copy plus an LSN. The write-out and the sync of the log run after the apply locks are released, and before the reply. This rule replaces the earlier order, in which the record went to the log before the inode lock (owner decision for v2.2).

Writer:
1. Get the inode (file cell) lock. Apply the write to the overlay, and append the log record in the same lock hold (a memory copy plus an LSN). Release the lock (microseconds).
2. After the apply locks are released, write out and sync the log with no lock held.
3. Reply.

Steps of a writer (rebase and stream steps: chunk, hash, store put):
- A step copies its range under the inode lock, does the I/O with no lock, and takes the lock again only to publish the result.
- Each file has one step token, so one step is in flight per file. Another writer that needs a step waits for the token with the inode lock released. A reader never waits for the token.
- If the file changed during the I/O, the step drops its result and tries again with no lock. After a bounded number of dropped stream steps in a row, the write fails with EIO. A dropped rebase step leaves the work to the next write or to the release.
- Hard cap: the dirty bytes of one file have a hard cap. A write that would go above the cap waits for the step in flight, or runs its own step with the lock released, before it applies. Each step frees a bounded amount (at most one step size), and the writer steps again until it is under the cap. Thus no lock hold copies more than one step size.
- The last release stores the file in the same unlocked steps. A size change of an open file waits for the step token, and runs its I/O as unlocked steps too.

Reader:
1. Get the inode lock only to copy the overlay view for the range (or the stream state and the parts of pending writes in the range), then release the lock (microseconds).
2. `pread` the bytes from log segments and committed chunks (the slow part, no lock), and compose pending writes on top in memory.

A reader waits for a writer for microseconds at most, never for disk I/O or a hash.

Semantics:
- Read after write: a `write()` publishes its extent before it replies. A `read()` that starts after that sees the data, from each handle and each process.
- Each write is atomic: it applies to the overlay in one lock hold. A concurrent read sees all of the write or none of it.
- An `O_APPEND` write takes its offset from the current size under the inode lock, and the log record has that offset. Concurrent appends through different handles never overlap.
- Reads continue during a commit, because the frozen layer does not change.

Replay:
- Replay applies the records in log order. Records of one inode are in the order of their applies, because each one was appended under the lock of its apply.
- A create replays with the logged inode number, so creates logged in any inode order replay to the same inode numbers.

Option B (if a profile shows readers that wait): per inode, `ArcSwap<ExtentMap>` with a persistent (structurally shared) map, for example from the `im` or `rpds` crates. Readers load a snapshot with no lock. Writers of the same inode use a small `Mutex`, make a new map in O(log n), and swap it. Keep the extent map behind a small interface, so that the change from option A to option B is local.

## Lock order (no deadlocks)

1. Commit gate (shared, one hold per request)
2. Rename mutex (cross-directory rename only)
3. Namespace stripes, in ascending stripe order (the inode number is hashed to a stripe). For operations on two inodes (rename, link), this gives the same freedom from deadlock as increasing inode order, and it also covers two inodes in one stripe. Create, mknod, symlink and mkdir take the new inode number first, and lock the stripes of the parent and of the new inode before the name is linked.
4. File cell (the per-inode lock of an open file)
5. Short maps (handle table, cell map shards, `readers` map)
6. Store internals (metadata caches, log queue, chunk store), innermost

A debug build checks this order on each lock and fails the test on a violation.

`fsync` releases all locks before it waits for `durable_lsn`. The committer uses the frozen overlays and holds no inode locks.

Some work off the read and write paths still does I/O under a lock: GC, the commit cut (under the exclusive gate, when no request runs) and offline log rotate. A size change of a closed file does its I/O under its own transient cell; reads do not wait for it.

## Acceptance criteria

- No global lock in the read or write path.
- A test with parallel readers and writers on one file shows correct data, and shows that readers do not wait for the I/O of writers.
- A test with parallel I/O on different files shows no contention between them.
- Each log record is appended under the same locks as its apply. After a crash, replay of the log gives the same state as the apply, including creates by logged inode number.

Depends on the write-log design (#20). Gives the base for multi-threaded FUSE.
