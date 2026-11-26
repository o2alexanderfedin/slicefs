#!/bin/bash
# Power-loss test of `slicefs rehash` on LazyFS, in a Linux Docker container.
# See README.md. Usage: scripts/power-loss/run.sh
# PL_TO (target hash, default sha256), PL_DEADLINE (time limit of the loop in
# seconds, default 900) and PL_ONLY (one crash point) pass to powerloss.sh.
# PL_OUT=<dir> gets the stores and logs of failed points.
# SLICEFS_TARGET_VOL names the cargo target volume (default slicefs-powerloss-target).
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd); REPO=$(cd "$HERE/../.." && pwd)
timeout 1800 docker build -q -t slicefs-powerloss "$HERE" </dev/null >/dev/null
OUT=()
if [ -n "${PL_OUT:-}" ]; then mkdir -p "$PL_OUT"; OUT=(-v "$(cd "$PL_OUT" && pwd)":/out); fi
# The 45 s wait before the first FUSE mount and the private mount namespace:
# see scripts/docker-linux-test/run.sh.
timeout 2400 docker run --rm --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined \
  -v "$REPO":/src:ro -v "${SLICEFS_TARGET_VOL:-slicefs-powerloss-target}":/target -v slicefs-cargo:/usr/local/cargo/registry \
  -v "$HERE":/pl:ro ${OUT[@]+"${OUT[@]}"} -e CARGO_TARGET_DIR=/target -e PL_TO -e PL_DEADLINE -e PL_ONLY \
  slicefs-powerloss unshare -m --propagation private bash -c '
    timeout 1200 cargo build -q -p slicefs-cli > /tmp/build.log 2>&1 || { tail -40 /tmp/build.log; exit 2; }
    bash /pl/powerloss.sh /target/debug/slicefs; rc=$?
    [ -d /out ] && [ -d /tmp/pl/failed ] && cp -a /tmp/pl/failed /out/
    exit $rc' </dev/null
