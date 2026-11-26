# Phase 24: Prolly evaluation - Discussion Log

> **Audit trail only.** Do not use as input to planning, research, or execution agents.
> Decisions are captured in CONTEXT.md — this log preserves the alternatives considered.

**Date:** 2026-11-26
**Phase:** 24-prolly-evaluation
**Mode:** `--auto` (recommended option selected for each area, no questions asked)
**Areas discussed:** Which map, Boundary rule, Node storage, API and memory, Crate and benchmark layout, Benchmark method, Decision document

Owner decisions given with the request (locked): prototype plus written decision only; no migration, no format change, no default change; bounded memory (no whole map or file in RAM); FastCDC 4/8/32 KiB, BLAKE3-256 and the 1 TiB cap stay; one worktree per plan, no file in two plans of a wave; short tests, benchmark by hand or in Docker.

---

## Which map

| Option | Description | Selected |
|--------|-------------|----------|
| `inode_data` (u64 -> ObjectKey) | Largest flat map, touched by every file change, fixed-width values | ✓ |
| `dir_data` | Fewer entries (one per directory); the roadmap's example | |
| Directory lists (name -> inode) | Variable keys, but already one node per directory | |

**Choice:** `inode_data` (recommended default); researcher measures all six maps and the planner may switch with a reason. Variable-length keys are covered in unit tests.

## Boundary rule

| Option | Description | Selected |
|--------|-------------|----------|
| Dolt-style size-aware rule on a key hash | Threshold grows with node size; hard min/max | ✓ |
| Fixed probability on a key hash (like `file.rs` `is_cut`) | Simple; wide node size spread | |
| Fixed entry count | Not content-defined; fails the order-independence goal | |

**Choice:** Dolt-style, key-only hash (recommended default). Parameters from research.

## Node storage

| Option | Description | Selected |
|--------|-------------|----------|
| One `ChunkStore::put` object per node | Deduped, compressed, packed; ID is the pointer | ✓ |
| `store_io::put_value` (FastCDC + manifest) | Cuts already content-defined nodes again | |

**Choice:** one object per node (recommended default).

## API and memory

| Option | Description | Selected |
|--------|-------------|----------|
| Streaming build, get, sorted batch apply, cursor | Minimal set the benchmark needs; bounded RAM | ✓ |
| Plus diff of two roots | Useful for sync; not needed for the decision | |

**Choice:** minimal API (recommended default); diff deferred.

## Crate and benchmark layout

| Option | Description | Selected |
|--------|-------------|----------|
| `crates/prolly` + `benchmarks/prolly-bench` + `scripts/prolly-bench/` | Crate depends on chunk-store only; bench holds the metadata dependency | ✓ |
| Module inside `metadata` | Couples the prototype to the product crate | |

**Choice:** separate crate and bench crate (recommended default).

## Benchmark method

| Option | Description | Selected |
|--------|-------------|----------|
| Same store and edit script; whole-commit and map-only comparisons | Honest against the FastCDC-cut baseline | ✓ |
| Map-only microbenchmark | Hides the rest of the commit cost | |

**Choice:** both comparisons (recommended default). Note found while scouting: the current maps are already FastCDC-cut values, so the report separates serialized/hashed bytes from new stored bytes.

## Decision document

| Option | Description | Selected |
|--------|-------------|----------|
| Research report + short design-doc section + #19 comment text in phase dir | No `gh` writes by executors | ✓ |
| Only a research report | Misses "in the design docs" | |

**Choice:** report + design section + comment text (recommended default). Adoption itself is an owner one-way decision (Q-1..Q-3).

## Claude's Discretion

Names, node layout, hash, CDF parameters, edit-script details, a bounded cache, the design-doc file, the plan split.

## Deferred Ideas

Root diff; prolly file manifests (already content-cut); wiring into root record, GC, rehash, snapshots; a combined namespace map.
