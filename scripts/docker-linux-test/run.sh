#!/bin/bash
# Run SliceFS tests against Linux FUSE inside Docker (works on macOS via OrbStack/Docker Desktop).
# A hung mount dies with the container, so no stuck processes are left on the host.
# Commands run in a private mount namespace (unshare): OrbStack briefly holds
# mounts of brand-new containers, which otherwise keeps the FUSE connection
# alive after unmount and makes the daemon hang on exit. unshare alone is not
# enough for the first ~40 s of a container's life, so smoke/verify wait until the
# container is 45 s old before the first mount (the cargo build counts).
# Usage: scripts/docker-linux-test/run.sh [smoke|test|all|verify|pjdfstest]   (default: all)
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd); REPO=$(cd "$HERE/../.." && pwd)
docker build -q -t slicefs-fusetest "$HERE" >/dev/null
RUN=(docker run --rm --device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined
  -v "$REPO":/src:ro -v slicefs-target:/target -v slicefs-cargo:/usr/local/cargo/registry
  -v "$HERE":/dk:ro -e CARGO_TARGET_DIR=/target slicefs-fusetest unshare -m --propagation private)
case "${1:-all}" in
  smoke) "${RUN[@]}" bash -c 'cargo build -q -p slicefs-cli && { w=$((45 - SECONDS)); [ $w -le 0 ] || sleep $w; } && /dk/mt-linux.sh /target/debug/slicefs && /dk/dedup-probe.sh /target/debug/slicefs' ;;
  test)  "${RUN[@]}" bash -c 'cargo test -q --workspace && cd vendor/fuser && cargo test -q --lib' ;;
  all)   "${RUN[@]}" bash -c 'cargo test -q --workspace && (cd vendor/fuser && cargo test -q --lib) && cargo build -q -p slicefs-cli && { w=$((45 - SECONDS)); [ $w -le 0 ] || sleep $w; } && /dk/mt-linux.sh /target/debug/slicefs && /dk/dedup-probe.sh /target/debug/slicefs' ;;
  verify) "${RUN[@]}" bash -c 'cargo build -q -p slicefs-cli && { w=$((45 - SECONDS)); [ $w -le 0 ] || sleep $w; } && /dk/verify-phases.sh /target/debug/slicefs' ;;
  pjdfstest) "${RUN[@]}" bash -c 'cargo build -q --release -p slicefs-cli && { w=$((45 - SECONDS)); [ $w -le 0 ] || sleep $w; } && /dk/pjdfstest.sh /target/release/slicefs' ;;
  *) echo "usage: $0 [smoke|test|all|verify|pjdfstest]"; exit 2 ;;
esac
