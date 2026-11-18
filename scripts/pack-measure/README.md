# Pack-file measurements (seed)

`seed-measure.sh` measures `slicefs seed` on one build. For each input and run,
it records:

- seed wall time, MB/s and peak RSS (from `/usr/bin/time -l`)
- store apparent bytes (sum of file sizes) and allocated bytes (`du`)
- store file count, split into `vt0/`, `packs/`, `segments/` and other files

The script does not mount. It runs on macOS only (`stat -f`, `/usr/bin/time -l`).
It calls only `slicefs seed <store> <dir>`, so it runs the same way on old and
new builds.

| Script | What |
|---|---|
| `seed-measure.sh <slicefs-binary> <data-dir> [runs]` | Seed each input `runs` times (default 3) into a new store; print one CSV row per run and one median row per input |

## Inputs

| Input | Source |
|---|---|
| `tree` | The Boost tree `DIR/x/boost_<v>/` (1_85_0: 77,655 files) |
| `tar` | The Boost tar `DIR/boost_<v>.tar` (1_85_0: 823,613,440 bytes), as a hard link in `DIR/in-tar-<v>/` |
| `rand` | One random file `DIR/in-rand-<bytes>/random.bin` (default 1 GiB) |

`seed` reads only directories, so the tar and the random file each go in a
directory of their own. The script makes the random file once, in the data dir.
After that, all builds and runs use the same file.

Get the Boost data with the mount-test script:

```sh
scripts/macos-mount-test/fetch-boost.sh DIR 1_85_0
```

## Run

```sh
scripts/pack-measure/seed-measure.sh BINARY DIR > results.csv
WORK=WORKDIR LABEL=c2f82fa INPUTS="tree tar" RUNS=3 scripts/pack-measure/seed-measure.sh BINARY DIR
```

| Env | Default | What |
|---|---|---|
| `WORK` | a new temp dir | Stores, seed logs, time files and `results-<LABEL>.csv` |
| `LABEL` | basename of the binary | Build name in the `build` column |
| `INPUTS` | `tree tar rand` | Inputs to measure, in this order |
| `BOOST_VER` | `1_85_0` | Boost version of the `tree` and `tar` inputs |
| `RAND_BYTES` | `1073741824` | Size of the random file |
| `RUNS` | `3` | Runs per input, when the third argument is not given |
| `BOOST_DIR` | none | Data dir, when the second argument is not given |

stdout gets only CSV. Progress goes to stderr. The same rows also go to
`WORK/results-<LABEL>.csv`, which the script makes new at each start.
The script stops with a non-zero exit code when a seed fails or an input is
missing.

## CSV columns

| Column | Unit | What |
|---|---|---|
| `build` | | `LABEL` |
| `input` | | `tree`, `tar` or `rand` |
| `run` | | 1 to RUNS, or `median` |
| `in_bytes` | bytes | Sum of the input file sizes |
| `in_files` | files | Input file count |
| `secs` | s | Wall time of `seed` (`real` from `/usr/bin/time -l`) |
| `mb_s` | MB/s | `in_bytes / 10^6 / secs` |
| `apparent_bytes` | bytes | Sum of the store file sizes |
| `alloc_bytes` | bytes | `du -sk` of the store x 1024 |
| `files` | files | All store files |
| `vt0_files`, `packs_files`, `segments_files` | files | Store files under that top dir (0 when it is absent) |
| `other_files` | files | `files` minus the three dirs (for example `FORMAT`) |
| `peak_rss_bytes` | bytes | Maximum resident set size of `seed` |

MB is 10^6 bytes. Each run seeds a new store, and the script deletes the store
after the row. Before the runs of an input, the script reads all input files
once, so the page cache is warm. In the median row, each number is the
`median_low` of that column over the runs, so it is always a value from a real
run.

## Build an old commit

Build each commit in a git worktree outside the repo, with its own target dir:

```sh
git worktree add --detach DIR/wt-c2f82fa c2f82fa
git -C DIR/wt-c2f82fa submodule update --init   # commits that still use data-id
(cd DIR/wt-c2f82fa && CARGO_TARGET_DIR=DIR/target-c2f82fa cargo build --release -p slicefs-cli)
cp DIR/target-c2f82fa/release/slicefs DIR/bin/slicefs-c2f82fa
git worktree remove --force DIR/wt-c2f82fa
```

Build all builds with the same Rust toolchain, so that the times compare.

## FSKit round trip

This script does not copy through a mount. For the Boost round trip through an
FSKit mount, run `scripts/macos-mount-test/boost-test.sh` by hand.
