# Phase 20: Dirty budget and rebase flusher - Discussion Log

> **Audit trail only.** Do not use as input to planning, research, or execution agents.
> Decisions are captured in CONTEXT.md — this log preserves the alternatives considered.

**Date:** 2026-11-23
**Phase:** 20-dirty-budget-and-rebase-flusher
**Areas discussed:** Wait limit, Limits

## Wait limit

| Option | Description | Selected |
|--------|-------------|----------|
| Writer flushes itself | After the wait, the writer steps its own file, then writes | |
| Return an error | After the wait, the write fails (EIO or ENOSPC) | |

**User's choice (free text):** "It should write, if idle for some time, or max time, or buffer is full"
**Interpretation:** dirty data is written on idle time, maximum age, or a full buffer; a throttled writer writes after the wait (no error).

## Limits

| Option | Description | Selected |
|--------|-------------|----------|
| Keep fixed | Constants only | |
| Add mount options | `--dirty-budget`, `--dirty-limit` | ✓ |

**User's choice:** "Add mount options with default values"

## Decided autonomously

[auto] Flusher threads, file pick, throttle place, commit deadlock rule, take-out check, RSS cap, wait limits (reversible technical — see CONTEXT.md).
