#!/bin/bash
# Peak memory of `slicefs rehash` and of the offline GC, as a function of the
# file count (docs/research/gc-memory.md). Linux, in Docker, no FUSE mount:
# the stores are made with `slicefs seed`.
# Usage: scripts/gc-memory/run.sh [N ...]          (default: 10000 100000 1000000)
#        GCMEM_SNAPS=4 scripts/gc-memory/run.sh 100000   (more snapshots per store; default 1)
#        GCMEM_HEAPTRACK=1 scripts/gc-memory/run.sh 100000   (also the allocation sites at the heap peak)
# GCMEM_STEP_TIMEOUT: seconds for each step (default 900). By hand, not in CI.
# The sources and stores are in the container layer, so --rm removes them.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd); REPO=$(cd "$HERE/../.." && pwd)
docker build -q -t slicefs-gcmem "$REPO/scripts/docker-linux-test" >/dev/null
docker run --rm --name "slicefs-gcmem-$$" \
  -v "$REPO":/src:ro -v "${SLICEFS_TARGET_VOL:-slicefs-gcmem-target}":/target \
  -v slicefs-cargo:/usr/local/cargo/registry -v "$HERE":/gm:ro \
  -e CARGO_TARGET_DIR=/target -e GCMEM_SNAPS -e GCMEM_STEP_TIMEOUT -e GCMEM_HEAPTRACK \
  slicefs-gcmem bash /gm/measure.sh ${*:-10000 100000 1000000} </dev/null
