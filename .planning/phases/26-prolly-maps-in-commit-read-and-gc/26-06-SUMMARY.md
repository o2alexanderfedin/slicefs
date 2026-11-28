---
phase: 26-prolly-maps-in-commit-read-and-gc
plan: 06
subsystem: metadata
tags: [prolly, boundary-salt, node-cache, mount-option, pmap-03]
requires: ["26-04"]
provides:
  - "StoreIo::with_tree_salt, StoreIo::set_node_cache_bytes; tree_config() is Config::DEFAULT.with_salt(header salt)"
  - "content::store_io and store_io_read_only read the header salt that the open already parsed"
  - "rehash builds its target trees with the header salt (both RehashOptions sites)"
  - "slicefs mount --meta-cache-size <SIZE> (default 64M, any size above 0)"
affects: [26-07, 26-08, 26-09, 26-14]
tech-stack:
  added: []
  patterns:
    - "one place sets the tree salt: content::store_io*"
    - "the cache cap is set on the StoreIo before load_from_root (the store keeps the cache it is given)"
key-files:
  created:
    - .planning/phases/26-prolly-maps-in-commit-read-and-gc/runs/gate-26-06.log
  modified:
    - crates/metadata/src/store_io.rs
    - crates/slicefs-cli/src/content.rs
    - crates/slicefs-cli/src/rehash.rs
    - crates/slicefs-cli/src/cli.rs
    - crates/slicefs-cli/src/mount.rs
    - crates/slicefs-cli/src/main.rs
    - crates/slicefs-cli/tests/boundary_salt_tests.rs
    - crates/slicefs-cli/Cargo.toml
    - Cargo.lock
    - README.md
key-decisions:
  - "Option name --meta-cache-size; --cache-size stays accepted and has no effect"
  - "A size of 0 is refused for --meta-cache-size; any size above 0 is allowed (no 4M minimum, unlike --dirty-budget)"
  - "load_store and load_store_read_only keep their signatures and call new load_store_with and load_store_read_only_with"
  - "StartArgs.meta_cache_size is an Option (None = 64 MiB), because StartArgs::default() gives zero values"
  - "A snapshot mount shares the StoreIo and so the cache and cap of the current root"
status: complete
metrics:
  duration: "about 1 h"
  completed: 2026-11-28
actuals:
  tokens: 9000
  tasks: 2
  commits: 2
plan_head_before: 4d63e86f5ee0e2d2cf5c48a8fafdd1c2dad7f78c
plan_head_after: 84fdebadaddb430cca28bd23912b4d7b4a4a819a
---

# Phase 26 Plan 06: Boundary salt for tree writers and --meta-cache-size Summary

Every tree writer now cuts with the boundary salt of the store header, and the node cache cap is the mount option `--meta-cache-size` (default 64M).

## What was built

- `store_io.rs`: `with_tree_salt(salt)` sets `tree_config` to `Config::DEFAULT.with_salt(salt)`; `set_node_cache_bytes(cap)` replaces the node cache with an empty one of that cap (call before the store loads).
- `content.rs`: `open` and `open_read_only` now share `open_parsed` / `open_read_only_with_salt`, which return the salt of the header they already parsed (no second header read). `store_io` and `store_io_read_only` call `with_tree_salt`. Every store open of seed, mount, snapshot, replay and commit goes through these two. `parse_dirty_bytes` is split into `parse_size` plus the 4M minimum; `parse_meta_cache_bytes` is `parse_size` plus "not 0".
- `rehash.rs`: the header salt is read in the existing header check and passed to `rewrite_and_switch` and `dry_run`; both `RehashOptions` take it (`tree_salt: None` is gone).
- `cli.rs`: `--meta-cache-size <SIZE>` (default `64M`). `mount.rs`: `load_store_with` and `load_store_read_only_with` call `set_node_cache_bytes` before `load_from_root`; `StartArgs.meta_cache_size`; `run_mount` takes the value (doc updated: `--cache-size` unused, names the new option). `main.rs` passes it.
- README: new section "Metadata cache (mount option)" with the option row, the memory sentence (dirty budget plus meta cache size plus small fixed buffers) and the `--cache-size` note.

## Tests

- `boundary_salt_tests` (4 new): `a_salted_store_writes_salted_trees` (1,500 files, tree height at least 2, root equals the salted `prolly::build` of its own entries and differs from the unsalted build), `an_unsalted_store_writes_unsalted_trees`, `the_node_store_has_the_salt_of_the_header` (both opens, salted and plain), `a_rehash_keeps_salted_trees` (sha256 target, root equals the salted build with the sha256 hasher and differs from the unsalted one). Mutation check: setting `tree_salt: None` in the rehash options made `a_rehash_keeps_salted_trees` fail.
- Unit tests: `meta_cache_size_*` in `cli.rs` (default 64 MiB, `1M`/`512`/`2G` parse, bad values refused: `0`, `-1`, `1.5M`, `64X`, empty, `M`, overflow), `content.rs` (parser), `mount.rs` (`load_store_with` with 1 MiB gives `node_cache().stats().cap == 1 MiB`; `load_store` keeps `NODE_CACHE_BYTES`).
- Gates: `runs/gate-26-06.log`, four sections, all `rc=0` (slicefs-cli lib; boundary_salt, rehash, mount_lifecycle tests; clippy `-D warnings` for slicefs-cli and metadata; `cargo test --workspace`). `retry_tests` did not fail.

## Decisions

See the frontmatter. Also: `scrub` loads the root with its own `StoreIo::new` to check that the metadata reloads. It only reads, so the salt does not matter there, and it was left alone. Test-only `StoreIo::open` sites keep the unsalted default.

## Deviations from Plan

**1. [Rule 3 - Blocking] Files outside `files_modified`**
- `crates/slicefs-cli/src/main.rs`: passes the new `run_mount` argument.
- `crates/slicefs-cli/Cargo.toml` and `Cargo.lock`: `prolly` as a dev-dependency (path dependency on the workspace crate, plan T-26-SC), for the tree rebuild in `boundary_salt_tests`.

**2. [Rule 1 - Bug] Clippy `type_complexity` in the new test**
- Found by the clippy gate; fixed with a type alias `Entries` in `boundary_salt_tests.rs`, then clippy and the CLI suites were run again. The workspace run was made just before this alias-only change; the changed test file was rebuilt and passed in the CLI suite run.

**3. One code commit for both tasks**
- Rule 2: `content.rs` carries both tasks and only the combined tree has a workspace run, so the two tasks are one commit (`1c275db`), plus the gate log commit (`84fdeba`).

**4. Signatures kept**
- The plan says to pass the value "into `load_store`, `load_store_read_only`". To avoid editing about 25 call sites in tests outside `files_modified`, the two functions keep their signature and delegate to `*_with` variants. The `set_node_cache_bytes` calls are in the `*_with` functions (2 in mount.rs).

## Known Stubs

None.

## Threat Flags

None. T-26-16: bad values are a clap usage error (tested), 0 is refused. T-26-17: one place sets the salt (`content::store_io*`), covered by the four salt tests.

## For later plans

- Plans 26-07 to 26-09 add trees that use `StoreIo::tree_config()` and `node_cache()` (through `TreeIo`); the salt and cap reach them with no further plumbing.
- A new store open that is not through `content::store_io*` (a production path, not a test) would write unsalted trees on a salted store. `scrub` is the only one today and it only reads.
- The 26-04 note about `tree_salt: None` in the CLI is resolved.

## Self-Check: PASSED

Commits `1c275db` and `84fdeba` are ancestors of HEAD; `runs/gate-26-06.log` exists with four `rc=0` lines and no `rc=` failure.
