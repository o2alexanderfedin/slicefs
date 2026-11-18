# Phase 9: Compression Removal - Research

**Researched:** 2026-03-29
**Domain:** Rust crate refactoring — remove compression layer from write/read path
**Confidence:** HIGH

---

<user_constraints>
## User Constraints (from CONTEXT.md)

### Locked Decisions

**Write Path (DECOMP-01)**
- `to_wire_bytes()` is removed or becomes identity — raw bytes go directly to FileStorageAdd
- No `compress_block` call anywhere in the write path
- Digest224 is computed on raw content (already the case — hash before compress was the v1.0 design)

**Store Format (DECOMP-02)**
- Store format version is v3 (raw blocks, no compression header)
- `store_version` field removed or fixed at 3 — no version gating needed
- No `AlgorithmId` header byte in stored blocks

**Read Path (DECOMP-03) — CLEAN BREAK**
- No v1/v2 backward compatibility — no production stores exist
- `from_wire_bytes()` is removed or becomes identity — stored bytes are raw content
- `decompress_block` calls removed entirely
- Old stores must be re-seeded (acceptable — no production data)

**Cross-Dedup (DECOMP-04)**
- Digest224 identity computed on raw content — cross-file dedup works naturally since there's only one format now

**Compression Crate**
- `slicefs-compression` crate can be removed from workspace or kept as dead code for future segment-level compression (v2.1)
- Remove `compressor` field from `SliceFsFilesystem` and `--compressor` CLI flags
- Remove `slicefs-compression` dependency from `slicefs-cli/Cargo.toml`

### Claude's Discretion
- Whether to keep `slicefs-compression` crate in workspace for future segment-level compression or remove entirely
- How to handle `store_version` field — remove, hardcode 3, or keep as config
- Cleanup of `AlgorithmId` references across the codebase

### Deferred Ideas (OUT OF SCOPE)
None — discussion stayed within phase scope. Segment-level compression noted as v2.1 item in PROJECT.md.
</user_constraints>

---

<phase_requirements>
## Phase Requirements

| ID | Description | Research Support |
|----|-------------|-----------------|
| DECOMP-01 | Write path pushes raw (uncompressed) bytes to Merkle tree — no compress_block call | `to_wire_bytes()` in filesystem.rs lines 333-339 calls `compress_block`; removing it makes write path pass raw bytes directly to `State::push_all` |
| DECOMP-02 | Store format version bumped to v3 (raw blocks, no compression header) | `store_version` field in `SliceFsFilesystem` and constructor; `run_mount` hard-codes version `2`; removing version gating means fixed v3 |
| DECOMP-03 | Read path handles v3 only (clean break — no v1/v2 legacy) | `from_wire_bytes()` in filesystem.rs lines 346-355 dispatches on `store_version`; remove entirely, return bytes as-is |
| DECOMP-04 | Digest224 identity is computed on raw content — cross-file dedup works regardless of historical compressor | With raw bytes flowing directly into `State::push_all`, the digest is always over raw content; identity dedup is automatic |
</phase_requirements>

---

## Summary

Phase 9 is a targeted surgical removal of compression infrastructure from the SliceFS write/read path. The compression layer was introduced in Phase 6 as a pluggable shim: `to_wire_bytes()` compresses raw bytes before CAS storage, and `from_wire_bytes()` decompresses on read, both gated on `store_version >= 2`. The decision to remove this layer is motivated by the v2.0 streaming writes design, which requires raw bytes to flow directly through `push_bytes` — compression at the block level is incompatible with incremental streaming.

The clean-break constraint (no production stores, no v1/v2 backward compatibility required) makes this straightforward: every call to `compress_block`/`decompress_block` can be deleted, the `store_version` field removed, and the `compressor` field and all associated CLI flags dropped. The `slicefs-compression` crate is only consumed by `slicefs-cli`; removing the dependency from `slicefs-cli/Cargo.toml` isolates the crate from the active code. The `Compressor` trait and `AlgorithmId` enum live in `slicefs-traits`, which is a separate decision to clean up.

The concrete surgery spans: two private methods in `filesystem.rs` (`to_wire_bytes` / `from_wire_bytes`), one struct field and constructor parameter (`compressor: Arc<dyn Compressor>`, `store_version: u32`), six call sites across filesystem.rs that invoke those methods, the `run_mount` function signature and compressor construction in `mount.rs`, two CLI argument definitions in `cli.rs`, the `compressor` field in `StoreStats`, the entire `compression_tests.rs` integration test file, and `NoneCompressor` imports in five other test files. The `seed.rs` path already pushes raw bytes without compression — it requires no changes.

**Primary recommendation:** Remove `to_wire_bytes`/`from_wire_bytes` and the `compressor`/`store_version` fields entirely from `SliceFsFilesystem`. Update `SliceFsFilesystem::new` signature (drop two parameters). Update all callers (mount.rs, tests). Remove CLI flags. Remove `slicefs-compression` from `slicefs-cli/Cargo.toml`. Keep the `slicefs-compression` crate in the workspace for future segment-level use.

---

## Standard Stack

### Core (unchanged — no new libraries needed)

| Library | Version | Purpose | Why Standard |
|---------|---------|---------|--------------|
| blockset | workspace | CAS Merkle tree via `State::push_all` / `file_storage_get` | Already the write/read primitive; raw bytes flow directly |
| slicefs-traits | workspace | `Digest224`, `MetadataStore`, etc. | All trait bounds remain; `Compressor` trait stays in crate but unused by CLI |

### Libraries Being Removed from slicefs-cli

| Library | Was Used For | Removal Notes |
|---------|-------------|---------------|
| slicefs-compression | `compress_block`, `decompress_block`, `NoneCompressor` in tests | Remove from `slicefs-cli/Cargo.toml` `[dependencies]` |
| slicefs-traits::compressor | `Arc<dyn Compressor>` field, `AlgorithmId` | Remove field and imports from filesystem.rs; trait stays in crate |

---

## Architecture Patterns

### Recommended Project Structure (after removal)

```
crates/
├── slicefs-cli/
│   ├── src/
│   │   ├── filesystem.rs     # Remove compressor field, store_version, to_wire_bytes, from_wire_bytes
│   │   ├── mount.rs          # Remove compressor_name/level params, parse_compressor call
│   │   ├── cli.rs            # Remove --compressor, --compressor-level args from Mount variant
│   │   └── stats.rs          # Remove compressor field from StoreStats, hardcode "none (v3)"
│   ├── tests/
│   │   ├── compression_tests.rs    # DELETE entirely (tests compression behavior no longer present)
│   │   ├── write_path_tests.rs     # Update fresh_fs() — remove NoneCompressor, store_version args
│   │   ├── fsync_tests.rs          # Update make_fs() — same
│   │   ├── dir_link_tests.rs       # Update fresh_fs() — same
│   │   ├── statfs_tests.rs         # Update helper — same
│   │   ├── posix_compliance_tests.rs # Update helper — same
│   │   └── crash_recovery_tests.rs   # Update make_fs() — same
│   └── Cargo.toml            # Remove slicefs-compression dependency
├── slicefs-compression/      # Keep in workspace, no changes needed
└── slicefs-traits/           # Keep Compressor/AlgorithmId (future use), no changes needed
```

### Pattern 1: Removing a Struct Field with Multiple Callers

**What:** `SliceFsFilesystem::new` currently takes `compressor: Arc<dyn Compressor>` and `store_version: u32` as its 4th and 5th parameters. Removing them is a compile-error-driven refactor — every caller fails to compile until updated.

**When to use:** Clean break with no callers outside the crate that need the old signature.

**Current constructor (to be simplified):**
```rust
// filesystem.rs — CURRENT (6 parameters)
pub fn new(
    meta: DictMetadataStore,
    io: Arc<Mutex<StoreIo>>,
    store_path: Option<PathBuf>,
    compressor: Arc<dyn Compressor>,  // REMOVE
    store_version: u32,               // REMOVE
) -> Self
```

**Target constructor (after removal):**
```rust
// filesystem.rs — TARGET (4 parameters)
pub fn new(
    meta: DictMetadataStore,
    io: Arc<Mutex<StoreIo>>,
    store_path: Option<PathBuf>,
) -> Self
```

**Callers that must be updated:**

| File | Location | Change Required |
|------|----------|----------------|
| `mount.rs` | `run_mount()` line 233 | Remove `compressor` and `2` args |
| `tests/write_path_tests.rs` | `fresh_fs()` line 24 | Remove `NoneCompressor`, `1` |
| `tests/fsync_tests.rs` | `make_fs()` line 28 | Remove `NoneCompressor`, `1` |
| `tests/dir_link_tests.rs` | line 26 | Remove `NoneCompressor`, `1` |
| `tests/statfs_tests.rs` | lines 23, 277, 304 | Remove `NoneCompressor`, `1` |
| `tests/posix_compliance_tests.rs` | line 32 | Remove `NoneCompressor`, `1` |
| `tests/crash_recovery_tests.rs` | line 36 | Remove `NoneCompressor`, `1` |

### Pattern 2: Simplifying Private Methods to Identity (or Deletion)

**What:** `to_wire_bytes` and `from_wire_bytes` are private methods on `SliceFsFilesystem`. They can be deleted entirely; their 6 call sites in filesystem.rs are inlined as direct byte references.

**Call sites in filesystem.rs (all become no-ops):**

| Line | Current | Replacement |
|------|---------|-------------|
| 392 | `let wire_bytes = self.to_wire_bytes(&buf);` | `let wire_bytes = buf.clone();` or pass `&buf` directly |
| 433 | `let wire_bytes = self.to_wire_bytes(&buf);` | same |
| 501 | `self.from_wire_bytes(wire_bytes)` | `wire_bytes` |
| 516 | `let wire_bytes = self.to_wire_bytes(&content);` | same |
| 901 | `let wire_bytes = self.to_wire_bytes(target_bytes);` | `let wire_bytes = target_bytes.to_vec();` |
| 936 | `let raw_bytes = self.from_wire_bytes(wire_bytes);` | `let raw_bytes = wire_bytes;` |
| 1073 | `let raw_bytes = self.from_wire_bytes(wire_bytes);` | `let raw_bytes = wire_bytes;` |

Note: line 323 in `test_read` also calls `from_wire_bytes`.

### Pattern 3: CLI Argument Removal (clap)

**What:** Remove two fields from the `Mount` variant of the `Cmd` enum in `cli.rs`, remove their wiring in `main.rs`, and remove their forwarding to `run_mount`.

**cli.rs** — remove from `Cmd::Mount { ... }`:
```rust
// REMOVE these two fields:
/// Block compressor: zstd, lz4, or none (default: zstd).
#[arg(long, default_value = "zstd")]
compressor: String,
/// Compression level (zstd: 1-22, default 3; lz4: ignored).
#[arg(long)]
compressor_level: Option<i32>,
```

**main.rs** — remove `compressor` and `compressor_level` from the `Cmd::Mount` destructure and the `run_mount` call.

**mount.rs** — remove `compressor_name: &str`, `compressor_level: Option<i32>` from `run_mount` signature; remove `parse_compressor` call; remove `slicefs_compression::parse_compressor` import.

**Tests affected:** All `cli.rs` tests that construct `--compressor` or `--compressor-level` args must be deleted or updated. Six tests in `compression_tests.rs` for `test_mount_default_compressor_is_zstd`, `test_mount_compressor_lz4`, etc.

### Pattern 4: Stats Cleanup

**What:** `StoreStats.compressor` field is informational only. After removal it becomes either "none (v3 raw)" hardcoded, or the field is dropped from the struct entirely.

**Recommendation (Claude's discretion):** Keep the field but hardcode to `"none (v3 raw)"` — JSON consumers may depend on the key being present. This is a one-line change in `run_stats`.

### Anti-Patterns to Avoid

- **Leaving dead imports:** After removing `slicefs-compression` from `Cargo.toml`, any `use slicefs_compression::...` line is a compile error — do not leave them in test files as comments.
- **Partial removal:** Removing the `compressor` field but leaving `store_version` creates a confused codebase. Both must go together since `store_version` only exists to gate compression behavior.
- **Allocating a clone when a reference suffices:** When replacing `self.to_wire_bytes(&buf)` calls, prefer passing `&buf` directly to `State::push_all` if the API accepts `&[u8]`, rather than cloning into a new `Vec<u8>`.

---

## Don't Hand-Roll

| Problem | Don't Build | Use Instead | Why |
|---------|-------------|-------------|-----|
| Removing unused Cargo deps | Manual Cargo.toml edits are correct | `cargo check` to verify | Compiler will flag any lingering `use slicefs_compression::` |
| Finding all call sites | Manual grep | `cargo check` after removing the field — compile errors enumerate every caller | Type system is the perfect call-site tracker |
| Verifying no compression in stored bytes | Custom inspection tool | Write a test: write raw bytes, read back, assert equal without any transform | Existing test patterns (`test_read` / `test_write`) already cover this |

**Key insight:** Rust's borrow checker and type system make this refactor mechanical — remove the field, watch the compile errors, fix each one. No call site can be missed.

---

## Common Pitfalls

### Pitfall 1: Leaving `store_version` Field Without `compressor`
**What goes wrong:** If `store_version` is kept (e.g., hardcoded to 3) but the `to_wire_bytes`/`from_wire_bytes` methods are removed, the field serves no purpose and confuses future readers.
**Why it happens:** Incremental removal — fixing one thing at a time.
**How to avoid:** Remove both `compressor` and `store_version` in the same commit. The methods depend on both.
**Warning signs:** `store_version` referenced in comments but not in logic.

### Pitfall 2: Forgetting `test_read` in filesystem.rs
**What goes wrong:** `test_read` at line 323 calls `self.from_wire_bytes(wire_bytes)` — it is a public test helper, not a FUSE callback, and may be missed in the sweep of FUSE callback edits.
**Why it happens:** It's in a different section of the file (pub test helpers, not the Filesystem impl block).
**How to avoid:** grep for `from_wire_bytes` and `to_wire_bytes` in filesystem.rs after the changes; the compiler will also catch it.

### Pitfall 3: Leaving `NoneCompressor` Imports in Tests After Removing the Dependency
**What goes wrong:** `use slicefs_compression::NoneCompressor;` in six test files becomes a compile error once `slicefs-compression` is removed from `slicefs-cli/Cargo.toml`.
**Why it happens:** Removing the dependency before updating the test files.
**How to avoid:** Update all test files that import from `slicefs_compression::*` before or in the same commit as the Cargo.toml change.

### Pitfall 4: Accidentally Breaking `blockset` Signature
**What goes wrong:** `State::push_all` takes `&mut impl Tree` plus `&[u8]`. The raw bytes slice must be passed directly — no intermediate `wire_bytes: Vec<u8>` allocation needed.
**Why it happens:** Copy-paste from the existing pattern that assigns `wire_bytes = self.to_wire_bytes(&buf)` then passes `&wire_bytes`.
**How to avoid:** Pass `&buf` directly to `State::push_all(&mut fsa, &buf)` to avoid the allocation. The Rust compiler will enforce the types.

### Pitfall 5: symlink write path at line 901
**What goes wrong:** `simulate_symlink` at line 901 calls `self.to_wire_bytes(target_bytes)` — this stores symlink targets through the compression path. If missed, symlinks will still go through the old path.
**Why it happens:** Symlink writes are in a separate method from regular file writes; reviewers may focus only on `flush_buffer_to_cas`.
**How to avoid:** Treat every `to_wire_bytes` / `from_wire_bytes` call uniformly — grep confirms 7 call sites total in filesystem.rs.

### Pitfall 6: Stats JSON API Break
**What goes wrong:** Removing the `compressor` field from `StoreStats` breaks any downstream tooling consuming the JSON output.
**Why it happens:** Treating it as dead code when it may be part of a stable JSON contract.
**How to avoid:** Keep the field in the JSON struct, update the value to `"none (v3 raw)"`. This is safe — content changes, key stays.

---

## Code Examples

Verified patterns from codebase inspection:

### Write Path After Removal (flush_buffer_to_cas)
```rust
// filesystem.rs — flush_buffer_to_cas after removing to_wire_bytes
fn flush_buffer_to_cas(&self, ino: u64, buf: Vec<u8>) -> Result<(), i32> {
    if buf.is_empty() {
        self.meta.set_manifest(ino, &[]).map_err(|_| libc::EIO)?;
    } else {
        // Raw bytes go directly to FileStorage — no compression header
        let content_digest = {
            let mut io = self.io.lock().unwrap();
            let mut fsa = FileStorageAdd::new(&mut *io);
            let digest = State::push_all(&mut fsa, &buf);  // &buf directly, no wire_bytes
            drop(fsa);
            digest
        };
        self.meta.set_manifest(ino, &[content_digest]).map_err(|_| libc::EIO)?;
        self.meta.increment_refcount(&content_digest);
    }
    // ... inode update unchanged
    Ok(())
}
```

### Read Path After Removal (test_read)
```rust
// filesystem.rs — test_read after removing from_wire_bytes
pub fn test_read(&self, ino: u64, offset: u64, size: u32) -> Result<Vec<u8>, i32> {
    let manifest = self.meta.get_manifest(ino).map_err(|_| libc::EIO)?;
    if manifest.is_empty() {
        return Ok(vec![]);
    }
    let root_digest = manifest[0];
    let raw_bytes: Vec<u8> = {
        let mut io = self.io.lock().unwrap();
        file_storage_get(&mut *io, &root_digest)
            .ok_or(libc::EIO)?
    };
    // No from_wire_bytes — raw_bytes IS the content
    let start = (offset as usize).min(raw_bytes.len());
    let end = (start + size as usize).min(raw_bytes.len());
    Ok(raw_bytes[start..end].to_vec())
}
```

### Constructor After Removal
```rust
// filesystem.rs — SliceFsFilesystem::new after field removal
pub fn new(
    meta: DictMetadataStore,
    io: Arc<Mutex<StoreIo>>,
    store_path: Option<PathBuf>,
) -> Self {
    Self {
        meta: Arc::new(meta),
        io,
        open_files: Mutex::new(HashMap::new()),
        next_fh: AtomicU64::new(0),
        store_path,
        auto_snapshot: false,
    }
}
```

### Test Helper After Removal
```rust
// tests/write_path_tests.rs — fresh_fs() after removal
fn fresh_fs() -> (SliceFsFilesystem, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let io = Arc::new(Mutex::new(StoreIo::new(dir.path())));
    let meta = DictMetadataStore::new(io.clone());
    let fs = SliceFsFilesystem::new(meta, io, None);  // no compressor/version args
    (fs, dir)
}
```

### Cargo.toml After Removal
```toml
# slicefs-cli/Cargo.toml — [dependencies] after removal
[dependencies]
slicefs-traits       = { path = "../slicefs-traits" }
# slicefs-compression removed — no compression in v3 write/read path
metadata             = { path = "../metadata" }
blockset             = { path = "../data-id/blockset" }
```

---

## State of the Art

| Old Approach | Current Approach | When Changed | Impact |
|--------------|------------------|--------------|--------|
| Raw bytes (v1, no header) | 1-byte AlgorithmId header + payload (v2) | Phase 6 | Compression overhead in write/read path |
| v2 with compression | v3 raw bytes (Phase 9) | Phase 9 (now) | Removes compression overhead; prepares write path for streaming |
| `store_version` gating | No version gating (fixed v3) | Phase 9 (now) | Simpler read path; old stores require re-seed |

**Deprecated/outdated after Phase 9:**
- `compress_block` / `decompress_block` functions: no longer called from CLI write/read path
- `compressor: Arc<dyn Compressor>` field in `SliceFsFilesystem`: removed
- `store_version: u32` field: removed
- `--compressor` / `--compressor-level` CLI flags: removed
- `compression_tests.rs` integration test file: deleted (tests compression behavior that no longer exists)
- `compressor: "zstd (default)"` in `StoreStats`: replaced with `"none (v3 raw)"`

---

## Open Questions

1. **Keep or remove `Compressor` / `AlgorithmId` from `slicefs-traits`?**
   - What we know: `slicefs-traits` has no direct dependency on `slicefs-compression`; the trait is defined in `slicefs-traits/src/compressor.rs` and re-exported via `slicefs-traits::Compressor`
   - What's unclear: Whether keeping dead trait definitions in `slicefs-traits` is confusing vs. useful for future segment-level compression
   - Recommendation: Keep in `slicefs-traits` (no cost, preserves future use); add a `// Future: segment-level compression (v2.1)` doc comment

2. **Remove `slicefs-compression` from workspace members or just from CLI deps?**
   - What we know: It is currently in `workspace.members`; removing it from the workspace deletes it from `cargo build --workspace`
   - What's unclear: Whether future phases will need it immediately
   - Recommendation (Claude's discretion): Keep in workspace members, remove only from `slicefs-cli/Cargo.toml`. This preserves the crate for v2.1 without breaking anything.

3. **`statfs_tests.rs` lines 277 and 304 — what context are those in?**
   - What we know: Two additional `NoneCompressor` usages in statfs_tests.rs beyond line 23
   - What's unclear: Whether they build a separate `SliceFsFilesystem` instance or share the top-level one
   - Recommendation: Verify during implementation — likely both are inline `SliceFsFilesystem::new` calls in individual test functions; update all three.

---

## Validation Architecture

### Test Framework

| Property | Value |
|----------|-------|
| Framework | Rust built-in test + cargo test |
| Config file | none — workspace Cargo.toml |
| Quick run command | `cargo test -p slicefs-cli 2>&1 \| tail -20` |
| Full suite command | `cargo test --workspace 2>&1 \| tail -30` |

### Phase Requirements → Test Map

| Req ID | Behavior | Test Type | Automated Command | File Exists? |
|--------|----------|-----------|-------------------|-------------|
| DECOMP-01 | Write path pushes raw bytes, no compress_block call | unit | `cargo test -p slicefs-cli test_write_raw_no_compression -- --nocapture` | Wave 0 (new test) |
| DECOMP-01 | Existing write_path_tests still pass with updated fresh_fs | unit | `cargo test -p slicefs-cli --test write_path_tests` | Yes |
| DECOMP-02 | SliceFsFilesystem::new() accepts no compressor/version args | unit/compile | `cargo build -p slicefs-cli` | implicitly verified by compile |
| DECOMP-02 | Stats reports "none (v3 raw)" for compressor field | unit | `cargo test -p slicefs-cli test_stats` | Yes (existing stats tests) |
| DECOMP-03 | Read path returns raw bytes unchanged | unit | `cargo test -p slicefs-cli test_read_returns_raw_bytes -- --nocapture` | Wave 0 (new test) |
| DECOMP-03 | All existing read tests pass (posix, fsync, crash_recovery) | integration | `cargo test -p slicefs-cli` | Yes |
| DECOMP-04 | Same raw content → same Digest224 → dedup | unit | `cargo test -p slicefs-cli test_raw_content_dedup` | Wave 0 (new test) |

### Sampling Rate
- **Per task commit:** `cargo test -p slicefs-cli 2>&1 | tail -20`
- **Per wave merge:** `cargo test --workspace 2>&1 | tail -30`
- **Phase gate:** Full workspace suite green before `/gsd:verify-work`

### Wave 0 Gaps

- [ ] `crates/slicefs-cli/tests/compression_tests.rs` — DELETE this file (tests v1/v2 behavior that no longer exists)
- [ ] New test: `test_write_raw_no_compression` — write file, verify `file_storage_get` returns exact bytes written (no header byte) — covers DECOMP-01
- [ ] New test: `test_read_returns_raw_bytes` — round-trip write+read with v3 filesystem, assert exact match — covers DECOMP-03
- [ ] New test: `test_raw_content_dedup` — two files with same content produce same manifest digest — covers DECOMP-04
- [ ] These three tests can live in a new `crates/slicefs-cli/tests/v3_store_tests.rs` file

*(New tests replace the deleted compression_tests.rs, covering the v3 invariants rather than the old v1/v2 wire format behavior)*

---

## Sources

### Primary (HIGH confidence)
- Direct codebase inspection — all findings verified against source files in `/Volumes/Unitek-B/Projects/file-systems/crates/`
- `filesystem.rs` — `to_wire_bytes` (lines 333-339), `from_wire_bytes` (lines 346-355), all 7 call sites
- `mount.rs` — `run_mount` signature, `SliceFsFilesystem::new` call at line 233
- `cli.rs` — `--compressor` / `--compressor-level` fields in `Cmd::Mount`
- `compression_tests.rs` — full test file, 12 tests to be deleted
- 6 other test files — `NoneCompressor` import pattern confirmed

### Secondary (MEDIUM confidence)
- CONTEXT.md — locked decisions from user discussion
- STATE.md / REQUIREMENTS.md — phase requirement descriptions and acceptance criteria

---

## Metadata

**Confidence breakdown:**
- Standard stack: HIGH — inspected actual source files, confirmed all call sites
- Architecture: HIGH — concrete line numbers identified, change patterns exact
- Pitfalls: HIGH — derived from actual code structure, not speculation
- Test gaps: HIGH — deletion of compression_tests.rs is correct; new v3 tests are clearly scoped

**Research date:** 2026-03-29
**Valid until:** Until Phase 9 plan is written (this codebase is stable; no external dependencies changing)
