# Milestones

## v2.0 Streaming Writes & Hardening (Shipped: 2026-11-17)

**Phases completed:** 6 phases (07.1, 8-12), 13 plans, 21 tasks
**Requirements:** 21/21 accounted (9 complete, 12 superseded by later design changes)
**Audit:** `.planning/milestones/v2.0-MILESTONE-AUDIT.md` (status tech_debt, no open gaps)
**Tag:** `milestone-v2.0` (not `v2.0`: release tags `v2.0.0`, `v2.0.1` and `v2.1.0` already exist)

**What shipped (state at close):**
- Streaming writes: sequential writes go straight into content-defined chunks (`content::Stream`); reads during a write are served from the stream. Out-of-order writes fall back to a Buffered mode per handle.
- Correctness: real statfs counts, O(1) snapshot lookup (FIX-02..04). Refcounts were replaced later by reachability GC with online GC at mount.
- Compression: the whole-file compression layer was removed (phase 9); zstd per chunk-store object came back later (PR #53, `--compression`).
- macOS: FUSE-T 1.2+ with FSKit as the only backend (SMB and NFS removed in PR #42). SIGTERM, SIGINT and SIGHUP do a soft unmount, so `destroy()` makes the final commit (PR #59).
- Write log (#20): each request is logged and replayed after a crash, with a clean cut at each commit; fixes in PRs #79, #81, #83 and #94 (shared write handles, replayed mtime, sync of re-logged bytes, utimens at close).

**Known tech debt:** issue #93 (full re-log of large Buffered files at each commit); write_alias not safe with several FUSE threads (not reachable: 1 thread); see the audit file.

---
