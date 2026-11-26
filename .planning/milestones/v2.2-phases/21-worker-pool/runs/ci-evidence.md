# Phase 21 CI evidence (plan 21-08, last code commit)

sha = 7853542bd6c5a6d3bf3d0e07dac7064aa4181e51

| Workflow | Run | Commit | Result | Check |
|---|---|---|---|---|
| ci.yml (PR #173), Linux job in Docker | 45421387746 | fdceef0 (same code as 7853542; later commits change only `.planning/`) | success | `Workers: 4 (set)` in the log, 33 lines |
| ci.yml (PR #173), macOS job | 45421387746 | fdceef0 | success | |
| macos-mounts.yml on `ci/21-08-workers` | 45421361354 | 7853542 | success | `Workers: 1 (default)` in the log, 4 lines |
| pool-bench-macos.yml on `ci-pool/21-08-bench` | 45421265175 | 7853542 | success | `FAILS=0`; log in `pool-bench-macos.log` |

## macOS FSKit bench (1 and 2 workers, recorded, not judged)

The runner had 3 CPUs and a 1-minute load of 50, so the numbers vary a lot.

| Workers | Write MB/s (3 runs) | Median | Read MB/s (3 runs) | Median | Max RSS (MiB) |
|---|---|---|---|---|---|
| 1 | 25.2, 23.4, 60.8 | 25.2 | 330.7, 171.1, 265.6 | 265.6 | 63 |
| 2 | 77.7, 13.3, 37.3 | 37.3 | 116.5, 206.8, 165.7 | 165.7 | 69 |

- The data does not show a throughput gain with 2 workers on FSKit, so the macOS default stays 1.
- All 6 runs had 0 mismatches.
- Known defect in the log: every `macbench` line prints `run=1`. The `== n=<N> run=<r>` header before each line gives the true run number.
