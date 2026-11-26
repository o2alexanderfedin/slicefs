# Phase 22: Chunk profiles - Discussion Log

> **Audit trail only.** Do not use as input to planning, research, or execution agents.
> Decisions are captured in CONTEXT.md — this log preserves the alternatives considered.

**Date:** 2026-11-25
**Phase:** 22-chunk-profiles
**Mode:** `--auto` (recommended option picked for each area; owner decisions given by the orchestrator are locked)
**Areas discussed:** Header form, Flag class, Profile location, Zero chunk, Limits, CLI, Benchmark form

---

## Header form

| Option | Description | Selected |
|--------|-------------|----------|
| Setting line + flag, only for a non-default profile | Default header unchanged; `chunks <min> <avg> <max>` + `ro_compat chunk-profile <release>` otherwise | ✓ |
| Setting line + flag in every new store | Explicit profile always; changes the default header and its tests | |
| Setting line with no flag | Breaks the 21.1 rule "a new setting line always comes with a flag" | |

**Choice:** [auto] first option (recommended).

## Flag class

| Option | Description | Selected |
|--------|-------------|----------|
| ro_compat | Older build reads (lengths come from manifests) but must not write with its own profile | ✓ |
| incompat | Older build refuses the store; safe but stricter than needed | |
| compat | Older build would write 4/8/32 chunks into the store; breaks one profile per store | |

**Choice:** [auto] ro_compat; the planner verifies it against the zero shortcut paths.

## Profile location

| Option | Description | Selected |
|--------|-------------|----------|
| On the store (`Options` -> `ChunkStore::profile()`) | `file.rs` only changes constant reads; `FileWriter::new()` and `metadata::store_io` unchanged | ✓ |
| A parameter on each `FileWriter` | More call sites change, also in `metadata` | |
| Metadata values fixed at 4/8/32, data per store | Two profiles in one store; more code | |

**Choice:** [auto] on the store; metadata values follow the store profile (recorded for the report and Phase 24).

## Zero chunk

| Option | Description | Selected |
|--------|-------------|----------|
| Per-store zero data (or a cache keyed by max) | Fixes the process-wide `OnceLock` sized `CHUNK_MAX` | ✓ |
| Keep the global `OnceLock` | Wrong with two profiles in one process | |

**Choice:** [auto] per store, plus a zero-run premise check in the profile constructor.

## Limits

| Option | Description | Selected |
|--------|-------------|----------|
| Powers of two, min < avg < max, fastcdc v2020 limits | Plain reading of PROF-01; memory 2 x max per stream, bounded | ✓ |
| A tighter SliceFS cap (for example max <= 1 MiB) | Lower worst-case memory; not asked for | |

**Choice:** [auto] crate limits; memory cost documented.

## CLI

| Option | Description | Selected |
|--------|-------------|----------|
| `--chunk-profile MIN/AVG/MAX` (KiB) on `seed` and `mount` | Both can make a new store; another profile on an existing store is refused | ✓ |
| Only on `seed` | A new store made by `mount` could not get a profile | |

**Choice:** [auto] first option; `stats` shows the profile.

## Benchmark form

| Option | Description | Selected |
|--------|-------------|----------|
| In process, release Rust example + driver script, by hand | No mount, runs on macOS and Linux | ✓ |
| Through a FUSE mount in Docker | Slower, the VM is low on memory, measures FUSE more than chunking | |
| A `cargo bench` / test | Would enter the test suite | |

**Choice:** [auto] first option; Boost trees (two versions) and the Boost tar; report in `docs/research/chunk-profiles.md`.

## Claude's Discretion

- Names, the exact header word, message wording, the internal structure of the benchmark, the plan split.

## Deferred Ideas

- A default change (owner, after the report); size-class or bimodal chunking; per-file profiles; a rechunk command; the combined chunk and prolly benchmark (Phase 24).

## Owner questions (not blocking)

- The `<release>` word of the `chunk-profile` flag: placeholder `v2.2` until the owner names the release.
