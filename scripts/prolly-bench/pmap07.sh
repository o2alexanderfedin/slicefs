#!/bin/bash
# PMAP-07 runner (Phase 26). The rule is in
# .planning/phases/26-prolly-maps-in-commit-read-and-gc/runs/pmap07-rule.md and
# the decision script is pmap07.py next to this file. Plan 26-13 adds the
# `boost` mode.
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
# env:   SLICEFS_TARGET_VOL  cargo target volume (default slicefs-target-pmap07-<label>)
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
  *) usage ;;
esac
