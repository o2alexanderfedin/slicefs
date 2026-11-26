# Phase 22: Chunk profiles - Context

**Gathered:** 2026-11-25 (auto mode; owner decisions locked, other choices are the recommended defaults and are reversible: no deployments, no store backward compatibility)
**Status:** Ready for planning

<domain>
## Phase Boundary

A store has one FastCDC profile (min/avg/max), fixed when the store is made (`seed`, or the first `mount` of a new store), and a benchmark shows what four larger profiles cost and gain against 4/8/32 KiB. The default stays 4/8/32 KiB (#67, PROF-01, PROF-02, PROF-03).

In scope:
- A chunk profile type in `chunk-store`, carried by the store (`Options` -> `ChunkStore`); every use of the chunk constants reads the profile of the store.
- The profile in the store header (`<store>/FORMAT`) with a feature flag, by the Phase 21.1 rules; `--chunk-profile` on `seed` and `mount`; a different profile on an existing store is refused; `stats` shows the profile.
- Tests: two stores with different profiles in one process (byte-for-byte read back, zero runs, a spliced edit), a golden cut-point test, header and refusal tests.
- A reproducible benchmark for the five profiles that runs outside the test suite, and a committed report that recommends a default.

Out of scope: a change of the default profile; size-class or bimodal chunking (parked); per-file or per-directory profiles; re-chunking an existing store to another profile (a "rechunk" command); prolly-tree work (Phase 24) and the combined chunk-plus-tree benchmark (Phase 24 uses these profiles); new hashes (Phase 23).

</domain>

<decisions>
## Implementation Decisions

### Locked by the owner (do not reopen)
- **D-01:** The chunk profile is fixed per store at `seed` time, in the store header. The default stays FastCDC 4/8/32 KiB. Phase 22 benchmarks five profiles (4/8/32, 8/16/64, 8/32/128, 16/64/256, 32/128/512 KiB) and does not change the default. Size-class and bimodal ideas stay parked.
- **D-02:** No store backward compatibility. The Phase 21.1 header and format policy (format number 7; compat, ro_compat and incompat flags; `docs/design/store-format.md`) is the place for the profile.
- **D-03:** BLAKE3-256 full IDs, no bit saving. Online GC stays mandatory. Per-file daemon memory stays bounded and never O(file size).
- **D-04:** Phase 23 edits `crates/chunk-store/src/file.rs` and the FORMAT marker after this phase. Keep the Phase 22 changes in `file.rs` small: replace the constant reads, do not restructure.
- **D-05:** Phase 21.2 safety gates (`scripts/safety/*`, `.github/workflows/safety.yml`, the release gate) stay green for every plan that touches concurrency, unsafe code or the Kani-proved header core. The unsafe-count doc must match (`scripts/safety/unsafe-count.sh`).
- **D-06:** Tests stay short (test-time budget). The benchmark runs by hand, never in the test suite. Local gate: `timeout 3600 cargo test --workspace -- --skip a_panic_on_another_thread_does_not_abort --skip a_reader_thread_panic_aborts_and_the_remount_replays --skip a_worker_panic_aborts_and_the_remount_replays --skip a_full_queue_that_stays_full_aborts`.
- **D-07:** No local macOS mounts. Linux FUSE tests run in OrbStack Docker (`scripts/docker-linux-test/run.sh`).
- **D-08:** Plans run as executors in their own git worktrees (local branch `exec/22-NN`). Executors never push.

### Header form
- [auto] Header form — Q: "How is the profile kept in FORMAT?" → Selected: "A setting line plus a flag, written only for a non-default profile" (recommended default)
- **D-09:** A store with the default profile has no profile line and no flag: its header stays exactly as today (`slicefs-store 7 ...` + `hash <name>`). No profile line means 4/8/32 KiB, now and after v2.2 (criterion 4). The existing default-header tests stay unchanged.
- **D-10:** A non-default profile writes two lines: a setting line `chunks <min> <avg> <max>` (sizes in bytes, decimal) and a flag line `ro_compat chunk-profile <release>`. This follows the 21.1 rule "a new setting line always comes with a flag", so an older build never meets the `chunks` line without an unknown flag. `chunk-profile` goes into `KNOWN_FLAGS`. At most one `chunks` line; a `chunks` line with no `chunk-profile` flag, or a flag with no line, is refused as a bad header (M9). The exact word and message text are the planner's choice, documented in `store-format.md`. The `<release>` word comes from one constant in `header.rs`; its value is the milestone name `v2.2` as a placeholder, because the release name is still open with the owner (21.1). Tests do not depend on the value. — **Reversibility:** reversible — no deployed stores; the line is one parse rule in `header.rs`.

### Flag class
- [auto] Flag class — Q: "What may an older build do with a store that has a non-default profile?" → Selected: "ro_compat (read, no write)" (recommended default)
- **D-11:** `ro_compat`. Reason: a reader takes each chunk length from the manifest entries, so an older build reads the store correctly. Its zero shortcut in the reader matches only zero chunks of its own `CHUNK_MAX` length and otherwise reads the chunk, so reads stay correct. But an older build would write with 4/8/32 KiB, which breaks the one-profile-per-store rule and the "a spliced file has the key of a fresh write" property, so it must not write. The planner verifies this reasoning against `known_zero` and the reader zero path before it locks the class; if a read path is found that depends on the profile, the class becomes `incompat`. — **Reversibility:** reversible — no deployed stores.

### Where the profile lives
- [auto] Profile location — Q: "Where does the code get the profile?" → Selected: "On the store: `chunk_store::Options` -> `ChunkStore::profile()`" (recommended default)
- **D-12:** A `ChunkProfile { min, avg, max }` type in `chunk-store` with a `DEFAULT` (4/8/32 KiB), a validating constructor and a `ranges(&[u8])` method (FastCDC v2020). `Options` gets a `profile` field (default `DEFAULT`); `ChunkStore::profile()` returns it. `file.rs` reads `store.profile()` where it now reads `CHUNK_MIN`/`CHUNK_AVG`/`CHUNK_MAX`/`chunk_ranges` (writer buffer limit `2 * max`, `cut`, zero shortcut, `known_zero`, splice re-chunk and read-ahead step `avg`). `FileWriter::new()` and its `store` arguments stay as they are (every method already takes `&ChunkStore`), and `metadata::store_io` needs no change.
- **D-13:** The free function `chunk_ranges(data)` and the constants stay only as the default profile (for tests and existing callers), or are removed where a caller has a store; no non-test code path uses a constant in place of the store profile (PROF-02). The planner lists every site (grep: `CHUNK_MIN|CHUNK_AVG|CHUNK_MAX|chunk_ranges`, also `crates/slicefs-cli/src/content.rs` ~373 and `crates/slicefs-cli/tests/write_path_tests.rs` ~503).
- **D-14:** One profile per store for all FastCDC use: file data and the metadata values that `metadata::store_io::put_value` cuts. Manifest node boundaries (`LEAF_ENTRIES`, `NODE_MIN`, `is_cut` on IDs) do not depend on the profile (issue #67: data chunking stays independent of the tree). The benchmark report states that metadata values follow the store profile, because it changes the rewrite-bytes metric and Phase 24 reads it.

### Zero chunk
- **D-15:** Must fix: `zero_chunk()` in `file.rs` is a process-wide `OnceLock` sized `CHUNK_MAX`. With two profiles in one process it gives the wrong buffer and IDs. Replace it with per-profile zero data owned by the store (computed when the store opens, or a small cache keyed by `max`). Memory is one zero buffer of `max` bytes per store (at most the crate maximum), bounded.
- **D-16:** The zero-shortcut premise ("FastCDC cuts a run of zeros into chunks of `max` bytes") is tested for each of the five profiles (generalize `zero_runs_cut_at_chunk_max`). The profile constructor also checks it (chunk `2 * max + 1` zero bytes once) and refuses a profile where it fails, so the shortcut can never give a wrong key.

### Limits and CLI
- [auto] Limits — Q: "Which profiles are accepted?" → Selected: "Powers of two, min < avg < max, inside the fastcdc v2020 crate limits" (recommended default)
- **D-17:** Accept min, avg, max that are powers of two, with min < avg < max, inside the `fastcdc::v2020` limits (min 64 B..1 MiB, avg 256 B..4 MiB, max 1 KiB..16 MiB), and that pass the zero check (D-16). The writer keeps under `2 * max` bytes per open stream and a stream step clones it, so a 16 MiB max costs up to about 32 MiB per writer plus a clone: bounded, not O(file size) (D-03). The doc states this cost. If the dirty budget or `memory_bound_tests` assume `2 * CHUNK_MAX`, they read the store profile.
- [auto] CLI — Q: "Which commands take the profile and how?" → Selected: "`--chunk-profile MIN/AVG/MAX` in KiB on `seed` and `mount`" (recommended default)
- **D-18:** `--chunk-profile <min>/<avg>/<max>` with sizes in KiB (for example `16/64/256`), on `seed` and `mount` (both can make a new store). For a new store it sets the profile in the first header write (before `packs/` and `segments/` exist, as for `--hash`). For an existing store: the same profile is accepted; another profile is refused with a message that names both profiles (unlike `--hash`, there is no rewrite). Without the option, an existing store uses its recorded profile and a new store gets the default. A ro_compat (read-only) mount refuses `--chunk-profile` if it differs, as it refuses `--hash`.
- **D-19:** `slicefs stats` prints the profile (human line and a `--json` field), also for a default store (shown as 4/8/32 KiB, from the absence of the line).
- **D-20:** The content store opens with the header profile at every open point (`content::open`, `open_read_only`, `store_io`, `store_io_read_only`), so gc, scrub, snapshot and seed all use the store profile.

### Tests
- **D-21:** Golden cut-point test (in `chunk-store`): a fixed pseudo-random input (for example the xorshift generator already used in tests, about 1 MiB) and, for each of the five profiles, the recorded cut offsets (the full list for the default profile; for the others a count plus a digest of the offset list is enough). The test fails if any cut point moves.
- **D-22:** Two stores with different profiles in one process (default and one large profile, for example 16/64/256): a file written under each reads back byte for byte, including a file with zero runs (sparse, the zero shortcut) and a spliced edit whose key equals the key of a fresh write of the same bytes. One test at `chunk-store` level and one through the `slicefs-cli` content/filesystem layer (in process, no mount).
- **D-23:** Header and CLI tests: a non-default profile writes the `chunks` line and the flag; a default profile writes neither; seed or mount of an existing store with another profile is refused; a bad profile (not a power of two, out of order, out of limits) is refused at parse; a `chunks` line without the flag is refused; `stats` shows the profile. The Kani harness of the header core is updated if the byte-level core changes, and `scripts/safety/kani.sh` stays green.
- **D-24:** No new Docker mount test is required; the in-process tests cover the profile paths. The planner may add one short Linux Docker check (seed with a large profile, mount, write, read back) only if a path is not reachable in process.

### Benchmark and report
- [auto] Benchmark form — Q: "Where and how does the benchmark run?" → Selected: "In process, no mount: a release-built Rust example plus one driver script, run by hand on macOS or Linux" (recommended default)
- **D-25:** One driver script (for example `scripts/chunk-profiles/bench.sh` with a README, the style of `scripts/pack-measure/`) that builds in release and runs a Rust example (or bin) in process for each of the five profiles. It needs no mount, so it runs on the macOS dev machine and in Linux. It writes CSV to stdout and a work dir, and it is never part of `cargo test`.
- **D-26:** Inputs: two successive Boost trees (for example 1_84_0 then 1_85_0 seeded into the same store, for cross-version dedup) and the Boost tar, from `scripts/macos-mount-test/fetch-boost.sh`. Optional inputs by environment variable (a random file, a sparse file).
- **D-27:** Metrics for each profile (PROF-03): physical bytes after dedup and zstd (packs apparent and allocated, index included), object count, index RAM and cold-open time (store open plus index load in a new process; the page cache is warm unless dropped by hand, and the report says which), ingest throughput (MB/s and CPU time), read latency (sequential and random 4 KiB reads, p50/p99, through the file reader), and rewrite bytes and objects after small edits (insert, delete and modify at the start, middle and end of a file, through `splice`). Several runs, median reported.
- **D-28:** The report goes to `docs/research/chunk-profiles.md`: the command lines, machine, inputs, the table for the five profiles, the trade-off, and a recommended default. It states that the code default stays 4/8/32 KiB in v2.2 (any change is an owner decision later). A plan runs the benchmark by hand (time-limited) and commits the report. Posting a summary on #67 is left to the orchestrator or owner (executors run no `gh` write command).

### Docs
- **D-29:** `docs/design/store-format.md`: the grammar table gets the `chunks` line, the flag list gets the `chunk-profile` row (class, release, meaning), with an example header. `docs/design/chunk-store.md`: the chunking section says the profile is per store and points to the header doc. README: one line for `--chunk-profile` if the README lists seed or mount options.

### Claude's Discretion
- Type, field and function names; the exact header word (`chunks`) and message wording (plain, short); whether `chunk_ranges` and the constants stay as default-profile helpers or go; the benchmark example's internal structure; the plan split.
- Suggested plan split (planner may change it): 01 chunk-store profile type, store field, `file.rs` reads, per-store zero chunk, golden and two-store tests (wave 1); 02 header line and flag, `--chunk-profile` on seed and mount, refusals, stats, docs, Kani (wave 2); 03 benchmark example and script with README (wave 2, after 01); 04 run the benchmark and write the report (wave 3).

</decisions>

<canonical_refs>
## Canonical References

**Downstream agents MUST read these before planning or implementing.**

### Phase scope and requirements
- `.planning/ROADMAP.md` — Phase 22 goal and success criteria; owner decisions for the milestone; research flag "skip for Phase 22: standard patterns"
- `.planning/REQUIREMENTS.md` — PROF-01, PROF-02, PROF-03
- GitHub issue #67 — motivation, the five profiles, the metrics list, "keep data chunk sizes independent of manifest node boundaries"

### Store header and format policy
- `docs/design/store-format.md` — header grammar, parse order, flag classes, rule to add a flag, read-only mode, messages
- `crates/slicefs-cli/src/header.rs` — `KNOWN_FLAGS`, `Line`, `parse`, `decide`, `check`, `create_with`; the byte-level core proved by Kani
- `.planning/phases/21.1-store-header-and-format-policy-154/21.1-CONTEXT.md` — header decisions, "a new setting line always comes with a flag", no real release name yet

### Chunking code
- `crates/chunk-store/src/lib.rs` — `CHUNK_MIN`/`CHUNK_AVG`/`CHUNK_MAX`, `chunk_ranges`, `Options`
- `crates/chunk-store/src/file.rs` — `FileWriter` (buffer `2 * CHUNK_MAX`, `cut`), `zero_chunk()` (process-wide `OnceLock`), `known_zero`, `splice` (re-chunk, read-ahead `CHUNK_AVG`), `zero_runs_cut_at_chunk_max` test
- `crates/slicefs-cli/src/content.rs` — `open`, `open_read_only`, `store_io`, `store_io_read_only`, `select_hash`/`check_write` (the model for a setting given at seed or mount)
- `crates/metadata/src/store_io.rs` — `put_value` uses `FileWriter::default()` with the store
- `docs/design/chunk-store.md` — chunking section (FastCDC v2020, 4/8/32 KiB), splice restart rule, memory bound of a stream

### Safety gates and test rules
- `scripts/safety/kani.sh`, `scripts/safety/unsafe-count.sh`, `docs/design/unsafe-review.md`, `.github/workflows/safety.yml` — release gate of Phase 21.2
- `scripts/docker-linux-test/run.sh` — Linux FUSE tests in Docker (only if a mount check is added)

### Benchmark
- `scripts/pack-measure/README.md`, `scripts/pack-measure/seed-measure.sh` — the style of a by-hand measurement script (CSV, work dir, median)
- `scripts/macos-mount-test/fetch-boost.sh` — Boost tree and tar inputs

</canonical_refs>

<code_context>
## Existing Code Insights

### Reusable Assets
- `ChunkStore` is passed to every `FileWriter`/`FileReader`/`splice` call, so the profile can come from the store with no new parameters.
- `content::open*` already build `Options` from the parsed header (`hasher_of`); the profile is one more field read the same way.
- `header::create_with` writes the first header of a new store before any data (the place where `--chunk-profile` goes, as `--hash` does).
- The xorshift `random_bytes` helper (`content.rs` tests) and the `VecSource`/`Rng` helpers in `file.rs` tests can feed the golden and two-store tests.
- `scripts/pack-measure/` is the model for the benchmark driver and README.

### Established Patterns
- Store settings live in `FORMAT`; a new setting line comes with a flag; unknown lines are allowed only next to an unknown flag.
- Durable header writes (temp file, sync, link or rename, dir sync).
- Tests are short; long runs are `#[ignore]` or scripts run by hand.

### Integration Points
- `crates/chunk-store/src/lib.rs` (`Options`, `ChunkStore::open`), `file.rs` (constant reads, zero chunk).
- `crates/slicefs-cli/src/header.rs` (line kind, flag), `content.rs` (open points), `cli.rs` (`--chunk-profile` on Seed and Mount), `seed.rs`, `mount.rs`, `stats.rs`.
- The filesystem test at `crates/slicefs-cli/src/filesystem.rs` ~7017 hard-codes `8 * 32768` (default profile; stays valid).

</code_context>

<specifics>
## Specific Ideas

- Profile notation in KiB as in the issue: `4/8/32`, `8/16/64`, `8/32/128`, `16/64/256`, `32/128/512`.
- The report recommends a default from measured trade-offs, not from assumed optimality (#67).
- No home paths in committed text; no license talk; plain short English.

</specifics>

<deferred>
## Deferred Ideas

- A change of the default profile (owner decision after the report).
- Size-class or bimodal chunking; per-file or per-directory profiles (parked by the owner).
- A rechunk command that moves an existing store to another profile.
- The combined benchmark of chunk profiles and prolly-tree node policies (Phase 24).
- Manifest or prolly node count and metadata churn per profile (after #19 / Phase 24).

</deferred>

---

*Phase: 22-chunk-profiles*
*Context gathered: 2026-11-25*
