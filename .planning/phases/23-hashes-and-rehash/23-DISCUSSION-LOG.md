# Phase 23: Hashes and rehash - Discussion Log

> **Audit trail only.** Do not use as input to planning, research, or execution agents.
> Decisions are captured in CONTEXT.md — this log preserves the alternatives considered.

**Date:** 2026-11-26
**Phase:** 23-hashes-and-rehash
**Areas discussed:** SHA-256 hasher, header for SHA-256, conversion method, old-to-new map, switch protocol and snapshot supersession, GC and report, dry-run
**Mode:** `--auto` (recommended option chosen for each area; owner decisions given by the orchestrator are locked)

---

## SHA-256 hasher

| Option | Description | Selected |
|--------|-------------|----------|
| `sha256`, tag 0x12 (multihash), RustCrypto `sha2` with run-time CPU detection | Matches HASH-01 wording and the multihash pattern of 0x1e | ✓ |
| `sha2-256` name | Multihash name, but HASH-01 says `sha256` | |
| Own SIMD code | New unsafe code, unsafe-count changes | |

**Choice:** first option (recommended default).

## Header for SHA-256

| Option | Description | Selected |
|--------|-------------|----------|
| incompat flag `hash-sha256`, written at first use, kept | 21.1 rule: an older build cannot read SHA-256 objects | ✓ |
| No flag | Unknown hash name is already refused, but a mixed store with a BLAKE3 hash line would open and fail reads | |

**Choice:** first option (recommended default).

## Conversion method

| Option | Description | Selected |
|--------|-------------|----------|
| Rebuild through the real write path | Same key as a fresh write by construction; `is_cut` depends on IDs | ✓ |
| Node-by-node ID translation | Wrong key: node cuts move with new IDs | |

**Choice:** first option; a chunk-boundary reuse path is allowed only with a key-equality test.

## Old-to-new map

| Option | Description | Selected |
|--------|-------------|----------|
| Disk-backed map in `rehash-tmp/` + bounded RAM cache | Bounded RAM, crash-safe (a miss only costs a rewrite) | ✓ |
| No map | Each snapshot re-walks shared subtrees: time grows with snapshot count | |
| In-RAM map | Not allowed (owner: no in-RAM map of all objects) | |

**Choice:** first option; research picks the structure.

## Switch protocol and snapshot supersession

| Option | Description | Selected |
|--------|-------------|----------|
| Superseding snapshot records (same version) + root in one atomic new segment | One loader rule, no format change | ✓ |
| Atomic rewrite of all segment files | More I/O, more code | |

**Choice:** first option as default; research decides.

## GC and report

| Option | Description | Selected |
|--------|-------------|----------|
| Rehash runs a full GC itself under the held lock | Gives "bytes freed"; no second lock | ✓ |
| User runs `slicefs gc` after | No bytes-freed figure in the rehash report | |

**Choice:** first option.

## Dry-run

| Option | Description | Selected |
|--------|-------------|----------|
| Read-only walk, same lock and log checks, nothing written | Same refusals as a real run | ✓ |
| No checks in dry-run | Could report a run that would be refused | |

**Choice:** first option.

## Claude's Discretion

Names, message wording, flag word, progress interval, map structure details, plan split.

## Deferred Ideas

Online rehash job; removal of the `hash-sha256` flag; rechunk command; more hashes.
