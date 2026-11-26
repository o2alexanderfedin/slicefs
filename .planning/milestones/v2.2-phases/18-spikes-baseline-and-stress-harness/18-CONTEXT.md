# Phase 18: Spikes, baseline and stress harness - Context

**Gathered:** 2026-11-20
**Status:** Ready for planning

<domain>
## Phase Boundary

Phase 18 delivers measurements and a test tool. It does not change the request path. It delivers three items:

1. A result for each spike S1-S7 in `docs/research/multithreaded-request-handling.md` (issue #127). Each result names the migration step that it decides. S2 and S5 are measured again on the current code (after #108). Baselines that no longer hold are marked superseded.
2. An in-process stress harness. It runs N threads of mixed operations (read, write, fsync, create, rename, unlink) on one file and on many files. It checks the data against a model, and it reports latency histograms and lock hold and wait times. It has a short run for the test suite and a long run that is started by hand. This harness defines "done" for Phases 19-21.
3. S1 (FUSE-T overlap and reply order) on a macOS FSKit mount in macOS CI. The doc states whether the macOS default can be more than one worker.

Out of scope: fine-grained locks (Phase 19), the budget flusher (Phase 20), the worker pool in SliceFS (Phase 21).

</domain>

<decisions>
## Implementation Decisions

### Harness dependencies
- Latency and lock histograms use `hdrhistogram` 7.6 (user choice, as in `.planning/research/STACK.md`) — **Reversibility:** costly — a new crate in `Cargo.lock`; removal means a rewrite of the harness report code.
- The lock-stats recorder in `chunk-store` uses `hdrhistogram` as an optional normal dependency (`default-features = false`, `optional = true`), turned on only by the `lock-stats` feature (`lock-stats = ["dep:hdrhistogram"]`); `metadata` and `slicefs-cli` forward the feature. There is one histogram per lock site (hold and wait), and the report gives exact p50/p99/max; there are no power-of-two buckets. With the feature off, the crate is not in the product build (user decision, 2026-11-20, overrides the research note about log2 buckets) — **Reversibility:** costly — feature wiring in three Cargo.toml files.

- S1 runs in macOS CI from a temporary pushed branch `ci-s1/phase-18`, because the new workflow cannot be started by hand before it is on main. The branch is deleted after the run, and there are at most 3 runs (user approval, 2026-11-20).

### Decided autonomously
- [Harness] Q: "Where does the harness run?" → Selected: "In process, against the `Filesystem` core API (no FUSE mount), as an integration test in `crates/slicefs-cli/tests/`" — test code only; it can move without effect on the product.
- [Harness] Q: "How does the harness pass on today's code, which is safe only with one thread?" → Selected: "A `serialize` mode: one harness mutex around each operation, so N threads run but only one operation at a time. The suite runs with serialize on. A run with serialize off is the target for Phases 19-21 and is `#[ignore]` until then" — a harness flag; Phases 19-21 turn it off.
- [Harness] Q: "Short run and long run?" → Selected: "Short run in `cargo test` (a few seconds, small files, below the dirty budget); long run is `#[ignore]` with env vars for threads, duration and file size, started by hand" — test settings only; the "keep tests short" rule applies.
- [Harness] Q: "How are the data checked?" → Selected: "A model of each file (expected bytes per range) and of the namespace (names to inodes), compared after each step and at the end; a seed is printed so a failure can be run again" — test code only.
- [Lock stats] Q: "How are lock hold and wait times measured?" → Selected: "Thin timing wrappers on the store lock (`io`), the commit gate, `open_files`, `readers` and the WAL append mutex, compiled only with a cargo feature `lock-stats` (off by default, on for the harness and spikes)" — feature-gated; no cost in normal builds and simple to remove.
- [Spikes] Q: "How are S3, S5 and S7 run before a worker pool exists?" → Selected: "With the harness threads, which call the core directly (S3 store lock with N = 1, 2, 4, 8; S5 gate fairness with a 1 s commit timer and long shared holds; S7 4 random writers on 1 GiB files with a 256 MiB cap)" — measurement code only.
- [Spikes] Q: "How are S2, S4 and S6 run?" → Selected: "On the Linux Docker mount (`scripts/docker-linux-test/`): S2 with `bigwrite.sh` at 1 GiB and 4 GiB; S4 by the kernel queue depth per inode (`/sys/fs/fuse/connections/*/waiting`) during random writes plus readahead; S6 with many small fsynced files, per-op log against periodic" — scripts only.
- [Spikes] Q: "How is S1 run, given that local macOS mounts are not allowed?" → Selected: "A small standalone fuser test server (one reader and a 4-worker pool, one READ reply delayed by 200 ms while another process stats files), run in a separate macOS CI workflow that starts by hand (`workflow_dispatch`) or on a `ci/**` branch, like `macos-mounts.yml`" — a test binary and a CI workflow; both can be removed.
- [Docs] Q: "Where do the results go?" → Selected: "A results section per spike in `docs/research/multithreaded-request-handling.md`, with the numbers, the decided step, and superseded baselines marked" — documentation only.

### Claude's Discretion
- Exact operation mix, thread counts and durations of the short run, inside the "keep tests short" rule.

</decisions>

<code_context>
## Existing Code Insights

### Reusable Assets
- `scripts/docker-linux-test/run.sh` and `bigwrite.sh`: the Linux Docker mount runs and the 1 GiB / 4 GiB random-write benchmark (baseline 1124 / 1772 ms slowest write).
- `.github/workflows/macos-mounts.yml`: the pattern for a manual macOS FSKit CI job (FUSE-T install, `enable-fskit.sh`).
- `crates/slicefs-cli/tests/dirty_budget_tests.rs`, `memory_bound_tests.rs`, `small_file_bench.rs`: tests that drive the core in process.
- `crates/chunk-store/src/race_tests.rs`: race tests for put and read against GC and merge.

### Established Patterns
- `Filesystem` in `crates/slicefs-cli/src/filesystem.rs` holds `io: Arc<Mutex<StoreIo>>`, `open_files`, `write_alias`, `readers`, `handles`, `orphans` (all `Mutex`) and `commit_gate: Arc<RwLock<()>>`.
- Writes take their `OpenFileState` out of `open_files` (filesystem.rs ~996, 1113, 1363), and `keep_budget` (~1242) takes the state of another file out. This is safe only with one thread, so the harness needs the serialize mode until Phase 20.

### Integration Points
- New integration test file(s) in `crates/slicefs-cli/tests/`; a `lock-stats` feature in `crates/slicefs-cli/Cargo.toml`.
- New spike scripts beside `scripts/docker-linux-test/`; a new macOS workflow beside `macos-mounts.yml`.

</code_context>

<specifics>
## Specific Ideas

- Order from #127: S2 first (no threads), then S1, then S3-S7 with the harness.
- Each spike result names the migration step (1-8) in the research doc that it decides.
- Do not mount on macOS locally; macOS CI mounts are fine. Wait loops in scripts must end when the process ends and must have a time limit.

</specifics>

<deferred>
## Deferred Ideas

None — discussion stayed within phase scope.

</deferred>
