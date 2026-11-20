# Phase 14: Bounded-memory random writes - Context

**Gathered:** 2026-11-18
**Status:** Ready for planning

<domain>
## Phase Boundary

RE-SCOPED 2026-11-18 by the user ("it never should keep files in memory - 1 GiB is huuuge!!"). The phase implements #98: remove the full-file buffer of Buffered mode. A random write never loads the whole file into RAM. The memory of one open file is bounded by a fixed limit that does not depend on the file size. The same design makes a commit log only the changed bytes, so it also fixes #93. Requirements: BUFR-01..05.

</domain>

<decisions>
## Implementation Decisions

### Direction set by the user (2026-11-18)
- No full-file buffer anywhere. `WriteMode::Buffered { buf: Vec<u8> }` and every `handle_content` call that reads a whole file into RAM go away. — **Reversibility:** costly — core write path of the filesystem
- Memory of an open file is bounded by a fixed limit (dirty bytes plus about one chunk of work space), independent of the file size. Tested in Docker with a 1 GiB and a 4 GiB random-write workload under the same VmHWM cap. — **Reversibility:** costly

### Proposed design from #98 (the planner confirms or improves it after research)
- Patch overlay: base = manifest (chunk list) of the last committed version, kept in the chunk store; dirty extents = sparse map offset -> bytes; reads merge both; truncate/extend change only size and extents.
- Rebase at a dirty-bytes limit (for example 64 MiB), at fsync and at close: re-chunk only near the dirty extents (start at the chunk boundary before an extent, feed base + dirty bytes through FastCDC until a new boundary meets an old boundary after the extent), then replace that range of chunk references in the manifest. Unchanged chunks keep their IDs.
- Write log: dirty extents are already `Write` records; a commit re-logs only the dirty extents since the last rebase (fixes #93).
- GC: a rebase outside the commit gate is protected from online GC (GC root or rebase/GC lock), with a test that runs GC during a rebase.
- Open questions: can the manifest replace a range of chunk references without a full rewrite of the chunk list; is a global limit over all open handles also needed.

### Decisions on the research open questions (2026-11-18, orchestrator; see 14-RESEARCH.md)
- Chunking stays deterministic: the same bytes give the same chunks and the same file key, whatever the write history. No forced cut to bound rebase CPU. A rebase over long repeating non-zero data may be slow, but memory stays fixed. Zero runs may use the exact shortcut that keeps chunk IDs. — **Reversibility:** costly — dedup and content-addressed keys depend on it (global/decentralized goal)
- No rebase at fsync. Fsync stays durable through the write log. A rebase runs when the dirty limit is crossed (inside that FUSE callback) and at close.
- Dirty limit is per open file in this phase; start at 32 MiB, set the final value from measured runs. A global limit over all open files is a follow-up issue.
- RAM that grows with written bytes in the flush-on-fsync and periodic log modes, and pending records loaded at replay, are follow-up issues. The Docker memory test uses the default per-op log.
- Phase 13 is executed (PR #100). Phase 14 execution starts after #100 is merged into develop. Keep the "switches to Buffered" log text or update the Phase 13 Docker check in the same change.
- The chunk-list splice API in chunk-store/src/file.rs is IN scope now (the old deferral is void).

### Write-log RAM (decided 2026-11-18 after the spike on local branch spike/wal-memory; user: "do spikes to test both scenarios, then choose")
- Option A: per-op calls flush() (one write() into the kernel, no fsync) after each operation record; flush-on-fsync and periodic write their buffer to the segment (no fsync) when it passes 4 MiB. fsync counts do not change. Spike: kill -9 lost a written+renamed small file in 3/3 runs with the old code and with option B, and kept it in 3/3 with A; flush-on-fsync peak RSS for 1 GiB with no fsync went from 1276 MiB to 284 MiB; no measurable cost for writes of 4 KiB or more, +8 % CPU for small files, about 6 us per request for 64-byte writes (in-process, no FUSE). — **Reversibility:** reversible
- Periodic gets its timer: `interval_secs` is used to flush and sync on a schedule (today it is never used, so periodic acts as flush-on-fsync).
- Document the new crash behaviour of the capped modes: after a crash with no fsync, a file can come back as a correct prefix (for example 4 MiB of 6 MiB).
- Phase 13 Docker check 05c: its control rename (no sync) now survives kill -9 in per-op mode; the control must use flush-on-fsync (where a small record below the cap is still lost).

### SUPERSEDED (pre-2026-11-18 scope: delta re-log while the full buffer stays in RAM)
The "Decided autonomously" items below were written for the old scope. Keep only what still applies (replay rules, shrink-then-extend zeros, write aliases on the owner state, test_write taking the state out of open_files, GC race). Ignore anything that keeps a full-file buffer.


### Decided autonomously
- [write-log record format] Q: "Does the delta re-log need a new Op or a record-format change?" → Selected: "No. Use the existing Op::Write{ino,offset,data} (tag 12) for each dirty range and Op::Setattr{size:Some(..)} (tag 9) for shrink/extend, on top of the base manifest in the root. Replay already works: a Write on a fresh O_RDWR handle of a file with content is not 'fresh' (filesystem.rs:734), so it switches to Buffered via handle_content(), which loads the committed manifest (591-602), and then pwrites the range. Emit Op::Truncate + writes only when the handle has no base (the file has no committed content that matches)." — Uses records that already exist; oplog.rs encode/decode stays as it is.
- [record size limit (pre-existing bug)] Q: "How are large dirty ranges logged?" → Selected: "Split each logged range into pieces of at most 16 MiB. Today commit_cut (filesystem.rs:1078) logs the whole buffer as one Op::Write. The segment reader drops any payload over MAX_PAYLOAD = 64 MiB (metadata/src/segment/mod.rs:186, reader.rs:60) and stops at it. So a Buffered file over about 64 MiB loses that record and every record after it on replay. Also Enc::bytes stores the length as u32 (oplog.rs:99). Add a regression test with a Buffered file over 64 MiB." — A local loop in the relog code; no format change.
- [dirty-range tracking] Q: "How are changed bytes tracked per Buffered handle?" → Selected: "Add to WriteMode::Buffered a `base: Option<ObjectKey>` (the content the root manifest holds, refcounted), a `base_len: u64`, a `dirty: BTreeMap<u64,u64>` (start->end, merged intervals) of bytes written since the base, and a `shrunk_to: Option<u64>` (the smallest size set by a truncate since the base). The Buffered pwrite in test_write (804-810) and pwrite_into callers add intervals. The test_setattr_size Buffered branch (1241-1261) clips intervals and sets shrunk_to; a resize to 0 clears the base. No new crate; intervals at byte granularity, so a 4-byte write logs 4 bytes." — In-memory state only; can be swapped for a bitmap or pages later.
- [base at the Streaming->Buffered switch] Q: "What base does a handle get when it switches to Buffered?" → Selected: "Keep last_committed_root as the base instead of taking and decrementing it (filesystem.rs:794-800). If the stream has new bytes since then, end_padded(state, next_expected_offset, min_size) gives a root at the cost of a finish (the chunks are already hashed). Set the manifest to that root at the next cut. Initial dirty = the pending writes plus the triggering write. If streamed==0, the base is the current manifest m[0]." — Uses the existing end_padded and handle_content paths.
- [commit_cut relog (BUFR-01)] Q: "What does commit_cut log for a Buffered handle?" → Selected: "Under the gate: (1) If a finished rebase is waiting, install it: set_manifest(ino,[snap_root]), increment the new refcount, decrement the old base, base=snap, dirty=dirty_since_snap. (2) Log Setattr size=shrunk_to if shrunk_to < base_len, then Setattr size=buf.len() if it is not base_len, then one Write per dirty interval (split at 16 MiB). Each record has time=s.modified. (3) Keep the ctime Setattr logic at 1081-1090. (4) flush_wal stays as it is (1101-1104). Note: what is logged is the dirty bytes since the base, which is at most the rebase threshold plus the bytes changed since the last commit." — Local to commit_cut (filesystem.rs:1046-1106).
- [rebase outside the gate (BUFR-03)] Q: "Where and when is the full buffer hashed?" → Selected: "Add SliceFsFilesystem::prepare_cut(). The commit timer calls it before gate.write() (commit_timer.rs:47-50). For each Buffered handle with dirty bytes since the base >= a threshold (internal const, e.g. 64 MiB or 1/8 of the file, whichever is smaller; there is no CLI flag), take a snapshot, start dirty_since_snap, release open_files, hash the snapshot with Stream::push per piece and finish, then store the result as the waiting base. commit_cut never hashes Buffered content. When a handle is absent from open_files during a write (test_write takes the state out of the map at 723), skip it for this round." — An internal policy and a constant; tunable without any contract change.
- [snapshot mechanism] Q: "How to snapshot a 1 GB buffer without blocking writes or doubling RAM for long?" → Selected: "Recommended: change Buffered buf to copy-on-write pages, e.g. Vec<Arc<[u8]>> of 1 MiB pages. A snapshot is an O(pages) Arc clone under the open_files lock. A write makes a private copy (Arc::make_mut) only of pages that a snapshot still shares. Fallback if that is too large for this phase: Vec::clone under the lock. That is simpler, but it is an O(N) memcpy under the global open_files mutex and holds 2x RAM while the hash runs. Readers (test_read 962-966), put_all callers (892, 1162) and resize (1255) must use the paged form." — Internal representation; no persisted or public effect.
- [online GC interplay] Q: "Can a rebase that runs during an online GC lose chunks?" → Selected: "Yes, unless the two are serialized. gc_begin only protects objects that are put or read after it starts (chunk-store lib.rs:608-616, gc_finish 628+). A rebase Stream that is in flight across gc_begin holds subtrees that no root refers to, and gc_start only adds Streaming states as file_roots (filesystem.rs:502-507). Decision: a rebase mutex that prepare_cut holds while it hashes and that gc_start takes before gc_begin. gc_start's commit_cut then installs any finished waiting base, so its root covers it. A crash between the hash and the cut leaves orphaned chunks, which online GC removes." — A mutex in-process; no format change.
- [refcounts] Q: "How are base refcounts handled?" → Selected: "Install of a base: increment the new root, decrement the old one if it differs. Use last_committed_root as the base field so that finalize_write (863-918) and flush_handle keep their dedup and decrement logic. At release, finalize_write may skip put_all when dirty is empty and base is Some (an optional optimization)." — Follows existing patterns at 901-912 and 1165-1176.
- [tests] Q: "How to prove BUFR-01..03 without mounts?" → Selected: "In write_log_replay_tests.rs, extend the existing tests (a_commit_keeps_the_times_of_a_buffered_file at 290, fsynced_buffered_bytes_survive_a_commit_and_a_crash at 331). Add: (a) a large Buffered file (for example 64-128 MiB), a commit, a small write, a commit, and an assertion that the second cut logged about the change size, using a test hook that returns the bytes re-logged by the last cut; (b) a crash and replay for PerOp and FlushOnFsync with a shrink, an extend and sparse writes; (c) a file over 64 MiB that replays (the MAX_PAYLOAD regression); (d) a rebase-then-cut with writes between the snapshot and the cut; (e) an assertion that commit_cut does not call put_all on Buffered content, either by timing in the style of fsync_tests.rs:154 or by a counter." — Test-only hooks follow the existing test_* pattern.

### Claude's Discretion
- Test names, helper layout and plan split are at Claude's discretion.

</decisions>

<code_context>
## Existing Code Insights

### Reusable Assets
- crates/slicefs-cli/src/oplog.rs: Op::Write (tag 12), Op::Setattr (tag 9), Op::Truncate (tag 13), and replay(), which already reopens O_RDWR and switches to Buffered on a non-fresh write (oplog.rs:295-306)
- crates/slicefs-cli/src/filesystem.rs: end_padded (471), handle_content (591), pwrite_into (264), stamp_content (1025), flush_handle (1111), log_record (183)
- crates/slicefs-cli/src/content.rs: Stream::push/finish (214-237), put_all (239), Reader (254)
- crates/slicefs-cli/src/commit_timer.rs:47-56: the hook point before gate.write()
- crates/slicefs-cli/tests/write_log_replay_tests.rs helpers: new_fs_with(WalConfig), reload_and_replay, dump, data(), check()
- crates/slicefs-cli/tests/fsync_tests.rs:140-180 buffered_file() helper and the timing-style assertion

### Established Patterns
- Records carry s.modified as the time; replay freezes it with oplog::Clock (no commit time leaks into mtime)
- Take the state out of open_files, work, and re-insert it (test_write 723/814); release open_files before io and CAS work
- last_committed_root plus a refcount increment or decrement on each manifest change
- commit_cut runs under the exclusive commit gate; re-logged records are synced with flush_wal before return (#20 durability)
- Test hooks are named test_* on SliceFsFilesystem; no macOS mounts in tests

### Integration Points
- SliceFsFilesystem::commit_cut (filesystem.rs:1046-1106): the relog loop, WriteMode::Buffered branch at 1076-1079
- WriteMode::Buffered enum (filesystem.rs:57-59) and OpenFileState (78-98)
- Streaming->Buffered switch in test_write (filesystem.rs:781-802) and the Buffered pwrite (804-810)
- test_setattr_size Buffered branch (filesystem.rs:1241-1261)
- finalize_write (863-918) and flush_handle (1111-1190): put_all on Buffered content
- gc_start (filesystem.rs:491-537): must be serialized with the background rebase
- commit_timer.rs spawn loop (47-56): call prepare_cut() before taking the gate
- test_read Buffered fast path (filesystem.rs:962-966) if the buffer becomes paged
- metadata segment MAX_PAYLOAD (crates/metadata/src/segment/mod.rs:186)
- docs/design/write-log.md: describes the Buffered relog; update it

### Files likely modified
- crates/slicefs-cli/src/filesystem.rs
- crates/slicefs-cli/src/commit_timer.rs
- crates/slicefs-cli/tests/write_log_replay_tests.rs
- crates/slicefs-cli/tests/fsync_tests.rs
- docs/design/write-log.md

### Risks
- Online GC race: a rebase Stream that is in flight across gc_begin can have its chunks swept (only puts after gc_begin are protected; gc_start adds only Streaming states as file_roots, filesystem.rs:502-507). A rebase/GC mutex is required.
- Pre-existing: the switch to Buffered decrements last_committed_root while the manifest still refers to it (filesystem.rs:794-800). The new base logic must hold that refcount and not drop it.
- The rebase still costs O(N) CPU and N bytes of RAM traffic per rebase (with COW pages, only touched pages are copied). The threshold sets the trade-off between write-log bytes per commit and how often the rebase runs.
- test_write takes the OpenFileState out of open_files during a write (filesystem.rs:723), so prepare_cut outside the gate may not find a handle; it must skip it, and the cut must still log correctly.
- Shrink then extend: zeros in the old base area must come back on replay. Log Setattr size=shrunk_to before the final-size Setattr and the dirty writes.
- Write aliases (write_alias, owner_fh) share one state across handles; the dirty tracking must sit on the owner state.
- The Phase 13 close-out (#20) is recommended first; it touches the same tests and docs/design/write-log.md.
- GC safety (from the 2026-11-18 bug check): today every FUSE callback holds `commit_gate.read()` and `gc_start` takes it exclusively, so current code is safe. BUFR-03 moves the Buffered hash out of the gate, so the plan must protect the rebase stream from online GC (GC root or rebase/GC mutex) and add a test that runs GC during a rebase. The Buffered-switch refcount decrement (filesystem.rs ~794-800) is bookkeeping only (GC is mark-and-sweep), but the new base logic must keep the refcount right.

</code_context>

<specifics>
## Specific Ideas

The code today (filesystem.rs:1076-1079): `WriteMode::Buffered { buf } => { relog.push(Record { time, op: Op::Truncate { ino } }); relog.push(Record { time, op: Op::Write { ino, offset: 0, data: buf.clone() } }); }`. This logs the full buffer at each cut. The cut does not hash Buffered content: flush_handle(.., false) returns early for Buffered at 1136-1143. So the root manifest of a Buffered inode is stale; it is the content from before the switch, or empty. The switch to Buffered also decrements last_committed_root at 794-800, but the manifest still points to that root. Because of this, a delta needs a base that the root really holds.

Plan:
1. Keep or install a base root in the manifest. At the switch it is free (stream finish or the existing manifest). After that, a background rebase (a full hash) runs outside the gate in prepare_cut() when dirty bytes since the base pass a threshold, and its result is installed cheaply at the next cut.
2. At the cut, log Setattr size records (shrink to shrunk_to, then the final size) and Write records for the dirty intervals only, split at 16 MiB.
3. Replay needs no change: a Write on a file with content switches to Buffered and loads the base (734, 785).

A pre-existing bug to fix in this phase: segment MAX_PAYLOAD = 64 MiB (metadata/src/segment/mod.rs:186). The reader stops at any larger record (reader.rs:60). So today a Buffered file over about 64 MiB that is open at a commit loses its re-logged bytes and every later record on replay. The record format is not changed (Op::Write, Op::Setattr and Op::Truncate are reused). No new crate is needed.

Project rules: no macOS mounts in automated tests; use targeted cargo commands in worktrees and one full test run per merge; no store backward compatibility; FSKit is the only macOS backend.

</specifics>

<deferred>
## Deferred Ideas

Spill of large Buffered files to an on-disk chunked form; an incremental CDC re-chunk or splice API in chunk-store (FileWriter has no edit or patch operation; file.rs:99-205); a re-log reduction for Streaming pending out-of-order writes (usually small, bounded by REORDER_WINDOW = 64 MiB at filesystem.rs:41). Note: a full pending window can itself exceed MAX_PAYLOAD only per record; each pending entry is at most one FUSE write, so it is fine. A CLI flag for the rebase threshold is also deferred.

</deferred>
