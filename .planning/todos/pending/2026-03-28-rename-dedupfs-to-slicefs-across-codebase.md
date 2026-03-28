---
created: 2026-03-28T08:29:44.698Z
title: Rename dedupfs to slicefs across codebase
area: general
files:
  - crates/dedupfs-traits/Cargo.toml
  - crates/dedupfs-traits/src/lib.rs
  - crates/cas-local/Cargo.toml
  - crates/metadata/Cargo.toml
  - Cargo.toml
---

## Problem

The project was initially named "DedupFS" but should be rebranded to "SliceFS" (also styled as "Slice/FS" where appropriate). All references to "dedupfs" in crate names, module paths, documentation, and configuration need to be updated to "slicefs". This includes:

- Crate naming convention: `dedupfs-traits` → `slicefs-traits` (or similar)
- **Directory names**: `crates/dedupfs-traits/` → `crates/slicefs-traits/` (and any other dirs with "dedupfs")
- Binary name: `dedupfs` → `slicefs`
- Internal references in doc comments, error messages, README
- Cargo workspace member paths must be updated after directory renames

This should be done as a coordinated rename to avoid breaking imports across the workspace.

## Solution

Batch rename across all crate names, directory names, and internal references. Best done as a dedicated task after current phase work stabilizes, to avoid merge conflicts with in-flight plans. Consider:
1. Rename crate directories
2. Update all Cargo.toml `[package] name` fields
3. Update all `use dedupfs_*` imports
4. Update documentation and comments
5. Update .planning docs to reflect new naming
