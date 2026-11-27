# Phase 26: Prolly maps in commit, read and GC - Discussion Log

> **Audit trail only.** Do not use as input to planning, research, or execution agents.
> Decisions are captured in CONTEXT.md — this log preserves the alternatives considered.

**Date:** 2026-11-27
**Phase:** 26-prolly-maps-in-commit-read-and-gc
**Mode:** autonomous smart discuss
**Areas discussed:** On-disk format

---

## On-disk format

| Option | Description | Selected |
|--------|-------------|----------|
| By name in tree | `dir_data` key = dir ino || name → child ino + type; O(height) per change; readdir is a range scan | ✓ |
| Keep one list per dir | `dir_data` key = dir ino → ObjectKey of the list; a change in a big directory costs O(directory size) | |
| You decide | Claude picks and reports | |

| Option | Description | Selected |
|--------|-------------|----------|
| Inline small values | Inode record and small xattr sets inline in the leaf; big xattr sets spill to an object; manifests stay ObjectKeys | ✓ |
| Keep ObjectKey values | Values stay 33-byte keys to separate objects | |
| You decide | Claude picks and reports | |

## Decided autonomously

[auto] [Read path] — Q: "Where do map entries live at run time?" → Selected: "read on demand from prolly nodes through a bounded node cache; dirty entries in memory until commit" (reversible technical — decided autonomously: internal code, no format effect)
[auto] [Read path] — Q: "Node cache size?" → Selected: "64 MiB default byte cap, mount option" (reversible technical — decided autonomously: a number and an option)
[auto] [Commit] — Q: "How does a commit write?" → Selected: "per tree sorted apply batch, then root record" (reversible technical — decided autonomously: internal)
[auto] [Commit] — Q: "Interim Phase 25 stores?" → Selected: "refuse cleanly at mount; fix IN-01 and IN-05" (reversible technical — decided autonomously: no deployments)
[auto] [GC] — Q: "How does mark walk the trees?" → Selected: "one node path per tree per root, memory-cap test" (reversible technical — decided autonomously: internal)
[auto] [Crash tests] — Q: "How to cut a commit?" → Selected: "test-only fault points after each write and sync, replay, check" (reversible technical — decided autonomously: test code)
[auto] [Bench] — Q: "1M+ store and flat baseline?" → Selected: "generator through the product API; v2.3.0 binary for flat" (reversible technical — decided autonomously: tooling)
