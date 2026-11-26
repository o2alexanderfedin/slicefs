---
phase: 24-prolly-evaluation
plan: 04
subsystem: prolly benchmark
status: complete
tags: [prolly, benchmark, driver, docker]
requires:
  - "24-03: the prolly-bench binary (gen, seed, whole, batches, flat, prolly, scale)"
provides:
  - "scripts/prolly-bench/bench.sh: the driver (seed once, runs, modes, six output files, median rows, machine.txt with bench_data, SMOKE mode, cleanup trap)"
  - "scripts/prolly-bench/README.md: run lines, modes, edit script, env, every column, notes"
  - "scripts/docker-linux-test/run.sh prolly-bench-smoke (no FUSE)"
  - "macOS and Linux smoke results, gate log"
affects: [24-05]
tech-stack:
  added: []
  patterns:
    - "one generic awk helper turns `kind key=value` lines into CSV rows (NA for a missing key)"
    - "one python3 median_low helper for all CSVs: group by key columns, run=median rows"
key-files:
  created:
    - scripts/prolly-bench/bench.sh
    - scripts/prolly-bench/README.md
    - .planning/phases/24-prolly-evaluation/runs/smoke-24-04-macos.csv
    - .planning/phases/24-prolly-evaluation/runs/smoke-24-04-linux.log
    - .planning/phases/24-prolly-evaluation/runs/gate-24-04.log
  modified:
    - scripts/docker-linux-test/run.sh
decisions:
  - "Six output files with the columns of the plan; median_low over the run rows of each group; seq=all on the median rows of results.csv"
  - "The store copy per run is a clone (cp -cR on macOS, cp -a --reflink=auto on Linux), a plain cp -R when the clone fails"
  - "SMOKE defaults KS=\"1 10\" COMMITS=2 SCALE_NS=\"10000 50000\"; trees of 2,000 and 2,100 files of up to 4 KiB; SMOKE forces BOOST_VERS=\"0_0_1 0_0_2\""
  - "Scale rows run in each run, after flat and prolly, in the same mode order as the run (flat first on odd runs)"
  - "Signals are ignored once the script ends, so a late signal cannot cut the rm of the EXIT trap"
  - "The data-dir, tree and BOOST_VERS checks run before the build of BIN and before WORK is made, so a refusal is fast and writes nothing"
  - "The Docker mode also runs golden_root by name (non-quiet), so the log shows `test tests::golden_root ... ok`"
metrics:
  duration: "about 25 min"
  completed: 2026-11-26
requirements: [PROLLY-02]
actuals:
  tokens: 12200    # chars/4 over bench.sh, README.md, the run.sh lines and the three run files
  tasks: 3
  commits: 3       # git rev-list --count 50bbdbf..75d0d15
plan_head_before: 50bbdbf
plan_head_after: 75d0d15
---

# Phase 24 Plan 04: Prolly benchmark driver Summary

`scripts/prolly-bench/bench.sh` seeds the first tree once, then for each run copies the seeded store and runs `whole`, `batches`, `flat` and `prolly` (flat and prolly in alternating order), and the `scale` rows, each step as its own process under `/usr/bin/time`. It writes `results.csv` (20 columns, also on stdout), `scale.csv`, `disk.csv`, `edits.csv`, `batches.csv`, `shape.txt` and `machine.txt` (with `bench_data=smoke|boost` and the run parameters). `SMOKE=1` runs in about 3 s on macOS and in Linux Docker with no FUSE.

## Commits

| Task | Commit | Subject |
|------|--------|---------|
| 1 (tracer) | 7c9aeb8 | feat(24-04): prolly bench driver tracer: smoke data to results.csv with medians |
| 2 | 702074e | feat(24-04): scale, disk, edits, batches and shape outputs; refusals; README; macOS smoke |
| 3 | 75d0d15 | feat(24-04): Linux Docker mode prolly-bench-smoke (no FUSE); Linux smoke and gate logs |

## Results

- Task 1 verify: `rc=0`, `rows=30 medians=18 bad=0`, `bench_data=smoke` count 1, 0 stores dirs left; `syntax=0`. Tracer gate: the verify passed before the commit; Task 2 started after it.
- Task 2 verify 1: `rc=0`, `scale=16`, `disk=12`, `edits=10`, `batches=9`, 2 `ok=1` checks, 0 home paths in `machine.txt`.
- Task 2 verify 2 (full mode on generated data): `full_rc=0`, `data_unchanged` (cksum listings of the data dir before and after are equal), `bench_data=boost` count 1, `smoke_with_dir=2`, `work_root=2`, `no_data=2`. By hand also: `BOOST_VERS` with one version exits 2, a missing tree exits 2 with the `fetch-boost.sh DATA_DIR <vers>` hint, no argument exits 2.
- Task 2 verify 3 (interrupt): see deviation 1. After the fix, a sweep of `timeout -s INT` at 1, 2, 3, 4, 4.5, 5, 5.5, 6 and 7 s cut the run in seed, whole, flat, prolly, scale and in the cleanup; each time 0 stores dirs were left.
- Acceptance greps: `bench.sh` executable, `median_low` 3 times, `trap 'rm -rf` 1 time; README `new_record_bytes` and `fetch-boost.sh` present; `smoke-24-04-macos.csv` has 18 `,median,` rows and 0 hits of the forbidden-text grep; `Linux` 6 times in the README; `result=ok` 2 times in the gate log.
- Task 3 verify: `prolly-bench-smoke` 2 times in `run.sh`; 0 `--device` or `cap-add` in the case; Linux log: 3 `test result: ok. N passed` lines, 18 median rows, 1 `os=Linux`, 1 `# checks ok: 2`, 0 hits of the forbidden-text grep in the Linux log and the gate log.
- Extra check after the commits: `SMOKE=1 RUNS=2 scripts/prolly-bench/bench.sh` run directly (file mode 100755), with `BIN` and `WORK` unset (the script built the binary and made its own temp work dir): `rc=0`, 60 run rows and 18 median rows, `scale=24`, `disk=18`, `edits=20`, `batches=18`, 4 `ok=1` checks, prolly rows before flat rows in run 2 (also in `scale.csv`), no stores dir left.
- No change under `crates/`, `benchmarks/`, `docs/design/store-format.md`, `Cargo.toml` or `Cargo.lock` (`git diff --quiet 50bbdbf`: `diff_rc=0`).

### Gates

- `timeout 900 cargo test -p prolly -p prolly-bench`: exit 0; prolly 21 passed; prolly-bench smoke 2 passed.
- `RUST_STABLE=stable timeout 1800 scripts/safety/clippy.sh`: `CLIPPY os=Darwin toolchain=stable result=ok`, `UNSAFE files=32 tokens=134 result=ok`.
- Log: `runs/gate-24-04.log`. The gate ran on the Task 3 work tree before the Task 3 commit (no Rust change in this plan).

### Smoke on macOS (`SMOKE=1 RUNS=1`)

- APFS, rustc 1.95 nightly (host default), load about 11. Start to end 3 s.
- `results.csv`: 30 run rows, 18 median rows. Two `check ... ok=1` lines (flat and prolly match the real final `inode_data`: 2,118 entries).
- Saved as `runs/smoke-24-04-macos.csv`. Plumbing only; these numbers say nothing about Boost.

### Smoke on Linux Docker (`scripts/docker-linux-test/run.sh prolly-bench-smoke`)

- Image `rust:1.100.0-bookworm`, OrbStack kernel 7.2, overlay file system, `-j 2`, a private target volume (`SLICEFS_TARGET_VOL`, removed after the run).
- `cargo test -q --release -p prolly`: 21 passed. `tests::golden_root ... ok` by name. **The golden prolly root (`1ee1cf56…`) and the cut-probability bits are the same on Linux (x86_64) as on macOS.**
- The smoke: 18 median rows, `# checks ok: 2`, `os=Linux`, `bench_data=smoke`. `peak_rss_bytes` is `NA` (no GNU `time` in the image), as expected.
- Cross-platform: for every `prolly` row, `entries_changed`, `serialized_bytes`, `new_objects`, `new_raw_bytes`, `nodes_read` and `nodes_written` are the same on macOS and Linux. The `whole` and `flat` byte columns differ a little: the mtimes of the generated files go into the inode digests, and FastCDC cuts the flat map by content. This agrees with the boundary rule (cuts depend only on keys and entry lengths).
- `commit=unknown` in the container (git cannot read the read-only `/src` there). Saved as `runs/smoke-24-04-linux.log`.

## Deviations from Plan

1. **[Rule 1 - Bug] A late signal cut the cleanup.** With the plan's `trap 'exit 130' INT TERM HUP`, an INT that came while the EXIT trap ran `rm -rf` (at the end of a normal run) killed `rm` (timeout signals the whole process group), and a stores dir was left behind. Fix: the INT trap first ignores INT, TERM and HUP, and the script ignores them after its last step; `rm` inherits the ignored signals. The line `trap 'rm -rf "$S"' EXIT` is unchanged. Commit 702074e.
2. **[Discretion] One median helper for all CSVs.** The plan says "the python3 block of the model". The script has one `median <csv> <key cols> <seq col>` function that prints all median rows of a file in one python3 call, in first-seen group order, instead of one call per group. Same `median_low` rule.
3. **[Discretion] The Docker case runs `golden_root` once more by name** (non-quiet, with `set -o pipefail`), so the log proves which test ran. The plan only asked for `cargo test -q -p prolly`.
4. **[Discretion] Private target volume for the Docker run.** Another agent used Docker on the machine; the run used `SLICEFS_TARGET_VOL=slicefs-target-x2404` and removed the volume after it. The README names the variable.
5. **[Discretion] Scratch folder.** The launch message gave a fixed private scratch folder, so the plan's `mktemp -d "${TMPDIR}/p24-04.XXXXXX"` was not used; every temp dir was made with `mktemp -d` inside it.
6. **[Process] GSD state steps not done.** The launch message does not permit changes to `STATE.md` and `ROADMAP.md`. The plan-commit ledger file was not written (the worktree guard refused the compound command); the base `50bbdbf` is known from the start step.

## Notes for plan 24-05

- `bench.sh DATA 3` with `fetch-boost.sh DATA 1_84_0 1_85_0`. The full run time is not measured yet.
- Peak RSS is `NA` in Docker; run the Boost benchmark on macOS (or install GNU `time` in a container) to get `peak_rss_bytes`.
- `machine.txt` has `bench_data=boost` only when a data dir is given.

## Known Stubs

None.

## Threat Flags

None. The script removes only its `mktemp` dir under WORK and paths in it (T-24-09); `machine.txt` and the committed files have no path, host or user (T-24-10); the Docker mode has no `--device` and no `--cap-add`, and `/src` is read-only (T-24-11). No dependency change (T-24-SC).

## Open owner questions

None from this plan. Q-1 to Q-3 of 24-CONTEXT.md stay open.

## Self-Check: PASSED

- `scripts/prolly-bench/bench.sh`, `scripts/prolly-bench/README.md`, `runs/smoke-24-04-macos.csv`, `runs/smoke-24-04-linux.log` and `runs/gate-24-04.log` exist.
- Commits 7c9aeb8, 702074e and 75d0d15 are on `exec/24-04` (`git log 50bbdbf..HEAD`).
