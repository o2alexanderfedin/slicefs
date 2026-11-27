# Phase 19: Fine-grained locks - Discussion Log

> **Audit trail only.** Do not use as input to planning, research, or execution agents.
> Decisions are captured in CONTEXT.md — this log preserves the alternatives considered.

**Date:** 2026-11-21
**Phase:** 19-fine-grained-locks
**Areas discussed:** Lock crate, Read while write

---

## Lock crate

| Option | Description | Selected |
|--------|-------------|----------|
| parking_lot 0.12 | No poisoning, deadlock watchdog; about 59 sites change; already in Cargo.lock via fuser | ✓ |
| Keep std locks | No dependency change; poisoning stays; no watchdog | |

**User's choice:** parking_lot 0.12

## Read while write

| Option | Description | Selected |
|--------|-------------|----------|
| Step outside the lock | Copy range under lock, I/O without lock, publish under lock if generation unchanged | ✓ |
| Keep it inline for now | Simpler; readers can wait during the step; LOCK-05 moves to Phase 20 | |

**User's choice:** Step outside the lock

## Decided autonomously

[auto] [Locks] — Q: "Per-file lock structure?" → Selected: "Arc<FileCell> per open inode in a sharded map" (reversible technical — decided autonomously: internal structure)
[auto] [Locks] — Q: "Namespace consistency?" → Selected: "Striped NamespaceLocks, ascending order, rename mutex first" (reversible technical — decided autonomously: research marks it reversible)
[auto] [Locks] — Q: "Single-inode metadata RMW?" → Selected: "modify_inode under the inode_live shard lock" (reversible technical — decided autonomously: internal API)
[auto] [Locks] — Q: "Lock rank?" → Selected: "gate, rename, stripes, FileCell, short maps, store internals; debug checker" (reversible technical — decided autonomously: debug-only code)
[auto] [Locks] — Q: "readers map?" → Selected: "Arc<Mutex<Reader>> per inode" (reversible technical — decided autonomously: internal)
[auto] [Log] — Q: "Log order?" → Selected: "append under apply locks; replay by logged inode number" (reversible technical — decided autonomously: owner decision, code shape only)
[auto] [Log] — Q: "Issue #21 body?" → Selected: "update in this phase" (reversible technical — decided autonomously: owner asked for it)
[auto] [Tests] — Q: "Per-write test counters?" → Selected: "thread-local or test-only" (reversible technical — decided autonomously: test code)
[auto] [Tests] — Q: "How to show LOCK-05 and POSIX rules?" → Selected: "extend the Phase 18 harness" (reversible technical — decided autonomously: test code)
[auto] [Research] — Q: "Append offset?" → Selected: "research checks kernel behavior; daemon uses size under file lock" (reversible technical — decided autonomously: research item)
