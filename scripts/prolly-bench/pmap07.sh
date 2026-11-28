#!/bin/bash
# PMAP-07 runner (Phase 26). The rule is in
# .planning/phases/26-prolly-maps-in-commit-read-and-gc/runs/pmap07-rule.md and
# the decision script is pmap07.py next to this file.
#
# usage: pmap07.sh prepare-v230 <dir>
#          Make the detached worktree <dir> of tag v2.3.0 (if it does not exist)
#          and copy the bench file and the flat report module into it.
#        pmap07.sh commit <src-dir> <label> <n> <samples> <out-file>
#          Run the ignored test one_change_commit_at_scale of the tree <src-dir>
#          (mounted read-only) in Docker, release build, with the store and the
#          temp files on the disk-backed volume slicefs-pmap07-work. Stdout and
#          stderr go to <out-file>. Exit code: that of the container.
#          Refuses (exit 2) while another container named pmap07-* runs.
#        pmap07.sh boost <bin-src-dir> <label> <data-dir> <out-dir>
#          Check C: build prolly-bench in release from <bin-src-dir> (mounted
#          read-only) and run bench.sh of this tree with BIN = that binary, in
#          Docker, with the work dir on slicefs-pmap07-work. <data-dir> holds
#          x/boost_1_84_0 and x/boost_1_85_0 (fetch-boost.sh); it is mounted
#          read-only. Settings of the rule: RUNS=3, KS="1 10 100 1000",
#          COMMITS=10, PATTERNS="scatter cluster insdel", SEED=24, SCALE_NS="".
#          Writes results.csv, disk.csv, edits.csv, shape.txt, machine.txt and
#          build.txt (the commit of <bin-src-dir>) to <out-dir>. Same refusal.
#          With SMOKE=1: <data-dir> is "-", and bench.sh runs SMOKE=1 RUNS=1
#          KS="1 10 100" (generated trees; checks the harness, decides nothing).
# env:   SLICEFS_TARGET_VOL  cargo target volume (default slicefs-target-pmap07-<label>)
#        SMOKE               1: the smoke of the boost mode
# By hand, not in CI. Runs on a host with Docker (OrbStack on macOS).
set -euo pipefail
usage() { sed -n '/^# usage:/,/^# env:/p' "$0" | sed 's/^# \{0,1\}//' >&2; exit 2; }
HERE=$(cd "$(dirname "$0")" && pwd); REPO=$(cd "$HERE/../.." && pwd)
MODE=${1:-}
case "$MODE" in
  prepare-v230)
    [ $# -eq 2 ] || usage
    DIR=$2
    if [ ! -e "$DIR" ]; then
      git -C "$REPO" worktree add --detach "$DIR" v2.3.0 >&2
    fi
    DIR=$(cd "$DIR" && pwd)
    mkdir -p "$DIR/crates/metadata/tests/support"
    cp "$REPO/crates/metadata/tests/commit_bench.rs" "$DIR/crates/metadata/tests/commit_bench.rs"
    cp "$HERE/pmap07/report_flat.rs" "$DIR/crates/metadata/tests/support/pmap07_report.rs"
    echo "v230 head=$(git -C "$DIR" rev-parse --short HEAD)"
    git -C "$DIR" status --short
    ;;
  commit)
    [ $# -eq 6 ] || usage
    SRC=$2; LABEL=$3; N=$4; SAMPLES=$5; OUT=$6
    [ -d "$SRC" ] || { echo "no such source dir: $SRC" >&2; exit 2; }
    SRC=$(cd "$SRC" && pwd)
    [[ "$LABEL" =~ ^[a-z0-9][a-z0-9-]*$ ]] || { echo "bad label: $LABEL" >&2; exit 2; }
    [[ "$N" =~ ^[1-9][0-9]*$ && "$SAMPLES" =~ ^[1-9][0-9]*$ ]] || { echo "n and samples are positive numbers" >&2; exit 2; }
    OUTDIR=$(cd "$(dirname "$OUT")" && pwd) || { echo "no such dir for $OUT" >&2; exit 2; }
    OUT=$OUTDIR/$(basename "$OUT")
    RUNNING=$(docker ps --filter name=pmap07- -q </dev/null)
    [ -z "$RUNNING" ] || { echo "another pmap07 run is in progress: $RUNNING" >&2; exit 2; }
    docker build -q -t slicefs-fusetest "$REPO/scripts/docker-linux-test" </dev/null >/dev/null
    rc=0
    docker run --rm --name "pmap07-$LABEL-$$" \
      -v "$SRC":/src:ro -v "${SLICEFS_TARGET_VOL:-slicefs-target-pmap07-$LABEL}":/target \
      -v slicefs-cargo:/usr/local/cargo/registry -v slicefs-pmap07-work:/work \
      -e CARGO_TARGET_DIR=/target -e PMAP07_N="$N" -e PMAP07_SAMPLES="$SAMPLES" -e PMAP07_LABEL="$LABEL" \
      slicefs-fusetest bash -c '
        mkdir -p /work/tmp && export TMPDIR=/work/tmp
        rustc -V
        grep MemAvailable /proc/meminfo
        df -h /work
        cd /src && cargo test --release --locked -p metadata --test commit_bench -- --ignored --exact one_change_commit_at_scale --nocapture
      ' </dev/null > "$OUT" 2>&1 || rc=$?
    exit "$rc"
    ;;
  boost)
    [ $# -eq 5 ] || usage
    SRC=$2; LABEL=$3; DATA=$4; OUT=$5; SMOKE=${SMOKE:-0}
    [ -d "$SRC" ] || { echo "no such source dir: $SRC" >&2; exit 2; }
    SRC=$(cd "$SRC" && pwd)
    [[ "$LABEL" =~ ^[a-z0-9][a-z0-9-]*$ ]] || { echo "bad label: $LABEL" >&2; exit 2; }
    DATA_MOUNT=()
    if [ "$SMOKE" = 1 ]; then
      [ "$DATA" = - ] || { echo "SMOKE=1 takes no data dir (give -)" >&2; exit 2; }
    else
      [ -d "$DATA/x/boost_1_84_0" ] && [ -d "$DATA/x/boost_1_85_0" ] || {
        echo "missing $DATA/x/boost_1_84_0 or boost_1_85_0; get them with scripts/macos-mount-test/fetch-boost.sh DIR 1_84_0 1_85_0" >&2; exit 2; }
      DATA_MOUNT=(-v "$(cd "$DATA" && pwd)":/data:ro)
    fi
    mkdir -p "$OUT"; OUT=$(cd "$OUT" && pwd)
    RUNNING=$(docker ps --filter name=pmap07- -q </dev/null)
    [ -z "$RUNNING" ] || { echo "another pmap07 run is in progress: $RUNNING" >&2; exit 2; }
    echo "label=$LABEL bin_src_commit=$(git -C "$SRC" rev-parse HEAD) bench_commit=$(git -C "$REPO" rev-parse HEAD) smoke=$SMOKE" > "$OUT/build.txt"
    docker build -q -t slicefs-fusetest "$REPO/scripts/docker-linux-test" </dev/null >/dev/null
    rc=0
    docker run --rm --name "pmap07-$LABEL-$$" \
      -v "$SRC":/bin-src:ro -v "$REPO":/src:ro ${DATA_MOUNT[@]+"${DATA_MOUNT[@]}"} -v "$OUT":/out \
      -v "${SLICEFS_TARGET_VOL:-slicefs-target-pmap07-$LABEL}":/target \
      -v slicefs-cargo:/usr/local/cargo/registry -v slicefs-pmap07-work:/work \
      -e CARGO_TARGET_DIR=/target -e SMOKE="$SMOKE" \
      slicefs-fusetest bash -c '
        set -euo pipefail
        mkdir -p /work/tmp && export TMPDIR=/work/tmp
        rustc -V
        grep MemAvailable /proc/meminfo
        df -h /work
        (cd /bin-src && cargo build --release --locked -p prolly-bench)
        W=$(mktemp -d /work/boost.XXXXXX)
        trap "rm -rf \"$W\"" EXIT
        if [ "$SMOKE" = 1 ]; then
          SMOKE=1 RUNS=1 KS="1 10 100" WORK=$W BIN=/target/release/prolly-bench bash /src/scripts/prolly-bench/bench.sh > /dev/null
        else
          RUNS=3 KS="1 10 100 1000" COMMITS=10 PATTERNS="scatter cluster insdel" SEED=24 SCALE_NS= \
            BOOST_VERS="1_84_0 1_85_0" WORK=$W BIN=/target/release/prolly-bench bash /src/scripts/prolly-bench/bench.sh /data > /dev/null
        fi
        cp "$W/results.csv" "$W/disk.csv" "$W/edits.csv" "$W/shape.txt" "$W/machine.txt" /out/
      ' </dev/null > "$OUT/run.log" 2>&1 || rc=$?
    exit "$rc"
    ;;
  *) usage ;;
esac
